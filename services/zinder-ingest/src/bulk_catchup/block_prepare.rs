use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    future::Future,
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{
    FutureExt,
    future::BoxFuture,
    stream::{self, BoxStream, FuturesUnordered, Stream, StreamExt},
};
use parking_lot::Mutex;
use prost::Message as _;
use zinder_core::{BlockHeight, TransparentOutPoint, TransparentOutputArtifact};
use zinder_source::{NodeSource, SourceBlock, SourceError};
use zinder_store::PrimaryChainStore;

use super::abort_on_drop::AbortOnDropTask;
use super::source_fetch::{
    BulkCatchupSourceFetchStreamConfig, SourceSegmentSizer, build_source_block_stream,
};
use super::watermark::{ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer};
use super::{
    BULK_STAGE_CANONICAL_BLOCK_PREPARE, IngestError, usize_to_u32_saturating,
    usize_to_u64_saturating,
};
use crate::artifact_builder::DerivedBlockArtifacts;
use crate::chain_ingest::{
    prefetched_spent_transparent_output_bytes, record_ingest_block_prepare_outcome,
};

pub(super) struct BulkCatchupBlockPrepareStreamConfig {
    pub(super) request_timeout: Duration,
    pub(super) from_height: BlockHeight,
    pub(super) to_height: BlockHeight,
    pub(super) max_response_bytes: NonZeroU64,
    pub(super) target_response_payload_bytes: NonZeroU64,
    pub(super) source_fetch_max_in_flight_requests: NonZeroU32,
    pub(super) source_fetch_max_in_flight_bytes: NonZeroU64,
    pub(super) source_segment_sizer: Arc<Mutex<SourceSegmentSizer>>,
    pub(super) block_prepare_concurrency: usize,
    pub(super) block_prepare_max_in_flight_artifact_bytes: NonZeroU64,
    pub(super) store: PrimaryChainStore,
}

pub(super) struct PreparedBlockArtifacts {
    pub(super) derived: DerivedBlockArtifacts,
    pub(super) prefetched_spent_transparent_outputs: Vec<TransparentOutputArtifact>,
}

pub(super) fn build_block_prepare_stream<'a, Source, F, Fut>(
    source: &'a Source,
    config: BulkCatchupBlockPrepareStreamConfig,
    derive_fn: F,
) -> impl Stream<Item = Result<PreparedBlockArtifacts, IngestError>> + Send + 'a
where
    Source: NodeSource + Clone + 'a,
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
{
    let BulkCatchupBlockPrepareStreamConfig {
        request_timeout,
        from_height,
        to_height,
        max_response_bytes,
        target_response_payload_bytes,
        source_fetch_max_in_flight_requests,
        source_fetch_max_in_flight_bytes,
        source_segment_sizer,
        block_prepare_concurrency,
        block_prepare_max_in_flight_artifact_bytes,
        store,
    } = config;
    let block_prepare_concurrency = block_prepare_concurrency.max(1);
    let source_fetch_config = BulkCatchupSourceFetchStreamConfig {
        request_timeout,
        from_height,
        to_height,
        max_response_bytes,
        target_response_payload_bytes,
        source_fetch_max_in_flight_requests,
        source_fetch_max_in_flight_bytes,
        source_segment_sizer,
    };
    let state = BlockPrepareStreamState {
        source_blocks: build_source_block_stream(source, source_fetch_config).boxed(),
        in_flight_block_prepares: FuturesUnordered::new(),
        completed_block_prepares: BTreeMap::new(),
        completed_block_prepare_bytes: 0,
        pending_source_blocks: VecDeque::new(),
        derive_fn,
        store,
        block_prepare_concurrency,
        block_prepare_watermark: ByteWatermark::new(
            BULK_STAGE_CANONICAL_BLOCK_PREPARE,
            block_prepare_max_in_flight_artifact_bytes,
        ),
        next_emit_height: Some(from_height),
        to_height,
        source_exhausted: false,
    };

    stream::unfold(state, |mut state| async move {
        let next_derived_block = next_derived_block_from_block_prepare_stream(&mut state).await;
        next_derived_block.map(|derived_result| (derived_result, state))
    })
}

struct PrefetchedDerivedBlock {
    height: BlockHeight,
    prepared: PreparedBlockArtifacts,
    artifact_bytes: u64,
    reservation: ByteReservation,
}

struct QueuedDerivedBlock {
    prepared: PreparedBlockArtifacts,
    artifact_bytes: u64,
    reservation: ByteReservation,
}

