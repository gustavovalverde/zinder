//! Boundary error types for the derive-plane runtime.

use std::path::PathBuf;

use thiserror::Error;

/// Boundary error returned by the derive-plane runtime.
///
/// `DeriveError` is the top-level error consumers see. It folds storage and
/// transport failures behind named variants so the binary's operator-facing
/// error path stays narrow.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeriveError {
    /// `RocksDB`-shaped storage failure.
    #[error("derive store failure: {0}")]
    Store(#[from] DeriveStoreError),
    /// gRPC client transport failure when talking to upstream `zinder-query`.
    #[error("derive transport failure: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// Upstream gRPC call returned a non-Ok status.
    #[error("derive upstream error: {0}")]
    Upstream(#[from] tonic::Status),
    /// Cursor delivered by upstream did not match the persisted cursor.
    #[error("derive cursor mismatch: persisted cursor disagrees with upstream stream resume")]
    CursorMismatch,
    /// Channel C backfill could not advance because upstream lost the gap.
    #[error(
        "derive backfill gap unrecoverable: persisted={persisted}, oldest_retained={oldest_retained}"
    )]
    BackfillGapUnrecoverable {
        /// The last height the consumer persisted before going offline.
        persisted: u32,
        /// The oldest height upstream still retains.
        oldest_retained: u32,
    },
}

/// `RocksDB`-shaped failure surfaced from `DeriveStore`.
///
/// Variants are independent from `zinder_store::StoreError` because the
/// derive plane has its own column-family namespace and its own schema
/// version. The two stores share no keys.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeriveStoreError {
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
    /// Persisted schema version is incompatible with the running binary.
    #[error("derive store schema version mismatch: persisted={persisted}, running={running}")]
    SchemaMismatch {
        /// Schema version persisted on disk.
        persisted: u16,
        /// Schema version the running binary expects.
        running: u16,
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
}

/// Column-family identifier surfaced in `DeriveStoreError` variants.
///
/// Mirrors the same string value used as the `RocksDB` column-family name so
/// operator-facing logs and error messages refer to the on-disk family by its
/// canonical name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeriveStoreColumnFamily {
    /// `cursor` column family: per-consumer cursor persistence.
    Cursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl DeriveStoreColumnFamily {
    /// Returns the canonical `RocksDB` column-family name for the variant.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::ConsumerMetadata => "consumer_metadata",
        }
    }
}
