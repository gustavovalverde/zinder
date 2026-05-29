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

use prost::Message as _;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use zinder_derive::{BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore};
use zinder_proto::v1::explorer::{
    BlockSummaryRecord, DeriveStatus, ExplorerFreshness, IndexedHead, UpstreamObservation,
};
use zinder_proto::v1::wallet;
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

/// Reads the explorer's indexed head: the highest block the derive
/// projections have fully materialized, decoded from the newest
/// `BlockSummaryRecord`. Returns `None` when no block is materialized yet.
///
/// All chain-event derive consumers advance under one shared cursor, so the
/// block-summary head is an accurate indexed head for every capability.
pub(crate) fn read_indexed_head(derive_store: &DeriveStore) -> Result<Option<IndexedHead>, Status> {
    let Some((_, payload)) = derive_store
        .last_consumer_entry(BLOCK_SUMMARY_COLUMN_FAMILY)
        .map_err(|error| Status::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    let summary = BlockSummaryRecord::decode(payload.as_slice())
        .map_err(|error| Status::internal(format!("BlockSummaryRecord decode failed: {error}")))?
        .summary
        .ok_or_else(|| Status::internal("BlockSummaryRecord.summary missing"))?;
    Ok(Some(IndexedHead {
        height: summary.block_height,
        hash: summary.block_hash,
        block_time_unix_seconds: summary.block_time_unix_seconds,
    }))
}

/// Builds the per-response [`ExplorerFreshness`] body shared by every read
/// handler.
///
/// Carries the canonical follower tip (`chain_epoch`) and the derive plane's
/// indexed head (the block the response actually reflects); consumers read
/// index lag as `chain_epoch.tip_height - indexed_head.height`. `derive_store`
/// is optional so the bootstrap `ServerInfo` call and any response built
/// before a derive store is wired leave `indexed_head` unset. The upstream
/// observation is overlaid separately by [`attach_upstream_observation`].
pub(crate) fn build_explorer_freshness(
    derive_store: Option<&DeriveStore>,
    capability_version: &str,
    chain_epoch: Option<wallet::ChainEpoch>,
    snapshot_age_millis: u64,
) -> Result<ExplorerFreshness, Status> {
    let indexed_head = match derive_store {
        Some(store) => read_indexed_head(store)?,
        None => None,
    };
    Ok(ExplorerFreshness {
        chain_epoch,
        snapshot_age_millis,
        capability_version: capability_version.to_owned(),
        unavailable: Vec::new(),
        upstream: None,
        indexed_head,
    })
}

/// Reads the derive-status record the ingest plane persists, decoding it into
/// the wire [`DeriveStatus`]. Returns `None` when no derive store is wired or
/// the ingest plane has not written a record yet.
pub(crate) fn read_derive_status(
    derive_store: Option<&DeriveStore>,
) -> Result<Option<DeriveStatus>, Status> {
    let Some(store) = derive_store else {
        return Ok(None);
    };
    let Some(bytes) = store
        .get_derive_status()
        .map_err(|error| Status::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    DeriveStatus::decode(bytes.as_slice())
        .map(Some)
        .map_err(|error| Status::internal(format!("DeriveStatus decode failed: {error}")))
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
            capability_version: zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1
                .to_owned(),
            unavailable: Vec::new(),
            upstream: None,
            indexed_head: None,
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
