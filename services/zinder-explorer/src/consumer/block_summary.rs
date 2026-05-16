//! `BlockSummary` derive consumer.
//!
//! Materializes one [`zinder_proto::v1::explorer::BlockSummaryRecord`] per
//! canonical block in the consumer-owned `block_summary` column family per
//! [Explorer plane §Block view shape](../../../../../docs/architecture/explorer-plane.md#block-view-shape).
//! Reads the full block bytes from `WalletQuery.FullBlock`, parses via
//! `zebra-chain` to extract canonical transaction order including the
//! coinbase and transparent-only transactions the compact-block format
//! omits, and writes the record keyed by big-endian block height. Reorg
//! events delete the reverted height range and re-fetch the replacement
//! range in the same atomic batch as the cursor advance.
//!
//! The consumer is the first real `DeriveConsumer` implementation; the SDK
//! contract documented in [`crate::consumer`] is the only chain-events
//! integration point.
//!
//! Capability strings advertised when the view is wired and the cursor has
//! caught up: [`EXPLORER_BLOCK_SUMMARY_V1`] and [`EXPLORER_BLOCK_DETAIL_V1`].

use async_trait::async_trait;
use prost::Message as _;
use tonic::Request;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto as _;
use zinder_core::wire::encode_internal_transaction_id;
use zinder_proto::capabilities::{EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1};
use zinder_proto::v1::explorer::{BlockSummary, BlockSummaryRecord};
use zinder_proto::v1::wallet::{self, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use crate::consumer::{
    ChainCommittedEvent, ChainReorgedEvent, DeriveConsumer, DeriveConsumerCtx, DeriveConsumerError,
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

/// Materializes one [`BlockSummaryRecord`] per canonical block from the
/// wallet plane's `FullBlock` reads.
pub struct BlockSummaryConsumer {
    wallet_client: WalletQueryClient<AuthenticatedChannel>,
}

impl BlockSummaryConsumer {
    /// Builds a consumer that drives writes against `wallet_client`.
    #[must_use]
    pub fn new(wallet_client: WalletQueryClient<AuthenticatedChannel>) -> Self {
        Self { wallet_client }
    }

    /// Returns the canonical big-endian storage key for `height`.
    #[must_use]
    pub const fn key_for_height(height: u32) -> [u8; 4] {
        height.to_be_bytes()
    }

    async fn write_height(
        &mut self,
        height: u32,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let record = match self.fetch_record(height).await {
            Ok(record) => record,
            Err(BlockSummaryConsumerError::WalletFullBlockNotFound { height }) => {
                tracing::debug!(
                    target: "zinder::explorer",
                    event = "block_summary_skip_missing_height",
                    height,
                    "BlockSummary skip: wallet has no FullBlock artifact at this height; \
                     typical for the checkpoint height during initial bootstrap"
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let cf = ctx
            .store
            .consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY)?;
        let mut payload = Vec::with_capacity(record.encoded_len());
        record
            .encode(&mut payload)
            .map_err(|error| BlockSummaryConsumerError::Encode(error.to_string()))?;
        ctx.batch.put_cf(&cf, Self::key_for_height(height), payload);
        Ok(())
    }

    fn delete_height(
        height: u32,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY)?;
        ctx.batch.delete_cf(&cf, Self::key_for_height(height));
        Ok(())
    }

    async fn fetch_record(
        &mut self,
        height: u32,
    ) -> Result<BlockSummaryRecord, BlockSummaryConsumerError> {
        let response = self
            .wallet_client
            .full_block(Request::new(wallet::FullBlockRequest {
                block_height: height,
                at_epoch: None,
            }))
            .await
            .map_err(|status| {
                if status.code() == tonic::Code::NotFound {
                    BlockSummaryConsumerError::WalletFullBlockNotFound { height }
                } else {
                    BlockSummaryConsumerError::WalletFullBlockUnavailable {
                        height,
                        status: status.to_string(),
                    }
                }
            })?
            .into_inner();
        let block =
            response
                .block
                .ok_or(BlockSummaryConsumerError::WalletFullBlockMissingField {
                    height,
                    field: "block",
                })?;
        decode_block_summary_record(height, block)
    }
}

fn decode_block_summary_record(
    height: u32,
    block: wallet::FullBlock,
) -> Result<BlockSummaryRecord, BlockSummaryConsumerError> {
    let parsed: ZebraBlock = block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| BlockSummaryConsumerError::BlockParseFailed {
            height,
            reason: error.to_string(),
        })?;
    let block_time_unix_seconds = parsed.header.time.timestamp();
    let transaction_count = u32::try_from(parsed.transactions.len()).unwrap_or(u32::MAX);
    let transaction_ids = parsed
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
        block_height: height,
        block_hash: block.block_hash,
        block_time_unix_seconds,
        transaction_count,
        previous_block_hash: block.parent_block_hash,
    };
    Ok(BlockSummaryRecord {
        summary: Some(summary),
        transaction_ids,
    })
}

#[async_trait]
impl DeriveConsumer for BlockSummaryConsumer {
    fn name(&self) -> DeriveConsumerName {
        BLOCK_SUMMARY_CONSUMER_NAME
    }

    async fn apply_chain_committed(
        &mut self,
        event: &ChainCommittedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let start = event.start_height.value();
        let end = event.end_height.value();
        for height in start..=end {
            self.write_height(height, ctx).await?;
        }
        Ok(())
    }

    async fn apply_chain_reorged(
        &mut self,
        event: &ChainReorgedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let reverted_start = event.reverted.start_height.value();
        let reverted_end = event.reverted.end_height.value();
        for height in reverted_start..=reverted_end {
            Self::delete_height(height, ctx)?;
        }
        let replacement_start = event.replacement.start_height.value();
        let replacement_end = event.replacement.end_height.value();
        for height in replacement_start..=replacement_end {
            self.write_height(height, ctx).await?;
        }
        Ok(())
    }
}

/// Operator-facing failure modes the consumer can surface to the SDK.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockSummaryConsumerError {
    /// Upstream `WalletQuery.FullBlock` returned a non-OK status.
    #[error("wallet FullBlock for height {height} returned: {status}")]
    WalletFullBlockUnavailable {
        /// Height the consumer was trying to materialize.
        height: u32,
        /// Stringified status returned by the wallet client.
        status: String,
    },
    /// Upstream `WalletQuery.FullBlock` returned `NOT_FOUND` for the
    /// requested height.
    ///
    /// Surfaces during initial bootstrap when the `ChainCommitted` event
    /// covers the checkpoint height even though no `BlockArtifact` was
    /// persisted at that height. The consumer skips the write and continues.
    #[error("wallet FullBlock for height {height} returned NOT_FOUND")]
    WalletFullBlockNotFound {
        /// Height the consumer was trying to materialize.
        height: u32,
    },
    /// Upstream `WalletQuery.FullBlock` returned a response with missing
    /// payload fields.
    #[error("wallet FullBlock for height {height} response missing field: {field}")]
    WalletFullBlockMissingField {
        /// Height the consumer was trying to materialize.
        height: u32,
        /// Proto field name that was unexpectedly absent.
        field: &'static str,
    },
    /// `raw_block_bytes` did not decode as a Zcash block.
    #[error("FullBlock raw_block_bytes for height {height} did not parse: {reason}")]
    BlockParseFailed {
        /// Height whose payload failed to decode.
        height: u32,
        /// Reason returned by `zebra-chain`'s deserializer.
        reason: String,
    },
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
