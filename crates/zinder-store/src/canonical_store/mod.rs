//! Version-1 canonical storage contract.
//!
//! This module owns the exact `RocksDB` layout for the clean fact-first
//! canonical data plane. It deliberately exposes no generic database adapter
//! and no compatibility decoder for earlier Zinder stores.

mod block_load;
mod block_replay;
mod builder;
mod control;
mod publication;
mod rocksdb;
mod subtree_load;

use std::{io, path::PathBuf};

use thiserror::Error;
use zinder_core::{
    BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion, ChainEpochId,
    CommitmentTreeCheckpoint, CommitmentTreeFrontiers,
    MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES, NetworkUpgradeActivations,
    NetworkUpgradeActivationsFingerprint, NetworkUpgradeActivationsFingerprintVersion,
    ShieldedProtocol,
};

pub use block_load::{CanonicalBlockLoadEvidence, CanonicalBuildBlock};
pub use block_replay::CanonicalReplayScan;
pub use builder::RocksDbCanonicalBuilder;
pub use publication::{
    CanonicalBaselinePublication, PreparedCanonicalBaselinePublication,
    ValidatedRocksDbCanonicalBuild,
};
pub use rocksdb::RocksDbCanonicalStore;
pub use subtree_load::{CanonicalBuildSubtreeRoot, CanonicalSubtreeRootLoadEvidence};

/// Exact persisted identity of the clean canonical store.
pub const CANONICAL_STORE_IDENTITY: &str = "canonical";
/// Exact physical schema accepted by this canonical store implementation.
pub const CANONICAL_STORE_SCHEMA_VERSION: u16 = 1;
/// Global block-height cadence for typed commitment-tree checkpoints.
///
/// A checkpoint at least every 100 blocks keeps wallet rewind anchors within the
/// standard scan-recovery window regardless of canonical loader batch boundaries.
pub const TREE_STATE_CHECKPOINT_STRIDE: u32 = 100;

const REQUIRED_CANONICAL_NETWORK_UPGRADES: [&str; 5] =
    ["Overwinter", "Sapling", "Blossom", "Heartwood", "Canopy"];

/// Fixed source-chain range for one fresh canonical construction.
///
/// The predecessor anchors the first retained block even for complete history,
/// where it is the network's height-zero block. The fixed tip prevents an
/// exhausted or failed source stream from publishing a contiguous prefix as a
/// complete build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalStoreBuildPlan {
    network: zinder_core::Network,
    network_upgrade_activations_fingerprint: NetworkUpgradeActivationsFingerprint,
    history_bounds: zinder_core::CanonicalHistoryBounds,
    history_predecessor: CommitmentTreeCheckpoint,
    build_tip: BlockId,
}

impl CanonicalStoreBuildPlan {
    /// Builds a plan retaining every non-genesis block through `build_tip`.
    pub fn complete(
        network_upgrade_activations: &NetworkUpgradeActivations,
        genesis_block_time_seconds: u32,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        validate_required_network_upgrades(network_upgrade_activations)?;
        let network = network_upgrade_activations.network();
        Self::from_parts(
            network,
            network_upgrade_activations
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            zinder_core::CanonicalHistoryBounds::complete(),
            CommitmentTreeCheckpoint::new(
                BlockId::new(BlockHeight::new(0), network.genesis_hash()),
                genesis_block_time_seconds,
                CommitmentTreeFrontiers::default(),
            ),
            build_tip,
        )
    }

    /// Builds a plan retaining blocks immediately after `checkpoint` through `build_tip`.
    pub fn checkpointed(
        network_upgrade_activations: &NetworkUpgradeActivations,
        checkpoint: CommitmentTreeCheckpoint,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        validate_required_network_upgrades(network_upgrade_activations)?;
        if checkpoint.block_id.height.value() == 0 {
            return Err(CanonicalStoreBuildPlanError::CheckpointAtGenesis);
        }
        let history_bounds = zinder_core::CanonicalHistoryBounds::checkpointed(checkpoint.block_id)
            .map_err(|_| CanonicalStoreBuildPlanError::CheckpointHasNoSuccessor)?;
        validate_checkpoint_frontier_presence(
            network_upgrade_activations,
            checkpoint.block_id,
            &checkpoint.frontiers,
        )?;
        Self::from_parts(
            network_upgrade_activations.network(),
            network_upgrade_activations
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            history_bounds,
            checkpoint,
            build_tip,
        )
    }

