//! Digest-bound checkpoint capture and admission for canonical fixture replay.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    num::NonZeroU32,
    path::Path,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CommitmentTreeAccumulator, CommitmentTreeCheckpoint,
    CommitmentTreeFrontier, CommitmentTreeFrontiers, FinalNoteCommitmentRoot,
    NetworkUpgradeActivations, NetworkUpgradeActivationsFingerprint,
    NetworkUpgradeActivationsFingerprintVersion, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange,
};
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceChainSegment, SourceChainSegmentLimits,
    SourceError, SourceSubtreeRoots,
};

use crate::{
    error::BenchError,
    fixture::{FixtureManifest, FixtureNodeSource, read_segment_blocks},
};

/// Digest-bound checkpoint sidecar written beside a version-1 fixture manifest.
pub const CANONICAL_FIXTURE_REPLAY_PLAN_FILE_NAME: &str = "canonical-replay-plan.json";
const CANONICAL_FIXTURE_REPLAY_PLAN_TEMP_FILE_NAME: &str = "canonical-replay-plan.json.tmp";
const CANONICAL_FIXTURE_REPLAY_PLAN_CONTRACT_IDENTITY: &str = "canonical-fixture-replay-plan";
const CANONICAL_FIXTURE_REPLAY_PLAN_FORMAT_VERSION: u32 = 1;
const CANONICAL_FIXTURE_REPLAY_PLAN_DIGEST_DOMAIN: &[u8] =
    b"zinder-bench-canonical-fixture-replay-plan-v1\0";

/// Digest-bound predecessor and fixed-tip checkpoints for canonical fixture replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFixtureReplayPlan {
    /// Stable sidecar contract identity.
    pub contract_identity: String,
    /// Sidecar format version.
    pub format_version: u32,
    /// SHA-256 identity of the exact version-1 fixture manifest.
    pub fixture_manifest_sha256: String,
    /// Fingerprint algorithm used for the captured activation table.
    pub network_upgrade_activations_fingerprint_version: u16,
    /// Fingerprint of the exact node-discovered activation table.
    pub network_upgrade_activations_fingerprint_hex: String,
    /// Authenticated block and tree state immediately before the fixture range.
    pub history_predecessor: CanonicalFixtureReplayCheckpointRecord,
    /// Authenticated block and tree state at the fixture's fixed tip.
    pub source_tip_checkpoint: CanonicalFixtureReplayCheckpointRecord,
}

/// Serialized block and commitment-tree state for one replay boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFixtureReplayCheckpointRecord {
    /// Exact block identity, using Zinder's internal block-hash byte order.
    pub block_id: CanonicalFixtureReplayBlockIdRecord,
    /// Checkpoint block timestamp in Unix seconds.
    pub block_time_seconds: u32,
    /// Optional pool frontiers after applying this block.
    pub frontiers: CanonicalFixtureReplayFrontierSet,
}

/// Serialized canonical block identity for one replay checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFixtureReplayBlockIdRecord {
    /// Canonical block height.
    pub height: u32,
    /// Block hash in Zinder's internal byte order, lowercase hex-encoded.
    pub hash_hex: String,
}

/// Serialized commitment-tree frontiers grouped by shielded pool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFixtureReplayFrontierSet {
    /// Sapling frontier, absent only before Sapling activation.
    pub sapling: Option<CanonicalFixtureReplayFrontierRecord>,
    /// Orchard frontier, absent only before NU5 activation.
    pub orchard: Option<CanonicalFixtureReplayFrontierRecord>,
    /// Ironwood frontier, absent only before NU6.3 activation.
    pub ironwood: Option<CanonicalFixtureReplayFrontierRecord>,
}

/// One Zebra-compatible commitment-tree frontier.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFixtureReplayFrontierRecord {
    /// Final root in Zebra RPC display byte order, lowercase hex-encoded.
    pub final_root_hex: String,
    /// Canonical Zebra `finalState` bytes, lowercase hex-encoded.
    pub final_state_hex: String,
}

