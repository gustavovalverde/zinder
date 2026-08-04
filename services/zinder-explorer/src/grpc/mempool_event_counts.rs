//! `ExplorerQuery.MempoolEventCounts` handler.
//!
//! Reads the per-second counter rows written by
//! [`zinder_materialized_views::MempoolEventCountsConsumer`]
//! and aggregates them across the requested window.

use std::time::{SystemTime, UNIX_EPOCH};

use tonic::{Request, Response, Status};
use zinder_proto::capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1;
use zinder_proto::v1::explorer::{
    ExplorerFreshness, MempoolEventCountsRequest, MempoolEventCountsResponse,
};
use zinder_proto::v1::wallet::ChainView;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, read_materialized_view_status_snapshot,
};
use zinder_materialized_views::{
    MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, MaterializedViewStore, MaterializedViewStoreReadSnapshot,
    MempoolEventCountsConsumer,
};

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
#[allow(
    clippy::significant_drop_tightening,
    reason = "one local snapshot must span the complete event window and response freshness"
)]
pub(crate) async fn query_mempool_event_counts(
    materialized_view_store: &MaterializedViewStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MempoolEventCountsRequest>,
) -> Result<Response<MempoolEventCountsResponse>, Status> {
    let inner = request.into_inner();
    let window_seconds = clamp_window(inner.window_seconds);
    let now_seconds = current_unix_seconds();
    let (added_count, mined_count, invalidated_count, freshness) = {
        let snapshot = materialized_view_store
            .read_snapshot()
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        read_mempool_event_counts_snapshot(&snapshot, now_seconds, window_seconds)?
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    Ok(Response::new(MempoolEventCountsResponse {
        freshness: Some(freshness),
        window_seconds,
        added_count,
        mined_count,
        invalidated_count,
    }))
}

/// Reads one complete bounded event-count window from an immutable snapshot.
///
/// Keeping the window bounds, bucket rows, and freshness in one local read
/// unit prevents a later materialized-view commit from changing one response
/// axis after another has already been observed.
fn read_mempool_event_counts_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    now_seconds: u64,
    window_seconds: u32,
) -> Result<(u32, u32, u32, ExplorerFreshness), Status> {
    let (window_start, window_end) = mempool_event_count_window_bounds(now_seconds, window_seconds);
    let start_key = MempoolEventCountsConsumer::key_for_second(window_start);
    let end_key = MempoolEventCountsConsumer::key_for_second(window_end);
    let entries = snapshot
        .range_iterate_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_ROWS_PER_REQUEST,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let (added_count, mined_count, invalidated_count) =
        aggregate_mempool_event_count_entries(entries)?;
    let freshness = build_mempool_event_counts_freshness(snapshot)?;
    Ok((added_count, mined_count, invalidated_count, freshness))
}

/// Returns the inclusive second-bucket interval for an exact requested window.
///
/// A `window_seconds` value of 60 includes exactly 60 buckets: `now - 59`
/// through `now`. The underflow case is clipped to the Unix epoch.
fn mempool_event_count_window_bounds(now_seconds: u64, window_seconds: u32) -> (u64, u64) {
    let preceding_seconds = u64::from(window_seconds.saturating_sub(1));
    (now_seconds.saturating_sub(preceding_seconds), now_seconds)
}

