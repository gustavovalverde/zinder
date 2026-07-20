//! `ExplorerQuery.ValuePoolSummary` handler.
//!
//! Composes directly from `WalletQuery.ChainValuePoolsAtTip`. The wallet
//! method is already source-backed through ingest-control, so the explorer
//! handler only adds the standard freshness envelope.

use tonic::{Request, Response, Status};
use zinder_proto::capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1;
use zinder_proto::v1::explorer::{ValuePoolSummaryRequest, ValuePoolSummaryResponse};
use zinder_proto::v1::wallet::{
    ChainValuePoolsAtTipRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use zinder_materialized_views::MaterializedViewStore;

/// Executes one `ExplorerQuery.ValuePoolSummary` request.
pub(crate) async fn query_value_pool_summary(
    materialized_view_store: Option<&MaterializedViewStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    _request: Request<ValuePoolSummaryRequest>,
) -> Result<Response<ValuePoolSummaryResponse>, Status> {
    let response = wallet_client
        .chain_value_pools_at_tip(Request::new(ChainValuePoolsAtTipRequest {}))
        .await?
        .into_inner();
    let chain_epoch = response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("ChainValuePoolsAtTipResponse.chain_view.chain_epoch missing")
        })?;
    let source_tip = response.source_tip.ok_or_else(|| {
        ExplorerError::internal("ChainValuePoolsAtTipResponse.source_tip missing")
    })?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            EXPLORER_VALUE_POOL_SUMMARY_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(ValuePoolSummaryResponse {
        freshness: Some(freshness),
        pools: response.pools,
        source_tip: Some(source_tip),
    }))
}
