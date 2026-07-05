//! `ChainReorgHistory` derive consumer.
//!
//! Materializes one durable row for each `ChainReorged` event observed by the
//! derive tailer. The projection backfills from the earliest retained
//! chain-event row on first deployment and then preserves future incidents
//! independently of chain-event retention. It cannot reconstruct incidents
//! already pruned before this consumer first ran.

use prost::Message as _;
use zinder_proto::v1::{
    explorer::ChainReorgHistoryEvent,
    wallet::{ChainEpochCommitted, ChainRangeReverted},
};
use zinder_store::{block_tip_message, chain_epoch_message};

use crate::consumer::{
    ChainCommittedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveConsumerSchema, RevertedRange,
};

/// Column-family name the consumer owns.
pub const REORG_INCIDENTS_COLUMN_FAMILY: &str = "reorg_incidents";

/// Stable consumer name persisted in the SDK cursor table.
pub const REORG_INCIDENTS_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("reorg_incidents");

/// On-disk schema declaration for the reorg-incidents derive consumer.
pub const REORG_INCIDENTS_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    REORG_INCIDENTS_CONSUMER_NAME,
    2,
    &[REORG_INCIDENTS_COLUMN_FAMILY],
);

/// Length of one storage key: ascending `event_sequence` as `u64` big-endian.
pub const REORG_INCIDENTS_KEY_LEN: usize = 8;

/// Materializes one incident row per chain reorg.
#[derive(Default)]
pub struct ReorgIncidentsConsumer;

impl ReorgIncidentsConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the storage key for one chain-event sequence.
    #[must_use]
    pub const fn key_for_event_sequence(event_sequence: u64) -> [u8; REORG_INCIDENTS_KEY_LEN] {
        event_sequence.to_be_bytes()
    }

    /// Decodes an incident cursor produced by this consumer.
    ///
    /// The cursor is intentionally opaque at the API boundary. Its current
    /// internal representation is the row key, so pagination can resume with
    /// one bounded range seek.
    pub fn decode_event_sequence_cursor(cursor: &[u8]) -> Result<u64, ReorgIncidentsConsumerError> {
        let bytes: [u8; REORG_INCIDENTS_KEY_LEN] =
            cursor
                .try_into()
                .map_err(|_| ReorgIncidentsConsumerError::InvalidCursorLength {
                    length: cursor.len(),
                })?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Returns the API cursor for one incident row.
    #[must_use]
    pub const fn cursor_for_event_sequence(event_sequence: u64) -> [u8; REORG_INCIDENTS_KEY_LEN] {
        Self::key_for_event_sequence(event_sequence)
    }

    fn message_for_reorg(event: &ChainReorgedEvent) -> ChainReorgHistoryEvent {
        ChainReorgHistoryEvent {
            event_sequence: event.event_sequence,
            cursor: Self::cursor_for_event_sequence(event.event_sequence).to_vec(),
            chain_epoch_id: event.chain_epoch.id.value(),
            chain_epoch_created_at_millis: event.chain_epoch.created_at.value(),
            visible_tip: Some(block_tip_message(
                event.chain_epoch.visible_tip_height,
                event.chain_epoch.visible_tip_hash,
            )),
            settled_tip: Some(block_tip_message(
                event.chain_epoch.settled_tip_height,
                event.chain_epoch.settled_tip_hash,
            )),
            reverted: Some(chain_range_reverted_message(event.reverted)),
            committed: Some(chain_epoch_committed_message(event.replacement)),
        }
    }
}

impl DeriveConsumer for ReorgIncidentsConsumer {
    fn name(&self) -> DeriveConsumerName {
        REORG_INCIDENTS_CONSUMER_NAME
    }

    fn apply_chain_committed(
        &mut self,
        _event: &ChainCommittedEvent,
        _ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        Ok(())
    }

    fn apply_chain_reorged(
        &mut self,
        event: &ChainReorgedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let cf = ctx
            .store
            .consumer_column_family(REORG_INCIDENTS_COLUMN_FAMILY)?;
        let message = Self::message_for_reorg(event);
        let mut payload = Vec::with_capacity(message.encoded_len());
        message
            .encode(&mut payload)
            .map_err(|error| ReorgIncidentsConsumerError::Encode(error.to_string()))?;
        ctx.batch.put_cf(
            &cf,
            Self::key_for_event_sequence(event.event_sequence),
            payload,
        );
        Ok(())
    }
}

fn chain_range_reverted_message(reverted: RevertedRange) -> ChainRangeReverted {
    ChainRangeReverted {
        chain_epoch: Some(chain_epoch_message(reverted.chain_epoch)),
        start_height: reverted.start_height.value(),
        end_height: reverted.end_height.value(),
    }
}

fn chain_epoch_committed_message(committed: CommittedRange) -> ChainEpochCommitted {
    ChainEpochCommitted {
        chain_epoch: Some(chain_epoch_message(committed.chain_epoch)),
        start_height: committed.start_height.value(),
        end_height: committed.end_height.value(),
    }
}

/// Consumer-specific failure modes [`ReorgIncidentsConsumer`] can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReorgIncidentsConsumerError {
    /// Storage encoding of the materialized entry failed.
    #[error("ChainReorgHistoryEvent prost encode failed: {0}")]
    Encode(String),
    /// Cursor bytes do not match this consumer's key codec.
    #[error("reorg incident cursor must be 8 bytes; got {length}")]
    InvalidCursorLength {
        /// Observed cursor length.
        length: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_sequence_key_round_trips_as_cursor() {
        let key = ReorgIncidentsConsumer::key_for_event_sequence(42);
        let decoded = ReorgIncidentsConsumer::decode_event_sequence_cursor(&key);
        assert!(matches!(decoded, Ok(42)));
    }

    #[test]
    fn event_sequence_key_sorts_ascending() {
        let lower = ReorgIncidentsConsumer::key_for_event_sequence(41);
        let higher = ReorgIncidentsConsumer::key_for_event_sequence(42);
        assert!(lower < higher);
    }

    #[test]
    fn decode_event_sequence_cursor_rejects_wrong_length() {
        let decoded = ReorgIncidentsConsumer::decode_event_sequence_cursor(&[1, 2, 3]);
        assert!(matches!(
            decoded,
            Err(ReorgIncidentsConsumerError::InvalidCursorLength { length: 3 })
        ));
    }
}
