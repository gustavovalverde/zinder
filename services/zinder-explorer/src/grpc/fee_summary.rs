//! `ExplorerQuery.FeeSummary` handler.
//!
//! Aggregates per-transaction ZIP-317 conventional fee floors over an
//! inclusive block range from the typed `BlockSummaryRecord` rows
//! materialized by the materialized-view plane. Coinbase transactions are excluded
//! because they have no fee.
//!
//! The fee fields are ZIP-317 conventional fee floors, not
//! miner-collected fees. Computing actual fees requires prevout
//! resolution and is out of scope for v1; the conventional-fee floor
//! is the minimum a wallet should attach to a transaction with the
//! given shape.

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_core::BlockHeight;
use zinder_core::wire::encode_height_key_ascending;
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, MaterializedViewState, MaterializedViewStore,
    MaterializedViewStoreReadSnapshot,
};
use zinder_proto::capabilities::EXPLORER_FEE_SUMMARY_V1;
use zinder_proto::v1::explorer::{BlockSummaryRecord, FeeSummaryRequest, FeeSummaryResponse};
use zinder_proto::v1::wallet::wallet_query_client::WalletQueryClient;
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness_from_snapshot,
    pin_wallet_to_block_summary_snapshot, require_block_summary_range_coverage,
};

/// Hard cap on the blocks one `FeeSummary` request aggregates.
///
/// The wire response is a single aggregate over a contiguous window; the cap
/// bounds one request's materialized-view scan.
const MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST: u32 = 256;

