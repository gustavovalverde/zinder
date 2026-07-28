//! `ExplorerQuery.OverviewSnapshot` handler.
//!
//! Composes one coherent point-in-time bundle that overview consumers
//! used to assemble from six independent RPCs (`MempoolSummary` +
//! `FeeSummary` + `ValuePoolSummary` + `BlockSummariesInRange` +
//! `RecentTransactions` + `MempoolEventCounts`). Consumer-side fan-out
//! lets per-card freshness diverge: the tile claims height N while the
//! list shows height N+50 because the calls land on different RPC
//! windows. This handler instead anchors every sub-field to the same
//! `WalletQuery.VisibleTipBlock` tip and reads every materialized-view
//! column-family in one pass, so the response carries one
//! `ExplorerFreshness`. The bundle's snapshot identity is
//! `freshness.chain_view.chain_epoch`; consumers compare its
//! `chain_epoch_id` and `visible_tip` across responses.

use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_core::{BlockHeight, Network, wire::encode_height_key_ascending};
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
    MaterializedViewState, MaterializedViewStore, MaterializedViewStoreReadSnapshot,
    MempoolEventCountsConsumer, TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSACTION_HISTORY_CONSUMER_NAME,
};
use zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1;
use zinder_proto::v1::explorer::{
    BlockSummary, BlockSummaryRecord, ExplorerFreshness, OverviewFeeSummary, OverviewMempool,
    OverviewMempoolEvents, OverviewSnapshotRequest, OverviewSnapshotResponse,
    TransactionHistoryEntry,
};
use zinder_proto::v1::wallet::{
    self, ChainValuePoolsAtTipRequest, VisibleTipBlockRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness_from_snapshot,
};
use super::mempool::{CompleteMempoolObservation, fetch_complete_mempool_observation};
use super::transaction_history::decode_history_entry;

/// Range cap on `recent_blocks_limit`.
const MIN_RECENT_BLOCKS_LIMIT: u32 = 1;
/// Server-side ceiling on `recent_blocks_limit`.
const MAX_RECENT_BLOCKS_LIMIT: u32 = 16;
/// Server default when `recent_blocks_limit == 0`.
const DEFAULT_RECENT_BLOCKS_LIMIT: u32 = 8;

/// Range cap on `recent_transactions_limit`.
const MIN_RECENT_TRANSACTIONS_LIMIT: u32 = 1;
/// Server-side ceiling on `recent_transactions_limit`.
const MAX_RECENT_TRANSACTIONS_LIMIT: u32 = 64;
/// Server default when `recent_transactions_limit == 0`.
const DEFAULT_RECENT_TRANSACTIONS_LIMIT: u32 = 32;

/// Lower bound on `mempool_window_seconds` (matches `MempoolEventCounts`).
const MIN_MEMPOOL_WINDOW_SECONDS: u32 = 60;
/// Upper bound on `mempool_window_seconds` (matches `MempoolEventCounts`).
const MAX_MEMPOOL_WINDOW_SECONDS: u32 = 3_600;
/// Server default when `mempool_window_seconds == 0`.
const DEFAULT_MEMPOOL_WINDOW_SECONDS: u32 = 300;

/// Range cap on `fee_summary_block_count`.
const MIN_FEE_SUMMARY_BLOCK_COUNT: u32 = 1;
/// Server-side ceiling matches the per-request cap of `FeeSummary`.
const MAX_FEE_SUMMARY_BLOCK_COUNT: u32 = 256;
/// Server default when `fee_summary_block_count == 0`.
const DEFAULT_FEE_SUMMARY_BLOCK_COUNT: u32 = 50;

