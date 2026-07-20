//! `ExplorerQuery` block-view handlers.
//!
//! Both reads project the materialized `BlockSummaryRecord` payloads written
//! by [`zinder_materialized_views::BlockSummaryConsumer`] into the
//! public wire shapes. The handlers wrap reads in the cross-cutting
//! [`ExplorerFreshness`] envelope per
//! [ADR-0011](../../../docs/adrs/0011-explorer-freshness-envelope.md) and
//! compute `materialized_view_cursor_lag_blocks` against the wallet plane's visible
//! tip.

use std::collections::{HashMap, HashSet};

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, ChainEpochId, TransactionFactsArtifact,
    TransactionId,
    wire::{
        decode_internal_block_hash, decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex,
        encode_height_key_ascending, encode_internal_block_hash, encode_rpc_block_hash_hex,
        encode_rpc_transaction_id_hex,
    },
};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1, EXPLORER_BLOCK_SUMMARY_V1,
    EXPLORER_BLOCK_TRANSACTIONS_V2,
};
use zinder_proto::v1::explorer::{
    BlockDetailRequest, BlockDetailResponse, BlockFinalNoteCommitmentRoots,
    BlockProductionInTimeRangeRequest, BlockProductionInTimeRangeResponse, BlockProductionPoint,
    BlockProductionSeriesRequest, BlockProductionSeriesResponse, BlockProductionTimeRangeCoverage,
    BlockProductionTimeRangeReadFence, BlockSummariesInRangeRequest, BlockSummariesInRangeResponse,
    BlockSummary, BlockSummaryRecord, BlockTransaction, BlockTransactionsResponse,
    CoinbaseTransactionSummary, block_detail_request,
};
use zinder_proto::v1::wallet::{
    self, BlockSelector, LatestBlockRequest, block_selector, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
    build_explorer_freshness_from_snapshot,
};
use super::require_matching_chain_epoch;
use super::transaction_detail::encode_public_facts;
use super::transparent_input::{encode_mined_transparent_inputs, parent_transaction_ids};
use zinder_materialized_views::{
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME, BLOCK_SUMMARY_COLUMN_FAMILY, BlockProductionTimeConsumer,
    BlockProductionTimeCursor, BlockProductionTimePageRequest, ConsumerProjectionState,
    MaterializedViewStore, MaterializedViewStoreReadSnapshot, PaidFeeDistributionConsumer,
};
use zinder_store::{
    ChainEpochReader, SecondaryChainStore, chain_epoch_from_message, chain_epoch_message,
    status_from_store_error,
};

/// Hard cap on the number of block summaries one range request returns.
///
/// The wire response is a single repeated field; a multi-million-row request
/// would blow up the gRPC buffer. The cap mirrors the bounded-page rule used
/// across other explorer reads.
const MAX_BLOCK_SUMMARIES_PER_REQUEST: u32 = 1024;
const DEFAULT_BLOCK_PRODUCTION_TIME_PAGE_SIZE: usize = 256;
const BLOCK_PRODUCTION_CURSOR_VERSION: u8 = 1;
const BLOCK_PRODUCTION_CURSOR_FIXED_LEN: usize =
    1 + 2 * size_of::<i64>() + 2 * size_of::<u64>() + size_of::<u32>() + 32 + size_of::<u16>();

struct MaterializedBlockView {
    summary: zinder_proto::v1::explorer::BlockSummary,
    transaction_ids: Vec<String>,
    chain_epoch: wallet::ChainEpoch,
}

struct BlockProductionTimeCursorEnvelope {
    start_time_unix_seconds: i64,
    end_time_unix_seconds: i64,
    chain_epoch_id: u64,
    projection_revision: u64,
    projection_tip_height: BlockHeight,
    projection_tip_hash: BlockHash,
    after: BlockProductionTimeCursor,
}

struct MaterializedProductionTimePage {
    freshness: zinder_proto::v1::explorer::ExplorerFreshness,
    points: Vec<BlockProductionPoint>,
    next_cursor: Vec<u8>,
    scanned_block_count: u32,
    missing_block_count: u32,
    missing_coinbase_count: u32,
    missing_paid_fee_count: u32,
    coverage: BlockProductionTimeRangeCoverage,
    read_fence: BlockProductionTimeRangeReadFence,
}

pub(crate) async fn handle_block_summaries_in_range(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockSummariesInRangeRequest>,
) -> Result<Response<BlockSummariesInRangeResponse>, Status> {
    let inner = request.into_inner();
    let start_height = inner.start_height;
    let end_height = inner.end_height;
    validate_block_view_range(start_height, end_height)?;

    let (chain_epoch, canonical_tip) = read_canonical_tip(wallet_client).await?;
    let mut summaries =
        read_materialized_block_summaries(materialized_view_store, start_height, end_height)?;
    for summary in &mut summaries {
        annotate_request_time_fields(summary, canonical_tip);
    }

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_BLOCK_SUMMARY_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockSummariesInRangeResponse {
        freshness: Some(freshness),
        summaries,
    }))
}

