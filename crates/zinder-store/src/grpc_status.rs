//! gRPC status mapping for storage-boundary errors.

use tonic::Status;
use tonic_types::{ErrorDetails, FieldViolation, PreconditionViolation};
use zinder_proto::v1::ops::ErrorReason;
use zinder_proto::{BoundaryError, status_with_reason};

use crate::StoreError;

/// Maps a [`StoreError`] to the canonical gRPC status used by all services.
///
/// The reason comes from [`StoreError::error_reason`] and the code from the
/// shared reason policy; the typed `BadRequest`/`PreconditionFailure`/
/// `ResourceInfo` detail is attached per variant.
#[must_use]
pub fn status_from_store_error(error: &StoreError) -> Status {
    status_with_reason(
        error.error_reason(),
        error.to_string(),
        typed_detail_for(error),
    )
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only variants with structured detail attach it here; every other StoreError carries its reason via ErrorInfo with an empty detail set."
)]
fn typed_detail_for(error: &StoreError) -> ErrorDetails {
    match error {
        StoreError::ChainEventCursorInvalid { reason }
        | StoreError::MempoolEventCursorInvalid { reason }
        | StoreError::AddressOutputCursorInvalid { reason }
        | StoreError::TransparentHistoryCursorInvalid { reason }
        | StoreError::SnapshotPageCursorInvalid { reason } => {
            ErrorDetails::with_bad_request(vec![FieldViolation::new("from_cursor", *reason)])
        }
        StoreError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
            "CHAIN_EVENT_CURSOR_EXPIRED",
            format!("chain_event:{event_sequence}"),
            format!("oldest retained chain event sequence is {oldest_retained_sequence}"),
        )]),
        StoreError::MempoolEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
            "MEMPOOL_EVENT_CURSOR_EXPIRED",
            format!("mempool_event:{event_sequence}"),
            format!("oldest retained mempool event sequence is {oldest_retained_sequence}"),
        )]),
        StoreError::SnapshotPageCursorExpired {
            snapshot_sequence,
            current_snapshot_sequence,
        } => ErrorDetails::with_precondition_failure(vec![PreconditionViolation::new(
            "SNAPSHOT_PAGE_CURSOR_EXPIRED",
            format!("snapshot_page:{snapshot_sequence}"),
            format!("current snapshot sequence is {current_snapshot_sequence}"),
        )]),
        StoreError::ArtifactMissing { family, key } => ErrorDetails::with_resource_info(
            family.wire_label(),
            format!("{key:?}"),
            "zinder-store",
            "artifact is not available in the selected chain epoch",
        ),
        StoreError::ChainEpochMissing { chain_epoch } => ErrorDetails::with_resource_info(
            crate::ArtifactFamily::ChainEpoch.wire_label(),
            format!("chain_epoch:{}", chain_epoch.value()),
            "zinder-store",
            "chain epoch is not retained",
        ),
        _ => ErrorDetails::new(),
    }
}

