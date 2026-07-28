//! `BlockSummary` materialized-view consumer.
//!
//! Materializes one [`BlockSummaryRecord`] per canonical block in the
//! consumer-owned `block_summary` column family per
//! [Explorer plane §Block view shape](../../../../../docs/architecture/explorer-plane.md#block-view-shape).
//!
//! Endpoint capability derivation uses this consumer's stable identity; the
//! consumer does not own or advertise protocol contracts.

use prost::Message as _;
use zinder_core::wire::{
    HEIGHT_KEY_LEN, encode_height_key_ascending, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex,
};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFacts, CanonicalTransactionFacts,
    TransactionFactsArtifact, TransactionPublicFacts, TransparentOutputFact,
};
use zinder_proto::v1::explorer::{BlockSummary, BlockSummaryRecord};
use zinder_store::ChainEvent;

use crate::MaterializedViewState;
use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, MaterializedViewBlockProjection,
    MaterializedViewConsumerCtx, MaterializedViewConsumerError, MaterializedViewConsumerName,
    MaterializedViewConsumerSchema, advance_verified_materialized_view_coverage,
};

/// Column-family name the `BlockSummary` materialized view owns.
///
/// [`BLOCK_SUMMARY_SCHEMA`] carries this in its
/// [`column_families`](crate::MaterializedViewConsumerSchema::column_families) so the SDK
/// registers the column family before the consumer issues its first write.
pub const BLOCK_SUMMARY_COLUMN_FAMILY: &str = "block_summary";

/// Stable consumer name persisted in the SDK cursor table.
pub const BLOCK_SUMMARY_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("block_summary");

/// On-disk schema declaration for the block-summary materialized-view consumer.
pub const BLOCK_SUMMARY_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        BLOCK_SUMMARY_CONSUMER_NAME,
        2,
        &[BLOCK_SUMMARY_COLUMN_FAMILY],
    );

/// Materializes one [`BlockSummaryRecord`] per canonical block.
#[derive(Default)]
pub struct BlockSummaryConsumer;

impl BlockSummaryConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the canonical materialized-view key for `height`.
    #[must_use]
    pub const fn key_for_height(height: BlockHeight) -> [u8; HEIGHT_KEY_LEN] {
        encode_height_key_ascending(height)
    }
}

impl BlockKeyedConsumer for BlockSummaryConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        BLOCK_SUMMARY_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let record = project_block_summary_record_from_commit_context(block);
        let cf = ctx
            .store
            .consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY)?;
        let mut payload = Vec::with_capacity(record.encoded_len());
        record
            .encode(&mut payload)
            .map_err(|error| BlockSummaryConsumerError::Encode(error.to_string()))?;
        ctx.batch
            .put_cf(&cf, Self::key_for_height(block.height), payload);
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY)?;
        ctx.batch.delete_cf(&cf, Self::key_for_height(height));
        Ok(())
    }

    fn stage_block_projection_state(
        &mut self,
        checkpoint: MaterializedViewBlockProjection<'_>,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let tip_height = checkpoint
            .tip_height
            .ok_or(BlockSummaryConsumerError::IncompleteMaterializedViewCheckpoint)?;
        let tip_hash = checkpoint
            .tip_hash
            .ok_or(BlockSummaryConsumerError::IncompleteMaterializedViewCheckpoint)?;
        let current = ctx.store.consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?;
        if let Some(state) = current
            && matches!(checkpoint.chain_event, ChainEvent::ChainCommitted { .. })
            && tip_height < state.tip_height
        {
            return Ok(());
        }
        let revision = current
            .map_or(Some(1), |state| state.revision.checked_add(1))
            .ok_or(BlockSummaryConsumerError::MaterializedViewRevisionOverflow)?;
        let first_staged_height = match checkpoint.chain_event {
            ChainEvent::ChainCommitted { committed }
            | ChainEvent::ChainReorged { committed, .. }
                if committed.block_range.start <= committed.block_range.end =>
            {
                Some(committed.block_range.start)
            }
            ChainEvent::ChainCommitted { .. } | ChainEvent::ChainReorged { .. } | _ => None,
        };
        let coverage = advance_verified_materialized_view_coverage(
            current.and_then(|state| state.coverage),
            checkpoint,
            tip_height,
            tip_hash,
            first_staged_height,
        );
        ctx.store.stage_consumer_state(
            ctx.batch,
            BLOCK_SUMMARY_CONSUMER_NAME,
            MaterializedViewState {
                chain_epoch_id: checkpoint.chain_epoch.id,
                tip_height,
                tip_hash,
                revision,
                coverage,
            },
        )?;
        Ok(())
    }
}

