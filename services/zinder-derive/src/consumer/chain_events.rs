//! Cursor-persisting `ChainEvents` subscriber.
//!
//! [`run`] drives a generic `ChainEventEnvelope` stream against a
//! [`DeriveConsumer`]. For each envelope it decodes the typed event, hands it
//! to the consumer's `apply_chain_*` hook with a [`DeriveConsumerCtx`] that
//! borrows a fresh [`WriteBatch`], appends the cursor advance to that same
//! batch, and commits atomically. The batch shape is what makes the cursor
//! contract durable: a crash mid-write replays the event on next startup; a
//! crash after the commit advances the cursor and the consumer's writes
//! together.
//!
//! The function is generic over the stream type so production callers feed
//! it a `tonic::Streaming` and tests feed it an in-memory channel; both
//! implement `tokio_stream::Stream` over the same wire envelope shape.

use rust_rocksdb::WriteBatch;
use tokio_stream::{Stream, StreamExt as _};
use tonic::Status;
use zinder_core::BlockHeight;
use zinder_proto::v1::wallet::{
    ChainEpochCommitted as WireChainEpochCommitted, ChainEventEnvelope,
    ChainRangeReverted as WireChainRangeReverted, ChainReorged, TipAdvanced,
    chain_event_envelope::Event as WireChainEvent,
};
use zinder_store::{MempoolDecodeError, chain_epoch_from_message};

use crate::consumer::{
    TipAdvancedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    RevertedRange,
};
use crate::error::DeriveError;
use crate::store::{DeriveStore, DeriveStoreTable};

/// Outcome reported when a [`run`] call returns successfully.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChainEventsRunOutcome {
    /// Number of envelopes the subscriber dispatched before the stream
    /// terminated.
    pub applied_envelopes: u64,
    /// Last persisted event sequence, when at least one envelope was
    /// applied. `None` indicates the stream closed before delivering an
    /// envelope.
    pub last_event_sequence: Option<u64>,
}

/// Drains `stream` into `consumer`, persisting the cursor after every
/// envelope.
///
/// Returns when the stream terminates cleanly (server-side end) or with an
/// error if the upstream returns a non-OK status, an envelope fails to
/// decode, or the consumer's apply method fails. Callers that want to keep
/// running after a transient stream error open a new stream and call `run`
/// again with the persisted cursor.
pub async fn run<C, S>(
    consumer: &mut C,
    store: &DeriveStore,
    mut stream: S,
) -> Result<ChainEventsRunOutcome, DeriveError>
where
    C: DeriveConsumer,
    S: Stream<Item = Result<ChainEventEnvelope, Status>> + Unpin + Send,
{
    let mut outcome = ChainEventsRunOutcome::default();
    while let Some(envelope_result) = stream.next().await {
        let envelope = envelope_result?;
        let event_sequence = envelope.event_sequence;
        dispatch(consumer, store, envelope).await?;
        outcome.applied_envelopes = outcome.applied_envelopes.saturating_add(1);
        outcome.last_event_sequence = Some(event_sequence);
    }
    Ok(outcome)
}

async fn dispatch<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    envelope: ChainEventEnvelope,
) -> Result<(), DeriveError> {
    let decoded = decode_envelope(envelope)?;
    let mut batch = WriteBatch::default();
    let consumer_name = consumer.name();
    {
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        match decoded.event {
            DecodedEvent::TipAdvanced(event) => {
                consumer
                    .apply_tip_advanced(&event, &mut ctx)
                    .await
                    .map_err(DeriveError::Consumer)?;
            }
            DecodedEvent::Reorged(event) => {
                consumer
                    .apply_chain_reorged(event.as_ref(), &mut ctx)
                    .await
                    .map_err(DeriveError::Consumer)?;
            }
        }
    }
    let column_family = store.column_family(DeriveStoreTable::Cursor)?;
    batch.put_cf(
        &column_family,
        consumer_name.as_str().as_bytes(),
        &decoded.cursor_bytes,
    );
    store.write_batch(&batch)?;
    Ok(())
}

struct DecodedEnvelope {
    cursor_bytes: Vec<u8>,
    event: DecodedEvent,
}

enum DecodedEvent {
    TipAdvanced(TipAdvancedEvent),
    Reorged(Box<ChainReorgedEvent>),
}