/// Fixture source authorized by one admitted canonical replay plan.
///
/// This source delegates captured blocks and subtree roots while serving only
/// the plan's authenticated predecessor and fixed-tip checkpoints. It does not
/// advertise or implement general tree-state lookup.
#[derive(Clone)]
pub struct CanonicalFixtureNodeSource {
    fixture_source: FixtureNodeSource,
    history_predecessor: CommitmentTreeCheckpoint,
    source_tip_checkpoint: CommitmentTreeCheckpoint,
    activations_fingerprint: NetworkUpgradeActivationsFingerprint,
}

impl CanonicalFixtureNodeSource {
    /// Opens a fixture source after admitting its replay plan and activation table.
    pub fn open(fixture_directory: &Path, manifest: &FixtureManifest) -> Result<Self, BenchError> {
        Self::open_with_segment_delay(fixture_directory, manifest, Duration::ZERO)
    }

    /// Opens an admitted fixture source and delays each outer segment response.
    pub fn open_with_segment_delay(
        fixture_directory: &Path,
        manifest: &FixtureManifest,
        segment_response_delay: Duration,
    ) -> Result<Self, BenchError> {
        let activations = manifest.activations_typed()?;
        let replay_plan =
            CanonicalFixtureReplayPlan::read(fixture_directory, manifest, &activations)?;
        let fixture_source = FixtureNodeSource::open_with_segment_delay(
            fixture_directory,
            manifest,
            segment_response_delay,
        )?;
        Ok(Self {
            fixture_source,
            history_predecessor: replay_plan.history_predecessor_checkpoint()?,
            source_tip_checkpoint: replay_plan.source_tip_checkpoint()?,
            activations_fingerprint: activations
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
        })
    }
}

#[async_trait]
impl NodeSource for CanonicalFixtureNodeSource {
    fn capabilities(&self) -> NodeCapabilities {
        self.fixture_source.capabilities()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.fixture_source.fetch_block_at(height).await
    }

    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError> {
        self.fixture_source.fetch_chain_segment(limits).await
    }

    async fn fetch_chain_checkpoint(
        &self,
        height: BlockHeight,
        network_upgrade_activations: &NetworkUpgradeActivations,
    ) -> Result<CommitmentTreeCheckpoint, SourceError> {
        let requested_fingerprint = network_upgrade_activations
            .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        if requested_fingerprint != self.activations_fingerprint {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "checkpoint activation table does not match the canonical fixture replay plan",
            });
        }
        if height == self.history_predecessor.block_id.height {
            return Ok(self.history_predecessor.clone());
        }
        if height == self.source_tip_checkpoint.block_id.height {
            return Ok(self.source_tip_checkpoint.clone());
        }
        Err(SourceError::BlockUnavailable {
            height,
            reason:
                "canonical fixture replay plan serves only its history predecessor and fixed tip"
                    .to_owned(),
        })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        self.fixture_source.tip_id().await
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        self.fixture_source
            .fetch_subtree_roots(protocol, start_index, max_entries)
            .await
    }

    async fn fetch_subtree_root_range(
        &self,
        range: SubtreeRootRange,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        self.fixture_source.fetch_subtree_root_range(range).await
    }
}

/// Captures the predecessor and fixed-tip checkpoints for one admitted fixture.
///
/// The two checkpoint requests are issued only after the immutable manifest,
/// activation fingerprint, segment digests, and complete block linkage have
/// been admitted. Existing sidecar evidence is never overwritten.
pub async fn capture_canonical_fixture_replay_plan<S: NodeSource>(
    fixture_directory: &Path,
    source: &S,
    source_activations: &NetworkUpgradeActivations,
) -> Result<CanonicalFixtureReplayPlan, BenchError> {
    refuse_existing_plan(fixture_directory)?;
    let manifest = FixtureManifest::read(fixture_directory)?;
    validate_fixture_activations(&manifest, source_activations)?;
    let fixture_boundaries = admit_fixture_corpus(fixture_directory, &manifest)?;
    let predecessor_height = manifest.from_height.checked_sub(1).ok_or_else(|| {
        BenchError::invalid_argument(
            "canonical fixture replay checkpoint capture requires from_height greater than zero",
        )
    })?;
    let history_predecessor = source
        .fetch_chain_checkpoint(BlockHeight::new(predecessor_height), source_activations)
        .await?;
    let source_tip_checkpoint = source
        .fetch_chain_checkpoint(BlockHeight::new(manifest.to_height), source_activations)
        .await?;
    let replay_plan = CanonicalFixtureReplayPlan::from_checkpoints(
        &manifest,
        source_activations,
        &history_predecessor,
        &source_tip_checkpoint,
    )?;
    replay_plan.validate_checkpoint_links(&manifest, &fixture_boundaries)?;
    replay_plan.write_new(fixture_directory)?;
    Ok(replay_plan)
}

