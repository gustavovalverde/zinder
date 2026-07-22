//! Public client error vocabulary.

use thiserror::Error;
#[cfg(feature = "remote")]
use tonic::Code;
#[cfg(feature = "remote")]
use tonic_types::StatusExt;
use zinder_core::Network;
#[cfg(feature = "remote")]
use zinder_proto::v1::ops::ErrorReason;

/// Domain Zinder services set on every `google.rpc.ErrorInfo`.
///
/// Matches the error vocabulary reference; duplicated here so the client does
/// not need to depend on a service crate.
#[cfg(feature = "remote")]
pub(crate) const ZINDER_ERROR_DOMAIN: &str = "zinder.dev";

/// Suggested retry policy attached to every [`IndexerError`].
///
/// Clients consult this to decide whether to retry, surface to the operator,
/// or fail the caller. The policy is derived from the gRPC code and the
/// typed remote error reason, so it is stable across Zinder releases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryPolicy {
    /// Acquire a fresh chain epoch and restart the whole epoch-bound operation.
    /// Retrying the same pinned request cannot succeed.
    RefreshChainEpoch,
    /// Restart the chain-event stream from the earliest retained event and
    /// rebuild derived state from that replay boundary.
    RestartFromEarliestRetained,
    /// Retry with exponential backoff. The remote service or upstream node
    /// is transiently unavailable; the request shape is correct.
    RetryWithBackoff,
    /// An operator must intervene before the request can succeed. Examples:
    /// reorg window exceeded, schema mismatch, broadcast disabled. Clients
    /// must not retry without manual reconfiguration.
    OperatorActionRequired,
    /// The request itself is malformed or out of bounds. Clients fix the
    /// request and re-issue; retrying the same input will fail again.
    ClientError,
}

/// Recovery position for an expired chain-event cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEventCursorRecovery {
    /// Discard the expired cursor and subscribe from the earliest retained event.
    EarliestRetained,
}

