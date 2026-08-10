//! `ExplorerQuery.RecentTransactions` handler.
//!
//! Streams the newest-first view materialized by
//! [`zinder_materialized_views::RecentTransactionsConsumer`]
//! out of the consumer-owned `recent_transactions` column family. Joins
//! the per-tx `transaction_fees` rows in a single `multi_get` so the page
//! cost is one prefix scan plus one batched lookup.

use std::pin::Pin;

use prost::Message as _;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use zinder_core::wire::decode_rpc_transaction_id_hex;
use zinder_core::{PrivacyShape, TransactionId};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_RECENT_V1;
use zinder_proto::v1::explorer::{
    RecentTransactionEntry, RecentTransactionsChunk, RecentTransactionsRequest,
};
use zinder_proto::v1::wallet::wallet_query_client::WalletQueryClient;
use zinder_proto::wire::decode_privacy_shape;
use zinder_runtime::AuthenticatedChannel;
use zinder_store::CanonicalStoreConstructionIdentity;

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, WalletPinnedBlockSummarySnapshot, attach_upstream_observation,
    build_explorer_freshness_from_snapshot, pin_wallet_to_block_summary_snapshot,
};
use zinder_materialized_views::{
    MaterializedViewState, MaterializedViewStore, MaterializedViewStoreReadSnapshot,
    RECENT_TRANSACTIONS_COLUMN_FAMILY, RECENT_TRANSACTIONS_CONSUMER_NAME,
    TRANSACTION_FEES_CONSUMER_NAME, TransactionFeesConsumer,
};

/// Server-side maximum entries the handler ever returns in one stream.
const MAX_RECENT_TRANSACTIONS_PER_REQUEST: u32 = 1024;

/// Default `max_entries` when the caller passes zero.
const DEFAULT_RECENT_TRANSACTIONS: u32 = 64;

/// Length of one row key in the materialized view (`reverse_height` + position).
const ROW_KEY_LEN: usize = 8;

/// Version byte for a cursor bound to one admitted construction and read fence.
const RECENT_CURSOR_VERSION: u8 = 1;

/// Stream type returned by the RPC.
pub(crate) type RecentTransactionsStream =
    Pin<Box<dyn Stream<Item = Result<RecentTransactionsChunk, Status>> + Send + 'static>>;

/// Executes one `ExplorerQuery.RecentTransactions` request.
#[allow(
    clippy::too_many_lines,
    reason = "the streaming handler keeps admission, hydration, and response construction together"
)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "the Wallet-pinned snapshot must span recent rows, optional fees, cursor, and response freshness"
)]
pub(crate) async fn query_recent_transactions(
    materialized_view_store: &MaterializedViewStore,
    include_transaction_fees: bool,
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
    let (entries, cursor, freshness) = {
        let pinned =
            pin_wallet_to_block_summary_snapshot(materialized_view_store, wallet_client).await?;
        require_recent_transactions_snapshot_coherence(&pinned, include_transaction_fees)?;
        let state = pinned.block_summary_state();
        let cursor_start = decode_recent_cursor(
            &inner.from_cursor,
            materialized_view_store.construction_identity(),
            state,
        )?;
        let snapshot = pinned.snapshot();
        let (mut entries, last_key) =
            read_recent_transaction_entries(snapshot, cursor_start, max_entries)?;
        if include_transaction_fees {
            join_paid_fees_snapshot(snapshot, &mut entries)?;
        }
        let cursor = last_key.map_or_else(Vec::new, |key| {
            encode_recent_cursor(materialized_view_store.construction_identity(), state, key)
        });
        let freshness = build_explorer_freshness_from_snapshot(
            snapshot,
            EXPLORER_TRANSACTION_RECENT_V1,
            Some(pinned.wallet_chain_epoch().clone()),
            0,
        )?;
        (entries, cursor, freshness)
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    let chunk = RecentTransactionsChunk {
        freshness: Some(freshness),
        cursor,
        entries,
    };
    let stream = tokio_stream::iter(std::iter::once(Ok(chunk)));
    Ok(Response::new(Box::pin(stream)))
}

/// Hydrates `entries[*].paid_fee_zat` from the `transaction_fees` materialized view
/// in a single batched read.
///
/// Coinbase rows are skipped (no fee record exists). Missing fee records
/// leave `paid_fee_zat` unset; that's the explicit "not available" signal
/// per ADR-0018.
fn join_paid_fees_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
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
    let records =
        TransactionFeesConsumer::read_fees_records_many_snapshot(snapshot, &lookup_targets)
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
    Ok(())
}

