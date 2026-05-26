//! Storage error vocabulary.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::format::StoreKey;
use zinder_core::{BlockHeight, ChainEpochId, Network};

/// Stable storage-engine failure category for operator diagnostics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StorageErrorKind {
    /// `RocksDB` returned an error.
    RocksDb,
}

/// Artifact family used in storage errors and key diagnostics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ArtifactFamily {
    /// Chain epoch metadata.
    ChainEpoch,
    /// Chain event envelope.
    ChainEvent,
    /// Canonical block-header facts.
    BlockHeader,
    /// Optional raw block blob.
    BlockBlob,
    /// Compact block artifact.
    CompactBlock,
    /// Block-local transaction index row.
    BlockTransactionIndex,
    /// Mined transaction location.
    TransactionLocation,
    /// Canonical transaction facts.
    TransactionFacts,
    /// Optional raw transaction blob.
    TransactionBlob,
    /// Commitment tree-state artifact.
    TreeState,
    /// Commitment subtree-root artifact.
    SubtreeRoot,
    /// Transparent address output artifact.
    AddressOutputIndex,
    /// Transparent-output artifact keyed by outpoint.
    TransparentOutput,
    /// Resolved transparent spend fact.
    TransparentSpendFact,
    /// Transparent address tx-history index artifact.
    TransparentAddressTxIndex,
    /// Best-chain block-hash to height index entry.
    BlockHashIndex,
    /// Mempool event envelope.
    MempoolEvent,
}

/// Opaque storage-key bytes included in diagnostic storage errors.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct StorageKey(Vec<u8>);

impl StorageKey {
    /// Returns the encoded storage-key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for StorageKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("StorageKey").field(&self.0).finish()
    }
}

impl From<&StoreKey> for StorageKey {
    fn from(key: &StoreKey) -> Self {
        Self(key.as_bytes().to_vec())
    }
}

impl From<StoreKey> for StorageKey {
    fn from(key: StoreKey) -> Self {
        Self(key.into_bytes())
    }
}

