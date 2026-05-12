//! Cursor-persisting `MempoolEvents` subscriber.
//!
//! Mirrors [`super::chain_events::run`] for [`DeriveMempoolConsumer`]. The
//! subscriber takes any stream of `MempoolEventEnvelope` items and dispatches
//! the typed variant to the consumer along with a [`DeriveConsumerCtx`] that
//! carries a fresh [`WriteBatch`]; the SDK appends the cursor advance to the
//! same batch and commits atomically. Mempool retention is shorter than
//! chain retention, so consumers that fall behind for longer than the
//! retention window receive `DeriveError::Upstream` with
//! `MempoolCursorExpired` from the server side and decide on their own
//! whether to drop and rebuild.

use rust_rocksdb::WriteBatch;
use tokio_stream::{Stream, StreamExt as _};
use tonic::Status;
use zinder_core::BlockHeight;
use zinder_proto::v1::wallet::{
    self, MempoolEventEnvelope, mempool_event_envelope::Event as WireMempoolEvent,
};
use zinder_store::MempoolDecodeError;

use crate::consumer::{
    DeriveConsumerCtx, DeriveMempoolConsumer, MempoolConsumerEvent, MempoolConsumerEventVariant,
};
use crate::error::DeriveError;
use crate::store::{DeriveStore, DeriveStoreTable};

/// Outcome reported when a [`run`] call returns successfully.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct MempoolEventsRunOutcome {
    /// Number of envelopes the subscriber dispatched before the stream
    /// terminated.
    pub applied_envelopes: u64,
    /// Last persisted event sequence, when at least one envelope was
    /// applied.
    pub last_event_sequence: Option<u64>,
}

/// Drains `stream` into `consumer`, persisting the cursor after every
/// envelope.
pub async fn run<C, S>(
    consumer: &mut C,
    store: &DeriveStore,
    mut stream: S,
) -> Result<MempoolEventsRunOutcome, DeriveError>
where
    C: DeriveMempoolConsumer,
    S: Stream<Item = Result<MempoolEventEnvelope, Status>> + Unpin + Send,
{
    let mut outcome = MempoolEventsRunOutcome::default();
    while let Some(envelope_result) = stream.next().await {
        let envelope = envelope_result?;
        let event_sequence = envelope.event_sequence;
        dispatch(consumer, store, envelope).await?;
        outcome.applied_envelopes = outcome.applied_envelopes.saturating_add(1);
        outcome.last_event_sequence = Some(event_sequence);
    }
    Ok(outcome)
}

async fn dispatch<C: DeriveMempoolConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    envelope: MempoolEventEnvelope,
) -> Result<(), DeriveError> {
    if envelope.cursor.is_empty() {
        return Err(DeriveError::Decode(MempoolDecodeError::MissingField {
            field: "mempool_event_envelope.cursor",
        }));
    }
    let event = envelope.event.ok_or(MempoolDecodeError::MissingField {
        field: "mempool_event_envelope.event",
    })?;
    let consumer_name = consumer.name();
    let mut batch = WriteBatch::default();
    let cursor_bytes = envelope.cursor;
    {
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        let consumer_event = build_consumer_event(
            envelope.event_sequence,
            envelope.source_observed_unix_millis,
            &event,
        )?;
        consumer
            .apply_mempool_event(&consumer_event, &mut ctx)
            .await
            .map_err(DeriveError::Consumer)?;
    }
    let column_family = store.column_family(DeriveStoreTable::Cursor)?;
    batch.put_cf(
        &column_family,
        consumer_name.as_str().as_bytes(),
        &cursor_bytes,
    );
    store.write_batch(&batch)?;
    Ok(())
}

fn build_consumer_event(
    event_sequence: u64,
    source_observed_unix_millis: u64,
    event: &WireMempoolEvent,
) -> Result<MempoolConsumerEvent<'_>, DeriveError> {
    let variant = match event {
        WireMempoolEvent::Added(wire) => mempool_added_variant(wire)?,
        WireMempoolEvent::Invalidated(wire) => mempool_invalidated_variant(wire),
        WireMempoolEvent::Mined(wire) => mempool_mined_variant(wire),
        WireMempoolEvent::Suppressed(wire) => mempool_suppressed_variant(wire),
    };
    Ok(MempoolConsumerEvent {
        event_sequence,
        source_observed_unix_millis,
        variant,
    })
}

fn mempool_added_variant(
    wire: &wallet::MempoolAddedEvent,
) -> Result<MempoolConsumerEventVariant<'_>, DeriveError> {
    let entry = wire
        .entry
        .as_ref()
        .ok_or(MempoolDecodeError::MissingField {
            field: "mempool_added_event.entry",
        })?;
    Ok(MempoolConsumerEventVariant::Added {
        transaction_id: entry.transaction_id.as_slice(),
        raw_transaction_bytes: entry.raw_transaction_bytes.as_slice(),
    })
}

