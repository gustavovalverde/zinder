//! Shared builders for the cross-cutting [`ExplorerFreshness`] envelope.
//!
//! Every explorer handler folds the same `UpstreamObservation` into its
//! freshness envelope before responding. The adapter owns one cached
//! [`UpstreamHealthSnapshot`] (refreshed by a background probe) and shares
//! it with every handler through an [`UpstreamObservationCache`] handle.
//!
//! Per ADR-0011 the field is optional: a response that fires before the
//! first probe completes carries `freshness.upstream = None`. Consumers
//! treat absence as "unknown", not zero.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_proto::v1::explorer::{ExplorerFreshness, UpstreamObservation};
use zinder_source::{NodeSource, UpstreamHealthSnapshot};

/// Shared, lock-protected handle to the most recent
/// [`UpstreamHealthSnapshot`] the adapter has observed.
///
/// Cloned cheaply (it is `Arc<RwLock<_>>` inside) and passed into every
/// handler so the response builder can read the cached snapshot without
/// hitting the upstream node on the request path. Updated only by the
/// background probe task spawned by
/// [`spawn_upstream_observation_probe_task`].
#[derive(Clone, Debug, Default)]
pub(crate) struct UpstreamObservationCache {
    inner: Arc<RwLock<Option<UpstreamHealthSnapshot>>>,
}

impl UpstreamObservationCache {
    /// Returns a fresh empty cache.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Reads the latest cached snapshot, if the probe has fired at least
    /// once. Cloned out so handlers do not hold the read guard across
    /// freshness construction.
    pub(crate) async fn observe(&self) -> Option<UpstreamHealthSnapshot> {
        self.inner.read().await.clone()
    }

    async fn store(&self, snapshot: UpstreamHealthSnapshot) {
        *self.inner.write().await = Some(snapshot);
    }
}

/// Builds an [`UpstreamObservation`] proto from a cached snapshot.
///
/// Returns `None` when the probe has not fired yet so the freshness
/// envelope can leave `upstream` unset.
pub(crate) fn upstream_observation_from_snapshot(
    snapshot: &UpstreamHealthSnapshot,
) -> UpstreamObservation {
    UpstreamObservation {
        upstream_committed_tip_height: snapshot.upstream_committed_height,
        upstream_estimated_tip_height: snapshot.upstream_estimated_height,
        upstream_verification_progress: snapshot.upstream_verification_progress,
    }
}

/// Folds the cached upstream observation into an already-built
/// [`ExplorerFreshness`] body.
///
/// Every handler builds its own freshness (the chain epoch, snapshot age,
/// derive-cursor lag, capability version, and per-field unavailability
/// vary per RPC). The shared upstream observation is overlaid here so
/// no handler reaches into the cache directly.
pub(crate) async fn attach_upstream_observation(
    cache: &UpstreamObservationCache,
    mut freshness: ExplorerFreshness,
) -> ExplorerFreshness {
    if let Some(snapshot) = cache.observe().await {
        freshness.upstream = Some(upstream_observation_from_snapshot(&snapshot));
    }
    freshness
}

/// Spawns the background task that refreshes the
/// [`UpstreamObservationCache`] on a fixed cadence.
///
/// The task ticks every `poll_interval`, calls
/// [`NodeSource::poll_upstream_health`] on `source`, and writes the
/// returned [`UpstreamHealthSnapshot`] into `cache`. Errors are logged at
/// warn and never propagate; the cache keeps serving its prior value
/// (or stays empty if the probe never succeeded) so a transient upstream
/// outage does not poison the freshness envelope.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub(crate) fn spawn_upstream_observation_probe_task<Source>(
    source: Arc<Source>,
    cache: UpstreamObservationCache,
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
                    run_upstream_observation_probe_once(source.as_ref(), &cache).await;
                }
            }
        }
    })
}

async fn run_upstream_observation_probe_once<Source>(
    source: &Source,
    cache: &UpstreamObservationCache,
) where
    Source: NodeSource,
{
    match source.poll_upstream_health().await {
        Ok(snapshot) => cache.store(snapshot).await,
        Err(error) => {
            tracing::warn!(
                target: "zinder::explorer",
                event = "upstream_observation_probe_failed",
                error = %error,
                "upstream observation probe failed; freshness envelope keeps the prior snapshot",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use zinder_source::UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT;

    fn synthetic_freshness() -> ExplorerFreshness {
        ExplorerFreshness {
            chain_epoch: None,
            snapshot_age_millis: 0,
            derive_cursor_lag_blocks: 0,
            derive_cursor_lag_millis: 0,
            capability_version: zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1
                .to_owned(),
            unavailable: Vec::new(),
            upstream: None,
        }
    }

    #[tokio::test]
    async fn attach_leaves_upstream_unset_when_probe_never_fired() {
        let cache = UpstreamObservationCache::empty();
        let freshness = attach_upstream_observation(&cache, synthetic_freshness()).await;
        assert!(freshness.upstream.is_none());
    }

    #[tokio::test]
    async fn attach_copies_cached_snapshot_into_freshness() -> Result<(), &'static str> {
        let cache = UpstreamObservationCache::empty();
        cache
            .store(UpstreamHealthSnapshot::ready(
                UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT,
                Some(2_530_000),
                Some(2_544_375),
                Some(0.9943),
            ))
            .await;
        let freshness = attach_upstream_observation(&cache, synthetic_freshness()).await;
        let Some(upstream) = freshness.upstream else {
            return Err("expected upstream observation");
        };
        assert_eq!(upstream.upstream_committed_tip_height, Some(2_530_000));
        assert_eq!(upstream.upstream_estimated_tip_height, Some(2_544_375));
        assert_eq!(upstream.upstream_verification_progress, Some(0.9943));
        Ok(())
    }
}