    pub(super) fn from_parts(
        network: zinder_core::Network,
        network_upgrade_activations_fingerprint: NetworkUpgradeActivationsFingerprint,
        history_bounds: zinder_core::CanonicalHistoryBounds,
        history_predecessor: CommitmentTreeCheckpoint,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        match history_bounds.preceding_checkpoint() {
            None if history_predecessor.block_id
                != BlockId::new(BlockHeight::new(0), network.genesis_hash()) =>
            {
                return Err(CanonicalStoreBuildPlanError::InvalidHistoryPredecessor);
            }
            Some(checkpoint) if history_predecessor.block_id != checkpoint => {
                return Err(CanonicalStoreBuildPlanError::InvalidHistoryPredecessor);
            }
            Some(checkpoint) if checkpoint.height.value() == 0 => {
                return Err(CanonicalStoreBuildPlanError::CheckpointAtGenesis);
            }
            None | Some(_) => {}
        }
        if history_bounds.preceding_checkpoint().is_none()
            && history_predecessor.frontiers != CommitmentTreeFrontiers::default()
        {
            return Err(CanonicalStoreBuildPlanError::CompleteHistoryHasFrontiers);
        }
        for protocol in [
            ShieldedProtocol::Sapling,
            ShieldedProtocol::Orchard,
            ShieldedProtocol::Ironwood,
        ] {
            if let Some(frontier) = history_predecessor.frontiers.get(protocol)
                && frontier.final_state_bytes().len()
                    > MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES
            {
                return Err(CanonicalStoreBuildPlanError::PredecessorFrontierTooLarge {
                    protocol,
                    encoded_bytes: frontier.final_state_bytes().len(),
                });
            }
        }
        let first_available_height = history_bounds.first_available_height();
        if build_tip.height.value() < first_available_height.value() {
            return Err(CanonicalStoreBuildPlanError::BuildTipPrecedesHistory {
                build_tip: build_tip.height.value(),
                first_available_height: first_available_height.value(),
            });
        }
        Ok(Self {
            network,
            network_upgrade_activations_fingerprint,
            history_bounds,
            history_predecessor,
            build_tip,
        })
    }

    /// Returns the immutable network for this build.
    #[must_use]
    pub const fn network(&self) -> zinder_core::Network {
        self.network
    }

    /// Returns the immutable node activation-table identity for this build.
    #[must_use]
    pub const fn network_upgrade_activations_fingerprint(
        &self,
    ) -> NetworkUpgradeActivationsFingerprint {
        self.network_upgrade_activations_fingerprint
    }

    /// Returns the durable boundary of intentionally retained history.
    #[must_use]
    pub const fn history_bounds(&self) -> zinder_core::CanonicalHistoryBounds {
        self.history_bounds
    }

    /// Returns the typed checkpoint immediately preceding retained history.
    #[must_use]
    pub const fn history_predecessor(&self) -> &CommitmentTreeCheckpoint {
        &self.history_predecessor
    }

    /// Returns the exact source tip this build must reach.
    #[must_use]
    pub const fn build_tip(&self) -> BlockId {
        self.build_tip
    }
}

