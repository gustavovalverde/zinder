//! Canonical transaction-history derive consumer.
//!
//! Materializes every canonical transaction into a time-descending projection.
//! The projection owns a separate `transaction_history` column family and
//! consumer identity so the established `RecentTransactions` contract and
//! persisted rows remain available independently. The key encoding
//! `(reverse_height, in_block_position)` (8 bytes) lays the
//! newest blocks first lexicographically.
//!
//! Replaces the round-trip tree a "recent transactions" panel would
//! otherwise build: `BlockSummariesInRange` then per-block `BlockDetail`
//! then per-tx `TransactionDetail`. The consumer pays the parse cost
//! once at commit; the read path is a single bounded scan plus an
//! optional batched fee lookup.

use prost::Message as _;
use zinder_core::wire::{
    decode_height_key_descending, decode_in_block_position, encode_height_key_descending,
    encode_in_block_position, encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHeight, TransactionFactsArtifact};
use zinder_proto::v1::explorer::{
    TransactionComponentCounts as WireComponentCounts, TransactionHistoryEntry,
};
use zinder_proto::wire::encode_privacy_shape;

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, BlockProjectionCheckpoint, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveConsumerSchema,
};
use crate::{ConsumerProjectionCoverage, ConsumerProjectionState};
use zinder_store::ChainEvent;

/// Stable physical column-family name owned by transaction history.
pub const TRANSACTION_HISTORY_COLUMN_FAMILY: &str = "transaction_history";

/// Stable physical consumer name persisted in the SDK cursor table.
pub const TRANSACTION_HISTORY_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("transaction_history");

/// On-disk schema declaration for canonical transaction history.
///
/// Version 1 includes the full history entry and atomic projection-state
/// maintenance. The projection has no predecessor because it is additive to
/// the separately retained `recent_transactions` consumer.
pub const TRANSACTION_HISTORY_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    TRANSACTION_HISTORY_CONSUMER_NAME,
    1,
    &[TRANSACTION_HISTORY_COLUMN_FAMILY],
);

/// Length of one storage key: 4 reverse-height + 4 in-block position.
pub const TRANSACTION_HISTORY_KEY_LEN: usize = 8;

/// Materializes one [`TransactionHistoryEntry`] per canonical transaction.
#[derive(Default)]
pub struct TransactionHistoryConsumer;

impl TransactionHistoryConsumer {
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
    ) -> [u8; TRANSACTION_HISTORY_KEY_LEN] {
        let mut key = [0u8; TRANSACTION_HISTORY_KEY_LEN];
        key[0..4].copy_from_slice(&encode_height_key_descending(height));
        key[4..8].copy_from_slice(&encode_in_block_position(in_block_position));
        key
    }

    /// Builds the persisted projection row for one canonical transaction.
    #[must_use]
    pub fn project_entry(
        block: &BlockCommitContext,
        transaction: &TransactionFactsArtifact,
    ) -> TransactionHistoryEntry {
        let in_block_position = transaction.location.tx_index_in_block;
        let facts = &transaction.public_facts;
        let counts = facts.counts;
        TransactionHistoryEntry {
            transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
            block_height: block.height.value(),
            block_hash: encode_rpc_block_hash_hex(block.block_hash),
            block_time_unix_seconds: block.block_time_unix_seconds,
            is_coinbase: facts.is_coinbase,
            privacy_shape: encode_privacy_shape(facts.privacy_shape) as i32,
            component_counts: Some(WireComponentCounts {
                transparent_input_count: counts.transparent_input_count,
                transparent_output_count: counts.transparent_output_count,
                sapling_spend_count: counts.sapling_spend_count,
                sapling_output_count: counts.sapling_output_count,
                orchard_action_count: counts.orchard_action_count,
                ironwood_action_count: counts.ironwood_action_count,
                sprout_joinsplit_count: counts.sprout_joinsplit_count,
            }),
            size_bytes: facts.size_bytes,
            zip317_conventional_fee_zat: (!facts.is_coinbase)
                .then(|| counts.zip317_conventional_fee_zat()),
            paid_fee_zat: None,
            logical_actions: counts.logical_actions(),
            transaction_index: in_block_position,
            intrinsic_value_balances: None,
        }
    }

    /// Reads and normalizes every persisted transaction row at `height`.
    ///
    /// Version-1 payloads omitted `transaction_index`; the unchanged key is
    /// authoritative for every row version.
    pub fn entries_at_height(
        store: &crate::DeriveStore,
        height: BlockHeight,
    ) -> Result<Vec<TransactionHistoryEntry>, TransactionHistoryConsumerError> {
        const MAX_BLOCK_TRANSACTIONS: usize = 100_000;
        let start_key = Self::key_for_row(height, 0);
        let end_key = Self::key_for_row(height, u32::MAX);
        let rows = store.range_iterate_consumer(
            TRANSACTION_HISTORY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_BLOCK_TRANSACTIONS.saturating_add(1),
        )?;
        if rows.len() > MAX_BLOCK_TRANSACTIONS {
            return Err(TransactionHistoryConsumerError::BlockTransactionLimit {
                height: height.value(),
            });
        }
        rows.into_iter()
            .map(|(key, payload)| decode_persisted_entry(height, &key, &payload))
            .collect()
    }
}

