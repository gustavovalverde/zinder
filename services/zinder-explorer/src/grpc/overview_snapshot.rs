//! `ExplorerQuery.OverviewSnapshot` handler.
//!
//! Composes one coherent point-in-time bundle that overview consumers
//! used to assemble from six independent RPCs (`MempoolSummary` +
//! `FeeSummary` + `ValuePoolSummary` + `BlockSummariesInRange` +
//! `RecentTransactions` + `MempoolEventCounts`). Consumer-side fan-out
//! lets per-card freshness diverge: the tile claims height N while the
//! list shows height N+50 because the calls land on different RPC
//! windows. This handler instead anchors every sub-field to the same
//! `WalletQuery.LatestBlock` tip and reads every derive-store
//! column-family in one pass, so the response carries one
//! `ExplorerFreshness`. The `freshness.chain_epoch.tip_hash` is the
//! bundle's snapshot identity; two responses with the same `tip_hash`
//! are guaranteed to have been read against the same upstream snapshot.

use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tonic::{Code, Request, Response, Status};
use zinder_core::BlockHeight;
use zinder_core::wire::encode_height_key_ascending;
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore, MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
    MempoolEventCountsConsumer, RECENT_TRANSACTIONS_COLUMN_FAMILY,
};
use zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1;
use zinder_proto::v1::explorer::{
    BlockSummary, BlockSummaryRecord, OverviewFeeSummary, OverviewMempool, OverviewMempoolEvents,
    OverviewSnapshotRequest, OverviewSnapshotResponse, RecentTransactionEntry,
};
use zinder_proto::v1::wallet::{
    self, ChainValuePoolsAtTipRequest, LatestBlockRequest, MempoolSnapshotRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

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

/// Cap on entries the bundle's mempool aggregation hydrates from the
/// wallet `MempoolSnapshot`. Mirrors the cap in the per-feature mempool
/// handlers.
const MAX_MEMPOOL_SNAPSHOT_ENTRIES: u32 = 4_096;

/// Length of one row key in the `recent_transactions` projection
/// (reverse-height encoded; see `RecentTransactionsConsumer`).
const RECENT_TRANSACTIONS_ROW_KEY_LEN: usize = 8;

/// Executes one `ExplorerQuery.OverviewSnapshot` request.
pub(crate) async fn handle_overview_snapshot(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<OverviewSnapshotRequest>,
) -> Result<Response<OverviewSnapshotResponse>, Status> {
    let limits = RequestLimits::from_request(request.into_inner());
    let anchor = anchor_to_wallet_tip(wallet_client).await?;
    let block_records = read_block_summary_records(derive_store, anchor.tip_height, &limits)?;
    let recent_blocks =
        collect_recent_blocks(&block_records, limits.recent_blocks, anchor.tip_height);
    let fee_summary = aggregate_fee_summary(&block_records, limits.fee_summary_blocks);
    let tip_block_time_unix_seconds = recent_blocks
        .first()
        .map_or(0, |summary| summary.block_time_unix_seconds);
    let recent_transactions = read_recent_transactions(derive_store, limits.recent_transactions)?;
    let mempool_events = read_mempool_event_counts(derive_store, limits.mempool_window_seconds)?;
    let mempool = aggregate_mempool_summary(wallet_client).await?;
    let value_pools = read_value_pools(wallet_client).await?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_OVERVIEW_SNAPSHOT_V1,
            Some(anchor.chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(OverviewSnapshotResponse {
        freshness: Some(freshness),
        tip_block_time_unix_seconds,
        mempool: Some(mempool),
        mempool_events: Some(mempool_events),
        fee_summary: Some(fee_summary),
        value_pools,
        recent_blocks,
        recent_transactions,
    }))
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

async fn anchor_to_wallet_tip(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<WalletAnchor, Status> {
    let response = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch: None }))
        .await?
        .into_inner();
    let chain_epoch = response
        .chain_epoch
        .ok_or_else(|| Status::internal("LatestBlockResponse.chain_epoch missing"))?;
    let tip_height = response
        .latest_block
        .ok_or_else(|| Status::internal("LatestBlockResponse.latest_block missing"))?
        .height;
    Ok(WalletAnchor {
        chain_epoch,
        tip_height,
    })
}

fn read_block_summary_records(
    derive_store: &DeriveStore,
    canonical_tip_height: u32,
    limits: &RequestLimits,
) -> Result<Vec<BlockSummaryRecord>, Status> {
    let window = limits.block_window_count();
    let start_height = canonical_tip_height.saturating_sub(window.saturating_sub(1));
    let start_key = encode_height_key_ascending(BlockHeight::new(start_height));
    let end_key = encode_height_key_ascending(BlockHeight::new(canonical_tip_height));
    let cap = usize::try_from(window).unwrap_or(MAX_FEE_SUMMARY_BLOCK_COUNT as usize);
    let entries = derive_store
        .range_iterate_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &start_key, &end_key, cap)
        .map_err(|error| Status::internal(error.to_string()))?;
    let mut records = Vec::with_capacity(entries.len());
    for (_, payload) in entries {
        let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
            Status::internal(format!("BlockSummaryRecord decode failed: {error}"))
        })?;
        records.push(record);
    }
    Ok(records)
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

