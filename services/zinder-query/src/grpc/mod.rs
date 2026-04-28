//! Native gRPC adapter and protobuf encoders for the wallet query plane.

mod adapter;
mod native;

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, FieldViolation, PreconditionViolation, StatusExt};
use zinder_store::status_from_store_error;

use crate::QueryError;

pub use adapter::WalletQueryGrpcAdapter;
pub use native::{
    ServerInfoSettings, broadcast_transaction_response, build_server_capabilities_message,
    chain_events_response, compact_block_response, latest_block_response,
    latest_tree_state_response, subtree_roots_response, transaction_response, tree_state_response,
};

/// Maps a [`QueryError`] to a tonic [`Status`] using the canonical mapping
/// from [`Public Interfaces §Error Conventions`](../../../docs/architecture/public-interfaces.md#error-conventions).
///
/// This is the single source of truth for `QueryError` to gRPC translation.
/// Both [`WalletQueryGrpcAdapter`] (native surface) and the lightwalletd
/// compatibility adapter call into this function instead of duplicating the
/// mapping. Adding a new `QueryError` variant requires extending this match
/// arm exactly once.
#[must_use]
pub fn status_from_query_error(error: &QueryError) -> Status {
    let message = error.to_string();

    match error {
        QueryError::InvalidBlockRange { .. }
        | QueryError::CompactBlockRangeTooLarge { .. }
        | QueryError::ChainEventCursorInvalid { .. }
        | QueryError::UnsupportedShieldedProtocol { .. } => {
            Status::with_error_details(Code::InvalidArgument, message, bad_request_details(error))
        }
        QueryError::TransactionBroadcastDisabled
        | QueryError::ChainEventCursorExpired { .. }
        | QueryError::ChainEpochPinUnsupported
        | QueryError::ChainEpochPinUnavailable { .. }
        | QueryError::ChainEpochPinMismatch { .. } => Status::with_error_details(
            Code::FailedPrecondition,
            message,
            precondition_failure_details(error),
        ),
        QueryError::ArtifactUnavailable { family, key } => Status::with_error_details(
            Code::NotFound,
            message,
            ErrorDetails::with_resource_info(
                format!("{family:?}"),
                key.to_string(),
                "zinder-query",
                "artifact is not available in the selected chain epoch",
            ),
        ),
        QueryError::CompactBlockPayloadMalformed { .. } => Status::data_loss(message),
        QueryError::UnsupportedChainEvent { .. }
        | QueryError::BlockingTaskFailed { .. }
        | QueryError::Node(_) => Status::unavailable(message),
        QueryError::Store(error) => status_from_store_error(error),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only invalid-request variants carry BadRequest details; all other query errors intentionally return empty detail sets here."
)]
fn bad_request_details(error: &QueryError) -> ErrorDetails {
    match error {
        QueryError::InvalidBlockRange {
            start_height,
            end_height,
        } => ErrorDetails::with_bad_request(vec![
            FieldViolation::new(
                "start_height",
                format!(
                    "start height {} exceeds end height {}",
                    start_height.value(),
                    end_height.value()
                ),
            ),
            FieldViolation::new(
                "end_height",
                format!(
                    "end height {} is below start height {}",
                    end_height.value(),
                    start_height.value()
                ),
            ),
        ]),
        QueryError::CompactBlockRangeTooLarge { requested, maximum } => {
            ErrorDetails::with_bad_request_violation(
                "end_height",
                format!("requested {requested} compact blocks; maximum is {maximum}"),
            )
        }
        QueryError::ChainEventCursorInvalid { reason } => {
            ErrorDetails::with_bad_request_violation("from_cursor", *reason)
        }
        QueryError::UnsupportedShieldedProtocol { protocol } => {
            ErrorDetails::with_bad_request_violation(
                "shielded_protocol",
                format!("{protocol:?} is not supported by the native wallet protocol"),
            )
        }
        _ => ErrorDetails::new(),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only failed-precondition variants carry PreconditionFailure details; all other query errors intentionally return empty detail sets here."
)]
fn precondition_failure_details(error: &QueryError) -> ErrorDetails {
    match error {
        QueryError::TransactionBroadcastDisabled => {
            ErrorDetails::with_precondition_failure_violation(
                "TRANSACTION_BROADCAST_DISABLED",
                "wallet.broadcast.transaction_v1",
                "transaction broadcast is not configured for this deployment",
            )
        }
        QueryError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
            "CHAIN_EVENT_CURSOR_EXPIRED",
            format!("chain_event:{event_sequence}"),
            format!("oldest retained chain event sequence is {oldest_retained_sequence}"),
        )]),
        QueryError::ChainEpochPinUnsupported => ErrorDetails::with_precondition_failure_violation(
            "CHAIN_EPOCH_PIN_UNSUPPORTED",
            "at_epoch",
            "query implementation does not support request-side epoch pinning",
        ),
        QueryError::ChainEpochPinUnavailable { chain_epoch_id } => {
            ErrorDetails::with_precondition_failure_violation(
                "CHAIN_EPOCH_PIN_UNAVAILABLE",
                format!("chain_epoch:{}", chain_epoch_id.value()),
                "requested chain epoch is not retained",
            )
        }
        QueryError::ChainEpochPinMismatch {
            chain_epoch_id,
            reason,
        } => ErrorDetails::with_precondition_failure_violation(
            "CHAIN_EPOCH_PIN_MISMATCH",
            format!("chain_epoch:{}", chain_epoch_id.value()),
            *reason,
        ),
        _ => ErrorDetails::new(),
    }
}

#[cfg(test)]
mod tests {
    use zinder_core::BlockHeight;

    use super::*;

    #[test]
    fn expired_cursor_status_carries_precondition_failure_detail() {
        let status = status_from_query_error(&QueryError::ChainEventCursorExpired {
            event_sequence: 4,
            oldest_retained_sequence: 9,
        });
        let details = status.get_error_details();
        let violation = details
            .precondition_failure()
            .and_then(|failure| failure.violations.first())
            .cloned();

        assert_eq!(status.code(), Code::FailedPrecondition);
        assert!(matches!(
            violation,
            Some(violation)
                if violation.r#type == "CHAIN_EVENT_CURSOR_EXPIRED"
                    && violation.subject == "chain_event:4"
                    && violation.description.contains('9')
        ));
    }

    #[test]
    fn invalid_block_range_status_carries_bad_request_detail() {
        let status = status_from_query_error(&QueryError::InvalidBlockRange {
            start_height: BlockHeight::new(10),
            end_height: BlockHeight::new(5),
        });
        let details = status.get_error_details();
        let fields = details
            .bad_request()
            .map(|bad_request| {
                bad_request
                    .field_violations
                    .iter()
                    .map(|violation| violation.field.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(fields, vec!["start_height", "end_height"]);
    }
}
