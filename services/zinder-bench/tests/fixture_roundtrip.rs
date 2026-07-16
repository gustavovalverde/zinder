#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs::OpenOptions, io::Write, num::NonZeroU32, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::{Result, eyre};
use parking_lot::Mutex;
use serde_json::Value;
use tempfile::tempdir;
use zinder_bench::fixture::{
    ActivationRecord, CanonicalBlockFactsDigestEvidence, FIXTURE_CONTRACT_IDENTITY,
    FIXTURE_FORMAT_VERSION, FixtureManifest, FixtureNodeSource, SegmentDescriptor, SubtreeRootSet,
    WorkloadDensity, read_segment_blocks, write_segment,
};
use zinder_bench::{
    canonical_fixture_replay::{CanonicalFixtureReplayPlan, capture_canonical_fixture_replay_plan},
    capture::measure_fixture_blocks,
};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CommitmentTreeCheckpoint, CommitmentTreeFrontier,
    CommitmentTreeFrontiers, Network, NetworkUpgradeActivations, ShieldedProtocol,
    wire::encode_zinder_native_chain_name,
};
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceChainCursor, SourceChainSegmentLimits,
    SourceError,
};
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

#[derive(Clone)]
struct CheckpointSource {
    checkpoints: Arc<[CommitmentTreeCheckpoint]>,
    requested_heights: Arc<Mutex<Vec<BlockHeight>>>,
}

impl CheckpointSource {
    fn new(checkpoints: impl Into<Arc<[CommitmentTreeCheckpoint]>>) -> Self {
        Self {
            checkpoints: checkpoints.into(),
            requested_heights: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl NodeSource for CheckpointSource {
    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities::default()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        Err(SourceError::BlockUnavailable {
            height,
            reason: "checkpoint source serves only tree state".to_owned(),
        })
    }

    async fn fetch_chain_checkpoint(
        &self,
        height: BlockHeight,
        _network_upgrade_activations: &NetworkUpgradeActivations,
    ) -> Result<CommitmentTreeCheckpoint, SourceError> {
        self.requested_heights.lock().push(height);
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.block_id.height == height)
            .cloned()
            .ok_or_else(|| SourceError::BlockUnavailable {
                height,
                reason: "checkpoint source has no tree state at this height".to_owned(),
            })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        self.checkpoints
            .last()
            .map(|checkpoint| checkpoint.block_id)
            .ok_or_else(|| SourceError::NodeUnavailable {
                reason: "checkpoint source is empty".to_owned(),
            })
    }
}

fn write_regtest_fixture() -> Result<RegtestFixtureCase> {
    let block = load_regtest_block(REGTEST_BLOCK_603)?;
    let directory = tempdir()?;
    let blocks = vec![block.clone()];
    let descriptor = write_segment(directory.path(), 0, &blocks)?;
    let measurements = measure_fixture_blocks(&blocks, &sample_regtest_upgrade_activations())?;
    let manifest = FixtureManifest {
        contract_identity: FIXTURE_CONTRACT_IDENTITY.to_owned(),
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

fn checkpoint_frontiers_at(height: BlockHeight) -> CommitmentTreeFrontiers {
    CommitmentTreeFrontiers::from_validated_parts(
        Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Sapling)),
        Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Orchard)),
        (height.value() >= 603).then(|| CommitmentTreeFrontier::empty(ShieldedProtocol::Ironwood)),
    )
}

fn fixture_checkpoint_source(
    fixture: &RegtestFixtureCase,
) -> (
    CheckpointSource,
    CommitmentTreeCheckpoint,
    CommitmentTreeCheckpoint,
) {
    let predecessor_height = BlockHeight::new(fixture.manifest.from_height - 1);
    let predecessor = CommitmentTreeCheckpoint::new(
        BlockId::new(predecessor_height, fixture.block.parent_hash),
        fixture.block.block_time_seconds.saturating_sub(1),
        checkpoint_frontiers_at(predecessor_height),
    );
    let fixed_tip = CommitmentTreeCheckpoint::new(
        BlockId::new(fixture.block.height, fixture.block.hash),
        fixture.block.block_time_seconds,
        checkpoint_frontiers_at(fixture.block.height),
    );
    let source = CheckpointSource::new(vec![predecessor.clone(), fixed_tip.clone()]);
    (source, predecessor, fixed_tip)
}