/// Executes one `ExplorerQuery.OverviewSnapshot` request.
pub(crate) async fn query_overview_snapshot(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: Network,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<OverviewSnapshotRequest>,
) -> Result<Response<OverviewSnapshotResponse>, Status> {
    let limits = RequestLimits::from_request(request.into_inner());
    let candidate = read_overview_state(materialized_view_store)?;
    let anchor = anchor_to_wallet_tip(wallet_client, candidate.block_summary).await?;
    let mempool_observation = fetch_complete_mempool_observation(wallet_client, network).await?;
    require_mempool_identity(&anchor.chain_epoch, &mempool_observation)?;
    let value_pool_response = wallet_client
        .chain_value_pools_at_tip(Request::new(ChainValuePoolsAtTipRequest {}))
        .await?
        .into_inner();
    require_value_pool_identity(&anchor.chain_epoch, &value_pool_response)?;

    let rows = {
        let snapshot = materialized_view_store.read_snapshot();
        let final_state = read_overview_state_snapshot(&snapshot)?;
        require_unchanged_overview_state(candidate, final_state)?;
        let block_records = read_block_summary_records_snapshot(
            &snapshot,
            anchor.tip_height,
            &limits,
            final_state.block_summary,
        )?;
        let recent_blocks =
            collect_recent_blocks(&block_records, limits.recent_blocks, anchor.tip_height);
        let fee_summary = aggregate_fee_summary(&block_records, limits.fee_summary_blocks);
        let tip_block_time_unix_seconds = recent_blocks
            .first()
            .map_or(0, |summary| summary.block_time_unix_seconds);
        let recent_transactions = read_recent_transactions_snapshot(
            &snapshot,
            limits.recent_transactions,
            final_state.transaction_history,
        )?;
        let mempool_events =
            read_mempool_event_counts_snapshot(&snapshot, limits.mempool_window_seconds)?;
        let freshness = build_explorer_freshness_from_snapshot(
            &snapshot,
            EXPLORER_OVERVIEW_SNAPSHOT_V1,
            Some(anchor.chain_epoch),
            mempool_observation.snapshot_age_millis,
        )?;
        drop(snapshot);
        OverviewSnapshotRows {
            freshness,
            tip_block_time_unix_seconds,
            mempool_events,
            fee_summary,
            recent_blocks,
            recent_transactions,
        }
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, rows.freshness).await;
    Ok(Response::new(OverviewSnapshotResponse {
        freshness: Some(freshness),
        tip_block_time_unix_seconds: rows.tip_block_time_unix_seconds,
        mempool: Some(overview_mempool(&mempool_observation)),
        mempool_events: Some(rows.mempool_events),
        fee_summary: Some(rows.fee_summary),
        value_pools: value_pool_response.pools,
        recent_blocks: rows.recent_blocks,
        recent_transactions: rows.recent_transactions,
    }))
}

struct OverviewSnapshotRows {
    freshness: ExplorerFreshness,
    tip_block_time_unix_seconds: i64,
    mempool_events: OverviewMempoolEvents,
    fee_summary: OverviewFeeSummary,
    recent_blocks: Vec<BlockSummary>,
    recent_transactions: Vec<TransactionHistoryEntry>,
}

/// Server-clamped request limits derived from the caller's inputs.
#[derive(Clone, Copy)]
struct RequestLimits {
    recent_blocks: u32,
    recent_transactions: u32,
    mempool_window_seconds: u32,
    fee_summary_blocks: u32,
}

impl RequestLimits {
    fn from_request(request: OverviewSnapshotRequest) -> Self {
        Self {
            recent_blocks: clamp(
                request.recent_blocks_limit,
                DEFAULT_RECENT_BLOCKS_LIMIT,
                MIN_RECENT_BLOCKS_LIMIT,
                MAX_RECENT_BLOCKS_LIMIT,
            ),
            recent_transactions: clamp(
                request.recent_transactions_limit,
                DEFAULT_RECENT_TRANSACTIONS_LIMIT,
                MIN_RECENT_TRANSACTIONS_LIMIT,
                MAX_RECENT_TRANSACTIONS_LIMIT,
            ),
            mempool_window_seconds: clamp(
                request.mempool_window_seconds,
                DEFAULT_MEMPOOL_WINDOW_SECONDS,
                MIN_MEMPOOL_WINDOW_SECONDS,
                MAX_MEMPOOL_WINDOW_SECONDS,
            ),
            fee_summary_blocks: clamp(
                request.fee_summary_block_count,
                DEFAULT_FEE_SUMMARY_BLOCK_COUNT,
                MIN_FEE_SUMMARY_BLOCK_COUNT,
                MAX_FEE_SUMMARY_BLOCK_COUNT,
            ),
        }
    }

