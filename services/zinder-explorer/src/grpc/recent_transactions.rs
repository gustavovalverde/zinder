//! `ExplorerQuery.RecentTransactions` handler.
//!
//! Streams the newest-first projection materialized by
//! [`zinder_derive::RecentTransactionsConsumer`]
//! out of the consumer-owned `recent_transactions` column family. Joins
//! the per-tx `transaction_fees` rows in a single `multi_get` so the page
//! cost is one prefix scan plus one batched lookup.

use std::{collections::HashMap, pin::Pin};

use prost::Message as _;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use zinder_core::wire::decode_rpc_transaction_id_hex;
use zinder_core::{PrivacyShape, TransactionId};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_RECENT_V1;
use zinder_proto::v1::explorer::{
    RecentTransactionEntry, RecentTransactionsChunk, RecentTransactionsRequest,
    TransactionFeesRecord,
};
use zinder_proto::v1::wallet::{LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_proto::wire::decode_privacy_shape;
use zinder_runtime::AuthenticatedChannel;
use zinder_store::{SecondaryChainStore, chain_epoch_from_message, status_from_store_error};

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use zinder_derive::{DeriveStore, RECENT_TRANSACTIONS_COLUMN_FAMILY, TransactionFeesConsumer};

/// Server-side maximum entries the handler ever returns in one stream.
const MAX_RECENT_TRANSACTIONS_PER_REQUEST: u32 = 1024;

/// Default `max_entries` when the caller passes zero.
const DEFAULT_RECENT_TRANSACTIONS: u32 = 64;

/// Length of one row key in the projection (`reverse_height` + position).
const ROW_KEY_LEN: usize = 8;

/// Stream type returned by the RPC.
pub(crate) type RecentTransactionsStream =
    Pin<Box<dyn Stream<Item = Result<RecentTransactionsChunk, Status>> + Send + 'static>>;

/// Executes one `ExplorerQuery.RecentTransactions` request.
pub(crate) async fn handle_recent_transactions(
    derive_store: &DeriveStore,
    chain_store: Option<&SecondaryChainStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<RecentTransactionsRequest>,
) -> Result<Response<RecentTransactionsStream>, Status> {
    let inner = request.into_inner();
    let max_entries = clamp_max_entries(
        inner.max_entries,
        DEFAULT_RECENT_TRANSACTIONS,
        MAX_RECENT_TRANSACTIONS_PER_REQUEST,
    );
    let cursor_start: Option<[u8; ROW_KEY_LEN]> = if inner.from_cursor.is_empty() {
        None
    } else {
        Some(
            inner
                .from_cursor
                .as_slice()
                .try_into()
                .map_err(|_| ExplorerError::invalid_request("from_cursor must be 8 bytes"))?,
        )
    };
    let start_key = cursor_start.unwrap_or([0u8; ROW_KEY_LEN]);
    let end_key = [0xFFu8; ROW_KEY_LEN];
    // Cursor rows are exclusive, so request one extra row to skip the
    // resume row without short-changing the page.
    let scan_cap = (max_entries as usize).saturating_add(usize::from(cursor_start.is_some()));
    let rows = derive_store
        .range_iterate_consumer(
            RECENT_TRANSACTIONS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            scan_cap,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;

    let mut entries: Vec<RecentTransactionEntry> = Vec::with_capacity(rows.len());
    let mut last_key: Option<[u8; ROW_KEY_LEN]> = None;
    for (key, payload) in rows {
        let key_array: [u8; ROW_KEY_LEN] = key
            .as_slice()
            .try_into()
            .map_err(|_| ExplorerError::internal("recent_transactions row key not 8 bytes"))?;
        if cursor_start.is_some() && key_array == start_key {
            continue;
        }
        let entry = RecentTransactionEntry::decode(payload.as_slice())
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        entries.push(entry);
        last_key = Some(key_array);
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= max_entries {
            break;
        }
    }

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
    join_paid_fees(derive_store, chain_store, &chain_epoch, &mut entries)?;
    let cursor = last_key.map_or_else(Vec::new, |key| key.to_vec());
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_TRANSACTION_RECENT_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;
    let chunk = RecentTransactionsChunk {
        freshness: Some(freshness),
        cursor,
        entries,
    };
    let stream = tokio_stream::iter(std::iter::once(Ok(chunk)));
    Ok(Response::new(Box::pin(stream)))
}

/// Hydrates `entries[*].paid_fee_zat` from the `transaction_fees` projection
/// in a single batched read.
///
/// Coinbase rows are skipped (no fee record exists). Missing fee records
/// leave `paid_fee_zat` unset; that's the explicit "not available" signal
/// per ADR-0018.
fn join_paid_fees(
    derive_store: &DeriveStore,
    chain_store: Option<&SecondaryChainStore>,
    chain_epoch: &zinder_proto::v1::wallet::ChainEpoch,
    entries: &mut [RecentTransactionEntry],
) -> Result<(), Status> {
    let lookup_targets: Vec<(TransactionId, PrivacyShape)> = entries
        .iter()
        .filter(|entry| !entry.is_coinbase)
        .map(|entry| {
            let privacy_shape =
                decode_privacy_shape(entry.privacy_shape).unwrap_or(PrivacyShape::Unclassified);
            decode_rpc_transaction_id_hex(&entry.transaction_id)
                .map(|transaction_id| (transaction_id, privacy_shape))
        })
        .collect::<Result<_, _>>()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if lookup_targets.is_empty() {
        return Ok(());
    }
    let records = TransactionFeesConsumer::read_fees_records_many(derive_store, &lookup_targets)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    for entry in entries.iter_mut() {
        if entry.is_coinbase {
            continue;
        }
        let Ok(transaction_id) = decode_rpc_transaction_id_hex(&entry.transaction_id) else {
            continue;
        };
        if let Some(record) = records.get(&transaction_id) {
            entry.paid_fee_zat = record.paid_fee_zat;
        }
    }
    resolve_missing_transparent_fees(chain_store, chain_epoch, &records, entries)?;
    Ok(())
}

fn resolve_missing_transparent_fees(
    chain_store: Option<&SecondaryChainStore>,
    chain_epoch: &zinder_proto::v1::wallet::ChainEpoch,
    projected_records: &HashMap<TransactionId, TransactionFeesRecord>,
    entries: &mut [RecentTransactionEntry],
) -> Result<(), Status> {
    let unresolved_ids: Vec<TransactionId> = entries
        .iter()
        .filter(|entry| {
            !entry.is_coinbase
                && entry.paid_fee_zat.is_none()
                && decode_privacy_shape(entry.privacy_shape) == Some(PrivacyShape::TransparentOnly)
        })
        .map(|entry| decode_rpc_transaction_id_hex(&entry.transaction_id))
        .collect::<Result<_, _>>()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if unresolved_ids.is_empty() {
        return Ok(());
    }
    let Some(store) = chain_store else {
        return Ok(());
    };

    store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    let transactions = reader
        .transaction_facts_by_ids(&unresolved_ids)
        .map_err(|error| status_from_store_error(&error))?
        .into_values()
        .flatten()
        .collect::<Vec<_>>();
    let resolved =
        TransactionFeesConsumer::resolve_fee_records_from_canonical_facts(&reader, &transactions)
            .map_err(|error| status_from_store_error(&error))?;
    let transactions_by_id: HashMap<TransactionId, _> = transactions
        .iter()
        .map(|transaction| (transaction.location.transaction_id, transaction))
        .collect();
    for entry in entries {
        if entry.paid_fee_zat.is_some() {
            continue;
        }
        let Ok(transaction_id) = decode_rpc_transaction_id_hex(&entry.transaction_id) else {
            continue;
        };
        let transaction = transactions_by_id.get(&transaction_id).copied();
        if let (Some(transaction), Some(recovered)) = (transaction, resolved.get(&transaction_id)) {
            entry.paid_fee_zat = TransactionFeesConsumer::merge_fee_records(
                transaction,
                projected_records.get(&transaction_id),
                recovered,
            )
            .paid_fee_zat;
        }
    }
    Ok(())
}
