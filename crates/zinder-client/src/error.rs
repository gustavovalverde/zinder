//! Public client error vocabulary.

use thiserror::Error;
use tonic::Code;
use zinder_core::Network;
use zinder_store::StoreError;

/// Error returned by [`crate::ChainIndex`] implementations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexerError {
    /// No visible chain epoch has been committed yet.
    #[error("no visible chain epoch has been committed")]
    NoVisibleChainEpoch,

    /// Requested data is not indexed in the visible chain.
    #[error("{resource} was not found")]
    NotFound {
        /// Resource kind.
        resource: &'static str,
    },

    /// Request failed local validation or remote argument validation.
    #[error("invalid request: {reason}")]
    InvalidRequest {
        /// Stable diagnostic reason.
        reason: String,
    },

    /// Requested operation is unavailable until the deployment is reconfigured.
    #[error("operation failed precondition: {reason}")]
    FailedPrecondition {
        /// Stable diagnostic reason.
        reason: String,
    },

    /// Stored or transmitted data could not be decoded.
    #[error("data loss: {reason}")]
    DataLoss {
        /// Stable diagnostic reason.
        reason: String,
    },

    /// Storage could not serve the request.
    #[error("storage is unavailable: {reason}")]
    StorageUnavailable {
        /// Stable diagnostic reason.
        reason: String,
    },

    /// Remote service could not serve the request.
    #[error("remote service is unavailable: {reason}")]
    ServiceUnavailable {
        /// Stable diagnostic reason.
        reason: String,
    },

    /// A method requiring a service endpoint was called on a local index that
    /// was opened without one.
    #[error("remote endpoint is not configured for {operation}")]
    RemoteEndpointUnconfigured {
        /// Operation that needs a service endpoint.
        operation: &'static str,
    },

    /// A response was missing a required field or carried invalid bytes.
    #[error("malformed response field {field}: {reason}")]
    MalformedResponse {
        /// Field path.
        field: &'static str,
        /// Stable diagnostic reason.
        reason: String,
    },

    /// Remote service returned data for a different network.
    #[error("network mismatch: expected {expected:?}, actual {actual}")]
    NetworkMismatch {
        /// Expected network.
        expected: Network,
        /// Remote network name.
        actual: String,
    },

    /// A blocking local read task failed unexpectedly.
    #[error("blocking task failed: {reason}")]
    BlockingTaskFailed {
        /// Stable diagnostic reason.
        reason: String,
    },
}

impl IndexerError {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "StoreError is consumed through map_err adapters at storage boundaries"
    )]
    pub(crate) fn from_store_error(error: StoreError) -> Self {
        match error {
            StoreError::NoVisibleChainEpoch => Self::NoVisibleChainEpoch,
            StoreError::ChainEpochMissing { .. } | StoreError::ArtifactMissing { .. } => {
                Self::NotFound {
                    resource: "artifact",
                }
            }
            StoreError::EventCursorInvalid { reason }
            | StoreError::InvalidChainEpochArtifacts { reason }
            | StoreError::InvalidChainStoreOptions { reason }
            | StoreError::ArtifactCorrupt { reason, .. }
            | StoreError::Unsupported { feature: reason } => Self::InvalidRequest {
                reason: reason.to_owned(),
            },
            StoreError::EventCursorExpired {
                event_sequence,
                oldest_retained_sequence,
            } => Self::FailedPrecondition {
                reason: format!(
                    "event cursor {event_sequence} is before oldest retained event {oldest_retained_sequence}"
                ),
            },
            StoreError::StorageUnavailable { .. }
            | StoreError::EntropyUnavailable { .. }
            | StoreError::ChainEpochConflict { .. }
            | StoreError::ChainEpochNetworkMismatch { .. }
            | StoreError::SchemaMismatch { .. }
            | StoreError::SchemaTooNew { .. }
            | StoreError::PrimaryAlreadyOpen { .. }
            | StoreError::SecondaryCatchupFailed { .. }
            | StoreError::CheckpointUnavailable { .. }
            | StoreError::ReorgWindowExceeded { .. }
            | StoreError::ChainEventSequenceOverflow
            | StoreError::ChainEpochSequenceOverflow
            | StoreError::ArtifactPayloadTooLarge { .. }
            | _ => Self::StorageUnavailable {
                reason: error.to_string(),
            },
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "tonic::Status is consumed through map_err adapters at gRPC boundaries"
    )]
    pub(crate) fn from_status(status: tonic::Status) -> Self {
        let reason = status.message().to_owned();
        match status.code() {
            Code::InvalidArgument => Self::InvalidRequest { reason },
            Code::FailedPrecondition => Self::FailedPrecondition { reason },
            Code::NotFound => Self::NotFound {
                resource: "artifact",
            },
            Code::DataLoss => Self::DataLoss { reason },
            Code::Ok
            | Code::Cancelled
            | Code::Unknown
            | Code::DeadlineExceeded
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::OutOfRange
            | Code::Unimplemented
            | Code::Internal
            | Code::Unavailable
            | Code::Unauthenticated => Self::ServiceUnavailable { reason },
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "tonic transport errors are consumed when connection setup fails"
    )]
    pub(crate) fn from_transport_error(error: tonic::transport::Error) -> Self {
        Self::ServiceUnavailable {
            reason: error.to_string(),
        }
    }

    pub(crate) fn malformed(field: &'static str, reason: impl Into<String>) -> Self {
        Self::MalformedResponse {
            field,
            reason: reason.into(),
        }
    }

    pub(crate) fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: reason.into(),
        }
    }
}
