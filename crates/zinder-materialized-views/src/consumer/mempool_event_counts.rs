//! `MempoolEventCounts` materialized-view consumer.
//!
//! Counts the `Added`, `Mined`, and `Invalidated` mempool events the upstream source
//! emits, bucketed by wall-clock second, into the
//! consumer-owned `mempool_event_counts` column family. The
//! `ExplorerQuery.MempoolEventCounts` handler reads a rolling window from
//! the column family at request time and aggregates per the requested
//! window length.
//!
//! The column family survives consumer restarts and is shared across
//! horizontally scaled consumer replicas. Retention is bounded at write time to a 24-hour
//! sliding window: rows older than 24 h are pruned in the same batch as
//! the incoming write so storage stays `O(24 * 3600 * 12) = 1.0 MB` at most.

use zinder_core::wire::{UNIX_SECONDS_KEY_LEN, encode_unix_seconds};

use crate::consumer::{
    MaterializedViewConsumerCtx, MaterializedViewConsumerError, MaterializedViewConsumerName,
    MaterializedViewConsumerSchema, MaterializedViewMempoolConsumer, MempoolConsumerEvent,
    MempoolConsumerEventVariant,
};

/// Column-family name the consumer owns.
pub const MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY: &str = "mempool_event_counts";

/// Stable consumer name persisted in the SDK cursor table.
pub const MEMPOOL_EVENT_COUNTS_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("mempool_event_counts");

/// On-disk schema declaration for the mempool-event-counts materialized-view consumer.
pub const MEMPOOL_EVENT_COUNTS_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
        2,
        &[MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY],
    );

/// Number of bytes the consumer stores per per-second row.
///
/// Layout: `added | mined | invalidated`, each `u32`
/// big-endian. Fixed-shape so the read path decodes without a proto codec
/// on the hot read.
const MEMPOOL_EVENT_COUNTS_ROW_LEN: usize = 12;

/// Retention window for per-second rows, in seconds.
///
/// Rows older than this are pruned when the consumer writes a new row.
pub const MEMPOOL_EVENT_COUNTS_RETENTION_SECONDS: u64 = 24 * 3600;

/// Counts the three mempool event arms into 12-byte per-second rows.
pub struct MempoolEventCountsConsumer;

impl MempoolEventCountsConsumer {
    /// Builds a fresh consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the storage key for the per-second bucket `unix_seconds`.
    #[must_use]
    pub const fn key_for_second(unix_seconds: u64) -> [u8; UNIX_SECONDS_KEY_LEN] {
        encode_unix_seconds(unix_seconds)
    }

    /// Decodes a stored row into `(added, mined, invalidated)`.
    #[must_use]
    pub fn decode_row(bytes: &[u8]) -> Option<(u32, u32, u32)> {
        if bytes.len() != MEMPOOL_EVENT_COUNTS_ROW_LEN {
            return None;
        }
        let added = u32::from_be_bytes(bytes[0..4].try_into().ok()?);
        let mined = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
        let invalidated = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
        Some((added, mined, invalidated))
    }

    fn encode_row(row: (u32, u32, u32)) -> [u8; MEMPOOL_EVENT_COUNTS_ROW_LEN] {
        let mut bytes = [0u8; MEMPOOL_EVENT_COUNTS_ROW_LEN];
        bytes[0..4].copy_from_slice(&row.0.to_be_bytes());
        bytes[4..8].copy_from_slice(&row.1.to_be_bytes());
        bytes[8..12].copy_from_slice(&row.2.to_be_bytes());
        bytes
    }
}

impl Default for MempoolEventCountsConsumer {
    fn default() -> Self {
        Self::new()
    }
}

impl MaterializedViewMempoolConsumer for MempoolEventCountsConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        MEMPOOL_EVENT_COUNTS_CONSUMER_NAME
    }

    fn apply_mempool_event(
        &mut self,
        event: &MempoolConsumerEvent<'_>,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let bucket_seconds = event.source_observed_unix_millis / 1_000;
        let key = Self::key_for_second(bucket_seconds);
        let existing = ctx
            .store
            .get_consumer(MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, &key)?
            .as_deref()
            .and_then(Self::decode_row)
            .unwrap_or((0, 0, 0));
        let updated = match event.variant {
            MempoolConsumerEventVariant::Added { .. } => {
                (existing.0.saturating_add(1), existing.1, existing.2)
            }
            MempoolConsumerEventVariant::Mined { .. } => {
                (existing.0, existing.1.saturating_add(1), existing.2)
            }
            MempoolConsumerEventVariant::Invalidated { .. } => {
                (existing.0, existing.1, existing.2.saturating_add(1))
            }
        };
        let cf = ctx
            .store
            .consumer_column_family(MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY)?;
        ctx.batch.put_cf(&cf, key, Self::encode_row(updated));
        if let Some(prune_threshold) =
            bucket_seconds.checked_sub(MEMPOOL_EVENT_COUNTS_RETENTION_SECONDS)
        {
            let prune_key = Self::key_for_second(prune_threshold);
            ctx.batch
                .delete_range_cf(&cf, encode_unix_seconds(0).as_slice(), prune_key.as_slice());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_round_trip_preserves_counters() {
        let row = (1_u32, 2_u32, 3_u32);
        let bytes = MempoolEventCountsConsumer::encode_row(row);
        let decoded = MempoolEventCountsConsumer::decode_row(&bytes);
        assert!(matches!(decoded, Some(decoded_row) if decoded_row == row));
    }

    #[test]
    fn key_for_second_is_big_endian_eight_bytes() {
        let key = MempoolEventCountsConsumer::key_for_second(1_700_000_000);
        assert_eq!(key.len(), 8);
        let mut expected = [0u8; 8];
        expected.copy_from_slice(&1_700_000_000_u64.to_be_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn decode_row_rejects_wrong_length() {
        assert!(MempoolEventCountsConsumer::decode_row(&[0u8; 8]).is_none());
        assert!(MempoolEventCountsConsumer::decode_row(&[0u8; 16]).is_none());
    }
}
