//! `TransparentAddressTransactionHistory` materialized-view consumer.
//!
//! Materializes transparent-address transaction history from typed canonical
//! facts. Canonical ingest writes transparent outputs and spend facts once; this
//! consumer owns the address-to-transaction materialized view that serves wallet
//! `GetTaddressTxids`-style reads.

use std::{collections::HashMap, num::NonZeroU32};

use zinder_core::wire::{
    decode_height_key_ascending, decode_height_key_descending, decode_in_block_position,
    encode_address_script_hash, encode_height_key_ascending, encode_height_key_descending,
    encode_in_block_position,
};
use zinder_core::{
    BlockHash, BlockHeight, TransactionFactsArtifact, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentOutPoint,
};
use zinder_store::StreamCursorTokenV1;

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, MaterializedViewConsumerCtx,
    MaterializedViewConsumerError, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
};
use crate::error::{MaterializedViewStoreColumnFamily, MaterializedViewStoreError};
use crate::store::{ConsumerEntry, MaterializedViewStore, MaterializedViewStoreReadSnapshot};

/// Ascending address transaction-history rows keyed by address, height, tx index.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY: &str =
    "transparent_address_transaction_history";

/// Descending address transaction-history rows keyed by address, reverse height, tx index.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY: &str =
    "transparent_address_transaction_history_descending";

/// Per-height index used to delete both primary keys during reorg rewind.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY: &str =
    "transparent_address_transaction_history_index";

/// Column families the consumer needs registered before its first write.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILIES: &[&str] = &[
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
];

/// Stable consumer name persisted in the materialized-view cursor table.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("transparent_address_transaction_history");

/// On-disk schema declaration for the transparent-address-history consumer.
pub const TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        1,
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILIES,
    );

const ADDRESS_HASH_LEN: usize = 32;
const HEIGHT_LEN: usize = 4;
const POSITION_LEN: usize = 4;
const HISTORY_KEY_LEN: usize = ADDRESS_HASH_LEN + HEIGHT_LEN + POSITION_LEN;
const HISTORY_VALUE_LEN: usize = 64;
const INDEX_ENTRY_LEN: usize = HISTORY_KEY_LEN * 2;
const CURSOR_PREFIX: &[u8; 4] = b"zth2";
const CURSOR_DIRECTION_OFFSET: usize = CURSOR_PREFIX.len();
const CURSOR_FENCE_LENGTH_OFFSET: usize = CURSOR_DIRECTION_OFFSET + 1;
const CURSOR_FENCE_OFFSET: usize = CURSOR_FENCE_LENGTH_OFFSET + 2;
const CURSOR_MIN_LEN: usize = CURSOR_FENCE_OFFSET + HISTORY_KEY_LEN;
const TRANSACTION_ID_RANGE: std::ops::Range<usize> = 0..32;
const BLOCK_HASH_RANGE: std::ops::Range<usize> = 32..64;

/// Read parameters for a transparent-address transaction-history page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentAddressTransactionHistoryPageRequest<'cursor> {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Inclusive minimum block height.
    pub start_height: BlockHeight,
    /// Inclusive maximum block height.
    pub end_height: BlockHeight,
    /// Server-bounded maximum entries per page.
    pub max_entries: NonZeroU32,
    /// Optional cursor returned by a previous page.
    pub from_cursor: Option<&'cursor StreamCursorTokenV1>,
    /// Exact visible-chain event fence the rows and pagination cursor must
    /// represent. The caller obtains this from the canonical store and must
    /// compare it with this consumer's persisted cursor in the same
    /// materialized-view snapshot before reading rows.
    pub chain_event_fence: Option<&'cursor StreamCursorTokenV1>,
    /// Iterate newest-first when true.
    pub descending: bool,
}

/// Transparent-address transaction-history page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTransactionHistoryPage {
    /// Tx-history artifacts in requested order.
    pub artifacts: Vec<TransparentAddressTxIndexArtifact>,
    /// Resume cursor when more entries may be available.
    pub next_cursor: Option<StreamCursorTokenV1>,
}

#[derive(Clone, Copy)]
enum TransparentAddressTransactionHistoryRead<'store> {
    Store(&'store MaterializedViewStore),
    Snapshot(&'store MaterializedViewStoreReadSnapshot<'store>),
}

