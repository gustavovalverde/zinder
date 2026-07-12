#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::tempdir;
use zinder_bench::{
    fixture::{
        ActivationRecord, FIXTURE_FORMAT_VERSION, FixtureManifest, SubtreeRootSet, write_segment,
    },
    replay::{ReplayConfig, replay_fixture},
};
use zinder_core::{BlockHeight, Network, wire::encode_zinder_native_chain_name};
use zinder_source::SourceBlock;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_commits_a_genesis_block_over_the_real_pipeline() -> Result<()> {
    let fixture: Value = serde_json::from_str(REGTEST_BLOCK_1)?;
    let raw_block_hex = fixture
        .get("raw_block_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;
    let raw_block_bytes = hex::decode(raw_block_hex)?;
    let block_one = SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(1),
        raw_block_bytes,
    )?;

    let fixture_dir = tempdir()?;
    let descriptor = write_segment(fixture_dir.path(), 0, std::slice::from_ref(&block_one))?;
    let manifest = FixtureManifest {
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: 1,
        to_height: 1,
        block_count: 1,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        tip_hash_hex: hex::encode(block_one.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![descriptor],
        subtree_roots: SubtreeRootSet::default(),
    };
    manifest.write(fixture_dir.path())?;

    let store_dir = tempdir()?;
    let concurrency = NonZeroU32::new(2).ok_or_else(|| eyre!("2 is non-zero"))?;
    let report = replay_fixture(
        ReplayConfig {
            fixture_directory: fixture_dir.path().to_path_buf(),
            store_path: store_dir.path().join("canonical"),
            block_prepare_concurrency: concurrency,
            canonical_block_cache_bytes: None,
            run_derive: false,
        },
        None,
    )
    .await?;

    assert_eq!(report.replay.tip_height_after, Some(1));
    assert_eq!(report.replay.blocks_committed, 1);
    assert_eq!(report.fixture.from_height, 1);
    assert_eq!(report.fixture.to_height, 1);

    Ok(())
}
