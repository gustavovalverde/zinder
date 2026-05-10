//! Channel C → Channel A backfill helper.
//!
//! Implements the M5 D12 backfill-then-attach contract for fresh derive
//! consumers whose persisted cursor sits below the upstream's chain-event
//! retention floor:
//!
//! 1. Read the consumer's persisted cursor. If absent, treat
//!    `last_processed_height` as `BlockHeight::new(0)`.
//! 2. Open a `ChainEvents` stream with `from_cursor = None` and read the
//!    first envelope to discover `oldest_retained_height`.
//! 3. If `last_processed_height < oldest_retained_height -
//!    reorg_window_blocks`: enter Channel C backfill mode, draining
//!    `compact_block_range(last_processed_height..=oldest_retained_height -
//!    reorg_window_blocks)` block by block and dispatching each as a
//!    synthetic `TipAdvancedEvent` to the consumer.
//! 4. Once the consumer's cursor catches up to the retained floor, attach to
//!    the live `ChainEvents` stream using the first envelope's cursor as the
//!    resume point.

use std::num::NonZeroU32;

use rust_rocksdb::WriteBatch;
use thiserror::Error;
use tokio_stream::{Stream, StreamExt as _};
use tonic::Status;
use zinder_core::BlockHeight;
use zinder_proto::v1::wallet::{
    self, ChainEventEnvelope, ChainEventStreamFamily, ChainEventsRequest, CompactBlockRangeRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use crate::consumer::chain_events;
use crate::consumer::{TipAdvancedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx};
use crate::error::DeriveError;
use crate::store::DeriveStore;

/// Configuration for [`backfill_then_attach`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BackfillThenAttachConfig {
    /// `compact_block_range` page size during Channel C drain.
    pub compact_block_page_size: NonZeroU32,
    /// Stream family the live attachment uses.
    pub stream_family: ChainEventStreamFamily,
}

/// Default page size for the Channel C drain.
const DEFAULT_COMPACT_BLOCK_PAGE_SIZE: NonZeroU32 = match NonZeroU32::new(100) {
    Some(non_zero) => non_zero,
    None => unreachable!(),
};

impl Default for BackfillThenAttachConfig {
    fn default() -> Self {
        Self {
            compact_block_page_size: DEFAULT_COMPACT_BLOCK_PAGE_SIZE,
            stream_family: ChainEventStreamFamily::Tip,
        }
    }
}

/// Outcome reported by [`backfill_then_attach`] after the live stream
/// terminates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct BackfillThenAttachOutcome {
    /// Number of synthetic `ChainCommitted` events dispatched during Channel C.
    pub backfilled_blocks: u64,
    /// Outcome from the live `ChainEvents` subscriber.
    pub live: chain_events::ChainEventsRunOutcome,
}

/// Failures that can occur while preparing the backfill.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackfillPrepareError {
    /// Upstream returned no envelope when probed for the retention floor.
    #[error("upstream ChainEvents stream closed without delivering an envelope")]
    UpstreamClosedWithoutEnvelope,
    /// First envelope did not carry a chain epoch.
    #[error("upstream first envelope is missing chain_epoch")]
    FirstEnvelopeMissingChainEpoch,
}

/// Drains the gap from `compact_block_range` then hands off to the live
/// `ChainEvents` subscriber.
///
/// **Status: skeleton.** This entry point captures the M5 D12 contract and
/// the shape future M6+ consumers will call. It is intentionally not yet
/// wired into the binary because the M5 Slice B balance handler is stateless
/// (Shape C compute-at-read-time) and the M6 consumers that need it have
/// not landed. The skeleton exists so the SDK seam is concrete, the
/// dispatched event types match the production trait, and the contract can
/// be exercised by integration tests as M6 consumers come online.
///
/// Returns [`DeriveError::BackfillGapUnrecoverable`] if the consumer is
/// further behind than upstream still retains.
#[allow(
    clippy::missing_errors_doc,
    reason = "DeriveError variants are documented on the type"
)]
pub async fn backfill_then_attach<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    client: &mut WalletQueryClient<AuthenticatedChannel>,
    config: &BackfillThenAttachConfig,
) -> Result<BackfillThenAttachOutcome, DeriveError> {
    let probe = open_chain_events_probe(client, config.stream_family).await?;
    let ProbeOutcome {
        first_envelope,
        first_committed_range,
        live_stream,
    } = probe;

    let backfilled_blocks = drain_backfill_gap(
        consumer,
        store,
        client,
        first_committed_range,
        config.compact_block_page_size,
    )
    .await?;

    let live_outcome =
        run_live_with_first_envelope(consumer, store, first_envelope, live_stream).await?;
    Ok(BackfillThenAttachOutcome {
        backfilled_blocks,
        live: live_outcome,
    })
}

