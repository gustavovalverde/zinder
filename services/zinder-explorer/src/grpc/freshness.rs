//! Shared builders for the cross-cutting [`ExplorerFreshness`] envelope.
//!
//! Every explorer handler folds the same upstream tip into the
//! `chain_view.upstream_tip` axis of its freshness envelope before
//! responding. The adapter owns one cached [`UpstreamHealthSnapshot`]
//! (refreshed by a background probe) and shares it with every handler through
//! an [`UpstreamObservationCache`] handle.
//!
//! Per ADR-0011 the axis is optional: a response that fires before the first
//! probe completes carries `chain_view.upstream_tip = None`. Consumers treat
//! absence as "unknown", not zero.

use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use zinder_core::wire::encode_rpc_block_hash_hex;
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, MaterializedViewChainEventCheckpoint,
    MaterializedViewState, MaterializedViewStore, MaterializedViewStoreReadSnapshot,
};
use zinder_proto::capabilities::EXPLORER_SERVER_INFO_V1;

use super::error::ExplorerError;
use zinder_proto::v1::explorer::{BlockSummaryRecord, ExplorerFreshness};
use zinder_proto::v1::wallet::{
    self, ChainView, IndexedTip, MaterializedViewStatus, UpstreamTip, VisibleTipBlockRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;
use zinder_source::{NodeSource, UpstreamHealthSnapshot};

/// One materialized-view snapshot pinned to the admitted Wallet visible tip.
///
/// The value owns the `RocksDB` snapshot, exact Block Summary state, and Wallet
/// response epoch as one purpose-specific read fence. It is intentionally not
/// a generic Explorer read context: only row-backed contracts whose declared
/// dependency includes Block Summary may use it.
pub(crate) struct WalletPinnedBlockSummarySnapshot<'store> {
    snapshot: MaterializedViewStoreReadSnapshot<'store>,
    block_summary_state: MaterializedViewState,
    block_summary_checkpoint: MaterializedViewChainEventCheckpoint,
    wallet_chain_epoch: wallet::ChainEpoch,
}

impl WalletPinnedBlockSummarySnapshot<'_> {
    /// Returns the exact snapshot all row and freshness reads must use.
    pub(crate) const fn snapshot(&self) -> &MaterializedViewStoreReadSnapshot<'_> {
        &self.snapshot
    }

    /// Returns the state rechecked from [`Self::snapshot`].
    pub(crate) const fn block_summary_state(&self) -> MaterializedViewState {
        self.block_summary_state
    }

    /// Returns the authenticated chain-event checkpoint that fences the Block
    /// Summary rows in [`Self::snapshot`].
    pub(crate) const fn block_summary_checkpoint(&self) -> MaterializedViewChainEventCheckpoint {
        self.block_summary_checkpoint
    }

    /// Returns the Wallet epoch pinned to [`Self::block_summary_state`].
    pub(crate) fn wallet_chain_epoch(&self) -> &wallet::ChainEpoch {
        &self.wallet_chain_epoch
    }
}

/// Opens one Block Summary snapshot after pinning it to the admitted Wallet.
///
/// The candidate state is read before Wallet I/O solely to obtain the exact
/// epoch pin. The state is then reread from the opened snapshot and compared
/// in full, including revision and coverage, before the caller can observe a
/// row. The Wallet response must name the same epoch and complete visible-tip
/// identity in both its chain view and top-level response.
pub(crate) async fn pin_wallet_to_block_summary_snapshot<'store>(
    materialized_view_store: &'store MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<WalletPinnedBlockSummarySnapshot<'store>, Status> {
    let candidate_state = read_block_summary_state(materialized_view_store)?;
    let wallet_response = wallet_client
        .visible_tip_block(tonic::Request::new(VisibleTipBlockRequest {
            at_epoch_id: Some(candidate_state.chain_epoch_id.value()),
        }))
        .await?
        .into_inner();
    let wallet_chain_epoch =
        require_wallet_tip_matches_block_summary_state(&wallet_response, candidate_state)?;
    let snapshot = materialized_view_store
        .read_snapshot()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let snapshot_state =
        require_snapshot_block_summary_state_matches_candidate(&snapshot, candidate_state)?;
    require_complete_block_summary_coverage(snapshot_state)?;
    let block_summary_checkpoint = snapshot
        .chain_event_checkpoint(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("block-summary chain-event checkpoint is unavailable")
        })?;
    require_checkpoint_matches_block_summary_state(block_summary_checkpoint, snapshot_state)?;

    Ok(WalletPinnedBlockSummarySnapshot {
        snapshot,
        block_summary_state: snapshot_state,
        block_summary_checkpoint,
        wallet_chain_epoch,
    })
}