#[tokio::test]
async fn canonical_replay_plan_captures_digest_bound_predecessor_and_tip_checkpoints() -> Result<()>
{
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let predecessor_height = BlockHeight::new(fixture.manifest.from_height - 1);
    let (source, predecessor, fixed_tip) = fixture_checkpoint_source(&fixture);
    let manifest_path = fixture.directory.path().join("manifest.json");
    let original_manifest_bytes = std::fs::read(&manifest_path)?;

    let captured =
        capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations)
            .await?;
    let admitted = CanonicalFixtureReplayPlan::read(
        fixture.directory.path(),
        &fixture.manifest,
        &activations,
    )?;

    assert_eq!(
        source.requested_heights.lock().as_slice(),
        [predecessor_height, fixture.block.height]
    );
    assert_eq!(captured, admitted);
    assert_eq!(admitted.history_predecessor_checkpoint()?, predecessor);
    assert_eq!(admitted.source_tip_checkpoint()?, fixed_tip);
    assert_eq!(admitted.digest_sha256()?.len(), 64);
    assert_eq!(std::fs::read(manifest_path)?, original_manifest_bytes);

    let sidecar =
        std::fs::read_to_string(fixture.directory.path().join("canonical-replay-plan.json"))?;
    assert!(!sidecar.contains("tree_size"));
    assert!(sidecar.contains("final_root_hex"));
    assert!(sidecar.contains("final_state_hex"));
    let encoded_plan: Value = serde_json::from_str(&sidecar)?;
    assert_eq!(
        encoded_plan["contract_identity"],
        "canonical-fixture-replay-plan"
    );
    assert_eq!(encoded_plan["format_version"], 1);
    assert_eq!(
        encoded_plan["network_upgrade_activations_fingerprint_version"],
        1
    );
    assert!(
        encoded_plan
            .get("canonical_replay_plan_format_version")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_checkpoint_capture_preserves_existing_sidecar_bytes() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let (source, _, _) = fixture_checkpoint_source(&fixture);
    capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations).await?;
    let sidecar_path = fixture.directory.path().join("canonical-replay-plan.json");
    let captured_bytes = std::fs::read(&sidecar_path)?;
    let second_source = source.clone();

    let second_capture = capture_canonical_fixture_replay_plan(
        fixture.directory.path(),
        &second_source,
        &activations,
    )
    .await;

    let error = second_capture.err().ok_or_else(|| {
        eyre!("canonical fixture checkpoint capture must refuse an existing sidecar")
    })?;
    assert!(error.to_string().contains("already exists"));
    assert_eq!(std::fs::read(sidecar_path)?, captured_bytes);
    assert_eq!(second_source.requested_heights.lock().len(), 2);
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_replay_plan_rejects_a_different_manifest_digest() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let (source, _, _) = fixture_checkpoint_source(&fixture);
    capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations).await?;
    let sidecar_path = fixture.directory.path().join("canonical-replay-plan.json");
    let mut encoded_plan: Value = serde_json::from_slice(&std::fs::read(&sidecar_path)?)?;
    encoded_plan["fixture_manifest_sha256"] = Value::String("00".repeat(32));
    std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&encoded_plan)?)?;

    let error =
        CanonicalFixtureReplayPlan::read(fixture.directory.path(), &fixture.manifest, &activations)
            .err()
            .ok_or_else(|| eyre!("a replay plan bound to another manifest must be rejected"))?;

    assert!(error.to_string().contains("manifest SHA-256"));
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_checkpoint_capture_rejects_a_different_activation_fingerprint()
-> Result<()> {
    let fixture = write_regtest_fixture()?;
    let (source, _, _) = fixture_checkpoint_source(&fixture);
    let mismatched_activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);

    let error = capture_canonical_fixture_replay_plan(
        fixture.directory.path(),
        &source,
        &mismatched_activations,
    )
    .await
    .err()
    .ok_or_else(|| eyre!("checkpoint capture must reject a different activation table"))?;

    assert!(error.to_string().contains("activation fingerprint"));
    assert!(source.requested_heights.lock().is_empty());
    assert!(
        !fixture
            .directory
            .path()
            .join("canonical-replay-plan.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_replay_plan_rejects_a_malformed_frontier() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let (source, _, _) = fixture_checkpoint_source(&fixture);
    capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations).await?;
    let sidecar_path = fixture.directory.path().join("canonical-replay-plan.json");
    let mut encoded_plan: Value = serde_json::from_slice(&std::fs::read(&sidecar_path)?)?;
    encoded_plan["source_tip_checkpoint"]["frontiers"]["sapling"]["final_state_hex"] =
        Value::String("00".to_owned());
    std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&encoded_plan)?)?;

    let error =
        CanonicalFixtureReplayPlan::read(fixture.directory.path(), &fixture.manifest, &activations)
            .err()
            .ok_or_else(|| eyre!("a malformed canonical finalState must be rejected"))?;

    assert!(
        error
            .to_string()
            .contains("invalid source tip checkpoint Sapling frontier")
    );
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_checkpoint_capture_rejects_a_disconnected_predecessor() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let (_, mut predecessor, fixed_tip) = fixture_checkpoint_source(&fixture);
    predecessor.block_id = BlockId::new(
        predecessor.block_id.height,
        BlockHash::from_bytes([0x42; 32]),
    );
    let source = CheckpointSource::new(vec![predecessor, fixed_tip]);

    let error =
        capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations)
            .await
            .err()
            .ok_or_else(|| {
                eyre!("a checkpoint disconnected from the first block must be rejected")
            })?;

    assert!(
        error
            .to_string()
            .contains("does not link to fixture first block")
    );
    assert!(
        !fixture
            .directory
            .path()
            .join("canonical-replay-plan.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_checkpoint_capture_rejects_a_wrong_source_tip() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let activations = sample_regtest_upgrade_activations();
    let (_, predecessor, mut fixed_tip) = fixture_checkpoint_source(&fixture);
    fixed_tip.block_id = BlockId::new(fixed_tip.block_id.height, BlockHash::from_bytes([0x24; 32]));
    let source = CheckpointSource::new(vec![predecessor, fixed_tip]);

    let error =
        capture_canonical_fixture_replay_plan(fixture.directory.path(), &source, &activations)
            .await
            .err()
            .ok_or_else(|| eyre!("a checkpoint different from the fixture tip must be rejected"))?;

    assert!(error.to_string().contains("and manifest tip"));
    assert!(
        !fixture
            .directory
            .path()
            .join("canonical-replay-plan.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn canonical_fixture_checkpoint_capture_rejects_disconnected_segment_boundaries_before_rpc()
-> Result<()> {
    let first_block = load_regtest_block(REGTEST_BLOCK_603)?;
    let second_block = SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(604),
        first_block.raw_block_bytes.clone(),
    )?;
    let directory = tempdir()?;
    let first_segment = write_segment(directory.path(), 0, &[first_block])?;
    let second_segment = write_segment(directory.path(), 1, std::slice::from_ref(&second_block))?;
    let manifest = FixtureManifest {
        contract_identity: FIXTURE_CONTRACT_IDENTITY.to_owned(),
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: 603,
        to_height: 604,
        block_count: 2,
        workload_density: WorkloadDensity {
            block_count: 2,
            ..WorkloadDensity::default()
        },
        current_schema_oracle_artifact_schema_version:
            zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence {
            block_digest_version: zinder_core::CanonicalBlockFactsDigestVersion::CURRENT.value(),
            sequence_digest_version: zinder_core::CanonicalBlockFactsSequenceDigestVersion::CURRENT
                .value(),
            block_count: 2,
            sequence_digest_sha256: "00".repeat(32),
        },
        tip_hash_hex: hex::encode(second_block.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![first_segment, second_segment],
        subtree_roots: SubtreeRootSet::default(),
    };
    manifest.write(directory.path())?;
    let source = CheckpointSource::new(Vec::<CommitmentTreeCheckpoint>::new());

    let error = capture_canonical_fixture_replay_plan(
        directory.path(),
        &source,
        &sample_regtest_upgrade_activations(),
    )
    .await
    .err()
    .ok_or_else(|| eyre!("disconnected fixture segments must fail admission"))?;

    assert!(error.to_string().contains("not an ordered connected pair"));
    assert!(source.requested_heights.lock().is_empty());
    assert!(!directory.path().join("canonical-replay-plan.json").exists());
    Ok(())
}

#[test]
fn fixture_v1_requires_exact_contract_identity() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut missing_identity = serde_json::to_value(&fixture.manifest)?;
    missing_identity
        .as_object_mut()
        .ok_or_else(|| eyre!("fixture manifest must encode as an object"))?
        .remove("contract_identity");
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&missing_identity)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!("fixture v1 contract identity must be mandatory"));
    };
    assert!(error.to_string().contains("contract_identity"));

    let mut old_identity = fixture.manifest;
    old_identity.contract_identity = "zinder-bench-fixture-manifest".to_owned();
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&old_identity)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!("fixture v1 must reject an earlier contract identity"));
    };
    assert!(error.to_string().contains("fixture contract identity"));
    Ok(())
}

