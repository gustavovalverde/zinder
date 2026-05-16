//! `ExplorerQuery.FeeSummary` handler.
//!
//! Aggregates per-transaction ZIP-317 conventional fee floors over an
//! inclusive block range. The handler reads each block via
//! `WalletQuery.FullBlock`, parses the block bytes with `zebra-chain`,
//! re-serializes each transaction so the canonical
//! `zinder_source::parse_transaction_public_facts` produces the
//! component counts, and sums the
//! `TransactionComponentCounts::zip317_conventional_fee_zat` across
//! every non-coinbase transaction. Coinbase transactions are excluded
//! because they have no fee.
//!
//! The fee fields are ZIP-317 conventional fee floors, not
//! miner-collected fees. Computing actual fees requires prevout
//! resolution and is out of scope for v1; the conventional-fee floor
//! is the minimum a wallet should attach to a transaction with the
//! given shape.

use tonic::{Request, Response, Status};
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::{ZcashDeserializeInto as _, ZcashSerialize as _};
use zinder_core::{Network, NetworkUpgradeActivations};
use zinder_proto::capabilities::EXPLORER_FEE_SUMMARY_V1;
use zinder_proto::v1::explorer::{ExplorerFreshness, FeeSummaryRequest, FeeSummaryResponse};
use zinder_proto::v1::wallet::{
    self, FullBlockRequest, LatestBlockRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

/// Hard cap on the blocks one `FeeSummary` request aggregates.
///
/// Each block triggers a `WalletQuery.FullBlock` RPC plus a per-block
/// `zebra-chain` parse and per-transaction
/// `parse_transaction_public_facts` decode. The cap keeps total parse
/// cost bounded on mainnet blocks (median ~ a few dozen txs) without
/// requiring a derive consumer.
const MAX_FEE_SUMMARY_BLOCKS_PER_REQUEST: u32 = 256;

/// Executes one `ExplorerQuery.FeeSummary` request.
pub(crate) async fn handle_fee_summary(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: Network,
    request: Request<FeeSummaryRequest>,
) -> Result<Response<FeeSummaryResponse>, Status> {
    let inner = request.into_inner();
    validate_range(inner.start_height, inner.end_height)?;
    let activations = NetworkUpgradeActivations::empty(network);
    let mut aggregate = FeeAggregate::default();
    for height in inner.start_height..=inner.end_height {
        if let Some(full_block) =
            fetch_full_block(wallet_client, height, inner.at_epoch.as_ref()).await?
        {
            aggregate_block(&mut aggregate, height, &full_block, &activations)?;
        }
    }
    let chain_epoch = fetch_latest_chain_epoch(wallet_client).await?;
    Ok(Response::new(build_response(aggregate, chain_epoch)))
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

async fn fetch_full_block(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    height: u32,
    at_epoch: Option<&wallet::ChainEpoch>,
) -> Result<Option<wallet::FullBlock>, Status> {
    let outcome = wallet_client
        .full_block(Request::new(FullBlockRequest {
            block_height: height,
            at_epoch: at_epoch.cloned(),
        }))
        .await;
    match outcome {
        Ok(envelope) => Ok(envelope.into_inner().block),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(status) => Err(status),
    }
}

fn aggregate_block(
    aggregate: &mut FeeAggregate,
    height: u32,
    full_block: &wallet::FullBlock,
    activations: &NetworkUpgradeActivations,
) -> Result<(), Status> {
    let parsed: ZebraBlock = full_block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| {
            Status::internal(format!(
                "raw_block_bytes for {height} did not parse: {error}",
            ))
        })?;
    aggregate.block_count = aggregate.block_count.saturating_add(1);
    for transaction in &parsed.transactions {
        if transaction.is_coinbase() {
            continue;
        }
        let raw_tx_bytes = transaction.zcash_serialize_to_vec().map_err(|error| {
            Status::internal(format!(
                "could not re-serialize transaction in block {height}: {error}",
            ))
        })?;
        let facts = zinder_source::parse_transaction_public_facts(
            &raw_tx_bytes,
            Some(zinder_core::BlockHeight::new(height)),
            activations,
        )
        .map_err(|error| Status::internal(error.to_string()))?;
        let fee_zat = facts.counts.zip317_conventional_fee_zat();
        aggregate.transaction_count = aggregate.transaction_count.saturating_add(1);
        aggregate.total_fee_zat = aggregate.total_fee_zat.saturating_add(fee_zat);
        aggregate.min_fee_zat = Some(
            aggregate
                .min_fee_zat
                .map_or(fee_zat, |prior| prior.min(fee_zat)),
        );
        aggregate.max_fee_zat = Some(
            aggregate
                .max_fee_zat
                .map_or(fee_zat, |prior| prior.max(fee_zat)),
        );
    }
    Ok(())
}

fn build_response(aggregate: FeeAggregate, chain_epoch: wallet::ChainEpoch) -> FeeSummaryResponse {
    let freshness = ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: 0,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_FEE_SUMMARY_V1.to_owned(),
        unavailable: Vec::new(),
    };
    FeeSummaryResponse {
        freshness: Some(freshness),
        block_count: aggregate.block_count,
        transaction_count: aggregate.transaction_count,
        total_zip317_conventional_fee_zat: aggregate.total_fee_zat,
        min_zip317_conventional_fee_zat: aggregate.min_fee_zat.unwrap_or(0),
        max_zip317_conventional_fee_zat: aggregate.max_fee_zat.unwrap_or(0),
    }
}

async fn fetch_latest_chain_epoch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<wallet::ChainEpoch, Status> {
    let response = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch: None }))
        .await?
        .into_inner();
    response
        .chain_epoch
        .ok_or_else(|| Status::internal("LatestBlockResponse.chain_epoch missing"))
}