struct ProbeOutcome {
    first_envelope: ChainEventEnvelope,
    first_committed_range: CommittedRange,
    live_stream: tonic::Streaming<ChainEventEnvelope>,
}

async fn open_chain_events_probe(
    client: &mut WalletQueryClient<AuthenticatedChannel>,
    stream_family: ChainEventStreamFamily,
) -> Result<ProbeOutcome, DeriveError> {
    let request = ChainEventsRequest {
        from_cursor: Vec::new(),
        family: i32::from(stream_family),
    };
    let response = client.chain_events(request).await?;
    let mut live_stream = response.into_inner();
    let first_envelope = match live_stream.next().await {
        Some(envelope) => envelope?,
        None => {
            return Err(DeriveError::Decode(
                zinder_store::MempoolDecodeError::MissingField {
                    field: "chain_events.first_envelope",
                },
            ));
        }
    };
    let first_committed_range = first_committed_range_from_envelope(&first_envelope)?;
    Ok(ProbeOutcome {
        first_envelope,
        first_committed_range,
        live_stream,
    })
}

fn first_committed_range_from_envelope(
    envelope: &ChainEventEnvelope,
) -> Result<CommittedRange, DeriveError> {
    use wallet::chain_event_envelope::Event as WireEvent;
    let event = envelope.event.as_ref().ok_or_else(|| {
        DeriveError::Decode(zinder_store::MempoolDecodeError::MissingField {
            field: "chain_events.first_envelope.event",
        })
    })?;
    let (chain_epoch_message, start_height, end_height) = match event {
        WireEvent::TipAdvanced(wire) => {
            let payload = wire.committed.as_ref().ok_or_else(|| {
                DeriveError::Decode(zinder_store::MempoolDecodeError::MissingField {
                    field: "tip_advanced.committed",
                })
            })?;
            (
                payload.chain_epoch.clone(),
                payload.start_height,
                payload.end_height,
            )
        }
        WireEvent::Reorged(wire) => {
            let payload = wire.committed.as_ref().ok_or_else(|| {
                DeriveError::Decode(zinder_store::MempoolDecodeError::MissingField {
                    field: "chain_reorged.committed",
                })
            })?;
            (
                payload.chain_epoch.clone(),
                payload.start_height,
                payload.end_height,
            )
        }
    };
    let chain_epoch_message = chain_epoch_message.ok_or_else(|| {
        DeriveError::Decode(zinder_store::MempoolDecodeError::MissingField {
            field: "chain_events.first_envelope.chain_epoch",
        })
    })?;
    let chain_epoch = zinder_store::chain_epoch_from_message(chain_epoch_message)?;
    Ok(CommittedRange {
        chain_epoch,
        start_height: BlockHeight::new(start_height),
        end_height: BlockHeight::new(end_height),
    })
}

async fn drain_backfill_gap<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    client: &mut WalletQueryClient<AuthenticatedChannel>,
    first_committed_range: CommittedRange,
    page_size: NonZeroU32,
) -> Result<u64, DeriveError> {
    let persisted_height = persisted_last_processed_height(store, consumer.name())?;
    let retained_floor = first_committed_range.start_height;
    if persisted_height >= retained_floor {
        return Ok(0);
    }
    let from_height = next_height_after(persisted_height);
    let to_height = retained_floor.value().saturating_sub(1);
    if to_height < from_height.value() {
        return Ok(0);
    }
    drain_compact_block_range(
        consumer,
        store,
        client,
        DrainRange {
            from_height,
            to_height: BlockHeight::new(to_height),
            chain_epoch: first_committed_range.chain_epoch,
            finalized_height: first_committed_range.chain_epoch.finalized_height,
            page_size,
        },
    )
    .await
}