fn mempool_invalidated_variant(
    wire: &wallet::MempoolInvalidatedEvent,
) -> MempoolConsumerEventVariant<'_> {
    MempoolConsumerEventVariant::Invalidated {
        transaction_id: wire.transaction_id.as_slice(),
    }
}

fn mempool_mined_variant(wire: &wallet::MempoolMinedEvent) -> MempoolConsumerEventVariant<'_> {
    MempoolConsumerEventVariant::Mined {
        transaction_id: wire.transaction_id.as_slice(),
        mined_height: BlockHeight::new(wire.mined_height),
        block_hash: wire.block_hash.as_slice(),
    }
}

fn mempool_suppressed_variant(
    wire: &wallet::MempoolSuppressedEvent,
) -> MempoolConsumerEventVariant<'_> {
    MempoolConsumerEventVariant::Suppressed {
        transaction_id: wire.transaction_id.as_slice(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use eyre::Result;
    use parking_lot::Mutex;
    use tempfile::tempdir;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::consumer::{DeriveConsumerError, DeriveConsumerName};
    use crate::store::DeriveStoreOptions;

    const TEST_CONSUMER_NAME: DeriveConsumerName = DeriveConsumerName::from_static("test_mempool");

    #[tokio::test]
    async fn run_dispatches_added_then_persists_cursor() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let envelopes = vec![envelope_added(b"cursor-1".to_vec(), 1, &[0xAB; 32])];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await?;
        assert_eq!(outcome.applied_envelopes, 1);
        let transaction_ids = consumer.applied_transaction_ids();
        assert_eq!(transaction_ids.len(), 1);
        assert_eq!(transaction_ids[0].as_slice(), &[0xAB; 32]);
        assert_eq!(
            store.get_cursor(TEST_CONSUMER_NAME)?.as_deref(),
            Some(b"cursor-1".as_slice())
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_returns_decode_error_when_envelope_is_missing_event() -> Result<()> {
        let store = open_store()?;
        let mut consumer = RecordingConsumer::new(TEST_CONSUMER_NAME);
        let envelopes = vec![MempoolEventEnvelope {
            cursor: b"cursor-1".to_vec(),
            event_sequence: 1,
            source_observed_unix_millis: 1,
            event: None,
        }];
        let stream = stream_from(envelopes);
        let outcome = run(&mut consumer, &store, stream).await;
        let Err(DeriveError::Decode(MempoolDecodeError::MissingField { field })) = outcome else {
            return Err(eyre::eyre!("expected MissingField decode error"));
        };
        assert_eq!(field, "mempool_event_envelope.event");
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
        envelopes: Vec<MempoolEventEnvelope>,
    ) -> ReceiverStream<Result<MempoolEventEnvelope, Status>> {
        let (sender, receiver) = tokio::sync::mpsc::channel(envelopes.len().max(1));
        for envelope in envelopes {
            let _ = sender.try_send(Ok(envelope));
        }
        ReceiverStream::new(receiver)
    }

    fn envelope_added(
        cursor: Vec<u8>,
        sequence: u64,
        transaction_id: &[u8; 32],
    ) -> MempoolEventEnvelope {
        MempoolEventEnvelope {
            cursor,
            event_sequence: sequence,
            source_observed_unix_millis: 1,
            event: Some(WireMempoolEvent::Added(wallet::MempoolAddedEvent {
                entry: Some(wallet::MempoolEntry {
                    transaction_id: transaction_id.to_vec(),
                    auth_digest: Vec::new(),
                    raw_transaction_bytes: vec![1, 2, 3],
                    compact_transaction_bytes: vec![],
                    first_seen_unix_millis: 0,
                    first_seen_chain_epoch: None,
                    transparent_outputs: vec![],
                    transparent_spends: vec![],
                }),
            })),
        }
    }

    #[derive(Debug)]
    struct RecordingConsumer {
        name: DeriveConsumerName,
        applied_transaction_ids: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl RecordingConsumer {
        fn new(name: DeriveConsumerName) -> Self {
            Self {
                name,
                applied_transaction_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn applied_transaction_ids(&self) -> Vec<Vec<u8>> {
            self.applied_transaction_ids.lock().clone()
        }
    }

    #[async_trait]
    impl DeriveMempoolConsumer for RecordingConsumer {
        fn name(&self) -> DeriveConsumerName {
            self.name
        }

        async fn apply_mempool_event(
            &mut self,
            event: &MempoolConsumerEvent<'_>,
            _ctx: &mut DeriveConsumerCtx<'_>,
        ) -> Result<(), DeriveConsumerError> {
            if let MempoolConsumerEventVariant::Added { transaction_id, .. } = &event.variant {
                self.applied_transaction_ids
                    .lock()
                    .push(transaction_id.to_vec());
            }
            Ok(())
        }
    }
}
