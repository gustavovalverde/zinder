#![allow(
    missing_docs,
    reason = "Integration test names describe the lightwalletd compat behavior under test."
)]

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::eyre;
use parking_lot::Mutex;
use prost::Message;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt as _, wrappers::UnboundedReceiverStream};
use tonic::Request;
use zinder_compat_lightwalletd::{
    LightwalletdGrpcAdapter, MempoolEventEnvelopeStream, MempoolSnapshotPage, MempoolSurface,
    MempoolSurfaceError, TipChangeWatcher, TipChangeWatcherError,
};
use zinder_core::{
    AuthDigest, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, MempoolEntry,
    MempoolEvictionReason, Network, RawTransactionBytes, TransactionId, UnixTimestampMillis,
};
use zinder_proto::compat::lightwalletd::{self, compact_tx_streamer_server::CompactTxStreamer};
use zinder_query::WalletQuery;
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, MempoolEvent, MempoolEventEnvelope, MempoolEventStreamFamily,
    StreamCursorTokenV1,
};
use zinder_testkit::StoreFixture;

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_tx_returns_unavailable_without_surface() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));
    let outcome = adapter
        .get_mempool_tx(Request::new(lightwalletd::GetMempoolTxRequest {
            exclude_txid_suffixes: Vec::new(),
            pool_types: Vec::new(),
        }))
        .await;
    let status = outcome.err().ok_or_else(|| eyre!("expected unavailable"))?;
    assert_eq!(status.code(), tonic::Code::Unavailable);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_tx_filters_excluded_txid_suffixes() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(vec![
        synthetic_entry(0xAA, synthetic_chain_epoch()),
        synthetic_entry(0xBB, synthetic_chain_epoch()),
    ]);
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface));

    let suffix = vec![0xAA; 4];
    let response = adapter
        .get_mempool_tx(Request::new(lightwalletd::GetMempoolTxRequest {
            exclude_txid_suffixes: vec![suffix],
            pool_types: Vec::new(),
        }))
        .await?
        .into_inner();
    let collected = collect_compact_txids(response).await?;
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0], [0xBB; 32]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_tx_drops_transactions_outside_requested_pool_types()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(vec![
        synthetic_entry(0xC1, synthetic_chain_epoch()),
        transparent_only_entry(0xC2, synthetic_chain_epoch()),
    ]);
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface));

    let response = adapter
        .get_mempool_tx(Request::new(lightwalletd::GetMempoolTxRequest {
            exclude_txid_suffixes: Vec::new(),
            pool_types: vec![lightwalletd::PoolType::Transparent as i32],
        }))
        .await?
        .into_inner();
    let collected = collect_compact_txids(response).await?;
    assert_eq!(collected, vec![[0xC2; 32].to_vec()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_tx_reads_all_snapshot_pages() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(vec![
        synthetic_entry(0xA1, synthetic_chain_epoch()),
        synthetic_entry(0xB2, synthetic_chain_epoch()),
    ])
    .with_snapshot_page_size(1);
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface));

    let response = adapter
        .get_mempool_tx(Request::new(lightwalletd::GetMempoolTxRequest {
            exclude_txid_suffixes: Vec::new(),
            pool_types: Vec::new(),
        }))
        .await?
        .into_inner();
    let collected = collect_compact_txids(response).await?;
    assert_eq!(collected, vec![[0xA1; 32].to_vec(), [0xB2; 32].to_vec()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_stream_projects_added_envelopes_to_raw_transactions()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(Vec::new());
    let control = surface.event_control();
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface));

    let response = adapter
        .get_mempool_stream(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let mut response_stream = response;

    control.push_event(MempoolEvent::Added {
        entry: synthetic_entry(0x10, synthetic_chain_epoch()),
    })?;
    control.push_event(MempoolEvent::Invalidated {
        transaction_id: TransactionId::from_bytes([0x20; 32]),
        reason: MempoolEvictionReason::Conflict,
    })?;
    control.push_event(MempoolEvent::Added {
        entry: synthetic_entry(0x30, synthetic_chain_epoch()),
    })?;

    let first_raw = response_stream
        .next()
        .await
        .ok_or_else(|| eyre!("expected first raw transaction"))??;
    assert_eq!(first_raw.data, vec![0x10; 16]);

    let second_raw = response_stream
        .next()
        .await
        .ok_or_else(|| eyre!("expected second raw transaction"))??;
    // Invalidated was filtered; the second observation is the next Added.
    assert_eq!(second_raw.data, vec![0x30; 16]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_stream_starts_after_retained_tail() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(Vec::new());
    let control = surface.event_control();
    control.append_retained_event(MempoolEvent::Added {
        entry: synthetic_entry(0x10, synthetic_chain_epoch()),
    })?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface));

    let response = adapter
        .get_mempool_stream(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let mut response_stream = response;

    control.push_event(MempoolEvent::Added {
        entry: synthetic_entry(0x20, synthetic_chain_epoch()),
    })?;

    let raw = tokio::time::timeout(std::time::Duration::from_secs(2), response_stream.next())
        .await?
        .ok_or_else(|| eyre!("expected live raw transaction after retained tail"))??;
    assert_eq!(raw.data, vec![0x20; 16]);
    Ok(())
}

