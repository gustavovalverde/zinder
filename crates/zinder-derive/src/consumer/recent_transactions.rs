//! `RecentTransactions` derive consumer.
//!
//! Materializes a time-descending projection of canonical transactions
//! into the consumer-owned `recent_transactions` column family. The key
//! encoding `(reverse_height, in_block_position)` (8 bytes) lays the
//! newest entries first lexicographically so the handler-side
//! `ExplorerQuery.RecentTransactions` streams them as a single forward
//! range scan.
//!
//! Replaces the round-trip tree a "recent transactions" panel would
//! otherwise build: `BlockSummariesInRange` then per-block `BlockDetail`
//! then per-tx `TransactionDetail`. The consumer pays the parse cost
//! once at commit; the read path is a single bounded scan plus an
//! optional batched fee lookup.

use prost::Message as _;
use zinder_core::BlockHeight;
use zinder_core::wire::{
    encode_height_key_descending, encode_in_block_position, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex,
};
use zinder_proto::v1::explorer::{
    RecentTransactionEntry, TransactionComponentCounts as WireComponentCounts,
};
use zinder_proto::wire::encode_privacy_shape;

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName,
};

/// Column-family name the consumer owns.
pub const RECENT_TRANSACTIONS_COLUMN_FAMILY: &str = "recent_transactions";

/// Stable consumer name persisted in the SDK cursor table.
pub const RECENT_TRANSACTIONS_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("recent_transactions");

/// Length of one storage key: 4 reverse-height + 4 in-block position.
const RECENT_TRANSACTIONS_KEY_LEN: usize = 8;

/// Materializes one [`RecentTransactionEntry`] per canonical transaction.
#[derive(Default)]
pub struct RecentTransactionsConsumer;

impl RecentTransactionsConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the storage key for one `(height, in_block_position)` row.
    #[must_use]
    pub fn key_for_row(
        height: BlockHeight,
        in_block_position: u32,
    ) -> [u8; RECENT_TRANSACTIONS_KEY_LEN] {
        let mut key = [0u8; RECENT_TRANSACTIONS_KEY_LEN];
        key[0..4].copy_from_slice(&encode_height_key_descending(height));
        key[4..8].copy_from_slice(&encode_in_block_position(in_block_position));
        key
    }
}

impl BlockKeyedConsumer for RecentTransactionsConsumer {
    fn name(&self) -> DeriveConsumerName {
        RECENT_TRANSACTIONS_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(RECENT_TRANSACTIONS_COLUMN_FAMILY)?;
        let block_time_unix_seconds = block.block_time_unix_seconds;
        for transaction in &block.transactions {
            let in_block_position = transaction.location.tx_index_in_block;
            let facts = &transaction.public_facts;
            let is_coinbase = facts.is_coinbase;
            let counts = facts.counts;
            let logical_actions = counts.logical_actions();
            let privacy_shape = facts.privacy_shape;
            let size_bytes = facts.size_bytes;
            let zip317_conventional_fee_zat = if is_coinbase {
                None
            } else {
                Some(counts.zip317_conventional_fee_zat())
            };
            let entry = RecentTransactionEntry {
                transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
                block_height: block.height.value(),
                block_hash: encode_rpc_block_hash_hex(block.block_hash),
                block_time_unix_seconds,
                is_coinbase,
                privacy_shape: encode_privacy_shape(privacy_shape) as i32,
                component_counts: Some(WireComponentCounts {
                    transparent_input_count: counts.transparent_input_count,
                    transparent_output_count: counts.transparent_output_count,
                    sapling_spend_count: counts.sapling_spend_count,
                    sapling_output_count: counts.sapling_output_count,
                    orchard_action_count: counts.orchard_action_count,
                    sprout_joinsplit_count: counts.sprout_joinsplit_count,
                }),
                size_bytes,
                zip317_conventional_fee_zat,
                paid_fee_zat: None,
                logical_actions,
            };
            let mut payload = Vec::with_capacity(entry.encoded_len());
            entry
                .encode(&mut payload)
                .map_err(|error| RecentTransactionsConsumerError::Encode(error.to_string()))?;
            ctx.batch.put_cf(
                &cf,
                Self::key_for_row(block.height, in_block_position),
                payload,
            );
        }
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(RECENT_TRANSACTIONS_COLUMN_FAMILY)?;
        let mut start_key = [0u8; RECENT_TRANSACTIONS_KEY_LEN];
        start_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        let mut end_key = [0xFFu8; RECENT_TRANSACTIONS_KEY_LEN];
        end_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        ctx.batch
            .delete_range_cf(&cf, start_key.as_slice(), end_key.as_slice());
        Ok(())
    }
}

/// Consumer-specific failure modes [`RecentTransactionsConsumer`] can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecentTransactionsConsumerError {
    /// Storage encoding of the materialized entry failed.
    #[error("RecentTransactionEntry prost encode failed: {0}")]
    Encode(String),
}
