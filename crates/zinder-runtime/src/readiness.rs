//! Typed readiness state shared by every Zinder service.

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

/// Stable readiness cause matching `docs/architecture/service-operations.md`.
///
/// Causes that carry operator-actionable detail use struct variants so the
/// data is reachable by `serde_json` consumers without an out-of-band lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCause {
    /// Service is starting up but has not yet completed initialization.
    Starting,
    /// Service is catching up to the upstream node tip or replaying state.
    Syncing {
        /// Distance between the visible chain tip and the target tip, when
        /// known. `None` for read-only services with no observable tip.
        lag_blocks: Option<u64>,
    },
    /// Service is healthy and accepting production traffic.
    Ready,
    /// Upstream node source is unavailable.
    NodeUnavailable,
    /// A required node capability is missing.
    NodeCapabilityMissing {
        /// Stable name of the missing capability (matches
        /// `NodeCapability::name()` in `zinder-source`).
        capability: &'static str,
    },
    /// Canonical storage is unavailable.
    StorageUnavailable,
    /// Persisted store schema is incompatible with this binary.
    SchemaMismatch,
    /// Reorg replacement crossed the configured non-finalized window.
    ReorgWindowExceeded {
        /// Number of replaced visible heights.
        depth: u64,
        /// Configured non-finalized reorg window in blocks.
        configured: u64,
    },
    /// A `RocksDB` secondary reader is behind the primary beyond the configured threshold.
    ReplicaLagging {
        /// Chain-epoch distance between the writer and this secondary reader.
        lag_chain_epochs: u64,
    },
    /// The private ingest writer-status RPC cannot be reached.
    WriterStatusUnavailable,
    /// Retained event history is approaching the configured cursor-expiry window.
    CursorAtRisk {
        /// Age of the oldest retained event, rounded down to whole hours.
        oldest_retained_age_hours: u64,
        /// Configured retention window, rounded down to whole hours.
        retention_hours: u64,
    },
    /// Service is shutting down and no longer accepting new traffic.
    ShuttingDown,
}

impl ReadinessCause {
    /// Stable Prometheus label for this readiness cause.
    #[must_use]
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Syncing { .. } => "syncing",
            Self::Ready => "ready",
            Self::NodeUnavailable => "node_unavailable",
            Self::NodeCapabilityMissing { .. } => "node_capability_missing",
            Self::StorageUnavailable => "storage_unavailable",
            Self::SchemaMismatch => "schema_mismatch",
            Self::ReorgWindowExceeded { .. } => "reorg_window_exceeded",
            Self::ReplicaLagging { .. } => "replica_lagging",
            Self::WriterStatusUnavailable => "writer_status_unavailable",
            Self::CursorAtRisk { .. } => "cursor_at_risk",
            Self::ShuttingDown => "shutting_down",
        }
    }
}

/// Snapshot of the current readiness state surfaced to operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReadinessReport {
    /// `true` when the service is healthy enough to receive production traffic.
    pub is_ready: bool,
    /// Stable readiness cause.
    pub cause: ReadinessCause,
    /// Current visible chain height when known.
    pub current_height: Option<u32>,
    /// Node-observed target height when known.
    pub target_height: Option<u32>,
}

impl ReadinessReport {
    /// Returns a starting-state report carrying no chain heights yet.
    #[must_use]
    pub const fn starting() -> Self {
        Self {
            is_ready: false,
            cause: ReadinessCause::Starting,
            current_height: None,
            target_height: None,
        }
    }
}

/// Internal readiness state guarded by an `Arc<Mutex<_>>` so HTTP handlers
/// and runtime tasks can update and observe the same value.
#[derive(Clone, Debug)]
pub struct Readiness {
    inner: Arc<Mutex<ReadinessState>>,
}

impl Default for Readiness {
    fn default() -> Self {
        Self::new(ReadinessState::starting())
    }
}

impl Readiness {
    /// Creates a readiness handle seeded with `state`.
    #[must_use]
    pub fn new(state: ReadinessState) -> Self {
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    /// Replaces the current readiness state with `state`.
    pub fn set(&self, state: ReadinessState) {
        *self.inner.lock() = state;
    }

    /// Reports the current readiness as a serializable snapshot.
    #[must_use]
    pub fn report(&self) -> ReadinessReport {
        let state = *self.inner.lock();
        ReadinessReport {
            is_ready: matches!(state.cause, ReadinessCause::Ready),
            cause: state.cause,
            current_height: state.current_height,
            target_height: state.target_height,
        }
    }
}

/// Mutable readiness state owned by the service's runtime task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessState {
    /// Stable readiness cause.
    pub cause: ReadinessCause,
    /// Current visible chain height when known.
    pub current_height: Option<u32>,
    /// Node-observed target height when known.
    pub target_height: Option<u32>,
}

impl ReadinessState {
    /// Returns a starting state with no chain heights.
    #[must_use]
    pub const fn starting() -> Self {
        Self {
            cause: ReadinessCause::Starting,
            current_height: None,
            target_height: None,
        }
    }

    /// Returns a ready state.
    ///
    /// `current_height` carries the visible chain tip when known. Read-only
    /// services that have not yet observed a chain epoch (e.g. an empty store)
    /// pass `None`; the `/readyz` and `/metrics` outputs then omit the height
    /// rather than report a fabricated `0`.
    #[must_use]
    pub const fn ready(current_height: Option<u32>) -> Self {
        Self {
            cause: ReadinessCause::Ready,
            current_height,
            target_height: current_height,
        }
    }