fn decode_envelope(envelope: ChainEventEnvelope) -> Result<DecodedEnvelope, MempoolDecodeError> {
    if envelope.cursor.is_empty() {
        return Err(MempoolDecodeError::MissingField {
            field: "chain_event_envelope.cursor",
        });
    }
    let chain_epoch_message = envelope
        .chain_epoch
        .ok_or(MempoolDecodeError::MissingField {
            field: "chain_event_envelope.chain_epoch",
        })?;
    let chain_epoch = chain_epoch_from_message(chain_epoch_message)?;
    let finalized_height = BlockHeight::new(envelope.finalized_height);
    let event = envelope.event.ok_or(MempoolDecodeError::MissingField {
        field: "chain_event_envelope.event",
    })?;
    let decoded_event = match event {
        WireChainEvent::TipAdvanced(committed) => DecodedEvent::TipAdvanced(decode_tip_advanced(
            envelope.event_sequence,
            chain_epoch,
            finalized_height,
            committed,
        )?),
        WireChainEvent::Reorged(reorged) => DecodedEvent::Reorged(Box::new(decode_reorged(
            envelope.event_sequence,
            chain_epoch,
            finalized_height,
            reorged,
        )?)),
    };
    Ok(DecodedEnvelope {
        cursor_bytes: envelope.cursor,
        event: decoded_event,
    })
}

fn decode_tip_advanced(
    event_sequence: u64,
    chain_epoch: zinder_core::ChainEpoch,
    finalized_height: BlockHeight,
    wire: TipAdvanced,
) -> Result<TipAdvancedEvent, MempoolDecodeError> {
    let payload = wire.committed.ok_or(MempoolDecodeError::MissingField {
        field: "tip_advanced.committed",
    })?;
    Ok(TipAdvancedEvent {
        event_sequence,
        chain_epoch,
        finalized_height,
        start_height: BlockHeight::new(payload.start_height),
        end_height: BlockHeight::new(payload.end_height),
    })
}

fn decode_reorged(
    event_sequence: u64,
    chain_epoch: zinder_core::ChainEpoch,
    finalized_height: BlockHeight,
    wire: ChainReorged,
) -> Result<ChainReorgedEvent, MempoolDecodeError> {
    let reverted_wire = wire.reverted.ok_or(MempoolDecodeError::MissingField {
        field: "chain_reorged.reverted",
    })?;
    let replacement_wire = wire.committed.ok_or(MempoolDecodeError::MissingField {
        field: "chain_reorged.committed",
    })?;
    let reverted = decode_reverted_range(reverted_wire)?;
    let replacement = decode_tip_advanced_range(replacement_wire)?;
    Ok(ChainReorgedEvent {
        event_sequence,
        chain_epoch,
        finalized_height,
        reverted,
        replacement,
    })
}

fn decode_reverted_range(
    wire: WireChainRangeReverted,
) -> Result<RevertedRange, MempoolDecodeError> {
    let chain_epoch_message = wire.chain_epoch.ok_or(MempoolDecodeError::MissingField {
        field: "chain_range_reverted.chain_epoch",
    })?;
    let chain_epoch = chain_epoch_from_message(chain_epoch_message)?;
    Ok(RevertedRange {
        chain_epoch,
        start_height: BlockHeight::new(wire.start_height),
        end_height: BlockHeight::new(wire.end_height),
    })
}