pub(crate) async fn handle_block_production_series(
    materialized_view_store: &MaterializedViewStore,
    chain_store: &SecondaryChainStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockProductionSeriesRequest>,
) -> Result<Response<BlockProductionSeriesResponse>, Status> {
    let request = request.into_inner();
    let requested_block_count =
        validate_block_view_range(request.start_height, request.end_height)?;
    let records = read_materialized_block_records(
        materialized_view_store,
        request.start_height,
        request.end_height,
    )?;
    let summaries = records
        .iter()
        .map(|record| {
            record
                .summary
                .clone()
                .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing").into())
        })
        .collect::<Result<Vec<_>, Status>>()?;

    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let (chain_epoch, points) = {
        let reader = match request.at_epoch_id {
            Some(chain_epoch_id) => chain_store
                .chain_epoch_reader_at(ChainEpochId::new(chain_epoch_id))
                .map_err(|error| status_from_store_error(&error))?,
            None => chain_store
                .current_chain_epoch_reader()
                .map_err(|error| status_from_store_error(&error))?,
        };
        let chain_epoch = reader.chain_epoch();
        let headers = reader
            .block_headers_in_range(BlockHeightRange::inclusive(
                BlockHeight::new(request.start_height),
                BlockHeight::new(request.end_height),
            ))
            .map_err(|error| status_from_store_error(&error))?;
        let mut points = join_block_production_points(
            summaries,
            headers,
            request.start_height,
            chain_epoch.visible_tip_height.value(),
        );
        let coinbase_artifacts = read_coinbase_artifacts(&reader, &records)?;
        attach_coinbase_summaries(&mut points, &records, &coinbase_artifacts)?;
        (chain_epoch, points)
    };
    let covered_block_count = u32::try_from(points.len()).unwrap_or(u32::MAX);
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
            Some(chain_epoch_message(chain_epoch)),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockProductionSeriesResponse {
        freshness: Some(freshness),
        start_height: request.start_height,
        end_height: request.end_height,
        covered_block_count,
        missing_block_count: requested_block_count.saturating_sub(covered_block_count),
        points,
    }))
}

pub(crate) async fn handle_block_production_in_time_range(
    materialized_view_store: &MaterializedViewStore,
    chain_store: &SecondaryChainStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockProductionInTimeRangeRequest>,
) -> Result<Response<BlockProductionInTimeRangeResponse>, Status> {
    let materialized = read_block_production_time_page(
        materialized_view_store,
        chain_store,
        &request.into_inner(),
    )?;
    let freshness =
        attach_upstream_observation(upstream_observation_cache, materialized.freshness).await;
    Ok(Response::new(BlockProductionInTimeRangeResponse {
        freshness: Some(freshness),
        points: materialized.points,
        next_cursor: materialized.next_cursor,
        covered_block_count: materialized
            .scanned_block_count
            .saturating_sub(materialized.missing_block_count),
        missing_block_count: materialized.missing_block_count,
        missing_coinbase_count: materialized.missing_coinbase_count,
        missing_paid_fee_count: materialized.missing_paid_fee_count,
        coverage: Some(materialized.coverage),
        read_fence: Some(materialized.read_fence),
    }))
}

fn read_block_production_time_page(
    materialized_view_store: &MaterializedViewStore,
    chain_store: &SecondaryChainStore,
    request: &BlockProductionInTimeRangeRequest,
) -> Result<MaterializedProductionTimePage, Status> {
    validate_block_production_time_request(request)?;
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let reader = chain_store
        .current_chain_epoch_reader()
        .map_err(|error| status_from_store_error(&error))?;
    let chain_epoch = reader.chain_epoch();
    let snapshot = materialized_view_store.read_snapshot();
    let current_projection_state = snapshot
        .consumer_projection_state(BLOCK_PRODUCTION_TIME_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(
                "block-production time projection state is not available",
            )
        })?;
    let (cursor, projection_state) = resolve_block_production_cursor_fence(
        request,
        current_projection_state,
        chain_epoch.id,
        &snapshot,
    )?;
    validate_frozen_projection_tip(&reader, projection_state)?;
    let page = BlockProductionTimeConsumer::read_page_snapshot(
        &snapshot,
        BlockProductionTimePageRequest {
            start_time_unix_seconds: request.start_time_unix_seconds,
            end_time_unix_seconds: request.end_time_unix_seconds,
            after: cursor.map(|cursor| cursor.after),
            maximum_height: Some(projection_state.projection_tip_height),
            limit: block_production_time_page_size(request.max_entries),
        },
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let coverage = BlockProductionTimeConsumer::coverage_snapshot(&snapshot)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let fence_tip_time = BlockProductionTimeConsumer::row_at_height_snapshot(
        &snapshot,
        projection_state.projection_tip_height,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?
    .filter(|row| row.block_hash == projection_state.projection_tip_hash)
    .map(|row| row.block_time_unix_seconds);
    let records = read_block_summary_records_for_time_rows(&snapshot, &page.rows)?;
    let paid_fees = read_paid_fee_totals_for_time_rows(&snapshot, &page.rows)?;
    let freshness = build_explorer_freshness_from_snapshot(
        &snapshot,
        EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
        Some(chain_epoch_message(chain_epoch)),
        0,
    )?;
    drop(snapshot);

    let (points, missing_block_count, missing_coinbase_count, missing_paid_fee_count) =
        materialize_block_production_time_points(&reader, &page.rows, records, paid_fees)?;
    let next_cursor = page.next_cursor.map_or_else(Vec::new, |after| {
        encode_block_production_cursor(
            request.start_time_unix_seconds,
            request.end_time_unix_seconds,
            projection_state,
            after,
        )
    });
    let read_fence = block_production_time_read_fence(projection_state);
    let coverage = block_production_time_coverage(coverage, projection_state, fence_tip_time);
    Ok(MaterializedProductionTimePage {
        freshness,
        points,
        next_cursor,
        scanned_block_count: u32::try_from(page.rows.len()).unwrap_or(u32::MAX),
        missing_block_count,
        missing_coinbase_count,
        missing_paid_fee_count,
        coverage,
        read_fence,
    })
}

fn read_paid_fee_totals_for_time_rows(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    rows: &[zinder_materialized_views::BlockProductionTimeRow],
) -> Result<Vec<Option<zinder_materialized_views::PaidFeeBlockTotal>>, Status> {
    rows.iter()
        .map(|row| {
            PaidFeeDistributionConsumer::block_total_snapshot(
                snapshot,
                row.block_time_unix_seconds,
                row.block_height,
                row.block_hash,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()).into())
        })
        .collect()
}