/// Error returned by canonical storage operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Storage engine or filesystem access failed.
    #[error("storage is unavailable: {kind:?}")]
    StorageUnavailable {
        /// Stable failure category.
        kind: StorageErrorKind,
        /// Underlying storage-engine error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// Operating-system entropy was unavailable while creating store secrets.
    #[error("entropy is unavailable")]
    EntropyUnavailable {
        /// Underlying entropy failure.
        #[source]
        source: getrandom::Error,
    },

    /// Requested chain epoch does not exist.
    #[error("chain epoch {chain_epoch:?} was not found")]
    ChainEpochMissing {
        /// Missing chain epoch id.
        chain_epoch: ChainEpochId,
    },

    /// No visible chain epoch has been committed yet.
    #[error("no visible chain epoch has been committed")]
    NoVisibleChainEpoch,

    /// Attempted chain epoch conflicts with the current visible epoch.
    #[error("chain epoch conflict: current {current:?}, attempted {attempted:?}")]
    ChainEpochConflict {
        /// Current visible chain epoch id.
        current: ChainEpochId,
        /// Attempted chain epoch id.
        attempted: ChainEpochId,
    },

    /// Attempted commit belongs to a different network than the current store.
    #[error("chain epoch network mismatch: current {current:?}, attempted {attempted:?}")]
    ChainEpochNetworkMismatch {
        /// Current store network.
        current: Network,
        /// Attempted commit network.
        attempted: Network,
    },

    /// Persisted store schema version does not match the binary's expected version.
    ///
    /// Operators must run an explicit migration that produces a store at the
    /// expected schema version before retrying. The binary refuses to silently
    /// upgrade or downgrade canonical state.
    #[error(
        "store schema mismatch: persisted version {persisted_version}, expected {expected_version}"
    )]
    SchemaMismatch {
        /// Schema version recorded on disk.
        persisted_version: u16,
        /// Schema version expected by the running binary.
        expected_version: u16,
    },

    /// Persisted artifact schema is newer than this binary supports.
    #[error(
        "store artifact schema is too new: persisted {persisted_version}, supported {supported_version}"
    )]
    SchemaTooNew {
        /// Artifact schema version recorded on disk.
        persisted_version: u16,
        /// Highest artifact schema version supported by the running binary.
        supported_version: u16,
    },

    /// Persisted artifact schema is older than this binary requires.
    ///
    /// Wipe the store and resync with the schema required by this binary.
    #[error(
        "store artifact schema is too old: persisted {persisted_version}, required {required_version}; wipe the store and resync"
    )]
    SchemaTooOld {
        /// Artifact schema version recorded on disk.
        persisted_version: u16,
        /// Artifact schema version this binary requires.
        required_version: u16,
    },

    /// Another primary process already owns the `RocksDB` lock.
    #[error("primary store is already open: {lock_path:?}")]
    PrimaryAlreadyOpen {
        /// `RocksDB` lock path for operator diagnostics.
        lock_path: PathBuf,
    },

    /// A `RocksDB` secondary failed to catch up with its primary.
    #[error("secondary catchup failed")]
    SecondaryCatchupFailed {
        /// Underlying `RocksDB` error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// `RocksDB` checkpoint creation failed.
    #[error("checkpoint at {path:?} is unavailable")]
    CheckpointUnavailable {
        /// Requested checkpoint path.
        path: PathBuf,
        /// Underlying `RocksDB` or filesystem error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    /// Attempted replacement crossed the configured reorg boundary.
    #[error(
        "reorg window exceeded: attempted from {attempted_from_height:?}, minimum allowed {minimum_reorg_height:?}, safe tip {safe_tip_height:?}"
    )]
    ReorgWindowExceeded {
        /// First height requested for replacement.
        attempted_from_height: BlockHeight,
        /// Earliest height that may be replaced.
        minimum_reorg_height: BlockHeight,
        /// Current safe tip height (boundary above which reorgs are still possible).
        safe_tip_height: BlockHeight,
    },

    /// Chain event cursor points before retained chain-event history.
    #[error(
        "chain event cursor expired: event sequence {event_sequence}, oldest retained {oldest_retained_sequence}"
    )]
    ChainEventCursorExpired {
        /// Cursor event sequence.
        event_sequence: u64,
        /// Oldest retained chain event sequence.
        oldest_retained_sequence: u64,
    },

    /// Chain event cursor failed validation.
    #[error("chain event cursor is invalid: {reason}")]
    ChainEventCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Transparent output cursor failed validation.
    #[error("transparent output cursor is invalid: {reason}")]
    AddressOutputCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Transparent address tx-history cursor failed validation.
    #[error("transparent history cursor is invalid: {reason}")]
    TransparentHistoryCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Mempool event cursor points before retained mempool-event history.
    #[error(
        "mempool event cursor expired: event sequence {event_sequence}, oldest retained {oldest_retained_sequence}"
    )]
    MempoolEventCursorExpired {
        /// Cursor event sequence.
        event_sequence: u64,
        /// Oldest retained mempool event sequence.
        oldest_retained_sequence: u64,
    },

    /// Mempool event cursor failed validation.
    #[error("mempool event cursor is invalid: {reason}")]
    MempoolEventCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Chain event sequence reached the maximum representable value.
    #[error("chain event sequence overflow")]
    ChainEventSequenceOverflow,

    /// Mempool event sequence reached the maximum representable value.
    #[error("mempool event sequence overflow")]
    MempoolEventSequenceOverflow,

    /// Chain epoch id reached the maximum representable value.
    #[error("chain epoch sequence overflow")]
    ChainEpochSequenceOverflow,

    /// Commit value failed domain validation before a durable write.
    #[error("invalid chain epoch artifacts: {reason}")]
    InvalidChainEpochArtifacts {
        /// Validation failure reason.
        reason: &'static str,
    },

    /// Artifact payload is too large for the v1 storage envelope.
    #[error("artifact payload in {family:?} is too large: {payload_len} bytes")]
    ArtifactPayloadTooLarge {
        /// Artifact family that could not be encoded.
        family: ArtifactFamily,
        /// Payload byte length.
        payload_len: usize,
    },

    /// Store options are invalid.
    #[error("invalid chain store options: {reason}")]
    InvalidChainStoreOptions {
        /// Validation failure reason.
        reason: &'static str,
    },

    /// Artifact required by an epoch-bound read is missing.
    #[error("missing artifact in {family:?} for key {key:?}")]
    ArtifactMissing {
        /// Missing artifact family.
        family: ArtifactFamily,
        /// Missing artifact key.
        key: StorageKey,
    },

    /// Artifact exists but cannot be decoded or validated.
    #[error("corrupt artifact in {family:?} for key {key:?}: {reason}")]
    ArtifactCorrupt {
        /// Corrupt artifact family.
        family: ArtifactFamily,
        /// Corrupt artifact key.
        key: StorageKey,
        /// Corruption reason.
        reason: &'static str,
    },

    /// Requested feature is not implemented by this storage backend.
    #[error("unsupported storage feature: {feature}")]
    Unsupported {
        /// Unsupported feature name.
        feature: &'static str,
    },
}

impl StoreError {
    pub(crate) fn storage_unavailable(source: impl Error + Send + Sync + 'static) -> Self {
        Self::StorageUnavailable {
            kind: StorageErrorKind::RocksDb,
            source: Box::new(source),
        }
    }

    pub(crate) fn primary_open_failed(
        path: &Path,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        if source.to_string().to_ascii_lowercase().contains("lock") {
            return Self::PrimaryAlreadyOpen {
                lock_path: path.join("LOCK"),
            };
        }

        Self::storage_unavailable(source)
    }

    pub(crate) fn secondary_catchup_failed(source: impl Error + Send + Sync + 'static) -> Self {
        Self::SecondaryCatchupFailed {
            source: Box::new(source),
        }
    }

    pub(crate) fn checkpoint_unavailable(
        path: &Path,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::CheckpointUnavailable {
            path: path.to_path_buf(),
            source: Box::new(source),
        }
    }
}
