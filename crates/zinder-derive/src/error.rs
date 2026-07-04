//! Boundary error types for the derive-plane runtime.

use std::path::PathBuf;

use thiserror::Error;
use zinder_store::MempoolDecodeError;

use crate::consumer::DeriveConsumerError;

/// Boundary error returned by the derive-plane runtime.
///
/// `DeriveError` is the top-level error consumers see. It folds storage,
/// decode, and consumer failures behind named variants so the binary's
/// operator-facing error path stays narrow.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeriveError {
    /// `RocksDB`-shaped storage failure.
    #[error("derive store failure: {0}")]
    Store(#[from] DeriveStoreError),
    /// Wire envelope failed to decode into the typed event shape the
    /// subscriber dispatches to consumers.
    #[error("derive event decode failure: {0}")]
    Decode(#[from] MempoolDecodeError),
    /// Consumer `apply_*` hook returned a domain error.
    #[error("derive consumer apply failure: {0}")]
    Consumer(#[source] DeriveConsumerError),
    /// Cursor delivered by upstream did not match the persisted cursor.
    #[error("derive cursor mismatch: persisted cursor disagrees with upstream stream resume")]
    CursorMismatch,
    /// Chain-event dispatch was asked to process a variant no chain consumer
    /// understands.
    #[error("derive chain-event dispatch received an unsupported chain event variant")]
    UnsupportedChainEvent,
}

/// `RocksDB`-shaped failure surfaced from `DeriveStore`.
///
/// Variants are independent from `zinder_store::StoreError` because the
/// derive plane has its own column-family namespace and its own schema
/// version. The two stores share no keys.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeriveStoreError {
    /// Store options would reopen an unbounded `RocksDB` resource path.
    #[error("invalid derive store options: {reason}")]
    InvalidOptions {
        /// Validation failure reason.
        reason: &'static str,
    },
    /// A declared consumer column family is not unique across consumers or
    /// reuses a name reserved by the store (a store table or the `RocksDB`
    /// default family).
    #[error(
        "derive store consumer column family `{name}` is declared by more than one consumer or reuses a reserved name"
    )]
    ConsumerColumnFamilyConflict {
        /// Column family name that collided.
        name: &'static str,
    },
    /// `RocksDB` could not open the database at the configured path.
    #[error("derive store could not open RocksDB at {path:?}: {source}")]
    Open {
        /// Path the operator configured.
        path: PathBuf,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// `RocksDB` returned an error during a put, get, or batch write.
    #[error("derive store {operation} failed for column family {column_family:?}: {source}")]
    Operation {
        /// Logical operation that failed (e.g. `put`, `get`, `delete`).
        operation: &'static str,
        /// Column family the operation targeted.
        column_family: DeriveStoreColumnFamily,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// Stored bytes did not decode as the expected payload shape.
    #[error("derive store payload decode failed for column family {column_family:?}: {reason}")]
    Decode {
        /// Column family whose payload failed to decode.
        column_family: DeriveStoreColumnFamily,
        /// Operator-facing reason describing the decode failure.
        reason: String,
    },
    /// Persisted store-format version is incompatible with the running binary.
    #[error("derive store format version mismatch: persisted={persisted}, running={running}")]
    SchemaMismatch {
        /// Store-format version persisted on disk.
        persisted: u16,
        /// Store-format version the running binary expects.
        running: u16,
    },
    /// A declared consumer's persisted schema version disagrees with the
    /// running binary. Secondary readers reject this rather than decode rows
    /// written under a different consumer layout; the primary rebuilds the
    /// consumer and rewrites the manifest before the reader can proceed.
    /// `persisted` is `None` when the primary has not recorded the consumer.
    #[error(
        "derive store consumer `{consumer}` schema version mismatch: persisted={persisted:?}, running={running}"
    )]
    ConsumerSchemaMismatch {
        /// Consumer whose recorded version diverged.
        consumer: &'static str,
        /// Schema version recorded in the manifest, or `None` when absent.
        persisted: Option<u16>,
        /// Schema version the running binary declares.
        running: u16,
    },
    /// Per-consumer schema reconciliation failed while opening the store.
    ///
    /// Returned when a consumer whose declared version moved could not have
    /// its column-family rows cleared, or when the persisted consumer manifest
    /// could not be read or rewritten.
    #[error("derive store consumer schema reconciliation failed during {operation}: {reason}")]
    SchemaReconcile {
        /// Reconciliation step that failed.
        operation: &'static str,
        /// Operator-facing reason describing the failure.
        reason: String,
    },
    /// Column-family handle was unexpectedly absent after open.
    ///
    /// `RocksDB` returns `None` from `cf_handle` if the named column family
    /// was not registered when the database opened. The derive store always
    /// registers every variant of [`DeriveStoreColumnFamily`] at open time,
    /// so this variant indicates an internal invariant violation and never
    /// fires during normal operation.
    #[error("derive store column family {column_family:?} missing after open")]
    ColumnFamilyMissing {
        /// Column family that could not be resolved.
        column_family: DeriveStoreColumnFamily,
    },
    /// Consumer-owned column family handle was unexpectedly absent.
    ///
    /// Returned by [`crate::store::DeriveStore::consumer_column_family`] when
    /// the requested name was not registered through
    /// [`crate::store::DeriveStoreOptions::consumers`] before the store
    /// opened.
    #[error("derive store consumer column family {name} missing after open")]
    ConsumerColumnFamilyMissing {
        /// Column family name the consumer asked for.
        name: &'static str,
    },
    /// Operation on a consumer-owned column family failed.
    #[error("derive store {operation} failed for consumer column family {name}: {source}")]
    ConsumerOperation {
        /// Logical operation that failed (e.g. `get`, `range_iterate`).
        operation: &'static str,
        /// Consumer-owned column family the operation targeted.
        name: &'static str,
        /// Underlying `RocksDB` error.
        #[source]
        source: rust_rocksdb::Error,
    },
}

/// Column-family identifier surfaced in `DeriveStoreError` variants.
///
/// Mirrors the same string value used as the `RocksDB` column-family name so
/// operator-facing logs and error messages refer to the on-disk family by its
/// canonical name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeriveStoreColumnFamily {
    /// `chain_event_cursor` column family: per-chain-consumer cursor persistence.
    ChainEventCursor,
    /// `mempool_event_cursor` column family: per-mempool-consumer cursor persistence.
    MempoolEventCursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl DeriveStoreColumnFamily {
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
