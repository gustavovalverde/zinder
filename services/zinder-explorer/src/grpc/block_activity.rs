//! Bounded request-time block-activity aggregation.
//!
//! The handler reads existing `BlockSummary` projection rows and groups their
//! block and transaction counts into a complete weekday/hour grid. It owns no
//! projection and reports coverage explicitly when the requested range has
//! missing materialized rows.

use prost::Message as _;
use time::OffsetDateTime;
use tonic::{Request, Response, Status};
use zinder_core::{BlockHeight, wire::encode_height_key_ascending};
use zinder_materialized_views::{BLOCK_SUMMARY_COLUMN_FAMILY, MaterializedViewStore};
use zinder_proto::capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1;
use zinder_proto::v1::explorer::{
    BlockActivityBucket, BlockActivityDistributionRequest, BlockActivityDistributionResponse,
    BlockSummary, BlockSummaryRecord,
};
use zinder_proto::v1::wallet::{self, LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

/// Server-side ceiling for one request-time activity aggregate.
///
/// A larger history needs a dedicated durable projection with explicit reorg
/// and retention semantics rather than an unbounded read-time scan.
const MAX_BLOCK_ACTIVITY_DISTRIBUTION_BLOCKS: u32 = 20_000;
const WEEKDAYS_PER_WEEK: usize = 7;
const HOURS_PER_DAY: usize = 24;
const ACTIVITY_BUCKET_COUNT: usize = WEEKDAYS_PER_WEEK * HOURS_PER_DAY;

/// Executes one `ExplorerQuery.BlockActivityDistribution` request.
pub(crate) async fn handle_block_activity_distribution(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockActivityDistributionRequest>,
) -> Result<Response<BlockActivityDistributionResponse>, Status> {
    let request = request.into_inner();
    let requested_block_count = validate_requested_range(request.start_height, request.end_height)?;
    let (chain_epoch, _) = read_canonical_tip(wallet_client).await?;
    let start_key = encode_height_key_ascending(BlockHeight::new(request.start_height));
    let end_key = encode_height_key_ascending(BlockHeight::new(request.end_height));
    let entries = materialized_view_store
        .range_iterate_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            usize::try_from(requested_block_count).unwrap_or(usize::MAX),
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let materialized_block_count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let summaries = entries
        .into_iter()
        .map(|(_, payload)| {
            let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
                ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
            })?;
            record
                .summary
                .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let aggregate = aggregate_block_activity(&summaries)?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockActivityDistributionResponse {
        freshness: Some(freshness),
        start_height: request.start_height,
        end_height: request.end_height,
        materialized_block_count,
        missing_block_count: requested_block_count.saturating_sub(materialized_block_count),
        first_block_time_unix_seconds: aggregate.first_block_time_unix_seconds,
        last_block_time_unix_seconds: aggregate.last_block_time_unix_seconds,
        transaction_count: aggregate.transaction_count,
        buckets: aggregate.into_buckets(),
    }))
}

fn validate_requested_range(start_height: u32, end_height: u32) -> Result<u32, Status> {
    if end_height < start_height {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_BLOCK_ACTIVITY_DISTRIBUTION_BLOCKS) {
        return Err(ExplorerError::invalid_request(format!(
            "requested span {span} blocks exceeds the per-request cap of \
             {MAX_BLOCK_ACTIVITY_DISTRIBUTION_BLOCKS}",
        ))
        .into());
    }
    Ok(u32::try_from(span).unwrap_or(MAX_BLOCK_ACTIVITY_DISTRIBUTION_BLOCKS))
}

async fn read_canonical_tip(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<(wallet::ChainEpoch, u32), Status> {
    let latest = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner();
    let chain_epoch = latest
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing")
        })?;
    let canonical_tip = latest
        .latest_block
        .ok_or_else(|| ExplorerError::internal("LatestBlockResponse.latest_block missing"))?
        .height;
    Ok((chain_epoch, canonical_tip))
}

#[derive(Clone, Copy, Default)]
struct ActivityCounts {
    block_count: u32,
    transaction_count: u64,
}

