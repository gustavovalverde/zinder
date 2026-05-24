//! gRPC status mapping for storage-boundary errors.

use std::collections::HashMap;

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, FieldViolation, PreconditionViolation, StatusExt};
use zinder_proto::v1::ops::ErrorReason;

use crate::StoreError;

/// Domain attached to every `google.rpc.ErrorInfo` returned by a Zinder
/// service.
///
/// Duplicated here so `zinder-store` does not need to import the query crate;
/// the value is fixed by the error vocabulary reference.
const ZINDER_ERROR_DOMAIN: &str = "zinder.dev";

/// Maps a [`StoreError`] to the canonical gRPC status used by all services.
#[must_use]
pub fn status_from_store_error(error: &StoreError) -> Status {
    let message = error.to_string();
    let (code, mut details) = code_and_typed_detail_for(error);
    let reason = error_reason_for_store_error(error);
    details.set_error_info(reason.as_str_name(), ZINDER_ERROR_DOMAIN, HashMap::new());
    Status::with_error_details(code, message, details)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "StoreError is non-exhaustive; future storage failures fail closed as unavailable until explicitly classified."
)]
fn code_and_typed_detail_for(error: &StoreError) -> (Code, ErrorDetails) {
    match error {
        StoreError::ChainEventCursorInvalid { reason }
        | StoreError::MempoolEventCursorInvalid { reason }
        | StoreError::AddressOutputCursorInvalid { reason }
        | StoreError::TransparentHistoryCursorInvalid { reason } => (
            Code::InvalidArgument,
            ErrorDetails::with_bad_request(vec![FieldViolation::new("from_cursor", *reason)]),
        ),
        StoreError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => (
            Code::FailedPrecondition,
            ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
                "CHAIN_EVENT_CURSOR_EXPIRED",
                format!("chain_event:{event_sequence}"),
                format!("oldest retained chain event sequence is {oldest_retained_sequence}"),
            )]),
        ),
        StoreError::MempoolEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => (
            Code::FailedPrecondition,
            ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
                "MEMPOOL_EVENT_CURSOR_EXPIRED",
                format!("mempool_event:{event_sequence}"),
                format!("oldest retained mempool event sequence is {oldest_retained_sequence}"),
            )]),
        ),
        StoreError::SchemaMismatch { .. }
        | StoreError::SchemaTooNew { .. }
        | StoreError::ReorgWindowExceeded { .. }
        | StoreError::ChainEpochConflict { .. }
        | StoreError::ChainEpochNetworkMismatch { .. } => {
            (Code::FailedPrecondition, ErrorDetails::new())
        }
        StoreError::ArtifactMissing { family, key } => (
            Code::NotFound,
            ErrorDetails::with_resource_info(
                format!("{family:?}"),
                format!("{key:?}"),
                "zinder-store",
                "artifact is not available in the selected chain epoch",
            ),
        ),
        StoreError::ChainEpochMissing { chain_epoch } => (
            Code::NotFound,
            ErrorDetails::with_resource_info(
                "ChainEpoch",
                format!("chain_epoch:{}", chain_epoch.value()),
                "zinder-store",
                "chain epoch is not retained",
            ),
        ),
        StoreError::EntropyUnavailable { .. } => (Code::Internal, ErrorDetails::new()),
        StoreError::ArtifactCorrupt { .. } => (Code::DataLoss, ErrorDetails::new()),
        StoreError::InvalidChainEpochArtifacts { .. }
        | StoreError::ArtifactPayloadTooLarge { .. }
        | StoreError::InvalidChainStoreOptions { .. } => {
            (Code::InvalidArgument, ErrorDetails::new())
        }
        _ => (Code::Unavailable, ErrorDetails::new()),
    }
}

/// Maps each [`StoreError`] variant to its stable [`ErrorReason`].
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Unclassified storage failures fall back to STORAGE_UNAVAILABLE alongside the wildcard Code::Unavailable branch."
)]
fn error_reason_for_store_error(error: &StoreError) -> ErrorReason {
    match error {
        StoreError::ChainEventCursorInvalid { .. } => ErrorReason::ChainEventCursorInvalid,
        StoreError::MempoolEventCursorInvalid { .. } => {
            // Mempool cursor reuses the chain-event-cursor reason because
            // the wire shape carries the cursor failure category, not the
            // stream family; the family is encoded in the cursor itself.
            ErrorReason::ChainEventCursorInvalid
        }
        StoreError::AddressOutputCursorInvalid { .. } => ErrorReason::AddressOutputCursorInvalid,
        StoreError::TransparentHistoryCursorInvalid { .. } => {
            ErrorReason::TransparentHistoryCursorInvalid
        }
        StoreError::ChainEventCursorExpired { .. } => ErrorReason::ChainEventCursorExpired,
        StoreError::MempoolEventCursorExpired { .. } => ErrorReason::MempoolEventCursorExpired,
        StoreError::SchemaMismatch { .. } => ErrorReason::SchemaMismatch,
        StoreError::SchemaTooNew { .. } => ErrorReason::SchemaTooNew,
        StoreError::ReorgWindowExceeded { .. } => ErrorReason::ReorgWindowExceeded,
        StoreError::ChainEpochConflict { .. } => ErrorReason::ChainEpochConflict,
        StoreError::ChainEpochNetworkMismatch { .. } => ErrorReason::ChainEpochNetworkMismatch,
        StoreError::ArtifactMissing { .. } => ErrorReason::ArtifactUnavailable,
        StoreError::ChainEpochMissing { .. } => ErrorReason::ChainEpochMissing,
        StoreError::EntropyUnavailable { .. } => ErrorReason::EntropyUnavailable,
        StoreError::ArtifactCorrupt { .. } => ErrorReason::ArtifactCorrupt,
        StoreError::InvalidChainEpochArtifacts { .. } => ErrorReason::InvalidChainEpochArtifacts,
        StoreError::ArtifactPayloadTooLarge { .. } => ErrorReason::ArtifactPayloadTooLarge,
        StoreError::InvalidChainStoreOptions { .. } => ErrorReason::InvalidChainStoreOptions,
        _ => ErrorReason::StorageUnavailable,
    }
}
