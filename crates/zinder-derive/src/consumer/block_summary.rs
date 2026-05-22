//! `BlockSummary` derive consumer.
//!
//! Materializes one [`BlockSummaryRecord`] per canonical block in the
//! consumer-owned `block_summary` column family per
//! [Explorer plane §Block view shape](../../../../../docs/architecture/explorer-plane.md#block-view-shape).
//!
//! Capability strings advertised when the view is wired and the cursor has
//! caught up: [`EXPLORER_BLOCK_SUMMARY_V1`] and [`EXPLORER_BLOCK_DETAIL_V1`].

use prost::Message as _;
use zebra_chain::block::Block as ZebraBlock;
use zinder_core::BlockHeight;
use zinder_core::wire::{
    HEIGHT_KEY_LEN, encode_height_key_ascending, encode_internal_transaction_id,
};
use zinder_proto::capabilities::{EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1};
use zinder_proto::v1::explorer::{BlockSummary, BlockSummaryRecord};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName,
};

/// Column-family name the `BlockSummary` derive view owns.
///
/// Pass this in [`crate::store::DeriveStoreOptions::consumer_column_families`]
/// at store open time so the SDK registers the column family before the
/// consumer issues its first write.
pub const BLOCK_SUMMARY_COLUMN_FAMILY: &str = "block_summary";

/// Stable consumer name persisted in the SDK cursor table.
pub const BLOCK_SUMMARY_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("block_summary");

/// Capability strings the consumer's read surface lights up once caught up.
pub const BLOCK_SUMMARY_CAPABILITIES: &[&str] =
    &[EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_BLOCK_DETAIL_V1];

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
    let block_time_unix_seconds = block.block.header.time.timestamp();
    let transaction_count = u32::try_from(block.block.transactions.len()).unwrap_or(u32::MAX);
    let total_size_bytes = u64::try_from(block.raw_block_size_bytes).unwrap_or(u64::MAX);
    let aggregates = aggregate_block_facts(&block.block);
    let transaction_ids = block
        .block
        .transactions
        .iter()
        .map(|transaction| {
            encode_internal_transaction_id(zinder_core::TransactionId::from_bytes(
                transaction.hash().0,
            ))
            .to_vec()
        })
        .collect();
    let summary = BlockSummary {
        block_height: block.height.value(),
        block_hash: block.block_hash.clone(),
        block_time_unix_seconds,
        transaction_count,
        previous_block_hash: block.previous_block_hash.clone(),
        total_size_bytes,
        fees_collected_zat: aggregates.zip317_conventional_fees_collected_zat,
        paid_fees_collected_zat: None,
        coinbase_reward_zat: aggregates.coinbase_reward_zat,
        sapling_output_count: aggregates.sapling_output_count,
        orchard_action_count: aggregates.orchard_action_count,
        confirmations: 0,
        is_canonical: true,
    };
    BlockSummaryRecord {
        summary: Some(summary),
        transaction_ids,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockFactsAggregate {
    coinbase_reward_zat: u64,
    sapling_output_count: u32,
    orchard_action_count: u32,
    zip317_conventional_fees_collected_zat: u64,
}

fn aggregate_block_facts(block: &ZebraBlock) -> BlockFactsAggregate {
    let mut aggregate = BlockFactsAggregate::default();
    for (position, transaction) in block.transactions.iter().enumerate() {
        let is_coinbase = position == 0;
        let counts = zinder_source::transaction_component_counts(transaction);
        aggregate.sapling_output_count = aggregate
            .sapling_output_count
            .saturating_add(counts.sapling_output_count);
        aggregate.orchard_action_count = aggregate
            .orchard_action_count
            .saturating_add(counts.orchard_action_count);
        if is_coinbase {
            for output in transaction.outputs() {
                let amount = i64::from(output.value);
                let zat = u64::try_from(amount).unwrap_or(0);
                aggregate.coinbase_reward_zat = aggregate.coinbase_reward_zat.saturating_add(zat);
            }
            continue;
        }
        aggregate.zip317_conventional_fees_collected_zat = aggregate
            .zip317_conventional_fees_collected_zat
            .saturating_add(counts.zip317_conventional_fee_zat());
    }
    aggregate
}

/// Consumer-specific failure modes [`BlockSummaryConsumer`] can surface.
///
/// Infrastructure failures (store I/O, `WalletQuery.FullBlock` errors) reach
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
