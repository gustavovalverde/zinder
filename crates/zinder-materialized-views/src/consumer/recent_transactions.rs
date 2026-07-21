//! `RecentTransactions` materialized-view consumer.
//!
//! Materializes a time-descending materialized view of canonical transactions
//! into the consumer-owned `recent_transactions` column family. The key
//! encoding `(reverse_height, reverse_in_block_position)` (8 bytes) lays the
//! newest entries first lexicographically, including later transactions
//! within the same block, so the handler-side
//! `ExplorerQuery.RecentTransactions` streams them as a single forward
//! range scan.
//!
//! The consumer pays the parse cost once at commit; the read path serves the
//! stream with one bounded scan plus an optional batched fee lookup.

use prost::Message as _;
use zinder_core::wire::{
    encode_height_key_descending, encode_in_block_position, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHeight, TransactionFactsArtifact};
use zinder_proto::v1::explorer::{
    RecentTransactionEntry, TransactionComponentCounts as WireComponentCounts,
};
use zinder_proto::wire::encode_privacy_shape;

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, MaterializedViewConsumerCtx,
    MaterializedViewConsumerError, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
};

/// Column-family name the consumer owns.
pub const RECENT_TRANSACTIONS_COLUMN_FAMILY: &str = "recent_transactions";

/// Stable consumer name persisted in the SDK cursor table.
pub const RECENT_TRANSACTIONS_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("recent_transactions");

/// On-disk schema declaration for the recent-transactions materialized-view consumer.
///
/// Version 2 reverses the in-block position in the row key so a bounded
/// forward scan matches descending `(block_height, tx_index)` order. Version
/// 1 stores the same payload under ascending in-block positions and is
/// rejected by exact manifest admission rather than reinterpreted.
pub const RECENT_TRANSACTIONS_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        RECENT_TRANSACTIONS_CONSUMER_NAME,
        2,
        &[RECENT_TRANSACTIONS_COLUMN_FAMILY],
    );

/// Length of one storage key: 4 reverse-height + 4 reverse in-block position.
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

    /// Returns the number of rows this consumer will write for `transactions`.
    #[must_use]
    pub fn projected_row_count_for_transactions(
        transactions: &[TransactionFactsArtifact],
    ) -> usize {
        transactions.len()
    }

    /// Returns the number of rows this consumer will write for `block`.
    #[must_use]
    pub fn projected_row_count_for_block(block: &BlockCommitContext) -> usize {
        Self::projected_row_count_for_transactions(&block.transactions)
    }

    /// Returns the storage key for one `(height, in_block_position)` row.
    #[must_use]
    pub fn key_for_row(
        height: BlockHeight,
        in_block_position: u32,
    ) -> [u8; RECENT_TRANSACTIONS_KEY_LEN] {
        let mut key = [0u8; RECENT_TRANSACTIONS_KEY_LEN];
        key[0..4].copy_from_slice(&encode_height_key_descending(height));
        key[4..8].copy_from_slice(&encode_in_block_position(u32::MAX - in_block_position));
        key
    }
}

impl BlockKeyedConsumer for RecentTransactionsConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        RECENT_TRANSACTIONS_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
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
                    ironwood_action_count: counts.ironwood_action_count,
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
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(RECENT_TRANSACTIONS_COLUMN_FAMILY)?;
        let mut start_key = [0u8; RECENT_TRANSACTIONS_KEY_LEN];
        start_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        let mut end_key = [0xFFu8; RECENT_TRANSACTIONS_KEY_LEN];
        end_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        ctx.batch
            .delete_range_cf(&cf, start_key.as_slice(), end_key.as_slice());
        // RocksDB range deletes exclude the end key. Version 2 maps the
        // coinbase position zero to the all-`0xFF` suffix, so remove that
        // boundary row explicitly as part of the same atomic batch.
        ctx.batch.delete_cf(&cf, end_key);
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

#[cfg(test)]
mod tests {
    use prost::Message as _;
    use rust_rocksdb::WriteBatch;
    use zinder_core::wire::encode_rpc_transaction_id_hex;
    use zinder_core::{
        BlockHash, BlockHeight, LockTime, PrivacyShape, TransactionComponentCounts,
        TransactionFactsArtifact, TransactionId, TransactionLocation, TransactionPublicFacts,
        TransactionVersion,
    };
    use zinder_proto::v1::explorer::RecentTransactionEntry;

    use super::{
        RECENT_TRANSACTIONS_COLUMN_FAMILY, RECENT_TRANSACTIONS_SCHEMA, RecentTransactionsConsumer,
    };
    use crate::{
        BlockCommitContext, BlockCommitInput, BlockKeyedConsumer, MaterializedViewConsumerCtx,
        MaterializedViewStore, MaterializedViewStoreOptions, TransparentSpendFacts,
    };

