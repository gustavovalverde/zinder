//! `ExplorerQuery.FeeSummary` handler.
//!
//! Aggregates per-transaction ZIP-317 conventional fee floors over an
//! inclusive block range from the typed `BlockSummaryRecord` rows
//! materialized by the derive plane. Coinbase transactions are excluded
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
use zinder_derive::{BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore};
use zinder_proto::capabilities::EXPLORER_FEE_SUMMARY_V1;
use zinder_proto::v1::explorer::{BlockSummaryRecord, FeeSummaryRequest, FeeSummaryResponse};
use zinder_proto::v1::wallet::{self, LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

/// Hard cap on the blocks one `FeeSummary` request aggregates.
///
/// The wire response is a single aggregate over a contiguous window; the cap
/// bounds one request's derive-store scan.
const MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST: u32 = 256;

/// Executes one `ExplorerQuery.FeeSummary` request.
pub(crate) async fn handle_fee_summary(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<FeeSummaryRequest>,
) -> Result<Response<FeeSummaryResponse>, Status> {
    let inner = request.into_inner();
    validate_range(inner.start_height, inner.end_height)?;
    let aggregate = aggregate_block_summaries(derive_store, inner.start_height, inner.end_height)?;
    let chain_epoch = fetch_latest_chain_epoch(wallet_client).await?;
    let mut response = build_response(derive_store, aggregate, chain_epoch)?;
    if let Some(freshness) = response.freshness.take() {
        response.freshness =
            Some(attach_upstream_observation(upstream_observation_cache, freshness).await);
    }
    Ok(Response::new(response))
}

fn validate_range(start_height: u32, end_height: u32) -> Result<(), Status> {
    if end_height < start_height {
        return Err(Status::invalid_argument(
            "end_height must be >= start_height",
        ));
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST) {
        return Err(Status::invalid_argument(format!(
            "requested span {span} blocks exceeds the per-request cap of \
             {MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST}",
        )));
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
    derive_store: &DeriveStore,
    start_height: u32,
    end_height: u32,
) -> Result<FeeAggregate, Status> {
    let start_key = encode_height_key_ascending(BlockHeight::new(start_height));
    let end_key = encode_height_key_ascending(BlockHeight::new(end_height));
    let entries = derive_store
        .range_iterate_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST as usize,
        )
        .map_err(|error| Status::internal(error.to_string()))?;

    let mut aggregate = FeeAggregate::default();
    for (_, payload) in entries {
        let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
            Status::internal(format!("BlockSummaryRecord decode failed: {error}"))
        })?;
        let summary = record
            .summary
            .ok_or_else(|| Status::internal("BlockSummaryRecord.summary missing"))?;
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

fn build_response(
    derive_store: &DeriveStore,
    aggregate: FeeAggregate,
    chain_epoch: wallet::ChainEpoch,
) -> Result<FeeSummaryResponse, Status> {
    let freshness = build_explorer_freshness(
        Some(derive_store),
        EXPLORER_FEE_SUMMARY_V1,
        Some(chain_epoch),
        0,
    )?;
    Ok(FeeSummaryResponse {
        freshness: Some(freshness),
        block_count: aggregate.block_count,
        transaction_count: aggregate.transaction_count,
        total_zip317_conventional_fee_zat: aggregate.total_fee_zat,
        min_zip317_conventional_fee_zat: aggregate.min_fee_zat.unwrap_or(0),
        max_zip317_conventional_fee_zat: aggregate.max_fee_zat.unwrap_or(0),
    })
}

async fn fetch_latest_chain_epoch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<wallet::ChainEpoch, Status> {
    wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch: None }))
        .await?
        .into_inner()
        .chain_epoch
        .ok_or_else(|| Status::internal("LatestBlockResponse.chain_epoch missing"))
}
