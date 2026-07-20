#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::{NonZeroU32, NonZeroU64};

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::{TempDir, tempdir};
use zinder_bench::{
    capture::measure_fixture_blocks,
    fixture::{
        ActivationRecord, FIXTURE_CONTRACT_IDENTITY, FIXTURE_FORMAT_VERSION, FixtureManifest,
        SubtreeRootSet, write_segment,
    },
    recorder::install_recorder,
    replay::{
        MaterializedViewReplayScope, ReplayConfig, replay_fixture,
        seed_materialized_view_replay_at_canonical_tip,
    },
    report::{AcceptanceThresholds, StartingCanonicalStateKind},
};
use zinder_core::{BlockHeight, Network, wire::encode_zinder_native_chain_name};
use zinder_ingest::open_primary_materialized_view_store_for_canonical_with_materialized_view_preset;
use zinder_materialized_views::{
    MaterializedViewPreset, TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME, TransparentAddressRankingConsumer,
};
use zinder_source::SourceBlock;
use zinder_store::{
    ChainEventStreamFamily, EventStreamStartPosition, PrimaryChainStore, RocksDbResourceBudget,
};
use zinder_testkit::sample_regtest_upgrade_activations;

const REGTEST_BLOCK_1: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-block-1.json");
const REGTEST_BLOCK_603: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-ironwood-block-603.json");

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
    write_regtest_fixture_from_json(REGTEST_BLOCK_1)
}

fn write_non_genesis_manifest_fixture() -> Result<TempDir> {
    write_regtest_fixture_from_json(REGTEST_BLOCK_603)
}

fn write_regtest_fixture_from_json(fixture_json: &str) -> Result<TempDir> {
    let fixture: Value = serde_json::from_str(fixture_json)?;
    let height = fixture
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|height| u32::try_from(height).ok())
        .ok_or_else(|| eyre!("fixture height must fit in u32"))?;
    let raw_block_hex = fixture
        .get("raw_block_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;
    let block = SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(height),
        hex::decode(raw_block_hex)?,
    )?;
    let fixture_directory = tempdir()?;
    let descriptor = write_segment(fixture_directory.path(), 0, std::slice::from_ref(&block))?;
    let measurements = measure_fixture_blocks(
        std::slice::from_ref(&block),
        &sample_regtest_upgrade_activations(),
    )?;
    FixtureManifest {
        contract_identity: FIXTURE_CONTRACT_IDENTITY.to_owned(),
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: height,
        to_height: height,
        block_count: 1,
        workload_density: measurements.workload_density,
        canonical_artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        canonical_block_facts_digest_evidence: measurements
            .canonical_block_facts_digest_evidence()?,
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
    materialized_view_preset: Option<MaterializedViewPreset>,
) -> Result<ReplayConfig> {
    Ok(ReplayConfig {
        fixture_directory: fixture_directory.path().to_path_buf(),
        store_path: store_directory.path().join("canonical"),
        block_prepare_concurrency: NonZeroU32::new(2).ok_or_else(|| eyre!("2 is non-zero"))?,
        max_response_bytes: None,
        source_segment_max_blocks: None,
        source_segment_target_response_bytes: None,
        source_fetch_max_in_flight_requests: None,
        source_fetch_max_in_flight_bytes: None,
        block_prepare_memory_watermark_bytes: None,
        source_segment_delay_millis: 0,
        canonical_block_cache_bytes: None,
        materialized_view_preset,
        materialized_view_replay_scope: MaterializedViewReplayScope::FixedRange,
        software_revision: Some("test-revision".to_owned()),
        trial_id: None,
        fixture_cache_policy: None,
        run_started_at_unix_millis: 1,
        runner_id: Some("test-runner".to_owned()),
        cpu_limit_cores: Some(2.0),
        memory_limit_bytes: Some(1024 * 1024 * 1024),
        storage_class: Some("test-tmpfs".to_owned()),
        image_reference: Some(format!("sha256:{}", "a".repeat(64))),
        canonical_fixture_replay_thresholds: None,
    })
}

