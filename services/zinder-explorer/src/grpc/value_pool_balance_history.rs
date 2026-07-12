//! Canonical UTC-day cumulative value-pool balance history handler.

use tonic::{Request, Response, Status};
use zinder_core::wire::encode_rpc_block_hash_hex;
use zinder_derive::{
    DeriveStore, ValuePoolBalanceBackfillCoverage, ValuePoolBalanceDay,
    ValuePoolBalanceHistoryConsumer, ValuePoolBalanceTailCoverage,
};
use zinder_proto::capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1;
use zinder_proto::v1::explorer::{
    ValuePoolBalance, ValuePoolBalanceHistoryCoverage, ValuePoolBalanceHistoryPoint,
    ValuePoolBalanceHistoryRequest, ValuePoolBalanceHistoryResponse,
};
use zinder_proto::v1::wallet::{LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

const DEFAULT_PAGE_SIZE: u32 = 512;
const MAX_PAGE_SIZE: u32 = 4_096;
const CURSOR_PREFIX: &[u8; 4] = b"zvb1";
const CURSOR_LEN: usize = CURSOR_PREFIX.len() + size_of::<i64>();

/// Executes one `ExplorerQuery.ValuePoolBalanceHistory` request.
pub(crate) async fn handle_value_pool_balance_history(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<ValuePoolBalanceHistoryRequest>,
) -> Result<Response<ValuePoolBalanceHistoryResponse>, Status> {
    let request = request.into_inner();
    let page_size = clamp_max_entries(request.page_size, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE);
    let before_day = if request.cursor.is_empty() {
        None
    } else {
        Some(decode_cursor(&request.cursor)?)
    };
    let backfill = ValuePoolBalanceHistoryConsumer::backfill_coverage(derive_store)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let tail = ValuePoolBalanceHistoryConsumer::tail_coverage(derive_store)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let chain_epoch = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner()
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            Status::from(ExplorerError::internal(
                "LatestBlockResponse.chain_view.chain_epoch missing",
            ))
        })?;
    let visible_tip_height = chain_epoch
        .visible_tip
        .as_ref()
        .map(|tip| tip.height)
        .ok_or_else(|| Status::from(ExplorerError::internal("ChainEpoch.visible_tip missing")))?;
    let mut days = read_days_blocking(
        derive_store,
        before_day,
        usize::try_from(page_size)
            .unwrap_or(usize::MAX)
            .saturating_add(1),
    )
    .await?;
    let has_more = days.len() > usize::try_from(page_size).unwrap_or(usize::MAX);
    if has_more {
        days.truncate(usize::try_from(page_size).unwrap_or(usize::MAX));
    }
    let next_cursor = if has_more {
        days.last()
            .map_or_else(Vec::new, |day| encode_cursor(day.day_start_unix_seconds))
    } else {
        Vec::new()
    };
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(ValuePoolBalanceHistoryResponse {
        freshness: Some(freshness),
        points: days.into_iter().map(map_point).collect(),
        next_cursor,
        has_more,
        coverage: Some(map_coverage(backfill, tail, visible_tip_height)),
    }))
}

async fn read_days_blocking(
    derive_store: &DeriveStore,
    before_day: Option<i64>,
    cap: usize,
) -> Result<Vec<ValuePoolBalanceDay>, Status> {
    let derive_store = derive_store.clone();
    tokio::task::spawn_blocking(move || {
        ValuePoolBalanceHistoryConsumer::read_days_before(&derive_store, before_day, cap)
            .map_err(|error| Status::from(ExplorerError::internal(error.to_string())))
    })
    .await
    .map_err(|error| {
        ExplorerError::internal(format!("value-pool balance history scan failed: {error}"))
    })?
}

fn map_point(day: ValuePoolBalanceDay) -> ValuePoolBalanceHistoryPoint {
    ValuePoolBalanceHistoryPoint {
        day_start_unix_seconds: day.day_start_unix_seconds,
        block_height: day.point.block_height.value(),
        block_hash: encode_rpc_block_hash_hex(day.point.block_hash),
        block_time_unix_seconds: day.point.block_time_unix_seconds,
        pools: day
            .point
            .pools
            .into_iter()
            .map(|pool| ValuePoolBalance {
                id: pool.id,
                monitored: pool.monitored,
                value_zat: pool.value_zat,
            })
            .collect(),
    }
}

