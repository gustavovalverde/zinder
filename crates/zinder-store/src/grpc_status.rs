//! gRPC status mapping for storage-boundary errors.

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, FieldViolation, PreconditionViolation, StatusExt};

use crate::StoreError;

/// Maps a [`StoreError`] to the canonical gRPC status used by all services.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "StoreError is non-exhaustive; future storage failures fail closed as unavailable until explicitly classified."
)]
pub fn status_from_store_error(error: &StoreError) -> Status {
    let message = error.to_string();
    match error {
        StoreError::EventCursorInvalid { reason } => Status::with_error_details(
            Code::InvalidArgument,
            message,
            ErrorDetails::with_bad_request(vec![FieldViolation::new("from_cursor", *reason)]),
        ),
        StoreError::EventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => Status::with_error_details(
            Code::FailedPrecondition,
            message,
            ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
                "CHAIN_EVENT_CURSOR_EXPIRED",
                format!("chain_event:{event_sequence}"),
                format!("oldest retained chain event sequence is {oldest_retained_sequence}"),
            )]),
        ),
        StoreError::SchemaMismatch { .. } | StoreError::SchemaTooNew { .. } => {
            Status::failed_precondition(message)
        }
        StoreError::ReorgWindowExceeded { .. }
        | StoreError::ChainEpochConflict { .. }
        | StoreError::ChainEpochNetworkMismatch { .. } => Status::failed_precondition(message),
        StoreError::ArtifactMissing { family, key } => Status::with_error_details(
            Code::NotFound,
            message,
            ErrorDetails::with_resource_info(
                format!("{family:?}"),
                format!("{key:?}"),
                "zinder-store",
                "artifact is not available in the selected chain epoch",
            ),
        ),
        StoreError::ChainEpochMissing { chain_epoch } => Status::with_error_details(
            Code::NotFound,
            message,
            ErrorDetails::with_resource_info(
                "ChainEpoch",
                format!("chain_epoch:{}", chain_epoch.value()),
                "zinder-store",
                "chain epoch is not retained",
            ),
        ),
        StoreError::EntropyUnavailable { .. } => Status::internal(message),
        StoreError::ArtifactCorrupt { .. } => Status::data_loss(message),
        StoreError::InvalidChainEpochArtifacts { .. }
        | StoreError::ArtifactPayloadTooLarge { .. }
        | StoreError::InvalidChainStoreOptions { .. } => Status::invalid_argument(message),
        _ => Status::unavailable(message),
    }
}
