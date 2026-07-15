//! Wallet projection build, readiness, control, and checkpoint contracts.

use zinder_core::{
    BlockHeight, BlockId, CanonicalBlockFactsSequenceDigest, ChainEpochId, Network,
    NetworkUpgradeActivationsFingerprint, TransparentUtxoSetCommitment, UtxoSetCommitmentScheme,
};

use crate::contract_error::encoded_len;
use crate::{
    REQUIRED_CANONICAL_FACTS_DIGEST_VERSION, REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION,
    REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION, REQUIRED_CANONICAL_STORE_SCHEMA_VERSION,
    WALLET_PROJECTION_CHECKPOINT_MANIFEST_IDENTITY, WALLET_PROJECTION_CHECKPOINT_VERSION,
    WALLET_PROJECTION_SCHEMA_VERSION, WALLET_PROJECTION_STORE_IDENTITY,
    WALLET_PROJECTION_VALUE_ENCODING_VERSION, WalletProjectionContractError,
};

/// Exact position of wallet projection state in the canonical event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionPosition {
    /// Monotonic canonical epoch visible at this position.
    pub chain_epoch_id: ChainEpochId,
    /// Canonical tip projected into wallet state.
    pub tip: BlockId,
    /// Monotonic event-stream sequence projected through this position.
    pub event_sequence: u64,
    /// Authenticated opaque cursor for resuming strictly after this position.
    pub chain_event_cursor: Vec<u8>,
}

impl WalletProjectionPosition {
    /// Creates an exact wallet projection position.
    #[must_use]
    pub fn new(
        chain_epoch_id: ChainEpochId,
        tip: BlockId,
        event_sequence: u64,
        chain_event_cursor: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            chain_epoch_id,
            tip,
            event_sequence,
            chain_event_cursor: chain_event_cursor.into(),
        }
    }
}

/// Canonical event-stream anchor without the opaque resume token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletProjectionAnchor {
    /// Monotonic canonical epoch visible at this anchor.
    pub chain_epoch_id: ChainEpochId,
    /// Canonical tip represented by the wallet projection.
    pub tip: BlockId,
    /// Monotonic source event sequence represented by the projection.
    pub event_sequence: u64,
}

impl WalletProjectionAnchor {
    /// Creates an exact canonical anchor.
    #[must_use]
    pub const fn new(chain_epoch_id: ChainEpochId, tip: BlockId, event_sequence: u64) -> Self {
        Self {
            chain_epoch_id,
            tip,
            event_sequence,
        }
    }
}

impl WalletProjectionPosition {
    /// Returns the cursor-independent canonical anchor.
    #[must_use]
    pub const fn anchor(&self) -> WalletProjectionAnchor {
        WalletProjectionAnchor::new(self.chain_epoch_id, self.tip, self.event_sequence)
    }
}

/// Proof that wallet rows cover complete chain history through one block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletProjectionCoverage {
    /// First projected block height. Version 1 requires height one.
    pub first_projected_height: BlockHeight,
    /// Last block included in the complete projection.
    pub last_projected_block: BlockId,
}

impl WalletProjectionCoverage {
    /// Creates complete-history coverage through `last_projected_block`.
    #[must_use]
    pub const fn complete_through(last_projected_block: BlockId) -> Self {
        Self {
            first_projected_height: BlockHeight::new(1),
            last_projected_block,
        }
    }
}

/// Opaque digest identifying one Zinder-exported wallet checkpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WalletProjectionCheckpointId([u8; 32]);

impl WalletProjectionCheckpointId {
    /// Reconstructs an identifier from exact digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact identifier bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Exact checkpoint state accepted as the base of a wallet build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionCheckpointReference {
    /// Identity of the Zinder wallet state bundle.
    pub checkpoint_id: WalletProjectionCheckpointId,
    /// Exact source position represented by the checkpoint.
    pub position: WalletProjectionPosition,
}

/// Starting state for a fresh wallet projection build.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WalletProjectionBuildBase {
    /// Replay complete canonical history beginning at block height one.
    CompleteHistory,
    /// Restore a previously ready Zinder wallet projection checkpoint.
    ZinderCheckpoint(WalletProjectionCheckpointReference),
}

/// Immutable plan recorded before a wallet projection build starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionBuildPlan {
    /// Complete-history replay or an exact Zinder wallet checkpoint.
    pub base: WalletProjectionBuildBase,
    /// Canonical position the build must reach before readiness publication.
    pub target: WalletProjectionPosition,
}

