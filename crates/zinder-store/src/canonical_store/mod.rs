//! Version-1 canonical storage contract.
//!
//! This module owns the exact `RocksDB` layout for the clean fact-first
//! canonical data plane. It deliberately exposes no generic database adapter
//! and no compatibility decoder for earlier Zinder stores.

mod block_load;
mod block_replay;
mod builder;
mod control;
mod rocksdb;

use std::{io, path::PathBuf};

use thiserror::Error;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion, ChainEpochId,
    ChainTipMetadata,
};

pub use block_load::{CanonicalBlockLoadEvidence, CanonicalBuildBlock};
pub use builder::RocksDbCanonicalBuilder;
pub use rocksdb::RocksDbCanonicalStore;

/// Exact persisted identity of the clean canonical store.
pub const CANONICAL_STORE_IDENTITY: &str = "canonical";
/// Exact physical schema accepted by this canonical store implementation.
pub const CANONICAL_STORE_SCHEMA_VERSION: u16 = 1;

/// Fixed source-chain range for one fresh canonical construction.
///
/// The predecessor anchors the first retained block even for complete history,
/// where it is the network's height-zero block. The fixed tip prevents an
/// exhausted or failed source stream from publishing a contiguous prefix as a
/// complete build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalStoreBuildPlan {
    network: zinder_core::Network,
    history_bounds: zinder_core::CanonicalHistoryBounds,
    history_predecessor: BlockId,
    history_predecessor_tip_metadata: ChainTipMetadata,
    build_tip: BlockId,
}

impl CanonicalStoreBuildPlan {
    /// Builds a plan retaining every non-genesis block through `build_tip`.
    pub fn complete(
        network: zinder_core::Network,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        Self::from_parts(
            network,
            zinder_core::CanonicalHistoryBounds::complete(),
            BlockId::new(BlockHeight::new(0), network.genesis_hash()),
            ChainTipMetadata::empty(),
            build_tip,
        )
    }

    /// Builds a plan retaining blocks immediately after `checkpoint` through `build_tip`.
    pub fn checkpointed(
        network: zinder_core::Network,
        checkpoint: BlockId,
        checkpoint_tip_metadata: ChainTipMetadata,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        let history_bounds = zinder_core::CanonicalHistoryBounds::checkpointed(checkpoint)
            .map_err(|_| CanonicalStoreBuildPlanError::CheckpointHasNoSuccessor)?;
        Self::from_parts(
            network,
            history_bounds,
            checkpoint,
            checkpoint_tip_metadata,
            build_tip,
        )
    }

    pub(super) fn from_parts(
        network: zinder_core::Network,
        history_bounds: zinder_core::CanonicalHistoryBounds,
        history_predecessor: BlockId,
        history_predecessor_tip_metadata: ChainTipMetadata,
        build_tip: BlockId,
    ) -> Result<Self, CanonicalStoreBuildPlanError> {
        match history_bounds.preceding_checkpoint() {
            None if history_predecessor
                != BlockId::new(BlockHeight::new(0), network.genesis_hash()) =>
            {
                return Err(CanonicalStoreBuildPlanError::InvalidHistoryPredecessor);
            }
            Some(checkpoint) if history_predecessor != checkpoint => {
                return Err(CanonicalStoreBuildPlanError::InvalidHistoryPredecessor);
            }
            None | Some(_) => {}
        }
        if history_bounds.preceding_checkpoint().is_none()
            && history_predecessor_tip_metadata != ChainTipMetadata::empty()
        {
            return Err(CanonicalStoreBuildPlanError::CompleteHistoryHasNonEmptyTipMetadata);
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
            history_bounds,
            history_predecessor,
            history_predecessor_tip_metadata,
            build_tip,
        })
    }

    /// Returns the immutable network for this build.
    #[must_use]
    pub const fn network(self) -> zinder_core::Network {
        self.network
    }

    /// Returns the durable boundary of intentionally retained history.
    #[must_use]
    pub const fn history_bounds(self) -> zinder_core::CanonicalHistoryBounds {
        self.history_bounds
    }

    /// Returns the block immediately preceding retained history.
    #[must_use]
    pub const fn history_predecessor(self) -> BlockId {
        self.history_predecessor
    }

    /// Returns commitment-tree sizes immediately before retained history.
    #[must_use]
    pub const fn history_predecessor_tip_metadata(self) -> ChainTipMetadata {
        self.history_predecessor_tip_metadata
    }

    /// Returns the exact source tip this build must reach.
    #[must_use]
    pub const fn build_tip(self) -> BlockId {
        self.build_tip
    }
}

