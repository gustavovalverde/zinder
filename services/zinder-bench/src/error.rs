//! Error vocabulary for the benchmark harness.

use std::path::PathBuf;

use thiserror::Error;

/// A failure raised while capturing or replaying a fixed-range fixture.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BenchError {
    /// A filesystem operation failed.
    #[error("filesystem error at {path}: {source}")]
    Io {
        /// Path the operation targeted.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A manifest or report could not be encoded or decoded as JSON.
    #[error("json error: {source}")]
    Json {
        /// Underlying serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// A fixture file did not match the expected on-disk format.
    #[error("fixture format error: {reason}")]
    FixtureFormat {
        /// Human-readable description of the mismatch.
        reason: String,
    },
    /// An upstream source call failed during capture.
    #[error("source error: {source}")]
    Source {
        /// Underlying source failure.
        #[source]
        source: zinder_source::SourceError,
    },
    /// The bulk-catchup pipeline returned an error during replay.
    #[error("ingest error: {source}")]
    Ingest {
        /// Underlying ingest failure.
        #[source]
        source: zinder_ingest::IngestError,
    },
    /// A canonical-store operation failed.
    #[error("store error: {source}")]
    Store {
        /// Underlying store failure.
        #[source]
        source: zinder_store::StoreError,
    },
    /// A derive-store operation failed while driving derive replay.
    #[error("derive store error: {source}")]
    Derive {
        /// Underlying derive-store failure.
        #[source]
        source: zinder_derive::DeriveStoreError,
    },
    /// The Prometheus recorder could not be installed.
    #[error("metrics recorder error: {reason}")]
    Recorder {
        /// Human-readable description of the failure.
        reason: String,
    },
    /// A caller-supplied argument was rejected before any work started.
    #[error("invalid argument: {reason}")]
    InvalidArgument {
        /// Human-readable description of the rejection.
        reason: String,
    },
}

impl From<serde_json::Error> for BenchError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json { source }
    }
}

impl From<zinder_source::SourceError> for BenchError {
    fn from(source: zinder_source::SourceError) -> Self {
        Self::Source { source }
    }
}

impl From<zinder_ingest::IngestError> for BenchError {
    fn from(source: zinder_ingest::IngestError) -> Self {
        Self::Ingest { source }
    }
}

impl From<zinder_store::StoreError> for BenchError {
    fn from(source: zinder_store::StoreError) -> Self {
        Self::Store { source }
    }
}

impl From<zinder_derive::DeriveStoreError> for BenchError {
    fn from(source: zinder_derive::DeriveStoreError) -> Self {
        Self::Derive { source }
    }
}

impl BenchError {
    /// Wraps a filesystem failure with the path it targeted.
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Builds a fixture-format error from a reason string.
    #[must_use]
    pub fn fixture_format(reason: impl Into<String>) -> Self {
        Self::FixtureFormat {
            reason: reason.into(),
        }
    }

    /// Builds an invalid-argument error from a reason string.
    #[must_use]
    pub fn invalid_argument(reason: impl Into<String>) -> Self {
        Self::InvalidArgument {
            reason: reason.into(),
        }
    }
}
