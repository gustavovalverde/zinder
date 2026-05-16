//! `ExplorerQuery.TransparentAddressActivity` handler.
//!
//! Composes `WalletQuery.TransparentAddressTxIdsInRange` (confirmed
//! history, server-streamed) with
//! `WalletQuery.TransparentMempoolOutputsByAddress` (mempool overlay,
//! single point lookup) so a single call returns the unified activity
//! feed an explorer page renders. The mempool overlay is emitted only
//! on the first page (`from_cursor` empty and `include_mempool=true`)
//! so subsequent pages stay deterministic; clients that need refreshed
//! mempool state restart pagination.

use std::collections::{HashMap, HashSet};

use tonic::{Request, Response, Status};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1;
use zinder_proto::v1::explorer::{
    ExplorerFreshness, TransparentAddressActivityEntry, TransparentAddressActivityRequest,
    TransparentAddressActivityResponse,
};
use zinder_proto::v1::wallet::{
    self, LatestBlockRequest, MempoolSnapshotRequest, TransparentAddressTxIdsInRangeRequest,
    TransparentMempoolOutputsByAddressRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

/// Hard cap on the mempool snapshot the overlay joins against.
///
/// The mempool snapshot drives the per-entry `first_seen_unix_millis`
/// timestamp; if the snapshot misses a tx the overlay returns the entry
/// with `first_seen_unix_millis = 0` (the wire shape's documented
/// fallback). Matches the cap in `mempool.rs` for consistency.
const MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_REQUEST: u32 = 4_096;

/// Hard cap on the activity rows one page returns.
const MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST: u32 = 256;

/// Default entries when the caller passes `max_entries = 0`.
const DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES: u32 = 64;

/// Executes one `ExplorerQuery.TransparentAddressActivity` request.
pub(crate) async fn handle_transparent_address_activity(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: Request<TransparentAddressActivityRequest>,
) -> Result<Response<TransparentAddressActivityResponse>, Status> {
    let inner = request.into_inner();
    let max_entries = clamp_max_entries(
        inner.max_entries,
        DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES,
        MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST,
    );
    let address = inner
        .address
        .ok_or_else(|| Status::invalid_argument("address selector is required"))?;
    let is_first_page = inner.from_cursor.is_empty();
    let want_mempool = inner.include_mempool && is_first_page;

    let mempool_entries = if want_mempool {
        load_mempool_overlay(wallet_client, address.clone()).await?
    } else {
        Vec::new()
    };

    let mempool_transaction_ids: HashSet<Vec<u8>> = mempool_entries
        .iter()
        .map(|entry| entry.transaction_id.clone())
        .collect();
    let max_confirmed =
        u32::try_from(mempool_entries.len()).map_or(0, |count| max_entries.saturating_sub(count));

    let (confirmed_entries, next_cursor, chain_epoch) = load_confirmed_history(
        wallet_client,
        address,
        ConfirmedHistoryRequest {
            start_height: inner.start_height,
            end_height: inner.end_height,
            descending: inner.descending,
            max_entries: max_confirmed,
            from_cursor: inner.from_cursor,
            at_epoch: inner.at_epoch,
            skip_transaction_ids: mempool_transaction_ids,
        },
    )
    .await?;

    let mut entries = Vec::with_capacity(mempool_entries.len() + confirmed_entries.len());
    if inner.descending {
        entries.extend(mempool_entries);
        entries.extend(confirmed_entries);
    } else {
        entries.extend(confirmed_entries);
        entries.extend(mempool_entries);
    }

    let freshness = ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: 0,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1.to_owned(),
        unavailable: Vec::new(),
    };

    Ok(Response::new(TransparentAddressActivityResponse {
        freshness: Some(freshness),
        entries,
        next_cursor,
    }))
}