/// Invalid source-chain range for canonical construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CanonicalStoreBuildPlanError {
    /// The node activation table cannot interpret the universal v3-v4 history baseline.
    #[error("canonical build activation table is missing required network upgrade {name}")]
    MissingRequiredNetworkUpgrade {
        /// Exact node-advertised upgrade name required by v1.
        name: &'static str,
    },
    /// The checkpoint cannot have retained history after it.
    #[error("canonical build checkpoint has no successor height")]
    CheckpointHasNoSuccessor,
    /// Height zero has complete-history semantics and cannot be a checkpoint.
    #[error("canonical build checkpoint cannot be the height-zero genesis block")]
    CheckpointAtGenesis,
    /// The predecessor does not match the selected complete or checkpointed history.
    #[error("canonical build history predecessor does not match its history bounds")]
    InvalidHistoryPredecessor,
    /// A complete build must start before every shielded-pool frontier.
    #[error("complete history predecessor commitment-tree frontiers must all be absent")]
    CompleteHistoryHasFrontiers,
    /// One predecessor frontier exceeds the exact version-1 store-control bound.
    #[error(
        "canonical build {protocol:?} predecessor frontier is {encoded_bytes} bytes; maximum is 1090"
    )]
    PredecessorFrontierTooLarge {
        /// Shielded pool whose frontier exceeded the bound.
        protocol: ShieldedProtocol,
        /// Observed canonical `finalState` byte length.
        encoded_bytes: usize,
    },
    /// Checkpoint frontier presence disagrees with the source activation table.
    #[error(
        "canonical build {protocol:?} predecessor frontier presence at height {checkpoint_height} does not match the network upgrade activations"
    )]
    PredecessorFrontierActivationMismatch {
        /// Shielded pool whose presence was inconsistent.
        protocol: ShieldedProtocol,
        /// Checkpoint height used for activation admission.
        checkpoint_height: u32,
    },
    /// The target tip is below the first retained height.
    #[error(
        "canonical build tip {build_tip} precedes first available height {first_available_height}"
    )]
    BuildTipPrecedesHistory {
        /// Invalid target height.
        build_tip: u32,
        /// First retained height required by the history bounds.
        first_available_height: u32,
    },
}

fn validate_required_network_upgrades(
    network_upgrade_activations: &NetworkUpgradeActivations,
) -> Result<(), CanonicalStoreBuildPlanError> {
    for name in REQUIRED_CANONICAL_NETWORK_UPGRADES {
        if network_upgrade_activations
            .activation_height_by_name(name)
            .is_none()
        {
            return Err(CanonicalStoreBuildPlanError::MissingRequiredNetworkUpgrade { name });
        }
    }
    Ok(())
}

fn validate_checkpoint_frontier_presence(
    network_upgrade_activations: &NetworkUpgradeActivations,
    checkpoint: BlockId,
    frontiers: &CommitmentTreeFrontiers,
) -> Result<(), CanonicalStoreBuildPlanError> {
    for protocol in [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ] {
        let is_active = network_upgrade_activations
            .activation_height_by_name(protocol.activation_upgrade_name())
            .is_some_and(|activation_height| activation_height <= checkpoint.height);
        if frontiers.get(protocol).is_some() != is_active {
            return Err(
                CanonicalStoreBuildPlanError::PredecessorFrontierActivationMismatch {
                    protocol,
                    checkpoint_height: checkpoint.height.value(),
                },
            );
        }
    }
    Ok(())
}

/// Failure while streaming source facts into a fresh canonical build.
#[derive(Debug, Error)]
pub enum CanonicalStoreBuildError<SourceError> {
    /// Upstream fetch or canonical preparation failed before ingestion.
    #[error("canonical source stream failed")]
    Source {
        /// Concrete upstream failure preserved for diagnosis.
        #[source]
        source: SourceError,
    },
    /// Canonical storage construction failed.
    #[error(transparent)]
    Store(#[from] CanonicalStoreError),
}

/// Closed canonical data workload persisted before any data family is built.
///
/// Both workloads retain the semantic replay needed by projections. The
/// selected workload fixes which optional canonical source artifacts must be
/// complete, so missing raw or explorer-only rows cannot be mistaken for an
/// incomplete build after restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalStoreWorkload {
    /// Wallet APIs, including retained raw transactions, typed tree-state
    /// checkpoints, and continuous subtree roots.
    ///
    /// This workload omits per-block final roots, daily value-pool balances,
    /// and raw block blobs so explorer-only acquisition cannot slow the
    /// fastest supported sync path.
    Wallet,
    /// Wallet APIs plus per-block final roots, daily value-pool balances, raw
    /// blocks, and other explorer-only source facts.
    Explorer,
}

impl CanonicalStoreWorkload {
    /// Returns the persisted configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::Explorer => "explorer",
        }
    }
}