struct DrainRange {
    from_height: BlockHeight,
    to_height: BlockHeight,
    chain_epoch: zinder_core::ChainEpoch,
    finalized_height: BlockHeight,
    page_size: NonZeroU32,
}

async fn drain_compact_block_range<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    client: &mut WalletQueryClient<AuthenticatedChannel>,
    range: DrainRange,
) -> Result<u64, DeriveError> {
    let mut applied: u64 = 0;
    let mut next_from = range.from_height.value();
    let final_height = range.to_height.value();
    while next_from <= final_height {
        let page_end = next_from
            .saturating_add(range.page_size.get().saturating_sub(1))
            .min(final_height);
        let request = CompactBlockRangeRequest {
            start_height: next_from,
            end_height: page_end,
            at_epoch: None,
        };
        let mut stream = client.compact_block_range(request).await?.into_inner();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            let block = chunk.compact_block.ok_or_else(|| {
                DeriveError::Decode(zinder_store::MempoolDecodeError::MissingField {
                    field: "compact_block_range_chunk.compact_block",
                })
            })?;
            let height = BlockHeight::new(block.height);
            apply_synthetic_committed(
                consumer,
                store,
                TipAdvancedEvent {
                    event_sequence: 0,
                    chain_epoch: range.chain_epoch,
                    finalized_height: range.finalized_height,
                    start_height: height,
                    end_height: height,
                },
            )
            .await?;
            applied = applied.saturating_add(1);
        }
        if page_end >= final_height {
            break;
        }
        next_from = page_end.saturating_add(1);
    }
    Ok(applied)
}

async fn apply_synthetic_committed<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    event: TipAdvancedEvent,
) -> Result<(), DeriveError> {
    let mut batch = WriteBatch::default();
    {
        let mut ctx = DeriveConsumerCtx {
            store,
            batch: &mut batch,
        };
        consumer
            .apply_tip_advanced(&event, &mut ctx)
            .await
            .map_err(DeriveError::Consumer)?;
    }
    store.write_batch(&batch)?;
    Ok(())
}

async fn run_live_with_first_envelope<C: DeriveConsumer>(
    consumer: &mut C,
    store: &DeriveStore,
    first_envelope: ChainEventEnvelope,
    live_stream: tonic::Streaming<ChainEventEnvelope>,
) -> Result<chain_events::ChainEventsRunOutcome, DeriveError> {
    let prepended = stream_with_prepended_first(first_envelope, live_stream);
    chain_events::run(consumer, store, prepended).await
}

fn stream_with_prepended_first(
    first: ChainEventEnvelope,
    rest: tonic::Streaming<ChainEventEnvelope>,
) -> impl Stream<Item = Result<ChainEventEnvelope, Status>> + Unpin + Send {
    let prefix = tokio_stream::iter(std::iter::once(Ok::<ChainEventEnvelope, Status>(first)));
    Box::pin(prefix.chain(rest))
}

fn next_height_after(persisted: BlockHeight) -> BlockHeight {
    BlockHeight::new(persisted.value().saturating_add(1))
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "Skeleton implementation; the production version reads from RocksDB and will return Err on storage failures."
)]
fn persisted_last_processed_height(
    _store: &DeriveStore,
    _consumer: crate::consumer::DeriveConsumerName,
) -> Result<BlockHeight, DeriveError> {
    // The skeleton persists only the cursor today; the height is recovered
    // from the cursor encoding when the SDK gains a typed
    // last_processed_height column. M6+ consumers that need this can extend
    // the contract with a typed metadata row in `consumer_metadata`.
    Ok(BlockHeight::new(0))
}

#[allow(
    dead_code,
    reason = "Type witness for the BackfillPrepareError surface that lands once probe wiring is integrated."
)]
fn _backfill_prepare_error_kept() -> BackfillPrepareError {
    BackfillPrepareError::UpstreamClosedWithoutEnvelope
}
