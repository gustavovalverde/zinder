//! Public client error vocabulary.

use thiserror::Error;
use tonic::Code;
use tonic_types::StatusExt;
use zinder_core::{Network, artifact_family};
use zinder_derive::DeriveStoreError;
use zinder_proto::v1::ops::ErrorReason;
use zinder_store::{ArtifactFamily, StoreError};

/// Domain Zinder services set on every `google.rpc.ErrorInfo`.
///
/// Matches the error vocabulary reference; duplicated here so the client does
/// not need to depend on a service crate.
pub(crate) const ZINDER_ERROR_DOMAIN: &str = "zinder.dev";

/// Suggested retry policy attached to every [`IndexerError`].
///
/// Clients consult this to decide whether to retry, surface to the operator,
/// or fail the caller. The policy is derived from the gRPC code and the
/// typed [`ErrorReason`], so it is stable across Zinder releases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryPolicy {
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

    /// Requested artifact key is valid but absent from the named family.
    ///
    /// This is the canonical per-artifact unavailability signal; future
    /// per-artifact "not present" cases use this variant rather than adding
    /// a new top-level variant. Constants for `family` live in
    /// [`zinder_core::artifact_family`].
    ///
    #[error("artifact unavailable in family {family}: key {key}")]
    ArtifactUnavailable {
        /// Canonical family label (see [`zinder_core::artifact_family`]).
        family: &'static str,
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
            StoreError::ArtifactMissing { family, key } => Self::ArtifactUnavailable {
                family: artifact_family_label(family),
                key: format!("{key:?}"),
            },
            StoreError::ChainEpochMissing { .. } => Self::NotFound {
                resource: "artifact",
            },
            StoreError::ChainEventCursorInvalid { reason }
            | StoreError::MempoolEventCursorInvalid { reason }
            | StoreError::InvalidChainEpochArtifacts { reason }
            | StoreError::InvalidChainStoreOptions { reason }
            | StoreError::ArtifactCorrupt { reason, .. }
            | StoreError::Unsupported { feature: reason } => Self::InvalidRequest {
                reason: reason.to_owned(),
            },
            StoreError::ChainEventCursorExpired {
                event_sequence,
                oldest_retained_sequence,
            } => Self::FailedPrecondition {
                reason: format!(
                    "chain event cursor {event_sequence} is before oldest retained event {oldest_retained_sequence}"
                ),
            },
            StoreError::MempoolEventCursorExpired {
                event_sequence,
                oldest_retained_sequence,
            } => Self::FailedPrecondition {
                reason: format!(
                    "mempool event cursor {event_sequence} is before oldest retained event {oldest_retained_sequence}"
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
        clippy::wildcard_enum_match_arm,
        reason = "DeriveStoreError is consumed through map_err adapters at storage boundaries; unknown future variants stay storage-unavailable for clients."
    )]
    pub(crate) fn from_derive_store_error(error: DeriveStoreError) -> Self {
        match &error {
            DeriveStoreError::Decode { reason, .. } if reason.contains("cursor") => {
                Self::InvalidRequest {
                    reason: reason.clone(),
                }
            }
            DeriveStoreError::Decode { reason, .. } => Self::DataLoss {
                reason: reason.clone(),
            },
            DeriveStoreError::InvalidOptions { reason } => Self::InvalidRequest {
                reason: (*reason).to_owned(),
            },
            _ => Self::StorageUnavailable {
                reason: error.to_string(),
            },
        }
    }

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
                family: artifact_family_for_label(&resource_info.resource_type),
                key: resource_info.resource_name.clone(),
            };
        }

        match status.code() {
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
    pub fn reason(&self) -> Option<ErrorReason> {
        // Variant-level inference: each variant is most commonly produced by
        // one reason. The full reason is available on the wire via
        // ErrorInfo; this accessor exposes the variant's canonical mapping
        // so consumers can pattern-match without parsing strings.
        match self {
            Self::NotFound { .. } => Some(ErrorReason::BlockNotInBestChain),
            Self::ArtifactUnavailable { .. } => Some(ErrorReason::ArtifactUnavailable),
            Self::StorageUnavailable { .. } => Some(ErrorReason::StorageUnavailable),
            Self::ServiceUnavailable { .. } => Some(ErrorReason::NodeUnavailable),
            Self::BlockingTaskFailed { .. } => Some(ErrorReason::BlockingTaskFailed),
            Self::NoVisibleChainEpoch
            | Self::InvalidRequest { .. }
            | Self::FailedPrecondition { .. }
            | Self::DataLoss { .. }
            | Self::RemoteEndpointUnconfigured { .. }
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
            Self::NoVisibleChainEpoch
            | Self::NotFound { .. }
            | Self::ArtifactUnavailable { .. }
            | Self::ServiceUnavailable { .. }
            | Self::StorageUnavailable { .. }
            | Self::BlockingTaskFailed { .. } => RetryPolicy::RetryWithBackoff,
            Self::FailedPrecondition { .. }
            | Self::DataLoss { .. }
            | Self::NetworkMismatch { .. } => RetryPolicy::OperatorActionRequired,
            Self::InvalidRequest { .. }
            | Self::RemoteEndpointUnconfigured { .. }
            | Self::MalformedResponse { .. } => RetryPolicy::ClientError,
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

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "ArtifactFamily is #[non_exhaustive]; the wildcard handles future variants without a compile break, and unknown families surface as the literal \"unknown_artifact\" so consumers can detect drift."
)]
fn artifact_family_label(family: ArtifactFamily) -> &'static str {
    match family {
        ArtifactFamily::ChainEpoch => artifact_family::CHAIN_EPOCH,
        ArtifactFamily::ChainEvent => artifact_family::CHAIN_EVENT,
        ArtifactFamily::BlockHeader => artifact_family::BLOCK_HEADER_ARTIFACT,
        ArtifactFamily::BlockBlob => artifact_family::BLOCK_BLOB,
        ArtifactFamily::CompactBlock => artifact_family::COMPACT_BLOCK,
        ArtifactFamily::BlockTransactionIndex => artifact_family::BLOCK_TRANSACTION_INDEX,
        ArtifactFamily::TransactionLocation => artifact_family::TRANSACTION_LOCATION,
        ArtifactFamily::TransactionFacts => artifact_family::TRANSACTION_FACTS,
        ArtifactFamily::TransactionBlob => artifact_family::TRANSACTION_BLOB,
        ArtifactFamily::TreeState => artifact_family::TREE_STATE,
        ArtifactFamily::SubtreeRoot => artifact_family::SUBTREE_ROOT,
        ArtifactFamily::TransparentOutput => artifact_family::TRANSPARENT_OUTPUT,
        ArtifactFamily::AddressOutputIndex => artifact_family::ADDRESS_OUTPUT_INDEX,
        ArtifactFamily::TransparentSpendFact => artifact_family::TRANSPARENT_SPEND_FACT,
        ArtifactFamily::TransparentAddressTxIndex => artifact_family::TRANSPARENT_ADDRESS_TX_INDEX,
        ArtifactFamily::BlockHashIndex => artifact_family::BLOCK_HASH_INDEX,
        ArtifactFamily::MempoolEvent => artifact_family::MEMPOOL_EVENT,
        _ => "unknown_artifact",
    }
}