/// Durable construction state of a canonical store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalStoreBuildState {
    /// Data families are inactive and may still be under construction.
    Building,
    /// Every required family was validated and the baseline epoch is visible.
    Ready(CanonicalStoreReadyEvidence),
}

/// Validation evidence that makes a constructed canonical store visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalStoreReadyEvidence {
    /// First retained canonical block.
    pub first_retained_block: BlockId,
    /// Current visible canonical tip.
    pub visible_tip: BlockId,
    /// Current visible epoch identifier.
    pub visible_epoch: ChainEpochId,
    /// Latest durable chain-event sequence that produced `visible_epoch`.
    pub visible_event_sequence: u64,
    /// Number of contiguous blocks authenticated in the baseline build.
    pub baseline_block_count: u64,
    /// Canonical block-fact digest contract.
    pub block_digest_version: CanonicalBlockFactsDigestVersion,
    /// Canonical replay-envelope contract.
    pub replay_format_version: CanonicalBlockReplayFormatVersion,
    /// Ordered sequence-digest contract.
    pub sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion,
    /// Ordered digest of the baseline build's fact sequence.
    pub baseline_sequence_digest: [u8; 32],
    /// Total semantic replay-envelope bytes in the baseline build.
    pub baseline_logical_fact_bytes: u64,
}

/// Failure to create or admit a clean canonical store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalStoreError {
    /// The supplied `RocksDB` resource budget violates a hard bound.
    #[error("invalid canonical store resource budget: {reason}")]
    InvalidResourceBudget {
        /// Stable validation reason.
        reason: &'static str,
    },

    /// A builder was pointed at a path that already exists.
    #[error("canonical store builder requires a fresh path: {path:?}")]
    PathNotFresh {
        /// Existing path refused by the builder.
        path: PathBuf,
    },

    /// A prior process left the deterministic canonical block staging directory.
    #[error(
        "canonical block staging path already exists and requires full build cleanup: {path:?}"
    )]
    BlockLoadStagingExists {
        /// Existing staging path preserved without repair or deletion.
        path: PathBuf,
    },

    /// A filesystem operation failed.
    #[error("canonical store path {path:?} is unavailable")]
    PathUnavailable {
        /// Path being accessed.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// Secure cursor-authentication material could not be generated.
    #[error("canonical store cursor authentication key generation failed")]
    EntropyUnavailable {
        /// Operating-system entropy failure.
        #[source]
        source: getrandom::Error,
    },

    /// An existing path does not exactly match the clean canonical contract.
    #[error("canonical store admission refused for {path:?}: {reason}")]
    AdmissionRefused {
        /// Existing path that was inspected without data-family creation.
        path: PathBuf,
        /// Exact incompatibility observed during admission.
        reason: String,
    },

    /// A serving store open encountered an unpublished BUILDING store.
    #[error("canonical store is not READY: {path:?}")]
    StoreNotReady {
        /// Exact store path that remains unpublished.
        path: PathBuf,
    },

    /// A `RocksDB` operation failed after identity admission.
    #[error("canonical store RocksDB {operation} failed")]
    RocksDbOperation {
        /// Concrete operation that failed.
        operation: &'static str,
        /// Underlying `RocksDB` failure.
        #[source]
        source: rust_rocksdb::Error,
    },

    /// Canonical block replay input is empty, discontinuous, or inconsistent.
    #[error("canonical block replay sequence is invalid: {reason}")]
    BlockReplaySequenceInvalid {
        /// Exact sequence invariant that failed.
        reason: String,
    },

    /// A persisted canonical block replay row is malformed or mis-keyed.
    #[error("canonical block replay at height {height} is invalid: {reason}")]
    BlockReplayInvalid {
        /// Height used to address the replay row.
        height: u32,
        /// Exact replay invariant that failed.
        reason: String,
    },

    /// A persisted replay key is not the exact version-1 ascending height key.
    #[error("canonical block replay key is invalid: {reason}")]
    BlockReplayKeyInvalid {
        /// Exact key-decoding failure.
        reason: String,
    },

    /// Canonical block-family input is empty, discontinuous, or inconsistent.
    #[error("canonical block sequence is invalid: {reason}")]
    BlockLoadSequenceInvalid {
        /// Exact sequence invariant that failed.
        reason: String,
    },

    /// Canonical block-family rows already exist in an unpublished store.
    #[error(
        "canonical block families are already populated in a BUILDING store; full build cleanup is required"
    )]
    BlockLoadAlreadyLoaded,

    /// Persisted replay rows differ from the prepared canonical block sequence.
    #[error("canonical block replay readback does not match the prepared version-1 block sequence")]
    BlockLoadReadbackMismatch,

    /// Subtree roots were requested before the canonical block families were complete.
    #[error("canonical subtree roots require completed canonical block families")]
    CanonicalBlocksNotLoaded,

    /// Subtree roots were already loaded into this fresh build.
    #[error("canonical subtree roots are already populated; full build cleanup is required")]
    SubtreeRootLoadAlreadyLoaded,

    /// Source subtree roots do not exactly cover the predecessor-to-tip ranges.
    #[error("canonical subtree-root sequence is invalid: {reason}")]
    SubtreeRootSequenceInvalid {
        /// Exact source or chain-identity mismatch.
        reason: String,
    },

    /// Persisted subtree-root rows differ from the authenticated source sequence.
    #[error("canonical subtree-root readback does not match the authenticated source sequence")]
    SubtreeRootReadbackMismatch,

    /// The final source checkpoint does not authenticate the locally accumulated fixed tip.
    #[error("canonical fixed-tip source checkpoint is invalid: {reason}")]
    SourceTipCheckpointMismatch {
        /// Exact identity, time, or frontier mismatch.
        reason: String,
    },

    /// A fresh canonical build is incomplete or changed during cold validation.
    #[error("canonical publication refused: {reason}")]
    PublicationRefused {
        /// Exact missing prerequisite or validation mismatch.
        reason: String,
    },

    /// The atomic write returned an error, so admission must determine whether it committed.
    #[error("canonical publication write outcome is unknown for {path:?}")]
    PublicationWriteOutcomeUnknown {
        /// Store path that must be reopened through normal admission.
        path: PathBuf,
        /// Underlying atomic write failure.
        #[source]
        source: rust_rocksdb::Error,
    },

    /// The atomic write committed but its immediate readback could not certify it.
    #[error(
        "canonical publication committed but immediate verification failed for {path:?}: {reason}"
    )]
    PublicationCommittedButUnverified {
        /// Store path that must be reopened through normal admission.
        path: PathBuf,
        /// Immediate verification failure.
        reason: String,
    },
}