fn read_recent_transaction_entries(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    cursor_start: Option<[u8; ROW_KEY_LEN]>,
    max_entries: u32,
) -> Result<(Vec<RecentTransactionEntry>, Option<[u8; ROW_KEY_LEN]>), Status> {
    let start_key = cursor_start.unwrap_or([0u8; ROW_KEY_LEN]);
    let end_key = [0xFFu8; ROW_KEY_LEN];
    // Cursor rows are exclusive, so request one extra row to skip the resume
    // row without short-changing the page.
    let scan_cap = (max_entries as usize).saturating_add(usize::from(cursor_start.is_some()));
    let rows = snapshot
        .range_iterate_consumer(
            RECENT_TRANSACTIONS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            scan_cap,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut entries = Vec::with_capacity(rows.len());
    let mut last_key = None;
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
    Ok((entries, last_key))
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "one borrowed snapshot must span every retained consumer state and checkpoint comparison"
)]
fn require_recent_transactions_snapshot_coherence(
    pinned: &WalletPinnedBlockSummarySnapshot<'_>,
    include_transaction_fees: bool,
) -> Result<(), Status> {
    let snapshot = pinned.snapshot();
    let block_summary_state = pinned.block_summary_state();
    require_matching_recent_transactions_state(
        snapshot,
        RECENT_TRANSACTIONS_CONSUMER_NAME,
        block_summary_state,
    )?;
    if include_transaction_fees {
        require_matching_recent_transactions_state(
            snapshot,
            TRANSACTION_FEES_CONSUMER_NAME,
            block_summary_state,
        )?;
    }
    for consumer in recent_transaction_checkpoint_consumers(include_transaction_fees) {
        let checkpoint = snapshot
            .chain_event_checkpoint(*consumer)
            .map_err(|error| ExplorerError::internal(error.to_string()))?
            .ok_or_else(|| {
                ExplorerError::not_materialized(format!(
                    "{} chain-event checkpoint is unavailable",
                    consumer.as_str(),
                ))
            })?;
        if checkpoint != pinned.block_summary_checkpoint() {
            return Err(ExplorerError::not_materialized(format!(
                "{} checkpoint does not match the Block Summary snapshot",
                consumer.as_str(),
            ))
            .into());
        }
    }
    Ok(())
}

fn recent_transaction_checkpoint_consumers(
    include_transaction_fees: bool,
) -> &'static [zinder_materialized_views::MaterializedViewConsumerName] {
    if include_transaction_fees {
        &[
            RECENT_TRANSACTIONS_CONSUMER_NAME,
            TRANSACTION_FEES_CONSUMER_NAME,
        ]
    } else {
        &[RECENT_TRANSACTIONS_CONSUMER_NAME]
    }
}

fn require_matching_recent_transactions_state(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    consumer: zinder_materialized_views::MaterializedViewConsumerName,
    block_summary_state: MaterializedViewState,
) -> Result<(), Status> {
    let state = snapshot
        .consumer_state(consumer)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "{} materialized-view state is unavailable",
                consumer.as_str(),
            ))
        })?;
    if state != block_summary_state {
        return Err(ExplorerError::not_materialized(format!(
            "{} state does not match the Block Summary snapshot",
            consumer.as_str(),
        ))
        .into());
    }
    Ok(())
}

fn encode_recent_cursor(
    construction_identity: CanonicalStoreConstructionIdentity,
    state: MaterializedViewState,
    row_key: [u8; ROW_KEY_LEN],
) -> Vec<u8> {
    let identity = construction_identity.encode_persisted();
    let mut cursor = Vec::with_capacity(1 + identity.len() + 8 + 4 + 32 + 8 + ROW_KEY_LEN);
    cursor.push(RECENT_CURSOR_VERSION);
    cursor.extend_from_slice(&identity);
    cursor.extend_from_slice(&state.chain_epoch_id.value().to_be_bytes());
    cursor.extend_from_slice(&state.tip_height.value().to_be_bytes());
    cursor.extend_from_slice(&state.tip_hash.as_bytes());
    cursor.extend_from_slice(&state.revision.to_be_bytes());
    cursor.extend_from_slice(&row_key);
    cursor
}