impl BlockKeyedConsumer for TransactionHistoryConsumer {
    fn name(&self) -> DeriveConsumerName {
        TRANSACTION_HISTORY_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        for transaction in &block.transactions {
            let in_block_position = transaction.location.tx_index_in_block;
            let entry = Self::project_entry(block, transaction);
            let mut payload = Vec::with_capacity(entry.encoded_len());
            entry
                .encode(&mut payload)
                .map_err(|error| TransactionHistoryConsumerError::Encode(error.to_string()))?;
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
            .consumer_column_family(TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        let mut start_key = [0u8; TRANSACTION_HISTORY_KEY_LEN];
        start_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        let mut end_key = [0xFFu8; TRANSACTION_HISTORY_KEY_LEN];
        end_key[0..4].copy_from_slice(&encode_height_key_descending(height));
        ctx.batch
            .delete_range_cf(&cf, start_key.as_slice(), end_key.as_slice());
        Ok(())
    }

    fn stage_chain_event_checkpoint(
        &mut self,
        checkpoint: BlockProjectionCheckpoint<'_>,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let projection_tip_height = checkpoint
            .projection_tip_height
            .ok_or(TransactionHistoryConsumerError::IncompleteProjectionCheckpoint)?;
        let projection_tip_hash = checkpoint
            .projection_tip_hash
            .ok_or(TransactionHistoryConsumerError::IncompleteProjectionCheckpoint)?;
        let current = ctx
            .store
            .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?;
        // Re-applied committed chunks must not regress the fence a later chunk advanced.
        if let Some(state) = current
            && matches!(checkpoint.chain_event, ChainEvent::ChainCommitted { .. })
            && projection_tip_height < state.projection_tip_height
        {
            return Ok(());
        }
        let revision = current
            .map_or(Some(1), |state| state.revision.checked_add(1))
            .ok_or(TransactionHistoryConsumerError::ProjectionRevisionOverflow)?;
        let coverage = advance_verified_coverage(
            current.and_then(|state| state.coverage),
            checkpoint,
            projection_tip_height,
            projection_tip_hash,
        );
        ctx.store.stage_consumer_projection_state(
            ctx.batch,
            TRANSACTION_HISTORY_CONSUMER_NAME,
            ConsumerProjectionState {
                projection_epoch_id: checkpoint.chain_epoch.id,
                projection_tip_height,
                projection_tip_hash,
                revision,
                coverage,
            },
        )?;
        Ok(())
    }
}

fn decode_persisted_entry(
    requested_height: BlockHeight,
    key: &[u8],
    payload: &[u8],
) -> Result<TransactionHistoryEntry, TransactionHistoryConsumerError> {
    if key.len() != TRANSACTION_HISTORY_KEY_LEN {
        return Err(TransactionHistoryConsumerError::InvalidKeyLength { bytes: key.len() });
    }
    let indexed_height = decode_height_key_descending(&key[..4])
        .map_err(|error| TransactionHistoryConsumerError::Decode(error.to_string()))?;
    if indexed_height != requested_height {
        return Err(TransactionHistoryConsumerError::UnexpectedHeight {
            requested: requested_height.value(),
            indexed: indexed_height.value(),
        });
    }
    let transaction_index = decode_in_block_position(&key[4..])
        .map_err(|error| TransactionHistoryConsumerError::Decode(error.to_string()))?;
    let mut entry = TransactionHistoryEntry::decode(payload)
        .map_err(|error| TransactionHistoryConsumerError::Decode(error.to_string()))?;
    entry.transaction_index = transaction_index;
    Ok(entry)
}

fn advance_verified_coverage(
    coverage: Option<ConsumerProjectionCoverage>,
    checkpoint: BlockProjectionCheckpoint<'_>,
    projection_tip_height: BlockHeight,
    projection_tip_hash: zinder_core::BlockHash,
) -> Option<ConsumerProjectionCoverage> {
    let coverage = coverage?;
    match checkpoint.chain_event {
        ChainEvent::ChainCommitted { committed } => {
            let range = committed.block_range;
            if range.start > range.end {
                return Some(coverage);
            }
            if coverage.complete_through_height.next() == Some(range.start) {
                return Some(ConsumerProjectionCoverage {
                    complete_from_height: coverage.complete_from_height,
                    complete_through_height: projection_tip_height,
                    complete_through_hash: projection_tip_hash,
                });
            }
            Some(coverage)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            let replacement = committed.block_range;
            let reverted_start = reverted.block_range.start;
            let covers_reverted_boundary = coverage.complete_through_height >= reverted_start;
            let replacement_starts_at_reverted_boundary = replacement.start == reverted_start;
            if covers_reverted_boundary && replacement_starts_at_reverted_boundary {
                return Some(ConsumerProjectionCoverage {
                    complete_from_height: coverage.complete_from_height,
                    complete_through_height: projection_tip_height,
                    complete_through_hash: projection_tip_hash,
                });
            }
            if coverage.complete_through_height < reverted_start {
                return Some(coverage);
            }
            None
        }
        _ => Some(coverage),
    }
}

/// Consumer-specific failure modes [`TransactionHistoryConsumer`] can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionHistoryConsumerError {
    /// Storage encoding of the materialized entry failed.
    #[error("TransactionHistoryEntry prost encode failed: {0}")]
    Encode(String),
    /// Persisted row or key bytes could not be decoded.
    #[error("transaction-history row decode failed: {0}")]
    Decode(String),
    /// Persisted key length does not match the stable row key.
    #[error("transaction-history key has invalid length {bytes}")]
    InvalidKeyLength {
        /// Observed key length.
        bytes: usize,
    },
    /// A bounded height scan returned a key for another height.
    #[error("transaction-history scan requested height {requested} but found {indexed}")]
    UnexpectedHeight {
        /// Requested block height.
        requested: u32,
        /// Height decoded from the key.
        indexed: u32,
    },
    /// One block exceeded the defensive transaction-row limit.
    #[error("transaction-history block {height} exceeds the transaction-row limit")]
    BlockTransactionLimit {
        /// Block height whose row count exceeded the limit.
        height: u32,
    },
    /// Derive-store read failed.
    #[error(transparent)]
    Store(#[from] crate::DeriveStoreError),
    /// A dispatch omitted one or more block contexts required by the projection.
    #[error("transaction-history projection checkpoint is missing its indexed tip")]
    IncompleteProjectionCheckpoint,
    /// Projection-state revision exhausted its integer domain.
    #[error("transaction-history projection revision overflowed")]
    ProjectionRevisionOverflow,
}

#[cfg(test)]
mod tests {
    use eyre::Result;
    use rust_rocksdb::WriteBatch;
    use tempfile::tempdir;
    use zinder_core::{
        ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
        ChainTipMetadata, LockTime, Network, PrivacyShape, TransactionComponentCounts,
        TransactionFactsArtifact, TransactionId, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, UnixTimestampMillis,
    };
    use zinder_store::{ChainEpochCommitted, ChainEvent};

    use super::{
        BlockProjectionCheckpoint, ConsumerProjectionCoverage, ConsumerProjectionState,
        TRANSACTION_HISTORY_CONSUMER_NAME, TransactionHistoryConsumer,
    };
    use crate::consumer::{BlockKeyedConsumer, DeriveConsumerCtx};
    use crate::store::{DeriveStore, DeriveStoreOptions};

    fn chain_epoch(id: u64, tip: u32) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(tip),
            visible_tip_hash: BlockHash::from_bytes([0x66; 32]),
            settled_tip_height: BlockHeight::new(1),
            settled_tip_hash: BlockHash::from_bytes([0x01; 32]),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_200_000 + id),
        }
    }

    fn committed_chunk_event(chain_epoch: ChainEpoch, start: u32, end: u32) -> ChainEvent {
        ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(start),
                    BlockHeight::new(end),
                ),
            },
        }
    }

    fn seed_projection_state(
        store: &DeriveStore,
        tip: u32,
        complete_through_height: u32,
    ) -> Result<ConsumerProjectionState> {
        let state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(1),
            projection_tip_height: BlockHeight::new(tip),
            projection_tip_hash: BlockHash::from_bytes([0x33; 32]),
            revision: 5,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(complete_through_height),
                complete_through_hash: BlockHash::from_bytes([0x44; 32]),
            }),
        };
        store.put_consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME, state)?;
        Ok(state)
    }

    fn stage_and_commit_checkpoint(
        store: &DeriveStore,
        chain_epoch: ChainEpoch,
        chunk_event: &ChainEvent,
        tip: u32,
    ) -> Result<()> {
        let checkpoint = BlockProjectionCheckpoint {
            chain_epoch,
            chain_event: chunk_event,
            projection_tip_height: Some(BlockHeight::new(tip)),
            projection_tip_hash: Some(BlockHash::from_bytes([0x55; 32])),
        };
        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        TransactionHistoryConsumer::new()
            .stage_chain_event_checkpoint(checkpoint, &mut ctx)
            .map_err(|error| eyre::eyre!("{error}"))?;
        store.write_batch(&batch)?;
        Ok(())
    }

    #[test]
    fn re_applied_lower_committed_chunk_preserves_advanced_coverage() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        let armed = seed_projection_state(&store, 180_512, 180_512)?;

        let epoch = chain_epoch(1, 180_512);
        let re_applied_chunk = committed_chunk_event(epoch, 180_001, 180_256);
        stage_and_commit_checkpoint(&store, epoch, &re_applied_chunk, 180_256)?;

        let stored = store
            .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("expected the seeded projection state to survive"))?;
        assert_eq!(stored, armed);
        Ok(())
    }

    #[test]
    fn contiguous_committed_chunk_advances_coverage_to_tip() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        seed_projection_state(&store, 180_256, 180_256)?;

        let epoch = chain_epoch(1, 180_512);
        let next_chunk = committed_chunk_event(epoch, 180_257, 180_512);
        stage_and_commit_checkpoint(&store, epoch, &next_chunk, 180_512)?;

        let stored = store
            .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("expected an advanced projection state"))?;
        assert_eq!(stored.projection_tip_height, BlockHeight::new(180_512));
        let coverage = stored
            .coverage
            .ok_or_else(|| eyre::eyre!("expected verified coverage to advance"))?;
        assert_eq!(coverage.complete_from_height, BlockHeight::new(1));
        assert_eq!(coverage.complete_through_height, BlockHeight::new(180_512));
        assert_eq!(stored.revision, 6);
        Ok(())
    }

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
                privacy_shape: PrivacyShape::Unclassified,
                is_coinbase: false,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                unsupported_sections: Vec::new(),
            },
        )
    }

    #[test]
    fn projected_row_count_matches_transaction_count() {
        let transactions = vec![transaction(1, 0), transaction(2, 1), transaction(3, 2)];

        assert_eq!(
            TransactionHistoryConsumer::projected_row_count_for_transactions(&transactions),
            3
        );
    }
}
