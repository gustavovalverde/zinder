//! Typed readiness state shared by every Zinder service.
//!
//! The hand-written [`ReadinessCause`] enum below mirrors the proto-defined
//! `zinder.v1.ops.ReadinessCause` enum 1:1. The proto enum and
//! `docs/architecture/service-operations.md` are the documented source of
//! truth for the cause vocabulary.
//! The Rust enum carries the struct-variant payloads so the existing
//! `/readyz` JSON wire shape stays byte-identical; the [`ReadinessReport`]
//! type below converts to the proto message via [`Into`] for any gRPC
//! consumer.

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use zinder_proto::v1::ops as ops_proto;

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
    /// Mempool retained-event history is approaching the configured retention
    /// window. Mempool retention is shorter than chain-event retention, so
    /// this signal is reported in minutes rather than hours.
    MempoolCursorAtRisk {
        /// Age of the oldest retained mempool event, rounded down to whole
        /// minutes.
        oldest_retained_age_minutes: u64,
        /// Shortest configured mempool retention window, rounded down to
        /// whole minutes.
        retention_minutes: u64,
    },
    /// Mempool source observation is unavailable. The writer cannot hydrate
    /// `Added` events without an upstream mempool stream.
    MempoolSourceUnavailable,
    /// Mempool source hydration is falling behind the source's emission rate.
    MempoolHydrationLagging {
        /// Total hydration failures observed since startup.
        ///
        /// Diagnostic only; operators should compare against
        /// `zinder_mempool_hydration_failures_total` for a rate.
        recent_hydration_failures: u64,
    },
    /// Service is shutting down and no longer accepting new traffic.
    ShuttingDown,
}