impl TransparentAddressTransactionHistoryRead<'_> {
    fn range_iterate_consumer(
        self,
        column_family: &'static str,
        start_key: &[u8],
        end_key_inclusive: &[u8],
        entries_cap: usize,
    ) -> Result<Vec<ConsumerEntry>, MaterializedViewStoreError> {
        match self {
            Self::Store(store) => store.range_iterate_consumer(
                column_family,
                start_key,
                end_key_inclusive,
                entries_cap,
            ),
            Self::Snapshot(snapshot) => snapshot.range_iterate_consumer(
                column_family,
                start_key,
                end_key_inclusive,
                entries_cap,
            ),
        }
    }
}

/// Materializes confirmed transparent-address transaction-history rows.
#[derive(Default)]
pub struct TransparentAddressTransactionHistoryConsumer;

impl TransparentAddressTransactionHistoryConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns an upper bound on rows this consumer can write for
    /// `transactions`.
    ///
    /// Spend-side rows need canonical spend facts to resolve the spent
    /// address. Replay uses this bound before those facts are read, so every
    /// non-coinbase transparent input counts as one possible history row.
    #[must_use]
    pub fn projected_row_count_upper_bound_for_transactions(
        transactions: &[TransactionFactsArtifact],
    ) -> usize {
        transactions
            .iter()
            .fold(0usize, |projected_rows, transaction| {
                let receive_rows = transaction.transparent_outputs.len();
                let spend_rows = transaction
                    .transparent_inputs
                    .iter()
                    .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
                    .count();
                projected_rows.saturating_add(receive_rows.saturating_add(spend_rows))
            })
    }

    /// Returns an upper bound on rows this consumer can write for `block`.
    #[must_use]
    pub fn projected_row_count_upper_bound_for_block(block: &BlockCommitContext) -> usize {
        Self::projected_row_count_upper_bound_for_transactions(&block.transactions)
    }

    /// Reads a transparent-address transaction-history page from a materialized-view store.
    pub fn read_page(
        store: &MaterializedViewStore,
        request: TransparentAddressTransactionHistoryPageRequest<'_>,
    ) -> Result<TransparentAddressTransactionHistoryPage, MaterializedViewStoreError> {
        Self::read_page_from(
            TransparentAddressTransactionHistoryRead::Store(store),
            request,
        )
    }

    /// Reads a transparent-address transaction-history page from one snapshot.
    pub fn read_page_snapshot(
        snapshot: &MaterializedViewStoreReadSnapshot<'_>,
        request: TransparentAddressTransactionHistoryPageRequest<'_>,
    ) -> Result<TransparentAddressTransactionHistoryPage, MaterializedViewStoreError> {
        Self::read_page_from(
            TransparentAddressTransactionHistoryRead::Snapshot(snapshot),
            request,
        )
    }

    fn read_page_from(
        store: TransparentAddressTransactionHistoryRead<'_>,
        request: TransparentAddressTransactionHistoryPageRequest<'_>,
    ) -> Result<TransparentAddressTransactionHistoryPage, MaterializedViewStoreError> {
        let cursor = request
            .from_cursor
            .map(history_cursor_from_token)
            .transpose()?;
        let expected_fence = request.chain_event_fence.map(StreamCursorTokenV1::as_bytes);
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.chain_event_fence.as_deref() != expected_fence)
        {
            return Err(cursor_error(
                "cursor is bound to a different visible chain fence",
            ));
        }
        let request = TransparentAddressTransactionHistoryPageRequest {
            descending: cursor
                .as_ref()
                .map_or(request.descending, |cursor| cursor.descending),
            ..request
        };
        let column_family = if request.descending {
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY
        } else {
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY
        };
        let start_key = start_key_for_request(&request)?;
        let end_key = end_key_for_request(&request);
        let raw_entries = store.range_iterate_consumer(
            column_family,
            &start_key,
            &end_key,
            nonzero_u32_to_usize(request.max_entries).saturating_add(1),
        )?;
        let resume_key = cursor.map(|cursor| cursor.key);
        let mut artifacts = Vec::with_capacity(nonzero_u32_to_usize(request.max_entries).min(64));
        let mut last_key = None;
        for (key, history_payload) in raw_entries {
            if resume_key.is_some_and(|cursor| cursor.as_slice() == key.as_slice()) {
                continue;
            }
            if artifacts.len() >= nonzero_u32_to_usize(request.max_entries) {
                break;
            }
            let artifact = decode_history_entry(
                request.address_script_hash,
                request.descending,
                key.as_slice(),
                history_payload.as_slice(),
            )?;
            last_key = Some(key);
            artifacts.push(artifact);
        }
        let next_cursor = last_key.map(|key| {
            history_cursor_token(
                request.descending,
                request.chain_event_fence.map(StreamCursorTokenV1::as_bytes),
                key.as_slice(),
            )
        });
        Ok(TransparentAddressTransactionHistoryPage {
            artifacts,
            next_cursor,
        })
    }
}