/// Aggregates one snapshot's already-bounded bucket rows.
///
/// Persisted rows are a typed materialized-view contract. A malformed payload
/// therefore fails the whole request rather than silently undercounting it.
fn aggregate_mempool_event_count_entries(
    entries: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<(u32, u32, u32), Status> {
    let mut added_count = 0u32;
    let mut mined_count = 0u32;
    let mut invalidated_count = 0u32;
    for (_, payload) in entries {
        let (added, mined, invalidated) = MempoolEventCountsConsumer::decode_row(&payload)
            .ok_or_else(|| {
                ExplorerError::internal("Mempool Event Counts persisted bucket row is malformed")
            })?;
        added_count = added_count.saturating_add(added);
        mined_count = mined_count.saturating_add(mined);
        invalidated_count = invalidated_count.saturating_add(invalidated);
    }
    Ok((added_count, mined_count, invalidated_count))
}

/// Builds the Mempool Event Counts freshness envelope from its own read snapshot.
///
/// This endpoint has no Wallet or Block Summary dependency. Its chain view may
/// therefore carry only the persisted materialized-view status from the exact
/// snapshot that supplied the event buckets. The background observation axis
/// is attached after the snapshot has been released.
fn build_mempool_event_counts_freshness(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<ExplorerFreshness, Status> {
    let materialized_views = read_materialized_view_status_snapshot(snapshot)?;
    Ok(mempool_event_counts_freshness_from_status(
        materialized_views,
    ))
}

fn mempool_event_counts_freshness_from_status(
    materialized_views: Option<zinder_proto::v1::wallet::MaterializedViewStatus>,
) -> ExplorerFreshness {
    ExplorerFreshness {
        chain_view: materialized_views.map(|materialized_views| ChainView {
            chain_epoch: None,
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: Some(materialized_views),
        }),
        snapshot_age_millis: 0,
        capability_version: EXPLORER_MEMPOOL_EVENT_COUNTS_V1.to_owned(),
        unavailable: Vec::new(),
    }
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

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use prost::Message as _;
    use tempfile::tempdir;
    use zinder_materialized_views::{MEMPOOL_EVENT_COUNTS_SCHEMA, MaterializedViewStoreOptions};
    use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};
    use zinder_store::RocksDbResourceBudget;

    fn encoded_bucket(added: u32, mined: u32, invalidated: u32) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&added.to_be_bytes());
        bytes[4..8].copy_from_slice(&mined.to_be_bytes());
        bytes[8..12].copy_from_slice(&invalidated.to_be_bytes());
        bytes
    }

    fn status(indexed_height: u32, observed_at_millis: u64) -> MaterializedViewStatus {
        MaterializedViewStatus {
            health: MaterializedViewHealth::Live as i32,
            indexed_height,
            lag_blocks: 0,
            observed_at_millis,
        }
    }

    #[test]
    fn mempool_event_counts_freshness_shape_omits_wallet_and_block_summary_axes()
    -> Result<(), &'static str> {
        let freshness = mempool_event_counts_freshness_from_status(Some(
            zinder_proto::v1::wallet::MaterializedViewStatus::default(),
        ));
        let Some(chain_view) = freshness.chain_view.as_ref() else {
            return Err("materialized-view status must keep the local chain view present");
        };

        assert!(chain_view.chain_epoch.is_none());
        assert!(chain_view.indexed_tip.is_none());
        assert!(chain_view.materialized_views.is_some());
        Ok(())
    }

    #[test]
    fn mempool_event_counts_window_includes_exactly_the_minimum_bucket_count() {
        let (start, end) = mempool_event_count_window_bounds(1_000, MIN_WINDOW_SECONDS);
        assert_eq!(end - start + 1, u64::from(MIN_WINDOW_SECONDS));
        assert_eq!(start, 941);
        assert_eq!(end, 1_000);
    }

    #[test]
    fn mempool_event_counts_window_includes_exactly_the_maximum_bucket_count() {
        let (start, end) = mempool_event_count_window_bounds(10_000, MAX_WINDOW_SECONDS);
        assert_eq!(end - start + 1, u64::from(MAX_WINDOW_SECONDS));
        assert_eq!(start, 6_401);
        assert_eq!(end, 10_000);
    }

    #[test]
    fn mempool_event_counts_rejects_a_malformed_persisted_bucket() -> Result<(), &'static str> {
        let error = aggregate_mempool_event_count_entries(vec![(
            b"bucket".to_vec(),
            b"malformed".to_vec(),
        )])
        .err()
        .ok_or("malformed bucket must fail")?;
        assert_eq!(error.code(), tonic::Code::Internal);
        Ok(())
    }

    #[test]
    fn mempool_event_counts_snapshot_retains_e1_totals_and_freshness_after_e2_write()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            zinder_testkit::published_regtest_canonical_construction_identity()?,
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: &[MEMPOOL_EVENT_COUNTS_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        let bucket_key = MempoolEventCountsConsumer::key_for_second(1_000);
        let e1_status = status(100, 1_000);
        store.put_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &bucket_key,
            &encoded_bucket(2, 3, 5),
        )?;
        store.put_materialized_view_status(&e1_status.encode_to_vec())?;

        let e1_snapshot = store.read_snapshot()?;

        let e2_status = status(101, 1_001);
        store.put_consumer(
            MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
            &bucket_key,
            &encoded_bucket(7, 11, 13),
        )?;
        store.put_materialized_view_status(&e2_status.encode_to_vec())?;

        let (added, mined, invalidated, freshness) =
            read_mempool_event_counts_snapshot(&e1_snapshot, 1_000, MIN_WINDOW_SECONDS)?;
        assert_eq!((added, mined, invalidated), (2, 3, 5));
        assert_eq!(
            freshness
                .chain_view
                .and_then(|chain_view| chain_view.materialized_views)
                .map(|snapshot_status| snapshot_status.observed_at_millis),
            Some(e1_status.observed_at_millis)
        );
        Ok(())
    }
}