    fn block_window_count(&self) -> u32 {
        self.recent_blocks.max(self.fee_summary_blocks)
    }
}

/// Wallet-anchored snapshot identity (chain epoch + canonical tip height).
struct WalletAnchor {
    chain_epoch: wallet::ChainEpoch,
    tip_height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverviewMaterializedViewState {
    block_summary: MaterializedViewState,
    transaction_history: MaterializedViewState,
}

fn read_overview_state(
    store: &MaterializedViewStore,
) -> Result<OverviewMaterializedViewState, Status> {
    let block_summary = store
        .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("overview block-summary state is unavailable")
        })?;
    let transaction_history = store
        .consumer_state(TRANSACTION_HISTORY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("overview transaction-history state is unavailable")
        })?;
    validate_overview_state(block_summary, transaction_history)
}

fn read_overview_state_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<OverviewMaterializedViewState, Status> {
    let block_summary = snapshot
        .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("overview block-summary state is unavailable")
        })?;
    let transaction_history = snapshot
        .consumer_state(TRANSACTION_HISTORY_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("overview transaction-history state is unavailable")
        })?;
    validate_overview_state(block_summary, transaction_history)
}

fn validate_overview_state(
    block_summary: MaterializedViewState,
    transaction_history: MaterializedViewState,
) -> Result<OverviewMaterializedViewState, Status> {
    let block_coverage = complete_overview_coverage("block-summary", block_summary)?;
    let history_coverage = complete_overview_coverage("transaction-history", transaction_history)?;
    if block_summary.chain_epoch_id != transaction_history.chain_epoch_id
        || block_summary.tip_height != transaction_history.tip_height
        || block_summary.tip_hash != transaction_history.tip_hash
        || block_coverage != history_coverage
    {
        return Err(ExplorerError::unsatisfied_precondition(
            "overview block-summary and transaction-history states do not share one chain fence",
        )
        .into());
    }
    Ok(OverviewMaterializedViewState {
        block_summary,
        transaction_history,
    })
}

fn require_unchanged_overview_state(
    candidate: OverviewMaterializedViewState,
    final_state: OverviewMaterializedViewState,
) -> Result<(), Status> {
    if final_state != candidate {
        return Err(ExplorerError::unsatisfied_precondition(
            "overview materialized-view state changed while wallet observations were collected",
        )
        .into());
    }
    Ok(())
}

fn complete_overview_coverage(
    consumer: &str,
    state: MaterializedViewState,
) -> Result<zinder_materialized_views::MaterializedViewCoverage, Status> {
    let coverage = state.coverage.ok_or_else(|| {
        ExplorerError::not_materialized(format!(
            "overview {consumer} coverage has not been verified"
        ))
    })?;
    if coverage.complete_through_height != state.tip_height
        || coverage.complete_through_hash != state.tip_hash
    {
        return Err(ExplorerError::not_materialized(format!(
            "overview {consumer} coverage is not complete through its indexed tip"
        ))
        .into());
    }
    Ok(coverage)
}

async fn anchor_to_wallet_tip(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    state: MaterializedViewState,
) -> Result<WalletAnchor, Status> {
    let response = wallet_client
        .visible_tip_block(Request::new(VisibleTipBlockRequest {
            at_epoch_id: Some(state.chain_epoch_id.value()),
        }))
        .await?
        .into_inner();
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
    let expected_hash = zinder_core::wire::encode_rpc_block_hash_hex(state.tip_hash);
    if chain_epoch.chain_epoch_id != state.chain_epoch_id.value()
        || epoch_tip.height != state.tip_height.value()
        || epoch_tip.hash != expected_hash
        || response_tip.height != epoch_tip.height
        || response_tip.block_hash != epoch_tip.hash
    {
        return Err(ExplorerError::unsatisfied_precondition(
            "wallet visible-tip identity does not match the overview materialized-view state",
        )
        .into());
    }
    Ok(WalletAnchor {
        chain_epoch: chain_epoch.clone(),
        tip_height: epoch_tip.height,
    })
}