fn synthetic_chain_epoch() -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(7),
        network: Network::ZcashRegtest,
        tip_height: BlockHeight::new(123),
        tip_hash: BlockHash::from_bytes([0x42; 32]),
        finalized_height: BlockHeight::new(123),
        finalized_hash: BlockHash::from_bytes([0x42; 32]),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_700_000_000_000),
    }
}

fn synthetic_entry(transaction_id_byte: u8, chain_epoch: ChainEpoch) -> MempoolEntry {
    synthetic_entry_with_compact_tx(
        transaction_id_byte,
        chain_epoch,
        &lightwalletd::CompactTx {
            index: 0,
            txid: transaction_id_byte_to_txid_vec(transaction_id_byte),
            fee: 0,
            spends: Vec::new(),
            outputs: vec![lightwalletd::CompactSaplingOutput {
                cmu: vec![transaction_id_byte; 32],
                ephemeral_key: vec![transaction_id_byte; 32],
                ciphertext: vec![transaction_id_byte; 52],
            }],
            actions: Vec::new(),
            vin: Vec::new(),
            vout: Vec::new(),
        },
    )
}

fn transparent_only_entry(transaction_id_byte: u8, chain_epoch: ChainEpoch) -> MempoolEntry {
    synthetic_entry_with_compact_tx(
        transaction_id_byte,
        chain_epoch,
        &lightwalletd::CompactTx {
            index: 0,
            txid: transaction_id_byte_to_txid_vec(transaction_id_byte),
            fee: 0,
            spends: Vec::new(),
            outputs: Vec::new(),
            actions: Vec::new(),
            vin: vec![lightwalletd::CompactTxIn {
                prevout_txid: vec![0x11; 32],
                prevout_index: 0,
            }],
            vout: vec![lightwalletd::TxOut {
                value: 100,
                script_pub_key: vec![0xAB; 25],
            }],
        },
    )
}

fn synthetic_entry_with_compact_tx(
    transaction_id_byte: u8,
    chain_epoch: ChainEpoch,
    compact_tx: &lightwalletd::CompactTx,
) -> MempoolEntry {
    let transaction_id = TransactionId::from_bytes([transaction_id_byte; 32]);
    let compact_bytes = compact_tx.encode_to_vec();
    MempoolEntry {
        transaction_id,
        auth_digest: Some(AuthDigest::from_bytes([transaction_id_byte; 32])),
        raw_transaction_bytes: RawTransactionBytes::new(vec![transaction_id_byte; 16]),
        compact_transaction_bytes: compact_bytes,
        first_seen_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        first_seen_chain_epoch: chain_epoch,
        transparent_outputs: Vec::new(),
        transparent_spends: Vec::new(),
    }
}

