//! `ExplorerQuery.ChainReorgHistory` handler.
//!
//! Reads the durable reorg-incidents materialized view. The view
//! backfills from the earliest retained chain-event row when the consumer first
//! appears, then preserves future incidents independently of chain-event
//! retention.

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_materialized_views::{
    MaterializedViewStore, REORG_INCIDENTS_COLUMN_FAMILY, REORG_INCIDENTS_KEY_LEN,
    ReorgIncidentsConsumer,
};
use zinder_proto::capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1;
use zinder_proto::v1::explorer::{
    ChainReorgHistoryEvent, ChainReorgHistoryRequest, ChainReorgHistoryResponse,
};

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

/// Server-side maximum retained chain events scanned per request.
const MAX_CHAIN_REORG_HISTORY_EVENTS_PER_REQUEST: u32 = 1024;

/// Default retained chain-event scan size when the caller passes zero.
const DEFAULT_CHAIN_REORG_HISTORY_EVENTS: u32 = 64;

/// Executes one `ExplorerQuery.ChainReorgHistory` request.
pub(crate) async fn query_chain_reorg_history(
    materialized_view_store: &MaterializedViewStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<ChainReorgHistoryRequest>,
) -> Result<Response<ChainReorgHistoryResponse>, Status> {
    let inner = request.into_inner();
    let max_events = clamp_max_entries(
        inner.max_events,
        DEFAULT_CHAIN_REORG_HISTORY_EVENTS,
        MAX_CHAIN_REORG_HISTORY_EVENTS_PER_REQUEST,
    );
    let start_key = if inner.from_cursor.is_empty() {
        Some([0u8; REORG_INCIDENTS_KEY_LEN])
    } else {
        let event_sequence =
            ReorgIncidentsConsumer::decode_event_sequence_cursor(&inner.from_cursor)
                .map_err(|error| ExplorerError::invalid_request(error.to_string()))?;
        event_sequence
            .checked_add(1)
            .map(ReorgIncidentsConsumer::key_for_event_sequence)
    };
    let Some(start_key) = start_key else {
        return empty_response(materialized_view_store, upstream_observation_cache).await;
    };
    let end_key = [0xFFu8; REORG_INCIDENTS_KEY_LEN];
    let scan_cap = (max_events as usize).saturating_add(1);

    materialized_view_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut rows = materialized_view_store
        .range_iterate_consumer(
            REORG_INCIDENTS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            scan_cap,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;

    let has_more = rows.len() > max_events as usize;
    if has_more {
        rows.truncate(max_events as usize);
    }
    let mut next_cursor = Vec::new();
    let mut events = Vec::with_capacity(rows.len());
    for (key, payload) in rows {
        let key_array: [u8; REORG_INCIDENTS_KEY_LEN] = key
            .as_slice()
            .try_into()
            .map_err(|_| ExplorerError::internal("reorg_incidents row key is not 8 bytes"))?;
        let event = ChainReorgHistoryEvent::decode(payload.as_slice())
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if has_more {
            next_cursor = key_array.to_vec();
        }
        events.push(event);
    }
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_CHAIN_REORG_HISTORY_V1,
            None,
            0,
        )?,
    )
    .await;

    Ok(Response::new(ChainReorgHistoryResponse {
        freshness: Some(freshness),
        events,
        next_cursor,
    }))
}

async fn empty_response(
    materialized_view_store: &MaterializedViewStore,
    upstream_observation_cache: &UpstreamObservationCache,
) -> Result<Response<ChainReorgHistoryResponse>, Status> {
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_CHAIN_REORG_HISTORY_V1,
            None,
            0,
        )?,
    )
    .await;
    Ok(Response::new(ChainReorgHistoryResponse {
        freshness: Some(freshness),
        events: Vec::new(),
        next_cursor: Vec::new(),
    }))
}