#[tokio::test]
async fn replay_reports_source_admission_and_delay_settings() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.max_response_bytes = NonZeroU64::new(67_108_864);
    config.source_segment_max_blocks = NonZeroU32::new(64);
    config.source_segment_target_response_bytes = NonZeroU64::new(33_554_432);
    config.source_fetch_max_in_flight_requests = NonZeroU32::new(12);
    config.source_fetch_max_in_flight_bytes = NonZeroU64::new(156_249_984);
    config.block_prepare_memory_watermark_bytes = NonZeroU64::new(156_249_984);
    config.source_segment_delay_millis = 37;

    let report = replay_fixture(config, None).await?;

    assert_eq!(report.replay.max_response_bytes, 67_108_864);
    assert_eq!(report.replay.source_segment_max_blocks, 64);
    assert_eq!(
        report.replay.source_segment_target_response_bytes,
        33_554_432
    );
    assert_eq!(report.replay.source_fetch_max_in_flight_requests, 12);
    assert_eq!(report.replay.source_fetch_max_in_flight_bytes, 156_249_984);
    assert_eq!(
        report.replay.block_prepare_memory_watermark_bytes,
        156_249_984
    );
    assert_eq!(report.replay.source_segment_delay_millis, 37);
    Ok(())
}

#[tokio::test]
async fn replay_resolves_pipeline_watermarks_from_the_resource_envelope() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.cpu_limit_cores = Some(10.0);
    config.memory_limit_bytes = Some(10 * 1024 * 1024 * 1024);
    config.max_response_bytes = NonZeroU64::new(64 * 1024 * 1024);
    config.source_fetch_max_in_flight_bytes = None;
    config.block_prepare_memory_watermark_bytes = None;

    let report = replay_fixture(config, None).await?;

    assert_eq!(
        report.replay.source_fetch_max_in_flight_bytes,
        160 * 1024 * 1024
    );
    assert_eq!(
        report.replay.block_prepare_memory_watermark_bytes,
        160 * 1024 * 1024
    );
    assert_eq!(report.replay.block_prepare_concurrency, 2);
    Ok(())
}

#[tokio::test]
async fn replay_rejects_source_segment_target_above_response_limit() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.max_response_bytes = NonZeroU64::new(64 * 1024 * 1024);
    config.source_segment_target_response_bytes = NonZeroU64::new(128 * 1024 * 1024);

    let Some(error) = replay_fixture(config, None).await.err() else {
        return Err(eyre!(
            "replay must reject a source segment target above its response limit"
        ));
    };

    assert!(
        error
            .to_string()
            .contains("source segment target 134217728 exceeds maximum response 67108864")
    );
    Ok(())
}