impl BlockKeyedConsumer for TransparentAddressTransactionHistoryConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let transparent_spends = block.transparent_spends();
        let rows = collect_address_transaction_rows(block, transparent_spends.as_deref());
        let ascending_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        let descending_cf = ctx.store.consumer_column_family(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
        )?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY)?;

        let mut index_payload = Vec::with_capacity(rows.len() * INDEX_ENTRY_LEN);
        for artifact in rows {
            let ascending_key = history_key(
                artifact.address_script_hash,
                artifact.block_height,
                artifact.tx_index_in_block,
                false,
            );
            let descending_key = history_key(
                artifact.address_script_hash,
                artifact.block_height,
                artifact.tx_index_in_block,
                true,
            );
            let history_payload = encode_history_value(&artifact);
            ctx.batch
                .put_cf(&ascending_cf, ascending_key, history_payload.as_slice());
            ctx.batch
                .put_cf(&descending_cf, descending_key, history_payload.as_slice());
            index_payload.extend_from_slice(&ascending_key);
            index_payload.extend_from_slice(&descending_key);
        }

        ctx.batch.put_cf(
            &index_cf,
            encode_height_key_ascending(block.height),
            index_payload,
        );
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let index_key = encode_height_key_ascending(height);
        let Some(index_payload) = ctx.store.get_consumer(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            &index_key,
        )?
        else {
            return Ok(());
        };
        if index_payload.len() % INDEX_ENTRY_LEN != 0 {
            return Err(Box::new(
                TransparentAddressTransactionHistoryConsumerError::IndexLengthMismatch {
                    height: height.value(),
                    bytes: index_payload.len(),
                },
            ));
        }
        let ascending_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        let descending_cf = ctx.store.consumer_column_family(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
        )?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY)?;
        for chunk in index_payload.chunks_exact(INDEX_ENTRY_LEN) {
            ctx.batch
                .delete_cf(&ascending_cf, &chunk[..HISTORY_KEY_LEN]);
            ctx.batch
                .delete_cf(&descending_cf, &chunk[HISTORY_KEY_LEN..]);
        }
        ctx.batch.delete_cf(&index_cf, index_key);
        Ok(())
    }
}

fn collect_address_transaction_rows(
    block: &BlockCommitContext,
    transparent_spends: Option<&HashMap<TransparentOutPoint, zinder_core::TransparentSpendFact>>,
) -> Vec<TransparentAddressTxIndexArtifact> {
    let mut rows = Vec::new();
    let mut emitted = HashMap::<(TransparentAddressScriptHash, u32), usize>::new();
    for transaction in &block.transactions {
        let tx_index = transaction.location.tx_index_in_block;
        for output in &transaction.transparent_outputs {
            push_row(
                &mut rows,
                &mut emitted,
                TransparentAddressTxIndexArtifact::new(
                    output.address_script_hash,
                    transaction.location.block_height,
                    tx_index,
                    transaction.location.transaction_id,
                    transaction.location.block_hash,
                ),
            );
        }
        if let Some(spends_by_outpoint) = transparent_spends {
            for input in &transaction.transparent_inputs {
                let Some(spend) = spends_by_outpoint.get(&input.spent_outpoint) else {
                    continue;
                };
                push_row(
                    &mut rows,
                    &mut emitted,
                    TransparentAddressTxIndexArtifact::new(
                        spend.spent_address_script_hash,
                        transaction.location.block_height,
                        tx_index,
                        transaction.location.transaction_id,
                        transaction.location.block_hash,
                    ),
                );
            }
        }
    }
    rows
}