fn read_block_summary_records_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    canonical_tip_height: u32,
    limits: &RequestLimits,
    state: MaterializedViewState,
) -> Result<Vec<BlockSummaryRecord>, Status> {
    let window = limits.block_window_count();
    let coverage = complete_overview_coverage("block-summary", state)?;
    let start_height = canonical_tip_height.saturating_sub(window.saturating_sub(1));
    require_overview_block_window_coverage(coverage, start_height, canonical_tip_height)?;
    let heights = (start_height..=canonical_tip_height).collect::<Vec<_>>();
    let keys = heights
        .iter()
        .map(|height| encode_height_key_ascending(BlockHeight::new(*height)))
        .collect::<Vec<_>>();
    let payloads = snapshot
        .multi_get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &keys)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut records = Vec::with_capacity(payloads.len());
    for (expected_height, payload) in heights.into_iter().zip(payloads) {
        let payload = payload.ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "overview BlockSummary is not materialized for height {expected_height}"
            ))
        })?;
        let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
            ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
        })?;
        let summary = record
            .summary
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
        if summary.block_height != expected_height {
            return Err(ExplorerError::internal(format!(
                "overview BlockSummaryRecord at height {expected_height} carries height {}",
                summary.block_height
            ))
            .into());
        }
        if expected_height == state.tip_height.value()
            && summary.block_hash != zinder_core::wire::encode_rpc_block_hash_hex(state.tip_hash)
        {
            return Err(ExplorerError::unsatisfied_precondition(
                "overview block-summary tip row does not match its materialized-view state",
            )
            .into());
        }
        records.push(record);
    }
    Ok(records)
}

fn require_overview_block_window_coverage(
    coverage: zinder_materialized_views::MaterializedViewCoverage,
    start_height: u32,
    end_height: u32,
) -> Result<(), Status> {
    if coverage.complete_from_height > BlockHeight::new(start_height)
        || coverage.complete_through_height < BlockHeight::new(end_height)
    {
        return Err(ExplorerError::not_materialized(format!(
            "overview block-summary coverage {}..={} does not include requested window \
             {start_height}..={end_height}",
            coverage.complete_from_height.value(),
            coverage.complete_through_height.value(),
        ))
        .into());
    }
    Ok(())
}

fn collect_recent_blocks(
    records: &[BlockSummaryRecord],
    limit: u32,
    canonical_tip_height: u32,
) -> Vec<BlockSummary> {
    let cap = usize::try_from(limit).unwrap_or(MAX_RECENT_BLOCKS_LIMIT as usize);
    let mut summaries: Vec<BlockSummary> = records
        .iter()
        .rev()
        .filter_map(|record| record.summary.clone())
        .take(cap)
        .collect();
    for summary in &mut summaries {
        summary.confirmations = canonical_tip_height
            .saturating_sub(summary.block_height)
            .saturating_add(1);
        summary.is_canonical = true;
    }
    summaries
}