fn validate_frozen_projection_tip(
    reader: &ChainEpochReader<'_>,
    projection_state: ConsumerProjectionState,
) -> Result<(), Status> {
    let header = reader
        .block_header_at(projection_state.projection_tip_height)
        .map_err(|error| status_from_store_error(&error))?;
    if header.is_none_or(|header| header.block_hash != projection_state.projection_tip_hash) {
        return Err(ExplorerError::unsatisfied_precondition(
            "block-production projection tip is not canonical in the current chain view",
        )
        .into());
    }
    Ok(())
}

fn validate_block_production_time_request(
    request: &BlockProductionInTimeRangeRequest,
) -> Result<(), Status> {
    if request.start_time_unix_seconds >= request.end_time_unix_seconds {
        return Err(ExplorerError::invalid_request(
            "end_time_unix_seconds must be greater than start_time_unix_seconds",
        )
        .into());
    }
    Ok(())
}

fn block_production_time_page_size(requested: u32) -> usize {
    if requested == 0 {
        DEFAULT_BLOCK_PRODUCTION_TIME_PAGE_SIZE
    } else {
        usize::try_from(requested)
            .unwrap_or(usize::MAX)
            .min(zinder_materialized_views::BLOCK_PRODUCTION_TIME_MAX_PAGE_SIZE)
    }
}

fn read_block_summary_records_for_time_rows(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    rows: &[zinder_materialized_views::BlockProductionTimeRow],
) -> Result<Vec<Option<BlockSummaryRecord>>, Status> {
    let keys = rows
        .iter()
        .map(|row| encode_height_key_ascending(row.block_height))
        .collect::<Vec<_>>();
    snapshot
        .multi_get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &keys)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .into_iter()
        .map(|payload| {
            payload
                .map(|payload| {
                    BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
                        ExplorerError::internal(format!(
                            "BlockSummaryRecord decode failed: {error}"
                        ))
                        .into()
                    })
                })
                .transpose()
        })
        .collect()
}

fn materialize_block_production_time_points(
    reader: &ChainEpochReader<'_>,
    rows: &[zinder_materialized_views::BlockProductionTimeRow],
    records: Vec<Option<BlockSummaryRecord>>,
    paid_fees: Vec<Option<zinder_materialized_views::PaidFeeBlockTotal>>,
) -> Result<(Vec<BlockProductionPoint>, u32, u32, u32), Status> {
    let canonical_tip = reader.chain_epoch().visible_tip_height.value();
    let mut points = Vec::with_capacity(rows.len());
    let mut point_records = Vec::with_capacity(rows.len());
    let mut missing_block_count = 0_u32;
    let mut missing_paid_fee_count = 0_u32;
    for ((row, record), paid_fee) in rows.iter().zip(records).zip(paid_fees) {
        let Some(record) = record else {
            missing_block_count = missing_block_count.saturating_add(1);
            continue;
        };
        let mut summary = validate_time_index_block_summary(row, &record)?;
        let Some(header) = reader
            .block_header_at(row.block_height)
            .map_err(|error| status_from_store_error(&error))?
        else {
            missing_block_count = missing_block_count.saturating_add(1);
            continue;
        };
        let expected_hash = row.block_hash;
        let expected_time = row.block_time_unix_seconds;
        if header.block_hash != expected_hash || header.block_time != expected_time {
            return Err(ExplorerError::internal(
                "block-production time row disagrees with its canonical header",
            )
            .into());
        }
        match paid_fee {
            Some(paid_fee) if paid_fee.unavailable_transaction_count == 0 => {
                summary.paid_fees_collected_zat = Some(paid_fee.paid_fee_zat);
            }
            Some(_) | None => {
                missing_paid_fee_count = missing_paid_fee_count.saturating_add(1);
            }
        }
        annotate_request_time_fields(&mut summary, canonical_tip);
        points.push(BlockProductionPoint {
            summary: Some(summary),
            bits: header.bits,
            coinbase: None,
        });
        point_records.push(record);
    }
    let artifacts = read_coinbase_artifacts(reader, &point_records)?;
    attach_coinbase_summaries(&mut points, &point_records, &artifacts)?;
    let missing_coinbase_count = u32::try_from(
        points
            .iter()
            .filter(|point| point.coinbase.is_none())
            .count(),
    )
    .unwrap_or(u32::MAX);
    Ok((
        points,
        missing_block_count,
        missing_coinbase_count,
        missing_paid_fee_count,
    ))
}

fn validate_time_index_block_summary(
    row: &zinder_materialized_views::BlockProductionTimeRow,
    record: &BlockSummaryRecord,
) -> Result<BlockSummary, Status> {
    let summary = record
        .summary
        .clone()
        .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
    let block_hash = decode_rpc_block_hash_hex(&summary.block_hash)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if summary.block_height != row.block_height.value()
        || block_hash != row.block_hash
        || summary.block_time_unix_seconds != row.block_time_unix_seconds
    {
        return Err(ExplorerError::internal(
            "block-production time row disagrees with its block summary",
        )
        .into());
    }
    Ok(summary)
}

fn block_production_time_coverage(
    coverage: Option<zinder_materialized_views::BlockProductionTimeBackfillCoverage>,
    projection_state: ConsumerProjectionState,
    fence_tip_time_unix_seconds: Option<i64>,
) -> BlockProductionTimeRangeCoverage {
    let coverage = coverage.map(|mut coverage| {
        if coverage.complete_through_height > projection_state.projection_tip_height
            && let Some(fence_tip_time_unix_seconds) = fence_tip_time_unix_seconds
        {
            coverage.complete_through_height = projection_state.projection_tip_height;
            coverage.complete_through_time_unix_seconds = fence_tip_time_unix_seconds;
        }
        coverage
    });
    BlockProductionTimeRangeCoverage {
        complete_from_height: coverage.map(|coverage| coverage.complete_from_height.value()),
        complete_through_height: coverage.map(|coverage| coverage.complete_through_height.value()),
        complete_from_time_unix_seconds: coverage
            .map(|coverage| coverage.complete_from_time_unix_seconds),
        complete_through_time_unix_seconds: coverage
            .map(|coverage| coverage.complete_through_time_unix_seconds),
        requested_range_complete: coverage.is_some_and(|coverage| {
            coverage.complete_from_height.value() <= 1
                && coverage.complete_through_height >= projection_state.projection_tip_height
        }),
    }
}