/// Rereads the Block Summary state from the request snapshot before any rows
/// become observable.
///
/// This is intentionally the narrow state barrier between the Wallet epoch
/// response and every row-backed handler read. A changed revision, coverage,
/// epoch, or tip rejects the request instead of allowing an E7 Wallet answer
/// to combine with E8 materialized-view rows.
fn require_snapshot_block_summary_state_matches_candidate(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    candidate_state: MaterializedViewState,
) -> Result<MaterializedViewState, Status> {
    let snapshot_state = snapshot
        .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("block-summary materialized-view state is unavailable")
        })?;
    if snapshot_state != candidate_state {
        return Err(ExplorerError::unsatisfied_precondition(
            "block-summary materialized-view state changed while the Wallet epoch was observed",
        )
        .into());
    }
    Ok(snapshot_state)
}

fn read_block_summary_state(
    materialized_view_store: &MaterializedViewStore,
) -> Result<MaterializedViewState, Status> {
    materialized_view_store
        .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("block-summary materialized-view state is unavailable")
                .into()
        })
}

fn require_complete_block_summary_coverage(state: MaterializedViewState) -> Result<(), Status> {
    let coverage = state.coverage.ok_or_else(|| {
        ExplorerError::not_materialized(
            "block-summary materialized-view coverage has not been verified",
        )
    })?;
    if coverage.complete_through_height != state.tip_height
        || coverage.complete_through_hash != state.tip_hash
    {
        return Err(ExplorerError::not_materialized(
            "block-summary materialized-view coverage does not reach its indexed tip",
        )
        .into());
    }
    Ok(())
}

fn require_checkpoint_matches_block_summary_state(
    checkpoint: MaterializedViewChainEventCheckpoint,
    state: MaterializedViewState,
) -> Result<(), Status> {
    let fence = checkpoint.resulting_fence();
    let visible_tip = fence.visible_tip();
    if fence.chain_epoch_id() != state.chain_epoch_id
        || visible_tip.height != state.tip_height
        || visible_tip.hash != state.tip_hash
    {
        return Err(ExplorerError::not_materialized(
            "block-summary chain-event checkpoint does not match its materialized-view state",
        )
        .into());
    }
    Ok(())
}

/// Requires the verified Block Summary coverage to contain one requested
/// inclusive height range.
pub(crate) fn require_block_summary_range_coverage(
    state: MaterializedViewState,
    start_height: u32,
    end_height: u32,
) -> Result<(), Status> {
    let coverage = state.coverage.ok_or_else(|| {
        ExplorerError::not_materialized(
            "block-summary materialized-view coverage has not been verified",
        )
    })?;
    if coverage.complete_from_height > zinder_core::BlockHeight::new(start_height)
        || coverage.complete_through_height < zinder_core::BlockHeight::new(end_height)
    {
        return Err(ExplorerError::not_materialized(format!(
            "block-summary materialized-view coverage {}..={} does not include requested range \
             {start_height}..={end_height}",
            coverage.complete_from_height.value(),
            coverage.complete_through_height.value(),
        ))
        .into());
    }
    Ok(())
}

fn require_wallet_tip_matches_block_summary_state(
    response: &wallet::VisibleTipBlockResponse,
    state: MaterializedViewState,
) -> Result<wallet::ChainEpoch, Status> {
    let chain_epoch = response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| {
            ExplorerError::internal("VisibleTipBlockResponse.chain_view.chain_epoch missing")
        })?;
    let epoch_tip = chain_epoch.visible_tip.as_ref().ok_or_else(|| {
        ExplorerError::internal("VisibleTipBlockResponse chain epoch visible_tip missing")
    })?;
    let response_tip = response.visible_tip_block.as_ref().ok_or_else(|| {
        ExplorerError::internal("VisibleTipBlockResponse.visible_tip_block missing")
    })?;
    let expected_hash = encode_rpc_block_hash_hex(state.tip_hash);
    if chain_epoch.chain_epoch_id != state.chain_epoch_id.value()
        || epoch_tip.height != state.tip_height.value()
        || epoch_tip.hash != expected_hash
        || response_tip.height != epoch_tip.height
        || response_tip.block_hash != epoch_tip.hash
    {
        return Err(ExplorerError::unsatisfied_precondition(
            "Wallet visible-tip identity does not match the Block Summary materialized-view state",
        )
        .into());
    }
    Ok(chain_epoch.clone())
}

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