impl WalletProjectionBuildPlan {
    /// Creates a complete-history build plan.
    #[must_use]
    pub fn complete_history(target: WalletProjectionPosition) -> Self {
        Self {
            base: WalletProjectionBuildBase::CompleteHistory,
            target,
        }
    }

    /// Creates a build plan restoring an exact Zinder wallet checkpoint.
    #[must_use]
    pub fn from_zinder_checkpoint(
        checkpoint: WalletProjectionCheckpointReference,
        target: WalletProjectionPosition,
    ) -> Self {
        Self {
            base: WalletProjectionBuildBase::ZinderCheckpoint(checkpoint),
            target,
        }
    }
}

/// SHA-256 commitment to every version-1 wallet projection row in key order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WalletProjectionDigest([u8; 32]);

impl WalletProjectionDigest {
    /// Reconstructs a digest from exact committed bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Row counts published with wallet readiness evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalletProjectionFamilyRowCounts {
    /// Rows in `live_output`.
    pub live_output_count: u64,
    /// Rows in `live_output_by_address`.
    pub live_output_by_address_count: u64,
    /// Rows in `spent_output`.
    pub spent_output_count: u64,
    /// Rows in `address_history`.
    pub address_history_count: u64,
    /// Non-zero rows in `address_balance`.
    pub address_balance_count: u64,
    /// Rows in the bounded `reorg_undo` window.
    pub reorg_undo_count: u64,
}

/// Complete UTXO aggregate published with wallet readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletUtxoSetSummary {
    /// Number of currently unspent transparent outputs.
    pub utxo_count: u64,
    /// Sum of all currently unspent transparent output values.
    pub total_value_zat: u64,
    /// Full order-independent `LtHash16` accumulator.
    pub commitment: TransparentUtxoSetCommitment,
}

/// Evidence required before wallet queries may serve a projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionReadyEvidence {
    /// Exact source position of the ready projection.
    pub position: WalletProjectionPosition,
    /// Complete-history coverage ending at the same source tip.
    pub coverage: WalletProjectionCoverage,
    /// Ordered digest of every canonical block-facts record through `position`.
    pub source_sequence_digest: CanonicalBlockFactsSequenceDigest,
    /// Digest of every logical projection row.
    pub projection_digest: WalletProjectionDigest,
    /// Exact logical row counts by family.
    pub row_counts: WalletProjectionFamilyRowCounts,
    /// Complete current UTXO aggregate.
    pub utxo_summary: WalletUtxoSetSummary,
    /// Inputs whose predecessor output could not be resolved.
    pub unresolved_predecessor_count: u64,
}

/// Durable wallet store lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WalletProjectionBuildState {
    /// Rows are incomplete and query admission must refuse the store.
    Building(WalletProjectionBuildPlan),
    /// Evidence is complete and query admission may validate the source fence.
    Ready(WalletProjectionReadyEvidence),
}

/// Singleton durable control record for one wallet projection store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletStoreControl {
    /// Network whose canonical facts produced the projection.
    pub network: Network,
    /// Fingerprint of the exact network-upgrade activation table.
    pub network_upgrade_activations_fingerprint: NetworkUpgradeActivationsFingerprint,
    /// Maximum rollback depth represented by `reorg_undo`.
    pub supported_reorg_depth: u32,
    /// Monotonic single-writer generation.
    pub writer_generation: u64,
    /// Build or ready lifecycle state.
    pub build_state: WalletProjectionBuildState,
}

impl WalletStoreControl {
    /// Encodes the exact, single-version wallet control record.
    pub fn encode(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(WALLET_PROJECTION_STORE_IDENTITY);
        bytes.extend_from_slice(&WALLET_PROJECTION_SCHEMA_VERSION.to_be_bytes());
        bytes.extend_from_slice(&WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.network.id().to_be_bytes());
        bytes.extend_from_slice(
            &self
                .network_upgrade_activations_fingerprint
                .version()
                .value()
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&self.network_upgrade_activations_fingerprint.as_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_STORE_SCHEMA_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_FACTS_DIGEST_VERSION.to_be_bytes());
        bytes.extend_from_slice(&REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.supported_reorg_depth.to_be_bytes());
        bytes.extend_from_slice(&self.writer_generation.to_be_bytes());
        match &self.build_state {
            WalletProjectionBuildState::Building(plan) => {
                bytes.push(1);
                encode_build_plan(plan, &mut bytes)?;
            }
            WalletProjectionBuildState::Ready(evidence) => {
                bytes.push(2);
                encode_ready_evidence(evidence, &mut bytes)?;
            }
        }
        Ok(bytes)
    }
}