fn block_production_time_read_fence(
    projection_state: ConsumerProjectionState,
) -> BlockProductionTimeRangeReadFence {
    BlockProductionTimeRangeReadFence {
        chain_epoch_id: projection_state.projection_epoch_id.value(),
        projection_revision: projection_state.revision,
        projection_tip: Some(wallet::BlockTip {
            height: projection_state.projection_tip_height.value(),
            hash: encode_rpc_block_hash_hex(projection_state.projection_tip_hash),
        }),
    }
}

fn encode_block_production_cursor(
    start_time_unix_seconds: i64,
    end_time_unix_seconds: i64,
    projection_state: ConsumerProjectionState,
    after: BlockProductionTimeCursor,
) -> Vec<u8> {
    let after_bytes = after.as_bytes();
    let after_len = u16::try_from(after_bytes.len()).unwrap_or(u16::MAX);
    let mut cursor = Vec::with_capacity(BLOCK_PRODUCTION_CURSOR_FIXED_LEN + after_bytes.len());
    cursor.push(BLOCK_PRODUCTION_CURSOR_VERSION);
    cursor.extend_from_slice(&start_time_unix_seconds.to_be_bytes());
    cursor.extend_from_slice(&end_time_unix_seconds.to_be_bytes());
    cursor.extend_from_slice(&projection_state.projection_epoch_id.value().to_be_bytes());
    cursor.extend_from_slice(&projection_state.revision.to_be_bytes());
    cursor.extend_from_slice(&projection_state.projection_tip_height.value().to_be_bytes());
    cursor.extend_from_slice(&encode_internal_block_hash(
        projection_state.projection_tip_hash,
    ));
    cursor.extend_from_slice(&after_len.to_be_bytes());
    cursor.extend_from_slice(after_bytes);
    cursor
}

fn resolve_block_production_cursor_fence(
    request: &BlockProductionInTimeRangeRequest,
    current_projection_state: ConsumerProjectionState,
    current_chain_epoch_id: ChainEpochId,
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<
    (
        Option<BlockProductionTimeCursorEnvelope>,
        ConsumerProjectionState,
    ),
    Status,
> {
    if request.from_cursor.is_empty() {
        if request
            .at_epoch_id
            .is_some_and(|requested| requested != current_chain_epoch_id.value())
        {
            return Err(ExplorerError::unsatisfied_precondition(
                "requested chain epoch is not the current canonical chain view",
            )
            .into());
        }
        return Ok((
            None,
            ConsumerProjectionState {
                projection_epoch_id: current_chain_epoch_id,
                ..current_projection_state
            },
        ));
    }
    let cursor = decode_and_validate_block_production_cursor_request(request)?;
    let frozen_tip_is_still_canonical = current_projection_state.projection_tip_height
        >= cursor.projection_tip_height
        && BlockProductionTimeConsumer::row_at_height_snapshot(
            snapshot,
            cursor.projection_tip_height,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .is_some_and(|row| row.block_hash == cursor.projection_tip_hash);
    if !frozen_tip_is_still_canonical {
        return Err(ExplorerError::unsatisfied_precondition(
            "block-production cursor projection fence is no longer current",
        )
        .into());
    }
    let frozen_projection_state = ConsumerProjectionState {
        projection_epoch_id: ChainEpochId::new(cursor.chain_epoch_id),
        projection_tip_height: cursor.projection_tip_height,
        projection_tip_hash: cursor.projection_tip_hash,
        revision: cursor.projection_revision,
        coverage: None,
    };
    Ok((Some(cursor), frozen_projection_state))
}

fn decode_and_validate_block_production_cursor_request(
    request: &BlockProductionInTimeRangeRequest,
) -> Result<BlockProductionTimeCursorEnvelope, Status> {
    let cursor = decode_block_production_cursor(&request.from_cursor)?;
    let request_epoch_matches = request
        .at_epoch_id
        .is_none_or(|chain_epoch_id| chain_epoch_id == cursor.chain_epoch_id);
    if cursor.start_time_unix_seconds != request.start_time_unix_seconds
        || cursor.end_time_unix_seconds != request.end_time_unix_seconds
        || !request_epoch_matches
    {
        return Err(ExplorerError::invalid_request(
            "block-production cursor does not match the request bounds or chain epoch",
        )
        .into());
    }
    Ok(cursor)
}

fn decode_block_production_cursor(
    bytes: &[u8],
) -> Result<BlockProductionTimeCursorEnvelope, Status> {
    if bytes.len() < BLOCK_PRODUCTION_CURSOR_FIXED_LEN
        || bytes.first() != Some(&BLOCK_PRODUCTION_CURSOR_VERSION)
    {
        return Err(ExplorerError::invalid_request("block-production cursor is malformed").into());
    }
    let mut offset = 1;
    let start_time_unix_seconds = i64::from_be_bytes(cursor_field(bytes, &mut offset)?);
    let end_time_unix_seconds = i64::from_be_bytes(cursor_field(bytes, &mut offset)?);
    let chain_epoch_id = u64::from_be_bytes(cursor_field(bytes, &mut offset)?);
    let projection_revision = u64::from_be_bytes(cursor_field(bytes, &mut offset)?);
    let projection_tip_height =
        BlockHeight::new(u32::from_be_bytes(cursor_field(bytes, &mut offset)?));
    let projection_tip_hash = decode_internal_block_hash(&bytes[offset..offset + 32])
        .map_err(|_| ExplorerError::invalid_request("block-production cursor hash is malformed"))?;
    offset += 32;
    let after_len = usize::from(u16::from_be_bytes(cursor_field(bytes, &mut offset)?));
    if bytes.len() != offset.saturating_add(after_len) {
        return Err(ExplorerError::invalid_request("block-production cursor is malformed").into());
    }
    let after = BlockProductionTimeCursor::from_bytes(&bytes[offset..])
        .map_err(|_| ExplorerError::invalid_request("block-production cursor key is malformed"))?;
    Ok(BlockProductionTimeCursorEnvelope {
        start_time_unix_seconds,
        end_time_unix_seconds,
        chain_epoch_id,
        projection_revision,
        projection_tip_height,
        projection_tip_hash,
        after,
    })
}

fn cursor_field<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], Status> {
    let end = offset.saturating_add(N);
    let field = bytes
        .get(*offset..end)
        .ok_or_else(|| ExplorerError::invalid_request("block-production cursor is malformed"))?;
    *offset = end;
    field
        .try_into()
        .map_err(|_| ExplorerError::invalid_request("block-production cursor is malformed").into())
}

fn validate_block_view_range(start_height: u32, end_height: u32) -> Result<u32, Status> {
    if end_height < start_height {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_BLOCK_SUMMARIES_PER_REQUEST) {
        return Err(ExplorerError::invalid_request(format!(
            "requested span {span} blocks exceeds the per-request cap of \
             {MAX_BLOCK_SUMMARIES_PER_REQUEST}",
        ))
        .into());
    }
    Ok(u32::try_from(span).unwrap_or(MAX_BLOCK_SUMMARIES_PER_REQUEST))
}

fn read_materialized_block_summaries(
    materialized_view_store: &MaterializedViewStore,
    start_height: u32,
    end_height: u32,
) -> Result<Vec<BlockSummary>, Status> {
    read_materialized_block_records(materialized_view_store, start_height, end_height)?
        .into_iter()
        .map(|record| {
            record
                .summary
                .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing").into())
        })
        .collect()
}

fn read_materialized_block_records(
    materialized_view_store: &MaterializedViewStore,
    start_height: u32,
    end_height: u32,
) -> Result<Vec<BlockSummaryRecord>, Status> {
    let start_key = encode_height_key_ascending(BlockHeight::new(start_height));
    let end_key = encode_height_key_ascending(BlockHeight::new(end_height));
    materialized_view_store
        .range_iterate_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_BLOCK_SUMMARIES_PER_REQUEST as usize,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .into_iter()
        .map(|(_, payload)| {
            BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
                ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}")).into()
            })
        })
        .collect()
}