/// Builds an [`UpstreamTip`] proto from a cached snapshot.
///
/// Carries heights only; the upstream probe has no single block hash.
pub(crate) fn upstream_tip_from_snapshot(snapshot: &UpstreamHealthSnapshot) -> UpstreamTip {
    UpstreamTip {
        committed_height: snapshot.upstream_committed_height,
        estimated_height: snapshot.upstream_estimated_height,
    }
}

/// Folds the cached upstream tip into the `chain_view.upstream_tip` axis of an
/// already-built [`ExplorerFreshness`] body.
///
/// Every handler builds its own freshness (the chain epoch, snapshot age,
/// capability version, and per-field unavailability vary per RPC). The shared
/// upstream tip is overlaid here so no handler reaches into the cache directly.
/// Responses such as `ServerInfo` do not resolve a chain epoch, but still need
/// the upstream tip as the sync-progress denominator during cold starts and
/// source-node catch-up. In that case this function creates a minimal
/// `chain_view` that carries only `upstream_tip`.
pub(crate) async fn attach_upstream_observation(
    cache: &UpstreamObservationCache,
    mut freshness: ExplorerFreshness,
) -> ExplorerFreshness {
    if let Some(snapshot) = cache.observe().await {
        let upstream_tip = upstream_tip_from_snapshot(&snapshot);
        match freshness.chain_view.as_mut() {
            Some(chain_view) => {
                chain_view.upstream_tip = Some(upstream_tip);
            }
            None => {
                freshness.chain_view = Some(ChainView {
                    chain_epoch: None,
                    indexed_tip: None,
                    upstream_tip: Some(upstream_tip),
                    materialized_views: None,
                });
            }
        }
    }
    freshness
}

/// Reads the explorer's indexed tip: the highest block the materialized views
/// have fully materialized, decoded from the newest `BlockSummaryRecord`.
/// Returns `None` when no block is materialized yet.
///
/// All chain-event materialized-view consumers advance under one shared cursor, so the
/// block-summary head is an accurate indexed tip for every capability.
pub(crate) fn read_indexed_tip(
    materialized_view_store: &MaterializedViewStore,
) -> Result<Option<IndexedTip>, Status> {
    if !materialized_view_store.has_consumer(BLOCK_SUMMARY_CONSUMER_NAME) {
        return Ok(None);
    }
    let Some((_, payload)) = materialized_view_store
        .last_consumer_entry(BLOCK_SUMMARY_COLUMN_FAMILY)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    decode_indexed_tip(&payload).map(Some)
}

fn read_indexed_tip_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<Option<IndexedTip>, Status> {
    let Some((_, payload)) = snapshot
        .last_consumer_entry(BLOCK_SUMMARY_COLUMN_FAMILY)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    decode_indexed_tip(&payload).map(Some)
}

fn decode_indexed_tip(payload: &[u8]) -> Result<IndexedTip, Status> {
    let summary = BlockSummaryRecord::decode(payload)
        .map_err(|error| {
            ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
        })?
        .summary
        .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
    Ok(IndexedTip {
        tip: Some(wallet::BlockTip {
            height: summary.block_height,
            hash: summary.block_hash,
        }),
        block_time_unix_seconds: summary.block_time_unix_seconds,
    })
}

