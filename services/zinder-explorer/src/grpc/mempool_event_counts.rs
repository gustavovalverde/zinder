//! `ExplorerQuery.MempoolEventCounts` handler.
//!
//! Reads the per-second counter rows written by
//! [`zinder_derive::MempoolEventCountsConsumer`]
//! and aggregates them across the requested window.

use std::time::{SystemTime, UNIX_EPOCH};

use tonic::{Request, Response, Status};
use zinder_proto::capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1;
use zinder_proto::v1::explorer::{
    ExplorerFreshness, MempoolEventCountsRequest, MempoolEventCountsResponse,
};
use zinder_proto::v1::wallet::{LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use zinder_derive::{DeriveStore, MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, MempoolEventCountsConsumer};

/// Minimum window size accepted by the handler.
const MIN_WINDOW_SECONDS: u32 = 60;

/// Maximum window size accepted by the handler.
const MAX_WINDOW_SECONDS: u32 = 3_600;

/// Default window when the caller passes `window_seconds = 0`.
const DEFAULT_WINDOW_SECONDS: u32 = 300;

/// Upper bound on rows the aggregation reads from the column family per
/// request. Matches the max window in seconds (one row per bucket).
const MAX_ROWS_PER_REQUEST: usize = MAX_WINDOW_SECONDS as usize;

/// Executes one `MempoolEventCounts` request.
pub(crate) async fn handle_mempool_event_counts(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: Request<MempoolEventCountsRequest>,
) -> Result<Response<MempoolEventCountsResponse>, Status> {
    let inner = request.into_inner();
    let window_seconds = clamp_window(inner.window_seconds);
    let now_seconds = current_unix_seconds();
    let window_start = now_seconds.saturating_sub(u64::from(window_seconds));
    let start_key = MempoolEventCountsConsumer::key_for_second(window_start);
    let end_key = MempoolEventCountsConsumer::key_for_second(now_seconds);
    let entries = derive_store
        .range_iterate_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_ROWS_PER_REQUEST,
        )
        .map_err(|error| Status::internal(error.to_string()))?;

    let mut added_count = 0u32;
    let mut mined_count = 0u32;
    let mut invalidated_count = 0u32;
    let mut suppressed_count = 0u32;
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

    let latest = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch: None }))
        .await?
        .into_inner();
    let chain_epoch = latest
        .chain_epoch
        .ok_or_else(|| Status::internal("LatestBlockResponse.chain_epoch missing"))?;

    Ok(Response::new(MempoolEventCountsResponse {
        freshness: Some(ExplorerFreshness {
            chain_epoch: Some(chain_epoch),
            snapshot_age_millis: 0,
            derive_cursor_lag_blocks: 0,
            derive_cursor_lag_millis: 0,
            capability_version: EXPLORER_MEMPOOL_EVENT_COUNTS_V1.to_owned(),
            unavailable: Vec::new(),
        }),
        window_seconds,
        added_count,
        mined_count,
        invalidated_count,
        suppressed_count,
    }))
}

const fn clamp_window(requested: u32) -> u32 {
    let target = if requested == 0 {
        DEFAULT_WINDOW_SECONDS
    } else {
        requested
    };
    if target < MIN_WINDOW_SECONDS {
        MIN_WINDOW_SECONDS
    } else if target > MAX_WINDOW_SECONDS {
        MAX_WINDOW_SECONDS
    } else {
        target
    }
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