fn write_starting_checkpoint_manifest(
    store_path: &std::path::Path,
    canonical_position: &serde_json::Value,
) -> Result<()> {
    std::fs::create_dir_all(store_path)?;
    let manifest = serde_json::json!({
        "format_version": 1,
        "network": "zcash-regtest",
        "canonical_position": canonical_position,
    });
    std::fs::write(
        store_path.join("zinder-benchmark-starting-store.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn assert_materialized_view_report(
    report: &zinder_bench::report::CanonicalStoreRangeReplayReport,
    materialized_view_preset: &'static str,
) {
    assert_eq!(report.fixture.workload_density.block_count, 1);
    assert_eq!(report.fixture.workload_density.transaction_count, 1);
    assert_eq!(report.replay.tip_height_after, Some(1));
    assert_eq!(report.replay.starting_canonical_state.tip_height, None);
    assert_eq!(
        report.replay.starting_canonical_state.tip_hash_rpc_hex,
        None
    );
    assert_eq!(report.replay.starting_canonical_state.chain_epoch_id, None);
    assert_eq!(
        report
            .replay
            .starting_canonical_state
            .artifact_schema_version,
        None
    );
    assert_eq!(
        report
            .replay
            .starting_canonical_state
            .checkpoint_manifest_sha256,
        None
    );
    assert_eq!(report.replay.blocks_committed, 1);
    assert_materialized_view_replay_measurements(report, materialized_view_preset);
    assert_eq!(report.fixture.digest_sha256.len(), 64);
    assert!(
        report
            .replay
            .canonical_writer
            .rocksdb_resource_budget
            .block_cache_bytes
            > 0
    );
    assert_eq!(report.storage_candidate.canonical_engine, "rocksdb");
    assert_eq!(report.storage_candidate.topology, "rocksdb-single-host");
    assert_eq!(
        report.provenance.software_revision.as_deref(),
        Some("test-revision")
    );
    assert_eq!(report.provenance.runner.id.as_deref(), Some("test-runner"));
    assert_eq!(
        report.acceptance.canonical_fixture_replay.scope,
        "fixture-range"
    );
}

fn assert_materialized_view_replay_measurements(
    report: &zinder_bench::report::CanonicalStoreRangeReplayReport,
    materialized_view_preset: &'static str,
) {
    assert_eq!(
        report.replay.materialized_view_preset,
        Some(materialized_view_preset)
    );
    assert_eq!(
        report.replay.materialized_view_replay_scope,
        Some("fixed-range")
    );
    assert!(
        report
            .replay
            .materialized_view_build_wall_clock_seconds
            .is_some()
    );
    assert!(
        report
            .replay
            .materialized_view_logical_write_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(report.replay.materialized_view_row_count.is_some());
    assert_eq!(
        report.replay.materialized_view_event_cursor_at_tip,
        Some(true)
    );
    assert!(
        report
            .replay
            .materialized_view_store_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        report
            .replay
            .materialized_view_store_reopen_seconds
            .is_some()
    );
}

fn assert_no_target_wallet_acceptance_claim(
    report: &zinder_bench::report::CanonicalStoreRangeReplayReport,
) -> Result<()> {
    let report = serde_json::to_value(report)?;
    assert!(report.get("lifecycle").is_none());
    assert!(report["acceptance"].get("wallet_build").is_none());
    assert!(report["acceptance"].get("wallet_build_lifecycle").is_none());
    assert!(report["acceptance"].get("wallet_ready").is_none());
    Ok(())
}

fn assert_materialized_view_at_canonical_tip(
    canonical_store: &PrimaryChainStore,
    materialized_view_store: &zinder_materialized_views::MaterializedViewStore,
) -> Result<()> {
    let expected_cursor = canonical_store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor
        .ok_or_else(|| eyre!("committed fixture must expose a canonical event cursor"))?;
    for consumer_name in materialized_view_store
        .chain_event_consumer_names()
        .chain(materialized_view_store.event_only_chain_event_consumer_names())
    {
        assert_eq!(
            materialized_view_store.get_chain_event_cursor(consumer_name)?,
            Some(expected_cursor.as_bytes().to_vec()),
            "materialized view {} must reach the canonical event tip",
            consumer_name.as_str()
        );
    }
    Ok(())
}

#[test]
fn thresholded_config_requires_immutable_structured_provenance_and_telemetry() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );

    assert!(config.validate(false).is_err());
    config.image_reference = Some("zinder-bench:mutable".to_owned());
    assert!(config.validate(true).is_err());
    config.image_reference = Some(format!("@sha256:{}", "a".repeat(64)));
    assert!(config.validate(true).is_err());
    config.image_reference = Some(format!("sha256:{}", "a".repeat(64)));
    config.cpu_limit_cores = None;
    assert!(config.validate(true).is_err());
    config.cpu_limit_cores = Some(2.0);
    config.materialized_view_preset = Some(MaterializedViewPreset::Wallet);
    assert!(config.validate(true).is_err());
    config.materialized_view_preset = None;
    config.validate(true)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_materialized_view_presets_report_diagnostics_not_wallet_acceptance() -> Result<()>
{
    let fixture_directory = write_regtest_fixture()?;
    let wallet_store_directory = tempdir()?;
    let wallet_report = replay_fixture(
        replay_config(
            &fixture_directory,
            &wallet_store_directory,
            Some(MaterializedViewPreset::Wallet),
        )?,
        None,
    )
    .await?;
    assert_materialized_view_report(&wallet_report, "wallet");
    assert_eq!(
        wallet_report
            .storage_candidate
            .diagnostic_materialized_view_engine,
        Some("rocksdb")
    );
    assert_no_target_wallet_acceptance_claim(&wallet_report)?;

    let explorer_store_directory = tempdir()?;
    let explorer_report = replay_fixture(
        replay_config(
            &fixture_directory,
            &explorer_store_directory,
            Some(MaterializedViewPreset::Explorer),
        )?,
        None,
    )
    .await?;
    assert_materialized_view_report(&explorer_report, "explorer");
    assert_eq!(
        explorer_report
            .storage_candidate
            .diagnostic_materialized_view_engine,
        Some("rocksdb")
    );
    assert_no_target_wallet_acceptance_claim(&explorer_report)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explorer_replay_bootstraps_ranking_while_wallet_remains_ranking_free() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let explorer_store_directory = tempdir()?;
    replay_fixture(
        replay_config(
            &fixture_directory,
            &explorer_store_directory,
            Some(MaterializedViewPreset::Explorer),
        )?,
        None,
    )
    .await?;

    let explorer_store_path = explorer_store_directory.path().join("canonical");
    let explorer_store =
        open_primary_materialized_view_store_for_canonical_with_materialized_view_preset(
            &explorer_store_path,
            RocksDbResourceBudget::materialized_view_writer_defaults(),
            MaterializedViewPreset::Explorer,
        )?;
    let explorer_canonical_store = PrimaryChainStore::open(
        &explorer_store_path,
        zinder_store::ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    assert_materialized_view_at_canonical_tip(&explorer_canonical_store, &explorer_store)?;
    let active = TransparentAddressRankingConsumer::active_metadata(&explorer_store)?
        .ok_or_else(|| eyre!("explorer replay must activate a ranking generation"))?;
    assert!(active.generation > 0);
    assert_eq!(
        active.coverage.balance_complete_through_height,
        BlockHeight::new(1)
    );
    assert!(TransparentAddressRankingConsumer::build_metadata(&explorer_store)?.is_none());
    let ranking_cursor = explorer_store
        .get_chain_event_cursor(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)?
        .ok_or_else(|| eyre!("explorer replay must commit the ranking cursor"))?;
    assert_eq!(
        Some(ranking_cursor),
        explorer_store.get_chain_event_cursor(TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME)?
    );
    drop(explorer_store);

    let wallet_store_directory = tempdir()?;
    replay_fixture(
        replay_config(
            &fixture_directory,
            &wallet_store_directory,
            Some(MaterializedViewPreset::Wallet),
        )?,
        None,
    )
    .await?;

    let wallet_store_path = wallet_store_directory.path().join("canonical");
    let wallet_store =
        open_primary_materialized_view_store_for_canonical_with_materialized_view_preset(
            &wallet_store_path,
            RocksDbResourceBudget::materialized_view_writer_defaults(),
            MaterializedViewPreset::Wallet,
        )?;
    let wallet_canonical_store = PrimaryChainStore::open(
        &wallet_store_path,
        zinder_store::ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    assert_materialized_view_at_canonical_tip(&wallet_canonical_store, &wallet_store)?;
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
    let materialized_view_store =
        zinder_materialized_views::MaterializedViewStore::open_with_materialized_view_preset(
            zinder_materialized_views::MaterializedViewStore::path_for_canonical(&range_store_path),
            MaterializedViewPreset::Explorer,
            zinder_materialized_views::MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: MaterializedViewPreset::Explorer.consumer_schemas(),
                rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            },
        )?;
    let seeded_cursor =
        seed_materialized_view_replay_at_canonical_tip(&canonical_store, &materialized_view_store)?
            .ok_or_else(|| eyre!("committed canonical history must have a cursor"))?;
    for consumer_name in materialized_view_store.chain_event_consumer_names() {
        let cursor = materialized_view_store.get_chain_event_cursor(consumer_name)?;
        if consumer_name == TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME {
            assert_eq!(cursor, None);
        } else {
            assert_eq!(cursor, Some(seeded_cursor.as_bytes().to_vec()));
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn materialized_view_replay_rejects_a_preexisting_materialized_view_store() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let config = replay_config(
        &fixture_directory,
        &store_directory,
        Some(MaterializedViewPreset::Wallet),
    )?;
    std::fs::create_dir_all(&config.store_path)?;
    let materialized_view_store =
        zinder_materialized_views::MaterializedViewStore::open_with_materialized_view_preset(
            zinder_materialized_views::MaterializedViewStore::path_for_canonical(
                &config.store_path,
            ),
            MaterializedViewPreset::Wallet,
            zinder_materialized_views::MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: MaterializedViewPreset::Wallet.consumer_schemas(),
                rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            },
        )?;
    drop(materialized_view_store);

    let Some(error) = replay_fixture(config, None).await.err() else {
        return Err(eyre!(
            "materialized-view replay must reject a populated starting materialized-view path"
        ));
    };
    assert!(error.to_string().contains("fresh materialized-view store"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thresholded_genesis_replay_accepts_a_proven_empty_start_without_manifest() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );
    let report = replay_fixture(config, Some(install_recorder()?)).await?;
    assert_eq!(
        report.replay.starting_canonical_state.kind,
        StartingCanonicalStateKind::Empty
    );
    assert_eq!(
        report
            .replay
            .starting_canonical_state
            .checkpoint_manifest_sha256,
        None
    );
    assert_eq!(report.replay.starting_canonical_state.chain_epoch_id, None);
    assert_eq!(report.replay.starting_canonical_state.tip_height, None);
    assert_eq!(
        report.replay.starting_canonical_state.tip_hash_rpc_hex,
        None
    );
    let source_fetch = report
        .replay
        .source_fetch_attribution
        .as_ref()
        .ok_or_else(|| eyre!("replay report must include source-fetch attribution"))?;
    assert!(source_fetch.completed_segment_request_count >= 1);
    assert!(source_fetch.total_connected_blocks_returned >= 1);
    assert!(source_fetch.total_response_payload_bytes > 0);
    assert!(source_fetch.completed_segment_requests_per_second > 0.0);
    assert!(source_fetch.response_payload_bytes_per_second > 0.0);
    assert!(source_fetch.cumulative_fetch_chain_segment_task_seconds >= 0.0);
    report.validate_acceptance()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thresholded_non_genesis_replay_requires_a_checkpoint_manifest() -> Result<()> {
    let fixture_directory = write_non_genesis_manifest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory, None)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );

    let Some(error) = replay_fixture(config, Some(install_recorder()?))
        .await
        .err()
    else {
        return Err(eyre!(
            "thresholded non-genesis replay must require checkpoint provenance"
        ));
    };
    assert!(error.to_string().contains("thresholded replay requires"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_rejects_checkpoint_manifest_position_that_disagrees_with_store() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let config = replay_config(&fixture_directory, &store_directory, None)?;
    write_starting_checkpoint_manifest(
        &config.store_path,
        &serde_json::json!({
            "chain_epoch_id": 7,
            "visible_tip_height": 6,
            "visible_tip_hash": "00".repeat(32),
            "artifact_schema_version": zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
            "history_bounds": {"kind": "complete", "first_available_height": 1}
        }),
    )?;

    let Some(error) = replay_fixture(config, None).await.err() else {
        return Err(eyre!(
            "replay must reject checkpoint position that disagrees with the store"
        ));
    };
    assert!(error.to_string().contains("opened store is empty"));
    Ok(())
}