/// Builds the per-response [`ExplorerFreshness`] body shared by every read
/// handler.
///
/// Assembles the cross-plane `chain_view` from the canonical follower tip
/// (`chain_epoch`), the materialized-view plane's indexed tip (the block the response
/// actually reflects), and the persisted materialized-view status. Consumers read index
/// lag as `chain_view.chain_epoch.visible_tip.height -
/// chain_view.indexed_tip.tip.height`. The materialized-view identity (indexed tip
/// and materialized-view status) is carried whenever `materialized_view_store` is wired, so the
/// bootstrap `ServerInfo` call reports how far the materialized views have
/// materialized even though its `chain_epoch` is absent because it makes no
/// snapshot-consistency claim. `chain_view` stays unset only when the response
/// resolves no chain epoch and no materialized-view store is wired. The upstream tip is
/// overlaid separately by [`attach_upstream_observation`].
pub(crate) fn build_explorer_freshness(
    materialized_view_store: Option<&MaterializedViewStore>,
    capability_version: &str,
    chain_epoch: Option<wallet::ChainEpoch>,
    snapshot_age_millis: u64,
) -> Result<ExplorerFreshness, Status> {
    let (indexed_tip, materialized_views) = match materialized_view_store {
        Some(store) => (
            read_indexed_tip(store)?,
            read_materialized_view_status(Some(store))?,
        ),
        None => (None, None),
    };
    Ok(explorer_freshness(
        indexed_tip,
        materialized_views,
        capability_version,
        chain_epoch,
        snapshot_age_millis,
    ))
}

/// Builds freshness from the same materialized-view snapshot as the response rows.
pub(crate) fn build_explorer_freshness_from_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    capability_version: &str,
    chain_epoch: Option<wallet::ChainEpoch>,
    snapshot_age_millis: u64,
) -> Result<ExplorerFreshness, Status> {
    let indexed_tip = read_indexed_tip_snapshot(snapshot)?;
    let materialized_views = read_materialized_view_status_snapshot(snapshot)?;
    Ok(explorer_freshness(
        indexed_tip,
        materialized_views,
        capability_version,
        chain_epoch,
        snapshot_age_millis,
    ))
}

/// Builds Server Info freshness from one unpinned local snapshot.
///
/// Server Info has no Wallet dependency. It may report an indexed tip only
/// when the composed store actually owns Block Summary; its persisted
/// materialized-view status is always read from the same snapshot as that tip.
pub(crate) fn build_server_info_freshness_from_snapshot(
    materialized_view_store: &MaterializedViewStore,
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<ExplorerFreshness, Status> {
    let indexed_tip = if materialized_view_store.has_consumer(BLOCK_SUMMARY_CONSUMER_NAME) {
        read_indexed_tip_snapshot(snapshot)?
    } else {
        None
    };
    let materialized_views = read_materialized_view_status_snapshot(snapshot)?;
    Ok(explorer_freshness(
        indexed_tip,
        materialized_views,
        EXPLORER_SERVER_INFO_V1,
        None,
        0,
    ))
}

fn explorer_freshness(
    indexed_tip: Option<IndexedTip>,
    materialized_views: Option<MaterializedViewStatus>,
    capability_version: &str,
    chain_epoch: Option<wallet::ChainEpoch>,
    snapshot_age_millis: u64,
) -> ExplorerFreshness {
    let chain_view =
        if chain_epoch.is_some() || indexed_tip.is_some() || materialized_views.is_some() {
            Some(ChainView {
                chain_epoch,
                indexed_tip,
                upstream_tip: None,
                materialized_views,
            })
        } else {
            None
        };
    ExplorerFreshness {
        chain_view,
        snapshot_age_millis,
        capability_version: capability_version.to_owned(),
        unavailable: Vec::new(),
    }
}

/// Returns true when the materialized-view indexed tip is the visible tip of the
/// supplied canonical chain epoch.
pub(crate) fn indexed_tip_matches_chain_epoch(
    freshness: &ExplorerFreshness,
    chain_epoch: &wallet::ChainEpoch,
) -> bool {
    freshness
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.indexed_tip.as_ref())
        .and_then(|indexed_tip| indexed_tip.tip.as_ref())
        .zip(chain_epoch.visible_tip.as_ref())
        .is_some_and(|(indexed_tip, visible_tip)| indexed_tip == visible_tip)
}

