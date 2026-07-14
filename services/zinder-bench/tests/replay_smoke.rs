#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::{TempDir, tempdir};
use zinder_bench::{
    capture::measure_workload_density,
    fixture::{
        ActivationRecord, FIXTURE_FORMAT_VERSION, FixtureManifest, SubtreeRootSet, write_segment,
    },
    replay::{
        ProjectionReplayScope, ReplayConfig, replay_fixture,
        seed_projection_replay_at_canonical_tip,
    },
};
use zinder_core::{BlockHeight, Network, wire::encode_zinder_native_chain_name};
use zinder_derive::{
    ProjectionPreset, TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME, TransparentAddressRankingConsumer,
};
use zinder_ingest::open_primary_derive_store_for_canonical_with_projection_preset;
use zinder_source::SourceBlock;
use zinder_store::RocksDbResourceBudget;
use zinder_testkit::sample_regtest_upgrade_activations;

const REGTEST_BLOCK_1: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-block-1.json");

fn regtest_activation_records() -> Vec<ActivationRecord> {
    sample_regtest_upgrade_activations()
        .activations()
        .iter()
        .map(|activation| ActivationRecord {
            branch_id: activation.branch_id.value(),
            activation_height: activation.activation_height.value(),
            name: activation.name.clone(),
        })
        .collect()
}

fn write_regtest_fixture() -> Result<TempDir> {
    let fixture: Value = serde_json::from_str(REGTEST_BLOCK_1)?;
    let raw_block_hex = fixture
        .get("raw_block_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;
    let block = SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(1),
        hex::decode(raw_block_hex)?,
    )?;
    let fixture_directory = tempdir()?;
    let descriptor = write_segment(fixture_directory.path(), 0, std::slice::from_ref(&block))?;
    let workload_density = measure_workload_density(
        std::slice::from_ref(&block),
        &sample_regtest_upgrade_activations(),
    )?;
    FixtureManifest {
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: 1,
        to_height: 1,
        block_count: 1,
        workload_density,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        tip_hash_hex: hex::encode(block.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![descriptor],
        subtree_roots: SubtreeRootSet::default(),
    }
    .write(fixture_directory.path())?;
    Ok(fixture_directory)
}

fn replay_config(
    fixture_directory: &TempDir,
    store_directory: &TempDir,
    projection_preset: Option<ProjectionPreset>,
) -> Result<ReplayConfig> {
    Ok(ReplayConfig {
        fixture_directory: fixture_directory.path().to_path_buf(),
        store_path: store_directory.path().join("canonical"),
        block_prepare_concurrency: NonZeroU32::new(2).ok_or_else(|| eyre!("2 is non-zero"))?,
        canonical_block_cache_bytes: None,
        projection_preset,
        projection_replay_scope: ProjectionReplayScope::FixedRange,
    })
}

fn assert_projection_report(
    report: &zinder_bench::report::Report,
    projection_preset: &'static str,
) {
    assert_eq!(report.fixture.workload_density.block_count, 1);
    assert_eq!(report.fixture.workload_density.transaction_count, 1);
    assert_eq!(report.replay.tip_height_after, Some(1));
    assert_eq!(report.replay.blocks_committed, 1);
    assert_eq!(report.replay.projection_preset, Some(projection_preset));
    assert_eq!(report.replay.projection_replay_scope, Some("fixed-range"));
    assert!(report.replay.derive_wall_clock_seconds.is_some());
    assert!(
        report
            .replay
            .derive_bytes_written
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(report.replay.projection_row_count.is_some());
    assert_eq!(report.replay.projection_lag_blocks, Some(0));
    assert!(
        report
            .replay
            .derive_store_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(report.replay.derive_reopen_seconds.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_commits_a_genesis_block_over_the_real_pipeline() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let wallet_store_directory = tempdir()?;
    let wallet_report = replay_fixture(
        replay_config(
            &fixture_directory,
            &wallet_store_directory,
            Some(ProjectionPreset::Wallet),
        )?,
        None,
    )
    .await?;
    assert_projection_report(&wallet_report, "wallet");

    let complete_store_directory = tempdir()?;
    let complete_report = replay_fixture(
        replay_config(
            &fixture_directory,
            &complete_store_directory,
            Some(ProjectionPreset::Complete),
        )?,
        None,
    )
    .await?;
    assert_projection_report(&complete_report, "complete");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_replay_bootstraps_ranking_while_wallet_remains_ranking_free() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let complete_store_directory = tempdir()?;
    replay_fixture(
        replay_config(
            &fixture_directory,
            &complete_store_directory,
            Some(ProjectionPreset::Complete),
        )?,
        None,
    )
    .await?;

    let complete_store_path = complete_store_directory.path().join("canonical");
    let complete_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &complete_store_path,
        RocksDbResourceBudget::derive_writer_defaults(),
        ProjectionPreset::Complete,
    )?;
    let active = TransparentAddressRankingConsumer::active_metadata(&complete_store)?
        .ok_or_else(|| eyre!("complete replay must activate a ranking generation"))?;
    assert!(active.generation > 0);
    assert_eq!(
        active.coverage.balance_complete_through_height,
        BlockHeight::new(1)
    );
    assert!(TransparentAddressRankingConsumer::build_metadata(&complete_store)?.is_none());
    let ranking_cursor = complete_store
        .get_chain_event_cursor(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)?
        .ok_or_else(|| eyre!("complete replay must commit the ranking cursor"))?;
    assert_eq!(
        Some(ranking_cursor),
        complete_store.get_chain_event_cursor(TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME)?
    );
    drop(complete_store);

    let wallet_store_directory = tempdir()?;
    replay_fixture(
        replay_config(
            &fixture_directory,
            &wallet_store_directory,
            Some(ProjectionPreset::Wallet),
        )?,
        None,
    )
    .await?;

    let wallet_store_path = wallet_store_directory.path().join("canonical");
    let wallet_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &wallet_store_path,
        RocksDbResourceBudget::derive_writer_defaults(),
        ProjectionPreset::Wallet,
    )?;
    assert!(!wallet_store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME));
    assert_eq!(
        wallet_store.get_chain_event_cursor(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)?,
        None
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_range_seeds_selected_consumers_at_the_starting_tip() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let range_store_path = store_directory.path().join("canonical");
    let canonical_report = replay_fixture(
        replay_config(&fixture_directory, &store_directory, None)?,
        None,
    )
    .await?;
    assert_eq!(canonical_report.replay.tip_height_after, Some(1));

    let canonical_store = zinder_store::PrimaryChainStore::open(
        &range_store_path,
        zinder_store::ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = zinder_derive::DeriveStore::open_with_projection_preset(
        zinder_derive::DeriveStore::path_for_canonical(&range_store_path),
        ProjectionPreset::Complete,
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumers: ProjectionPreset::Complete.consumer_schemas(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    let seeded_cursor = seed_projection_replay_at_canonical_tip(&canonical_store, &derive_store)?
        .ok_or_else(|| eyre!("committed canonical history must have a cursor"))?;
    for consumer_name in derive_store.chain_event_consumer_names() {
        let cursor = derive_store.get_chain_event_cursor(consumer_name)?;
        if consumer_name == TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME {
            assert_eq!(cursor, None);
        } else {
            assert_eq!(cursor, Some(seeded_cursor.as_bytes().to_vec()));
        }
    }

    Ok(())
}
