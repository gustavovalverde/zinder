//! Coordination point between the mempool orchestrator and the tip-follow
//! readiness state machine.
//!
//! Per ADR-0010 §Implementation, the writer must finish rebuilding the live
//! `MempoolIndex` before signalling `ready`. After a writer restart the index
//! is empty until the orchestrator successfully opens a `MempoolSource`
//! stream and (for the streaming backend) receives the upstream snapshot, or
//! (for the polling backend) completes its first poll cycle. Both backends
//! signal readiness by reporting
//! [`MempoolOrchestratorEventOutcome::SourceStreamOpened`] after their first
//! successful subscribe.
//!
//! [`MempoolReadyGate`] is a sticky `false → true` watch that the
//! tip-follow readiness state machine consults before flipping to
//! [`zinder_runtime::ReadinessCause::Ready`]. Once primed, the gate stays
//! primed across reconnects: a transient source disconnect should manifest
//! as `MempoolSourceUnavailable` rather than as a regression to `Syncing`.
//!
//! [`MempoolOrchestratorEventOutcome::SourceStreamOpened`]:
//! crate::mempool::MempoolOrchestratorEventOutcome::SourceStreamOpened

use tokio::sync::watch;

/// Read-side handle observed by the tip-follow state machine.
#[derive(Clone, Debug)]
pub struct MempoolReadyGate {
    primed: watch::Receiver<bool>,
}

impl MempoolReadyGate {
    /// Returns `true` when the orchestrator has opened the mempool source
    /// stream at least once.
    #[must_use]
    pub fn is_primed(&self) -> bool {
        *self.primed.borrow()
    }
}

/// Write-side handle owned by the orchestrator's spawn loop. Cloning is
/// cheap; every clone shares the same primed state.
#[derive(Clone, Debug)]
pub struct MempoolReadySignal {
    primed: watch::Sender<bool>,
}

impl MempoolReadySignal {
    /// Marks the gate as primed. Idempotent: subsequent calls are no-ops.
    pub fn set_primed(&self) {
        // `send_replace` ignores the "no receivers" condition that
        // `send` returns, so a primed signal that nobody is observing
        // (tests that omit the gate) does not error.
        if !*self.primed.borrow() {
            let _ = self.primed.send_replace(true);
        }
    }
}

/// Constructs a fresh, unprimed gate plus its sender.
#[must_use]
pub fn mempool_ready_channel() -> (MempoolReadySignal, MempoolReadyGate) {
    let (tx, rx) = watch::channel(false);
    (
        MempoolReadySignal { primed: tx },
        MempoolReadyGate { primed: rx },
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
    fn gate_is_unprimed_at_construction() {
        let (_signal, gate) = mempool_ready_channel();
        assert!(!gate.is_primed());
    }

    #[test]
    fn set_primed_flips_gate_to_true() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_primed();
        assert!(gate.is_primed());
    }

    #[test]
    fn set_primed_is_idempotent() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_primed();
        signal.set_primed();
        assert!(gate.is_primed());
    }

    #[test]
    fn dropping_signal_after_priming_keeps_gate_primed() {
        let (signal, gate) = mempool_ready_channel();
        signal.set_primed();
        drop(signal);
        assert!(gate.is_primed());
    }
}