fn read_coinbase_artifacts(
    reader: &ChainEpochReader<'_>,
    records: &[BlockSummaryRecord],
) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, Status> {
    let mut seen = HashSet::new();
    let mut transaction_ids = Vec::with_capacity(records.len());
    for record in records {
        let Some(transaction_id) = record.transaction_ids.first() else {
            continue;
        };
        let transaction_id = decode_rpc_transaction_id_hex(transaction_id)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if !seen.insert(transaction_id) {
            return Err(ExplorerError::internal(
                "BlockSummaryRecord range contains a duplicate coinbase transaction id",
            )
            .into());
        }
        transaction_ids.push(transaction_id);
    }
    reader
        .transaction_facts_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))
}

fn attach_coinbase_summaries(
    points: &mut [BlockProductionPoint],
    records: &[BlockSummaryRecord],
    artifacts: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
) -> Result<(), Status> {
    let mut coinbase_by_height = HashMap::with_capacity(records.len());
    for record in records {
        let summary = record
            .summary
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
        let Some(transaction_id) = record.transaction_ids.first() else {
            continue;
        };
        let transaction_id = decode_rpc_transaction_id_hex(transaction_id)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if coinbase_by_height
            .insert(summary.block_height, transaction_id)
            .is_some()
        {
            return Err(ExplorerError::internal(
                "BlockSummaryRecord range contains a duplicate block height",
            )
            .into());
        }
    }
    for point in points {
        let summary = point
            .summary
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("BlockProductionPoint.summary missing"))?;
        let Some(transaction_id) = coinbase_by_height.get(&summary.block_height) else {
            continue;
        };
        let Some(artifact) = artifacts.get(transaction_id).and_then(Option::as_ref) else {
            continue;
        };
        validate_coinbase_artifact(summary, *transaction_id, artifact)?;
        point.coinbase = Some(CoinbaseTransactionSummary {
            transaction_id: encode_rpc_transaction_id_hex(*transaction_id),
            transparent_outputs: artifact
                .transparent_outputs
                .iter()
                .map(|output| wallet::TransparentOutput {
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key.clone(),
                })
                .collect(),
            has_shielded_outputs: Some(artifact.public_facts.counts.has_shielded_output()),
        });
    }
    Ok(())
}

fn validate_coinbase_artifact(
    summary: &BlockSummary,
    transaction_id: TransactionId,
    artifact: &TransactionFactsArtifact,
) -> Result<(), Status> {
    let expected_block_hash = decode_rpc_block_hash_hex(&summary.block_hash)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let location = artifact.location;
    if location.transaction_id != transaction_id
        || location.block_height.value() != summary.block_height
        || location.block_hash != expected_block_hash
        || location.tx_index_in_block != 0
        || artifact.public_facts.transaction_id != transaction_id
        || !artifact.public_facts.is_coinbase
    {
        return Err(ExplorerError::internal(
            "canonical coinbase transaction fact does not match its block production point",
        )
        .into());
    }
    Ok(())
}