/// Projects immutable, block-local canonical facts into the persisted
/// [`BlockSummaryRecord`] read model.
///
/// The materialized view is pure: it does not parse retained raw bytes, resolve
/// previous outputs, read a store, or consult transaction-intrinsic value
/// balances. Transaction order in `facts` is preserved in the record.
///
#[must_use]
pub fn project_block_summary_record(facts: &CanonicalBlockFacts) -> BlockSummaryRecord {
    let transaction_ids = facts
        .transactions
        .iter()
        .map(|transaction| encode_rpc_transaction_id_hex(transaction.public_facts.transaction_id))
        .collect();
    let aggregates = aggregate_canonical_transaction_facts(&facts.transactions);
    build_block_summary_record(
        BlockSummaryInput {
            height: facts.block_header.height,
            hash: facts.block_header.block_hash,
            previous_hash: facts.block_header.parent_hash,
            time_unix_seconds: facts.block_header.block_time,
            size_bytes: facts.block_header.block_size_bytes,
        },
        transaction_ids,
        aggregates,
    )
}

fn project_block_summary_record_from_commit_context(
    block: &BlockCommitContext,
) -> BlockSummaryRecord {
    let transaction_ids = block
        .transactions
        .iter()
        .map(|transaction| encode_rpc_transaction_id_hex(transaction.location.transaction_id))
        .collect();
    let aggregates = aggregate_committed_transaction_facts(&block.transactions);
    build_block_summary_record(
        BlockSummaryInput {
            height: block.height,
            hash: block.block_hash,
            previous_hash: block.previous_block_hash,
            time_unix_seconds: block.block_time_unix_seconds,
            size_bytes: block.block_size_bytes,
        },
        transaction_ids,
        aggregates,
    )
}

fn build_block_summary_record(
    block: BlockSummaryInput,
    transaction_ids: Vec<String>,
    aggregates: BlockFactsAggregate,
) -> BlockSummaryRecord {
    let transaction_count = u32::try_from(transaction_ids.len()).unwrap_or(u32::MAX);
    let summary = BlockSummary {
        block_height: block.height.value(),
        block_hash: encode_rpc_block_hash_hex(block.hash),
        block_time_unix_seconds: block.time_unix_seconds,
        transaction_count,
        previous_block_hash: encode_rpc_block_hash_hex(block.previous_hash),
        total_size_bytes: block.size_bytes,
        fees_collected_zat: aggregates.zip317_conventional_fees_collected_zat,
        paid_fees_collected_zat: None,
        coinbase_reward_zat: aggregates.coinbase_reward_zat,
        sapling_output_count: aggregates.sapling_output_count,
        orchard_action_count: aggregates.orchard_action_count,
        ironwood_action_count: aggregates.ironwood_action_count,
        confirmations: 0,
        is_canonical: true,
    };
    BlockSummaryRecord {
        summary: Some(summary),
        transaction_ids,
        fee_transaction_count: aggregates.fee_transaction_count,
        min_zip317_conventional_fee_zat: aggregates.min_zip317_conventional_fee_zat.unwrap_or(0),
        max_zip317_conventional_fee_zat: aggregates.max_zip317_conventional_fee_zat.unwrap_or(0),
    }
}

