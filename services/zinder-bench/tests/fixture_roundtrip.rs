#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs::OpenOptions, io::Write};

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::tempdir;
use zinder_bench::capture::measure_fixture_blocks;
use zinder_bench::fixture::{
    ActivationRecord, FIXTURE_FORMAT_VERSION, FixtureManifest, FixtureNodeSource,
    SegmentDescriptor, SubtreeRootSet, read_segment_blocks, write_segment,
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

struct RegtestFixtureCase {
    directory: tempfile::TempDir,
    block: SourceBlock,
    descriptor: SegmentDescriptor,
    manifest: FixtureManifest,
}

fn write_regtest_fixture() -> Result<RegtestFixtureCase> {
    let block = load_regtest_block(REGTEST_BLOCK_603)?;
    let directory = tempdir()?;
    let blocks = vec![block.clone()];
    let descriptor = write_segment(directory.path(), 0, &blocks)?;
    let measurements = measure_fixture_blocks(&blocks, &sample_regtest_upgrade_activations())?;
    let manifest = FixtureManifest {
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: 603,
        to_height: 603,
        block_count: 1,
        workload_density: measurements.workload_density,
        current_schema_oracle_artifact_schema_version:
            zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        canonical_block_facts_digest_evidence: measurements
            .canonical_block_facts_digest_evidence()?,
        tip_hash_hex: hex::encode(block.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![descriptor.clone()],
        subtree_roots: SubtreeRootSet::default(),
    };
    manifest.write(directory.path())?;
    Ok(RegtestFixtureCase {
        directory,
        block,
        descriptor,
        manifest,
    })
}

#[test]
fn segment_rejects_noncontiguous_blocks() -> Result<()> {
    let block_one = load_regtest_block(REGTEST_BLOCK_1)?;
    let block_six_hundred_three = load_regtest_block(REGTEST_BLOCK_603)?;
    let fixture_dir = tempdir()?;
    let noncontiguous = write_segment(
        fixture_dir.path(),
        99,
        &[block_one, block_six_hundred_three],
    );
    assert!(noncontiguous.is_err());
    Ok(())
}

#[tokio::test]
async fn segment_and_manifest_round_trip_preserves_blocks() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    assert_eq!(fixture.descriptor.block_count, 1);
    assert_eq!(fixture.descriptor.from_height, 603);
    assert_eq!(fixture.descriptor.to_height, 603);
    assert_eq!(fixture.descriptor.sha256.len(), 64);

    let read_back = read_segment_blocks(
        fixture.directory.path(),
        &fixture.descriptor,
        Network::ZcashRegtest,
    )?;
    assert_eq!(read_back, vec![fixture.block.clone()]);

    let reloaded = FixtureManifest::read(fixture.directory.path())?;
    assert_eq!(reloaded.digest_sha256()?.len(), 64);
    assert_eq!(reloaded.network_typed()?, Network::ZcashRegtest);
    assert_eq!(reloaded.to_height, 603);
    assert_eq!(reloaded.workload_density.block_count, 1);
    assert_eq!(reloaded.workload_density.transaction_count, 2);
    assert!(reloaded.workload_density.raw_block_bytes > 0);
    assert_eq!(
        reloaded
            .canonical_block_facts_digest_evidence
            .block_digest_version,
        zinder_core::CanonicalBlockFactsDigestVersion::CURRENT.value()
    );
    assert_eq!(
        reloaded
            .canonical_block_facts_digest_evidence
            .sequence_digest_version,
        zinder_core::CanonicalBlockFactsSequenceDigestVersion::CURRENT.value()
    );
    assert_eq!(
        reloaded.canonical_block_facts_digest_evidence.block_count,
        1
    );
    assert_eq!(
        reloaded
            .canonical_block_facts_digest_evidence
            .sequence_digest_sha256
            .len(),
        64
    );
    assert_eq!(
        reloaded.activations_typed()?.activations().len(),
        sample_regtest_upgrade_activations().activations().len()
    );

    let source = FixtureNodeSource::open(fixture.directory.path(), &reloaded)?;
    let fetched = source.fetch_block_at(BlockHeight::new(603)).await?;
    assert_eq!(fetched, fixture.block);
    assert_eq!(source.tip_id().await?, reloaded.tip_id()?);

    let segment_path = fixture.directory.path().join(&fixture.descriptor.file);
    OpenOptions::new()
        .append(true)
        .open(&segment_path)?
        .write_all(b"corruption")?;
    let Err(error) = FixtureNodeSource::open(fixture.directory.path(), &reloaded) else {
        return Err(eyre!("fixture open must verify each segment digest"));
    };
    assert!(error.to_string().contains("SHA-256"));
    Ok(())
}

#[test]
fn fixture_v3_requires_digest_evidence() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut missing_digest_evidence = serde_json::to_value(&fixture.manifest)?;
    missing_digest_evidence
        .as_object_mut()
        .ok_or_else(|| eyre!("fixture manifest must encode as an object"))?
        .remove("canonical_block_facts_digest_evidence");
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&missing_digest_evidence)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!("fixture v3 digest evidence must be mandatory"));
    };
    assert!(
        error
            .to_string()
            .contains("canonical_block_facts_digest_evidence")
    );
    Ok(())
}

#[test]
fn fixture_v3_rejects_inconsistent_counts() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut inconsistent_digest_count = fixture.manifest.clone();
    inconsistent_digest_count
        .canonical_block_facts_digest_evidence
        .block_count = 0;
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&inconsistent_digest_count)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!(
            "canonical fact digest count must match the fixture range"
        ));
    };
    assert!(error.to_string().contains("digest count"));

    let mut inconsistent = fixture.manifest;
    inconsistent.workload_density.block_count = 0;
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&inconsistent)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!("density block count must match the fixture range"));
    };
    assert!(error.to_string().contains("density block count"));
    Ok(())
}

#[test]
fn fixture_v3_rejects_unknown_digest_version() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut unknown_digest_version = fixture.manifest;
    unknown_digest_version
        .canonical_block_facts_digest_evidence
        .block_digest_version = u16::MAX;
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&unknown_digest_version)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!(
            "unknown canonical fact digest version must fail closed"
        ));
    };
    assert!(
        error
            .to_string()
            .contains("unsupported canonical block facts digest version")
    );

    Ok(())
}

#[test]
fn fixture_v3_rejects_uppercase_sequence_digest_hex() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut uppercase_sequence_digest = fixture.manifest;
    uppercase_sequence_digest
        .canonical_block_facts_digest_evidence
        .sequence_digest_sha256 = "AB".repeat(32);
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&uppercase_sequence_digest)?,
    )?;

    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!(
            "uppercase canonical fact sequence digest must be rejected"
        ));
    };
    assert!(error.to_string().contains("lowercase hexadecimal"));
    Ok(())
}
