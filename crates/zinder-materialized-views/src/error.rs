//! Boundary error types for the materialized-view runtime.

use std::path::PathBuf;

use thiserror::Error;
use zinder_store::MempoolDecodeError;

use crate::consumer::MaterializedViewConsumerError;

/// Boundary error returned by the materialized-view runtime.
///
/// `MaterializedViewError` is the top-level error consumers see. It folds storage,
/// decode, and consumer failures behind named variants so the binary's
/// operator-facing error path stays narrow.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MaterializedViewError {
    /// `RocksDB`-shaped storage failure.
    #[error("materialized-view store failure: {0}")]
    Store(#[from] MaterializedViewStoreError),
    /// Wire envelope failed to decode into the typed event shape the
    /// subscriber dispatches to consumers.
    #[error("materialized-view event decode failure: {0}")]
    Decode(#[from] MempoolDecodeError),
    /// Consumer `apply_*` hook returned a domain error.
    #[error("materialized-view consumer apply failure: {0}")]
    Consumer(#[source] MaterializedViewConsumerError),
    /// Cursor delivered by upstream did not match the persisted cursor.
    #[error(
        "materialized-view cursor mismatch: persisted cursor disagrees with upstream stream resume"
    )]
    CursorMismatch,
    /// Chain-event dispatch was asked to process a variant no chain consumer
    /// understands.
    #[error("materialized-view chain-event dispatch received an unsupported chain event variant")]
    UnsupportedChainEvent,
}