impl CanonicalFixtureReplayPlan {
    fn from_checkpoints(
        manifest: &FixtureManifest,
        activations: &NetworkUpgradeActivations,
        history_predecessor: &CommitmentTreeCheckpoint,
        source_tip_checkpoint: &CommitmentTreeCheckpoint,
    ) -> Result<Self, BenchError> {
        validate_fixture_activations(manifest, activations)?;
        let fingerprint = activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        let replay_plan = Self {
            contract_identity: CANONICAL_FIXTURE_REPLAY_PLAN_CONTRACT_IDENTITY.to_owned(),
            format_version: CANONICAL_FIXTURE_REPLAY_PLAN_FORMAT_VERSION,
            fixture_manifest_sha256: manifest.digest_sha256()?,
            network_upgrade_activations_fingerprint_version: fingerprint.version().value(),
            network_upgrade_activations_fingerprint_hex: hex::encode(fingerprint.as_bytes()),
            history_predecessor: CanonicalFixtureReplayCheckpointRecord::from(history_predecessor),
            source_tip_checkpoint: CanonicalFixtureReplayCheckpointRecord::from(
                source_tip_checkpoint,
            ),
        };
        replay_plan.validate_contract(manifest, activations)?;
        Ok(replay_plan)
    }

    /// Reads and admits a replay plan against its exact fixture and activation table.
    pub fn read(
        fixture_directory: &Path,
        manifest: &FixtureManifest,
        activations: &NetworkUpgradeActivations,
    ) -> Result<Self, BenchError> {
        let path = fixture_directory.join(CANONICAL_FIXTURE_REPLAY_PLAN_FILE_NAME);
        let bytes = std::fs::read(&path).map_err(|source| BenchError::io(&path, source))?;
        let replay_plan: Self = serde_json::from_slice(&bytes)?;
        replay_plan.validate_contract(manifest, activations)?;
        let fixture_boundaries = admit_fixture_corpus(fixture_directory, manifest)?;
        replay_plan.validate_checkpoint_links(manifest, &fixture_boundaries)?;
        Ok(replay_plan)
    }

    /// Decodes and validates the authenticated predecessor checkpoint.
    pub fn history_predecessor_checkpoint(&self) -> Result<CommitmentTreeCheckpoint, BenchError> {
        self.history_predecessor.decode("history predecessor")
    }

    /// Decodes and validates the authenticated fixed-tip checkpoint.
    pub fn source_tip_checkpoint(&self) -> Result<CommitmentTreeCheckpoint, BenchError> {
        self.source_tip_checkpoint.decode("source tip checkpoint")
    }

    /// Returns a stable SHA-256 identity for this version-1 replay plan.
    pub fn digest_sha256(&self) -> Result<String, BenchError> {
        self.validate_local_contract()?;
        let normalized_plan = serde_json::to_vec(self)?;
        let mut hasher = Sha256::new();
        hasher.update(CANONICAL_FIXTURE_REPLAY_PLAN_DIGEST_DOMAIN);
        hasher.update(normalized_plan);
        Ok(hex::encode(hasher.finalize()))
    }

