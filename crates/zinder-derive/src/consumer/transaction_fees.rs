//! `TransactionFees` derive consumer.
//!
//! Materializes one [`TransactionFeesRecord`] per non-coinbase canonical
//! transaction into the consumer-owned `transaction_fees` column family
//! keyed by the canonical transaction id. The handler-side
//! `TransactionDetail` reads the record at request time to populate
//! `paid_fee_zat`, `prevout_resolution_status`, and the per-input
//! `value_zat` list when the wallet plane's transparent-output
//! resolution capability is online.
//!
//! When prevout resolution is offline the consumer still writes a row
//! per transaction with status `UNAVAILABLE` so the read path stays
//! uniform: the wire surface either has the field populated or carries
//! a typed `UNAVAILABLE` chip; it never silently zeroes the value.
//!
//! ## Rewind correctness
//!
//! The primary records are keyed by txid (32 bytes), so the consumer
//! maintains a per-height txid index in a second column family
//! [`TRANSACTION_FEES_INDEX_COLUMN_FAMILY`] keyed by ascending height.
//! On revert the consumer reads the persisted txid list for the reverted
//! height, deletes each primary record, and deletes the index entry. This
//! is what makes rewind correct under reorg: a reorg-then-rewrite at
//! height H replaces the canonical block, so re-fetching the block at H
//! and using *its* txids to drive deletes would delete the WRONG txids.
//! The persisted index captures what was actually written at apply time.

use std::collections::HashMap;

use prost::Message as _;
use zinder_core::wire::{encode_height_key_ascending, encode_internal_transaction_id};
use zinder_core::{
    BlockHeight, TransactionFactsArtifact, TransactionId, TransparentOutPoint, TransparentSpendFact,
};
use zinder_proto::v1::explorer::{
    PrevoutResolutionStatus, TransactionFeesRecord, TransparentInputDetail,
};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName,
};

/// Column family holding per-transaction fee records keyed by 32-byte txid.
pub const TRANSACTION_FEES_COLUMN_FAMILY: &str = "transaction_fees";

/// Column family holding the per-height txid index used by rewind.
///
/// Key: ascending big-endian block height (4 bytes).
/// Value: concatenation of 32-byte non-coinbase txids in canonical order.
pub const TRANSACTION_FEES_INDEX_COLUMN_FAMILY: &str = "transaction_fees_index";

/// Column families the consumer needs registered before its first write.
pub const TRANSACTION_FEES_COLUMN_FAMILIES: &[&str] = &[
    TRANSACTION_FEES_COLUMN_FAMILY,
    TRANSACTION_FEES_INDEX_COLUMN_FAMILY,
];

/// Stable consumer name persisted in the SDK cursor table.
pub const TRANSACTION_FEES_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("transaction_fees");

const TXID_LEN: usize = 32;

/// Materializes per-tx paid-fee records.
#[derive(Default)]
pub struct TransactionFeesConsumer;