/// One physical file in a Zinder wallet checkpoint bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionCheckpointFile {
    /// Bundle-relative path chosen by the checkpoint exporter.
    pub relative_path: String,
    /// Exact file size in bytes.
    pub size_bytes: u64,
    /// SHA-256 digest of the file bytes.
    pub sha256: [u8; 32],
}

/// Version-1 manifest for a physical Zinder wallet state bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionCheckpointManifest {
    /// Stable identity of this exported checkpoint.
    pub checkpoint_id: WalletProjectionCheckpointId,
    /// Ready wallet control record captured by the checkpoint.
    pub ready_control: WalletStoreControl,
    /// Every physical file required to restore the checkpoint.
    pub files: Vec<WalletProjectionCheckpointFile>,
}

impl WalletProjectionCheckpointManifest {
    /// Encodes the exact, single-version checkpoint manifest.
    pub fn encode(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        if !matches!(
            self.ready_control.build_state,
            WalletProjectionBuildState::Ready(_)
        ) {
            return Err(WalletProjectionContractError::CheckpointRequiresReadyControl);
        }
        let control_bytes = self.ready_control.encode()?;
        let control_len = encoded_len(control_bytes.len(), "checkpoint control record")?;
        let file_count = encoded_len(self.files.len(), "checkpoint file list")?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(WALLET_PROJECTION_CHECKPOINT_MANIFEST_IDENTITY);
        bytes.extend_from_slice(&WALLET_PROJECTION_CHECKPOINT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.checkpoint_id.as_bytes());
        bytes.extend_from_slice(&control_len.to_be_bytes());
        bytes.extend_from_slice(&control_bytes);
        bytes.extend_from_slice(&file_count.to_be_bytes());
        for file in &self.files {
            let path_bytes = file.relative_path.as_bytes();
            let path_len = encoded_len(path_bytes.len(), "checkpoint relative path")?;
            bytes.extend_from_slice(&path_len.to_be_bytes());
            bytes.extend_from_slice(path_bytes);
            bytes.extend_from_slice(&file.size_bytes.to_be_bytes());
            bytes.extend_from_slice(&file.sha256);
        }
        Ok(bytes)
    }
}

fn encode_build_plan(
    plan: &WalletProjectionBuildPlan,
    bytes: &mut Vec<u8>,
) -> Result<(), WalletProjectionContractError> {
    match &plan.base {
        WalletProjectionBuildBase::CompleteHistory => bytes.push(1),
        WalletProjectionBuildBase::ZinderCheckpoint(checkpoint) => {
            bytes.push(2);
            bytes.extend_from_slice(&checkpoint.checkpoint_id.as_bytes());
            encode_position(&checkpoint.position, bytes)?;
        }
    }
    encode_position(&plan.target, bytes)
}

fn encode_position(
    position: &WalletProjectionPosition,
    bytes: &mut Vec<u8>,
) -> Result<(), WalletProjectionContractError> {
    let cursor_len = encoded_len(position.chain_event_cursor.len(), "chain event cursor")?;
    bytes.extend_from_slice(&position.chain_epoch_id.value().to_be_bytes());
    bytes.extend_from_slice(&position.tip.height.value().to_be_bytes());
    bytes.extend_from_slice(&position.tip.hash.as_bytes());
    bytes.extend_from_slice(&position.event_sequence.to_be_bytes());
    bytes.extend_from_slice(&cursor_len.to_be_bytes());
    bytes.extend_from_slice(&position.chain_event_cursor);
    Ok(())
}