struct BlockPrepareStreamState<'a, F> {
    source_blocks: BoxStream<'a, Result<SourceBlock, IngestError>>,
    in_flight_block_prepares:
        FuturesUnordered<BoxFuture<'static, Result<PrefetchedDerivedBlock, IngestError>>>,
    completed_block_prepares: BTreeMap<BlockHeight, QueuedDerivedBlock>,
    completed_block_prepare_bytes: u64,
    pending_source_blocks: VecDeque<SourceBlock>,
    derive_fn: F,
    store: PrimaryChainStore,
    block_prepare_concurrency: usize,
    block_prepare_watermark: ByteWatermark,
    next_emit_height: Option<BlockHeight>,
    to_height: BlockHeight,
    source_exhausted: bool,
}

async fn next_derived_block_from_block_prepare_stream<F, Fut>(
    state: &mut BlockPrepareStreamState<'_, F>,
) -> Option<Result<PreparedBlockArtifacts, IngestError>>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
{
    loop {
        if let Some(next_emit_height) = state.next_emit_height
            && let Some(queued) = state.completed_block_prepares.remove(&next_emit_height)
        {
            state.next_emit_height = next_emit_height
                .next()
                .filter(|height| *height <= state.to_height);
            let QueuedDerivedBlock {
                prepared,
                artifact_bytes,
                reservation,
            } = queued;
            state.completed_block_prepare_bytes = state
                .completed_block_prepare_bytes
                .saturating_sub(artifact_bytes);
            record_block_prepare_reassembly_state(state);
            drop(reservation);
            return Some(Ok(prepared));
        }

        if let Some(source_block) = state.pending_source_blocks.pop_front() {
            match schedule_block_prepare(state, source_block) {
                Ok(()) => continue,
                Err(source_block) => state.pending_source_blocks.push_front(source_block),
            }
        }

        let can_schedule_block_prepare = state.can_schedule_block_prepare();
        if !can_schedule_block_prepare && state.in_flight_block_prepares.is_empty() {
            return None;
        }

        tokio::select! {
            source_block_result = state.source_blocks.next(), if can_schedule_block_prepare => {
                match source_block_result {
                    Some(Ok(source_block)) => {
                        if let Err(source_block) = schedule_block_prepare(state, source_block) {
                            state.pending_source_blocks.push_front(source_block);
                        }
                    }
                    Some(Err(error)) => return Some(Err(error)),
                    None => state.source_exhausted = true,
                }
            }
            block_prepare_result = state.in_flight_block_prepares.next(), if !state.in_flight_block_prepares.is_empty() => {
                let prefetched_derived = match block_prepare_result {
                    Some(Ok(prefetched_derived)) => prefetched_derived,
                    Some(Err(error)) => return Some(Err(error)),
                    None => continue,
                };
                if let Err(error) = insert_completed_block_prepare(state, prefetched_derived) {
                    return Some(Err(error));
                }
                record_block_prepare_reassembly_state(state);
            }
        }
    }
}

impl<F> BlockPrepareStreamState<'_, F> {
    fn can_schedule_block_prepare(&self) -> bool {
        !self.source_exhausted
            && self.pending_source_blocks.is_empty()
            && self.in_flight_block_prepares.len() < self.block_prepare_concurrency
            && self.completed_block_prepares.len() < self.block_prepare_concurrency
    }
}

fn schedule_block_prepare<F, Fut>(
    state: &BlockPrepareStreamState<'_, F>,
    source_block: SourceBlock,
) -> Result<(), SourceBlock>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
{
    if state.in_flight_block_prepares.len() >= state.block_prepare_concurrency {
        return Err(source_block);
    }
    let estimated_artifact_bytes = source_block.raw_block_bytes.len().max(1);
    let Some(reservation) = state
        .block_prepare_watermark
        .try_reserve(usize_to_u64_saturating(estimated_artifact_bytes))
    else {
        return Err(source_block);
    };
    let derive_fn = state.derive_fn.clone();
    let store = state.store.clone();
    // Spawned so per-block artifact derivation progresses on runtime workers
    // instead of only while this stream is polled by the commit consumer.
    let block_prepare_task = AbortOnDropTask::spawn(async move {
        let height = source_block.height;
        let block_prepare_started_at = Instant::now();
        let block_prepare_outcome = async {
            let derived = derive_fn(source_block).await?;
            let prefetched_spent_transparent_outputs =
                prefetch_spent_transparent_outputs(store, &derived).await?;
            let prepared = PreparedBlockArtifacts {
                derived,
                prefetched_spent_transparent_outputs,
            };
            let artifact_bytes = prepared_block_artifact_bytes(&prepared);
            let mut reservation = reservation;
            reservation.resize(artifact_bytes);
            Ok(PrefetchedDerivedBlock {
                height,
                prepared,
                artifact_bytes,
                reservation,
            })
        }
        .await;
        record_ingest_block_prepare_outcome(block_prepare_started_at, &block_prepare_outcome);
        block_prepare_outcome
    });
    state.in_flight_block_prepares.push(
        async move {
            match block_prepare_task.join().await {
                Ok(block_prepare_outcome) => block_prepare_outcome,
                Err(join_error) => Err(IngestError::BlockingTaskFailed {
                    reason: join_error.to_string(),
                }),
            }
        }
        .boxed(),
    );
    Ok(())
}