fn map_coverage(
    backfill: Option<ValuePoolBalanceBackfillCoverage>,
    tail: Option<ValuePoolBalanceTailCoverage>,
    visible_tip_height: u32,
) -> ValuePoolBalanceHistoryCoverage {
    ValuePoolBalanceHistoryCoverage {
        historical_from_height: backfill.map(|coverage| coverage.complete_from_height.value()),
        historical_through_height: backfill
            .map(|coverage| coverage.complete_through_height.value()),
        live_tail_from_height: tail.map(|coverage| coverage.boundary_height.value()),
        live_tail_through_height: tail
            .and_then(|coverage| coverage.complete_through_height)
            .map(zinder_core::BlockHeight::value),
        complete_through_visible_tip: coverage_reaches_visible_tip(
            backfill,
            tail,
            visible_tip_height,
        ),
    }
}

fn coverage_reaches_visible_tip(
    backfill: Option<ValuePoolBalanceBackfillCoverage>,
    tail: Option<ValuePoolBalanceTailCoverage>,
    visible_tip_height: u32,
) -> bool {
    let Some(backfill) = backfill else {
        return false;
    };
    if backfill.complete_from_height != zinder_core::BlockHeight::new(1) {
        return false;
    }
    if backfill.complete_through_height.value() >= visible_tip_height {
        return true;
    }
    let Some(tail) = tail else {
        return false;
    };
    backfill.complete_through_height.next() == Some(tail.boundary_height)
        && tail
            .complete_through_height
            .is_some_and(|through| through.value() >= visible_tip_height)
}

fn encode_cursor(day_start_unix_seconds: i64) -> Vec<u8> {
    let mut cursor = Vec::with_capacity(CURSOR_LEN);
    cursor.extend_from_slice(CURSOR_PREFIX);
    cursor.extend_from_slice(&day_start_unix_seconds.to_be_bytes());
    cursor
}

fn decode_cursor(cursor: &[u8]) -> Result<i64, Status> {
    if cursor.len() != CURSOR_LEN || &cursor[..CURSOR_PREFIX.len()] != CURSOR_PREFIX {
        return Err(ExplorerError::invalid_request(
            "value-pool balance history cursor is malformed",
        )
        .into());
    }
    let day = i64::from_be_bytes(
        cursor[CURSOR_PREFIX.len()..]
            .try_into()
            .map_err(|_| ExplorerError::invalid_request("cursor is malformed"))?,
    );
    if day.rem_euclid(86_400) != 0 {
        return Err(ExplorerError::invalid_request(
            "value-pool balance history cursor is not a UTC-day boundary",
        )
        .into());
    }
    Ok(day)
}

#[cfg(test)]
mod tests {
    use zinder_core::BlockHeight;

    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_non_day_values() -> Result<(), Status> {
        let day = 42 * 86_400;
        assert_eq!(decode_cursor(&encode_cursor(day))?, day);
        assert!(decode_cursor(&encode_cursor(day + 1)).is_err());
        assert!(decode_cursor(b"bad").is_err());
        Ok(())
    }

    #[test]
    fn completeness_requires_one_contiguous_height_domain() {
        let backfill =
            ValuePoolBalanceBackfillCoverage::new(BlockHeight::new(1), BlockHeight::new(100));
        let tail = ValuePoolBalanceTailCoverage {
            boundary_height: BlockHeight::new(101),
            complete_through_height: Some(BlockHeight::new(110)),
        };
        assert!(coverage_reaches_visible_tip(
            Some(backfill),
            Some(tail),
            110
        ));
        assert!(!coverage_reaches_visible_tip(
            Some(backfill),
            Some(tail),
            111
        ));
        assert!(!coverage_reaches_visible_tip(
            Some(backfill),
            Some(ValuePoolBalanceTailCoverage {
                boundary_height: BlockHeight::new(102),
                complete_through_height: Some(BlockHeight::new(110)),
            }),
            110,
        ));
    }
}