impl TransactionFeesConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the persisted [`TransactionFeesRecord`] for `transaction_id`,
    /// when one was materialized.
    ///
    /// Used by the gRPC handlers (`TransactionDetail`, `RecentTransactions`)
    /// so the storage layout stays encapsulated; handlers never decode the
    /// stored bytes themselves.
    pub fn read_fees_record(
        store: &crate::store::DeriveStore,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionFeesRecord>, crate::error::DeriveStoreError> {
        let key = encode_internal_transaction_id(transaction_id);
        let Some(bytes) = store.get_consumer(TRANSACTION_FEES_COLUMN_FAMILY, &key)? else {
            return Ok(None);
        };
        Ok(TransactionFeesRecord::decode(bytes.as_slice()).ok())
    }

    /// Batch-reads fee records for `transaction_ids`, returning every record
    /// that was materialized. Issues one `multi_get_cf` so the read path
    /// avoids N seeks for an N-transaction page.
    pub fn read_fees_records_many(
        store: &crate::store::DeriveStore,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, TransactionFeesRecord>, crate::error::DeriveStoreError> {
        if transaction_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let keys: Vec<[u8; TXID_LEN]> = transaction_ids
            .iter()
            .copied()
            .map(encode_internal_transaction_id)
            .collect();
        let values = store.multi_get_consumer(TRANSACTION_FEES_COLUMN_FAMILY, &keys)?;
        let mut out = HashMap::with_capacity(values.len());
        for (transaction_id, maybe_bytes) in transaction_ids.iter().copied().zip(values) {
            if let Some(bytes) = maybe_bytes
                && let Ok(record) = TransactionFeesRecord::decode(bytes.as_slice())
            {
                out.insert(transaction_id, record);
            }
        }
        Ok(out)
    }
}

impl BlockKeyedConsumer for TransactionFeesConsumer {
    fn name(&self) -> DeriveConsumerName {
        TRANSACTION_FEES_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let transparent_spends = block.transparent_spends()?;
        let fees_cf = ctx
            .store
            .consumer_column_family(TRANSACTION_FEES_COLUMN_FAMILY)?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSACTION_FEES_INDEX_COLUMN_FAMILY)?;

        let non_coinbase = block
            .transactions
            .iter()
            .filter(|transaction| !transaction.public_facts.is_coinbase);
        let non_coinbase_count = block
            .transactions
            .iter()
            .filter(|transaction| !transaction.public_facts.is_coinbase)
            .count();
        let mut index_payload: Vec<u8> = Vec::with_capacity(non_coinbase_count * TXID_LEN);

        for transaction in non_coinbase {
            let transaction_id_bytes = transaction.location.transaction_id.as_bytes();
            let record = build_fee_record(transaction, transparent_spends.as_deref());
            let mut payload = Vec::with_capacity(record.encoded_len());
            record
                .encode(&mut payload)
                .map_err(|error| TransactionFeesConsumerError::Encode(error.to_string()))?;
            ctx.batch.put_cf(&fees_cf, transaction_id_bytes, payload);
            index_payload.extend_from_slice(&transaction_id_bytes);
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
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSACTION_FEES_INDEX_COLUMN_FAMILY)?;
        let index_key = encode_height_key_ascending(height);
        let Some(index_payload) = ctx
            .store
            .get_consumer(TRANSACTION_FEES_INDEX_COLUMN_FAMILY, &index_key)?
        else {
            return Ok(());
        };
        if index_payload.len() % TXID_LEN != 0 {
            return Err(Box::new(
                TransactionFeesConsumerError::IndexLengthMismatch {
                    height: height.value(),
                    bytes: index_payload.len(),
                },
            ));
        }
        let fees_cf = ctx
            .store
            .consumer_column_family(TRANSACTION_FEES_COLUMN_FAMILY)?;
        for chunk in index_payload.chunks_exact(TXID_LEN) {
            ctx.batch.delete_cf(&fees_cf, chunk);
        }
        ctx.batch.delete_cf(&index_cf, index_key);
        Ok(())
    }
}

fn build_fee_record(
    transaction: &TransactionFactsArtifact,
    transparent_spends: Option<&HashMap<TransparentOutPoint, TransparentSpendFact>>,
) -> TransactionFeesRecord {
    let counts = transaction.public_facts.counts;
    let logical_actions = counts.logical_actions();

    let Some(spends_by_outpoint) = transparent_spends else {
        return TransactionFeesRecord {
            paid_fee_zat: None,
            prevout_resolution_status: PrevoutResolutionStatus::Unavailable as i32,
            transparent_inputs: Vec::new(),
            logical_actions,
        };
    };

    let mut transparent_inputs: Vec<TransparentInputDetail> = Vec::new();
    let mut total_transparent_input_zat: i128 = 0;
    let mut total_transparent_output_zat: i128 = 0;
    let mut has_unresolved = false;
    for input in &transaction.transparent_inputs {
        let value_zat = spends_by_outpoint
            .get(&input.spent_outpoint)
            .map(|spend| spend.spent_value_zat);
        if let Some(zat) = value_zat {
            total_transparent_input_zat =
                total_transparent_input_zat.saturating_add(i128::from(zat));
        } else {
            has_unresolved = true;
        }
        transparent_inputs.push(TransparentInputDetail {
            input_index: input.input_index,
            value_zat,
        });
    }
    for output in &transaction.transparent_outputs {
        total_transparent_output_zat =
            total_transparent_output_zat.saturating_add(i128::from(output.value_zat));
    }
    let has_shielded_input = counts.has_shielded_input();
    let status = if has_unresolved {
        PrevoutResolutionStatus::Partial
    } else {
        PrevoutResolutionStatus::Resolved
    };
    let paid_fee_zat = if has_unresolved || has_shielded_input {
        None
    } else {
        total_transparent_input_zat
            .checked_sub(total_transparent_output_zat)
            .filter(|net| *net >= 0)
            .and_then(|net| u64::try_from(net).ok())
    };
    TransactionFeesRecord {
        paid_fee_zat,
        prevout_resolution_status: status as i32,
        transparent_inputs,
        logical_actions,
    }
}

/// Consumer-specific failure modes [`TransactionFeesConsumer`] can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionFeesConsumerError {
    /// Storage encoding of the materialized record failed.
    #[error("TransactionFeesRecord prost encode failed: {0}")]
    Encode(String),
    /// Per-height txid index was not a clean multiple of 32 bytes.
    #[error(
        "transaction_fees_index entry for height {height} has {bytes} bytes, not a multiple of 32"
    )]
    IndexLengthMismatch {
        /// Height whose persisted index was malformed.
        height: u32,
        /// Byte length actually persisted.
        bytes: usize,
    },
}