impl CanonicalStoreError {
    fn admission(path: &std::path::Path, reason: impl Into<String>) -> Self {
        Self::AdmissionRefused {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }

    fn block_replay_sequence(reason: impl Into<String>) -> Self {
        Self::BlockReplaySequenceInvalid {
            reason: reason.into(),
        }
    }

    fn block_replay_invalid(height: BlockHeight, reason: impl Into<String>) -> Self {
        Self::BlockReplayInvalid {
            height: height.value(),
            reason: reason.into(),
        }
    }

    fn block_load_sequence(reason: impl Into<String>) -> Self {
        Self::BlockLoadSequenceInvalid {
            reason: reason.into(),
        }
    }

    fn subtree_root_sequence(reason: impl Into<String>) -> Self {
        Self::SubtreeRootSequenceInvalid {
            reason: reason.into(),
        }
    }

    fn source_tip_checkpoint(reason: impl Into<String>) -> Self {
        Self::SourceTipCheckpointMismatch {
            reason: reason.into(),
        }
    }

    fn publication(reason: impl Into<String>) -> Self {
        Self::PublicationRefused {
            reason: reason.into(),
        }
    }
}

impl From<zinder_rocksdb::BulkLoadError> for CanonicalStoreError {
    fn from(source: zinder_rocksdb::BulkLoadError) -> Self {
        match source {
            zinder_rocksdb::BulkLoadError::InvalidInput { reason } => {
                Self::block_load_sequence(reason)
            }
            zinder_rocksdb::BulkLoadError::PathUnavailable { path, source } => {
                Self::PathUnavailable { path, source }
            }
            zinder_rocksdb::BulkLoadError::RocksDbOperation { operation, source } => {
                Self::RocksDbOperation { operation, source }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn test_network_upgrade_activations(
    network: zinder_core::Network,
) -> Result<NetworkUpgradeActivations, zinder_core::NetworkUpgradeActivationsError> {
    let activations = [
        "Overwinter",
        "Sapling",
        "Blossom",
        "Heartwood",
        "Canopy",
        "NU5",
        "NU6",
        "NU6.1",
        "NU6.2",
        "NU6.3",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| zinder_core::NetworkUpgradeActivation {
        branch_id: zinder_core::ConsensusBranchId::new(
            u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1),
        ),
        activation_height: BlockHeight::new(
            u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1),
        ),
        name: name.to_owned(),
    })
    .collect();
    NetworkUpgradeActivations::new(network, activations)
}

#[cfg(test)]
pub(crate) fn test_checkpoint_frontiers(
    network_upgrade_activations: &NetworkUpgradeActivations,
    checkpoint_height: BlockHeight,
) -> CommitmentTreeFrontiers {
    let active_frontier = |protocol: ShieldedProtocol| {
        network_upgrade_activations
            .activation_height_by_name(protocol.activation_upgrade_name())
            .is_some_and(|activation_height| activation_height <= checkpoint_height)
            .then(|| zinder_core::CommitmentTreeFrontier::empty(protocol))
    };
    CommitmentTreeFrontiers::from_validated_parts(
        active_frontier(ShieldedProtocol::Sapling),
        active_frontier(ShieldedProtocol::Orchard),
        active_frontier(ShieldedProtocol::Ironwood),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{BlockHash, CommitmentTreeFrontier, Network};

    #[test]
    fn complete_build_plan_preserves_genesis_anchor_and_tip()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = zinder_core::Network::ZcashTestnet;
        let activations = test_network_upgrade_activations(network)?;
        let build_tip = BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32]));
        let plan = CanonicalStoreBuildPlan::complete(&activations, 1_234, build_tip)?;
        assert_eq!(plan.network(), network);
        assert_eq!(
            plan.network_upgrade_activations_fingerprint(),
            activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
        );
        assert_eq!(
            plan.history_predecessor().block_id,
            BlockId::new(BlockHeight::new(0), network.genesis_hash())
        );
        assert_eq!(plan.history_predecessor().block_time_seconds, 1_234);
        assert_eq!(
            plan.history_bounds().first_available_height(),
            BlockHeight::new(1)
        );
        assert_eq!(
            &plan.history_predecessor().frontiers,
            &CommitmentTreeFrontiers::default()
        );
        assert_eq!(plan.build_tip(), build_tip);
        Ok(())
    }