impl ReadinessCause {
    /// Every Prometheus label `metric_label` may return, in declaration order.
    ///
    /// `ops_endpoint::record_readiness_metrics` iterates this slice every
    /// scrape to zero out the gauges for inactive causes. The unit test
    /// `metric_label_is_listed_in_all_metric_labels` enforces that every
    /// `ReadinessCause` variant is represented; combined with the exhaustive
    /// `match` in [`Self::metric_label`], adding a new variant without
    /// extending this table fails CI.
    pub const ALL_METRIC_LABELS: &'static [&'static str] = &[
        "starting",
        "syncing",
        "ready",
        "node_unavailable",
        "node_capability_missing",
        "storage_unavailable",
        "schema_mismatch",
        "reorg_window_exceeded",
        "replica_lagging",
        "writer_status_unavailable",
        "cursor_at_risk",
        "mempool_cursor_at_risk",
        "mempool_source_unavailable",
        "mempool_hydration_lagging",
        "shutting_down",
    ];

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
            Self::MempoolCursorAtRisk { .. } => "mempool_cursor_at_risk",
            Self::MempoolSourceUnavailable => "mempool_source_unavailable",
            Self::MempoolHydrationLagging { .. } => "mempool_hydration_lagging",
            Self::ShuttingDown => "shutting_down",
        }
    }

    /// Returns whether this cause still permits production traffic.
    ///
    /// Warning causes remain operator-actionable through `/readyz` and
    /// `zinder_readiness_state`, but they must not make load balancer probes
    /// fail while the service can still safely serve requests.
    #[must_use]
    pub const fn permits_traffic(&self) -> bool {
        matches!(
            self,
            Self::Ready
                | Self::CursorAtRisk { .. }
                | Self::MempoolCursorAtRisk { .. }
                | Self::MempoolSourceUnavailable
                | Self::MempoolHydrationLagging { .. }
        )
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
            is_ready: state.cause.permits_traffic(),
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

    /// Returns an upstream-node-unavailable state.
    ///
    /// `current_height` carries the last visible tip when the writer can still
    /// read local storage while waiting for the upstream node to recover.
    #[must_use]
    pub const fn node_unavailable(current_height: Option<u32>) -> Self {
        Self {
            cause: ReadinessCause::NodeUnavailable,
            current_height,
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

    /// Returns a mempool-cursor-at-risk state for the mempool event log.
    #[must_use]
    pub const fn mempool_cursor_at_risk(
        oldest_retained_age_minutes: u64,
        retention_minutes: u64,
        current_height: Option<u32>,
    ) -> Self {
        Self {
            cause: ReadinessCause::MempoolCursorAtRisk {
                oldest_retained_age_minutes,
                retention_minutes,
            },
            current_height,
            target_height: current_height,
        }
    }

    /// Returns a state reporting the mempool source is unavailable.
    #[must_use]
    pub const fn mempool_source_unavailable(current_height: Option<u32>) -> Self {
        Self {
            cause: ReadinessCause::MempoolSourceUnavailable,
            current_height,
            target_height: current_height,
        }
    }

    /// Returns a state reporting that mempool hydration is failing.
    #[must_use]
    pub const fn mempool_hydration_lagging(
        recent_hydration_failures: u64,
        current_height: Option<u32>,
    ) -> Self {
        Self {
            cause: ReadinessCause::MempoolHydrationLagging {
                recent_hydration_failures,
            },
            current_height,
            target_height: current_height,
        }
    }
}

impl From<&ReadinessCause> for ops_proto::ReadinessCause {
    fn from(cause: &ReadinessCause) -> Self {
        match cause {
            ReadinessCause::Starting => Self::Starting,
            ReadinessCause::Syncing { .. } => Self::Syncing,
            ReadinessCause::Ready => Self::Ready,
            ReadinessCause::NodeUnavailable => Self::NodeUnavailable,
            ReadinessCause::NodeCapabilityMissing { .. } => Self::NodeCapabilityMissing,
            ReadinessCause::StorageUnavailable => Self::StorageUnavailable,
            ReadinessCause::SchemaMismatch => Self::SchemaMismatch,
            ReadinessCause::ReorgWindowExceeded { .. } => Self::ReorgWindowExceeded,
            ReadinessCause::ReplicaLagging { .. } => Self::ReplicaLagging,
            ReadinessCause::WriterStatusUnavailable => Self::WriterStatusUnavailable,
            ReadinessCause::CursorAtRisk { .. } => Self::CursorAtRisk,
            ReadinessCause::MempoolCursorAtRisk { .. } => Self::MempoolCursorAtRisk,
            ReadinessCause::MempoolSourceUnavailable => Self::MempoolSourceUnavailable,
            ReadinessCause::MempoolHydrationLagging { .. } => Self::MempoolHydrationLagging,
            ReadinessCause::ShuttingDown => Self::ShuttingDown,
        }
    }
}

impl From<ReadinessCause> for ops_proto::ReadinessCause {
    fn from(cause: ReadinessCause) -> Self {
        Self::from(&cause)
    }
}

impl From<&ReadinessCause> for Option<ops_proto::ReadinessCauseDetail> {
    fn from(cause: &ReadinessCause) -> Self {
        let payload = match cause {
            ReadinessCause::Syncing { lag_blocks } => {
                ops_proto::readiness_cause_detail::Payload::Syncing(ops_proto::SyncingDetail {
                    lag_blocks: *lag_blocks,
                })
            }
            ReadinessCause::NodeCapabilityMissing { capability } => {
                ops_proto::readiness_cause_detail::Payload::NodeCapabilityMissing(
                    ops_proto::NodeCapabilityMissingDetail {
                        capability: (*capability).to_owned(),
                    },
                )
            }
            ReadinessCause::ReorgWindowExceeded { depth, configured } => {
                ops_proto::readiness_cause_detail::Payload::ReorgWindowExceeded(
                    ops_proto::ReorgWindowExceededDetail {
                        depth: *depth,
                        configured: *configured,
                    },
                )
            }
            ReadinessCause::ReplicaLagging { lag_chain_epochs } => {
                ops_proto::readiness_cause_detail::Payload::ReplicaLagging(
                    ops_proto::ReplicaLaggingDetail {
                        lag_chain_epochs: *lag_chain_epochs,
                    },
                )
            }
            ReadinessCause::CursorAtRisk {
                oldest_retained_age_hours,
                retention_hours,
            } => ops_proto::readiness_cause_detail::Payload::CursorAtRisk(
                ops_proto::CursorAtRiskDetail {
                    oldest_retained_age_hours: *oldest_retained_age_hours,
                    retention_hours: *retention_hours,
                },
            ),
            ReadinessCause::MempoolCursorAtRisk {
                oldest_retained_age_minutes,
                retention_minutes,
            } => ops_proto::readiness_cause_detail::Payload::MempoolCursorAtRisk(
                ops_proto::MempoolCursorAtRiskDetail {
                    oldest_retained_age_minutes: *oldest_retained_age_minutes,
                    retention_minutes: *retention_minutes,
                },
            ),
            ReadinessCause::MempoolHydrationLagging {
                recent_hydration_failures,
            } => ops_proto::readiness_cause_detail::Payload::MempoolHydrationLagging(
                ops_proto::MempoolHydrationLaggingDetail {
                    recent_hydration_failures: *recent_hydration_failures,
                },
            ),
            ReadinessCause::Starting
            | ReadinessCause::Ready
            | ReadinessCause::NodeUnavailable
            | ReadinessCause::StorageUnavailable
            | ReadinessCause::SchemaMismatch
            | ReadinessCause::WriterStatusUnavailable
            | ReadinessCause::MempoolSourceUnavailable
            | ReadinessCause::ShuttingDown => return None,
        };
        Some(ops_proto::ReadinessCauseDetail {
            payload: Some(payload),
        })
    }
}

impl From<&ReadinessReport> for ops_proto::ReadinessReport {
    fn from(report: &ReadinessReport) -> Self {
        Self {
            cause: ops_proto::ReadinessCause::from(&report.cause) as i32,
            current_height: report.current_height,
            target_height: report.target_height,
            detail: Option::<ops_proto::ReadinessCauseDetail>::from(&report.cause),
        }
    }
}

impl From<ReadinessReport> for ops_proto::ReadinessReport {
    fn from(report: ReadinessReport) -> Self {
        Self::from(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_marks_ready_for_ready_cause() {
        let readiness = Readiness::new(ReadinessState::ready(Some(10)));
        let report = readiness.report();
        assert!(report.is_ready);
        assert!(matches!(report.cause, ReadinessCause::Ready));
        assert_eq!(report.current_height, Some(10));
    }

    #[test]
    fn warning_causes_remain_ready_for_traffic() {
        let warning_states = [
            ReadinessState::cursor_at_risk(145, 168, Some(100)),
            ReadinessState::mempool_cursor_at_risk(49, 60, Some(100)),
            ReadinessState::mempool_source_unavailable(Some(100)),
            ReadinessState::mempool_hydration_lagging(3, Some(100)),
        ];

        for state in warning_states {
            let report = Readiness::new(state).report();
            assert!(
                report.is_ready,
                "warning cause {:?} must not fail traffic readiness",
                report.cause
            );
            assert_eq!(report.current_height, Some(100));
        }
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
        assert!(report.is_ready);
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
    fn metric_label_is_listed_in_all_metric_labels() {
        // Every constructed variant's metric_label must appear in
        // ALL_METRIC_LABELS, and the table cardinality must equal the
        // variant count so a new variant cannot silently drop a gauge.
        let every_cause: &[ReadinessCause] = &[
            ReadinessCause::Starting,
            ReadinessCause::Syncing { lag_blocks: None },
            ReadinessCause::Ready,
            ReadinessCause::NodeUnavailable,
            ReadinessCause::NodeCapabilityMissing { capability: "test" },
            ReadinessCause::StorageUnavailable,
            ReadinessCause::SchemaMismatch,
            ReadinessCause::ReorgWindowExceeded {
                depth: 0,
                configured: 0,
            },
            ReadinessCause::ReplicaLagging {
                lag_chain_epochs: 0,
            },
            ReadinessCause::WriterStatusUnavailable,
            ReadinessCause::CursorAtRisk {
                oldest_retained_age_hours: 0,
                retention_hours: 0,
            },
            ReadinessCause::MempoolCursorAtRisk {
                oldest_retained_age_minutes: 0,
                retention_minutes: 0,
            },
            ReadinessCause::MempoolSourceUnavailable,
            ReadinessCause::MempoolHydrationLagging {
                recent_hydration_failures: 0,
            },
            ReadinessCause::ShuttingDown,
        ];
        for cause in every_cause {
            let label = cause.metric_label();
            assert!(
                ReadinessCause::ALL_METRIC_LABELS.contains(&label),
                "metric_label {label} for {cause:?} missing from ALL_METRIC_LABELS",
            );
        }
        assert_eq!(
            every_cause.len(),
            ReadinessCause::ALL_METRIC_LABELS.len(),
            "ALL_METRIC_LABELS cardinality must equal ReadinessCause variant count",
        );
    }

    fn proto_cause_for(cause: &ReadinessCause) -> ops_proto::ReadinessCause {
        match cause {
            ReadinessCause::Starting => ops_proto::ReadinessCause::Starting,
            ReadinessCause::Syncing { .. } => ops_proto::ReadinessCause::Syncing,
            ReadinessCause::Ready => ops_proto::ReadinessCause::Ready,
            ReadinessCause::NodeUnavailable => ops_proto::ReadinessCause::NodeUnavailable,
            ReadinessCause::NodeCapabilityMissing { .. } => {
                ops_proto::ReadinessCause::NodeCapabilityMissing
            }
            ReadinessCause::StorageUnavailable => ops_proto::ReadinessCause::StorageUnavailable,
            ReadinessCause::SchemaMismatch => ops_proto::ReadinessCause::SchemaMismatch,
            ReadinessCause::ReorgWindowExceeded { .. } => {
                ops_proto::ReadinessCause::ReorgWindowExceeded
            }
            ReadinessCause::ReplicaLagging { .. } => ops_proto::ReadinessCause::ReplicaLagging,
            ReadinessCause::WriterStatusUnavailable => {
                ops_proto::ReadinessCause::WriterStatusUnavailable
            }
            ReadinessCause::CursorAtRisk { .. } => ops_proto::ReadinessCause::CursorAtRisk,
            ReadinessCause::MempoolCursorAtRisk { .. } => {
                ops_proto::ReadinessCause::MempoolCursorAtRisk
            }
            ReadinessCause::MempoolSourceUnavailable => {
                ops_proto::ReadinessCause::MempoolSourceUnavailable
            }
            ReadinessCause::MempoolHydrationLagging { .. } => {
                ops_proto::ReadinessCause::MempoolHydrationLagging
            }
            ReadinessCause::ShuttingDown => ops_proto::ReadinessCause::ShuttingDown,
        }
    }

    #[test]
    fn proto_cause_maps_every_variant() {
        for cause in every_rust_cause() {
            assert_eq!(
                ops_proto::ReadinessCause::from(&cause),
                proto_cause_for(&cause),
                "Rust cause {cause:?} mapped to the wrong proto code"
            );
        }
    }

    fn every_rust_cause() -> Vec<ReadinessCause> {
        vec![
            ReadinessCause::Starting,
            ReadinessCause::Syncing { lag_blocks: None },
            ReadinessCause::Ready,
            ReadinessCause::NodeUnavailable,
            ReadinessCause::NodeCapabilityMissing {
                capability: "tx_broadcast",
            },
            ReadinessCause::StorageUnavailable,
            ReadinessCause::SchemaMismatch,
            ReadinessCause::ReorgWindowExceeded {
                depth: 0,
                configured: 0,
            },
            ReadinessCause::ReplicaLagging {
                lag_chain_epochs: 0,
            },
            ReadinessCause::WriterStatusUnavailable,
            ReadinessCause::CursorAtRisk {
                oldest_retained_age_hours: 0,
                retention_hours: 0,
            },
            ReadinessCause::MempoolCursorAtRisk {
                oldest_retained_age_minutes: 0,
                retention_minutes: 0,
            },
            ReadinessCause::MempoolSourceUnavailable,
            ReadinessCause::MempoolHydrationLagging {
                recent_hydration_failures: 0,
            },
            ReadinessCause::ShuttingDown,
        ]
    }

    #[test]
    fn proto_report_preserves_payload_for_parametric_causes() {
        let report = ReadinessReport {
            is_ready: false,
            cause: ReadinessCause::ReorgWindowExceeded {
                depth: 12,
                configured: 10,
            },
            current_height: Some(100),
            target_height: None,
        };

        let proto = ops_proto::ReadinessReport::from(&report);
        assert_eq!(
            proto.cause,
            ops_proto::ReadinessCause::ReorgWindowExceeded as i32
        );
        assert_eq!(proto.current_height, Some(100));
        assert_eq!(proto.target_height, None);

        let Some(detail) = proto.detail else {
            unreachable!("parametric cause must carry detail")
        };
        let Some(payload) = detail.payload else {
            unreachable!("detail must carry a payload")
        };
        let ops_proto::readiness_cause_detail::Payload::ReorgWindowExceeded(payload) = payload
        else {
            unreachable!("expected ReorgWindowExceeded payload variant")
        };
        assert_eq!(payload.depth, 12);
        assert_eq!(payload.configured, 10);
    }

    #[test]
    fn proto_report_carries_no_detail_for_scalar_causes() {
        let report = ReadinessReport {
            is_ready: false,
            cause: ReadinessCause::NodeUnavailable,
            current_height: None,
            target_height: None,
        };

        let proto = ops_proto::ReadinessReport::from(&report);
        assert_eq!(
            proto.cause,
            ops_proto::ReadinessCause::NodeUnavailable as i32
        );
        assert!(proto.detail.is_none());
    }

    #[test]
    fn set_replaces_current_state() {
        let readiness = Readiness::default();
        assert!(matches!(readiness.report().cause, ReadinessCause::Starting));
        readiness.set(ReadinessState::ready(Some(7)));
        assert!(matches!(readiness.report().cause, ReadinessCause::Ready));
    }
}