fn encode_ready_evidence(
    evidence: &WalletProjectionReadyEvidence,
    bytes: &mut Vec<u8>,
) -> Result<(), WalletProjectionContractError> {
    validate_ready_evidence(evidence)?;
    encode_position(&evidence.position, bytes)?;
    bytes.extend_from_slice(
        &evidence
            .coverage
            .first_projected_height
            .value()
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &evidence
            .coverage
            .last_projected_block
            .height
            .value()
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&evidence.coverage.last_projected_block.hash.as_bytes());
    bytes.extend_from_slice(
        &evidence
            .source_sequence_digest
            .version()
            .value()
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&evidence.source_sequence_digest.block_count().to_be_bytes());
    bytes.extend_from_slice(&evidence.source_sequence_digest.as_bytes());
    bytes.extend_from_slice(&evidence.projection_digest.as_bytes());
    let counts = evidence.row_counts;
    bytes.extend_from_slice(&counts.live_output_count.to_be_bytes());
    bytes.extend_from_slice(&counts.live_output_by_address_count.to_be_bytes());
    bytes.extend_from_slice(&counts.spent_output_count.to_be_bytes());
    bytes.extend_from_slice(&counts.address_history_count.to_be_bytes());
    bytes.extend_from_slice(&counts.address_balance_count.to_be_bytes());
    bytes.extend_from_slice(&counts.reorg_undo_count.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.utxo_count.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.total_value_zat.to_be_bytes());
    bytes.extend_from_slice(&evidence.utxo_summary.commitment.scheme().id().to_be_bytes());
    bytes.extend_from_slice(evidence.utxo_summary.commitment.accumulator());
    bytes.extend_from_slice(&evidence.unresolved_predecessor_count.to_be_bytes());
    Ok(())
}

