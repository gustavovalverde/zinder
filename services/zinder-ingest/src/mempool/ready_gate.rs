//! Coordination point between the live mempool owner and the tip-follow
//! readiness state machine.
//!
//! The writer must finish rebuilding the live `MempoolIndex` before
//! signalling `ready`. After a writer restart the index is empty until the
//! owner receives the source's
//! [`MempoolSourceEvent::InitialSnapshotComplete`] control marker. The
//! streaming backend emits it after its upstream snapshot; the polling
//! backend emits it after a complete first poll. Opening a stream alone is
//! not evidence that an empty index is safe to expose.
//!
//! [`MempoolReadyGate`] watches the source tip certified by the current
//! hydrated generation. The tip-follow readiness state machine publishes
//! [`zinder_runtime::ReadinessCause::Ready`] only when that source tip exactly
//! matches its canonical fence. The certification is withdrawn on every
//! source reconnect because a discarded in-memory index is not safe to serve
//! until the replacement snapshot completes.
//!
//! [`MempoolSourceEvent::InitialSnapshotComplete`]:
//! zinder_source::MempoolSourceEvent::InitialSnapshotComplete

use tokio::sync::watch;
use zinder_core::BlockId;

/// Read-side handle observed by the tip-follow state machine.
#[derive(Clone, Debug)]
pub struct MempoolReadyGate {
    certified_source_tip: watch::Receiver<Option<BlockId>>,
}

impl MempoolReadyGate {
    /// Returns the current source generation's certified tip while its owner
    /// is still running.
    ///
    /// A closed channel fails closed even though `tokio::sync::watch` retains
    /// its last value. Otherwise an owner that exits after publishing `true`
    /// could leave ingest readiness permanently admitted.
    #[must_use]
    pub fn certified_source_tip(&self) -> Option<BlockId> {
        self.certified_source_tip
            .has_changed()
            .is_ok()
            .then(|| *self.certified_source_tip.borrow())
            .flatten()
    }

    /// Returns whether the current source generation is certified against the
    /// supplied canonical fence.
    #[must_use]
    pub fn admits_canonical_tip(&self, canonical_tip: BlockId) -> bool {
        self.certified_source_tip() == Some(canonical_tip)
    }

    /// Waits until the source generation changes its hydration state.
    ///
    /// The canonical follower uses this to withdraw or restamp readiness
    /// immediately instead of waiting for its next tip poll.
    pub(crate) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.certified_source_tip.changed().await
    }
}

/// Write-side handle owned by the live mempool owner's source loop. Cloning is
/// cheap; every clone shares the same hydration state.
#[derive(Clone, Debug)]
pub struct MempoolReadySignal {
    certified_source_tip: watch::Sender<Option<BlockId>>,
}

impl MempoolReadySignal {
    /// Marks the current source generation as hydrated and certified against
    /// `source_tip`.
    pub fn certify_source_tip(&self, source_tip: BlockId) {
        // `send_replace` ignores the "no receivers" condition that `send`
        // returns, so optional readiness wiring does not make source
        // ownership fail.
        let _ = self.certified_source_tip.send_replace(Some(source_tip));
    }

    /// Withdraws the gate while the current index is being rebuilt.
    pub fn withdraw_certification(&self) {
        let _ = self.certified_source_tip.send_replace(None);
    }
}

/// Constructs a fresh, unhydrated gate plus its sender.
#[must_use]
pub fn mempool_ready_channel() -> (MempoolReadySignal, MempoolReadyGate) {
    let (tx, rx) = watch::channel(None);
    (
        MempoolReadySignal {
            certified_source_tip: tx,
        },
        MempoolReadyGate {
            certified_source_tip: rx,
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use zinder_core::{BlockHash, BlockHeight, BlockId};

    use super::mempool_ready_channel;

    fn source_tip(tag: u8) -> BlockId {
        BlockId::new(
            BlockHeight::new(u32::from(tag)),
            BlockHash::from_bytes([tag; 32]),
        )
    }

    #[test]
    fn gate_is_unhydrated_at_construction() {
        let (_signal, gate) = mempool_ready_channel();
        assert_eq!(gate.certified_source_tip(), None);
    }

    #[test]
    fn certifying_a_source_tip_admits_that_exact_tip() {
        let (signal, gate) = mempool_ready_channel();
        let certified_tip = source_tip(1);
        signal.certify_source_tip(certified_tip);
        assert!(gate.admits_canonical_tip(certified_tip));
    }

    #[test]
    fn certifying_the_same_source_tip_is_idempotent() {
        let (signal, gate) = mempool_ready_channel();
        let certified_tip = source_tip(1);
        signal.certify_source_tip(certified_tip);
        signal.certify_source_tip(certified_tip);
        assert!(gate.admits_canonical_tip(certified_tip));
    }

    #[test]
    fn withdrawing_certification_revokes_a_previously_admitted_tip() {
        let (signal, gate) = mempool_ready_channel();
        let certified_tip = source_tip(1);
        signal.certify_source_tip(certified_tip);
        signal.withdraw_certification();
        assert!(!gate.admits_canonical_tip(certified_tip));
    }

    #[test]
    fn dropping_the_signal_withdraws_a_previously_ready_gate() {
        let (signal, gate) = mempool_ready_channel();
        let certified_tip = source_tip(1);
        signal.certify_source_tip(certified_tip);
        assert!(gate.admits_canonical_tip(certified_tip));

        drop(signal);

        assert!(!gate.admits_canonical_tip(certified_tip));
    }
}