fn decode_recent_cursor(
    encoded: &[u8],
    construction_identity: CanonicalStoreConstructionIdentity,
    state: MaterializedViewState,
) -> Result<Option<[u8; ROW_KEY_LEN]>, Status> {
    if encoded.is_empty() {
        return Ok(None);
    }
    let identity = construction_identity.encode_persisted();
    let expected_length = 1 + identity.len() + 8 + 4 + 32 + 8 + ROW_KEY_LEN;
    if encoded.len() != expected_length || encoded.first() != Some(&RECENT_CURSOR_VERSION) {
        return Err(
            ExplorerError::invalid_request("Recent Transactions cursor is malformed").into(),
        );
    }
    let identity_start = 1;
    let identity_end = identity_start + identity.len();
    let epoch_end = identity_end + 8;
    let height_end = epoch_end + 4;
    let hash_end = height_end + 32;
    let revision_end = hash_end + 8;
    let encoded_epoch =
        u64::from_be_bytes(encoded[identity_end..epoch_end].try_into().map_err(|_| {
            ExplorerError::invalid_request("Recent Transactions cursor epoch malformed")
        })?);
    let encoded_height =
        u32::from_be_bytes(encoded[epoch_end..height_end].try_into().map_err(|_| {
            ExplorerError::invalid_request("Recent Transactions cursor height malformed")
        })?);
    let encoded_revision =
        u64::from_be_bytes(encoded[hash_end..revision_end].try_into().map_err(|_| {
            ExplorerError::invalid_request("Recent Transactions cursor revision malformed")
        })?);
    if encoded[identity_start..identity_end] != identity
        || encoded_epoch != state.chain_epoch_id.value()
        || encoded_height != state.tip_height.value()
        || encoded[height_end..hash_end] != state.tip_hash.as_bytes()
        || encoded_revision != state.revision
    {
        return Err(ExplorerError::unsatisfied_precondition(
            "Recent Transactions cursor belongs to a different admitted chain lineage or read fence",
        )
        .into());
    }
    let row_key: [u8; ROW_KEY_LEN] = encoded[revision_end..].try_into().map_err(|_| {
        ExplorerError::invalid_request("Recent Transactions cursor row key malformed")
    })?;
    Ok(Some(row_key))
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use tempfile::tempdir;
    use zinder_core::wire::{
        encode_internal_transaction_id, encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex,
    };
    use zinder_core::{
        BlockHash, BlockHeight, ChainEpochId, Network, NetworkUpgradeActivationsFingerprintVersion,
        PrivacyShape, TransactionId,
    };
    use zinder_materialized_views::{
        BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA,
        BlockSummaryConsumer, MaterializedViewStoreOptions, RECENT_TRANSACTIONS_SCHEMA,
        RecentTransactionsConsumer, TRANSACTION_FEES_COLUMN_FAMILY, TRANSACTION_FEES_SCHEMA,
    };
    use zinder_proto::v1::explorer::{
        BlockSummary, BlockSummaryRecord, PrevoutResolutionStatus, TransactionFeesRecord,
    };
    use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};
    use zinder_proto::wire::encode_privacy_shape;
    use zinder_store::{
        CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION, CanonicalEventCursor, RocksDbResourceBudget,
    };

    fn construction_identity(seed: u8) -> Result<CanonicalStoreConstructionIdentity, &'static str> {
        let mut encoded = vec![0u8; 1 + 4 + 2 + 32 + 2 + 32];
        encoded[0] = 1;
        encoded[1..5].copy_from_slice(&Network::ZcashRegtest.id().to_be_bytes());
        encoded[5..7].copy_from_slice(
            &NetworkUpgradeActivationsFingerprintVersion::CURRENT
                .value()
                .to_be_bytes(),
        );
        encoded[7..39].fill(seed);
        encoded[39..41]
            .copy_from_slice(&CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION.to_be_bytes());
        CanonicalStoreConstructionIdentity::decode_persisted(&encoded)
            .map_err(|_| "test construction identity must decode")
    }

    fn materialized_view_state(revision: u64) -> MaterializedViewState {
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(47),
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([0xa5; 32]),
            revision,
            coverage: None,
        }
    }

    #[test]
    fn recent_cursor_round_trips_only_at_its_exact_read_fence() -> Result<(), &'static str> {
        let identity = construction_identity(1)?;
        let state = materialized_view_state(7);
        let row_key = [9; ROW_KEY_LEN];
        let encoded = encode_recent_cursor(identity, state, row_key);

        let decoded = decode_recent_cursor(&encoded, identity, state)
            .map_err(|_| "cursor must decode at its exact read fence")?;
        assert_eq!(decoded, Some(row_key));
        Ok(())
    }

    #[test]
    fn recent_cursor_rejects_another_lineage_or_changed_read_fence() -> Result<(), &'static str> {
        let identity = construction_identity(1)?;
        let state = materialized_view_state(7);
        let encoded = encode_recent_cursor(identity, state, [9; ROW_KEY_LEN]);

        let other_identity_error = decode_recent_cursor(&encoded, construction_identity(2)?, state)
            .err()
            .ok_or("different construction identity must fail")?;
        assert_eq!(other_identity_error.code(), tonic::Code::FailedPrecondition);

        let changed_epoch = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(48),
            ..state
        };
        let changed_epoch_error = decode_recent_cursor(&encoded, identity, changed_epoch)
            .err()
            .ok_or("changed epoch must fail")?;
        assert_eq!(changed_epoch_error.code(), tonic::Code::FailedPrecondition);

        let changed_tip = MaterializedViewState {
            tip_hash: BlockHash::from_bytes([0xb6; 32]),
            ..state
        };
        let changed_tip_error = decode_recent_cursor(&encoded, identity, changed_tip)
            .err()
            .ok_or("changed tip hash must fail")?;
        assert_eq!(changed_tip_error.code(), tonic::Code::FailedPrecondition);

        let changed_revision_error = decode_recent_cursor(
            &encoded,
            identity,
            materialized_view_state(state.revision.saturating_add(1)),
        )
        .err()
        .ok_or("changed revision must fail")?;
        assert_eq!(
            changed_revision_error.code(),
            tonic::Code::FailedPrecondition
        );
        Ok(())
    }

    #[test]
    fn recent_without_the_fee_field_never_requires_the_fee_consumer() {
        assert_eq!(
            recent_transaction_checkpoint_consumers(false),
            &[RECENT_TRANSACTIONS_CONSUMER_NAME],
        );
    }

    #[test]
    #[allow(
        clippy::significant_drop_tightening,
        clippy::too_many_lines,
        reason = "the E1 snapshot intentionally spans the E2 replacement and every coupled row, fee, cursor, state, and checkpoint assertion"
    )]
    fn recent_transactions_snapshot_retains_e1_rows_fees_cursor_and_freshness_after_e2_write()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let activations = zinder_testkit::sample_regtest_upgrade_activations();
        let chain = zinder_testkit::ChainFixture::new(activations.network()).extend_blocks(2);
        let mut canonical_fixture =
            zinder_testkit::WalletServingStoreFixture::from_chain_after_live_append(
                &chain,
                &activations,
            )?;
        let identity = canonical_fixture.canonical_construction_identity()?;
        let (canonical_reader, _) = canonical_fixture.take_readers()?;
        let e1_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(1)?)?,
            );
        let e2_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(2)?)?,
            );
        let store = MaterializedViewStore::open(
            directory.path(),
            identity,
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: &[
                    RECENT_TRANSACTIONS_SCHEMA,
                    TRANSACTION_FEES_SCHEMA,
                    BLOCK_SUMMARY_SCHEMA,
                ],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        let e1_state = materialized_view_state(7);
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        let e1_row_key = RecentTransactionsConsumer::key_for_row(BlockHeight::new(100), 0);
        let e1_entry = RecentTransactionEntry {
            transaction_id: encode_rpc_transaction_id_hex(transaction_id),
            block_height: 100,
            block_hash: encode_rpc_block_hash_hex(e1_state.tip_hash),
            block_time_unix_seconds: 1_000,
            is_coinbase: false,
            privacy_shape: encode_privacy_shape(PrivacyShape::TransparentOnly) as i32,
            component_counts: None,
            size_bytes: 100,
            zip317_conventional_fee_zat: Some(10_000),
            paid_fee_zat: None,
            logical_actions: 2,
        };
        let e1_transaction_id = e1_entry.transaction_id.clone();
        let e1_fee = TransactionFeesRecord {
            paid_fee_zat: Some(50),
            prevout_resolution_status: PrevoutResolutionStatus::Resolved as i32,
            transparent_inputs: Vec::new(),
            logical_actions: 2,
        };
        let e1_block_summary = BlockSummaryRecord {
            summary: Some(BlockSummary {
                block_height: 100,
                block_hash: encode_rpc_block_hash_hex(e1_state.tip_hash),
                block_time_unix_seconds: 1_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        let e1_status = MaterializedViewStatus {
            health: MaterializedViewHealth::Live as i32,
            indexed_height: 100,
            lag_blocks: 0,
            observed_at_millis: 1_000,
        };
        store.put_consumer(
            RECENT_TRANSACTIONS_COLUMN_FAMILY,
            &e1_row_key,
            &e1_entry.encode_to_vec(),
        )?;
        store.put_consumer(
            TRANSACTION_FEES_COLUMN_FAMILY,
            &encode_internal_transaction_id(transaction_id),
            &e1_fee.encode_to_vec(),
        )?;
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &BlockSummaryConsumer::key_for_height(e1_state.tip_height),
            &e1_block_summary.encode_to_vec(),
        )?;
        for consumer in [
            BLOCK_SUMMARY_CONSUMER_NAME,
            RECENT_TRANSACTIONS_CONSUMER_NAME,
            TRANSACTION_FEES_CONSUMER_NAME,
        ] {
            store.put_consumer_state(consumer, e1_state)?;
            store.put_chain_event_checkpoint(consumer, e1_checkpoint)?;
        }
        store.put_materialized_view_status(&e1_status.encode_to_vec())?;

        let e1_snapshot = store.read_snapshot()?;

        let e2_fee = TransactionFeesRecord {
            paid_fee_zat: Some(75),
            ..e1_fee
        };
        let e2_state = materialized_view_state(8);
        let e2_entry = RecentTransactionEntry {
            transaction_id: encode_rpc_transaction_id_hex(TransactionId::from_bytes([0x22; 32])),
            block_height: 101,
            block_hash: "e2".to_owned(),
            ..e1_entry
        };
        store.put_consumer(
            TRANSACTION_FEES_COLUMN_FAMILY,
            &encode_internal_transaction_id(transaction_id),
            &e2_fee.encode_to_vec(),
        )?;
        store.put_consumer(
            RECENT_TRANSACTIONS_COLUMN_FAMILY,
            &RecentTransactionsConsumer::key_for_row(BlockHeight::new(101), 0),
            &e2_entry.encode_to_vec(),
        )?;
        for consumer in [
            BLOCK_SUMMARY_CONSUMER_NAME,
            RECENT_TRANSACTIONS_CONSUMER_NAME,
            TRANSACTION_FEES_CONSUMER_NAME,
        ] {
            store.put_consumer_state(consumer, e2_state)?;
            store.put_chain_event_checkpoint(consumer, e2_checkpoint)?;
        }
        store.put_materialized_view_status(
            &MaterializedViewStatus {
                health: MaterializedViewHealth::Live as i32,
                indexed_height: 101,
                lag_blocks: 0,
                observed_at_millis: 1_001,
            }
            .encode_to_vec(),
        )?;

        let (mut entries, last_key) = read_recent_transaction_entries(&e1_snapshot, None, 1)?;
        join_paid_fees_snapshot(&e1_snapshot, &mut entries)?;
        let last_key = last_key.ok_or_else(|| eyre::eyre!("E1 page must retain its row key"))?;
        let cursor = encode_recent_cursor(identity, e1_state, last_key);
        let freshness = build_explorer_freshness_from_snapshot(
            &e1_snapshot,
            EXPLORER_TRANSACTION_RECENT_V1,
            None,
            0,
        )?;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].transaction_id, e1_transaction_id);
        assert_eq!(entries[0].paid_fee_zat, Some(50));
        assert_eq!(
            decode_recent_cursor(&cursor, identity, e1_state)?,
            Some(e1_row_key)
        );
        for consumer in [
            BLOCK_SUMMARY_CONSUMER_NAME,
            RECENT_TRANSACTIONS_CONSUMER_NAME,
            TRANSACTION_FEES_CONSUMER_NAME,
        ] {
            assert_eq!(e1_snapshot.consumer_state(consumer)?, Some(e1_state));
            assert_eq!(
                e1_snapshot.chain_event_checkpoint(consumer)?,
                Some(e1_checkpoint)
            );
        }
        assert_eq!(
            freshness
                .chain_view
                .and_then(|chain_view| chain_view.materialized_views)
                .map(|status| status.observed_at_millis),
            Some(e1_status.observed_at_millis)
        );
        Ok(())
    }
}