fn push_row(
    rows: &mut Vec<TransparentAddressTxIndexArtifact>,
    emitted: &mut HashMap<(TransparentAddressScriptHash, u32), usize>,
    artifact: TransparentAddressTxIndexArtifact,
) {
    let key = (artifact.address_script_hash, artifact.tx_index_in_block);
    if emitted.contains_key(&key) {
        return;
    }
    emitted.insert(key, rows.len());
    rows.push(artifact);
}

fn start_key_for_request(
    request: &TransparentAddressTransactionHistoryPageRequest<'_>,
) -> Result<[u8; HISTORY_KEY_LEN], MaterializedViewStoreError> {
    if let Some(cursor) = request.from_cursor {
        let key = history_cursor_from_token(cursor)?.key;
        validate_cursor_matches_request(request, &key)?;
        return Ok(key);
    }
    Ok(history_key(
        request.address_script_hash,
        if request.descending {
            request.end_height
        } else {
            request.start_height
        },
        if request.descending { u32::MAX } else { 0 },
        request.descending,
    ))
}

fn end_key_for_request(
    request: &TransparentAddressTransactionHistoryPageRequest<'_>,
) -> [u8; HISTORY_KEY_LEN] {
    history_key(
        request.address_script_hash,
        if request.descending {
            request.start_height
        } else {
            request.end_height
        },
        if request.descending { 0 } else { u32::MAX },
        request.descending,
    )
}

fn validate_cursor_matches_request(
    request: &TransparentAddressTransactionHistoryPageRequest<'_>,
    key: &[u8; HISTORY_KEY_LEN],
) -> Result<(), MaterializedViewStoreError> {
    let address = decode_address_from_key(key)?;
    if address != request.address_script_hash {
        return Err(cursor_error(
            "cursor address does not match request address",
        ));
    }
    let height = decode_height_from_key(key, request.descending)?;
    if height < request.start_height || height > request.end_height {
        return Err(cursor_error("cursor height is outside request range"));
    }
    Ok(())
}

fn decode_history_entry(
    address_script_hash: TransparentAddressScriptHash,
    descending: bool,
    key: &[u8],
    history_payload: &[u8],
) -> Result<TransparentAddressTxIndexArtifact, MaterializedViewStoreError> {
    let key = history_key_from_bytes(key)?;
    let key_address = decode_address_from_key(&key)?;
    if key_address != address_script_hash {
        return Err(decode_error(
            "history key address does not match request address",
        ));
    }
    if history_payload.len() != HISTORY_VALUE_LEN {
        return Err(decode_error("history value length is invalid"));
    }
    let transaction_id_bytes: [u8; 32] = history_payload[TRANSACTION_ID_RANGE]
        .try_into()
        .map_err(|_| decode_error("transaction id length is invalid"))?;
    let block_hash_bytes: [u8; 32] = history_payload[BLOCK_HASH_RANGE]
        .try_into()
        .map_err(|_| decode_error("block hash length is invalid"))?;
    Ok(TransparentAddressTxIndexArtifact::new(
        key_address,
        decode_height_from_key(&key, descending)?,
        decode_position_from_key(&key, descending)?,
        zinder_core::TransactionId::from_bytes(transaction_id_bytes),
        BlockHash::from_bytes(block_hash_bytes),
    ))
}

fn history_key(
    address_script_hash: TransparentAddressScriptHash,
    height: BlockHeight,
    tx_index_in_block: u32,
    descending: bool,
) -> [u8; HISTORY_KEY_LEN] {
    let mut key = [0u8; HISTORY_KEY_LEN];
    key[..ADDRESS_HASH_LEN].copy_from_slice(&encode_address_script_hash(address_script_hash));
    let height_key = if descending {
        encode_height_key_descending(height)
    } else {
        encode_height_key_ascending(height)
    };
    key[ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + HEIGHT_LEN].copy_from_slice(&height_key);
    let position_key = if descending {
        encode_in_block_position(u32::MAX - tx_index_in_block)
    } else {
        encode_in_block_position(tx_index_in_block)
    };
    key[ADDRESS_HASH_LEN + HEIGHT_LEN..].copy_from_slice(&position_key);
    key
}