fn transaction_id_byte_to_txid_vec(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

async fn collect_compact_txids<S>(mut stream: S) -> eyre::Result<Vec<Vec<u8>>>
where
    S: tokio_stream::Stream<Item = Result<lightwalletd::CompactTx, tonic::Status>> + Unpin,
{
    let mut transaction_ids = Vec::new();
    while let Some(next) = stream.next().await {
        let compact_tx = next?;
        transaction_ids.push(compact_tx.txid);
    }
    Ok(transaction_ids)
}

type SharedEventSenderSlot =
    Arc<Mutex<Option<mpsc::UnboundedSender<Result<MempoolEventEnvelope, MempoolSurfaceError>>>>>;

struct ScriptedMempoolSurface {
    entries: Mutex<Vec<MempoolEntry>>,
    snapshot_page_size: Option<usize>,
    retained_events: Arc<Mutex<Vec<MempoolEventEnvelope>>>,
    pending_event_sender: SharedEventSenderSlot,
    event_sequence: Arc<Mutex<u64>>,
}

impl ScriptedMempoolSurface {
    fn with_entries(entries: Vec<MempoolEntry>) -> Self {
        Self {
            entries: Mutex::new(entries),
            snapshot_page_size: None,
            retained_events: Arc::new(Mutex::new(Vec::new())),
            pending_event_sender: Arc::new(Mutex::new(None)),
            event_sequence: Arc::new(Mutex::new(0u64)),
        }
    }

    fn with_snapshot_page_size(mut self, snapshot_page_size: usize) -> Self {
        self.snapshot_page_size = Some(snapshot_page_size);
        self
    }

    fn event_control(&self) -> ScriptedMempoolEventControl {
        ScriptedMempoolEventControl {
            retained_events: Arc::clone(&self.retained_events),
            sender_slot: Arc::clone(&self.pending_event_sender),
            event_sequence: Arc::clone(&self.event_sequence),
        }
    }
}

struct ScriptedMempoolEventControl {
    retained_events: Arc<Mutex<Vec<MempoolEventEnvelope>>>,
    sender_slot: SharedEventSenderSlot,
    event_sequence: Arc<Mutex<u64>>,
}

impl ScriptedMempoolEventControl {
    fn append_retained_event(&self, event: MempoolEvent) -> eyre::Result<()> {
        let envelope = self.next_envelope(event)?;
        self.retained_events.lock().push(envelope);
        Ok(())
    }

    fn push_event(&self, event: MempoolEvent) -> eyre::Result<()> {
        let envelope = self.next_envelope(event)?;
        self.retained_events.lock().push(envelope.clone());
        let active_sender = self
            .sender_slot
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| eyre!("scripted mempool surface has no open event stream"))?;
        active_sender
            .send(Ok(envelope))
            .map_err(|_| eyre!("scripted mempool surface receiver dropped"))?;
        Ok(())
    }

    fn next_envelope(&self, event: MempoolEvent) -> eyre::Result<MempoolEventEnvelope> {
        let mut sequence_guard = self.event_sequence.lock();
        *sequence_guard = sequence_guard.saturating_add(1);
        let event_sequence = *sequence_guard;
        drop(sequence_guard);

        Ok(MempoolEventEnvelope {
            cursor: StreamCursorTokenV1::mempool_event(
                Network::ZcashRegtest,
                MempoolEventStreamFamily::Mempool,
                event_sequence,
                event.transaction_id(),
                [9; 32],
            )?,
            event_sequence,
            source_observed_unix_millis: 1_700_000_000_000,
            event,
        })
    }
}