fn read_recent_transactions(
    derive_store: &DeriveStore,
    limit: u32,
) -> Result<Vec<RecentTransactionEntry>, Status> {
    let start_key = [0u8; RECENT_TRANSACTIONS_ROW_KEY_LEN];
    let end_key = [0xFFu8; RECENT_TRANSACTIONS_ROW_KEY_LEN];
    let cap = usize::try_from(limit).unwrap_or(MAX_RECENT_TRANSACTIONS_LIMIT as usize);
    let rows = derive_store
        .range_iterate_consumer(RECENT_TRANSACTIONS_COLUMN_FAMILY, &start_key, &end_key, cap)
        .map_err(|error| Status::internal(error.to_string()))?;
    let mut entries = Vec::with_capacity(rows.len());
    for (_, payload) in rows {
        let entry = RecentTransactionEntry::decode(payload.as_slice()).map_err(|error| {
            Status::internal(format!("RecentTransactionEntry decode failed: {error}"))
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

fn read_mempool_event_counts(
    derive_store: &DeriveStore,
    window_seconds: u32,
) -> Result<OverviewMempoolEvents, Status> {
    let now_seconds = current_unix_seconds();
    let window_start = now_seconds.saturating_sub(u64::from(window_seconds));
    let start_key = MempoolEventCountsConsumer::key_for_second(window_start);
    let end_key = MempoolEventCountsConsumer::key_for_second(now_seconds);
    let cap = usize::try_from(window_seconds).unwrap_or(MAX_MEMPOOL_WINDOW_SECONDS as usize);
    let entries = derive_store
        .range_iterate_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            cap,
        )
        .map_err(|error| Status::internal(error.to_string()))?;
    let mut added_count: u32 = 0;
    let mut mined_count: u32 = 0;
    let mut invalidated_count: u32 = 0;
    let mut suppressed_count: u32 = 0;
    for (_, payload) in entries {
        if let Some((added, mined, invalidated, suppressed)) =
            MempoolEventCountsConsumer::decode_row(&payload)
        {
            added_count = added_count.saturating_add(added);
            mined_count = mined_count.saturating_add(mined);
            invalidated_count = invalidated_count.saturating_add(invalidated);
            suppressed_count = suppressed_count.saturating_add(suppressed);
        }
    }
    Ok(OverviewMempoolEvents {
        window_seconds,
        added_count,
        mined_count,
        invalidated_count,
        suppressed_count,
    })
}

async fn aggregate_mempool_summary(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<OverviewMempool, Status> {
    // Best-effort: if the wallet's MempoolSnapshot is wired off (e.g.
    // the ingest-control proxy is not configured) the bundle still
    // returns with zero mempool counts rather than failing the entire
    // overview. Surfacing "tried but could not observe" via
    // `freshness.unavailable` is a v2 follow-up.
    let snapshot = match wallet_client
        .mempool_snapshot(Request::new(MempoolSnapshotRequest {
            max_entries: MAX_MEMPOOL_SNAPSHOT_ENTRIES,
            from_cursor: Vec::new(),
        }))
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) if status.code() == Code::Unavailable => return Ok(OverviewMempool::default()),
        Err(status) => return Err(status),
    };
    let now_millis = current_unix_millis();
    let mut transaction_count: u32 = 0;
    let mut total_size_bytes: u64 = 0;
    let mut oldest_first_seen: Option<u64> = None;
    let mut newest_first_seen: Option<u64> = None;
    for entry in &snapshot.entries {
        transaction_count = transaction_count.saturating_add(1);
        let entry_size = u64::try_from(entry.raw_transaction_bytes.len()).unwrap_or(u64::MAX);
        total_size_bytes = total_size_bytes.saturating_add(entry_size);
        oldest_first_seen = Some(
            oldest_first_seen.map_or(entry.first_seen_unix_millis, |prior| {
                prior.min(entry.first_seen_unix_millis)
            }),
        );
        newest_first_seen = Some(
            newest_first_seen.map_or(entry.first_seen_unix_millis, |prior| {
                prior.max(entry.first_seen_unix_millis)
            }),
        );
    }
    Ok(OverviewMempool {
        transaction_count,
        total_size_bytes,
        oldest_entry_age_millis: oldest_first_seen
            .map_or(0, |seen| now_millis.saturating_sub(seen)),
        newest_entry_age_millis: newest_first_seen
            .map_or(0, |seen| now_millis.saturating_sub(seen)),
    })
}

async fn read_value_pools(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<Vec<wallet::ChainValuePool>, Status> {
    // Best-effort parallel to `aggregate_mempool_summary`: the bundle
    // returns with empty pool data rather than failing when the wallet's
    // `ChainValuePoolsAtTip` is unavailable upstream.
    match wallet_client
        .chain_value_pools_at_tip(Request::new(ChainValuePoolsAtTipRequest {}))
        .await
    {
        Ok(response) => Ok(response.into_inner().pools),
        Err(status) if status.code() == Code::Unavailable => Ok(Vec::new()),
        Err(status) => Err(status),
    }
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
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
