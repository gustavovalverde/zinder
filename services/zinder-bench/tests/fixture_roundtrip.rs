#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::tempdir;
use zinder_bench::fixture::{
    ActivationRecord, FIXTURE_FORMAT_VERSION, FixtureManifest, FixtureNodeSource, SubtreeRootSet,
    read_segment_blocks, write_segment,
};
use zinder_core::{BlockHeight, Network, wire::encode_zinder_native_chain_name};
use zinder_source::{NodeSource, SourceBlock};
use zinder_testkit::sample_regtest_upgrade_activations;

const REGTEST_BLOCK_1: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-block-1.json");
const REGTEST_BLOCK_603: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-ironwood-block-603.json");

fn load_regtest_block(fixture_json: &str) -> Result<SourceBlock> {
    let fixture: Value = serde_json::from_str(fixture_json)?;
    let raw_block_hex = fixture
        .get("raw_block_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;
    let height = fixture
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| eyre!("fixture height must be an integer"))?;
    let raw_block_bytes = hex::decode(raw_block_hex)?;
    let height = u32::try_from(height)?;
    Ok(SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(height),
        raw_block_bytes,
    )?)
}

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

#[tokio::test]
async fn segment_and_manifest_round_trip_preserves_blocks() -> Result<()> {
    let block_one = load_regtest_block(REGTEST_BLOCK_1)?;
    let block_six_hundred_three = load_regtest_block(REGTEST_BLOCK_603)?;
    let blocks = vec![block_one.clone(), block_six_hundred_three.clone()];

    let fixture_dir = tempdir()?;
    let descriptor = write_segment(fixture_dir.path(), 0, &blocks)?;
    assert_eq!(descriptor.block_count, 2);
    assert_eq!(descriptor.from_height, 1);
    assert_eq!(descriptor.to_height, 603);
    assert_eq!(descriptor.sha256.len(), 64);

    let read_back = read_segment_blocks(fixture_dir.path(), &descriptor, Network::ZcashRegtest)?;
    assert_eq!(read_back, blocks);

    let manifest = FixtureManifest {
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: 1,
        to_height: 603,
        block_count: 2,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        tip_hash_hex: hex::encode(block_six_hundred_three.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![descriptor.clone()],
        subtree_roots: SubtreeRootSet::default(),
    };
    manifest.write(fixture_dir.path())?;

    let reloaded = FixtureManifest::read(fixture_dir.path())?;
    assert_eq!(reloaded.network_typed()?, Network::ZcashRegtest);
    assert_eq!(reloaded.to_height, 603);
    assert_eq!(
        reloaded.activations_typed()?.activations().len(),
        sample_regtest_upgrade_activations().activations().len()
    );

    let source = FixtureNodeSource::open(fixture_dir.path(), &reloaded)?;
    let fetched = source.fetch_block_at(BlockHeight::new(1)).await?;
    assert_eq!(fetched, block_one);
    assert_eq!(source.tip_id().await?, reloaded.tip_id()?);

    Ok(())
}