impl BoundaryError for StoreError {
    /// Maps each [`StoreError`] variant to its stable [`ErrorReason`].
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "Unclassified and overflow storage failures are sustained-outage signals that map to STORAGE_UNAVAILABLE alongside the explicit storage variants."
    )]
    fn error_reason(&self) -> ErrorReason {
        match self {
            // The mempool cursor reuses the chain-event-cursor reason because
            // the wire shape carries the cursor failure category, not the
            // stream family; the family is encoded in the cursor itself.
            Self::ChainEventCursorInvalid { .. } | Self::MempoolEventCursorInvalid { .. } => {
                ErrorReason::ChainEventCursorInvalid
            }
            Self::AddressOutputCursorInvalid { .. } => ErrorReason::AddressOutputCursorInvalid,
            Self::TransparentHistoryCursorInvalid { .. } => {
                ErrorReason::TransparentHistoryCursorInvalid
            }
            Self::ChainEventCursorExpired { .. } => ErrorReason::ChainEventCursorExpired,
            Self::MempoolEventCursorExpired { .. } => ErrorReason::MempoolEventCursorExpired,
            Self::SnapshotPageCursorInvalid { .. } => ErrorReason::SnapshotPageCursorInvalid,
            Self::SnapshotPageCursorExpired { .. } => ErrorReason::SnapshotPageCursorExpired,
            Self::SchemaMismatch { .. } | Self::SchemaTooOld { .. } => ErrorReason::SchemaMismatch,
            Self::SchemaTooNew { .. } => ErrorReason::SchemaTooNew,
            Self::ReorgWindowExceeded { .. } => ErrorReason::ReorgWindowExceeded,
            Self::ChainEpochConflict { .. } => ErrorReason::ChainEpochConflict,
            Self::ChainEpochNetworkMismatch { .. } => ErrorReason::ChainEpochNetworkMismatch,
            Self::ArtifactMissing { .. } => ErrorReason::ArtifactUnavailable,
            Self::ChainEpochMissing { .. } => ErrorReason::ChainEpochMissing,
            Self::NoVisibleChainEpoch => ErrorReason::NoVisibleChainEpoch,
            Self::EntropyUnavailable { .. } => ErrorReason::EntropyUnavailable,
            Self::ArtifactCorrupt { .. } => ErrorReason::ArtifactCorrupt,
            Self::InvalidChainEpochArtifacts { .. } => ErrorReason::InvalidChainEpochArtifacts,
            Self::ArtifactPayloadTooLarge { .. } => ErrorReason::ArtifactPayloadTooLarge,
            Self::InvalidChainStoreOptions { .. } => ErrorReason::InvalidChainStoreOptions,
            _ => ErrorReason::StorageUnavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use zinder_core::{BlockHeight, ChainEpochId, Network};

    use super::*;
    use crate::ArtifactFamily;
    use crate::store_error::{StorageErrorKind, StorageKey};

    /// One representative of every [`StoreError`] variant.
    ///
    /// The list is exhaustive, so a new variant fails to compile until it is
    /// listed; [`no_store_error_variant_maps_to_unspecified`] then asserts the
    /// new variant carries a real reason.
    #[allow(
        clippy::too_many_lines,
        reason = "One literal per StoreError variant; the length tracks the enum, not branching complexity."
    )]
    fn one_of_each_variant() -> Vec<StoreError> {
        fn boxed() -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(io::Error::other("probe"))
        }
        let sample_key = || StorageKey::from(crate::format::StoreKey::store_metadata());
        let variants = vec![
            StoreError::StorageUnavailable {
                kind: StorageErrorKind::RocksDb,
                source: boxed(),
            },
            StoreError::EntropyUnavailable {
                source: getrandom::Error::UNSUPPORTED,
            },
            StoreError::ChainEpochMissing {
                chain_epoch: ChainEpochId::new(1),
            },
            StoreError::NoVisibleChainEpoch,
            StoreError::ChainEpochConflict {
                current: ChainEpochId::new(1),
                attempted: ChainEpochId::new(2),
            },
            StoreError::ChainEpochNetworkMismatch {
                current: Network::ZcashMainnet,
                attempted: Network::ZcashTestnet,
            },
            StoreError::SchemaMismatch {
                persisted_version: 1,
                expected_version: 2,
            },
            StoreError::SchemaTooNew {
                persisted_version: 3,
                supported_version: 2,
            },
            StoreError::SchemaTooOld {
                persisted_version: 1,
                required_version: 2,
            },
            StoreError::PrimaryAlreadyOpen {
                lock_path: "lock".into(),
            },
            StoreError::SecondaryCatchupFailed { source: boxed() },
            StoreError::CheckpointUnavailable {
                path: "cp".into(),
                source: boxed(),
            },
            StoreError::ReorgWindowExceeded {
                attempted_from_height: BlockHeight::new(1),
                minimum_reorg_height: BlockHeight::new(2),
                safe_tip_height: BlockHeight::new(3),
            },
            StoreError::ChainEventCursorExpired {
                event_sequence: 1,
                oldest_retained_sequence: 2,
            },
            StoreError::ChainEventCursorInvalid { reason: "probe" },
            StoreError::AddressOutputCursorInvalid { reason: "probe" },
            StoreError::TransparentHistoryCursorInvalid { reason: "probe" },
            StoreError::MempoolEventCursorExpired {
                event_sequence: 1,
                oldest_retained_sequence: 2,
            },
            StoreError::MempoolEventCursorInvalid { reason: "probe" },
            StoreError::SnapshotPageCursorInvalid { reason: "probe" },
            StoreError::SnapshotPageCursorExpired {
                snapshot_sequence: 2,
                current_snapshot_sequence: 1,
            },
            StoreError::ChainEventSequenceOverflow,
            StoreError::MempoolEventSequenceOverflow,
            StoreError::ChainEpochSequenceOverflow,
            StoreError::InvalidChainEpochArtifacts { reason: "probe" },
            StoreError::ArtifactPayloadTooLarge {
                family: ArtifactFamily::CompactBlock,
                payload_len: 1,
            },
            StoreError::InvalidChainStoreOptions { reason: "probe" },
            StoreError::ArtifactMissing {
                family: ArtifactFamily::CompactBlock,
                key: sample_key(),
            },
            StoreError::ArtifactCorrupt {
                family: ArtifactFamily::CompactBlock,
                key: sample_key(),
                reason: "probe",
            },
            StoreError::Unsupported { feature: "probe" },
        ];
        variants
    }

    #[test]
    fn no_store_error_variant_maps_to_unspecified() {
        for error in one_of_each_variant() {
            assert_ne!(
                error.error_reason(),
                ErrorReason::Unspecified,
                "StoreError variant {error:?} mapped to ERROR_REASON_UNSPECIFIED"
            );
        }
    }
}
