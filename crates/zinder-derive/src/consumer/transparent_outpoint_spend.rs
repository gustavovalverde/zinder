//! `TransparentOutpointSpend` derive consumer.
//!
//! Materializes durable spender identity keyed by spent outpoint. The canonical
//! store retains transparent spend facts only inside the reorg window and its
//! retention release floor; below that floor the safe-tip sweep deletes them.
//! This consumer records the spender of every settled outpoint so
//! `WalletQuery.TransparentSpendsByOutpoint` can resolve a spend long after the
//! canonical fact is swept, which wallet offline-recovery depends on.
//!
//! Authority split: spentness is decided by the canonical
//! `TransparentUnspentOutputsByOutpoint` (durable, LtHash16-committed). This
//! projection answers only *who* spent an outpoint, never *whether* it is
//! spent. A missing row means "no spender recorded", never "unspent".
//!
//! Schema cost: the canonical retention sweep releases only up to this
//! projection's durable height, and swept facts can never be re-derived (their
//! source rows are gone). Bumping [`TRANSPARENT_OUTPOINT_SPEND_SCHEMA`]'s
//! version wipes and rebuilds this projection from retained canonical events;
//! if the rebuild floor sits above already-swept heights the only remedy is a
//! full canonical re-ingest. Keep the row format conservative.

use std::collections::HashMap;

use zinder_core::wire::{
    OUTPOINT_KEY_LEN, decode_height_key_ascending, decode_internal_block_hash,
    decode_internal_transaction_id, encode_height_key_ascending, encode_internal_block_hash,
    encode_internal_transaction_id, encode_outpoint_key,
};
use zinder_core::{BlockHeight, TransparentOutPoint, TransparentSpendEntry, TransparentSpendFact};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName, DeriveConsumerSchema,
};
use crate::error::{DeriveStoreColumnFamily, DeriveStoreError};

/// Primary rows keyed by spent outpoint, valued with the spender identity.
pub const TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY: &str = "transparent_outpoint_spend";

/// Per-height index used to delete the primary rows during reorg rewind.
pub const TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY: &str = "transparent_outpoint_spend_index";

/// Column families the consumer needs registered before its first write.
pub const TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILIES: &[&str] = &[
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
];

/// Stable consumer name persisted in the derive cursor table.
pub const TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("transparent_outpoint_spend");

/// On-disk schema declaration for the transparent-outpoint-spend consumer.
///
/// Moving this version is expensive: see the module-level schema-cost note.
pub const TRANSPARENT_OUTPOINT_SPEND_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
    1,
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILIES,
);

const TRANSACTION_ID_LEN: usize = 32;
const BLOCK_HASH_LEN: usize = 32;
const HEIGHT_LEN: usize = 4;
const INPUT_INDEX_LEN: usize = 4;
const SPEND_VALUE_LEN: usize = TRANSACTION_ID_LEN + BLOCK_HASH_LEN + HEIGHT_LEN + INPUT_INDEX_LEN;
const SPENDING_TRANSACTION_ID_RANGE: std::ops::Range<usize> = 0..TRANSACTION_ID_LEN;
const SPENDING_BLOCK_HASH_RANGE: std::ops::Range<usize> =
    TRANSACTION_ID_LEN..TRANSACTION_ID_LEN + BLOCK_HASH_LEN;
const SPENDING_HEIGHT_RANGE: std::ops::Range<usize> =
    TRANSACTION_ID_LEN + BLOCK_HASH_LEN..TRANSACTION_ID_LEN + BLOCK_HASH_LEN + HEIGHT_LEN;
const INPUT_INDEX_RANGE: std::ops::Range<usize> =
    TRANSACTION_ID_LEN + BLOCK_HASH_LEN + HEIGHT_LEN..SPEND_VALUE_LEN;

/// Materializes durable transparent-outpoint spender identity.
#[derive(Default)]
pub struct TransparentOutpointSpendConsumer;

impl TransparentOutpointSpendConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Resolves the recorded spender for each requested outpoint.
    ///
    /// Outpoints with no recorded spender are absent from the returned map;
    /// absence means "no spender recorded here", never "unspent".
    pub fn read_spends_by_outpoints(
        store: &crate::store::DeriveStore,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentSpendEntry>, DeriveStoreError> {
        if outpoints.is_empty() {
            return Ok(HashMap::new());
        }
        let keys = outpoints
            .iter()
            .map(|outpoint| encode_outpoint_key(*outpoint))
            .collect::<Vec<_>>();
        let stored_rows =
            store.multi_get_consumer(TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY, &keys)?;
        let mut spends = HashMap::with_capacity(outpoints.len());
        for (outpoint, stored_row) in outpoints.iter().zip(stored_rows) {
            let Some(stored_row) = stored_row else {
                continue;
            };
            spends.insert(*outpoint, decode_spend_entry(*outpoint, &stored_row)?);
        }
        Ok(spends)
    }
}