fn validate_ready_evidence(
    evidence: &WalletProjectionReadyEvidence,
) -> Result<(), WalletProjectionContractError> {
    if evidence.coverage.first_projected_height != BlockHeight::new(1) {
        return Err(WalletProjectionContractError::ReadyCoverageMustBeginAtHeightOne);
    }
    if evidence.coverage.last_projected_block != evidence.position.tip {
        return Err(WalletProjectionContractError::ReadyCoverageTipMismatch);
    }
    if evidence.unresolved_predecessor_count != 0 {
        return Err(WalletProjectionContractError::ReadyHasUnresolvedPredecessors);
    }
    if evidence.row_counts.live_output_count != evidence.row_counts.live_output_by_address_count {
        return Err(WalletProjectionContractError::ReadyLiveOutputIndexCountMismatch);
    }
    if evidence.row_counts.live_output_count != evidence.utxo_summary.utxo_count {
        return Err(WalletProjectionContractError::ReadyUtxoCountMismatch);
    }
    if evidence.source_sequence_digest.block_count()
        != u64::from(evidence.position.tip.height.value())
    {
        return Err(WalletProjectionContractError::ReadySourceSequenceLengthMismatch);
    }
    if evidence.utxo_summary.commitment.scheme() != UtxoSetCommitmentScheme::LtHash16 {
        return Err(WalletProjectionContractError::ReadyUtxoCommitmentSchemeMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{
        BlockHash, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
        CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    };

    fn sample_position(height: u32) -> WalletProjectionPosition {
        WalletProjectionPosition::new(
            ChainEpochId::new(0x1112_1314_1516_1718),
            BlockId::new(BlockHeight::new(height), BlockHash::from_bytes([0x33; 32])),
            0x2122_2324_2526_2728,
            [0xfe, 0xff],
        )
    }

    fn sequence_digest(block_count: u32) -> CanonicalBlockFactsSequenceDigest {
        let mut builder = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::V1,
        );
        for index in 0..block_count {
            let digest = CanonicalBlockFactsDigest::from_reference_encoding(
                CanonicalBlockFactsDigestVersion::V1,
                &index.to_be_bytes(),
            );
            assert!(builder.try_append(digest).is_ok());
        }
        builder.finish()
    }

    fn ready_evidence() -> WalletProjectionReadyEvidence {
        let position = sample_position(1);
        WalletProjectionReadyEvidence {
            coverage: WalletProjectionCoverage::complete_through(position.tip),
            position,
            source_sequence_digest: sequence_digest(1),
            projection_digest: WalletProjectionDigest::from_bytes([0x77; 32]),
            row_counts: WalletProjectionFamilyRowCounts {
                live_output_count: 1,
                live_output_by_address_count: 1,
                spent_output_count: 2,
                address_history_count: 3,
                address_balance_count: 1,
                reorg_undo_count: 0,
            },
            utxo_summary: WalletUtxoSetSummary {
                utxo_count: 1,
                total_value_zat: 9,
                commitment: TransparentUtxoSetCommitment::empty(),
            },
            unresolved_predecessor_count: 0,
        }
    }

    fn sample_control(build_state: WalletProjectionBuildState) -> WalletStoreControl {
        WalletStoreControl {
            network: Network::ZcashRegtest,
            network_upgrade_activations_fingerprint:
                NetworkUpgradeActivationsFingerprint::from_bytes(
                    zinder_core::NetworkUpgradeActivationsFingerprintVersion::V1,
                    [0xaa; 32],
                ),
            supported_reorg_depth: 100,
            writer_generation: 0x0102_0304_0506_0708,
            build_state,
        }
    }

    #[test]

    fn building_control_has_exact_version_one_bytes() {
        let control = sample_control(WalletProjectionBuildState::Building(
            WalletProjectionBuildPlan::complete_history(sample_position(0x0a0b_0c0d)),
        ));
        assert_eq!(
            hex::encode(
                control
                    .encode()
                    .unwrap_or_else(|error| unreachable!("valid building control: {error}"))
            ),
            concat!(
                "77616c6c65742d70726f6a656374696f6e",
                "0001",
                "0001",
                "00000003",
                "0001",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "0001",
                "00000001",
                "0001",
                "0001",
                "00000064",
                "0102030405060708",
                "01",
                "01",
                "1112131415161718",
                "0a0b0c0d",
                "3333333333333333333333333333333333333333333333333333333333333333",
                "2122232425262728",
                "00000002",
                "feff"
            )
        );
    }

    #[test]

    fn ready_control_rejects_inconsistent_evidence() {
        let mut cases = Vec::new();

        let mut evidence = ready_evidence();
        evidence.unresolved_predecessor_count = 1;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyHasUnresolvedPredecessors,
        ));

        let mut evidence = ready_evidence();
        evidence.coverage.first_projected_height = BlockHeight::new(2);
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyCoverageMustBeginAtHeightOne,
        ));

        let mut evidence = ready_evidence();
        evidence.coverage.last_projected_block.hash = BlockHash::from_bytes([0x88; 32]);
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyCoverageTipMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.row_counts.live_output_by_address_count = 2;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyLiveOutputIndexCountMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.utxo_summary.utxo_count = 2;
        cases.push((
            evidence,
            WalletProjectionContractError::ReadyUtxoCountMismatch,
        ));

        let mut evidence = ready_evidence();
        evidence.source_sequence_digest = sequence_digest(0);
        cases.push((
            evidence,
            WalletProjectionContractError::ReadySourceSequenceLengthMismatch,
        ));

        for (evidence, expected_error) in cases {
            let control = sample_control(WalletProjectionBuildState::Ready(evidence));
            assert_eq!(control.encode(), Err(expected_error));
        }
    }

    #[test]

    fn checkpoint_manifest_frames_ready_control_and_files_exactly() {
        let control = sample_control(WalletProjectionBuildState::Ready(ready_evidence()));
        let control_bytes = control
            .encode()
            .unwrap_or_else(|error| unreachable!("valid ready control: {error}"));
        let manifest = WalletProjectionCheckpointManifest {
            checkpoint_id: WalletProjectionCheckpointId::from_bytes([0x99; 32]),
            ready_control: control,
            files: vec![WalletProjectionCheckpointFile {
                relative_path: "CURRENT".to_owned(),
                size_bytes: 7,
                sha256: [0xbb; 32],
            }],
        };
        let encoded = manifest
            .encode()
            .unwrap_or_else(|error| unreachable!("valid checkpoint manifest: {error}"));
        let mut expected = Vec::new();
        expected.extend_from_slice(WALLET_PROJECTION_CHECKPOINT_MANIFEST_IDENTITY);
        expected.extend_from_slice(&1u16.to_be_bytes());
        expected.extend_from_slice(&[0x99; 32]);
        expected.extend_from_slice(
            &u32::try_from(control_bytes.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        expected.extend_from_slice(&control_bytes);
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&7u32.to_be_bytes());
        expected.extend_from_slice(b"CURRENT");
        expected.extend_from_slice(&7u64.to_be_bytes());
        expected.extend_from_slice(&[0xbb; 32]);
        assert_eq!(encoded, expected);
    }

    #[test]

    fn checkpoint_manifest_rejects_building_control() {
        let manifest = WalletProjectionCheckpointManifest {
            checkpoint_id: WalletProjectionCheckpointId::from_bytes([0x99; 32]),
            ready_control: sample_control(WalletProjectionBuildState::Building(
                WalletProjectionBuildPlan::complete_history(sample_position(1)),
            )),
            files: Vec::new(),
        };
        assert_eq!(
            manifest.encode(),
            Err(WalletProjectionContractError::CheckpointRequiresReadyControl)
        );
    }
}