#[derive(Clone, Copy, Debug)]
struct BlockSummaryInput {
    height: BlockHeight,
    hash: BlockHash,
    previous_hash: BlockHash,
    time_unix_seconds: i64,
    size_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockFactsAggregate {
    coinbase_reward_zat: u64,
    sapling_output_count: u32,
    orchard_action_count: u32,
    ironwood_action_count: u32,
    zip317_conventional_fees_collected_zat: u64,
    fee_transaction_count: u32,
    min_zip317_conventional_fee_zat: Option<u64>,
    max_zip317_conventional_fee_zat: Option<u64>,
}

fn aggregate_canonical_transaction_facts(
    transactions: &[CanonicalTransactionFacts],
) -> BlockFactsAggregate {
    let mut aggregate = BlockFactsAggregate::default();
    for transaction in transactions {
        accumulate_transaction_facts(
            &mut aggregate,
            &transaction.public_facts,
            &transaction.transparent_outputs,
        );
    }
    aggregate
}

fn aggregate_committed_transaction_facts(
    transactions: &[TransactionFactsArtifact],
) -> BlockFactsAggregate {
    let mut aggregate = BlockFactsAggregate::default();
    for transaction in transactions {
        accumulate_transaction_facts(
            &mut aggregate,
            &transaction.public_facts,
            &transaction.transparent_outputs,
        );
    }
    aggregate
}

fn accumulate_transaction_facts(
    aggregate: &mut BlockFactsAggregate,
    public_facts: &TransactionPublicFacts,
    transparent_outputs: &[TransparentOutputFact],
) {
    let counts = public_facts.counts;
    aggregate.sapling_output_count = aggregate
        .sapling_output_count
        .saturating_add(counts.sapling_output_count);
    aggregate.orchard_action_count = aggregate
        .orchard_action_count
        .saturating_add(counts.orchard_action_count);
    aggregate.ironwood_action_count = aggregate
        .ironwood_action_count
        .saturating_add(counts.ironwood_action_count);
    if public_facts.is_coinbase {
        for output in transparent_outputs {
            aggregate.coinbase_reward_zat = aggregate
                .coinbase_reward_zat
                .saturating_add(output.value_zat);
        }
        return;
    }
    aggregate.fee_transaction_count = aggregate.fee_transaction_count.saturating_add(1);
    let conventional_fee_zat = counts.zip317_conventional_fee_zat();
    aggregate.min_zip317_conventional_fee_zat = Some(
        aggregate
            .min_zip317_conventional_fee_zat
            .map_or(conventional_fee_zat, |prior| {
                prior.min(conventional_fee_zat)
            }),
    );
    aggregate.max_zip317_conventional_fee_zat = Some(
        aggregate
            .max_zip317_conventional_fee_zat
            .map_or(conventional_fee_zat, |prior| {
                prior.max(conventional_fee_zat)
            }),
    );
    aggregate.zip317_conventional_fees_collected_zat = aggregate
        .zip317_conventional_fees_collected_zat
        .saturating_add(conventional_fee_zat);
}

/// Consumer-specific failure modes [`BlockSummaryConsumer`] can surface.
///
/// Infrastructure failures (store I/O, materialized-view writes) reach
/// the SDK through the boxed [`MaterializedViewConsumerError`] without going through
/// this enum; the variants here are reserved for the shape-specific things
/// the consumer itself can fail at.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockSummaryConsumerError {
    /// Storage encoding of the materialized record failed.
    #[error("BlockSummaryRecord prost encode failed: {0}")]
    Encode(String),
    /// A dispatch omitted one or more block contexts required by the consumer.
    #[error("block-summary materialized-view checkpoint is missing its indexed tip")]
    IncompleteMaterializedViewCheckpoint,
    /// Materialized-view revision exhausted its integer domain.
    #[error("block-summary materialized-view revision overflowed")]
    MaterializedViewRevisionOverflow,
}

/// Decodes a stored [`BlockSummaryRecord`] payload, surfacing a typed error
/// when the bytes do not round-trip cleanly.
///
/// Used by the explorer-query handlers to project the on-disk record into
/// the public wire shapes.
pub fn decode_stored_record(payload: &[u8]) -> Result<BlockSummaryRecord, prost::DecodeError> {
    BlockSummaryRecord::decode(payload)
}