impl BlockKeyedConsumer for TransparentOutpointSpendConsumer {
    fn name(&self) -> DeriveConsumerName {
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        // This projection gates irreversible canonical deletion through its
        // durable height, so it must never advance that height for a block
        // whose spenders it could not observe.
        let Some(spends_by_outpoint) = block.transparent_spends()? else {
            return Err(Box::new(
                TransparentOutpointSpendConsumerError::OfflineSpendFacts {
                    height: block.height.value(),
                },
            ));
        };
        let rows = collect_spend_rows(block, &spends_by_outpoint);
        let spend_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY)?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)?;

        let mut index_payload = Vec::with_capacity(rows.len() * OUTPOINT_KEY_LEN);
        for (outpoint, spend) in rows {
            let key = encode_outpoint_key(outpoint);
            ctx.batch.put_cf(
                &spend_cf,
                key,
                encode_transparent_spend_row_value(&spend).as_slice(),
            );
            index_payload.extend_from_slice(&key);
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
        let index_key = encode_height_key_ascending(height);
        let Some(index_payload) = ctx
            .store
            .get_consumer(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY, &index_key)?
        else {
            return Ok(());
        };
        if index_payload.len() % OUTPOINT_KEY_LEN != 0 {
            return Err(Box::new(
                TransparentOutpointSpendConsumerError::IndexLengthMismatch {
                    height: height.value(),
                    bytes: index_payload.len(),
                },
            ));
        }
        let spend_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY)?;
        let index_cf = ctx
            .store
            .consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)?;
        for chunk in index_payload.chunks_exact(OUTPOINT_KEY_LEN) {
            ctx.batch.delete_cf(&spend_cf, chunk);
        }
        ctx.batch.delete_cf(&index_cf, index_key);
        Ok(())
    }
}

fn collect_spend_rows(
    block: &BlockCommitContext,
    spends_by_outpoint: &HashMap<TransparentOutPoint, TransparentSpendFact>,
) -> Vec<(TransparentOutPoint, TransparentSpendFact)> {
    let mut rows = Vec::new();
    for transaction in &block.transactions {
        for input in &transaction.transparent_inputs {
            if let Some(spend) = spends_by_outpoint.get(&input.spent_outpoint) {
                rows.push((input.spent_outpoint, spend.clone()));
            }
        }
    }
    rows
}

/// Encodes a transparent spend fact into its durable projection row value.
///
/// Layout: spending transaction id, spending block hash, spending height, then
/// transparent input index. This is the single owner of the row's byte layout;
/// test seeders reuse it instead of hand-writing the same offsets.
#[must_use]
pub fn encode_transparent_spend_row_value(spend: &TransparentSpendFact) -> [u8; SPEND_VALUE_LEN] {
    let mut encoded = [0u8; SPEND_VALUE_LEN];
    encoded[SPENDING_TRANSACTION_ID_RANGE].copy_from_slice(&encode_internal_transaction_id(
        spend.spending_transaction_id,
    ));
    encoded[SPENDING_BLOCK_HASH_RANGE]
        .copy_from_slice(&encode_internal_block_hash(spend.block_hash));
    encoded[SPENDING_HEIGHT_RANGE]
        .copy_from_slice(&encode_height_key_ascending(spend.block_height));
    encoded[INPUT_INDEX_RANGE].copy_from_slice(&spend.input_index.to_be_bytes());
    encoded
}

fn decode_spend_entry(
    spent_outpoint: TransparentOutPoint,
    stored_row: &[u8],
) -> Result<TransparentSpendEntry, DeriveStoreError> {
    if stored_row.len() != SPEND_VALUE_LEN {
        return Err(decode_error("spend value length is invalid"));
    }
    let spending_transaction_id =
        decode_internal_transaction_id(&stored_row[SPENDING_TRANSACTION_ID_RANGE])
            .map_err(|error| decode_error(error.to_string()))?;
    let spending_block_hash = decode_internal_block_hash(&stored_row[SPENDING_BLOCK_HASH_RANGE])
        .map_err(|error| decode_error(error.to_string()))?;
    let spending_block_height = decode_height_key_ascending(&stored_row[SPENDING_HEIGHT_RANGE])
        .map_err(|error| decode_error(error.to_string()))?;
    let input_index_bytes: [u8; INPUT_INDEX_LEN] = stored_row[INPUT_INDEX_RANGE]
        .try_into()
        .map_err(|_| decode_error("input index length is invalid"))?;
    Ok(TransparentSpendEntry {
        spent_outpoint,
        spending_transaction_id,
        input_index: u32::from_be_bytes(input_index_bytes),
        spending_block_height,
        spending_block_hash,
    })
}