/// `RocksDB`-shaped failure surfaced from `MaterializedViewStore`.
///
/// Variants are independent from `zinder_store::StoreError` because the
/// materialized-view plane has its own column-family namespace and its own schema
/// version. The two stores share no keys.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MaterializedViewStoreError {
    /// Store options would reopen an unbounded `RocksDB` resource path.
    #[error("invalid materialized-view store options: {reason}")]
    InvalidOptions {
        /// Validation failure reason.
        reason: &'static str,
    },
    /// A declared consumer column family is not unique across consumers or
    /// reuses a name reserved by the store (a store table or the `RocksDB`
    /// default family).
    #[error(
        "materialized-view store consumer column family `{name}` is declared by more than one consumer or reuses a reserved name"
    )]
    ConsumerColumnFamilyConflict {
        /// Column family name that collided.
        name: &'static str,
    },
    /// `RocksDB` could not open the database at the configured path.
    #[error("materialized-view store could not open RocksDB at {path:?}: {source}")]
    Open {
        /// Path the operator configured.
        path: PathBuf,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// `RocksDB` returned an error during a put, get, or batch write.
    #[error(
        "materialized-view store {operation} failed for column family {column_family:?}: {source}"
    )]
    Operation {
        /// Logical operation that failed (e.g. `put`, `get`, `delete`).
        operation: &'static str,
        /// Column family the operation targeted.
        column_family: MaterializedViewStoreColumnFamily,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// Stored bytes did not decode as the expected payload shape.
    #[error(
        "materialized-view store payload decode failed for column family {column_family:?}: {reason}"
    )]
    Decode {
        /// Column family whose payload failed to decode.
        column_family: MaterializedViewStoreColumnFamily,
        /// Operator-facing reason describing the decode failure.
        reason: String,
    },
    /// A writer staged consumer projection coverage whose bounds violate the
    /// `complete_from_height <= complete_through_height <= projection_tip_height`
    /// ordering the on-disk record requires. The store refuses to persist it so
    /// no reader can later decode an undecodable record.
    #[error(
        "materialized-view store consumer `{consumer}` projection coverage bounds are invalid: complete_from_height={complete_from_height}, complete_through_height={complete_through_height}, projection_tip_height={projection_tip_height}"
    )]
    InvalidProjectionCoverage {
        /// Consumer whose coverage bounds were rejected.
        consumer: &'static str,
        /// Staged first verified height.
        complete_from_height: u32,
        /// Staged last verified height.
        complete_through_height: u32,
        /// Staged projection tip height.
        projection_tip_height: u32,
    },
    /// Persisted store-format version is incompatible with the running binary.
    #[error(
        "materialized-view store format version mismatch: persisted={persisted}, running={running}"
    )]
    SchemaMismatch {
        /// Store-format version persisted on disk.
        persisted: u16,
        /// Store-format version the running binary expects.
        running: u16,
    },
    /// The requested preset conflicts with the store's durable projection
    /// identities.
    #[error(
        "materialized-view store projection identities are incompatible with requested preset {requested}; configure a new empty canonical storage path and re-ingest from upstream because in-place preset changes are unsupported"
    )]
    ProjectionPresetRequiresFreshStore {
        /// Non-default preset requested by the opening process.
        requested: &'static str,
    },
    /// A projection-owned write targeted an identity omitted by the opened
    /// workload.
    #[error(
        "materialized-view store projection `{projection}` is not selected by the opened workload"
    )]
    ProjectionNotSelected {
        /// Stable projection identity rejected by the store.
        projection: &'static str,
    },
    /// A declared consumer's persisted schema contract cannot be read safely
    /// by the running binary. Secondary readers reject this rather than decode
    /// incompatible rows. A primary rebuilds older incompatible versions but
    /// rejects newer persisted versions so rollback never destroys that
    /// consumer's newer rows.
    /// `persisted` is `None` when the primary has not recorded the consumer.
    #[error(
        "materialized-view store consumer `{consumer}` schema version mismatch: persisted={persisted:?}, running={running}"
    )]
    ConsumerSchemaMismatch {
        /// Consumer whose recorded version diverged.
        consumer: &'static str,
        /// Schema version recorded in the manifest, or `None` when absent.
        persisted: Option<u16>,
        /// Schema version the running binary declares.
        running: u16,
    },
    /// The manifest contains a consumer the running binary did not declare.
    ///
    /// Consumer removal is destructive and therefore cannot be inferred from
    /// absence in an older or differently configured binary. The store fails
    /// closed until an explicit removal migration exists.
    #[error(
        "materialized-view store manifest contains undeclared consumer `{consumer}` at schema version {persisted_schema_version}"
    )]
    ConsumerNotDeclared {
        /// Consumer name persisted in the manifest.
        consumer: String,
        /// Latest writer schema version recorded for that consumer.
        persisted_schema_version: u16,
    },
    /// Per-consumer schema reconciliation failed while opening the store.
    ///
    /// Returned when a consumer whose declared version moved could not have
    /// its column-family rows cleared, or when the persisted consumer manifest
    /// could not be read or rewritten.
    #[error(
        "materialized-view store consumer schema reconciliation failed during {operation}: {reason}"
    )]
    SchemaReconcile {
        /// Reconciliation step that failed.
        operation: &'static str,
        /// Operator-facing reason describing the failure.
        reason: String,
    },
    /// Column-family handle was unexpectedly absent after open.
    ///
    /// `RocksDB` returns `None` from `cf_handle` if the named column family
    /// was not registered when the database opened. The materialized-view store always
    /// registers every variant of [`MaterializedViewStoreColumnFamily`] at open time,
    /// so this variant indicates an internal invariant violation and never
    /// fires during normal operation.
    #[error("materialized-view store column family {column_family:?} missing after open")]
    ColumnFamilyMissing {
        /// Column family that could not be resolved.
        column_family: MaterializedViewStoreColumnFamily,
    },
    /// Consumer-owned column family handle was unexpectedly absent.
    ///
    /// Returned by [`crate::store::MaterializedViewStore::consumer_column_family`] when
    /// the requested name was not registered through
    /// [`crate::store::MaterializedViewStoreOptions::consumers`] before the store
    /// opened.
    #[error("materialized-view store consumer column family {name} missing after open")]
    ConsumerColumnFamilyMissing {
        /// Column family name the consumer asked for.
        name: &'static str,
    },
    /// A projection-specific cursor could not be decoded or did not match its
    /// read request.
    #[error("materialized view {projection} cursor is invalid: {reason}")]
    ProjectionCursorInvalid {
        /// Stable projection identity that owns the cursor.
        projection: &'static str,
        /// Specific validation failure.
        reason: &'static str,
    },
    /// Operation on a consumer-owned column family failed.
    #[error(
        "materialized-view store {operation} failed for consumer column family {name}: {source}"
    )]
    ConsumerOperation {
        /// Logical operation that failed (e.g. `get`, `range_iterate`).
        operation: &'static str,
        /// Consumer-owned column family the operation targeted.
        name: &'static str,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// A consumer-owned row could not be interpreted by a bounded scan.
    #[error(
        "materialized-view store payload decode failed for consumer column family {name}: {reason}"
    )]
    ConsumerPayloadDecode {
        /// Consumer-owned column family being scanned.
        name: &'static str,
        /// Operator-facing reason describing the invalid row.
        reason: String,
    },
    /// A checkpoint was requested from a secondary reader.
    #[error(
        "materialized-view store checkpoint requires a primary store; opened secondary at {path:?}"
    )]
    CheckpointRequiresPrimary {
        /// Materialized-view store path opened by the secondary reader.
        path: PathBuf,
    },
    /// `RocksDB` could not create a checkpoint at the requested path.
    #[error("materialized-view store checkpoint at {path:?} failed: {source}")]
    Checkpoint {
        /// Checkpoint destination path.
        path: PathBuf,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
}

/// Column-family identifier surfaced in `MaterializedViewStoreError` variants.
///
/// Mirrors the same string value used as the `RocksDB` column-family name so
/// operator-facing logs and error messages refer to the on-disk family by its
/// canonical name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MaterializedViewStoreColumnFamily {
    /// `chain_event_cursor` column family: per-chain-consumer cursor persistence.
    ChainEventCursor,
    /// `mempool_event_cursor` column family: per-mempool-consumer cursor persistence.
    MempoolEventCursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl MaterializedViewStoreColumnFamily {
    /// Returns the canonical `RocksDB` column-family name for the variant.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChainEventCursor => "chain_event_cursor",
            Self::MempoolEventCursor => "mempool_event_cursor",
            Self::ConsumerMetadata => "consumer_metadata",
        }
    }
}
