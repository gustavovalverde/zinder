//! Version-1 canonical storage contract.
//!
//! This module owns the exact `RocksDB` layout for the clean fact-first
//! canonical data plane. It deliberately exposes no generic database adapter
//! and no compatibility decoder for earlier Zinder stores.

mod control;
mod rocksdb;

use std::{io, path::PathBuf};

use thiserror::Error;
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockReplayFormatVersion, ChainEpochId,
};

pub use rocksdb::RocksDbCanonicalStore;

/// Exact persisted identity of the clean canonical store.
pub const CANONICAL_STORE_IDENTITY: &str = "canonical";
/// Exact physical schema accepted by this canonical store implementation.
pub const CANONICAL_STORE_SCHEMA_VERSION: u16 = 1;

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

    /// A `RocksDB` operation failed after identity admission.
    #[error("canonical store RocksDB {operation} failed")]
    RocksDbOperation {
        /// Concrete operation that failed.
        operation: &'static str,
        /// Underlying `RocksDB` failure.
        #[source]
        source: rust_rocksdb::Error,
    },
}

impl CanonicalStoreError {
    fn admission(path: &std::path::Path, reason: impl Into<String>) -> Self {
        Self::AdmissionRefused {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}
