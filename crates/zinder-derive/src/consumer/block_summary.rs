//! `BlockSummary` derive consumer.
//!
//! Materializes one [`BlockSummaryRecord`] per canonical block in the
//! consumer-owned `block_summary` column family per
//! [Explorer plane §Block view shape](../../../../../docs/architecture/explorer-plane.md#block-view-shape).
//!
//! Capability strings advertised when the view is wired and the cursor has
//! caught up: [`EXPLORER_BLOCK_SUMMARY_V1`] and [`EXPLORER_BLOCK_DETAIL_V1`].

use prost::Message as _;
use zinder_core::wire::{
    HEIGHT_KEY_LEN, encode_height_key_ascending, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHeight, TransactionFactsArtifact};
use zinder_proto::capabilities::{EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1};
use zinder_proto::v1::explorer::{BlockSummary, BlockSummaryRecord};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName, DeriveConsumerSchema,
};

/// Column-family name the `BlockSummary` derive view owns.
///
/// [`BLOCK_SUMMARY_SCHEMA`] carries this in its
/// [`column_families`](crate::DeriveConsumerSchema::column_families) so the SDK
/// registers the column family before the consumer issues its first write.
pub const BLOCK_SUMMARY_COLUMN_FAMILY: &str = "block_summary";

/// Stable consumer name persisted in the SDK cursor table.
pub const BLOCK_SUMMARY_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("block_summary");

/// Capability strings the consumer's read surface lights up once caught up.
pub const BLOCK_SUMMARY_CAPABILITIES: &[&str] =
    &[EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_BLOCK_DETAIL_V1];

/// On-disk schema declaration for the block-summary derive consumer.
pub const BLOCK_SUMMARY_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    BLOCK_SUMMARY_CONSUMER_NAME,
    1,
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

    /// Returns the canonical derive-store key for `height`.
    #[must_use]
    pub const fn key_for_height(height: BlockHeight) -> [u8; HEIGHT_KEY_LEN] {
        encode_height_key_ascending(height)
    }
}

impl BlockKeyedConsumer for BlockSummaryConsumer {
    fn name(&self) -> DeriveConsumerName {
        BLOCK_SUMMARY_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let record = build_block_summary_record(block);
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
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY)?;
        ctx.batch.delete_cf(&cf, Self::key_for_height(height));
        Ok(())
    }
}

fn build_block_summary_record(block: &BlockCommitContext) -> BlockSummaryRecord {
    let block_time_unix_seconds = block.block_time_unix_seconds;
    let transaction_count = u32::try_from(block.transactions.len()).unwrap_or(u32::MAX);
    let total_size_bytes = block.block_size_bytes;
    let aggregates = aggregate_block_facts(&block.transactions);
    let transaction_ids = block
        .transactions
        .iter()
        .map(|transaction| encode_rpc_transaction_id_hex(transaction.location.transaction_id))
        .collect();
    let summary = BlockSummary {
        block_height: block.height.value(),
        block_hash: encode_rpc_block_hash_hex(block.block_hash),
        block_time_unix_seconds,
        transaction_count,
        previous_block_hash: encode_rpc_block_hash_hex(block.previous_block_hash),
        total_size_bytes,
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

fn aggregate_block_facts(transactions: &[TransactionFactsArtifact]) -> BlockFactsAggregate {
    let mut aggregate = BlockFactsAggregate::default();
    for transaction in transactions {
        let facts = &transaction.public_facts;
        let counts = facts.counts;
        aggregate.sapling_output_count = aggregate
            .sapling_output_count
            .saturating_add(counts.sapling_output_count);
        aggregate.orchard_action_count = aggregate
            .orchard_action_count
            .saturating_add(counts.orchard_action_count);
        aggregate.ironwood_action_count = aggregate
            .ironwood_action_count
            .saturating_add(counts.ironwood_action_count);
        if facts.is_coinbase {
            for output in &transaction.transparent_outputs {
                aggregate.coinbase_reward_zat = aggregate
                    .coinbase_reward_zat
                    .saturating_add(output.value_zat);
            }
            continue;
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
    aggregate
}

/// Consumer-specific failure modes [`BlockSummaryConsumer`] can surface.
///
/// Infrastructure failures (store I/O, derive-store writes) reach
/// the SDK through the boxed [`DeriveConsumerError`] without going through
/// this enum; the variants here are reserved for the shape-specific things
/// the consumer itself can fail at.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockSummaryConsumerError {
    /// Storage encoding of the materialized record failed.
    #[error("BlockSummaryRecord prost encode failed: {0}")]
    Encode(String),
}

/// Decodes a stored [`BlockSummaryRecord`] payload, surfacing a typed error
/// when the bytes do not round-trip cleanly.
///
/// Used by the explorer-query handlers to project the on-disk record into
/// the public wire shapes.
pub fn decode_stored_record(payload: &[u8]) -> Result<BlockSummaryRecord, prost::DecodeError> {
    BlockSummaryRecord::decode(payload)
}
