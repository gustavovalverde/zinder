//! Writer-owned displaced-block archive handlers.

use std::num::NonZeroU32;

use tonic::{Request, Response, Status};
use zinder_core::wire::{
    decode_rpc_block_hash_hex, encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHash, BlockHeight, DisplacedBlock, DisplacedBlockArchiveCoverage};
use zinder_proto::capabilities::{
    EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1, EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
};
use zinder_proto::v1::explorer::{
    DisplacedBlockArchiveCoverage as WireCoverage, DisplacedBlockCanonicalCounterpart,
    DisplacedBlockCoinbaseOutput, DisplacedBlockDetailRequest, DisplacedBlockDetailResponse,
    DisplacedBlockHistoryEntry, DisplacedBlockHistoryRequest, DisplacedBlockHistoryResponse,
    DisplacedBlockSummary,
};
use zinder_store::{
    ChainEpochReader, DisplacedBlockCursor, DisplacedBlockStore, SecondaryChainStore,
    chain_epoch_message, status_from_store_error,
};

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

const DEFAULT_PAGE_SIZE: u32 = 100;
const MAX_PAGE_SIZE: u32 = 4_096;
const CURSOR_PREFIX: &[u8; 4] = b"zdb1";
const CURSOR_LEN: usize = CURSOR_PREFIX.len() + size_of::<u64>() + size_of::<u32>() + 32;

pub(crate) async fn query_displaced_block_history(
    chain_store: &SecondaryChainStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<DisplacedBlockHistoryRequest>,
) -> Result<Response<DisplacedBlockHistoryResponse>, Status> {
    let request = request.into_inner();
    let page_size = clamp_max_entries(request.page_size, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE);
    let cursor = (!request.cursor.is_empty())
        .then(|| decode_cursor(&request.cursor))
        .transpose()?;
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let reader = chain_store
        .current_chain_epoch_reader()
        .map_err(|error| status_from_store_error(&error))?;
    let chain_epoch = reader.chain_epoch();
    let limit = NonZeroU32::new(page_size)
        .ok_or_else(|| Status::from(ExplorerError::internal("page size resolved to zero")))?;
    let page = chain_store
        .displaced_block_page(cursor.as_ref(), limit)
        .map_err(|error| status_from_store_error(&error))?;
    let total_count = chain_store
        .displaced_block_count()
        .map_err(|error| status_from_store_error(&error))?;
    let coverage = chain_store
        .displaced_block_archive_coverage()
        .map_err(|error| status_from_store_error(&error))?;
    let next_cursor = page.next_cursor.map_or_else(Vec::new, encode_cursor);
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            None,
            EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
            Some(chain_epoch_message(chain_epoch)),
            0,
        )?,
    )
    .await;
    let entries = page
        .blocks
        .into_iter()
        .map(|block| {
            let current_canonical_block = canonical_counterpart(&reader, block.header.height)?;
            Ok(DisplacedBlockHistoryEntry {
                block: Some(map_summary(block)),
                current_canonical_block,
            })
        })
        .collect::<Result<Vec<_>, Status>>()?;
    Ok(Response::new(DisplacedBlockHistoryResponse {
        freshness: Some(freshness),
        entries,
        next_cursor,
        has_more: page.has_more,
        total_count,
        coverage: coverage.map(map_coverage),
    }))
}

pub(crate) async fn query_displaced_block_detail(
    chain_store: &SecondaryChainStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<DisplacedBlockDetailRequest>,
) -> Result<Response<DisplacedBlockDetailResponse>, Status> {
    let block_hash = decode_rpc_block_hash_hex(&request.into_inner().block_hash)
        .map_err(|error| ExplorerError::invalid_request(error.to_string()))?;
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let reader = chain_store
        .current_chain_epoch_reader()
        .map_err(|error| status_from_store_error(&error))?;
    let chain_epoch = reader.chain_epoch();
    let block = chain_store
        .displaced_block_by_hash(block_hash)
        .map_err(|error| status_from_store_error(&error))?
        .ok_or_else(|| {
            Status::from(ExplorerError::not_materialized(
                "displaced block is not archived",
            ))
        })?;
    let current_canonical_block = canonical_counterpart(&reader, block.header.height)?;
    let coverage = chain_store
        .displaced_block_archive_coverage()
        .map_err(|error| status_from_store_error(&error))?;
    let raw_block_bytes = block.raw_block_bytes.clone();
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            None,
            EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
            Some(chain_epoch_message(chain_epoch)),
            0,
        )?,
    )
    .await;
    Ok(Response::new(DisplacedBlockDetailResponse {
        freshness: Some(freshness),
        block: Some(map_summary(block)),
        current_canonical_block,
        raw_block_bytes,
        coverage: coverage.map(map_coverage),
    }))
}