fn join_block_production_points(
    summaries: Vec<BlockSummary>,
    headers: Vec<Option<zinder_core::BlockHeaderArtifact>>,
    start_height: u32,
    canonical_tip: u32,
) -> Vec<BlockProductionPoint> {
    let mut summaries_by_height: HashMap<u32, BlockSummary> = summaries
        .into_iter()
        .map(|summary| (summary.block_height, summary))
        .collect();
    headers
        .into_iter()
        .enumerate()
        .filter_map(|(offset, header)| {
            let height = start_height.checked_add(u32::try_from(offset).ok()?)?;
            let header = header?;
            let mut summary = summaries_by_height.remove(&height)?;
            if header.height.value() != height
                || summary.block_hash != encode_rpc_block_hash_hex(header.block_hash)
                || summary.block_time_unix_seconds != header.block_time
            {
                return None;
            }
            annotate_request_time_fields(&mut summary, canonical_tip);
            Some(BlockProductionPoint {
                summary: Some(summary),
                bits: header.bits,
                coinbase: None,
            })
        })
        .collect()
}

pub(crate) async fn handle_block_detail(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockDetailRequest>,
) -> Result<Response<BlockDetailResponse>, Status> {
    let inner = request.into_inner();
    let materialized =
        read_materialized_block_view(materialized_view_store, wallet_client, &inner).await?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_BLOCK_DETAIL_V1,
            Some(materialized.chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(BlockDetailResponse {
        freshness: Some(freshness),
        summary: Some(materialized.summary),
        transaction_ids: materialized.transaction_ids,
    }))
}

pub(crate) async fn handle_block_transactions(
    chain_store: &SecondaryChainStore,
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockDetailRequest>,
) -> Result<Response<BlockTransactionsResponse>, Status> {
    let inner = request.into_inner();
    let materialized =
        read_materialized_block_view(materialized_view_store, wallet_client, &inner).await?;
    let transactions =
        read_block_transaction_rows(chain_store, materialized_view_store, &materialized)?;
    let final_note_commitment_roots =
        read_block_final_note_commitment_roots(chain_store, &materialized)?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_BLOCK_TRANSACTIONS_V2,
            Some(materialized.chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockTransactionsResponse {
        freshness: Some(freshness),
        summary: Some(materialized.summary),
        transactions,
        final_note_commitment_roots,
    }))
}

fn read_block_final_note_commitment_roots(
    chain_store: &SecondaryChainStore,
    materialized: &MaterializedBlockView,
) -> Result<Option<BlockFinalNoteCommitmentRoots>, Status> {
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(materialized.chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = chain_store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;
    reader
        .final_note_commitment_roots_at(BlockHeight::new(materialized.summary.block_height))
        .map(|roots| {
            roots.map(|roots| BlockFinalNoteCommitmentRoots {
                sapling: roots.sapling.map(|root| root.as_bytes().to_vec()),
                orchard: roots.orchard.map(|root| root.as_bytes().to_vec()),
                ironwood: roots.ironwood.map(|root| root.as_bytes().to_vec()),
            })
        })
        .map_err(|error| status_from_store_error(&error))
}

fn read_block_transaction_rows(
    chain_store: &SecondaryChainStore,
    materialized_view_store: &MaterializedViewStore,
    materialized: &MaterializedBlockView,
) -> Result<Vec<BlockTransaction>, Status> {
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(materialized.chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = chain_store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;
    let transaction_ids = materialized
        .transaction_ids
        .iter()
        .map(|transaction_id| {
            decode_rpc_transaction_id_hex(transaction_id)
                .map_err(|error| ExplorerError::internal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let artifacts_by_id = reader
        .transaction_facts_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let parent_ids = parent_transaction_ids(artifacts_by_id.values().flatten());
    let parent_transactions = reader
        .transaction_facts_by_ids(&parent_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let fee_lookup_targets = artifacts_by_id
        .iter()
        .filter_map(|(transaction_id, artifact)| {
            artifact
                .as_ref()
                .filter(|artifact| !artifact.public_facts.is_coinbase)
                .map(|artifact| (*transaction_id, artifact.public_facts.privacy_shape))
        })
        .collect::<Vec<_>>();
    let fee_records = zinder_materialized_views::TransactionFeesConsumer::read_fees_records_many(
        materialized_view_store,
        &fee_lookup_targets,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    encode_block_transaction_rows(
        materialized,
        transaction_ids,
        &artifacts_by_id,
        &parent_transactions,
        &fee_records,
    )
}

fn encode_block_transaction_rows(
    materialized: &MaterializedBlockView,
    transaction_ids: Vec<TransactionId>,
    artifacts_by_id: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    fee_records: &HashMap<TransactionId, zinder_proto::v1::explorer::TransactionFeesRecord>,
) -> Result<Vec<BlockTransaction>, Status> {
    let mut transactions = Vec::with_capacity(materialized.transaction_ids.len());

    for (index, (transaction_id, core_transaction_id)) in materialized
        .transaction_ids
        .iter()
        .zip(transaction_ids)
        .enumerate()
    {
        let transaction_index = u32::try_from(index)
            .map_err(|_| ExplorerError::internal("block transaction index exceeds u32"))?;
        let artifact = artifacts_by_id
            .get(&core_transaction_id)
            .and_then(Option::as_ref);
        let public_facts = artifact.map(|artifact| encode_public_facts(&artifact.public_facts));
        let transparent_outputs = artifact.map_or_else(Vec::new, |artifact| {
            artifact
                .transparent_outputs
                .iter()
                .map(|output| wallet::TransparentOutput {
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key.clone(),
                })
                .collect()
        });
        let transparent_inputs = artifact.map_or_else(Vec::new, |artifact| {
            encode_mined_transparent_inputs(
                artifact,
                parent_transactions,
                fee_records.get(&core_transaction_id),
            )
        });
        transactions.push(BlockTransaction {
            transaction_index,
            transaction_id: transaction_id.clone(),
            public_facts,
            transparent_outputs,
            transparent_inputs,
        });
    }

    Ok(transactions)
}

async fn read_materialized_block_view(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: &BlockDetailRequest,
) -> Result<MaterializedBlockView, Status> {
    let height = resolve_block_height(wallet_client, request).await?;
    let key = encode_height_key_ascending(BlockHeight::new(height));
    let payload = materialized_view_store
        .get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &key)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "BlockSummary is not materialized for height {height}"
            ))
        })?;
    let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
        ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
    })?;
    let mut summary = record
        .summary
        .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
    let (chain_epoch, canonical_tip) = read_canonical_tip(wallet_client).await?;
    annotate_request_time_fields(&mut summary, canonical_tip);

    Ok(MaterializedBlockView {
        summary,
        transaction_ids: record.transaction_ids,
        chain_epoch,
    })
}