struct BlockActivityAggregate {
    buckets: [ActivityCounts; ACTIVITY_BUCKET_COUNT],
    first_block_time_unix_seconds: Option<i64>,
    last_block_time_unix_seconds: Option<i64>,
    transaction_count: u64,
}

impl BlockActivityAggregate {
    fn new() -> Self {
        Self {
            buckets: [ActivityCounts::default(); ACTIVITY_BUCKET_COUNT],
            first_block_time_unix_seconds: None,
            last_block_time_unix_seconds: None,
            transaction_count: 0,
        }
    }

    fn add_summary(&mut self, summary: &BlockSummary) -> Result<(), Status> {
        let timestamp = OffsetDateTime::from_unix_timestamp(summary.block_time_unix_seconds)
            .map_err(|error| {
                ExplorerError::internal(format!(
                    "BlockSummary at height {} has an invalid block time: {error}",
                    summary.block_height
                ))
            })?;
        let weekday = usize::from(timestamp.weekday().number_days_from_sunday());
        let hour = usize::from(timestamp.hour());
        let counts = &mut self.buckets[weekday * HOURS_PER_DAY + hour];
        counts.block_count = counts.block_count.saturating_add(1);
        counts.transaction_count = counts
            .transaction_count
            .saturating_add(u64::from(summary.transaction_count));
        self.transaction_count = self
            .transaction_count
            .saturating_add(u64::from(summary.transaction_count));
        self.first_block_time_unix_seconds = Some(
            self.first_block_time_unix_seconds
                .map_or(summary.block_time_unix_seconds, |current| {
                    current.min(summary.block_time_unix_seconds)
                }),
        );
        self.last_block_time_unix_seconds = Some(
            self.last_block_time_unix_seconds
                .map_or(summary.block_time_unix_seconds, |current| {
                    current.max(summary.block_time_unix_seconds)
                }),
        );
        Ok(())
    }

    fn into_buckets(self) -> Vec<BlockActivityBucket> {
        let mut buckets = Vec::with_capacity(ACTIVITY_BUCKET_COUNT);
        for weekday in 0..WEEKDAYS_PER_WEEK {
            for hour in 0..HOURS_PER_DAY {
                let counts = self.buckets[weekday * HOURS_PER_DAY + hour];
                buckets.push(BlockActivityBucket {
                    weekday: u32::try_from(weekday).unwrap_or(u32::MAX),
                    hour: u32::try_from(hour).unwrap_or(u32::MAX),
                    transaction_count: counts.transaction_count,
                    block_count: counts.block_count,
                });
            }
        }
        buckets
    }
}

fn aggregate_block_activity(summaries: &[BlockSummary]) -> Result<BlockActivityAggregate, Status> {
    let mut aggregate = BlockActivityAggregate::new();
    for summary in summaries {
        aggregate.add_summary(summary)?;
    }
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::aggregate_block_activity;
    use zinder_proto::v1::explorer::BlockSummary;

    #[test]
    fn block_activity_uses_sunday_zero_and_emits_the_complete_grid()
    -> Result<(), Box<dyn std::error::Error>> {
        let aggregate = aggregate_block_activity(&[
            BlockSummary {
                block_time_unix_seconds: 1_736_035_200,
                transaction_count: 2,
                ..Default::default()
            },
            BlockSummary {
                block_time_unix_seconds: 1_736_046_000,
                transaction_count: 3,
                ..Default::default()
            },
        ])?;
        let first_block_time = aggregate.first_block_time_unix_seconds;
        let last_block_time = aggregate.last_block_time_unix_seconds;
        let transaction_count = aggregate.transaction_count;
        let buckets = aggregate.into_buckets();

        assert_eq!(buckets.len(), 168);
        assert_eq!(buckets[0].weekday, 0);
        assert_eq!(buckets[0].hour, 0);
        assert_eq!(buckets[3].transaction_count, 3);
        assert_eq!(buckets[3].block_count, 1);
        assert_eq!(buckets[4].transaction_count, 0);
        assert_eq!(first_block_time, Some(1_736_035_200));
        assert_eq!(last_block_time, Some(1_736_046_000));
        assert_eq!(transaction_count, 5);
        Ok(())
    }
}