fn insert_completed_block_prepare<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
    prefetched_derived: PrefetchedDerivedBlock,
) -> Result<(), IngestError> {
    if prefetched_derived.height > state.to_height {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "derived block completed outside the requested bulk-catchup range",
        }));
    }
    state.completed_block_prepare_bytes = state
        .completed_block_prepare_bytes
        .saturating_add(prefetched_derived.artifact_bytes);
    if state
        .completed_block_prepares
        .insert(
            prefetched_derived.height,
            QueuedDerivedBlock {
                prepared: prefetched_derived.prepared,
                artifact_bytes: prefetched_derived.artifact_bytes,
                reservation: prefetched_derived.reservation,
            },
        )
        .is_some()
    {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "derived block completed twice during bulk catchup",
        }));
    }
    Ok(())
}

fn record_block_prepare_reassembly_state<F>(state: &BlockPrepareStreamState<'_, F>) {
    metrics::gauge!("zinder_ingest_block_prepare_reassembly_blocks").set(f64::from(
        usize_to_u32_saturating(state.completed_block_prepares.len()),
    ));
    record_queue_depth(
        BULK_STAGE_CANONICAL_BLOCK_PREPARE,
        state.completed_block_prepares.len(),
    );
    record_reorder_buffer(
        BULK_STAGE_CANONICAL_BLOCK_PREPARE,
        state.completed_block_prepares.len(),
        state.completed_block_prepare_bytes,
    );
}

async fn prefetch_spent_transparent_outputs(
    store: PrimaryChainStore,
    derived: &DerivedBlockArtifacts,
) -> Result<Vec<TransparentOutputArtifact>, IngestError> {
    let block_output_outpoints = derived
        .transparent_outputs_by_outpoint
        .iter()
        .map(|output| output.outpoint)
        .collect::<HashSet<TransparentOutPoint>>();
    let mut spent_outpoints = derived
        .transaction_facts
        .iter()
        .flat_map(|transaction| {
            transaction
                .transparent_inputs
                .iter()
                .map(|input| input.spent_outpoint)
        })
        .filter(|outpoint| {
            !outpoint.is_coinbase_sentinel() && !block_output_outpoints.contains(outpoint)
        })
        .collect::<Vec<_>>();
    spent_outpoints.sort_unstable_by(|left, right| {
        left.transaction_id
            .as_bytes()
            .cmp(&right.transaction_id.as_bytes())
            .then(left.output_index.cmp(&right.output_index))
    });
    spent_outpoints.dedup();
    if spent_outpoints.is_empty() {
        return Ok(Vec::new());
    }

    tokio::task::spawn_blocking(move || {
        let Some(chain_epoch) = store.current_chain_epoch()? else {
            return Ok(Vec::new());
        };
        let resolved_outputs = store
            .transparent_outputs_by_outpoints_for_writer_commit(chain_epoch, &spent_outpoints)?;
        Ok(resolved_outputs.into_values().collect())
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
}

fn prepared_block_artifact_bytes(prepared: &PreparedBlockArtifacts) -> u64 {
    derived_block_artifact_bytes(&prepared.derived).saturating_add(usize_to_u64_saturating(
        prefetched_spent_transparent_output_bytes(&prepared.prefetched_spent_transparent_outputs),
    ))
}

fn derived_block_artifact_bytes(derived: &DerivedBlockArtifacts) -> u64 {
    let block_blob_bytes = derived
        .block_blob
        .as_ref()
        .map_or(0usize, |block_blob| block_blob.raw_block_bytes.len());
    let transaction_blob_bytes =
        derived
            .transaction_blobs
            .iter()
            .fold(0usize, |bytes, transaction_blob| {
                bytes.saturating_add(transaction_blob.raw_transaction_bytes.len())
            });
    usize_to_u64_saturating(
        block_blob_bytes
            .saturating_add(derived.partial_compact_block.encoded_len())
            .saturating_add(transaction_blob_bytes),
    )
}