fn encode_history_value(artifact: &TransparentAddressTxIndexArtifact) -> [u8; HISTORY_VALUE_LEN] {
    let mut encoded = [0u8; HISTORY_VALUE_LEN];
    encoded[TRANSACTION_ID_RANGE].copy_from_slice(&artifact.transaction_id.as_bytes());
    encoded[BLOCK_HASH_RANGE].copy_from_slice(&artifact.block_hash.as_bytes());
    encoded
}

fn history_key_from_bytes(
    bytes: &[u8],
) -> Result<[u8; HISTORY_KEY_LEN], MaterializedViewStoreError> {
    bytes
        .try_into()
        .map_err(|_| decode_error("history cursor length is invalid"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistoryCursor {
    descending: bool,
    chain_event_fence: Option<Vec<u8>>,
    key: [u8; HISTORY_KEY_LEN],
}

fn history_cursor_token(
    descending: bool,
    chain_event_fence: Option<&[u8]>,
    key: &[u8],
) -> StreamCursorTokenV1 {
    let chain_event_fence = chain_event_fence.unwrap_or_default();
    let fence_len = u16::try_from(chain_event_fence.len()).unwrap_or(u16::MAX);
    let mut bytes = Vec::with_capacity(CURSOR_MIN_LEN.saturating_add(chain_event_fence.len()));
    bytes.extend_from_slice(CURSOR_PREFIX);
    bytes.push(u8::from(descending));
    bytes.extend_from_slice(&fence_len.to_be_bytes());
    bytes.extend_from_slice(chain_event_fence);
    bytes.extend_from_slice(key);
    StreamCursorTokenV1::from_bytes(bytes)
}

fn history_cursor_from_token(
    cursor: &StreamCursorTokenV1,
) -> Result<HistoryCursor, MaterializedViewStoreError> {
    let bytes = cursor.as_bytes();
    if bytes.len() < CURSOR_MIN_LEN {
        return Err(cursor_error("history cursor length is invalid"));
    }
    if bytes.get(..CURSOR_PREFIX.len()) != Some(CURSOR_PREFIX) {
        return Err(cursor_error("history cursor prefix is invalid"));
    }
    let descending = match bytes[CURSOR_DIRECTION_OFFSET] {
        0 => false,
        1 => true,
        _ => return Err(cursor_error("history cursor direction is invalid")),
    };
    let fence_len = usize::from(u16::from_be_bytes(
        bytes[CURSOR_FENCE_LENGTH_OFFSET..CURSOR_FENCE_OFFSET]
            .try_into()
            .map_err(|_| cursor_error("history cursor fence length is invalid"))?,
    ));
    let fence_end = CURSOR_FENCE_OFFSET
        .checked_add(fence_len)
        .ok_or_else(|| cursor_error("history cursor fence length overflows"))?;
    let key_end = fence_end
        .checked_add(HISTORY_KEY_LEN)
        .ok_or_else(|| cursor_error("history cursor key length overflows"))?;
    if bytes.len() != key_end {
        return Err(cursor_error("history cursor length is invalid"));
    }
    Ok(HistoryCursor {
        descending,
        chain_event_fence: (!bytes[CURSOR_FENCE_OFFSET..fence_end].is_empty())
            .then(|| bytes[CURSOR_FENCE_OFFSET..fence_end].to_vec()),
        key: bytes[fence_end..]
            .try_into()
            .map_err(|_| cursor_error("history cursor length is invalid"))?,
    })
}

fn decode_address_from_key(
    key: &[u8; HISTORY_KEY_LEN],
) -> Result<TransparentAddressScriptHash, MaterializedViewStoreError> {
    let bytes: [u8; ADDRESS_HASH_LEN] = key[..ADDRESS_HASH_LEN]
        .try_into()
        .map_err(|_| decode_error("address hash length is invalid"))?;
    Ok(TransparentAddressScriptHash::from_bytes(bytes))
}

fn decode_height_from_key(
    key: &[u8; HISTORY_KEY_LEN],
    descending: bool,
) -> Result<BlockHeight, MaterializedViewStoreError> {
    let bytes = &key[ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + HEIGHT_LEN];
    if descending {
        decode_height_key_descending(bytes)
    } else {
        decode_height_key_ascending(bytes)
    }
    .map_err(|error| decode_error(error.to_string()))
}

fn decode_position_from_key(
    key: &[u8; HISTORY_KEY_LEN],
    descending: bool,
) -> Result<u32, MaterializedViewStoreError> {
    let encoded = decode_in_block_position(&key[ADDRESS_HASH_LEN + HEIGHT_LEN..])
        .map_err(|error| decode_error(error.to_string()))?;
    Ok(if descending {
        u32::MAX - encoded
    } else {
        encoded
    })
}

fn decode_error(reason: impl Into<String>) -> MaterializedViewStoreError {
    MaterializedViewStoreError::Decode {
        column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
        reason: reason.into(),
    }
}

fn cursor_error(reason: &'static str) -> MaterializedViewStoreError {
    MaterializedViewStoreError::ConsumerCursorInvalid {
        consumer: TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME.as_str(),
        reason,
    }
}

fn nonzero_u32_to_usize(amount: NonZeroU32) -> usize {
    usize::try_from(amount.get()).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "tests assert on a known-present row; absence is a test-code bug, not a runtime condition."
    )]

    use std::collections::HashMap;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use rust_rocksdb::WriteBatch;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeight, LockTime, PrivacyShape, TransactionComponentCounts,
        TransactionFactsArtifact, TransactionId, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, TransparentAddressScriptHash, TransparentInputFact,
        TransparentOutPoint, TransparentOutputFact, TransparentSpendFact,
    };
    use zinder_store::{RocksDbResourceBudget, StreamCursorTokenV1};

    use super::{
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
        TransparentAddressTransactionHistoryConsumer,
        TransparentAddressTransactionHistoryPageRequest,
    };
    use crate::consumer::block_commit_context::{
        BlockCommitContext, BlockCommitInput, TransparentSpendFacts,
    };
    use crate::consumer::{BlockKeyedConsumer, MaterializedViewConsumerCtx};
    use crate::{
        MaterializedViewStoreError,
        store::{MaterializedViewStore, MaterializedViewStoreOptions},
    };

    const WATCHED_ADDRESS: TransparentAddressScriptHash =
        TransparentAddressScriptHash::from_bytes([7; 32]);

    fn transaction_id(seed: u8) -> TransactionId {
        TransactionId::from_bytes([seed; 32])
    }

    fn block_hash(seed: u8) -> BlockHash {
        BlockHash::from_bytes([seed; 32])
    }

    fn public_facts(seed: u8) -> TransactionPublicFacts {
        TransactionPublicFacts {
            transaction_id: transaction_id(seed),
            auth_digest: None,
            wtxid: None,
            version: TransactionVersion::V5,
            consensus_branch_id: None,
            lock_time: LockTime::Unlocked,
            expiry_height: None,
            size_bytes: 0,
            counts: TransactionComponentCounts::EMPTY,
            orchard_value_balance_zat: None,
            orchard_anchor: None,
            ironwood_value_balance_zat: None,
            privacy_shape: PrivacyShape::Unclassified,
            is_coinbase: false,
            unsupported_sections: Vec::new(),
        }
    }

    fn open_store()
    -> Result<(tempfile::TempDir, MaterializedViewStore), Box<dyn std::error::Error + Send + Sync>>
    {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            MaterializedViewStoreOptions {
                consumers: &[TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        Ok((tempdir, store))
    }

    fn apply_block(
        store: &MaterializedViewStore,
        consumer: &mut TransparentAddressTransactionHistoryConsumer,
        block: &BlockCommitContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        consumer.apply_block(block, &mut ctx)?;
        store.write_batch(&batch)?;
        Ok(())
    }

    const RECEIVE_HEIGHT: BlockHeight = BlockHeight::new(100);
    const SPEND_HEIGHT: BlockHeight = BlockHeight::new(105);

    fn received_outpoint() -> TransparentOutPoint {
        TransparentOutPoint::new(transaction_id(10), 0)
    }

    fn receive_block() -> BlockCommitContext {
        let location =
            TransactionLocation::new(transaction_id(10), RECEIVE_HEIGHT, block_hash(1), 0);
        let transaction = TransactionFactsArtifact::new(location, public_facts(10))
            .with_transparent_facts(
                Vec::new(),
                vec![TransparentOutputFact::new(
                    0,
                    5_000,
                    vec![1, 2, 3],
                    WATCHED_ADDRESS,
                )],
            );
        BlockCommitContext::new(
            BlockCommitInput {
                height: RECEIVE_HEIGHT,
                block_hash: block_hash(1),
                previous_block_hash: block_hash(0),
                block_time_unix_seconds: 1_700_000_000,
                block_size_bytes: 0,
                transactions: vec![transaction],
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        )
    }

    fn spend_block() -> BlockCommitContext {
        let location = TransactionLocation::new(transaction_id(20), SPEND_HEIGHT, block_hash(5), 0);
        let transaction = TransactionFactsArtifact::new(location, public_facts(20))
            .with_transparent_facts(
                vec![TransparentInputFact::new(0, received_outpoint())],
                Vec::new(),
            );
        let mut spends = HashMap::new();
        spends.insert(
            received_outpoint(),
            TransparentSpendFact::new(
                received_outpoint(),
                0,
                transaction_id(20),
                0,
                SPEND_HEIGHT,
                block_hash(5),
                5_000,
                WATCHED_ADDRESS,
                RECEIVE_HEIGHT,
                block_hash(1),
            ),
        );
        BlockCommitContext::new(
            BlockCommitInput {
                height: SPEND_HEIGHT,
                block_hash: block_hash(5),
                previous_block_hash: block_hash(4),
                block_time_unix_seconds: 1_700_000_500,
                block_size_bytes: 0,
                transactions: vec![transaction],
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Static(Arc::new(spends)),
        )
    }

    #[test]
    fn spend_from_watched_address_emits_a_spend_row()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentAddressTransactionHistoryConsumer::new();

        apply_block(&store, &mut consumer, &receive_block())?;
        apply_block(&store, &mut consumer, &spend_block())?;

        let page = TransparentAddressTransactionHistoryConsumer::read_page(
            &store,
            TransparentAddressTransactionHistoryPageRequest {
                address_script_hash: WATCHED_ADDRESS,
                start_height: BlockHeight::new(1),
                end_height: BlockHeight::new(200),
                max_entries: NonZeroU32::new(10).expect("ten is non-zero"),
                from_cursor: None,
                chain_event_fence: None,
                descending: false,
            },
        )?;

        assert_eq!(page.artifacts.len(), 2);
        let spend_row = page
            .artifacts
            .iter()
            .find(|artifact| artifact.block_height == SPEND_HEIGHT)
            .expect("spend row must be present for the watched address");
        assert_eq!(spend_row.address_script_hash, WATCHED_ADDRESS);
        assert_eq!(spend_row.transaction_id, transaction_id(20));
        assert_eq!(spend_row.block_hash, block_hash(5));
        assert_eq!(spend_row.tx_index_in_block, 0);
        Ok(())
    }

    #[test]
    fn descending_page_keeps_both_range_boundaries()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentAddressTransactionHistoryConsumer::new();

        apply_block(&store, &mut consumer, &receive_block())?;
        apply_block(&store, &mut consumer, &spend_block())?;

        let read = |descending| {
            TransparentAddressTransactionHistoryConsumer::read_page(
                &store,
                TransparentAddressTransactionHistoryPageRequest {
                    address_script_hash: WATCHED_ADDRESS,
                    start_height: RECEIVE_HEIGHT,
                    end_height: SPEND_HEIGHT,
                    max_entries: NonZeroU32::new(10).expect("ten is non-zero"),
                    from_cursor: None,
                    chain_event_fence: None,
                    descending,
                },
            )
        };

        let ascending = read(false)?;
        let descending = read(true)?;

        assert_eq!(ascending.artifacts.len(), 2);
        assert_eq!(descending.artifacts.len(), 2);
        for height in [RECEIVE_HEIGHT, SPEND_HEIGHT] {
            assert!(
                descending
                    .artifacts
                    .iter()
                    .any(|row| row.block_height == height),
                "descending page must include the boundary row at height {height:?}"
            );
        }
        Ok(())
    }

    const MULTI_RECEIVE_HEIGHT: BlockHeight = BlockHeight::new(110);

    fn multi_receive_block() -> BlockCommitContext {
        let first = TransactionFactsArtifact::new(
            TransactionLocation::new(transaction_id(30), MULTI_RECEIVE_HEIGHT, block_hash(7), 0),
            public_facts(30),
        )
        .with_transparent_facts(
            Vec::new(),
            vec![TransparentOutputFact::new(
                0,
                5_000,
                vec![1, 2, 3],
                WATCHED_ADDRESS,
            )],
        );
        let second = TransactionFactsArtifact::new(
            TransactionLocation::new(transaction_id(31), MULTI_RECEIVE_HEIGHT, block_hash(7), 1),
            public_facts(31),
        )
        .with_transparent_facts(
            Vec::new(),
            vec![TransparentOutputFact::new(
                0,
                6_000,
                vec![4, 5, 6],
                WATCHED_ADDRESS,
            )],
        );
        BlockCommitContext::new(
            BlockCommitInput {
                height: MULTI_RECEIVE_HEIGHT,
                block_hash: block_hash(7),
                previous_block_hash: block_hash(6),
                block_time_unix_seconds: 1_700_001_000,
                block_size_bytes: 0,
                transactions: vec![first, second],
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        )
    }

    #[test]
    fn descending_page_keeps_every_transaction_within_a_block()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentAddressTransactionHistoryConsumer::new();

        apply_block(&store, &mut consumer, &multi_receive_block())?;

        let page = TransparentAddressTransactionHistoryConsumer::read_page(
            &store,
            TransparentAddressTransactionHistoryPageRequest {
                address_script_hash: WATCHED_ADDRESS,
                start_height: MULTI_RECEIVE_HEIGHT,
                end_height: MULTI_RECEIVE_HEIGHT,
                max_entries: NonZeroU32::new(10).expect("ten is non-zero"),
                from_cursor: None,
                chain_event_fence: None,
                descending: true,
            },
        )?;

        assert_eq!(page.artifacts.len(), 2);
        assert_eq!(page.artifacts[0].tx_index_in_block, 1);
        assert_eq!(page.artifacts[1].tx_index_in_block, 0);
        Ok(())
    }

    #[test]
    fn page_cursor_rejects_a_different_visible_chain_fence()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentAddressTransactionHistoryConsumer::new();
        apply_block(&store, &mut consumer, &multi_receive_block())?;
        let first_fence = StreamCursorTokenV1::from_bytes(vec![0xA1]);
        let first_page = TransparentAddressTransactionHistoryConsumer::read_page(
            &store,
            TransparentAddressTransactionHistoryPageRequest {
                address_script_hash: WATCHED_ADDRESS,
                start_height: MULTI_RECEIVE_HEIGHT,
                end_height: MULTI_RECEIVE_HEIGHT,
                max_entries: NonZeroU32::new(1).expect("one is non-zero"),
                from_cursor: None,
                chain_event_fence: Some(&first_fence),
                descending: false,
            },
        )?;
        let cursor = first_page
            .next_cursor
            .ok_or("first page must carry a cursor")?;
        let replacement_fence = StreamCursorTokenV1::from_bytes(vec![0xB2]);
        let error = TransparentAddressTransactionHistoryConsumer::read_page(
            &store,
            TransparentAddressTransactionHistoryPageRequest {
                address_script_hash: WATCHED_ADDRESS,
                start_height: MULTI_RECEIVE_HEIGHT,
                end_height: MULTI_RECEIVE_HEIGHT,
                max_entries: NonZeroU32::new(1).expect("one is non-zero"),
                from_cursor: Some(&cursor),
                chain_event_fence: Some(&replacement_fence),
                descending: false,
            },
        )
        .expect_err("a cursor from a replaced visible chain must fail closed");
        assert!(matches!(
            error,
            MaterializedViewStoreError::ConsumerCursorInvalid {
                reason: "cursor is bound to a different visible chain fence",
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn projected_row_count_upper_bound_counts_transparent_fanout() {
        assert_eq!(
            TransparentAddressTransactionHistoryConsumer::projected_row_count_upper_bound_for_block(
                &multi_receive_block()
            ),
            2
        );
        assert_eq!(
            TransparentAddressTransactionHistoryConsumer::projected_row_count_upper_bound_for_block(
                &spend_block()
            ),
            1
        );
    }
}

/// Consumer-specific failure modes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransparentAddressTransactionHistoryConsumerError {
    /// Per-height delete index payload had a malformed byte length.
    #[error(
        "transparent_address_transaction_history_index entry for height {height} has {bytes} bytes, not a multiple of {INDEX_ENTRY_LEN}"
    )]
    IndexLengthMismatch {
        /// Height whose persisted index was malformed.
        height: u32,
        /// Byte length actually persisted.
        bytes: usize,
    },
}