fn decode_tip_advanced_range(
    wire: WireChainEpochCommitted,
) -> Result<CommittedRange, MempoolDecodeError> {
    let chain_epoch_message = wire.chain_epoch.ok_or(MempoolDecodeError::MissingField {
        field: "chain_epoch_committed.chain_epoch",
    })?;
    let chain_epoch = chain_epoch_from_message(chain_epoch_message)?;
    Ok(CommittedRange {
        chain_epoch,
        start_height: BlockHeight::new(wire.start_height),
        end_height: BlockHeight::new(wire.end_height),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use eyre::Result;
    use parking_lot::Mutex;
    use tempfile::tempdir;
    use tokio_stream::wrappers::ReceiverStream;
    use zinder_core::ChainEpoch;
    use zinder_proto::v1::wallet;
    use zinder_store::chain_epoch_message;

    use super::*;
    use crate::consumer::{DeriveConsumerError, DeriveConsumerName};
    use crate::store::DeriveStoreOptions;

    const TEST_CONSUMER_NAME: DeriveConsumerName = DeriveConsumerName::from_static("test_chain");

    #[tokio::test]
    async fn run_dispatches_committed_then_persists_cursor() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let chain_epoch = test_chain_epoch(1);
        let envelopes = vec![envelope_committed(
            b"cursor-1".to_vec(),
            1,
            chain_epoch,
            10,
            12,
        )];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await?;
        assert_eq!(outcome.applied_envelopes, 1);
        assert_eq!(outcome.last_event_sequence, Some(1));
        let calls = consumer.applied();
        assert_eq!(calls.len(), 1);
        let AppliedCall::TipAdvanced(event) = &calls[0] else {
            return Err(eyre::eyre!("expected Committed call"));
        };
        assert_eq!(event.event_sequence, 1);
        assert_eq!(event.start_height.value(), 10);
        assert_eq!(event.end_height.value(), 12);
        assert_eq!(
            store.get_cursor(TEST_CONSUMER_NAME)?.as_deref(),
            Some(b"cursor-1".as_slice())
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_advances_cursor_through_multiple_envelopes() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let envelopes = vec![
            envelope_committed(b"cursor-1".to_vec(), 1, test_chain_epoch(1), 10, 10),
            envelope_committed(b"cursor-2".to_vec(), 2, test_chain_epoch(2), 11, 11),
            envelope_committed(b"cursor-3".to_vec(), 3, test_chain_epoch(3), 12, 12),
        ];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await?;
        assert_eq!(outcome.applied_envelopes, 3);
        assert_eq!(outcome.last_event_sequence, Some(3));
        assert_eq!(consumer.applied().len(), 3);
        assert_eq!(
            store.get_cursor(TEST_CONSUMER_NAME)?.as_deref(),
            Some(b"cursor-3".as_slice())
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_returns_consumer_error_without_advancing_cursor() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME).failing_on_first();
        let envelopes = vec![envelope_committed(
            b"cursor-1".to_vec(),
            1,
            test_chain_epoch(1),
            10,
            10,
        )];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await;
        let Err(DeriveError::Consumer(_)) = outcome else {
            return Err(eyre::eyre!("expected DeriveError::Consumer"));
        };
        assert!(store.get_cursor(TEST_CONSUMER_NAME)?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn run_dispatches_reorged_event() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let envelopes = vec![envelope_reorged(ReorgedFixture {
            cursor: b"cursor-1".to_vec(),
            sequence: 1,
            chain_epoch: test_chain_epoch(2),
            reverted: (10, 11),
            replacement: (10, 12),
        })];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await?;
        assert_eq!(outcome.applied_envelopes, 1);
        let calls = consumer.applied();
        let AppliedCall::Reorged(event) = &calls[0] else {
            return Err(eyre::eyre!("expected Reorged call"));
        };
        assert_eq!(event.reverted.start_height.value(), 10);
        assert_eq!(event.reverted.end_height.value(), 11);
        assert_eq!(event.replacement.start_height.value(), 10);
        assert_eq!(event.replacement.end_height.value(), 12);
        Ok(())
    }

    #[tokio::test]
    async fn run_returns_decode_error_when_cursor_is_empty() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let mut envelope = envelope_committed(Vec::new(), 1, test_chain_epoch(1), 10, 10);
        envelope.cursor.clear();
        let stream = stream_from(vec![envelope]);
        let outcome = run(&mut consumer, &store, stream).await;
        let Err(DeriveError::Decode(MempoolDecodeError::MissingField { field })) = outcome else {
            return Err(eyre::eyre!("expected MissingField decode error"));
        };
        assert_eq!(field, "chain_event_envelope.cursor");
        assert!(store.get_cursor(TEST_CONSUMER_NAME)?.is_none());
        Ok(())
    }

    fn open_store() -> Result<DeriveStore> {
        let dir = tempdir()?;
        Ok(DeriveStore::open(
            dir.path(),
            DeriveStoreOptions::default(),
        )?)
    }

    fn stream_from(
        envelopes: Vec<ChainEventEnvelope>,
    ) -> ReceiverStream<Result<ChainEventEnvelope, Status>> {
        let (sender, receiver) = tokio::sync::mpsc::channel(envelopes.len().max(1));
        for envelope in envelopes {
            let _ = sender.try_send(Ok(envelope));
        }
        ReceiverStream::new(receiver)
    }

    fn envelope_committed(
        cursor: Vec<u8>,
        sequence: u64,
        chain_epoch: ChainEpoch,
        start: u32,
        end: u32,
    ) -> ChainEventEnvelope {
        ChainEventEnvelope {
            cursor,
            event_sequence: sequence,
            chain_epoch: Some(chain_epoch_message(chain_epoch)),
            finalized_height: end,
            event: Some(WireChainEvent::TipAdvanced(wallet::TipAdvanced {
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(chain_epoch_message(chain_epoch)),
                    start_height: start,
                    end_height: end,
                }),
            })),
        }
    }

    struct ReorgedFixture {
        cursor: Vec<u8>,
        sequence: u64,
        chain_epoch: ChainEpoch,
        reverted: (u32, u32),
        replacement: (u32, u32),
    }

    fn envelope_reorged(fixture: ReorgedFixture) -> ChainEventEnvelope {
        let (reverted_start, reverted_end) = fixture.reverted;
        let (replacement_start, replacement_end) = fixture.replacement;
        ChainEventEnvelope {
            cursor: fixture.cursor,
            event_sequence: fixture.sequence,
            chain_epoch: Some(chain_epoch_message(fixture.chain_epoch)),
            finalized_height: replacement_end,
            event: Some(WireChainEvent::Reorged(wallet::ChainReorged {
                reverted: Some(wallet::ChainRangeReverted {
                    chain_epoch: Some(chain_epoch_message(fixture.chain_epoch)),
                    start_height: reverted_start,
                    end_height: reverted_end,
                }),
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(chain_epoch_message(fixture.chain_epoch)),
                    start_height: replacement_start,
                    end_height: replacement_end,
                }),
            })),
        }
    }

    fn test_chain_epoch(sequence: u64) -> ChainEpoch {
        use zinder_core::{
            ArtifactSchemaVersion, BlockHash, ChainEpochId, ChainTipMetadata, Network,
            UnixTimestampMillis,
        };
        ChainEpoch {
            id: ChainEpochId::new(sequence),
            network: Network::ZcashRegtest,
            tip_height: BlockHeight::new(12),
            tip_hash: BlockHash::from_bytes([1; 32]),
            finalized_height: BlockHeight::new(12),
            finalized_hash: BlockHash::from_bytes([2; 32]),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::new(0, 0),
            created_at: UnixTimestampMillis::new(1),
        }
    }

    #[derive(Clone, Debug)]
    enum AppliedCall {
        TipAdvanced(TipAdvancedEvent),
        Reorged(Box<ChainReorgedEvent>),
    }

    #[derive(Debug)]
    struct RecordingConsumer {
        name: DeriveConsumerName,
        applied: Arc<Mutex<Vec<AppliedCall>>>,
        fail_first: bool,
        first_call: Arc<Mutex<bool>>,
    }

    impl RecordingConsumer {
        fn new(name: DeriveConsumerName) -> Self {
            Self {
                name,
                applied: Arc::new(Mutex::new(Vec::new())),
                fail_first: false,
                first_call: Arc::new(Mutex::new(true)),
            }
        }

        fn failing_on_first(mut self) -> Self {
            self.fail_first = true;
            self
        }

        fn applied(&self) -> Vec<AppliedCall> {
            self.applied.lock().clone()
        }
    }

    #[async_trait]
    impl DeriveConsumer for RecordingConsumer {
        fn name(&self) -> DeriveConsumerName {
            self.name
        }

        async fn apply_tip_advanced(
            &mut self,
            event: &TipAdvancedEvent,
            _ctx: &mut DeriveConsumerCtx<'_>,
        ) -> Result<(), DeriveConsumerError> {
            if self.fail_first && std::mem::replace(&mut *self.first_call.lock(), false) {
                return Err(Box::new(IntentionalFailure));
            }
            self.applied.lock().push(AppliedCall::TipAdvanced(*event));
            Ok(())
        }

        async fn apply_chain_reorged(
            &mut self,
            event: &ChainReorgedEvent,
            _ctx: &mut DeriveConsumerCtx<'_>,
        ) -> Result<(), DeriveConsumerError> {
            self.applied
                .lock()
                .push(AppliedCall::Reorged(Box::new(*event)));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct IntentionalFailure;

    impl std::fmt::Display for IntentionalFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "intentional consumer failure")
        }
    }

    impl std::error::Error for IntentionalFailure {}
}
