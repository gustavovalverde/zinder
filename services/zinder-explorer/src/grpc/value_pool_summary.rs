//! `ExplorerQuery.ValuePoolSummary` handler.
//!
//! Composes directly from `WalletQuery.ChainValuePoolsAtTip`. The wallet
//! method is already source-backed through ingest-control, so the explorer
//! handler only adds the standard freshness envelope.

use tonic::{Request, Response, Status};
use zinder_proto::capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1;
use zinder_proto::v1::explorer::{
    ExplorerFreshness, ValuePoolSummaryRequest, ValuePoolSummaryResponse,
};
use zinder_proto::v1::wallet::{
    self, ChainValuePoolsAtTipRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::freshness::{UpstreamObservationCache, attach_upstream_observation};

/// Executes one `ExplorerQuery.ValuePoolSummary` request.
pub(crate) async fn handle_value_pool_summary(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    _request: Request<ValuePoolSummaryRequest>,
) -> Result<Response<ValuePoolSummaryResponse>, Status> {
    let response = wallet_client
        .chain_value_pools_at_tip(Request::new(ChainValuePoolsAtTipRequest {}))
        .await?
        .into_inner();
    let chain_epoch = response
        .chain_epoch
        .ok_or_else(|| Status::internal("ChainValuePoolsAtTipResponse.chain_epoch missing"))?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        value_pool_freshness(chain_epoch),
    )
    .await;
    Ok(Response::new(ValuePoolSummaryResponse {
        freshness: Some(freshness),
        pools: response.pools,
        tip_height: response.tip_height,
    }))
}

fn value_pool_freshness(chain_epoch: wallet::ChainEpoch) -> ExplorerFreshness {
    ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: 0,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_VALUE_POOL_SUMMARY_V1.to_owned(),
        unavailable: Vec::new(),
        upstream: None,
    }
}