#[cfg(test)]
mod tests {
    use eyre::Result;
    use rust_rocksdb::WriteBatch;
    use tempfile::tempdir;
    use zinder_core::wire::encode_rpc_transaction_id_hex;
    use zinder_core::{
        ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange,
        CanonicalBlockFacts, CanonicalBlockFactsDigestVersion, CanonicalBlockReplayFormatVersion,
        CanonicalTransactionFacts, ChainEpoch, ChainEpochId, ChainTipMetadata, LockTime, Network,
        PrivacyShape, TransactionComponentCounts, TransactionFactsArtifact, TransactionId,
        TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
        TransactionVersion, TransparentAddressScriptHash, TransparentOutputFact,
        UnixTimestampMillis, decode_canonical_block_replay, encode_canonical_block_replay,
    };
    use zinder_store::{ChainEpochCommitted, ChainEvent};

    use super::{
        BLOCK_SUMMARY_CONSUMER_NAME, BlockSummaryConsumer, project_block_summary_record,
        project_block_summary_record_from_commit_context,
    };
    use crate::consumer::{
        BlockCommitContext, BlockCommitInput, BlockKeyedConsumer, MaterializedViewBlockProjection,
        MaterializedViewConsumerCtx, TransparentSpendFacts,
    };
    use crate::store::{
        MaterializedViewCoverage, MaterializedViewState, MaterializedViewStore,
        MaterializedViewStoreOptions,
    };