fn aggregate_fee_summary(records: &[BlockSummaryRecord], limit: u32) -> OverviewFeeSummary {
    let cap = usize::try_from(limit).unwrap_or(MAX_FEE_SUMMARY_BLOCK_COUNT as usize);
    let mut block_count: u32 = 0;
    let mut transaction_count: u32 = 0;
    let mut total_fee_zat: u64 = 0;
    let mut min_fee_zat: Option<u64> = None;
    let mut max_fee_zat: Option<u64> = None;
    for record in records.iter().rev().take(cap) {
        block_count = block_count.saturating_add(1);
        transaction_count = transaction_count.saturating_add(record.fee_transaction_count);
        if let Some(summary) = &record.summary {
            total_fee_zat = total_fee_zat.saturating_add(summary.fees_collected_zat);
        }
        if record.fee_transaction_count > 0 {
            min_fee_zat = Some(
                min_fee_zat.map_or(record.min_zip317_conventional_fee_zat, |prior| {
                    prior.min(record.min_zip317_conventional_fee_zat)
                }),
            );
            max_fee_zat = Some(
                max_fee_zat.map_or(record.max_zip317_conventional_fee_zat, |prior| {
                    prior.max(record.max_zip317_conventional_fee_zat)
                }),
            );
        }
    }
    OverviewFeeSummary {
        block_count,
        transaction_count,
        total_zip317_conventional_fee_zat: total_fee_zat,
        min_zip317_conventional_fee_zat: min_fee_zat.unwrap_or(0),
        max_zip317_conventional_fee_zat: max_fee_zat.unwrap_or(0),
    }
}