fn annotate_request_time_fields(
    summary: &mut zinder_proto::v1::explorer::BlockSummary,
    canonical_tip: u32,
) {
    summary.confirmations = canonical_tip
        .saturating_sub(summary.block_height)
        .saturating_add(1);
    summary.is_canonical = summary.block_height <= canonical_tip;
}

async fn resolve_block_height(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: &BlockDetailRequest,
) -> Result<u32, Status> {
    match request
        .selector
        .as_ref()
        .ok_or_else(|| ExplorerError::invalid_request("BlockDetailRequest.selector is required"))?
    {
        block_detail_request::Selector::BlockHeight(height) => Ok(*height),
        block_detail_request::Selector::BlockHash(hash) => {
            let selector = BlockSelector {
                selector: Some(block_selector::Selector::Hash(hash.clone())),
            };
            let response = wallet_client
                .block_id_by_selector(Request::new(wallet::BlockSelectorRequest {
                    selector: Some(selector),
                    at_epoch_id: request.at_epoch_id,
                }))
                .await?
                .into_inner();
            let block_id = response
                .block_id
                .ok_or_else(|| ExplorerError::internal("BlockIdResponse.block_id missing"))?;
            Ok(block_id.height)
        }
    }
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

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use std::collections::HashMap;

    use super::{
        attach_coinbase_summaries, block_production_time_coverage,
        block_production_time_read_fence, decode_and_validate_block_production_cursor_request,
        encode_block_production_cursor, join_block_production_points,
    };
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpochId, TransactionFactsArtifact,
        TransactionId, TransactionLocation, TransactionVersion, TransparentAddressScriptHash,
        TransparentOutputFact,
        wire::{
            encode_height_key_ascending, encode_internal_block_hash, encode_rpc_block_hash_hex,
            encode_rpc_transaction_id_hex,
        },
    };
    use zinder_materialized_views::{
        BlockProductionTimeBackfillCoverage, BlockProductionTimeCursor, ConsumerProjectionState,
    };
    use zinder_proto::v1::explorer::{
        BlockProductionInTimeRangeRequest, BlockProductionPoint, BlockSummary, BlockSummaryRecord,
    };
    use zinder_testkit::synthetic_transaction_public_facts;

    #[test]
    fn block_production_attaches_validated_coinbase_outputs() -> eyre::Result<()> {
        let block_hash = BlockHash::from_bytes([1; 32]);
        let transaction_id = TransactionId::from_bytes([2; 32]);
        let summary = BlockSummary {
            block_height: 10,
            block_hash: encode_rpc_block_hash_hex(block_hash),
            ..Default::default()
        };
        let record = BlockSummaryRecord {
            summary: Some(summary.clone()),
            transaction_ids: vec![encode_rpc_transaction_id_hex(transaction_id)],
            ..Default::default()
        };
        let mut public_facts = synthetic_transaction_public_facts(transaction_id, 64);
        public_facts.is_coinbase = true;
        public_facts.version = TransactionVersion::V5;
        public_facts.unsupported_sections.clear();
        public_facts.counts.transparent_output_count = 1;
        let script_pub_key = vec![0x51];
        let artifact = TransactionFactsArtifact::new(
            TransactionLocation::new(transaction_id, BlockHeight::new(10), block_hash, 0),
            public_facts,
        )
        .with_transparent_facts(
            Vec::new(),
            vec![TransparentOutputFact::new(
                0,
                137_500_000,
                script_pub_key.clone(),
                TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
            )],
        );
        let artifacts = HashMap::from([(transaction_id, Some(artifact))]);
        let mut points = vec![BlockProductionPoint {
            summary: Some(summary),
            bits: 0,
            coinbase: None,
        }];

        attach_coinbase_summaries(&mut points, std::slice::from_ref(&record), &artifacts)?;

        let coinbase = points[0]
            .coinbase
            .as_ref()
            .ok_or_else(|| eyre::eyre!("coinbase summary missing"))?;
        assert_eq!(
            coinbase.transaction_id,
            encode_rpc_transaction_id_hex(transaction_id)
        );
        assert_eq!(coinbase.transparent_outputs.len(), 1);
        assert_eq!(coinbase.transparent_outputs[0].value_zat, 137_500_000);
        assert_eq!(coinbase.has_shielded_outputs, Some(false));

        points[0].coinbase = None;
        let unavailable_artifacts = HashMap::from([(transaction_id, None)]);
        attach_coinbase_summaries(&mut points, &[record], &unavailable_artifacts)?;
        assert!(points[0].coinbase.is_none());
        Ok(())
    }

    #[test]
    fn block_production_join_omits_mixed_epoch_rows() {
        let matching_hash = BlockHash::from_bytes([1; 32]);
        let mismatched_hash = BlockHash::from_bytes([2; 32]);
        let summaries = vec![
            BlockSummary {
                block_height: 10,
                block_hash: encode_rpc_block_hash_hex(matching_hash),
                block_time_unix_seconds: 1_000,
                ..Default::default()
            },
            BlockSummary {
                block_height: 11,
                block_hash: encode_rpc_block_hash_hex(mismatched_hash),
                block_time_unix_seconds: 1_075,
                ..Default::default()
            },
        ];
        let headers = vec![
            Some(block_header(10, matching_hash, 1_000, 0x1f34_bb90)),
            Some(block_header(11, matching_hash, 1_075, 0x1f34_bb90)),
        ];

        let points = join_block_production_points(summaries, headers, 10, 11);

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].bits, 0x1f34_bb90);
        assert_eq!(
            points[0]
                .summary
                .as_ref()
                .map(|summary| summary.block_height),
            Some(10)
        );
    }

    #[test]
    fn block_production_time_cursor_binds_request_and_projection_fence() -> eyre::Result<()> {
        let state = projection_state(9);
        let after = cursor_after(
            1_774_670_100,
            BlockHeight::new(100),
            state.projection_tip_hash,
        )?;
        let request = BlockProductionInTimeRangeRequest {
            start_time_unix_seconds: 1_774_670_000,
            end_time_unix_seconds: 1_774_671_000,
            max_entries: 2,
            from_cursor: encode_block_production_cursor(1_774_670_000, 1_774_671_000, state, after),
            at_epoch_id: Some(state.projection_epoch_id.value()),
        };

        let decoded = decode_and_validate_block_production_cursor_request(&request)?;
        assert_eq!(decoded.chain_epoch_id, state.projection_epoch_id.value());
        assert_eq!(decoded.projection_revision, state.revision);
        assert_eq!(decoded.projection_tip_height, state.projection_tip_height);
        assert_eq!(decoded.projection_tip_hash, state.projection_tip_hash);
        assert_eq!(decoded.after, after);

        let mismatched_bounds = BlockProductionInTimeRangeRequest {
            start_time_unix_seconds: 1_774_669_999,
            ..request.clone()
        };
        let bounds_error = decode_and_validate_block_production_cursor_request(&mismatched_bounds)
            .err()
            .ok_or_else(|| eyre::eyre!("changed time bounds should reject the cursor"))?;
        assert_eq!(bounds_error.code(), tonic::Code::InvalidArgument);

        let mismatched_epoch = BlockProductionInTimeRangeRequest {
            at_epoch_id: Some(state.projection_epoch_id.value() + 1),
            ..request
        };
        let epoch_error = decode_and_validate_block_production_cursor_request(&mismatched_epoch)
            .err()
            .ok_or_else(|| eyre::eyre!("changed chain epoch should reject the cursor"))?;
        assert_eq!(epoch_error.code(), tonic::Code::InvalidArgument);
        Ok(())
    }

    #[test]
    fn block_production_time_coverage_and_fence_map_one_projection_state() -> eyre::Result<()> {
        let state = projection_state(9);
        let coverage = BlockProductionTimeBackfillCoverage::new(
            BlockHeight::new(1),
            state.projection_tip_height,
            1_234,
            1_774_670_100,
        );

        let mapped_coverage =
            block_production_time_coverage(Some(coverage), state, Some(1_774_670_100));
        assert_eq!(mapped_coverage.complete_from_height, Some(1));
        assert_eq!(
            mapped_coverage.complete_through_height,
            Some(state.projection_tip_height.value())
        );
        assert_eq!(mapped_coverage.complete_from_time_unix_seconds, Some(1_234));
        assert_eq!(
            mapped_coverage.complete_through_time_unix_seconds,
            Some(1_774_670_100)
        );
        assert!(mapped_coverage.requested_range_complete);

        let incomplete_coverage = block_production_time_coverage(
            Some(BlockProductionTimeBackfillCoverage::new(
                BlockHeight::new(2),
                BlockHeight::new(99),
                1_235,
                1_774_670_099,
            )),
            state,
            Some(1_774_670_100),
        );
        assert!(!incomplete_coverage.requested_range_complete);
        assert!(!block_production_time_coverage(None, state, None).requested_range_complete);

        let read_fence = block_production_time_read_fence(state);
        assert_eq!(read_fence.chain_epoch_id, state.projection_epoch_id.value());
        assert_eq!(read_fence.projection_revision, state.revision);
        assert_eq!(
            read_fence
                .projection_tip
                .ok_or_else(|| eyre::eyre!("read fence projection tip missing"))?
                .height,
            state.projection_tip_height.value()
        );
        Ok(())
    }

    fn projection_state(revision: u64) -> ConsumerProjectionState {
        ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(47),
            projection_tip_height: BlockHeight::new(100),
            projection_tip_hash: BlockHash::from_bytes([0xa5; 32]),
            revision,
            coverage: None,
        }
    }

    fn cursor_after(
        block_time_unix_seconds: i64,
        block_height: BlockHeight,
        block_hash: BlockHash,
    ) -> eyre::Result<BlockProductionTimeCursor> {
        let mut bytes = Vec::with_capacity(44);
        bytes.extend_from_slice(
            &(block_time_unix_seconds.cast_unsigned() ^ (1_u64 << 63)).to_be_bytes(),
        );
        bytes.extend_from_slice(&encode_height_key_ascending(block_height));
        bytes.extend_from_slice(&encode_internal_block_hash(block_hash));
        Ok(BlockProductionTimeCursor::from_bytes(&bytes)?)
    }

    fn block_header(
        height: u32,
        block_hash: BlockHash,
        block_time_unix_seconds: i64,
        bits: u32,
    ) -> BlockHeaderArtifact {
        BlockHeaderArtifact::new(
            BlockHeight::new(height),
            block_hash,
            BlockHash::from_bytes([0; 32]),
            [0; 32],
            [0; 32],
            block_time_unix_seconds,
            bits,
            [0; 32],
            0,
            0,
        )
    }
}