/// Lightwalletd contract: `GetMempoolStream` closes cleanly when the writer
/// observes a best-chain tip change. Native `MempoolEvents` must NOT close on
/// tip change; this is a compat-only behavior preserved for the Go
/// lightwalletd contract Zallet relies on.
#[tokio::test(flavor = "multi_thread")]
async fn lightwalletd_get_mempool_stream_closes_on_tip_change() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let surface = ScriptedMempoolSurface::with_entries(Vec::new());
    let event_control = surface.event_control();
    let tip_change_watcher = ScriptedTipChangeWatcher::new();
    let tip_change_signal = tip_change_watcher.signal();
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .with_mempool_surface(Arc::new(surface))
            .with_tip_change_watcher(Arc::new(tip_change_watcher));

    let response = adapter
        .get_mempool_stream(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let mut response_stream = response;

    event_control.push_event(MempoolEvent::Added {
        entry: synthetic_entry(0xAA, synthetic_chain_epoch()),
    })?;
    let raw = response_stream
        .next()
        .await
        .ok_or_else(|| eyre!("expected first raw transaction"))??;
    assert_eq!(raw.data, vec![0xAA; 16]);

    // Signal a tip change; the stream should end cleanly.
    tip_change_signal.observe_tip_change();
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), response_stream.next())
        .await
        .map_err(|_| eyre!("stream did not end on tip change before timeout"))?;
    assert!(
        next.is_none(),
        "expected stream end after tip change, got: {next:?}"
    );
    Ok(())
}

struct ScriptedTipChangeWatcher {
    notify: Arc<tokio::sync::Notify>,
}

impl ScriptedTipChangeWatcher {
    fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn signal(&self) -> ScriptedTipChangeSignal {
        ScriptedTipChangeSignal {
            notify: Arc::clone(&self.notify),
        }
    }
}

#[derive(Clone)]
struct ScriptedTipChangeSignal {
    notify: Arc<tokio::sync::Notify>,
}

impl ScriptedTipChangeSignal {
    fn observe_tip_change(&self) {
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl TipChangeWatcher for ScriptedTipChangeWatcher {
    async fn await_tip_change(&self) -> Result<(), TipChangeWatcherError> {
        self.notify.notified().await;
        Ok(())
    }
}

#[async_trait]
impl MempoolSurface for ScriptedMempoolSurface {
    async fn mempool_snapshot_page(
        &self,
        max_entries: u32,
        from_cursor: Option<Vec<u8>>,
    ) -> Result<MempoolSnapshotPage, MempoolSurfaceError> {
        let entries = self.entries.lock().clone();
        let start_index = decode_snapshot_page_index(from_cursor.as_deref())?;
        let requested_page_size = usize::try_from(max_entries).unwrap_or(usize::MAX);
        let page_size = self
            .snapshot_page_size
            .map_or(requested_page_size, |limit| limit.min(requested_page_size));
        let end_index = start_index.saturating_add(page_size).min(entries.len());
        let page_entries = entries[start_index..end_index].to_vec();
        let next_cursor = if end_index < entries.len() {
            Some(
                u64::try_from(end_index)
                    .unwrap_or(u64::MAX)
                    .to_be_bytes()
                    .to_vec(),
            )
        } else {
            None
        };
        Ok(MempoolSnapshotPage {
            snapshot_sequence: *self.event_sequence.lock(),
            entries: page_entries,
            next_cursor,
        })
    }

    async fn mempool_events(
        &self,
        _from_cursor: Option<StreamCursorTokenV1>,
    ) -> Result<MempoolEventEnvelopeStream, MempoolSurfaceError> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        for envelope in self.retained_events.lock().iter().cloned() {
            if event_sender.send(Ok(envelope)).is_err() {
                return Err(MempoolSurfaceError::Unavailable {
                    reason: "scripted retained mempool event receiver dropped".to_owned(),
                });
            }
        }
        *self.pending_event_sender.lock() = Some(event_sender);
        let stream: Pin<
            Box<
                dyn Stream<Item = Result<MempoolEventEnvelope, MempoolSurfaceError>>
                    + Send
                    + 'static,
            >,
        > = Box::pin(UnboundedReceiverStream::new(event_receiver));
        Ok(stream)
    }
}

fn decode_snapshot_page_index(cursor: Option<&[u8]>) -> Result<usize, MempoolSurfaceError> {
    let Some(cursor_bytes) = cursor else {
        return Ok(0);
    };
    if cursor_bytes.len() != 8 {
        return Err(MempoolSurfaceError::CursorInvalid);
    }
    let mut index_bytes = [0u8; 8];
    index_bytes.copy_from_slice(cursor_bytes);
    let index = u64::from_be_bytes(index_bytes);
    usize::try_from(index).map_err(|_| MempoolSurfaceError::CursorInvalid)
}
