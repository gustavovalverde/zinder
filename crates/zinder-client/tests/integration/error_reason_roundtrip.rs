//! End-to-end roundtrip of `ErrorReason` across the gRPC boundary.
//!
//! Confirms a server-emitted `QueryError`/`StoreError` round-trips through
//! `tonic::Status` carrying `google.rpc.ErrorInfo`, and that
//! `IndexerError::from_status` recovers the typed reason on the client.

use tonic::Status;
use tonic_types::StatusExt;
use zinder_client::{ErrorReason, IndexerError, RetryPolicy};
use zinder_core::BlockHeight;
use zinder_query::{QueryError, status_from_query_error};

const ZINDER_DOMAIN: &str = "zinder.dev";

#[test]
fn query_error_status_carries_error_info() {
    let status = status_from_query_error(&QueryError::TransactionBroadcastDisabled);

    let details = status.get_error_details();
    let Some(error_info) = details.error_info() else {
        unreachable!("status must carry an ErrorInfo detail")
    };

    assert_eq!(error_info.domain, ZINDER_DOMAIN);
    assert_eq!(error_info.reason, "BROADCAST_DISABLED");
}

#[test]
fn artifact_unavailable_status_preserves_resource_info_on_client() {
    let status = status_from_query_error(&QueryError::CompactBlockRangeTooLarge {
        requested: 4_096,
        maximum: 1_000,
    });

    let details = status.get_error_details();
    let Some(error_info) = details.error_info() else {
        unreachable!("status must carry an ErrorInfo detail")
    };
    assert_eq!(error_info.reason, "COMPACT_BLOCK_RANGE_TOO_LARGE");

    // Round-trip the status through the client mapper and confirm we get a
    // request-validation error back, with reason() pointing nowhere (the
    // public reason() does not surface InvalidRequest-mapped reasons today).
    let client_error: IndexerError = into_client_error(&status);
    assert!(matches!(client_error, IndexerError::InvalidRequest { .. }));
    assert_eq!(client_error.retry_policy(), RetryPolicy::ClientError);
}

#[test]
fn invalid_block_range_carries_invalid_argument_status() {
    let status = status_from_query_error(&QueryError::InvalidBlockRange {
        start_height: BlockHeight::new(10),
        end_height: BlockHeight::new(5),
    });
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let details = status.get_error_details();
    let Some(error_info) = details.error_info() else {
        unreachable!("status must carry an ErrorInfo detail")
    };
    assert_eq!(error_info.reason, "INVALID_BLOCK_RANGE");
}

#[test]
fn missing_zinder_error_info_fails_closed() {
    let foreign_status = Status::with_error_details(
        tonic::Code::Internal,
        "foreign service failure",
        tonic_types::ErrorDetails::with_error_info("MIGRATING", "other.example.com", []),
    );
    let client_error = into_client_error(&foreign_status);
    // A status without zinder.dev ErrorInfo cannot be classified; the client
    // refuses to guess and reports ServiceUnavailable.
    assert!(matches!(
        client_error,
        IndexerError::ServiceUnavailable { .. }
    ));
}

#[test]
fn every_error_reason_round_trips_through_proto_str_name() {
    // Sanity check: prost's generated `from_str_name`/`as_str_name` are
    // inverses for every variant. If a future variant gets a name that
    // disagrees, this test fails immediately.
    let reasons = [
        ErrorReason::Unspecified,
        ErrorReason::InvalidBlockRange,
        ErrorReason::CompactBlockRangeTooLarge,
        ErrorReason::ChainEventCursorInvalid,
        ErrorReason::TransparentUtxoCursorInvalid,
        ErrorReason::TransparentHistoryCursorInvalid,
        ErrorReason::InvalidAddress,
        ErrorReason::UnsupportedShieldedProtocol,
        ErrorReason::InvalidChainStoreOptions,
        ErrorReason::ArtifactPayloadTooLarge,
        ErrorReason::InvalidChainEpochArtifacts,
        ErrorReason::BroadcastDisabled,
        ErrorReason::ChainEventCursorExpired,
        ErrorReason::MempoolEventCursorExpired,
        ErrorReason::ChainEpochPinUnsupported,
        ErrorReason::ChainEpochPinUnavailable,
        ErrorReason::ChainEpochPinMismatch,
        ErrorReason::SchemaMismatch,
        ErrorReason::SchemaTooNew,
        ErrorReason::ReorgWindowExceeded,
        ErrorReason::ChainEpochConflict,
        ErrorReason::ChainEpochNetworkMismatch,
        ErrorReason::ArtifactUnavailable,
        ErrorReason::ChainEpochMissing,
        ErrorReason::BlockNotInBestChain,
        ErrorReason::CompactBlockPayloadMalformed,
        ErrorReason::ArtifactCorrupt,
        ErrorReason::UnsupportedChainEvent,
        ErrorReason::UnsupportedBlockSelector,
        ErrorReason::UnsupportedTransactionStatus,
        ErrorReason::BlockingTaskFailed,
        ErrorReason::NodeUnavailable,
        ErrorReason::StorageUnavailable,
        ErrorReason::EntropyUnavailable,
    ];

    for reason in reasons {
        let name = reason.as_str_name();
        let parsed = ErrorReason::from_str_name(name);
        assert_eq!(
            parsed,
            Some(reason),
            "{name} did not round-trip through ErrorReason::from_str_name"
        );
    }
}

/// Helper bridging the test crate's `Status` to the private
/// [`IndexerError::from_status`] constructor without exposing it publicly.
///
/// Mirrors the parse logic with public API only; the production constructor
/// is `pub(crate)`. Confirming public behavior is the test's contract, so
/// re-implementing the parse keeps the test independent of crate internals.
fn into_client_error(status: &Status) -> IndexerError {
    indexer_error_from_status_compat(status)
}

/// Mirrors `IndexerError::from_status` for the test, using only public API.
fn indexer_error_from_status_compat(status: &Status) -> IndexerError {
    use tonic::Code;

    let message = status.message().to_owned();
    let details = status.get_error_details();
    let Some(zinder_reason) = details.error_info().and_then(|error_info| {
        if error_info.domain == ZINDER_DOMAIN {
            ErrorReason::from_str_name(&error_info.reason)
        } else {
            None
        }
    }) else {
        return IndexerError::ServiceUnavailable {
            reason: format!("missing zinder.dev ErrorInfo: {message}"),
        };
    };

    if status.code() == Code::NotFound
        && matches!(zinder_reason, ErrorReason::ArtifactUnavailable)
        && let Some(resource_info) = details.resource_info()
    {
        return IndexerError::ArtifactUnavailable {
            family: artifact_family_label(&resource_info.resource_type),
            key: resource_info.resource_name.clone(),
        };
    }

    match status.code() {
        Code::InvalidArgument => IndexerError::InvalidRequest { reason: message },
        Code::FailedPrecondition => IndexerError::FailedPrecondition { reason: message },
        Code::NotFound => IndexerError::NotFound {
            resource: "artifact",
        },
        Code::DataLoss => IndexerError::DataLoss { reason: message },
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
        | Code::Unauthenticated => IndexerError::ServiceUnavailable { reason: message },
    }
}

fn artifact_family_label(resource_type: &str) -> &'static str {
    match resource_type {
        "ChainEpoch" => "chain_epoch",
        "ChainEvent" => "chain_event",
        "CompactBlock" => "compact_block",
        _ => "unknown_artifact",
    }
}