/// Inverse of [`artifact_family_label`].
///
/// Maps a `ResourceInfo.resource_type` string back to the canonical static
/// `family` label exposed by [`IndexerError::ArtifactUnavailable`]. The
/// server emits the family via `format!("{family:?}")`, so the matching is
/// on the Rust `Debug` form.
fn artifact_family_for_label(resource_type: &str) -> &'static str {
    match resource_type {
        "ChainEpoch" => artifact_family::CHAIN_EPOCH,
        "ChainEvent" => artifact_family::CHAIN_EVENT,
        "BlockHeader" => artifact_family::BLOCK_HEADER_ARTIFACT,
        "BlockBlob" => artifact_family::BLOCK_BLOB,
        "CompactBlock" => artifact_family::COMPACT_BLOCK,
        "BlockTransactionIndex" => artifact_family::BLOCK_TRANSACTION_INDEX,
        "TransactionLocation" => artifact_family::TRANSACTION_LOCATION,
        "TransactionFacts" => artifact_family::TRANSACTION_FACTS,
        "TransactionBlob" => artifact_family::TRANSACTION_BLOB,
        "TreeState" => artifact_family::TREE_STATE,
        "SubtreeRoot" => artifact_family::SUBTREE_ROOT,
        "TransparentOutput" => artifact_family::TRANSPARENT_OUTPUT,
        "AddressOutputIndex" => artifact_family::ADDRESS_OUTPUT_INDEX,
        "TransparentSpendFact" => artifact_family::TRANSPARENT_SPEND_FACT,
        "TransparentAddressTxIndex" => artifact_family::TRANSPARENT_ADDRESS_TX_INDEX,
        "BlockHashIndex" => artifact_family::BLOCK_HASH_INDEX,
        "MempoolEvent" => artifact_family::MEMPOOL_EVENT,
        _ => "unknown_artifact",
    }
}