fn read_recent_transactions_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    limit: u32,
    state: MaterializedViewState,
) -> Result<Vec<TransactionHistoryEntry>, Status> {
    let coverage = complete_overview_coverage("transaction-history", state)?;
    let start_key = [0u8; zinder_materialized_views::TRANSACTION_HISTORY_KEY_LEN];
    let end_key = [0xFFu8; zinder_materialized_views::TRANSACTION_HISTORY_KEY_LEN];
    let cap = usize::try_from(limit).unwrap_or(MAX_RECENT_TRANSACTIONS_LIMIT as usize);
    let rows = snapshot
        .range_iterate_consumer(TRANSACTION_HISTORY_COLUMN_FAMILY, &start_key, &end_key, cap)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut entries = Vec::with_capacity(rows.len());
    for (key, payload) in rows {
        let entry = decode_history_entry(&key, &payload)?;
        if entry.block_height > state.tip_height.value() {
            return Err(ExplorerError::unsatisfied_precondition(
                "overview transaction-history row is newer than its materialized-view state",
            )
            .into());
        }
        if entry.block_height < coverage.complete_from_height.value() {
            break;
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn read_mempool_event_counts_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    window_seconds: u32,
) -> Result<OverviewMempoolEvents, Status> {
    let now_seconds = current_unix_seconds();
    let window_start = now_seconds.saturating_sub(u64::from(window_seconds));
    let start_key = MempoolEventCountsConsumer::key_for_second(window_start);
    let end_key = MempoolEventCountsConsumer::key_for_second(now_seconds);
    let cap = usize::try_from(window_seconds).unwrap_or(MAX_MEMPOOL_WINDOW_SECONDS as usize);
    let entries = snapshot
        .range_iterate_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            cap,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut added_count: u32 = 0;
    let mut mined_count: u32 = 0;
    let mut invalidated_count: u32 = 0;
    for (_, payload) in entries {
        if let Some((added, mined, invalidated)) = MempoolEventCountsConsumer::decode_row(&payload)
        {
            added_count = added_count.saturating_add(added);
            mined_count = mined_count.saturating_add(mined);
            invalidated_count = invalidated_count.saturating_add(invalidated);
        }
    }
    Ok(OverviewMempoolEvents {
        window_seconds,
        added_count,
        mined_count,
        invalidated_count,
    })
}

fn overview_mempool(observation: &CompleteMempoolObservation) -> OverviewMempool {
    OverviewMempool {
        transaction_count: observation.summary.transaction_count,
        total_size_bytes: observation.summary.total_size_bytes,
        oldest_entry_age_millis: observation.summary.oldest_entry_age_millis,
        newest_entry_age_millis: observation.summary.newest_entry_age_millis,
    }
}

fn require_matching_wallet_epoch(
    expected: &wallet::ChainEpoch,
    actual: &wallet::ChainEpoch,
    observation: &str,
) -> Result<(), Status> {
    if actual != expected {
        return Err(ExplorerError::unsatisfied_precondition(format!(
            "{observation} chain epoch does not match the overview wallet anchor"
        ))
        .into());
    }
    Ok(())
}

fn require_mempool_identity(
    expected_epoch: &wallet::ChainEpoch,
    observation: &CompleteMempoolObservation,
) -> Result<(), Status> {
    require_matching_wallet_epoch(expected_epoch, &observation.chain_epoch, "mempool snapshot")?;
    let expected_tip = expected_epoch
        .visible_tip
        .as_ref()
        .ok_or_else(|| ExplorerError::internal("overview wallet anchor visible_tip missing"))?;
    if observation.source_tip != *expected_tip {
        return Err(ExplorerError::unsatisfied_precondition(
            "mempool source_tip does not match the overview wallet anchor",
        )
        .into());
    }
    Ok(())
}

fn require_value_pool_identity(
    expected_epoch: &wallet::ChainEpoch,
    response: &wallet::ChainValuePoolsAtTipResponse,
) -> Result<(), Status> {
    let actual_epoch = response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| {
            ExplorerError::internal("ChainValuePoolsAtTipResponse.chain_view.chain_epoch missing")
        })?;
    require_matching_wallet_epoch(expected_epoch, actual_epoch, "value-pool response")?;
    let expected_tip = expected_epoch
        .visible_tip
        .as_ref()
        .ok_or_else(|| ExplorerError::internal("overview wallet anchor visible_tip missing"))?;
    let source_tip = response.source_tip.as_ref().ok_or_else(|| {
        ExplorerError::internal("ChainValuePoolsAtTipResponse.source_tip missing")
    })?;
    if source_tip != expected_tip {
        return Err(ExplorerError::unsatisfied_precondition(
            "value-pool source_tip does not match the overview wallet anchor",
        )
        .into());
    }
    Ok(())
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

const fn clamp(requested: u32, default: u32, min: u32, max: u32) -> u32 {
    let target = if requested == 0 { default } else { requested };
    if target < min {
        min
    } else if target > max {
        max
    } else {
        target
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tonic::Code;
    use zinder_core::{BlockHash, ChainEpochId};
    use zinder_materialized_views::{
        MaterializedViewCoverage, MaterializedViewPreset, MaterializedViewStoreOptions,
        TransactionHistoryConsumer,
    };

    use super::*;

    fn state(
        chain_epoch_id: u64,
        tip_height: u32,
        tip_hash: BlockHash,
        complete_from_height: u32,
        revision: u64,
    ) -> MaterializedViewState {
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(chain_epoch_id),
            tip_height: BlockHeight::new(tip_height),
            tip_hash,
            revision,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(complete_from_height),
                complete_through_height: BlockHeight::new(tip_height),
                complete_through_hash: tip_hash,
            }),
        }
    }

    fn wallet_epoch(chain_epoch_id: u64, tip_height: u32, tip_hash: &str) -> wallet::ChainEpoch {
        wallet::ChainEpoch {
            chain_epoch_id,
            visible_tip: Some(wallet::BlockTip {
                height: tip_height,
                hash: tip_hash.to_owned(),
            }),
            ..Default::default()
        }
    }

    fn complete_mempool_observation(
        chain_epoch: wallet::ChainEpoch,
        source_tip: wallet::BlockTip,
    ) -> CompleteMempoolObservation {
        CompleteMempoolObservation {
            chain_epoch,
            source_tip,
            snapshot_age_millis: 0,
            summary: zinder_proto::v1::explorer::MempoolSnapshotSummary::default(),
        }
    }

    fn put_block_summary(
        store: &MaterializedViewStore,
        height: u32,
        block_hash: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let record = BlockSummaryRecord {
            summary: Some(BlockSummary {
                block_height: height,
                block_hash,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut payload = Vec::new();
        record.encode(&mut payload)?;
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(height)),
            &payload,
        )?;
        Ok(())
    }

    fn put_transaction_history_entry(
        store: &MaterializedViewStore,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let entry = TransactionHistoryEntry {
            transaction_id: format!("{height:064x}"),
            block_height: height,
            block_hash: format!("{height:064x}"),
            ..Default::default()
        };
        let mut payload = Vec::new();
        entry.encode(&mut payload)?;
        store.put_consumer(
            TRANSACTION_HISTORY_COLUMN_FAMILY,
            &TransactionHistoryConsumer::key_for_row(BlockHeight::new(height), 0),
            &payload,
        )?;
        Ok(())
    }

    #[test]
    fn request_limits_apply_defaults_and_server_bounds() {
        let defaults = RequestLimits::from_request(OverviewSnapshotRequest::default());
        assert_eq!(defaults.recent_blocks, DEFAULT_RECENT_BLOCKS_LIMIT);
        assert_eq!(
            defaults.recent_transactions,
            DEFAULT_RECENT_TRANSACTIONS_LIMIT
        );
        assert_eq!(
            defaults.mempool_window_seconds,
            DEFAULT_MEMPOOL_WINDOW_SECONDS
        );
        assert_eq!(defaults.fee_summary_blocks, DEFAULT_FEE_SUMMARY_BLOCK_COUNT);

        let bounded = RequestLimits::from_request(OverviewSnapshotRequest {
            recent_blocks_limit: u32::MAX,
            recent_transactions_limit: u32::MAX,
            mempool_window_seconds: 1,
            fee_summary_block_count: u32::MAX,
        });
        assert_eq!(bounded.recent_blocks, MAX_RECENT_BLOCKS_LIMIT);
        assert_eq!(bounded.recent_transactions, MAX_RECENT_TRANSACTIONS_LIMIT);
        assert_eq!(bounded.mempool_window_seconds, MIN_MEMPOOL_WINDOW_SECONDS);
        assert_eq!(bounded.fee_summary_blocks, MAX_FEE_SUMMARY_BLOCK_COUNT);
    }

    #[test]
    fn overview_rejects_unequal_consumer_coverage() -> Result<(), Box<dyn std::error::Error>> {
        let tip_hash = BlockHash::from_bytes([0x33; 32]);
        let error =
            validate_overview_state(state(7, 20, tip_hash, 5, 3), state(7, 20, tip_hash, 6, 4))
                .err()
                .ok_or("unequal coverage must not form one overview fence")?;
        assert_eq!(error.code(), Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    fn overview_rejects_coverage_that_omits_the_requested_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let coverage = MaterializedViewCoverage {
            complete_from_height: BlockHeight::new(10),
            complete_through_height: BlockHeight::new(20),
            complete_through_hash: BlockHash::from_bytes([0x33; 32]),
        };
        let error = require_overview_block_window_coverage(coverage, 5, 20)
            .err()
            .ok_or("partial requested windows must not be truncated")?;
        assert_eq!(error.code(), Code::NotFound);
        Ok(())
    }

    #[test]
    fn overview_rejects_materialized_view_state_change() -> Result<(), Box<dyn std::error::Error>> {
        let tip_hash = BlockHash::from_bytes([0x33; 32]);
        let candidate = OverviewMaterializedViewState {
            block_summary: state(7, 20, tip_hash, 5, 3),
            transaction_history: state(7, 20, tip_hash, 5, 4),
        };
        let final_state = OverviewMaterializedViewState {
            block_summary: state(7, 20, tip_hash, 5, 5),
            ..candidate
        };
        let error = require_unchanged_overview_state(candidate, final_state)
            .err()
            .ok_or("revision changes must invalidate the optimistic fence")?;
        assert_eq!(error.code(), Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    fn overview_rejects_mismatched_mempool_epoch_and_source_tip()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = wallet_epoch(7, 20, &"33".repeat(32));
        let wrong_epoch = complete_mempool_observation(
            wallet_epoch(8, 20, &"33".repeat(32)),
            expected
                .visible_tip
                .clone()
                .ok_or("test epoch has no visible tip")?,
        );
        assert_eq!(
            require_mempool_identity(&expected, &wrong_epoch)
                .err()
                .ok_or("mempool epoch mismatch must fail")?
                .code(),
            Code::FailedPrecondition
        );

        let wrong_source = complete_mempool_observation(
            expected.clone(),
            wallet::BlockTip {
                height: 19,
                hash: "22".repeat(32),
            },
        );
        assert_eq!(
            require_mempool_identity(&expected, &wrong_source)
                .err()
                .ok_or("mempool source-tip mismatch must fail")?
                .code(),
            Code::FailedPrecondition
        );
        Ok(())
    }

    #[test]
    fn overview_rejects_mismatched_value_pool_epoch_and_source_tip()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = wallet_epoch(7, 20, &"33".repeat(32));
        let wrong_epoch = wallet::ChainValuePoolsAtTipResponse {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(wallet_epoch(8, 20, &"33".repeat(32))),
                ..Default::default()
            }),
            source_tip: expected.visible_tip.clone(),
            ..Default::default()
        };
        assert_eq!(
            require_value_pool_identity(&expected, &wrong_epoch)
                .err()
                .ok_or("value-pool epoch mismatch must fail")?
                .code(),
            Code::FailedPrecondition
        );

        let wrong_source = wallet::ChainValuePoolsAtTipResponse {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(expected.clone()),
                ..Default::default()
            }),
            source_tip: Some(wallet::BlockTip {
                height: 19,
                hash: "22".repeat(32),
            }),
            ..Default::default()
        };
        assert_eq!(
            require_value_pool_identity(&expected, &wrong_source)
                .err()
                .ok_or("value-pool source-tip mismatch must fail")?
                .code(),
            Code::FailedPrecondition
        );
        Ok(())
    }

    #[test]
    fn overview_requires_exact_contiguous_block_rows() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open_with_materialized_view_preset(
            directory.path(),
            zinder_core::Network::ZcashRegtest,
            MaterializedViewPreset::Explorer,
            MaterializedViewStoreOptions::default(),
        )?;
        let tip_hash = BlockHash::from_bytes([0x33; 32]);
        put_block_summary(&store, 1, "11".repeat(32))?;
        put_block_summary(&store, 3, "33".repeat(32))?;
        let limits = RequestLimits {
            recent_blocks: 3,
            recent_transactions: 1,
            mempool_window_seconds: 60,
            fee_summary_blocks: 3,
        };
        let materialized_state = state(7, 3, tip_hash, 1, 1);

        let snapshot = store.read_snapshot();
        let error = read_block_summary_records_snapshot(&snapshot, 3, &limits, materialized_state)
            .err()
            .ok_or("a missing middle row must fail the complete window")?;
        assert_eq!(error.code(), Code::NotFound);
        drop(snapshot);

        put_block_summary(&store, 2, "22".repeat(32))?;
        let snapshot = store.read_snapshot();
        let records =
            read_block_summary_records_snapshot(&snapshot, 3, &limits, materialized_state)?;
        drop(snapshot);
        assert_eq!(
            records
                .iter()
                .filter_map(|record| record.summary.as_ref())
                .map(|summary| summary.block_height)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        Ok(())
    }

    #[test]
    fn overview_recent_transactions_stop_at_verified_coverage()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open_with_materialized_view_preset(
            directory.path(),
            zinder_core::Network::ZcashRegtest,
            MaterializedViewPreset::Explorer,
            MaterializedViewStoreOptions::default(),
        )?;
        for height in [8, 9, 10] {
            put_transaction_history_entry(&store, height)?;
        }
        let tip_hash = BlockHash::from_bytes([0x33; 32]);
        let materialized_state = state(7, 10, tip_hash, 9, 1);

        let snapshot = store.read_snapshot();
        let entries = read_recent_transactions_snapshot(&snapshot, 3, materialized_state)?;
        drop(snapshot);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.block_height)
                .collect::<Vec<_>>(),
            vec![10, 9],
        );
        Ok(())
    }
}
