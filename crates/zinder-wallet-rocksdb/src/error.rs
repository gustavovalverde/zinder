//! Fail-closed errors for the concrete wallet `RocksDB` lifecycle.

use std::{io, path::PathBuf};

use thiserror::Error;
use zinder_core::{CanonicalBlockFactsSequenceLengthOverflow, Network};
use zinder_store::CanonicalStoreError;
use zinder_wallet_projection::{WalletCanonicalSourceIdentity, WalletProjectionContractError};

/// Failure to create, publish, admit, or query a version-1 wallet store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RocksDbWalletError {
    /// The authenticated canonical replay refused a row or its terminal fence.
    #[error("wallet projection canonical replay failed")]
    CanonicalReplay {
        /// Canonical storage failure preserved for diagnosis.
        #[source]
        source: CanonicalStoreError,
    },
    /// Complete-history construction observed no height-one block.
    #[error("complete-history wallet construction requires canonical block height 1")]
    EmptyCanonicalHistory,
    /// The canonical fact sequence exceeded the version-1 count domain.
    #[error(transparent)]
    SourceSequenceLength(#[from] CanonicalBlockFactsSequenceLengthOverflow),
    /// Accounted in-memory preparation would exceed the explicit tracer limit.
    #[error(
        "wallet projection preparation requires at least {required_bytes} accounted bytes, limit is {limit_bytes}"
    )]
    AccountedMemoryLimit {
        /// Caller-supplied hard limit.
        limit_bytes: u64,
        /// Minimum accounted bytes needed by the refused operation.
        required_bytes: u64,
    },
    /// Accounted retained relationship keys and values would exceed their explicit limit.
    #[error(
        "wallet cold semantic validation relationships require at least {required_bytes} accounted bytes, limit is {limit_bytes}"
    )]
    AccountedValidationRelationMemoryLimit {
        /// Caller-supplied hard limit.
        limit_bytes: u64,
        /// Minimum accounted bytes needed by the refused operation.
        required_bytes: u64,
    },
    /// A build counter exceeded the version-1 report domain.
    #[error("wallet projection build counter exceeds u64::MAX")]
    BuildCounterOverflow,
    /// The completed derivation did not match the canonical READY fence.
    #[error("wallet projection canonical source fence mismatch: {reason}")]
    CanonicalSourceFenceMismatch {
        /// Stable mismatch reason.
        reason: &'static str,
    },
    /// A fresh build target already exists, so construction refused to adopt it.
    #[error("wallet RocksDB build target is not fresh: {path}")]
    PathNotFresh {
        /// Rejected build path.
        path: PathBuf,
    },
    /// The wallet store path could not be created or resolved.
    #[error("wallet RocksDB path is unavailable at {path}: {source}")]
    PathUnavailable {
        /// Unavailable path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// The bounded resource budget violated a locked storage invariant.
    #[error("invalid wallet RocksDB resource budget: {reason}")]
    InvalidResourceBudget {
        /// Rejected budget invariant.
        reason: &'static str,
    },
    /// A bounded load cannot make progress with a zero-byte batch ceiling.
    #[error("wallet RocksDB load batch limit must be greater than zero")]
    ZeroLoadBatchLimit,
    /// One logical row cannot fit within the explicit bounded batch ceiling.
    #[error("wallet RocksDB {family} row requires {row_bytes} bytes, batch limit is {limit_bytes}")]
    RowExceedsLoadBatchLimit {
        /// Row family being loaded.
        family: &'static str,
        /// Logical durable key and value bytes.
        row_bytes: u64,
        /// Caller-supplied batch ceiling.
        limit_bytes: u64,
    },
    /// Logical row-byte or batch accounting exceeded `u64::MAX`.
    #[error("wallet RocksDB load accounting exceeds u64::MAX")]
    LoadAccountingOverflow,
    /// A concrete `RocksDB` operation failed.
    #[error("wallet RocksDB {operation} failed: {source}")]
    RocksDbOperation {
        /// Stable operation label.
        operation: &'static str,
        /// Driver error.
        #[source]
        source: rust_rocksdb::Error,
    },
    /// The database does not contain exactly the version-1 wallet families.
    #[error(
        "wallet RocksDB column families do not match version 1; expected {expected:?}, observed {observed:?}"
    )]
    ColumnFamilyContractMismatch {
        /// Exact required family names.
        expected: Vec<String>,
        /// Observed family names.
        observed: Vec<String>,
    },
    /// The default column family did not contain exactly one control record.
    #[error("wallet RocksDB default column family must contain only store_control")]
    StoreControlCardinalityMismatch,
    /// The singleton wallet control record is absent.
    #[error("wallet RocksDB store_control record is missing")]
    StoreControlMissing,
    /// A query open encountered an unpublished BUILDING store.
    #[error("wallet RocksDB store is not READY: {path}")]
    StoreNotReady {
        /// Rejected store path.
        path: PathBuf,
    },
    /// The admitted store belongs to a different network.
    #[error("wallet RocksDB network mismatch; expected {expected:?}, observed {observed:?}")]
    NetworkMismatch {
        /// Operator-selected network.
        expected: Network,
        /// Network committed by the control record.
        observed: Network,
    },
    /// The READY projection does not represent the caller's authenticated canonical source.
    #[error(
        "wallet RocksDB canonical source mismatch; expected {expected:?}, observed {observed:?}"
    )]
    CanonicalSourceMismatch {
        /// Canonical identity required by the serving process.
        expected: Box<WalletCanonicalSourceIdentity>,
        /// Canonical identity committed by the READY wallet control record.
        observed: Box<WalletCanonicalSourceIdentity>,
    },
    /// A page continuation belongs to an address other than the requested address.
    #[error("wallet RocksDB {index} continuation belongs to a different address")]
    ContinuationAddressMismatch {
        /// Stable index name whose continuation was rejected.
        index: &'static str,
    },
    /// A required data family could not be resolved after exact admission.
    #[error("wallet RocksDB required column family is unavailable: {name}")]
    ColumnFamilyUnavailable {
        /// Required family name.
        name: &'static str,
    },
    /// The store changed between admission and its bounded serving open.
    #[error("wallet RocksDB admission failed: {reason}")]
    AdmissionChanged {
        /// Failed admission invariant.
        reason: &'static str,
    },
    /// Durable wallet bytes violate the clean version-1 contract.
    #[error(transparent)]
    Contract(#[from] WalletProjectionContractError),
}

impl RocksDbWalletError {
    pub(crate) fn rocksdb(operation: &'static str, source: rust_rocksdb::Error) -> Self {
        Self::RocksDbOperation { operation, source }
    }
}