    fn write_new(&self, fixture_directory: &Path) -> Result<(), BenchError> {
        self.validate_local_contract()?;
        let temporary_path = fixture_directory.join(CANONICAL_FIXTURE_REPLAY_PLAN_TEMP_FILE_NAME);
        let final_path = fixture_directory.join(CANONICAL_FIXTURE_REPLAY_PLAN_FILE_NAME);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|source| BenchError::io(&temporary_path, source))?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")
            .map_err(|source| BenchError::io(&temporary_path, source))?;
        file.sync_all()
            .map_err(|source| BenchError::io(&temporary_path, source))?;
        if let Err(source) = std::fs::hard_link(&temporary_path, &final_path) {
            let _ = std::fs::remove_file(&temporary_path);
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(existing_plan_error(&final_path));
            }
            return Err(BenchError::io(&final_path, source));
        }
        std::fs::remove_file(&temporary_path)
            .map_err(|source| BenchError::io(&temporary_path, source))?;
        File::open(fixture_directory)
            .and_then(|fixture_directory| fixture_directory.sync_all())
            .map_err(|source| BenchError::io(fixture_directory, source))?;
        Ok(())
    }

    fn validate_contract(
        &self,
        manifest: &FixtureManifest,
        activations: &NetworkUpgradeActivations,
    ) -> Result<(), BenchError> {
        self.validate_local_contract()?;
        let expected_manifest_digest = manifest.digest_sha256()?;
        if self.fixture_manifest_sha256 != expected_manifest_digest {
            return Err(BenchError::fixture_format(
                "canonical fixture replay plan manifest SHA-256 does not match the fixture"
                    .to_owned(),
            ));
        }
        validate_fixture_activations(manifest, activations)?;
        let expected_fingerprint =
            activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        if self.network_upgrade_activations_fingerprint_hex
            != hex::encode(expected_fingerprint.as_bytes())
        {
            return Err(BenchError::fixture_format(
                "canonical fixture replay plan activation fingerprint does not match the fixture"
                    .to_owned(),
            ));
        }
        let history_predecessor = self.history_predecessor_checkpoint()?;
        let source_tip_checkpoint = self.source_tip_checkpoint()?;
        validate_checkpoint_frontier_activations(&history_predecessor, activations)?;
        validate_checkpoint_frontier_activations(&source_tip_checkpoint, activations)
    }

    fn validate_local_contract(&self) -> Result<(), BenchError> {
        if self.contract_identity != CANONICAL_FIXTURE_REPLAY_PLAN_CONTRACT_IDENTITY {
            return Err(BenchError::fixture_format(format!(
                "canonical fixture replay plan contract identity {:?} does not match {CANONICAL_FIXTURE_REPLAY_PLAN_CONTRACT_IDENTITY:?}",
                self.contract_identity
            )));
        }
        if self.format_version != CANONICAL_FIXTURE_REPLAY_PLAN_FORMAT_VERSION {
            return Err(BenchError::fixture_format(format!(
                "canonical fixture replay plan format version {} does not match {CANONICAL_FIXTURE_REPLAY_PLAN_FORMAT_VERSION}",
                self.format_version
            )));
        }
        if self.network_upgrade_activations_fingerprint_version
            != NetworkUpgradeActivationsFingerprintVersion::V1.value()
        {
            return Err(BenchError::fixture_format(format!(
                "canonical fixture replay plan activation fingerprint version {} does not match version 1",
                self.network_upgrade_activations_fingerprint_version
            )));
        }
        validate_digest_hex(&self.fixture_manifest_sha256, "fixture manifest SHA-256")?;
        validate_digest_hex(
            &self.network_upgrade_activations_fingerprint_hex,
            "network upgrade activations fingerprint",
        )?;
        self.history_predecessor_checkpoint()?;
        self.source_tip_checkpoint()?;
        Ok(())
    }

    fn validate_checkpoint_links(
        &self,
        manifest: &FixtureManifest,
        fixture_boundaries: &FixtureBoundaries,
    ) -> Result<(), BenchError> {
        let history_predecessor = self.history_predecessor_checkpoint()?;
        let source_tip_checkpoint = self.source_tip_checkpoint()?;
        let predecessor_height = manifest.from_height.checked_sub(1).ok_or_else(|| {
            BenchError::fixture_format(
                "canonical fixture replay requires a fixture with a predecessor block".to_owned(),
            )
        })?;
        let expected_predecessor = BlockId::new(
            BlockHeight::new(predecessor_height),
            fixture_boundaries.first_block.parent_hash,
        );
        if history_predecessor.block_id != expected_predecessor {
            return Err(BenchError::fixture_format(format!(
                "history predecessor {:?} does not link to fixture first block {:?}",
                history_predecessor.block_id,
                BlockId::new(
                    fixture_boundaries.first_block.height,
                    fixture_boundaries.first_block.hash
                )
            )));
        }
        let manifest_tip = manifest.tip_id()?;
        let last_block_id = BlockId::new(
            fixture_boundaries.last_block.height,
            fixture_boundaries.last_block.hash,
        );
        if source_tip_checkpoint.block_id != manifest_tip || last_block_id != manifest_tip {
            return Err(BenchError::fixture_format(format!(
                "source tip checkpoint {:?}, final fixture block {last_block_id:?}, and manifest tip {manifest_tip:?} must match",
                source_tip_checkpoint.block_id
            )));
        }
        Ok(())
    }
}