/// Executes one `ExplorerQuery.FeeSummary` request.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the Wallet-pinned snapshot must span aggregation and the response freshness fence"
)]
pub(crate) async fn query_fee_summary(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<FeeSummaryRequest>,
) -> Result<Response<FeeSummaryResponse>, Status> {
    let inner = request.into_inner();
    validate_range(inner.start_height, inner.end_height)?;
    let (aggregate, freshness) = {
        let pinned_snapshot =
            pin_wallet_to_block_summary_snapshot(materialized_view_store, wallet_client).await?;
        let state = pinned_snapshot.block_summary_state();
        require_block_summary_range_coverage(state, inner.start_height, inner.end_height)?;
        let aggregate = aggregate_block_summaries(
            pinned_snapshot.snapshot(),
            inner.start_height,
            inner.end_height,
            state,
        )?;
        let freshness = build_explorer_freshness_from_snapshot(
            pinned_snapshot.snapshot(),
            EXPLORER_FEE_SUMMARY_V1,
            Some(pinned_snapshot.wallet_chain_epoch().clone()),
            0,
        )?;
        (aggregate, freshness)
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    let response = FeeSummaryResponse {
        freshness: Some(freshness),
        block_count: aggregate.block_count,
        transaction_count: aggregate.transaction_count,
        total_zip317_conventional_fee_zat: aggregate.total_fee_zat,
        min_zip317_conventional_fee_zat: aggregate.min_fee_zat.unwrap_or(0),
        max_zip317_conventional_fee_zat: aggregate.max_fee_zat.unwrap_or(0),
    };
    Ok(Response::new(response))
}

fn validate_range(start_height: u32, end_height: u32) -> Result<(), Status> {
    if end_height < start_height {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST) {
        return Err(ExplorerError::invalid_request(format!(
            "requested span {span} blocks exceeds the per-request cap of \
             {MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST}",
        ))
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct FeeAggregate {
    block_count: u32,
    transaction_count: u32,
    total_fee_zat: u64,
    min_fee_zat: Option<u64>,
    max_fee_zat: Option<u64>,
}

fn aggregate_block_summaries(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    start_height: u32,
    end_height: u32,
    state: MaterializedViewState,
) -> Result<FeeAggregate, Status> {
    let expected_heights = (start_height..=end_height).collect::<Vec<_>>();
    let keys = expected_heights
        .iter()
        .map(|height| encode_height_key_ascending(BlockHeight::new(*height)))
        .collect::<Vec<_>>();
    let entries = snapshot
        .multi_get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &keys)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;

    let mut aggregate = FeeAggregate::default();
    for (payload, expected_height) in entries.into_iter().zip(expected_heights) {
        let payload = payload.ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "BlockSummary is not materialized for height {expected_height}"
            ))
        })?;
        let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
            ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
        })?;
        let summary = record
            .summary
            .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
        if summary.block_height != expected_height {
            return Err(ExplorerError::internal(format!(
                "BlockSummaryRecord at height {expected_height} carries height {}",
                summary.block_height
            ))
            .into());
        }
        if expected_height == state.tip_height.value()
            && summary.block_hash != zinder_core::wire::encode_rpc_block_hash_hex(state.tip_hash)
        {
            return Err(ExplorerError::unsatisfied_precondition(
                "block-summary tip row does not match its materialized-view state",
            )
            .into());
        }
        aggregate.block_count = aggregate.block_count.saturating_add(1);
        aggregate.transaction_count = aggregate
            .transaction_count
            .saturating_add(record.fee_transaction_count);
        aggregate.total_fee_zat = aggregate
            .total_fee_zat
            .saturating_add(summary.fees_collected_zat);
        if record.fee_transaction_count > 0 {
            aggregate.min_fee_zat = Some(
                aggregate
                    .min_fee_zat
                    .map_or(record.min_zip317_conventional_fee_zat, |prior| {
                        prior.min(record.min_zip317_conventional_fee_zat)
                    }),
            );
            aggregate.max_fee_zat = Some(
                aggregate
                    .max_fee_zat
                    .map_or(record.max_zip317_conventional_fee_zat, |prior| {
                        prior.max(record.max_zip317_conventional_fee_zat)
                    }),
            );
        }
    }
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use tempfile::tempdir;
    use zinder_core::{BlockHash, ChainEpochId};
    use zinder_materialized_views::{
        BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA, BlockSummaryConsumer,
        MaterializedViewStoreOptions,
    };
    use zinder_proto::v1::explorer::BlockSummary;
    use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};
    use zinder_store::RocksDbResourceBudget;

    fn state(revision: u64, hash_seed: u8) -> MaterializedViewState {
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(revision),
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([hash_seed; 32]),
            revision,
            coverage: None,
        }
    }

    fn record(state: MaterializedViewState, fees_collected_zat: u64) -> BlockSummaryRecord {
        BlockSummaryRecord {
            summary: Some(BlockSummary {
                block_height: state.tip_height.value(),
                block_hash: zinder_core::wire::encode_rpc_block_hash_hex(state.tip_hash),
                fees_collected_zat,
                ..Default::default()
            }),
            fee_transaction_count: 2,
            min_zip317_conventional_fee_zat: 10,
            max_zip317_conventional_fee_zat: 20,
            ..Default::default()
        }
    }

    #[test]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the E1 snapshot must remain open while the primary advances to E2"
    )]
    fn fee_summary_snapshot_retains_e1_aggregate_and_freshness_after_e2_write() -> eyre::Result<()>
    {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            zinder_testkit::published_regtest_canonical_construction_identity()?,
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: &[BLOCK_SUMMARY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        let e1_state = state(1, 0x11);
        let e1_status = MaterializedViewStatus {
            health: MaterializedViewHealth::Live as i32,
            indexed_height: 100,
            lag_blocks: 0,
            observed_at_millis: 1_000,
        };
        let key = BlockSummaryConsumer::key_for_height(e1_state.tip_height);
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &key,
            &record(e1_state, 30).encode_to_vec(),
        )?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e1_state)?;
        store.put_materialized_view_status(&e1_status.encode_to_vec())?;

        let e1_snapshot = store.read_snapshot()?;

        let e2_state = state(2, 0x22);
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &key,
            &record(e2_state, 300).encode_to_vec(),
        )?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e2_state)?;
        store.put_materialized_view_status(
            &MaterializedViewStatus {
                health: MaterializedViewHealth::Live as i32,
                indexed_height: 101,
                lag_blocks: 0,
                observed_at_millis: 1_001,
            }
            .encode_to_vec(),
        )?;

        let aggregate = aggregate_block_summaries(&e1_snapshot, 100, 100, e1_state)?;
        let freshness =
            build_explorer_freshness_from_snapshot(&e1_snapshot, EXPLORER_FEE_SUMMARY_V1, None, 0)?;
        assert_eq!(aggregate.block_count, 1);
        assert_eq!(aggregate.transaction_count, 2);
        assert_eq!(aggregate.total_fee_zat, 30);
        assert_eq!(aggregate.min_fee_zat, Some(10));
        assert_eq!(aggregate.max_fee_zat, Some(20));
        assert_eq!(
            freshness
                .chain_view
                .and_then(|chain_view| chain_view.materialized_views)
                .map(|status| status.observed_at_millis),
            Some(e1_status.observed_at_millis)
        );
        Ok(())
    }
}
