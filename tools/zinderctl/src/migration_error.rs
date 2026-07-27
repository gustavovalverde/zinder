//! Logical migration archive errors.

use std::path::PathBuf;

use thiserror::Error;

/// A failure while capturing or replaying a logical state archive.
#[derive(Debug, Error)]
#[non_exhaustive]
pub(crate) enum MigrationError {
    /// A filesystem operation failed.
    #[error("migration filesystem error at {path}: {source}")]
    Io {
        /// Path the operation targeted.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A manifest could not be encoded or decoded.
    #[error("migration manifest JSON error: {source}")]
    Json {
        /// Underlying serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// An archive file did not match the exact format.
    #[error("migration archive format error: {reason}")]
    ArchiveFormat {
        /// Exact rejection reason.
        reason: String,
    },
    /// An upstream source operation failed.
    #[error(transparent)]
    Source(#[from] zinder_source::SourceError),
    /// Canonical fact construction failed during export.
    #[error(transparent)]
    CanonicalBlockConstruction(#[from] zinder_ingest::CanonicalBlockConstructionError),
    /// A blocking preparation task failed.
    #[error("migration archive preparation task failed: {reason}")]
    PreparationTask {
        /// Join failure.
        reason: String,
    },
    /// A caller argument was invalid.
    #[error("invalid migration argument: {reason}")]
    InvalidArgument {
        /// Rejection reason.
        reason: String,
    },
}

impl From<serde_json::Error> for MigrationError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json { source }
    }
}

impl MigrationError {
    /// Wraps a filesystem failure with its path.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Creates a format rejection.
    pub(crate) fn archive_format(reason: impl Into<String>) -> Self {
        Self::ArchiveFormat {
            reason: reason.into(),
        }
    }

    /// Creates an invalid-argument rejection.
    pub(crate) fn invalid_argument(reason: impl Into<String>) -> Self {
        Self::InvalidArgument {
            reason: reason.into(),
        }
    }

    /// Creates a blocking-task rejection.
    pub(crate) fn canonical_replay_storage_preparation_task(reason: String) -> Self {
        Self::PreparationTask { reason }
    }
}