impl CanonicalFixtureReplayCheckpointRecord {
    fn decode(&self, field: &str) -> Result<CommitmentTreeCheckpoint, BenchError> {
        let block_hash = decode_lowercase_32_byte_hex(&self.block_id.hash_hex, field)?;
        let sapling = decode_frontier(
            ShieldedProtocol::Sapling,
            self.frontiers.sapling.as_ref(),
            field,
        )?;
        let orchard = decode_frontier(
            ShieldedProtocol::Orchard,
            self.frontiers.orchard.as_ref(),
            field,
        )?;
        let ironwood = decode_frontier(
            ShieldedProtocol::Ironwood,
            self.frontiers.ironwood.as_ref(),
            field,
        )?;
        Ok(CommitmentTreeCheckpoint::new(
            BlockId::new(
                BlockHeight::new(self.block_id.height),
                BlockHash::from_bytes(block_hash),
            ),
            self.block_time_seconds,
            CommitmentTreeFrontiers::from_validated_parts(sapling, orchard, ironwood),
        ))
    }
}

impl From<&CommitmentTreeCheckpoint> for CanonicalFixtureReplayCheckpointRecord {
    fn from(checkpoint: &CommitmentTreeCheckpoint) -> Self {
        Self {
            block_id: CanonicalFixtureReplayBlockIdRecord {
                height: checkpoint.block_id.height.value(),
                hash_hex: hex::encode(checkpoint.block_id.hash.as_bytes()),
            },
            block_time_seconds: checkpoint.block_time_seconds,
            frontiers: CanonicalFixtureReplayFrontierSet {
                sapling: checkpoint
                    .frontiers
                    .sapling()
                    .map(CanonicalFixtureReplayFrontierRecord::from),
                orchard: checkpoint
                    .frontiers
                    .orchard()
                    .map(CanonicalFixtureReplayFrontierRecord::from),
                ironwood: checkpoint
                    .frontiers
                    .ironwood()
                    .map(CanonicalFixtureReplayFrontierRecord::from),
            },
        }
    }
}

impl From<&CommitmentTreeFrontier> for CanonicalFixtureReplayFrontierRecord {
    fn from(frontier: &CommitmentTreeFrontier) -> Self {
        Self {
            final_root_hex: hex::encode(frontier.final_root().as_bytes()),
            final_state_hex: hex::encode(frontier.final_state_bytes()),
        }
    }
}

struct FixtureBoundaries {
    first_block: SourceBlock,
    last_block: SourceBlock,
}

fn admit_fixture_corpus(
    fixture_directory: &Path,
    manifest: &FixtureManifest,
) -> Result<FixtureBoundaries, BenchError> {
    manifest.digest_sha256()?;
    let network = manifest.network_typed()?;
    let mut first_block: Option<SourceBlock> = None;
    let mut previous_block: Option<SourceBlock> = None;
    for descriptor in &manifest.segments {
        for block in read_segment_blocks(fixture_directory, descriptor, network)? {
            if let Some(previous) = previous_block.as_ref()
                && (previous.height.next() != Some(block.height)
                    || block.parent_hash != previous.hash)
            {
                return Err(BenchError::fixture_format(format!(
                    "fixture blocks {} and {} are not an ordered connected pair",
                    previous.height.value(),
                    block.height.value()
                )));
            }
            if first_block.is_none() {
                first_block = Some(block.clone());
            }
            previous_block = Some(block);
        }
    }
    let first_block = first_block.ok_or_else(|| {
        BenchError::fixture_format("canonical fixture contains no blocks".to_owned())
    })?;
    let last_block = previous_block.ok_or_else(|| {
        BenchError::fixture_format("canonical fixture contains no final block".to_owned())
    })?;
    Ok(FixtureBoundaries {
        first_block,
        last_block,
    })
}