/// Reads the persisted materialized-view status.
///
/// Decodes the record into the wire [`MaterializedViewStatus`]. Returns `None`
/// when no materialized-view store is wired or the ingest plane has not written
/// a record yet.
pub(crate) fn read_materialized_view_status(
    materialized_view_store: Option<&MaterializedViewStore>,
) -> Result<Option<MaterializedViewStatus>, Status> {
    let Some(store) = materialized_view_store else {
        return Ok(None);
    };
    let Some(bytes) = store
        .get_materialized_view_status()
        .map_err(|error| ExplorerError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    decode_materialized_view_status(&bytes).map(Some)
}

pub(crate) fn read_materialized_view_status_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<Option<MaterializedViewStatus>, Status> {
    let Some(bytes) = snapshot
        .get_materialized_view_status()
        .map_err(|error| ExplorerError::internal(error.to_string()))?
    else {
        return Ok(None);
    };
    decode_materialized_view_status(&bytes).map(Some)
}

fn decode_materialized_view_status(bytes: &[u8]) -> Result<MaterializedViewStatus, Status> {
    MaterializedViewStatus::decode(bytes).map_err(|error| {
        ExplorerError::internal(format!("MaterializedViewStatus decode failed: {error}")).into()
    })
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
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::sync_channel,
        },
        time::Duration,
    };

    use eyre::{Result, eyre};
    use tempfile::tempdir;
    use tokio::{net::TcpListener, sync::Notify};
    use tokio_stream::wrappers::TcpListenerStream;
    use zinder_core::{
        BlockHash, BlockHeight, ChainEpochId, Network, wire::decode_rpc_block_hash_hex,
    };
    use zinder_materialized_views::{
        BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_SCHEMA, BlockSummaryConsumer,
        MaterializedViewCoverage, MaterializedViewStoreOptions,
    };
    use zinder_query::{
        AdmittedIngestControl, CanonicalReader, WalletEndpointMetadata, WalletProjectionReader,
        WalletQueryGrpcAdapter, WalletServingPairSlot, WalletServingQuery, WalletServingReadPair,
    };
    use zinder_runtime::connect_zinder_grpc;
    use zinder_source::UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT;
    use zinder_store::RocksDbResourceBudget;
    use zinder_testkit::{
        ChainFixture, IngestControlFixture, WalletServingStoreFixture,
        sample_regtest_upgrade_activations,
    };

    fn synthetic_freshness(chain_view: Option<ChainView>) -> ExplorerFreshness {
        ExplorerFreshness {
            chain_view,
            snapshot_age_millis: 0,
            capability_version: zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1
                .to_owned(),
            unavailable: Vec::new(),
        }
    }

    fn synthetic_chain_view() -> ChainView {
        ChainView {
            chain_epoch: Some(wallet::ChainEpoch::default()),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }
    }

    #[tokio::test]
    async fn attach_leaves_upstream_unset_when_probe_never_fired() {
        let cache = UpstreamObservationCache::empty();
        let freshness =
            attach_upstream_observation(&cache, synthetic_freshness(Some(synthetic_chain_view())))
                .await;
        let upstream_tip = freshness
            .chain_view
            .and_then(|chain_view| chain_view.upstream_tip);
        assert!(upstream_tip.is_none());
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
        let freshness =
            attach_upstream_observation(&cache, synthetic_freshness(Some(synthetic_chain_view())))
                .await;
        let Some(upstream) = freshness
            .chain_view
            .and_then(|chain_view| chain_view.upstream_tip)
        else {
            return Err("expected upstream tip");
        };
        assert_eq!(upstream.committed_height, Some(2_530_000));
        assert_eq!(upstream.estimated_height, Some(2_544_375));
        Ok(())
    }

    #[test]
    fn wallet_pin_rejects_same_epoch_with_a_different_tip_height() -> Result<(), &'static str> {
        let state = block_summary_state();
        let response = wallet_response(&state, BlockHeight::new(101), state.tip_hash);

        let error = require_wallet_tip_matches_block_summary_state(&response, state)
            .err()
            .ok_or("mismatched tip height must fail")?;

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    fn wallet_pin_rejects_same_epoch_and_height_with_a_different_tip_hash()
    -> Result<(), &'static str> {
        let state = block_summary_state();
        let response = wallet_response(&state, state.tip_height, BlockHash::from_bytes([9; 32]));

        let error = require_wallet_tip_matches_block_summary_state(&response, state)
            .err()
            .ok_or("mismatched tip hash must fail")?;

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    fn wallet_pin_rejects_a_response_tip_that_disagrees_with_its_epoch_tip()
    -> Result<(), &'static str> {
        let state = block_summary_state();
        let mut response = wallet_response(&state, state.tip_height, state.tip_hash);
        response.visible_tip_block = Some(wallet::BlockId {
            height: state.tip_height.value(),
            block_hash: encode_rpc_block_hash_hex(BlockHash::from_bytes([8; 32])),
        });

        let error = require_wallet_tip_matches_block_summary_state(&response, state)
            .err()
            .ok_or("inconsistent response tip must fail")?;

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(
        clippy::too_many_lines,
        reason = "the production race proof keeps the authenticated Wallet server, gated interceptor, two epochs, and escape assertion together"
    )]
    async fn wallet_pin_rejects_e8_before_any_e8_block_summary_row_can_escape() -> Result<()> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
        let activations = Arc::new(sample_regtest_upgrade_activations());
        let mut wallet_store_fixture =
            WalletServingStoreFixture::from_chain(&chain_fixture, activations.as_ref())?;
        let construction_identity = wallet_store_fixture.canonical_construction_identity()?;
        let (canonical_reader, wallet_reader) = wallet_store_fixture.take_readers()?;
        let pair = WalletServingReadPair::new(
            Arc::new(canonical_reader) as Arc<dyn CanonicalReader>,
            Arc::new(wallet_reader) as Arc<dyn WalletProjectionReader>,
        )?;
        let ingest_control_fixture = IngestControlFixture::spawn(chain_fixture.network()).await?;
        let ingest_control = AdmittedIngestControl::connect(
            ingest_control_fixture.endpoint(),
            None,
            chain_fixture.network(),
        )
        .await?;
        let query = WalletServingQuery::from_admitted_native_serving_pair(
            WalletServingPairSlot::new(Arc::new(pair)),
            (),
            ingest_control,
            activations,
        )?;
        let adapter = WalletQueryGrpcAdapter::new(query, WalletEndpointMetadata::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let gate_armed = Arc::new(AtomicBool::new(false));
        let gate_seen = Arc::new(Notify::new());
        let (release_sender, release_receiver) = sync_channel(1);
        let release_receiver = Arc::new(tokio::sync::Mutex::new(Some(release_receiver)));
        let gate_for_server = Arc::clone(&gate_armed);
        let seen_for_server = Arc::clone(&gate_seen);
        let release_for_server = Arc::clone(&release_receiver);
        let wallet_handle = tokio::spawn(async move {
            let _wallet_store_fixture = wallet_store_fixture;
            let _ingest_control_fixture = ingest_control_fixture;
            let service = tonic::service::interceptor::InterceptedService::new(
                adapter.into_server(),
                move |request| {
                    if gate_for_server.swap(false, Ordering::SeqCst) {
                        let receiver = release_for_server
                            .try_lock()
                            .map_err(|_| Status::internal("Wallet race gate lock unavailable"))?
                            .take()
                            .ok_or_else(|| Status::internal("Wallet race gate already consumed"))?;
                        seen_for_server.notify_one();
                        receiver.recv().map_err(|_| {
                            Status::internal("Wallet race gate release channel dropped")
                        })?;
                    }
                    Ok(request)
                },
            );
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
        });
        let endpoint = format!("http://{address}");
        let channel = connect_test_wallet_channel(&endpoint).await?;
        let mut inspection_client = WalletQueryClient::new(channel.clone());
        let e7_wallet_response = inspection_client
            .visible_tip_block(tonic::Request::new(VisibleTipBlockRequest {
                at_epoch_id: None,
            }))
            .await?
            .into_inner();
        let e7_wallet_epoch = e7_wallet_response
            .chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
            .cloned()
            .ok_or_else(|| eyre!("test Wallet response omitted its chain epoch"))?;
        let e7_wallet_tip = e7_wallet_response
            .visible_tip_block
            .as_ref()
            .ok_or_else(|| eyre!("test Wallet response omitted its visible tip"))?;
        let e7_tip_hash = decode_rpc_block_hash_hex(&e7_wallet_tip.block_hash)
            .map_err(|error| eyre!("test Wallet response carried malformed tip hash: {error}"))?;
        let e7_tip_height = BlockHeight::new(e7_wallet_tip.height);
        let e7_state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(e7_wallet_epoch.chain_epoch_id),
            tip_height: e7_tip_height,
            tip_hash: e7_tip_hash,
            revision: 7,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: e7_tip_height,
                complete_through_height: e7_tip_height,
                complete_through_hash: e7_tip_hash,
            }),
        };
        let temporary_directory = tempdir()?;
        let store = MaterializedViewStore::open(
            temporary_directory.path(),
            construction_identity,
            MaterializedViewStoreOptions {
                consumers: &[BLOCK_SUMMARY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        let e7_row_key = BlockSummaryConsumer::key_for_height(e7_state.tip_height);
        store.put_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &e7_row_key, b"E7-row")?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e7_state)?;

        let e8_height = BlockHeight::new(
            e7_state
                .tip_height
                .value()
                .checked_add(1)
                .ok_or_else(|| eyre!("test tip height overflow"))?,
        );
        let e8_state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(
                e7_state
                    .chain_epoch_id
                    .value()
                    .checked_add(1)
                    .ok_or_else(|| eyre!("test chain epoch overflow"))?,
            ),
            tip_height: e8_height,
            tip_hash: BlockHash::from_bytes([0xe8; 32]),
            revision: e7_state
                .revision
                .checked_add(1)
                .ok_or_else(|| eyre!("test state revision overflow"))?,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: e8_height,
                complete_through_height: e8_height,
                complete_through_hash: BlockHash::from_bytes([0xe8; 32]),
            }),
        };
        let e8_row_key = BlockSummaryConsumer::key_for_height(e8_height);
        gate_armed.store(true, Ordering::SeqCst);
        let observed_wallet_request = gate_seen.notified();
        tokio::pin!(observed_wallet_request);
        let mut wallet_client = WalletQueryClient::new(channel);
        let pin = pin_wallet_to_block_summary_snapshot(&store, &mut wallet_client);
        tokio::pin!(pin);
        tokio::select! {
            () = &mut observed_wallet_request => {}
            _outcome = &mut pin => {
                wallet_handle.abort();
                let _ = wallet_handle.await;
                return Err(eyre!("Wallet pin completed before the E7 response gate opened"));
            }
        }

        // Wallet is now held while the primary advances from distinguishable
        // E7 rows to E8. The pin helper must reject before it can observe a
        // row from this later state.
        store.put_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &e8_row_key, b"E8-row")?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e8_state)?;
        assert_eq!(
            store.get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &e8_row_key)?,
            Some(b"E8-row".to_vec())
        );
        release_sender
            .send(())
            .map_err(|_| eyre!("Wallet race gate receiver dropped"))?;
        let error = pin
            .await
            .err()
            .ok_or_else(|| eyre!("the E8 snapshot must not satisfy the E7 Wallet fence"))?;
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        wallet_handle.abort();
        let _ = wallet_handle.await;
        Ok(())
    }

    fn block_summary_state() -> MaterializedViewState {
        let tip_height = BlockHeight::new(100);
        let tip_hash = BlockHash::from_bytes([7; 32]);
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(42),
            tip_height,
            tip_hash,
            revision: 3,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: tip_height,
                complete_through_hash: tip_hash,
            }),
        }
    }

    async fn connect_test_wallet_channel(endpoint: &str) -> Result<AuthenticatedChannel> {
        let mut last_error = String::new();
        for _ in 0..50 {
            match connect_zinder_grpc(endpoint, None).await {
                Ok(channel) => return Ok(channel),
                Err(error) => last_error = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(eyre!(
            "test Wallet endpoint did not become reachable: {last_error}"
        ))
    }

    fn wallet_response(
        state: &MaterializedViewState,
        response_tip_height: BlockHeight,
        response_tip_hash: BlockHash,
    ) -> wallet::VisibleTipBlockResponse {
        let epoch_tip = wallet::BlockTip {
            height: response_tip_height.value(),
            hash: encode_rpc_block_hash_hex(response_tip_hash),
        };
        wallet::VisibleTipBlockResponse {
            chain_view: Some(ChainView {
                chain_epoch: Some(wallet::ChainEpoch {
                    chain_epoch_id: state.chain_epoch_id.value(),
                    visible_tip: Some(epoch_tip.clone()),
                    ..Default::default()
                }),
                indexed_tip: None,
                upstream_tip: None,
                materialized_views: None,
            }),
            visible_tip_block: Some(wallet::BlockId {
                height: epoch_tip.height,
                block_hash: epoch_tip.hash,
            }),
        }
    }
}