/// Invalid source-chain range for canonical construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CanonicalStoreBuildPlanError {
    /// The checkpoint cannot have retained history after it.
    #[error("canonical build checkpoint has no successor height")]
    CheckpointHasNoSuccessor,
    /// The predecessor does not match the selected complete or checkpointed history.
    #[error("canonical build history predecessor does not match its history bounds")]
    InvalidHistoryPredecessor,
    /// A complete build must start from the empty height-zero tree position.
    #[error("complete history predecessor commitment-tree sizes must be empty")]
    CompleteHistoryHasNonEmptyTipMetadata,
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
    /// Wallet APIs, including retained raw transactions.
    Wallet,
    /// Wallet APIs plus explorer raw blocks and explorer-only source facts.
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
    /// First retained block height.
    pub first_height: BlockHeight,
    /// First retained block hash in Zinder's internal byte order.
    pub first_hash: BlockHash,
    /// Visible tip height.
    pub tip_height: BlockHeight,
    /// Visible tip hash in Zinder's internal byte order.
    pub tip_hash: BlockHash,
    /// Baseline visible epoch identifier.
    pub visible_epoch: ChainEpochId,
    /// Number of contiguous retained blocks.
    pub block_count: u64,
    /// Canonical block-fact digest contract.
    pub block_digest_version: CanonicalBlockFactsDigestVersion,
    /// Canonical replay-envelope contract.
    pub replay_format_version: CanonicalBlockReplayFormatVersion,
    /// Ordered sequence-digest contract.
    pub sequence_digest_version: CanonicalBlockFactsSequenceDigestVersion,
    /// Ordered sequence digest bytes.
    pub sequence_digest: [u8; 32],
    /// Total semantic replay-envelope bytes.
    pub logical_fact_bytes: u64,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_build_plan_preserves_genesis_anchor_and_tip()
    -> Result<(), CanonicalStoreBuildPlanError> {
        let network = zinder_core::Network::ZcashTestnet;
        let build_tip = BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32]));
        let plan = CanonicalStoreBuildPlan::complete(network, build_tip)?;
        assert_eq!(plan.network(), network);
        assert_eq!(
            plan.history_predecessor(),
            BlockId::new(BlockHeight::new(0), network.genesis_hash())
        );
        assert_eq!(
            plan.history_bounds().first_available_height(),
            BlockHeight::new(1)
        );
        assert_eq!(
            plan.history_predecessor_tip_metadata(),
            ChainTipMetadata::empty()
        );
        assert_eq!(plan.build_tip(), build_tip);
        Ok(())
    }

    #[test]
    fn build_plan_rejects_tip_before_retained_history() {
        let error = CanonicalStoreBuildPlan::complete(
            zinder_core::Network::ZcashRegtest,
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
    }

    #[test]
    fn checkpointed_build_plan_rejects_height_ceiling() {
        let error = CanonicalStoreBuildPlan::checkpointed(
            zinder_core::Network::ZcashRegtest,
            BlockId::new(BlockHeight::new(u32::MAX), BlockHash::from_bytes([9; 32])),
            ChainTipMetadata::new(1, 2, 3),
            BlockId::new(BlockHeight::new(u32::MAX), BlockHash::from_bytes([9; 32])),
        )
        .err();
        assert_eq!(
            error,
            Some(CanonicalStoreBuildPlanError::CheckpointHasNoSuccessor)
        );
    }

    #[test]
    fn checkpointed_build_plan_preserves_tree_position() -> Result<(), CanonicalStoreBuildPlanError>
    {
        let checkpoint = BlockId::new(BlockHeight::new(99), BlockHash::from_bytes([9; 32]));
        let checkpoint_tip_metadata = ChainTipMetadata::new(11, 22, 33);
        let plan = CanonicalStoreBuildPlan::checkpointed(
            zinder_core::Network::ZcashTestnet,
            checkpoint,
            checkpoint_tip_metadata,
            BlockId::new(BlockHeight::new(100), BlockHash::from_bytes([10; 32])),
        )?;

        assert_eq!(plan.history_predecessor(), checkpoint);
        assert_eq!(
            plan.history_predecessor_tip_metadata(),
            checkpoint_tip_metadata
        );
        Ok(())
    }

    #[test]
    fn complete_build_plan_domain_rejects_nonempty_tree_position() {
        let network = zinder_core::Network::ZcashTestnet;
        let error = CanonicalStoreBuildPlan::from_parts(
            network,
            zinder_core::CanonicalHistoryBounds::complete(),
            BlockId::new(BlockHeight::new(0), network.genesis_hash()),
            ChainTipMetadata::new(1, 0, 0),
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32])),
        )
        .err();

        assert_eq!(
            error,
            Some(CanonicalStoreBuildPlanError::CompleteHistoryHasNonEmptyTipMetadata)
        );
    }
}