    #[test]
    fn build_plan_rejects_incomplete_activation_table() {
        let error = CanonicalStoreBuildPlan::complete(
            &NetworkUpgradeActivations::empty(Network::ZcashRegtest),
            0,
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
        )
        .err();
        assert_eq!(
            error,
            Some(
                CanonicalStoreBuildPlanError::MissingRequiredNetworkUpgrade { name: "Overwinter" }
            )
        );
    }

    #[test]
    fn build_plan_accepts_regtest_with_post_canopy_upgrades_disabled()
    -> Result<(), Box<dyn std::error::Error>> {
        let full_activations = test_network_upgrade_activations(Network::ZcashRegtest)?;
        let canopy_only_activations = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            full_activations.activations()[..REQUIRED_CANONICAL_NETWORK_UPGRADES.len()].to_vec(),
        )?;
        let build_tip = BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32]));

        let plan = CanonicalStoreBuildPlan::complete(&canopy_only_activations, 0, build_tip)?;

        assert_eq!(plan.build_tip(), build_tip);
        Ok(())
    }

    #[test]
    fn build_plan_rejects_tip_before_retained_history() -> Result<(), Box<dyn std::error::Error>> {
        let activations = test_network_upgrade_activations(Network::ZcashRegtest)?;
        let error = CanonicalStoreBuildPlan::complete(
            &activations,
            0,
            BlockId::new(BlockHeight::new(0), BlockHash::from_bytes([0; 32])),
        )
        .err();
        assert!(matches!(
            error,
            Some(CanonicalStoreBuildPlanError::BuildTipPrecedesHistory {
                build_tip: 0,
                first_available_height: 1,
            })
        ));
        Ok(())
    }

    #[test]
    fn checkpointed_build_plan_rejects_height_ceiling() -> Result<(), Box<dyn std::error::Error>> {
        let activations = test_network_upgrade_activations(Network::ZcashRegtest)?;
        let error = CanonicalStoreBuildPlan::checkpointed(
            &activations,
            CommitmentTreeCheckpoint::new(
                BlockId::new(BlockHeight::new(u32::MAX), BlockHash::from_bytes([9; 32])),
                99,
                CommitmentTreeFrontiers::default(),
            ),
            BlockId::new(BlockHeight::new(u32::MAX), BlockHash::from_bytes([9; 32])),
        )
        .err();
        assert_eq!(
            error,
            Some(CanonicalStoreBuildPlanError::CheckpointHasNoSuccessor)
        );
        Ok(())
    }

    #[test]
    fn checkpointed_build_plan_preserves_tree_position() -> Result<(), Box<dyn std::error::Error>> {
        let checkpoint = BlockId::new(BlockHeight::new(99), BlockHash::from_bytes([9; 32]));
        let activations = test_network_upgrade_activations(Network::ZcashTestnet)?;
        let checkpoint_frontiers = active_empty_frontiers();
        let plan = CanonicalStoreBuildPlan::checkpointed(
            &activations,
            CommitmentTreeCheckpoint::new(checkpoint, 1_234, checkpoint_frontiers.clone()),
            BlockId::new(BlockHeight::new(100), BlockHash::from_bytes([10; 32])),
        )?;

        assert_eq!(plan.history_predecessor().block_id, checkpoint);
        assert_eq!(plan.history_predecessor().block_time_seconds, 1_234);
        assert_eq!(plan.history_predecessor().frontiers, checkpoint_frontiers);
        Ok(())
    }

    #[test]
    fn complete_build_plan_domain_rejects_predecessor_frontiers()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = zinder_core::Network::ZcashTestnet;
        let activations = test_network_upgrade_activations(network)?;
        let error = CanonicalStoreBuildPlan::from_parts(
            network,
            activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            zinder_core::CanonicalHistoryBounds::complete(),
            CommitmentTreeCheckpoint::new(
                BlockId::new(BlockHeight::new(0), network.genesis_hash()),
                0,
                CommitmentTreeFrontiers::from_validated_parts(
                    Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Sapling)),
                    None,
                    None,
                ),
            ),
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
        )
        .err();

        assert_eq!(
            error,
            Some(CanonicalStoreBuildPlanError::CompleteHistoryHasFrontiers)
        );
        Ok(())
    }

    #[test]
    fn checkpointed_build_plan_rejects_genesis_and_activation_mismatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = test_network_upgrade_activations(Network::ZcashRegtest)?;
        let genesis = BlockId::new(BlockHeight::new(0), Network::ZcashRegtest.genesis_hash());
        assert_eq!(
            CanonicalStoreBuildPlan::checkpointed(
                &activations,
                CommitmentTreeCheckpoint::new(genesis, 0, CommitmentTreeFrontiers::default(),),
                BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
            )
            .err(),
            Some(CanonicalStoreBuildPlanError::CheckpointAtGenesis)
        );

        let checkpoint = BlockId::new(BlockHeight::new(3), BlockHash::from_bytes([3; 32]));
        assert!(matches!(
            CanonicalStoreBuildPlan::checkpointed(
                &activations,
                CommitmentTreeCheckpoint::new(checkpoint, 0, CommitmentTreeFrontiers::default(),),
                BlockId::new(BlockHeight::new(4), BlockHash::from_bytes([4; 32])),
            )
            .err(),
            Some(
                CanonicalStoreBuildPlanError::PredecessorFrontierActivationMismatch {
                    protocol: ShieldedProtocol::Sapling,
                    checkpoint_height: 3,
                }
            )
        ));
        Ok(())
    }

    fn active_empty_frontiers() -> CommitmentTreeFrontiers {
        CommitmentTreeFrontiers::from_validated_parts(
            Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Sapling)),
            Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Orchard)),
            Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Ironwood)),
        )
    }
}