fn decode_error(reason: impl Into<String>) -> DeriveStoreError {
    DeriveStoreError::Decode {
        column_family: DeriveStoreColumnFamily::ConsumerMetadata,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "tests assert on a known-present row; absence is a test-code bug, not a runtime condition."
    )]

    use std::collections::HashMap;
    use std::sync::Arc;

    use rust_rocksdb::WriteBatch;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeight, LockTime, PrivacyShape, TransactionComponentCounts,
        TransactionFactsArtifact, TransactionId, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, TransparentAddressScriptHash, TransparentInputFact,
        TransparentOutPoint, TransparentSpendFact,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::{
        TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY, TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
        TransparentOutpointSpendConsumer, TransparentOutpointSpendConsumerError,
    };
    use crate::consumer::block_commit_context::{
        BlockCommitContext, BlockCommitPayload, TransparentSpendFacts,
    };
    use crate::consumer::{BlockKeyedConsumer, DeriveConsumerCtx};
    use crate::store::{DeriveStore, DeriveStoreOptions};

    const SPENT_ADDRESS: TransparentAddressScriptHash =
        TransparentAddressScriptHash::from_bytes([7; 32]);
    const RECEIVE_HEIGHT: BlockHeight = BlockHeight::new(100);
    const SPEND_HEIGHT: BlockHeight = BlockHeight::new(105);

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

    fn received_outpoint() -> TransparentOutPoint {
        TransparentOutPoint::new(transaction_id(10), 0)
    }

    fn spend_fact() -> TransparentSpendFact {
        TransparentSpendFact::new(
            received_outpoint(),
            2,
            transaction_id(20),
            0,
            SPEND_HEIGHT,
            block_hash(5),
            5_000,
            SPENT_ADDRESS,
            RECEIVE_HEIGHT,
            block_hash(1),
        )
    }

    fn spend_block() -> BlockCommitContext {
        let location = TransactionLocation::new(transaction_id(20), SPEND_HEIGHT, block_hash(5), 0);
        let transaction = TransactionFactsArtifact::new(location, public_facts(20))
            .with_transparent_facts(
                vec![TransparentInputFact::new(2, received_outpoint())],
                Vec::new(),
            );
        let mut spends = HashMap::new();
        spends.insert(received_outpoint(), spend_fact());
        BlockCommitContext::new(
            BlockCommitPayload {
                height: SPEND_HEIGHT,
                block_hash: block_hash(5),
                previous_block_hash: block_hash(4),
                block_time_unix_seconds: 1_700_000_500,
                block_size_bytes: 0,
                transactions: vec![transaction],
            },
            TransparentSpendFacts::Static(Arc::new(spends)),
        )
    }

    fn coinbase_block() -> BlockCommitContext {
        let location =
            TransactionLocation::new(transaction_id(30), BlockHeight::new(106), block_hash(6), 0);
        let transaction = TransactionFactsArtifact::new(location, public_facts(30))
            .with_transparent_facts(
                vec![TransparentInputFact::new(
                    0,
                    TransparentOutPoint::COINBASE_SENTINEL,
                )],
                Vec::new(),
            );
        BlockCommitContext::new(
            BlockCommitPayload {
                height: BlockHeight::new(106),
                block_hash: block_hash(6),
                previous_block_hash: block_hash(5),
                block_time_unix_seconds: 1_700_001_000,
                block_size_bytes: 0,
                transactions: vec![transaction],
            },
            TransparentSpendFacts::Static(Arc::new(HashMap::new())),
        )
    }

    fn open_store()
    -> Result<(tempfile::TempDir, DeriveStore), Box<dyn std::error::Error + Send + Sync>> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        Ok((tempdir, store))
    }

    fn apply_block(
        store: &DeriveStore,
        consumer: &mut TransparentOutpointSpendConsumer,
        block: &BlockCommitContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        consumer.apply_block(block, &mut ctx)?;
        store.write_batch(&batch)?;
        Ok(())
    }

    fn revert_block(
        store: &DeriveStore,
        consumer: &mut TransparentOutpointSpendConsumer,
        height: BlockHeight,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        consumer.revert_block(height, &mut ctx)?;
        store.write_batch(&batch)?;
        Ok(())
    }

    #[test]
    fn spend_row_records_the_spender_identity()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentOutpointSpendConsumer::new();

        apply_block(&store, &mut consumer, &spend_block())?;

        let spends = TransparentOutpointSpendConsumer::read_spends_by_outpoints(
            &store,
            &[received_outpoint()],
        )?;
        let entry = spends
            .get(&received_outpoint())
            .expect("the spent outpoint must resolve to its spender");
        assert_eq!(entry.spent_outpoint, received_outpoint());
        assert_eq!(entry.spending_transaction_id, transaction_id(20));
        assert_eq!(entry.spending_block_hash, block_hash(5));
        assert_eq!(entry.spending_block_height, SPEND_HEIGHT);
        assert_eq!(entry.input_index, 2);
        assert_eq!(
            store.last_materialized_height_ascending(
                TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY
            )?,
            Some(SPEND_HEIGHT)
        );
        Ok(())
    }

    #[test]
    fn coinbase_input_records_no_spend_row() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentOutpointSpendConsumer::new();

        apply_block(&store, &mut consumer, &coinbase_block())?;

        let spends = TransparentOutpointSpendConsumer::read_spends_by_outpoints(
            &store,
            &[TransparentOutPoint::COINBASE_SENTINEL],
        )?;
        assert!(spends.is_empty());
        // The empty per-height index row still advances the durable height.
        assert_eq!(
            store.last_materialized_height_ascending(
                TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY
            )?,
            Some(BlockHeight::new(106))
        );
        Ok(())
    }

    #[test]
    fn offline_spend_facts_are_a_hard_error() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentOutpointSpendConsumer::new();
        let block = BlockCommitContext::new(
            BlockCommitPayload {
                height: SPEND_HEIGHT,
                block_hash: block_hash(5),
                previous_block_hash: block_hash(4),
                block_time_unix_seconds: 1_700_000_500,
                block_size_bytes: 0,
                transactions: Vec::new(),
            },
            TransparentSpendFacts::Offline,
        );

        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store: &store,
            batch: &mut batch,
        };
        let error = consumer
            .apply_block(&block, &mut ctx)
            .expect_err("offline spend facts must fail instead of advancing the durable height");
        assert!(matches!(
            error.downcast_ref::<TransparentOutpointSpendConsumerError>(),
            Some(TransparentOutpointSpendConsumerError::OfflineSpendFacts { height }) if *height == SPEND_HEIGHT.value()
        ));
        Ok(())
    }

    #[test]
    fn apply_then_rewind_matches_never_applied()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentOutpointSpendConsumer::new();

        apply_block(&store, &mut consumer, &spend_block())?;
        revert_block(&store, &mut consumer, SPEND_HEIGHT)?;

        let spends = TransparentOutpointSpendConsumer::read_spends_by_outpoints(
            &store,
            &[received_outpoint()],
        )?;
        assert!(spends.is_empty());
        assert_eq!(
            store.last_materialized_height_ascending(
                TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn rewind_of_top_block_keeps_lower_blocks()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (_tempdir, store) = open_store()?;
        let mut consumer = TransparentOutpointSpendConsumer::new();

        apply_block(&store, &mut consumer, &spend_block())?;
        apply_block(&store, &mut consumer, &coinbase_block())?;
        revert_block(&store, &mut consumer, BlockHeight::new(106))?;

        let spends = TransparentOutpointSpendConsumer::read_spends_by_outpoints(
            &store,
            &[received_outpoint()],
        )?;
        assert!(spends.contains_key(&received_outpoint()));
        assert_eq!(
            store.last_materialized_height_ascending(
                TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY
            )?,
            Some(SPEND_HEIGHT)
        );
        Ok(())
    }
}

/// Consumer-specific failure modes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransparentOutpointSpendConsumerError {
    /// Per-height delete index payload had a malformed byte length.
    #[error(
        "transparent_outpoint_spend_index entry for height {height} has {bytes} bytes, not a multiple of {OUTPOINT_KEY_LEN}"
    )]
    IndexLengthMismatch {
        /// Height whose persisted index was malformed.
        height: u32,
        /// Byte length actually persisted.
        bytes: usize,
    },

    /// The block context supplied no transparent spend facts, so the projection
    /// cannot record the block's spenders. Advancing its durable height here
    /// would unlock irreversible canonical deletion for a height it never
    /// covered.
    #[error(
        "transparent_outpoint_spend cannot consume block {height} with transparent spend facts offline"
    )]
    OfflineSpendFacts {
        /// Height whose spend facts were unavailable.
        height: u32,
    },
}
