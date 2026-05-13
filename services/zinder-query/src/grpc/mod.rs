//! Native gRPC adapter and protobuf encoders for the wallet query plane.

mod adapter;
mod chain_events;
mod native;

use std::collections::HashMap;

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, FieldViolation, PreconditionViolation, StatusExt};
use zinder_proto::capabilities::WALLET_BROADCAST_TRANSACTION_V1;
use zinder_proto::v1::ops::ErrorReason;
use zinder_store::status_from_store_error;

use crate::QueryError;

/// Domain attached to every `google.rpc.ErrorInfo` returned by a Zinder
/// service. The Rust client matches on this domain to know an [`ErrorInfo`]
/// originated from Zinder before trusting its `reason` field.
pub(crate) const ZINDER_ERROR_DOMAIN: &str = "zinder.dev";

pub use adapter::WalletQueryGrpcAdapter;
pub use native::{
    MAX_TRANSPARENT_ADDRESSES_PER_BALANCE_REQUEST, ServerInfoSettings, UpstreamNodeCapabilities,
    address_lookup_to_script_hash, block_header_by_selector_response,
    block_id_by_selector_response, broadcast_transaction_response,
    build_transparent_address_tx_ids_chunk, build_transparent_address_utxos_stream_chunk,
    build_wallet_server_info, chain_events_response, compact_block_response, latest_block_response,
    latest_tree_state_response, subtree_roots_response, transaction_response,
    transparent_address_confirmed_balance_response, transparent_address_tx_ids_response,
    transparent_address_utxos_response, transparent_prevouts_response, tree_state_response,
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

    let (code, mut details) = match error {
        QueryError::InvalidBlockRange { .. }
        | QueryError::CompactBlockRangeTooLarge { .. }
        | QueryError::ChainEventCursorInvalid { .. }
        | QueryError::TransparentUtxoCursorInvalid { .. }
        | QueryError::TransparentHistoryCursorInvalid { .. }
        | QueryError::InvalidAddress { .. }
        | QueryError::UnsupportedShieldedProtocol { .. } => {
            (Code::InvalidArgument, bad_request_details(error))
        }
        QueryError::TransactionBroadcastDisabled
        | QueryError::ChainEventCursorExpired { .. }
        | QueryError::ChainEpochPinUnsupported
        | QueryError::ChainEpochPinUnavailable { .. }
        | QueryError::ChainEpochPinMismatch { .. } => (
            Code::FailedPrecondition,
            precondition_failure_details(error),
        ),
        QueryError::ArtifactUnavailable { family, key } => (
            Code::NotFound,
            ErrorDetails::with_resource_info(
                format!("{family:?}"),
                key.to_string(),
                "zinder-query",
                "artifact is not available in the selected chain epoch",
            ),
        ),
        QueryError::CompactBlockPayloadMalformed { .. } | QueryError::ArtifactCorrupt { .. } => {
            (Code::DataLoss, ErrorDetails::new())
        }
        QueryError::BlockNotInBestChain => (Code::NotFound, ErrorDetails::new()),
        QueryError::UnsupportedChainEvent { .. }
        | QueryError::UnsupportedBlockSelector { .. }
        | QueryError::UnsupportedTransactionStatus { .. }
        | QueryError::BlockingTaskFailed { .. }
        | QueryError::Node(_) => (Code::Unavailable, ErrorDetails::new()),
        QueryError::Store(error) => return status_from_store_error(error),
    };

    let reason = error_reason_for_query_error(error);
    details.set_error_info(reason.as_str_name(), ZINDER_ERROR_DOMAIN, HashMap::new());
    Status::with_error_details(code, message, details)
}

/// Maps each [`QueryError`] variant to its stable [`ErrorReason`].
///
/// The reason code is the typed key clients pin to. Pair with the gRPC
/// `Status::code()` for the retry semantics and with the existing structured
/// detail types (`BadRequest`, `PreconditionFailure`, `ResourceInfo`) for the
/// failure-shape detail.
fn error_reason_for_query_error(error: &QueryError) -> ErrorReason {
    match error {
        QueryError::InvalidBlockRange { .. } => ErrorReason::InvalidBlockRange,
        QueryError::CompactBlockRangeTooLarge { .. } => ErrorReason::CompactBlockRangeTooLarge,
        QueryError::ChainEventCursorInvalid { .. } => ErrorReason::ChainEventCursorInvalid,
        QueryError::TransparentUtxoCursorInvalid { .. } => {
            ErrorReason::TransparentUtxoCursorInvalid
        }
        QueryError::TransparentHistoryCursorInvalid { .. } => {
            ErrorReason::TransparentHistoryCursorInvalid
        }
        QueryError::InvalidAddress { .. } => ErrorReason::InvalidAddress,
        QueryError::UnsupportedShieldedProtocol { .. } => ErrorReason::UnsupportedShieldedProtocol,
        QueryError::TransactionBroadcastDisabled => ErrorReason::BroadcastDisabled,
        QueryError::ChainEventCursorExpired { .. } => ErrorReason::ChainEventCursorExpired,
        QueryError::ChainEpochPinUnsupported => ErrorReason::ChainEpochPinUnsupported,
        QueryError::ChainEpochPinUnavailable { .. } => ErrorReason::ChainEpochPinUnavailable,
        QueryError::ChainEpochPinMismatch { .. } => ErrorReason::ChainEpochPinMismatch,
        QueryError::ArtifactUnavailable { .. } => ErrorReason::ArtifactUnavailable,
        QueryError::CompactBlockPayloadMalformed { .. } => {
            ErrorReason::CompactBlockPayloadMalformed
        }
        QueryError::ArtifactCorrupt { .. } => ErrorReason::ArtifactCorrupt,
        QueryError::BlockNotInBestChain => ErrorReason::BlockNotInBestChain,
        QueryError::UnsupportedChainEvent { .. } => ErrorReason::UnsupportedChainEvent,
        QueryError::UnsupportedBlockSelector { .. } => ErrorReason::UnsupportedBlockSelector,
        QueryError::UnsupportedTransactionStatus { .. } => {
            ErrorReason::UnsupportedTransactionStatus
        }
        QueryError::BlockingTaskFailed { .. } => ErrorReason::BlockingTaskFailed,
        QueryError::Node(_) => ErrorReason::NodeUnavailable,
        QueryError::Store(_) => ErrorReason::Unspecified,
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
        QueryError::ChainEventCursorInvalid { reason }
        | QueryError::TransparentUtxoCursorInvalid { reason }
        | QueryError::TransparentHistoryCursorInvalid { reason } => {
            ErrorDetails::with_bad_request_violation("from_cursor", *reason)
        }
        QueryError::InvalidAddress { reason } => {
            ErrorDetails::with_bad_request_violation("address", *reason)
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
                WALLET_BROADCAST_TRANSACTION_V1,
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
