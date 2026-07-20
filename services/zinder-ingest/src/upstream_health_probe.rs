//! Background task that drives `cause=upstream_not_ready` readiness from
//! [`NodeSource::poll_upstream_health`].
//!
//! Spawned once at writer startup per [ADR-0015 §Upstream sync detection].
//! The task is independent of the ingest loop: probe failures never cancel
//! the loop and never propagate beyond a warning log. The probe writes
//! only the [`zinder_runtime::ReadinessCause::UpstreamNotReady`] cause;
//! healthy snapshots leave readiness untouched so the loop's normal
//! `Ready`/`Syncing` writes carry the operator-visible status.
//!
//! [ADR-0015 §Upstream sync detection]:
//!     ../../../docs/adrs/0015-phase-driven-ingest.md#upstream-sync-detection

use std::{sync::Arc, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_runtime::{Readiness, UpstreamHealth, UpstreamNotReadyDetail};
use zinder_source::{NodeSource, UpstreamHealthSnapshot};

/// Spawns the upstream-health probe task.
///
/// The task ticks every `poll_interval`, calls
/// [`NodeSource::poll_upstream_health`] on `source`, and writes
/// [`zinder_runtime::ReadinessCause::UpstreamNotReady`] on the shared
/// [`Readiness`] when the snapshot is not ready. Healthy snapshots and
/// probe errors are no-ops (logged at debug/warn respectively); the
/// ingest loop continues to own every other readiness cause.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn spawn_upstream_health_probe_task<Source>(
    source: Arc<Source>,
    readiness: Readiness,
    poll_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    Source: NodeSource + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(poll_interval) => {
                    run_upstream_health_probe_once(source.as_ref(), &readiness).await;
                }
            }
        }
    })
}

async fn run_upstream_health_probe_once<Source>(source: &Source, readiness: &Readiness)
where
    Source: NodeSource,
{
    match source.poll_upstream_health().await {
        Ok(snapshot) => apply_upstream_health_snapshot(snapshot, readiness),
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "upstream_health_probe_failed",
                error = %error,
                "upstream health probe failed; leaving readiness untouched"
            );
        }
    }
}

fn apply_upstream_health_snapshot(snapshot: UpstreamHealthSnapshot, readiness: &Readiness) {
    if snapshot.ready_for_queries {
        return;
    }
    let detail = upstream_not_ready_detail_from_snapshot(snapshot);
    readiness.set_upstream_not_ready(detail);
}

fn upstream_not_ready_detail_from_snapshot(
    snapshot: UpstreamHealthSnapshot,
) -> UpstreamNotReadyDetail {
    UpstreamNotReadyDetail {
        upstream_committed_height: snapshot.upstream_committed_height,
        upstream_estimated_height: snapshot.upstream_estimated_height,
        upstream_verification_progress: snapshot.upstream_verification_progress,
        upstream_health: UpstreamHealth {
            source: snapshot.source,
            reason: snapshot.reason,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use zinder_runtime::{ReadinessCause, ReadinessState};
    use zinder_source::UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT;

    #[test]
    fn healthy_snapshot_leaves_readiness_untouched() {
        let readiness = Readiness::default();
        readiness.set(ReadinessState::ready(Some(42)));
        let snapshot = UpstreamHealthSnapshot::ready(
            UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT,
            Some(42),
            Some(42),
            Some(1.0),
        );
        apply_upstream_health_snapshot(snapshot, &readiness);
        let report = readiness.report();
        assert!(matches!(report.cause, ReadinessCause::Ready));
        assert_eq!(report.current_height, Some(42));
    }

    #[test]
    fn unhealthy_snapshot_sets_upstream_not_ready_with_current_height_preserved()
    -> Result<(), &'static str> {
        let readiness = Readiness::default();
        readiness.set(ReadinessState::ready(Some(17)));
        let snapshot = UpstreamHealthSnapshot::not_ready(
            UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT,
            "syncing",
            Some(15),
            Some(100),
            Some(0.5),
        );
        apply_upstream_health_snapshot(snapshot, &readiness);
        let report = readiness.report();
        let ReadinessCause::UpstreamNotReady(detail) = report.cause else {
            return Err("expected UpstreamNotReady cause");
        };
        assert_eq!(detail.upstream_health.source, "zebra_ready_endpoint");
        assert_eq!(detail.upstream_health.reason.as_ref(), "syncing");
        assert_eq!(detail.upstream_committed_height, Some(15));
        assert_eq!(detail.upstream_estimated_height, Some(100));
        assert_eq!(detail.upstream_verification_progress, Some(0.5));
        assert_eq!(report.current_height, Some(17));
        Ok(())
    }
}