/// Error returned by [`crate::ChainIndex`] implementations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexerError {
    /// No visible chain epoch has been committed yet.
    #[error("no visible chain epoch has been committed")]
    NoVisibleChainEpoch,

    /// Requested chain epoch is no longer retained by this serving pair.
    #[error("requested chain epoch pin is unavailable")]
    ChainEpochPinUnavailable,

    /// A chain-event cursor points before the retained event window.
    #[error("chain event cursor expired; restart from the earliest retained event")]
    ChainEventCursorExpired {
        /// Safe non-operator recovery action.
        recovery: ChainEventCursorRecovery,
    },

    /// Requested data is not indexed in the visible chain.
    #[error("{resource} was not found")]
    NotFound {
        /// Resource kind.
        resource: &'static str,
    },

    /// Requested artifact key is valid but absent from the named family.
    ///
    /// This is the canonical per-artifact unavailability signal; future
    /// per-artifact "not present" cases use this variant rather than adding
    /// a new top-level variant. `family` is the on-wire family label the
    /// server emits in `google.rpc.ResourceInfo.resource_type`; its values are
    /// the [`zinder_core::artifact_family`] constants.
    #[error("artifact unavailable in family {family}: key {key}")]
    ArtifactUnavailable {
        /// On-wire family label (see [`zinder_core::artifact_family`]).
        family: String,
        /// Diagnostic representation of the missing key.
        key: String,
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
    #[cfg(feature = "remote")]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "tonic::Status is consumed through map_err adapters at gRPC boundaries"
    )]
    pub(crate) fn from_status(status: tonic::Status) -> Self {
        let message = status.message().to_owned();
        let details = status.get_error_details();
        let Some(zinder_reason) = details.error_info().and_then(|error_info| {
            if error_info.domain == ZINDER_ERROR_DOMAIN {
                ErrorReason::from_str_name(&error_info.reason)
            } else {
                None
            }
        }) else {
            return Self::ServiceUnavailable {
                reason: format!("missing zinder.dev ErrorInfo: {message}"),
            };
        };

        if status.code() == Code::NotFound
            && matches!(zinder_reason, ErrorReason::ArtifactUnavailable)
            && let Some(resource_info) = details.resource_info()
        {
            return Self::ArtifactUnavailable {
                family: resource_info.resource_type.clone(),
                key: resource_info.resource_name.clone(),
            };
        }

        if status.code() == Code::FailedPrecondition
            && matches!(zinder_reason, ErrorReason::ChainEventCursorExpired)
        {
            return Self::ChainEventCursorExpired {
                recovery: ChainEventCursorRecovery::EarliestRetained,
            };
        }

        match status.code() {
            _ if matches!(zinder_reason, ErrorReason::ChainEpochPinUnavailable) => {
                Self::ChainEpochPinUnavailable
            }
            Code::InvalidArgument => Self::InvalidRequest { reason: message },
            Code::FailedPrecondition => Self::FailedPrecondition { reason: message },
            Code::NotFound => Self::NotFound {
                resource: "artifact",
            },
            Code::DataLoss => Self::DataLoss { reason: message },
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
            | Code::Unauthenticated => Self::ServiceUnavailable { reason: message },
        }
    }

    /// Returns the typed [`ErrorReason`] attached to the failure, when known.
    ///
    /// Returns `Some` for errors that originated at a gRPC boundary carrying
    /// a `google.rpc.ErrorInfo` with `domain = "zinder.dev"`. Returns `None`
    /// for client-side validation errors that never crossed a Zinder gRPC
    /// boundary.
    #[must_use]
    #[cfg(feature = "remote")]
    pub fn reason(&self) -> Option<ErrorReason> {
        // Variant-level inference: each variant is most commonly produced by
        // one reason. The full reason is available on the wire via
        // ErrorInfo; this accessor exposes the variant's canonical mapping
        // so consumers can pattern-match without parsing strings.
        match self {
            Self::ChainEpochPinUnavailable => Some(ErrorReason::ChainEpochPinUnavailable),
            Self::ChainEventCursorExpired { .. } => Some(ErrorReason::ChainEventCursorExpired),
            Self::NotFound { .. } => Some(ErrorReason::BlockNotInBestChain),
            Self::ArtifactUnavailable { .. } => Some(ErrorReason::ArtifactUnavailable),
            Self::StorageUnavailable { .. } => Some(ErrorReason::StorageUnavailable),
            Self::ServiceUnavailable { .. } => Some(ErrorReason::NodeUnavailable),
            Self::BlockingTaskFailed { .. } => Some(ErrorReason::BlockingTaskFailed),
            Self::NoVisibleChainEpoch
            | Self::InvalidRequest { .. }
            | Self::FailedPrecondition { .. }
            | Self::DataLoss { .. }
            | Self::MalformedResponse { .. }
            | Self::NetworkMismatch { .. } => None,
        }
    }

    /// Suggested retry policy for this error.
    ///
    /// Returns a stable [`RetryPolicy`] derived from the variant tag.
    /// Consumers map this to local retry, alerting, and operator-action
    /// decisions without parsing message strings.
    #[must_use]
    pub fn retry_policy(&self) -> RetryPolicy {
        match self {
            Self::ChainEpochPinUnavailable => RetryPolicy::RefreshChainEpoch,
            Self::ChainEventCursorExpired { .. } => RetryPolicy::RestartFromEarliestRetained,
            Self::NoVisibleChainEpoch
            | Self::NotFound { .. }
            | Self::ArtifactUnavailable { .. }
            | Self::ServiceUnavailable { .. }
            | Self::StorageUnavailable { .. }
            | Self::BlockingTaskFailed { .. } => RetryPolicy::RetryWithBackoff,
            Self::FailedPrecondition { .. }
            | Self::DataLoss { .. }
            | Self::NetworkMismatch { .. } => RetryPolicy::OperatorActionRequired,
            Self::InvalidRequest { .. } | Self::MalformedResponse { .. } => {
                RetryPolicy::ClientError
            }
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) fn malformed(field: &'static str, reason: impl Into<String>) -> Self {
        Self::MalformedResponse {
            field,
            reason: reason.into(),
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: reason.into(),
        }
    }
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use tonic_types::ErrorDetails;

    use super::*;

    #[test]
    fn stale_chain_epoch_pin_round_trips_as_typed_retryable_error() {
        let status = tonic::Status::with_error_details(
            Code::FailedPrecondition,
            "requested chain epoch is not retained",
            ErrorDetails::with_error_info(
                ErrorReason::ChainEpochPinUnavailable.as_str_name(),
                ZINDER_ERROR_DOMAIN,
                [],
            ),
        );

        let error = IndexerError::from_status(status);

        assert!(matches!(error, IndexerError::ChainEpochPinUnavailable));
        assert_eq!(error.reason(), Some(ErrorReason::ChainEpochPinUnavailable));
        assert_eq!(error.retry_policy(), RetryPolicy::RefreshChainEpoch);
    }

    #[test]
    fn expired_chain_event_cursor_round_trips_with_replay_recovery() {
        let status = zinder_query::status_from_query_error(
            &zinder_query::QueryError::ChainEventCursorExpired {
                event_sequence: 4,
                oldest_retained_sequence: 9,
            },
        );

        let error = IndexerError::from_status(status);

        assert!(matches!(
            error,
            IndexerError::ChainEventCursorExpired {
                recovery: ChainEventCursorRecovery::EarliestRetained,
            }
        ));
        assert_eq!(error.reason(), Some(ErrorReason::ChainEventCursorExpired));
        assert_eq!(
            error.retry_policy(),
            RetryPolicy::RestartFromEarliestRetained
        );
    }
}