    fn transaction(seed: u8, tx_index_in_block: u32) -> TransactionFactsArtifact {
        let transaction_id = TransactionId::from_bytes([seed; 32]);
        TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(10),
                BlockHash::from_bytes([0xA0; 32]),
                tx_index_in_block,
            ),
            TransactionPublicFacts {
                transaction_id,
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
            },
        )
    }

    #[test]
    fn projected_row_count_matches_transaction_count() {
        let transactions = vec![transaction(1, 0), transaction(2, 1), transaction(3, 2)];

        assert_eq!(
            RecentTransactionsConsumer::projected_row_count_for_transactions(&transactions),
            3
        );
    }

    #[test]
    fn row_keys_order_newer_blocks_and_later_transactions_first() {
        let older_block = RecentTransactionsConsumer::key_for_row(BlockHeight::new(9), 10);
        let newest_block_earlier_transaction =
            RecentTransactionsConsumer::key_for_row(BlockHeight::new(10), 1);
        let newest_block_later_transaction =
            RecentTransactionsConsumer::key_for_row(BlockHeight::new(10), 2);

        assert!(newest_block_later_transaction < newest_block_earlier_transaction);
        assert!(newest_block_earlier_transaction < older_block);
        assert_ne!(
            newest_block_later_transaction,
            newest_block_earlier_transaction
        );
        assert_eq!(RECENT_TRANSACTIONS_SCHEMA.schema_version, 2);
    }

    #[test]
    fn producer_persists_newest_block_and_later_transaction_positions_first()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let tempdir = tempfile::tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            MaterializedViewStoreOptions {
                consumers: &[RECENT_TRANSACTIONS_SCHEMA],
                ..MaterializedViewStoreOptions::default()
            },
        )?;
        let older_block_hash = BlockHash::from_bytes([9; 32]);
        let newer_block_hash = BlockHash::from_bytes([10; 32]);
        let older_block = BlockCommitContext::new(
            BlockCommitInput {
                height: BlockHeight::new(9),
                block_hash: older_block_hash,
                previous_block_hash: BlockHash::from_bytes([8; 32]),
                block_time_unix_seconds: 900,
                block_size_bytes: 0,
                transactions: vec![transaction_at(200, 9, older_block_hash, 0)],
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        );
        let newer_transactions = (0_u8..=100)
            .map(|position| transaction_at(position, 10, newer_block_hash, u32::from(position)))
            .collect();
        let newer_block = BlockCommitContext::new(
            BlockCommitInput {
                height: BlockHeight::new(10),
                block_hash: newer_block_hash,
                previous_block_hash: older_block_hash,
                block_time_unix_seconds: 1_000,
                block_size_bytes: 0,
                transactions: newer_transactions,
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        );
        let mut consumer = RecentTransactionsConsumer::new();
        let mut batch = WriteBatch::default();
        let mut context = MaterializedViewConsumerCtx {
            store: &store,
            batch: &mut batch,
        };
        consumer.apply_block(&older_block, &mut context)?;
        consumer.apply_block(&newer_block, &mut context)?;
        store.write_batch(&batch)?;

        let mut transaction_ids = Vec::new();
        store.visit_consumer_rows(RECENT_TRANSACTIONS_COLUMN_FAMILY, |_key, payload| {
            let entry =
                RecentTransactionEntry::decode(payload).map_err(|error| error.to_string())?;
            transaction_ids.push(entry.transaction_id);
            Ok(())
        })?;
        let expected_newer = (0_u8..=100)
            .rev()
            .map(|position| {
                encode_rpc_transaction_id_hex(TransactionId::from_bytes([position; 32]))
            })
            .collect::<Vec<_>>();
        assert_eq!(&transaction_ids[..101], expected_newer.as_slice());
        assert_eq!(
            transaction_ids[101],
            encode_rpc_transaction_id_hex(TransactionId::from_bytes([200; 32])),
        );

        let mut revert_batch = WriteBatch::default();
        let mut revert_context = MaterializedViewConsumerCtx {
            store: &store,
            batch: &mut revert_batch,
        };
        consumer.revert_block(BlockHeight::new(10), &mut revert_context)?;
        store.write_batch(&revert_batch)?;
        assert_eq!(
            store.consumer_row_count(RECENT_TRANSACTIONS_COLUMN_FAMILY)?,
            1,
        );
        Ok(())
    }

    fn transaction_at(
        seed: u8,
        height: u32,
        block_hash: BlockHash,
        tx_index_in_block: u32,
    ) -> TransactionFactsArtifact {
        let transaction_id = TransactionId::from_bytes([seed; 32]);
        TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(height),
                block_hash,
                tx_index_in_block,
            ),
            TransactionPublicFacts {
                transaction_id,
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
            },
        )
    }
}