    #[test]
    fn initial_committed_checkpoint_records_exact_contiguous_coverage() -> Result<()> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            zinder_core::Network::ZcashRegtest,
            MaterializedViewStoreOptions::default(),
        )?;
        let tip_height = BlockHeight::new(105);
        let tip_hash = BlockHash::from_bytes([0x55; 32]);
        let epoch = chain_epoch(1, tip_height, tip_hash);
        let event = committed_chunk_event(epoch, 100, 105);

        stage_and_commit_checkpoint(&store, epoch, &event, tip_height, tip_hash)?;

        let state = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("block-summary state missing"))?;
        let coverage = state
            .coverage
            .ok_or_else(|| eyre::eyre!("block-summary coverage missing"))?;
        assert_eq!(coverage.complete_from_height, BlockHeight::new(100));
        assert_eq!(coverage.complete_through_height, tip_height);
        assert_eq!(coverage.complete_through_hash, tip_hash);
        assert_eq!(state.revision, 1);
        Ok(())
    }

    #[test]
    fn empty_committed_checkpoint_does_not_claim_contiguous_coverage() -> Result<()> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            zinder_core::Network::ZcashRegtest,
            MaterializedViewStoreOptions::default(),
        )?;
        let tip_height = BlockHeight::new(105);
        let tip_hash = BlockHash::from_bytes([0x55; 32]);
        let epoch = chain_epoch(1, tip_height, tip_hash);
        let event = committed_chunk_event(epoch, 106, 105);

        stage_and_commit_checkpoint(&store, epoch, &event, tip_height, tip_hash)?;

        let state = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("block-summary state missing"))?;
        assert_eq!(state.coverage, None);
        assert_eq!(state.revision, 1);
        Ok(())
    }

    #[test]
    fn re_applied_lower_committed_chunk_preserves_advanced_coverage() -> Result<()> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            zinder_core::Network::ZcashRegtest,
            MaterializedViewStoreOptions::default(),
        )?;
        let tip_height = BlockHeight::new(512);
        let tip_hash = BlockHash::from_bytes([0x55; 32]);
        let existing = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(1),
            tip_height,
            tip_hash,
            revision: 5,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(100),
                complete_through_height: tip_height,
                complete_through_hash: tip_hash,
            }),
        };
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, existing)?;
        let epoch = chain_epoch(1, tip_height, tip_hash);
        let re_applied_chunk = committed_chunk_event(epoch, 100, 256);

        stage_and_commit_checkpoint(
            &store,
            epoch,
            &re_applied_chunk,
            BlockHeight::new(256),
            BlockHash::from_bytes([0x44; 32]),
        )?;

        assert_eq!(
            store.consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?,
            Some(existing),
        );
        Ok(())
    }

    #[test]
    fn canonical_facts_projection_matches_commit_context_consumer() {
        let facts = representative_block_facts();

        let canonical_record = project_block_summary_record(&facts);
        let commit_context_record =
            project_block_summary_record_from_commit_context(&block_commit_context_from(&facts));

        assert_eq!(canonical_record, commit_context_record);
        assert_eq!(
            canonical_record.transaction_ids,
            [0x11, 0x22, 0x33]
                .map(|seed| encode_rpc_transaction_id_hex(TransactionId::from_bytes([seed; 32])))
        );
        assert_eq!(canonical_record.fee_transaction_count, 2);
        assert_eq!(canonical_record.min_zip317_conventional_fee_zat, 10_000);
        assert_eq!(canonical_record.max_zip317_conventional_fee_zat, 50_000);
        assert_eq!(
            canonical_record.summary.as_ref().map(|summary| (
                summary.transaction_count,
                summary.fees_collected_zat,
                summary.coinbase_reward_zat,
                summary.sapling_output_count,
                summary.orchard_action_count,
                summary.ironwood_action_count,
            )),
            Some((3, 60_000, 625_000_000, 6, 2, 1))
        );
    }

    #[test]
    fn decoded_replay_projects_without_store_hydration() -> Result<(), Box<dyn std::error::Error>> {
        let facts = representative_block_facts();
        let replay_envelope = encode_canonical_block_replay(
            &facts,
            CanonicalBlockReplayFormatVersion::CURRENT,
            CanonicalBlockFactsDigestVersion::CURRENT,
        );
        let replay = decode_canonical_block_replay(replay_envelope.as_bytes())?;

        assert_eq!(
            project_block_summary_record(replay.facts()),
            project_block_summary_record(&facts)
        );
        Ok(())
    }

    #[test]
    fn intrinsic_balances_do_not_change_projection() {
        let baseline_facts = representative_block_facts();
        let mut changed_facts = baseline_facts.clone();
        for (index, transaction) in changed_facts.transactions.iter_mut().enumerate() {
            let balance_seed = i64::try_from(index).unwrap_or(i64::MAX);
            transaction.intrinsic_value_balances = TransactionIntrinsicValueBalances::new(
                balance_seed,
                balance_seed.saturating_add(1),
                balance_seed.saturating_add(2),
                balance_seed.saturating_add(3),
            );
        }

        assert_eq!(
            project_block_summary_record(&baseline_facts),
            project_block_summary_record(&changed_facts)
        );
    }

    #[test]
    fn projection_preserves_serialized_block_size() {
        let facts = representative_block_facts();
        let transaction_size_sum = facts
            .transactions
            .iter()
            .map(|transaction| u64::from(transaction.public_facts.size_bytes))
            .sum::<u64>();
        let record = project_block_summary_record(&facts);

        assert_eq!(
            record
                .summary
                .as_ref()
                .map(|summary| summary.total_size_bytes),
            Some(facts.block_header.block_size_bytes)
        );
        assert_ne!(transaction_size_sum, facts.block_header.block_size_bytes);
    }

    fn chain_epoch(id: u64, tip_height: BlockHeight, tip_hash: BlockHash) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network: Network::ZcashRegtest,
            visible_tip_height: tip_height,
            visible_tip_hash: tip_hash,
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

    fn stage_and_commit_checkpoint(
        store: &MaterializedViewStore,
        chain_epoch: ChainEpoch,
        chain_event: &ChainEvent,
        tip_height: BlockHeight,
        tip_hash: BlockHash,
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        BlockSummaryConsumer::new()
            .stage_block_projection_state(
                MaterializedViewBlockProjection {
                    chain_epoch,
                    chain_event,
                    tip_height: Some(tip_height),
                    tip_hash: Some(tip_hash),
                },
                &mut ctx,
            )
            .map_err(|error| eyre::eyre!("{error}"))?;
        store.write_batch(&batch)?;
        Ok(())
    }

    fn representative_block_facts() -> CanonicalBlockFacts {
        CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                BlockHeight::new(900_001),
                BlockHash::from_bytes([0x41; 32]),
                BlockHash::from_bytes([0x40; 32]),
                [0x42; 32],
                [0x43; 32],
                1_750_000_000,
                0x1f07_ffff,
                [0x44; 32],
                4,
                2_048,
            ),
            serialized_bytes_digest: zinder_core::SerializedBytesDigest::from_serialized_bytes(
                b"representative block",
            ),
            transactions: vec![
                transaction_facts(
                    0x11,
                    true,
                    TransactionComponentCounts {
                        transparent_input_count: 1,
                        transparent_output_count: 2,
                        sapling_spend_count: 0,
                        sapling_output_count: 2,
                        orchard_action_count: 0,
                        ironwood_action_count: 0,
                        sprout_joinsplit_count: 0,
                    },
                    &[500_000_000, 125_000_000],
                ),
                transaction_facts(
                    0x22,
                    false,
                    TransactionComponentCounts {
                        transparent_input_count: 1,
                        transparent_output_count: 1,
                        sapling_spend_count: 0,
                        sapling_output_count: 0,
                        orchard_action_count: 0,
                        ironwood_action_count: 0,
                        sprout_joinsplit_count: 0,
                    },
                    &[75_000],
                ),
                transaction_facts(
                    0x33,
                    false,
                    TransactionComponentCounts {
                        transparent_input_count: 3,
                        transparent_output_count: 1,
                        sapling_spend_count: 1,
                        sapling_output_count: 4,
                        orchard_action_count: 2,
                        ironwood_action_count: 1,
                        sprout_joinsplit_count: 0,
                    },
                    &[50_000],
                ),
            ],
        }
    }

    fn transaction_facts(
        seed: u8,
        is_coinbase: bool,
        counts: TransactionComponentCounts,
        transparent_output_values_zat: &[u64],
    ) -> CanonicalTransactionFacts {
        CanonicalTransactionFacts {
            public_facts: TransactionPublicFacts {
                transaction_id: TransactionId::from_bytes([seed; 32]),
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::V4,
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: u32::from(seed).saturating_mul(10),
                counts,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                privacy_shape: if is_coinbase {
                    PrivacyShape::ShieldedCoinbase
                } else {
                    PrivacyShape::Mixed
                },
                is_coinbase,
                unsupported_sections: Vec::new(),
            },
            serialized_bytes_digest: zinder_core::SerializedBytesDigest::from_serialized_bytes(&[
                seed,
            ]),
            intrinsic_value_balances: TransactionIntrinsicValueBalances::new(
                i64::from(seed),
                -i64::from(seed),
                i64::from(seed).saturating_mul(2),
                -i64::from(seed).saturating_mul(2),
            ),
            transparent_inputs: Vec::new(),
            transparent_outputs: transparent_output_values_zat
                .iter()
                .enumerate()
                .map(|(output_index, value_zat)| {
                    TransparentOutputFact::new(
                        u32::try_from(output_index).unwrap_or(u32::MAX),
                        *value_zat,
                        [0x51],
                        TransparentAddressScriptHash::from_bytes([seed; 32]),
                    )
                })
                .collect(),
        }
    }

    fn block_commit_context_from(facts: &CanonicalBlockFacts) -> BlockCommitContext {
        let transactions = facts
            .transactions
            .iter()
            .enumerate()
            .map(|(transaction_index, transaction)| {
                TransactionFactsArtifact::new(
                    TransactionLocation::new(
                        transaction.public_facts.transaction_id,
                        facts.block_header.height,
                        facts.block_header.block_hash,
                        u32::try_from(transaction_index).unwrap_or(u32::MAX),
                    ),
                    transaction.public_facts.clone(),
                )
                .with_transparent_facts(
                    transaction.transparent_inputs.clone(),
                    transaction.transparent_outputs.clone(),
                )
            })
            .collect();
        BlockCommitContext::new(
            BlockCommitInput {
                height: facts.block_header.height,
                block_hash: facts.block_header.block_hash,
                previous_block_hash: facts.block_header.parent_hash,
                block_time_unix_seconds: facts.block_header.block_time,
                block_size_bytes: facts.block_header.block_size_bytes,
                transactions,
                final_note_commitment_roots: None,
            },
            TransparentSpendFacts::Offline,
        )
    }
}