fn map_summary(block: DisplacedBlock) -> DisplacedBlockSummary {
    DisplacedBlockSummary {
        block_height: block.header.height.value(),
        block_hash: encode_rpc_block_hash_hex(block.block_hash),
        previous_block_hash: encode_rpc_block_hash_hex(block.header.parent_hash),
        block_time_unix_seconds: block.header.block_time,
        total_size_bytes: block.header.block_size_bytes,
        difficulty_bits: block.header.bits,
        transaction_ids: block
            .transaction_ids
            .into_iter()
            .map(encode_rpc_transaction_id_hex)
            .collect(),
        coinbase_outputs: block
            .coinbase_outputs
            .into_iter()
            .map(|output| DisplacedBlockCoinbaseOutput {
                output_index: output.output_index,
                value_zat: output.value_zat,
                script_pub_key: output.script_pub_key,
            })
            .collect(),
        displacement_event_sequence: block.displacement_event_sequence,
        displacement_epoch_id: block.displacement_epoch.value(),
        displaced_at_millis: block.displaced_at.value(),
    }
}

fn map_coverage(coverage: DisplacedBlockArchiveCoverage) -> WireCoverage {
    WireCoverage {
        activation_event_sequence: coverage.activation_event_sequence,
        activation_epoch_id: coverage.activation_epoch.value(),
        activated_at_millis: coverage.activated_at.value(),
    }
}

fn canonical_counterpart(
    reader: &ChainEpochReader<'_>,
    height: BlockHeight,
) -> Result<Option<DisplacedBlockCanonicalCounterpart>, Status> {
    let Some(header) = reader
        .block_header_at(height)
        .map_err(|error| status_from_store_error(&error))?
    else {
        return Ok(None);
    };
    let transaction_ids = reader
        .transaction_ids_at_height(height)
        .map_err(|error| status_from_store_error(&error))?;
    let coinbase_outputs = transaction_ids
        .first()
        .copied()
        .map(|transaction_id| {
            reader
                .transaction_facts_by_id(transaction_id)
                .map_err(|error| status_from_store_error(&error))
        })
        .transpose()?
        .flatten()
        .map_or_else(Vec::new, |facts| {
            facts
                .transparent_outputs
                .into_iter()
                .map(|output| DisplacedBlockCoinbaseOutput {
                    output_index: output.output_index,
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key,
                })
                .collect()
        });
    Ok(Some(DisplacedBlockCanonicalCounterpart {
        block_height: height.value(),
        block_hash: encode_rpc_block_hash_hex(header.block_hash),
        previous_block_hash: encode_rpc_block_hash_hex(header.parent_hash),
        block_time_unix_seconds: header.block_time,
        total_size_bytes: header.block_size_bytes,
        difficulty_bits: header.bits,
        transaction_count: u32::try_from(transaction_ids.len()).unwrap_or(u32::MAX),
        coinbase_outputs,
    }))
}

fn encode_cursor(cursor: DisplacedBlockCursor) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(CURSOR_LEN);
    encoded.extend_from_slice(CURSOR_PREFIX);
    encoded.extend_from_slice(&cursor.event_sequence().to_be_bytes());
    encoded.extend_from_slice(&cursor.height().value().to_be_bytes());
    encoded.extend_from_slice(&cursor.block_hash().as_bytes());
    encoded
}

fn decode_cursor(cursor: &[u8]) -> Result<DisplacedBlockCursor, Status> {
    if cursor.len() != CURSOR_LEN || &cursor[..CURSOR_PREFIX.len()] != CURSOR_PREFIX {
        return Err(ExplorerError::invalid_request("displaced-block cursor is malformed").into());
    }
    let event_start = CURSOR_PREFIX.len();
    let height_start = event_start + size_of::<u64>();
    let hash_start = height_start + size_of::<u32>();
    let event_sequence = u64::from_be_bytes(
        cursor[event_start..height_start]
            .try_into()
            .map_err(|_| ExplorerError::invalid_request("cursor event is malformed"))?,
    );
    let height = u32::from_be_bytes(
        cursor[height_start..hash_start]
            .try_into()
            .map_err(|_| ExplorerError::invalid_request("cursor height is malformed"))?,
    );
    let hash_bytes: [u8; 32] = cursor[hash_start..]
        .try_into()
        .map_err(|_| ExplorerError::invalid_request("cursor hash is malformed"))?;
    Ok(DisplacedBlockCursor::from_position(
        event_sequence,
        BlockHeight::new(height),
        BlockHash::from_bytes(hash_bytes),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_malformed_bytes() -> Result<(), Status> {
        let cursor = DisplacedBlockCursor::from_position(
            42,
            BlockHeight::new(100),
            BlockHash::from_bytes([0x55; 32]),
        );
        assert_eq!(decode_cursor(&encode_cursor(cursor))?, cursor);
        assert!(decode_cursor(b"bad").is_err());
        Ok(())
    }
}