async fn load_mempool_overlay(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    address: wallet::AddressLookup,
) -> Result<Vec<TransparentAddressActivityEntry>, Status> {
    let outputs_response = wallet_client
        .transparent_mempool_outputs_by_address(Request::new(
            TransparentMempoolOutputsByAddressRequest {
                address: Some(address),
                max_entries: None,
            },
        ))
        .await?
        .into_inner();
    if outputs_response.outputs.is_empty() {
        return Ok(Vec::new());
    }

    let snapshot = wallet_client
        .mempool_snapshot(Request::new(MempoolSnapshotRequest {
            max_entries: MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_REQUEST,
            from_cursor: Vec::new(),
        }))
        .await?
        .into_inner();
    let first_seen_by_txid: HashMap<Vec<u8>, u64> = snapshot
        .entries
        .into_iter()
        .map(|entry| (entry.transaction_id, entry.first_seen_unix_millis))
        .collect();

    let mut deduplicated: Vec<TransparentAddressActivityEntry> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for output in outputs_response.outputs {
        let Some(outpoint) = output.outpoint else {
            continue;
        };
        if !seen.insert(outpoint.transaction_id.clone()) {
            continue;
        }
        let first_seen_unix_millis = first_seen_by_txid
            .get(&outpoint.transaction_id)
            .copied()
            .unwrap_or(0);
        deduplicated.push(TransparentAddressActivityEntry {
            transaction_id: outpoint.transaction_id,
            block_height: 0,
            block_hash: Vec::new(),
            tx_index_in_block: 0,
            in_mempool: true,
            first_seen_unix_millis,
        });
    }
    deduplicated.sort_by(|left, right| {
        right
            .first_seen_unix_millis
            .cmp(&left.first_seen_unix_millis)
    });
    Ok(deduplicated)
}

struct ConfirmedHistoryRequest {
    start_height: u32,
    end_height: u32,
    descending: bool,
    max_entries: u32,
    from_cursor: Vec<u8>,
    at_epoch: Option<wallet::ChainEpoch>,
    skip_transaction_ids: HashSet<Vec<u8>>,
}

async fn load_confirmed_history(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    address: wallet::AddressLookup,
    request: ConfirmedHistoryRequest,
) -> Result<
    (
        Vec<TransparentAddressActivityEntry>,
        Vec<u8>,
        wallet::ChainEpoch,
    ),
    Status,
> {
    if request.max_entries == 0 {
        // The mempool overlay alone filled the page; emit an empty cursor
        // and fall back to `LatestBlock` for the chain epoch.
        let chain_epoch = fetch_latest_chain_epoch(wallet_client).await?;
        return Ok((Vec::new(), Vec::new(), chain_epoch));
    }

    let mut stream = wallet_client
        .transparent_address_tx_ids_in_range(Request::new(TransparentAddressTxIdsInRangeRequest {
            address: Some(address),
            start_height: request.start_height,
            end_height: request.end_height,
            max_entries: request.max_entries,
            from_cursor: request.from_cursor,
            at_epoch: request.at_epoch,
            descending: request.descending,
        }))
        .await?
        .into_inner();

    let mut entries = Vec::with_capacity(request.max_entries as usize);
    let mut chain_epoch: Option<wallet::ChainEpoch> = None;
    let mut next_cursor = Vec::new();
    while let Some(chunk) = stream.message().await? {
        if chain_epoch.is_none() {
            chain_epoch.clone_from(&chunk.chain_epoch);
        }
        if !chunk.cursor.is_empty() {
            next_cursor.clone_from(&chunk.cursor);
        }
        if request.skip_transaction_ids.contains(&chunk.transaction_id) {
            continue;
        }
        entries.push(TransparentAddressActivityEntry {
            transaction_id: chunk.transaction_id,
            block_height: chunk.block_height,
            block_hash: chunk.block_hash,
            tx_index_in_block: chunk.tx_index_in_block,
            in_mempool: false,
            first_seen_unix_millis: 0,
        });
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= request.max_entries {
            break;
        }
    }
    let chain_epoch = match chain_epoch {
        Some(epoch) => epoch,
        None => fetch_latest_chain_epoch(wallet_client).await?,
    };
    Ok((entries, next_cursor, chain_epoch))
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

const fn clamp_max_entries(requested: u32, default: u32, cap: u32) -> u32 {
    let target = if requested == 0 { default } else { requested };
    if target > cap { cap } else { target }
}
