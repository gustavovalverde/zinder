//! Coordination point between the mempool orchestrator and the tip-follow
//! readiness state machine.
//!
//! The writer must finish rebuilding the live `MempoolIndex` before
//! signalling `ready`. After a writer restart the index is empty until the
//! orchestrator receives the source's
//! [`MempoolSourceEvent::InitialSnapshotComplete`] control marker. The
//! streaming backend emits it after its upstream snapshot; the polling
//! backend emits it after a complete first poll. Opening a stream alone is
//! not evidence that an empty index is safe to expose.
//!
//! [`MempoolReadyGate`] is a live `false ↔ true` watch that the tip-follow
//! readiness state machine consults before flipping to
//! [`zinder_runtime::ReadinessCause::Ready`]. It returns to `false` on every
//! source reconnect because a discarded in-memory index is not safe to serve
//! until the replacement snapshot completes.
//!
//! [`MempoolSourceEvent::InitialSnapshotComplete`]:
//! zinder_source::MempoolSourceEvent::InitialSnapshotComplete

use tokio::sync::watch;

/// Read-side handle observed by the tip-follow state machine.
#[derive(Clone, Debug)]
pub struct MempoolReadyGate {
    hydrated: watch::Receiver<bool>,
}

impl MempoolReadyGate {
    /// Returns `true` only while the current source generation has observed a
    /// complete initial snapshot.
    #[must_use]
    pub fn is_hydrated(&self) -> bool {
        *self.hydrated.borrow()
    }

    /// Waits until the source generation changes its hydration state.
    ///
    /// The canonical follower uses this to withdraw or restamp readiness
    /// immediately instead of waiting for its next tip poll.
    pub(crate) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.hydrated.changed().await
    }
}

/// Write-side handle owned by the orchestrator's spawn loop. Cloning is
/// cheap; every clone shares the same hydration state.
#[derive(Clone, Debug)]
pub struct MempoolReadySignal {
    hydrated: watch::Sender<bool>,
}

impl MempoolReadySignal {
    /// Marks the current source generation as fully hydrated.
    pub fn set_ready(&self) {
        // `send_replace` ignores the "no receivers" condition that `send`
        // returns, so optional readiness wiring does not make source
        // ownership fail.
        let _ = self.hydrated.send_replace(true);
    }

    /// Withdraws the gate while the current index is being rebuilt.
    pub fn set_hydrating(&self) {
        let _ = self.hydrated.send_replace(false);
    }
}

/// Constructs a fresh, unhydrated gate plus its sender.
#[must_use]
pub fn mempool_ready_channel() -> (MempoolReadySignal, MempoolReadyGate) {
    let (tx, rx) = watch::channel(false);
    (
        MempoolReadySignal { hydrated: tx },
        MempoolReadyGate { hydrated: rx },
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::mempool_ready_channel;

    #[test]
    fn gate_is_unhydrated_at_construction() {
        let (_signal, gate) = mempool_ready_channel();
        assert!(!gate.is_hydrated());
    }

    #[test]
    fn set_ready_flips_gate_to_true() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_ready();
        assert!(gate.is_hydrated());
    }

    #[test]
    fn set_ready_is_idempotent() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_ready();
        signal.set_ready();
        assert!(gate.is_hydrated());
    }

    #[test]
    fn set_hydrating_withdraws_a_previously_ready_gate() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_ready();
        signal.set_hydrating();
        assert!(!gate.is_hydrated());
    }
}