fn decode_frontier(
    protocol: ShieldedProtocol,
    record: Option<&CanonicalFixtureReplayFrontierRecord>,
    checkpoint_field: &str,
) -> Result<Option<CommitmentTreeFrontier>, BenchError> {
    record
        .map(|record| {
            let final_root = decode_lowercase_32_byte_hex(
                &record.final_root_hex,
                &format!("{checkpoint_field} {protocol:?} final root"),
            )?;
            let final_state_bytes = decode_lowercase_hex(
                &record.final_state_hex,
                &format!("{checkpoint_field} {protocol:?} final state"),
            )?;
            CommitmentTreeFrontier::from_canonical_final_state(
                protocol,
                FinalNoteCommitmentRoot::from_bytes(final_root),
                final_state_bytes,
            )
            .map_err(|source| {
                BenchError::fixture_format(format!(
                    "invalid {checkpoint_field} {protocol:?} frontier: {source}"
                ))
            })
        })
        .transpose()
}

fn validate_fixture_activations(
    manifest: &FixtureManifest,
    activations: &NetworkUpgradeActivations,
) -> Result<(), BenchError> {
    let manifest_activations = manifest.activations_typed()?;
    let fingerprint_version = NetworkUpgradeActivationsFingerprintVersion::V1;
    if manifest_activations.fingerprint(fingerprint_version)
        != activations.fingerprint(fingerprint_version)
    {
        return Err(BenchError::fixture_format(
            "node activation fingerprint does not match the fixture manifest".to_owned(),
        ));
    }
    Ok(())
}

fn validate_checkpoint_frontier_activations(
    checkpoint: &CommitmentTreeCheckpoint,
    activations: &NetworkUpgradeActivations,
) -> Result<(), BenchError> {
    CommitmentTreeAccumulator::from_validated_frontiers(
        checkpoint.block_id.height,
        &checkpoint.frontiers,
        activations,
    )
    .map(|_| ())
    .map_err(|source| {
        BenchError::fixture_format(format!(
            "canonical fixture replay checkpoint frontier activation mismatch: {source}"
        ))
    })
}

fn refuse_existing_plan(fixture_directory: &Path) -> Result<(), BenchError> {
    let final_path = fixture_directory.join(CANONICAL_FIXTURE_REPLAY_PLAN_FILE_NAME);
    if final_path
        .try_exists()
        .map_err(|source| BenchError::io(&final_path, source))?
    {
        return Err(existing_plan_error(&final_path));
    }
    Ok(())
}

fn existing_plan_error(final_path: &Path) -> BenchError {
    BenchError::invalid_argument(format!(
        "canonical fixture replay plan already exists at {}",
        final_path.display()
    ))
}

fn validate_digest_hex(encoded: &str, field: &str) -> Result<(), BenchError> {
    let bytes = decode_lowercase_hex(encoded, field)?;
    if bytes.len() != 32 {
        return Err(BenchError::fixture_format(format!(
            "{field} must contain exactly 32 bytes"
        )));
    }
    Ok(())
}

fn decode_lowercase_32_byte_hex(encoded: &str, field: &str) -> Result<[u8; 32], BenchError> {
    let bytes = decode_lowercase_hex(encoded, field)?;
    bytes
        .try_into()
        .map_err(|_| BenchError::fixture_format(format!("{field} must contain exactly 32 bytes")))
}

fn decode_lowercase_hex(encoded: &str, field: &str) -> Result<Vec<u8>, BenchError> {
    if encoded.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(BenchError::fixture_format(format!(
            "{field} must use lowercase hexadecimal"
        )));
    }
    hex::decode(encoded)
        .map_err(|source| BenchError::fixture_format(format!("invalid {field} hex: {source}")))
}