    /// Returns a syncing state.
    ///
    /// `lag_blocks` is the distance between the visible chain tip and the
    /// target tip; pass `None` when the service has no observable target
    /// (for example, a read-only query node with no upstream node handle).
    #[must_use]
    pub const fn syncing(
        lag_blocks: Option<u64>,
        current_height: Option<u32>,
        target_height: Option<u32>,
    ) -> Self {
        Self {
            cause: ReadinessCause::Syncing { lag_blocks },
            current_height,
            target_height,
        }
    }

    /// Returns a not-ready state for a non-paramatric failure cause.
    ///
    /// For parametric causes ([`ReadinessCause::Syncing`],
    /// [`ReadinessCause::ReorgWindowExceeded`],
    /// [`ReadinessCause::ReplicaLagging`]) use the dedicated constructors.
    #[must_use]
    pub const fn not_ready(cause: ReadinessCause) -> Self {
        Self {
            cause,
            current_height: None,
            target_height: None,
        }
    }

    /// Returns a reorg-window-exceeded state.
    ///
    /// `current_height` carries the visible tip at the time of failure so the
    /// `/readyz` response includes the chain height the operator should
    /// reconcile against.
    #[must_use]
    pub const fn reorg_window_exceeded(
        depth: u64,
        configured: u64,
        current_height: Option<u32>,
    ) -> Self {
        Self {
            cause: ReadinessCause::ReorgWindowExceeded { depth, configured },
            current_height,
            target_height: None,
        }
    }

    /// Returns a node-capability-missing state.
    ///
    /// `capability` is the stable diagnostic name of the missing capability,
    /// matching the names returned by `NodeCapability::name()`.
    #[must_use]
    pub const fn node_capability_missing(capability: &'static str) -> Self {
        Self {
            cause: ReadinessCause::NodeCapabilityMissing { capability },
            current_height: None,
            target_height: None,
        }
    }

    /// Returns a replica-lagging state for secondary readers.
    #[must_use]
    pub const fn replica_lagging(lag_chain_epochs: u64, current_height: Option<u32>) -> Self {
        Self {
            cause: ReadinessCause::ReplicaLagging { lag_chain_epochs },
            current_height,
            target_height: None,
        }
    }

    /// Returns a cursor-at-risk state for event retention.
    #[must_use]
    pub const fn cursor_at_risk(
        oldest_retained_age_hours: u64,
        retention_hours: u64,
        current_height: Option<u32>,
    ) -> Self {
        Self {
            cause: ReadinessCause::CursorAtRisk {
                oldest_retained_age_hours,
                retention_hours,
            },
            current_height,
            target_height: current_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_marks_ready_only_for_ready_cause() {
        let readiness = Readiness::new(ReadinessState::ready(Some(10)));
        let report = readiness.report();
        assert!(report.is_ready);
        assert!(matches!(report.cause, ReadinessCause::Ready));
        assert_eq!(report.current_height, Some(10));
    }

    #[test]
    fn ready_without_height_omits_chain_heights() {
        let readiness = Readiness::new(ReadinessState::ready(None));
        let report = readiness.report();
        assert!(report.is_ready);
        assert_eq!(report.current_height, None);
        assert_eq!(report.target_height, None);
    }

    #[test]
    fn report_marks_not_ready_for_syncing_cause() {
        let readiness = Readiness::new(ReadinessState::syncing(Some(5), Some(5), Some(10)));
        let report = readiness.report();
        assert!(!report.is_ready);
        assert!(matches!(
            report.cause,
            ReadinessCause::Syncing {
                lag_blocks: Some(5)
            }
        ));
        assert_eq!(report.current_height, Some(5));
        assert_eq!(report.target_height, Some(10));
    }

    #[test]
    fn reorg_window_exceeded_carries_depth_and_configured() {
        let readiness = Readiness::new(ReadinessState::reorg_window_exceeded(12, 10, Some(100)));
        let report = readiness.report();
        assert!(!report.is_ready);
        assert!(matches!(
            report.cause,
            ReadinessCause::ReorgWindowExceeded {
                depth: 12,
                configured: 10
            }
        ));
        assert_eq!(report.current_height, Some(100));
    }

    #[test]
    fn replica_lagging_carries_lag_chain_epochs() {
        let readiness = Readiness::new(ReadinessState::replica_lagging(4, Some(100)));
        let report = readiness.report();
        assert!(!report.is_ready);
        assert!(matches!(
            report.cause,
            ReadinessCause::ReplicaLagging {
                lag_chain_epochs: 4
            }
        ));
        assert_eq!(report.current_height, Some(100));
    }

    #[test]
    fn cursor_at_risk_carries_retention_window() {
        let readiness = Readiness::new(ReadinessState::cursor_at_risk(145, 168, Some(100)));
        let report = readiness.report();
        assert!(!report.is_ready);
        assert!(matches!(
            report.cause,
            ReadinessCause::CursorAtRisk {
                oldest_retained_age_hours: 145,
                retention_hours: 168
            }
        ));
        assert_eq!(report.current_height, Some(100));
        assert_eq!(report.target_height, Some(100));
    }

    #[test]
    fn set_replaces_current_state() {
        let readiness = Readiness::default();
        assert!(matches!(readiness.report().cause, ReadinessCause::Starting));
        readiness.set(ReadinessState::ready(Some(7)));
        assert!(matches!(readiness.report().cause, ReadinessCause::Ready));
    }
}
