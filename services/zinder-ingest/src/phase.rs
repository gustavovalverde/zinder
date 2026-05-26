//! Phase classifier for the unified ingest loop.
//!
//! Per [ADR-0015](../../../docs/adrs/0015-unified-phase-driven-ingest.md),
//! the writer dispatches its work into one of three phases on every
//! iteration: [`IngestPhase::AwaitingUpstream`],
//! [`IngestPhase::BulkCatchup`], and [`IngestPhase::FollowingTip`]. This
//! module owns the pure-function classifier ([`classify_phase`]) plus the
//! shared [`current_chain_height`] helper consumed by every phase handler.
//! The handlers and the spawn-once orchestration live in `ingest_loop.rs`.

use zinder_runtime::IngestPhase;
use zinder_store::PrimaryChainStore;

/// Returns the tip height of the store's visible chain epoch, or `None`
/// when the store is empty or its epoch pointer cannot be read.
///
/// Used by the per-phase handlers (`bulk catchup`, `tip_follow`) and the
/// ingest binary entrypoint to populate readiness state. The
/// implementation is intentionally infallible: a failure to read the
/// epoch pointer is reported as "no height" rather than propagated,
/// because readiness reporting must not block on a transient storage
/// hiccup.
#[must_use]
pub fn current_chain_height(store: &PrimaryChainStore) -> Option<u32> {
    store
        .current_chain_epoch()
        .ok()
        .flatten()
        .map(|chain_epoch| chain_epoch.tip_height.value())
}

/// Picks the [`IngestPhase`] for the current iteration.
///
/// Rules per [ADR-0015 §Decision]:
///
/// - [`IngestPhase::AwaitingUpstream`] when `upstream_tip == 0`. Models
///   regtest near genesis and freshly initialized nodes: there is no
///   committable chain yet, so the loop parks until the upstream tip
///   moves.
/// - [`IngestPhase::BulkCatchup`] when the gap from the store tip to
///   the upstream tip exceeds `catchup_threshold_blocks`. The bulk
///   driver runs the pipelined fetch shape and commits with
///   `AdvanceSafeTipTo`.
/// - [`IngestPhase::FollowingTip`] otherwise. The serial driver
///   commits one block per poll cycle and advances the safe-tip
///   boundary through `finalize_tip_if_ready`.
///
/// An empty store (`store_tip = None`) is treated as height `0` so the
/// gap is the upstream tip itself. A store ahead of the upstream tip
/// (local reorg in progress, `invalidateblock` race) collapses the gap
/// to zero via saturating subtraction and lands in `FollowingTip`; the
/// inner handler observes the rewound state and parks until the
/// replacement chain re-emerges.
///
/// [ADR-0015 §Decision]:
///     ../../../docs/adrs/0015-unified-phase-driven-ingest.md#decision
#[must_use]
pub fn classify_phase(
    store_tip: Option<u32>,
    upstream_tip: u32,
    catchup_threshold_blocks: u32,
) -> IngestPhase {
    if upstream_tip == 0 {
        return IngestPhase::AwaitingUpstream;
    }
    let store = store_tip.unwrap_or(0);
    let gap = upstream_tip.saturating_sub(store);
    if gap > catchup_threshold_blocks {
        IngestPhase::BulkCatchup
    } else {
        IngestPhase::FollowingTip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaits_upstream_when_upstream_tip_is_genesis() {
        assert_eq!(classify_phase(None, 0, 100), IngestPhase::AwaitingUpstream);
        assert_eq!(
            classify_phase(Some(0), 0, 100),
            IngestPhase::AwaitingUpstream
        );
    }

    #[test]
    fn follows_tip_when_upstream_chain_is_shorter_than_threshold() {
        // Empty store, small upstream: FollowingTip can commit serially.
        assert_eq!(classify_phase(None, 50, 100), IngestPhase::FollowingTip);
    }

    #[test]
    fn follows_tip_when_gap_equals_threshold() {
        // Boundary: gap == threshold is FollowingTip; only a strictly
        // larger gap escalates to BulkCatchup.
        assert_eq!(
            classify_phase(Some(900), 1_000, 100),
            IngestPhase::FollowingTip
        );
    }

    #[test]
    fn bulk_catches_up_when_gap_exceeds_threshold() {
        assert_eq!(
            classify_phase(Some(0), 1_000_000, 100),
            IngestPhase::BulkCatchup
        );
        assert_eq!(
            classify_phase(Some(100), 100_001, 100),
            IngestPhase::BulkCatchup
        );
    }

    #[test]
    fn follows_tip_when_store_is_ahead_of_upstream() {
        // Local reorg or invalidateblock race: gap saturates to zero
        // and we land in FollowingTip; the handler parks until the
        // replacement chain re-emerges.
        assert_eq!(
            classify_phase(Some(1_000), 500, 100),
            IngestPhase::FollowingTip
        );
    }

    #[test]
    fn follows_tip_when_already_at_upstream_tip() {
        assert_eq!(
            classify_phase(Some(1_000), 1_000, 100),
            IngestPhase::FollowingTip
        );
    }
}