#[test]
fn fixture_v1_rejects_unknown_manifest_fields() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let mut unknown_field = serde_json::to_value(&fixture.manifest)?;
    unknown_field
        .as_object_mut()
        .ok_or_else(|| eyre!("fixture manifest must encode as an object"))?
        .insert("legacy_schema_version".to_owned(), serde_json::json!(14));
    std::fs::write(
        fixture.directory.path().join("manifest.json"),
        serde_json::to_vec_pretty(&unknown_field)?,
    )?;
    let Err(error) = FixtureManifest::read(fixture.directory.path()) else {
        return Err(eyre!("fixture v1 must reject unknown manifest fields"));
    };
    assert!(error.to_string().contains("unknown field"));
    Ok(())
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

#[tokio::test(start_paused = true)]
async fn fixture_source_delays_each_segment_response_by_the_configured_duration() -> Result<()> {
    let fixture = write_regtest_fixture()?;
    let source_segment_delay = Duration::from_millis(250);
    let source = FixtureNodeSource::open_with_segment_delay(
        fixture.directory.path(),
        &fixture.manifest,
        source_segment_delay,
    )?;
    let segment_limits = SourceChainSegmentLimits::new(
        SourceChainCursor::before_height(BlockHeight::new(603)),
        NonZeroU32::MIN,
        u64::MAX,
        u64::MAX,
    );

    let segment_fetch =
        tokio::spawn(async move { source.fetch_chain_segment(segment_limits).await });
    tokio::task::yield_now().await;
    assert!(!segment_fetch.is_finished());

    tokio::time::advance(Duration::from_millis(249)).await;
    tokio::task::yield_now().await;
    assert!(!segment_fetch.is_finished());

    tokio::time::advance(Duration::from_millis(1)).await;
    let segment = segment_fetch.await??;
    assert_eq!(segment.stats().connected_blocks(), 1);
    Ok(())
}

#[test]
fn fixture_v1_requires_digest_evidence() -> Result<()> {
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
        return Err(eyre!("fixture v1 digest evidence must be mandatory"));
    };
    assert!(
        error
            .to_string()
            .contains("canonical_block_facts_digest_evidence")
    );
    Ok(())
}

#[test]
fn fixture_v1_rejects_inconsistent_counts() -> Result<()> {
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
fn fixture_v1_rejects_unknown_digest_version() -> Result<()> {
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
fn fixture_v1_rejects_uppercase_sequence_digest_hex() -> Result<()> {
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
