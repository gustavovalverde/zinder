//! Concurrent block preparation ahead of canonical commit.
//!
//! `build_block_prepare_stream` fetches source blocks and parses them into
//! [`CanonicalBlockCommitPreparation`] with several prepares in flight at
//! once, but yields them to the caller in contiguous height order: a block
//! that finishes preparing out of turn is held until every lower height in
//! the current window has also completed, so commit reassembly never sees
//! a gap.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    mem::size_of,
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
use zinder_core::{
    BlockHeight, CanonicalBlockFacts, CanonicalTransactionFacts, TransactionBlobArtifact,
    TransparentInputFact, TransparentOutPoint, TransparentOutputArtifact, TransparentOutputFact,
    UnsupportedSection,
};
use zinder_proto::compat::lightwalletd::{
    CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
    CompactTxIn, TxOut,
};
use zinder_source::{NodeSource, SourceBlock, SourceError};
use zinder_store::{PrimaryChainStore, StoreReadCaller};

use super::{
    BULK_STAGE_CANONICAL_BLOCK_PREPARE, BULK_STAGE_CANONICAL_PREVOUT_RESOLVE, IngestError,
    record_bulk_pipeline_stage_duration, usize_to_u32_saturating, usize_to_u64_saturating,
};
use crate::artifact_builder::{PreparedCanonicalBlock, current_schema_transparent_outputs};
use crate::chain_ingest::{
    prefetched_spent_transparent_output_bytes, record_ingest_block_prepare_outcome,
};
use crate::writer::construction::{
    abort_on_drop::AbortOnDropTask,
    source_fetch::{
        CanonicalSourceFetchConfig, SourceBlockChunk, SourceSegmentSizer, build_source_block_stream,
    },
    watermark::{ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer},
};

const LIGHT_PREVOUT_COALESCE_DELAY: Duration = Duration::from_millis(2);
const DENSE_PREVOUT_COALESCE_DELAY: Duration = Duration::from_millis(20);
const DENSE_BLOCK_TRANSPARENT_INPUTS: usize = 128;
const PREVOUT_WINDOW_TARGET_TRANSPARENT_INPUTS: usize = 2_048;
// One active prepare temporarily owns the source bytes, Zebra's decoded block,
// canonical facts, replay bytes, compact artifacts, and (under `all`) two raw
// retention copies. This multiplier deliberately leaves headroom for decoded
// collection and allocator overhead. The reservation retains the larger of
// this peak and measured completed residency through prevout resolution, then
// shrinks to the resident commit-preparation handoff.
const BLOCK_PREPARE_PEAK_RAW_BYTE_MULTIPLIER: u64 = 16;
const BLOCK_PREPARE_FIXED_PEAK_BYTES: u64 = 64 * 1_024;

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
    pub(super) block_prepare_memory_watermark_bytes: NonZeroU64,
    pub(super) store: PrimaryChainStore,
}

pub(super) struct CanonicalBlockCommitPreparation {
    pub(super) prepared: PreparedCanonicalBlock,
    pub(super) prefetched_spent_transparent_outputs: Vec<TransparentOutputArtifact>,
    pub(super) block_prepare_reservation: ByteReservation,
}

pub(super) fn build_block_prepare_stream<'a, Source, F, Fut>(
    source: &'a Source,
    config: BulkCatchupBlockPrepareStreamConfig,
    prepare_fn: F,
) -> impl Stream<Item = Result<Vec<CanonicalBlockCommitPreparation>, IngestError>> + Send + 'a
where
    Source: NodeSource + Clone + 'a,
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<PreparedCanonicalBlock, IngestError>> + Send + 'static,
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
        block_prepare_memory_watermark_bytes,
        store,
    } = config;
    let block_prepare_concurrency = block_prepare_concurrency.max(1);
    let source_fetch_config = CanonicalSourceFetchConfig {
        request_timeout,
        history_predecessor: None,
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
        completed_block_prepare_resident_bytes: 0,
        pending_source_chunk: None,
        prepare_fn,
        store,
        block_prepare_concurrency,
        block_prepare_watermark: ByteWatermark::new(
            BULK_STAGE_CANONICAL_BLOCK_PREPARE,
            block_prepare_memory_watermark_bytes,
        ),
        recent_outputs: None,
        prevout_coalesce_deadline: None,
        next_emit_height: Some(from_height),
        to_height,
        source_exhausted: false,
    };

    stream::unfold(state, |mut state| async move {
        let next_chunk = next_block_prepare_chunk(&mut state).await;
        next_chunk.map(|chunk_result| (chunk_result, state))
    })
}

struct PreparedBlock {
    height: BlockHeight,
    prepared: PreparedCanonicalBlock,
    resident_bytes: u64,
    reservation: ByteReservation,
}

struct QueuedPreparedBlock {
    prepared: PreparedCanonicalBlock,
    resident_bytes: u64,
    reservation: ByteReservation,
}

enum PendingBlockPrepareSchedule {
    Scheduled,
    WatermarkBlocked,
    Idle,
}

struct BlockPrepareStreamState<'a, F> {
    source_blocks: BoxStream<'a, Result<SourceBlockChunk, IngestError>>,
    in_flight_block_prepares:
        FuturesUnordered<BoxFuture<'static, Result<PreparedBlock, IngestError>>>,
    completed_block_prepares: BTreeMap<BlockHeight, QueuedPreparedBlock>,
    completed_block_prepare_resident_bytes: u64,
    pending_source_chunk: Option<SourceBlockChunk>,
    prepare_fn: F,
    store: PrimaryChainStore,
    block_prepare_concurrency: usize,
    block_prepare_watermark: ByteWatermark,
    recent_outputs: Option<RecentTransparentOutputCache>,
    prevout_coalesce_deadline: Option<Instant>,
    next_emit_height: Option<BlockHeight>,
    to_height: BlockHeight,
    source_exhausted: bool,
}

async fn next_block_prepare_chunk<F, Fut>(
    state: &mut BlockPrepareStreamState<'_, F>,
) -> Option<Result<Vec<CanonicalBlockCommitPreparation>, IngestError>>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<PreparedCanonicalBlock, IngestError>> + Send + 'static,
{
    loop {
        let prepare_schedule_blocked = match schedule_next_pending_block_prepare(state) {
            PendingBlockPrepareSchedule::Scheduled => continue,
            PendingBlockPrepareSchedule::WatermarkBlocked => true,
            PendingBlockPrepareSchedule::Idle => false,
        };

        let (ready_block_count, ready_transparent_input_count) =
            state.contiguous_completed_window_shape();
        if ready_block_count > 0 && state.prevout_coalesce_deadline.is_none() {
            let coalesce_delay = if state.next_completed_block_is_dense() {
                DENSE_PREVOUT_COALESCE_DELAY
            } else {
                LIGHT_PREVOUT_COALESCE_DELAY
            };
            state.prevout_coalesce_deadline = Some(
                Instant::now()
                    .checked_add(coalesce_delay)
                    .unwrap_or_else(Instant::now),
            );
        }
        let coalesce_deadline_elapsed = state
            .prevout_coalesce_deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if ready_block_count > 0
            && (ready_block_count >= state.block_prepare_concurrency
                || ready_transparent_input_count >= PREVOUT_WINDOW_TARGET_TRANSPARENT_INPUTS
                || prepare_schedule_blocked
                || coalesce_deadline_elapsed
                || (state.source_exhausted && state.in_flight_block_prepares.is_empty()))
        {
            let queued_blocks = take_contiguous_completed_block_prepares(state);
            state.prevout_coalesce_deadline = None;
            record_block_prepare_reassembly_state(state);
            let prevout_resolve_started_at = Instant::now();
            let resolved = resolve_prevouts_for_window(state, queued_blocks).await;
            record_block_prepare_stage(
                "transparent_prevout_resolve",
                prevout_resolve_started_at,
                &resolved,
            );
            record_bulk_pipeline_stage_duration(
                BULK_STAGE_CANONICAL_PREVOUT_RESOLVE,
                prevout_resolve_started_at,
                resolved.as_ref().err(),
            );
            return Some(resolved);
        }

        let can_schedule_block_prepare = state.can_schedule_block_prepare();
        if !can_schedule_block_prepare && state.in_flight_block_prepares.is_empty() {
            return None;
        }

        let coalesce_deadline = state.prevout_coalesce_deadline.unwrap_or_else(Instant::now);
        tokio::select! {
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(coalesce_deadline)), if state.prevout_coalesce_deadline.is_some() => {}
            source_chunk_result = state.source_blocks.next(), if can_schedule_block_prepare => {
                match source_chunk_result {
                    Some(Ok(source_chunk)) => state.pending_source_chunk = Some(source_chunk),
                    Some(Err(error)) => return Some(Err(error)),
                    None => state.source_exhausted = true,
                }
            }
            block_prepare_result = state.in_flight_block_prepares.next(), if !state.in_flight_block_prepares.is_empty() => {
                let prepared_block = match block_prepare_result {
                    Some(Ok(prepared_block)) => prepared_block,
                    Some(Err(error)) => return Some(Err(error)),
                    None => continue,
                };
                if let Err(error) = insert_completed_block_prepare(state, prepared_block) {
                    return Some(Err(error));
                }
            }
        }
    }
}

fn schedule_next_pending_block_prepare<F, Fut>(
    state: &mut BlockPrepareStreamState<'_, F>,
) -> PendingBlockPrepareSchedule
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<PreparedCanonicalBlock, IngestError>> + Send + 'static,
{
    if state.in_flight_block_prepares.len() >= state.block_prepare_concurrency {
        return PendingBlockPrepareSchedule::Idle;
    }
    let Some(source_block) = state
        .pending_source_chunk
        .as_mut()
        .and_then(SourceBlockChunk::pop_front)
    else {
        return PendingBlockPrepareSchedule::Idle;
    };
    match schedule_block_prepare(state, source_block) {
        Ok(()) => {
            if state
                .pending_source_chunk
                .as_ref()
                .is_some_and(SourceBlockChunk::is_empty)
            {
                state.pending_source_chunk = None;
            }
            PendingBlockPrepareSchedule::Scheduled
        }
        Err(source_block) => {
            if let Some(source_chunk) = state.pending_source_chunk.as_mut() {
                source_chunk.push_front(source_block);
            }
            PendingBlockPrepareSchedule::WatermarkBlocked
        }
    }
}

fn take_contiguous_completed_block_prepares<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
) -> Vec<QueuedPreparedBlock> {
    let mut ready_blocks = Vec::new();
    let mut transparent_input_count = 0usize;
    while let Some(next_emit_height) = state.next_emit_height {
        if ready_blocks.len() >= state.block_prepare_concurrency {
            break;
        }
        if !ready_blocks.is_empty()
            && transparent_input_count >= PREVOUT_WINDOW_TARGET_TRANSPARENT_INPUTS
        {
            break;
        }
        let Some(queued) = state.completed_block_prepares.get(&next_emit_height) else {
            break;
        };
        let next_transparent_input_count = prepared_block_transparent_input_count(&queued.prepared);
        transparent_input_count =
            transparent_input_count.saturating_add(next_transparent_input_count);
        let Some(queued) = state.completed_block_prepares.remove(&next_emit_height) else {
            break;
        };
        state.next_emit_height = next_emit_height
            .next()
            .filter(|height| *height <= state.to_height);
        let resident_bytes = queued.resident_bytes;
        state.completed_block_prepare_resident_bytes = state
            .completed_block_prepare_resident_bytes
            .saturating_sub(resident_bytes);
        ready_blocks.push(queued);
    }
    ready_blocks
}

impl<F> BlockPrepareStreamState<'_, F> {
    fn contiguous_completed_window_shape(&self) -> (usize, usize) {
        let mut count = 0;
        let mut transparent_input_count = 0usize;
        let mut next_height = self.next_emit_height;
        while let Some(height) = next_height {
            let Some(queued) = self.completed_block_prepares.get(&height) else {
                break;
            };
            count += 1;
            transparent_input_count = transparent_input_count
                .saturating_add(prepared_block_transparent_input_count(&queued.prepared));
            next_height = height.next().filter(|height| *height <= self.to_height);
        }
        (count, transparent_input_count)
    }

    fn next_completed_block_is_dense(&self) -> bool {
        self.next_emit_height
            .and_then(|height| self.completed_block_prepares.get(&height))
            .is_some_and(|queued| {
                prepared_block_transparent_input_count(&queued.prepared)
                    >= DENSE_BLOCK_TRANSPARENT_INPUTS
            })
    }

    fn can_schedule_block_prepare(&self) -> bool {
        !self.source_exhausted
            && self.pending_source_chunk.is_none()
            && self.in_flight_block_prepares.len() < self.block_prepare_concurrency
            && self.completed_block_prepares.len() < self.block_prepare_concurrency
    }
}

fn schedule_block_prepare<F, Fut>(
    state: &mut BlockPrepareStreamState<'_, F>,
    source_block: SourceBlock,
) -> Result<(), SourceBlock>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<PreparedCanonicalBlock, IngestError>> + Send + 'static,
{
    if state.in_flight_block_prepares.len() >= state.block_prepare_concurrency {
        return Err(source_block);
    }
    let estimated_peak_resident_bytes =
        estimated_peak_block_prepare_bytes(source_block.raw_block_bytes.capacity());
    let Some(reservation) = state.reserve_block_prepare_bytes(estimated_peak_resident_bytes) else {
        return Err(source_block);
    };
    let prepare_fn = state.prepare_fn.clone();
    // Spawned so per-block canonical preparation progresses on runtime workers
    // instead of only while this stream is polled by the commit consumer.
    let block_prepare_task = AbortOnDropTask::spawn(async move {
        let height = source_block.height;
        let block_prepare_started_at = Instant::now();
        let block_prepare_outcome = async {
            let preparation_started_at = Instant::now();
            let preparation_outcome = prepare_fn(source_block).await;
            record_block_prepare_stage(
                "canonical_block_prepare",
                preparation_started_at,
                &preparation_outcome,
            );
            let prepared = preparation_outcome?;
            let resident_bytes = prepared_block_resident_bytes(&prepared);
            let mut reservation = reservation;
            reservation.resize(estimated_peak_resident_bytes.max(resident_bytes));
            Ok(PreparedBlock {
                height,
                prepared,
                resident_bytes,
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

impl<F> BlockPrepareStreamState<'_, F> {
    fn reserve_block_prepare_bytes(&mut self, bytes: u64) -> Option<ByteReservation> {
        loop {
            if let Some(reservation) = self.block_prepare_watermark.try_reserve(bytes) {
                return Some(reservation);
            }
            let recent_outputs = self.recent_outputs.as_mut()?;
            if !recent_outputs.evict_oldest() {
                return None;
            }
        }
    }

    fn recent_outputs(&mut self) -> &mut RecentTransparentOutputCache {
        self.recent_outputs.get_or_insert_with(|| {
            RecentTransparentOutputCache::new(self.block_prepare_watermark.clone())
        })
    }
}

fn record_block_prepare_stage<T>(
    stage: &'static str,
    started_at: Instant,
    outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_block_prepare_stage_duration_seconds",
        "stage" => stage,
        "status" => if outcome.is_ok() { "ok" } else { "error" }
    )
    .record(started_at.elapsed());
}

fn insert_completed_block_prepare<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
    prepared_block: PreparedBlock,
) -> Result<(), IngestError> {
    if prepared_block.height > state.to_height {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "prepared block completed outside the requested bulk-catchup range",
        }));
    }
    state.completed_block_prepare_resident_bytes = state
        .completed_block_prepare_resident_bytes
        .saturating_add(prepared_block.resident_bytes);
    if state
        .completed_block_prepares
        .insert(
            prepared_block.height,
            QueuedPreparedBlock {
                prepared: prepared_block.prepared,
                resident_bytes: prepared_block.resident_bytes,
                reservation: prepared_block.reservation,
            },
        )
        .is_some()
    {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "prepared block completed twice during bulk catchup",
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
        state.completed_block_prepare_resident_bytes,
    );
}

#[derive(Default)]
struct PrevoutResolutionStats {
    same_block: usize,
    same_window: usize,
    recent_cache: usize,
    store_requested: usize,
    store_resolved: usize,
}

struct CachedTransparentOutput {
    output: TransparentOutputArtifact,
    resident_bytes: u64,
    order_sequence: u128,
    _reservation: ByteReservation,
}

struct RecentTransparentOutputCache {
    outputs: HashMap<TransparentOutPoint, CachedTransparentOutput>,
    insertion_order: BTreeMap<u128, TransparentOutPoint>,
    next_order_sequence: u128,
    resident_bytes: u64,
    watermark: ByteWatermark,
}

impl RecentTransparentOutputCache {
    fn new(watermark: ByteWatermark) -> Self {
        Self {
            outputs: HashMap::new(),
            insertion_order: BTreeMap::new(),
            next_order_sequence: 0,
            resident_bytes: 0,
            watermark,
        }
    }

    fn get(&self, outpoint: &TransparentOutPoint) -> Option<&TransparentOutputArtifact> {
        self.outputs.get(outpoint).map(|cached| &cached.output)
    }

    fn remove(&mut self, outpoint: &TransparentOutPoint) -> bool {
        let Some(cached) = self.outputs.remove(outpoint) else {
            return false;
        };
        self.insertion_order.remove(&cached.order_sequence);
        self.resident_bytes = self.resident_bytes.saturating_sub(cached.resident_bytes);
        drop(cached);
        if self.outputs.is_empty() {
            self.insertion_order.clear();
            self.next_order_sequence = 0;
        }
        self.record_state();
        true
    }

    fn insert(&mut self, output: TransparentOutputArtifact) {
        if self.outputs.contains_key(&output.outpoint) {
            return;
        }
        let resident_bytes = transparent_output_cache_entry_bytes(&output);
        let reservation = loop {
            if let Some(reservation) = self.watermark.try_reserve(resident_bytes) {
                break reservation;
            }
            if !self.evict_oldest() {
                metrics::counter!(
                    "zinder_ingest_prevout_resolver_cache_admission_total",
                    "result" => "no_headroom"
                )
                .increment(1);
                return;
            }
        };
        let outpoint = output.outpoint;
        let order_sequence = self.next_order_sequence;
        self.next_order_sequence = self.next_order_sequence.saturating_add(1);
        self.resident_bytes = self.resident_bytes.saturating_add(resident_bytes);
        self.insertion_order.insert(order_sequence, outpoint);
        self.outputs.insert(
            outpoint,
            CachedTransparentOutput {
                output,
                resident_bytes,
                order_sequence,
                _reservation: reservation,
            },
        );
        metrics::counter!(
            "zinder_ingest_prevout_resolver_cache_admission_total",
            "result" => "cached"
        )
        .increment(1);
        self.record_state();
    }

    fn evict_oldest(&mut self) -> bool {
        while let Some((_order_sequence, outpoint)) = self.insertion_order.pop_first() {
            let Some(cached) = self.outputs.remove(&outpoint) else {
                continue;
            };
            self.resident_bytes = self.resident_bytes.saturating_sub(cached.resident_bytes);
            drop(cached);
            if self.outputs.is_empty() {
                self.insertion_order.clear();
                self.next_order_sequence = 0;
            }
            metrics::counter!("zinder_ingest_prevout_resolver_cache_evictions_total").increment(1);
            self.record_state();
            return true;
        }
        false
    }

    fn record_state(&self) {
        metrics::gauge!("zinder_ingest_prevout_resolver_recent_outputs")
            .set(f64::from(usize_to_u32_saturating(self.outputs.len())));
        metrics::gauge!("zinder_ingest_prevout_resolver_recent_output_bytes")
            .set(u64_to_f64(self.resident_bytes));
    }
}

impl Drop for RecentTransparentOutputCache {
    fn drop(&mut self) {
        metrics::gauge!("zinder_ingest_prevout_resolver_recent_outputs").set(0.0);
        metrics::gauge!("zinder_ingest_prevout_resolver_recent_output_bytes").set(0.0);
    }
}

async fn resolve_prevouts_for_window<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
    queued_blocks: Vec<QueuedPreparedBlock>,
) -> Result<Vec<CanonicalBlockCommitPreparation>, IngestError> {
    metrics::histogram!("zinder_ingest_prevout_resolver_window_blocks")
        .record(usize_to_u32_saturating(queued_blocks.len()));

    let mut prefetched_by_block: Vec<Vec<TransparentOutputArtifact>> =
        vec![Vec::new(); queued_blocks.len()];
    let mut window_output_locations: HashMap<TransparentOutPoint, (usize, usize)> = HashMap::new();
    let mut created_outputs_spent = HashSet::new();
    let mut cold_consumers = HashMap::<TransparentOutPoint, Vec<usize>>::new();
    let mut resolution_stats = PrevoutResolutionStats::default();
    let created_outputs_by_block = queued_blocks
        .iter()
        .map(|queued| current_schema_transparent_outputs(&queued.prepared.facts))
        .collect::<Vec<_>>();

    for (block_index, queued) in queued_blocks.iter().enumerate() {
        let same_block_outputs = created_outputs_by_block[block_index]
            .iter()
            .map(|output| output.outpoint)
            .collect::<HashSet<_>>();
        for spent_outpoint in spent_outpoints_for_prepared_block(&queued.prepared) {
            if same_block_outputs.contains(&spent_outpoint) {
                created_outputs_spent.insert(spent_outpoint);
                resolution_stats.same_block = resolution_stats.same_block.saturating_add(1);
                continue;
            }
            if let Some(&(producer_block_index, producer_output_index)) =
                window_output_locations.get(&spent_outpoint)
            {
                let output =
                    created_outputs_by_block[producer_block_index][producer_output_index].clone();
                prefetched_by_block[block_index].push(output);
                created_outputs_spent.insert(spent_outpoint);
                resolution_stats.same_window = resolution_stats.same_window.saturating_add(1);
                continue;
            }
            let cached_output = state.recent_outputs().get(&spent_outpoint).cloned();
            if let Some(output) = cached_output {
                prefetched_by_block[block_index].push(output);
                state.recent_outputs().remove(&spent_outpoint);
                resolution_stats.recent_cache = resolution_stats.recent_cache.saturating_add(1);
                continue;
            }
            cold_consumers
                .entry(spent_outpoint)
                .or_default()
                .push(block_index);
        }

        for (output_index, output) in created_outputs_by_block[block_index].iter().enumerate() {
            window_output_locations.insert(output.outpoint, (block_index, output_index));
        }
    }
    drop(window_output_locations);

    let mut cold_outpoints = cold_consumers.keys().copied().collect::<Vec<_>>();
    sort_outpoints(&mut cold_outpoints);
    resolution_stats.store_requested = cold_outpoints.len();
    let resolved_store_outputs = resolve_cold_prevouts(state.store.clone(), cold_outpoints).await?;
    resolution_stats.store_resolved = resolved_store_outputs.len();

    for (outpoint, consumers) in cold_consumers {
        let Some(output) = resolved_store_outputs.get(&outpoint) else {
            continue;
        };
        for block_index in consumers {
            prefetched_by_block[block_index].push(output.clone());
        }
    }
    drop(resolved_store_outputs);

    let prepared_blocks = prepare_resolved_window(
        state,
        queued_blocks,
        prefetched_by_block,
        created_outputs_by_block,
        &created_outputs_spent,
    );
    record_prevout_resolution_stats(&resolution_stats);
    Ok(prepared_blocks)
}

fn prepare_resolved_window<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
    queued_blocks: Vec<QueuedPreparedBlock>,
    prefetched_by_block: Vec<Vec<TransparentOutputArtifact>>,
    created_outputs_by_block: Vec<Vec<TransparentOutputArtifact>>,
    created_outputs_spent: &HashSet<TransparentOutPoint>,
) -> Vec<CanonicalBlockCommitPreparation> {
    let mut prepared_blocks = queued_blocks
        .into_iter()
        .zip(prefetched_by_block)
        .map(|(queued, mut prefetched_spent_transparent_outputs)| {
            sort_outputs(&mut prefetched_spent_transparent_outputs);
            CanonicalBlockCommitPreparation {
                prepared: queued.prepared,
                prefetched_spent_transparent_outputs,
                block_prepare_reservation: queued.reservation,
            }
        })
        .collect::<Vec<_>>();

    for output in created_outputs_by_block.into_iter().flatten() {
        if !created_outputs_spent.contains(&output.outpoint) {
            state.recent_outputs().insert(output);
        }
    }
    for prepared in &mut prepared_blocks {
        let resident_bytes = block_commit_preparation_resident_bytes(prepared);
        prepared.block_prepare_reservation.resize(resident_bytes);
    }
    prepared_blocks
}

async fn resolve_cold_prevouts(
    store: PrimaryChainStore,
    cold_outpoints: Vec<TransparentOutPoint>,
) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, IngestError> {
    if cold_outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    metrics::counter!("zinder_ingest_prevout_resolver_store_lookups_total").increment(1);
    tokio::task::spawn_blocking(move || {
        let Some(chain_epoch) = store.current_chain_epoch()? else {
            return Ok(HashMap::new());
        };
        store
            .transparent_outputs_by_outpoints_for_writer_commit(
                StoreReadCaller::BlockPrefetch,
                chain_epoch,
                &cold_outpoints,
            )
            .map_err(IngestError::from)
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
}

fn spent_outpoints_for_prepared_block(
    prepared: &PreparedCanonicalBlock,
) -> Vec<TransparentOutPoint> {
    let mut spent_outpoints = prepared
        .facts
        .transactions
        .iter()
        .flat_map(|transaction| {
            transaction
                .transparent_inputs
                .iter()
                .map(|input| input.spent_outpoint)
        })
        .filter(|outpoint| !outpoint.is_coinbase_sentinel())
        .collect::<Vec<_>>();
    sort_outpoints(&mut spent_outpoints);
    spent_outpoints.dedup();
    spent_outpoints
}

fn prepared_block_transparent_input_count(prepared: &PreparedCanonicalBlock) -> usize {
    prepared
        .facts
        .transactions
        .iter()
        .fold(0usize, |count, transaction| {
            count.saturating_add(transaction.transparent_inputs.len())
        })
}

fn sort_outpoints(outpoints: &mut [TransparentOutPoint]) {
    outpoints.sort_unstable_by(|left, right| {
        left.transaction_id
            .as_bytes()
            .cmp(&right.transaction_id.as_bytes())
            .then(left.output_index.cmp(&right.output_index))
    });
}

fn sort_outputs(outputs: &mut [TransparentOutputArtifact]) {
    outputs.sort_unstable_by(|left, right| {
        left.outpoint
            .transaction_id
            .as_bytes()
            .cmp(&right.outpoint.transaction_id.as_bytes())
            .then(left.outpoint.output_index.cmp(&right.outpoint.output_index))
    });
}

fn transparent_output_cache_entry_bytes(output: &TransparentOutputArtifact) -> u64 {
    let structural_bytes = size_of::<TransparentOutputArtifact>()
        .saturating_add(size_of::<TransparentOutPoint>())
        .saturating_add(size_of::<CachedTransparentOutput>())
        .saturating_add(size_of::<u128>())
        .saturating_add(size_of::<TransparentOutPoint>());
    usize_to_u64_saturating(structural_bytes.saturating_add(output.script_pub_key.capacity()))
}

fn record_prevout_resolution_stats(stats: &PrevoutResolutionStats) {
    for (source, count) in [
        ("same_block", stats.same_block),
        ("same_window", stats.same_window),
        ("recent_cache", stats.recent_cache),
        ("store_requested", stats.store_requested),
        ("store_resolved", stats.store_resolved),
        (
            "store_missing",
            stats.store_requested.saturating_sub(stats.store_resolved),
        ),
    ] {
        metrics::counter!(
            "zinder_ingest_prevout_resolver_outpoints_total",
            "source" => source
        )
        .increment(usize_to_u64_saturating(count));
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; cache byte counts are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

fn estimated_peak_block_prepare_bytes(raw_block_capacity: usize) -> u64 {
    BLOCK_PREPARE_FIXED_PEAK_BYTES.saturating_add(
        usize_to_u64_saturating(raw_block_capacity.max(1))
            .saturating_mul(BLOCK_PREPARE_PEAK_RAW_BYTE_MULTIPLIER),
    )
}

fn block_commit_preparation_resident_bytes(prepared: &CanonicalBlockCommitPreparation) -> u64 {
    prepared_block_resident_bytes(&prepared.prepared).saturating_add(usize_to_u64_saturating(
        prefetched_spent_transparent_output_bytes(&prepared.prefetched_spent_transparent_outputs),
    ))
}

fn prepared_block_resident_bytes(prepared: &PreparedCanonicalBlock) -> u64 {
    let resident_bytes = size_of::<PreparedCanonicalBlock>()
        .saturating_add(canonical_block_facts_heap_bytes(&prepared.facts))
        .saturating_add(prepared.replay_envelope.as_bytes().len())
        .saturating_add(compact_block_heap_bytes(&prepared.partial_compact_block))
        .saturating_add(retained_raw_blob_heap_bytes(prepared));
    usize_to_u64_saturating(resident_bytes)
}

fn canonical_block_facts_heap_bytes(facts: &CanonicalBlockFacts) -> usize {
    let mut resident_bytes =
        vector_allocation_bytes::<CanonicalTransactionFacts>(facts.transactions.capacity());
    for transaction in &facts.transactions {
        resident_bytes = resident_bytes
            .saturating_add(vector_allocation_bytes::<UnsupportedSection>(
                transaction.public_facts.unsupported_sections.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<TransparentInputFact>(
                transaction.transparent_inputs.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<TransparentOutputFact>(
                transaction.transparent_outputs.capacity(),
            ));
        for output in &transaction.transparent_outputs {
            resident_bytes = resident_bytes.saturating_add(output.script_pub_key.capacity());
        }
    }
    resident_bytes
}

fn compact_block_heap_bytes(block: &CompactBlock) -> usize {
    let mut resident_bytes = block
        .hash
        .capacity()
        .saturating_add(block.prev_hash.capacity())
        .saturating_add(block.header.capacity())
        .saturating_add(vector_allocation_bytes::<CompactTx>(block.vtx.capacity()));
    for transaction in &block.vtx {
        resident_bytes = resident_bytes
            .saturating_add(transaction.txid.capacity())
            .saturating_add(vector_allocation_bytes::<CompactSaplingSpend>(
                transaction.spends.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<CompactSaplingOutput>(
                transaction.outputs.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<CompactOrchardAction>(
                transaction.actions.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<CompactOrchardAction>(
                transaction.ironwood_actions.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<CompactTxIn>(
                transaction.vin.capacity(),
            ))
            .saturating_add(vector_allocation_bytes::<TxOut>(
                transaction.vout.capacity(),
            ));
        for spend in &transaction.spends {
            resident_bytes = resident_bytes.saturating_add(spend.nf.capacity());
        }
        for output in &transaction.outputs {
            resident_bytes = resident_bytes
                .saturating_add(output.cmu.capacity())
                .saturating_add(output.ephemeral_key.capacity())
                .saturating_add(output.ciphertext.capacity());
        }
        for action in transaction
            .actions
            .iter()
            .chain(&transaction.ironwood_actions)
        {
            resident_bytes = resident_bytes
                .saturating_add(action.nullifier.capacity())
                .saturating_add(action.cmx.capacity())
                .saturating_add(action.ephemeral_key.capacity())
                .saturating_add(action.ciphertext.capacity());
        }
        for input in &transaction.vin {
            resident_bytes = resident_bytes.saturating_add(input.prevout_txid.capacity());
        }
        for output in &transaction.vout {
            resident_bytes = resident_bytes.saturating_add(output.script_pub_key.capacity());
        }
    }
    resident_bytes
}

fn retained_raw_blob_heap_bytes(prepared: &PreparedCanonicalBlock) -> usize {
    let block_blob_bytes = prepared
        .retained_raw_blobs
        .block_blob
        .as_ref()
        .map_or(0usize, |blob| blob.raw_block_bytes.capacity());
    prepared.retained_raw_blobs.transaction_blobs.iter().fold(
        block_blob_bytes.saturating_add(vector_allocation_bytes::<TransactionBlobArtifact>(
            prepared.retained_raw_blobs.transaction_blobs.capacity(),
        )),
        |resident_bytes, transaction_blob| {
            resident_bytes.saturating_add(transaction_blob.raw_transaction_bytes.capacity())
        },
    )
}

fn vector_allocation_bytes<T>(capacity: usize) -> usize {
    capacity.saturating_mul(size_of::<T>())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet},
        error::Error,
        num::NonZeroU64,
        sync::Arc,
    };

    use futures_util::{StreamExt as _, stream};
    use serde_json::Value;
    use zinder_core::{BlockHeight, Network};
    use zinder_source::SourceBlock;
    use zinder_store::{ChainStoreOptions, PrimaryChainStore};
    use zinder_testkit::sample_regtest_upgrade_activations;

    use super::{
        BLOCK_PREPARE_FIXED_PEAK_BYTES, BLOCK_PREPARE_PEAK_RAW_BYTE_MULTIPLIER,
        BlockPrepareStreamState, ByteWatermark, FuturesUnordered, IngestError, QueuedPreparedBlock,
        block_commit_preparation_resident_bytes, canonical_block_facts_heap_bytes,
        compact_block_heap_bytes, current_schema_transparent_outputs,
        estimated_peak_block_prepare_bytes, prepare_resolved_window, prepared_block_resident_bytes,
        retained_raw_blob_heap_bytes, schedule_block_prepare,
    };
    use crate::artifact_builder::{RawBlobPolicy, prepare_canonical_block};

    #[test]
    fn peak_estimate_scales_from_raw_capacity_with_fixed_headroom() {
        assert_eq!(
            estimated_peak_block_prepare_bytes(0),
            BLOCK_PREPARE_FIXED_PEAK_BYTES + BLOCK_PREPARE_PEAK_RAW_BYTE_MULTIPLIER
        );
        assert_eq!(
            estimated_peak_block_prepare_bytes(1_024),
            BLOCK_PREPARE_FIXED_PEAK_BYTES + (1_024 * BLOCK_PREPARE_PEAK_RAW_BYTE_MULTIPLIER)
        );
    }

    #[test]
    fn peak_estimate_covers_fixture_prepare_residency_across_blob_policies()
    -> Result<(), Box<dyn Error>> {
        let source_block = regtest_fixture_block()?;
        let activations = sample_regtest_upgrade_activations();
        let estimated_peak_bytes =
            estimated_peak_block_prepare_bytes(source_block.raw_block_bytes.capacity());
        let no_blobs = prepare_canonical_block(&source_block, &activations, RawBlobPolicy::None)?;
        let transaction_blobs =
            prepare_canonical_block(&source_block, &activations, RawBlobPolicy::Transactions)?;
        let all_blobs = prepare_canonical_block(&source_block, &activations, RawBlobPolicy::All)?;

        let no_blob_resident_bytes = prepared_block_resident_bytes(&no_blobs);
        let transaction_blob_resident_bytes = prepared_block_resident_bytes(&transaction_blobs);
        let all_blob_resident_bytes = prepared_block_resident_bytes(&all_blobs);

        assert!(canonical_block_facts_heap_bytes(&no_blobs.facts) > 0);
        assert!(compact_block_heap_bytes(&no_blobs.partial_compact_block) > 0);
        assert!(!no_blobs.replay_envelope.as_bytes().is_empty());
        assert_eq!(retained_raw_blob_heap_bytes(&no_blobs), 0);
        assert!(transaction_blob_resident_bytes > no_blob_resident_bytes);
        assert!(all_blob_resident_bytes > transaction_blob_resident_bytes);
        assert!(estimated_peak_bytes >= all_blob_resident_bytes);
        Ok(())
    }

    #[test]
    fn resolved_window_reservation_covers_commit_handoff_and_cache_ownership()
    -> Result<(), Box<dyn Error>> {
        let source_block = regtest_fixture_block()?;
        let prepared = prepare_canonical_block(
            &source_block,
            &sample_regtest_upgrade_activations(),
            RawBlobPolicy::All,
        )?;
        let created_outputs = current_schema_transparent_outputs(&prepared.facts);
        assert!(!created_outputs.is_empty());

        let prepared_resident_bytes = prepared_block_resident_bytes(&prepared);
        let prepared_peak_bytes =
            estimated_peak_block_prepare_bytes(source_block.raw_block_bytes.capacity())
                .max(prepared_resident_bytes);
        let watermark = ByteWatermark::new(
            "test_block_prepare",
            NonZeroU64::new(64 * 1_024 * 1_024).ok_or("invalid test watermark")?,
        );
        let reservation = watermark
            .try_reserve(prepared_peak_bytes)
            .ok_or("prepared block reservation should fit")?;
        let tempdir = tempfile::tempdir()?;
        let store = PrimaryChainStore::open(
            tempdir.path(),
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut state = test_block_prepare_state((), watermark.clone(), store, source_block.height);
        let resolved_blocks = prepare_resolved_window(
            &mut state,
            vec![QueuedPreparedBlock {
                prepared,
                resident_bytes: prepared_resident_bytes,
                reservation,
            }],
            vec![Vec::new()],
            vec![created_outputs],
            &HashSet::new(),
        );
        let resolved_resident_bytes = block_commit_preparation_resident_bytes(&resolved_blocks[0]);
        let cache = state
            .recent_outputs
            .as_mut()
            .ok_or("created outputs should populate the recent-output cache")?;
        let cache_resident_bytes = cache.resident_bytes;

        assert_eq!(
            watermark.snapshot().reserved_bytes,
            resolved_resident_bytes.saturating_add(cache_resident_bytes)
        );

        drop(resolved_blocks);
        assert_eq!(watermark.snapshot().reserved_bytes, cache_resident_bytes);

        while cache.evict_oldest() {}
        assert_eq!(watermark.snapshot().reserved_bytes, 0);
        Ok(())
    }

    #[tokio::test]
    async fn completed_prepare_retains_admission_peak_until_prevout_resolution()
    -> Result<(), Box<dyn Error>> {
        let source_block = regtest_fixture_block()?;
        let prepared_peak_bytes =
            estimated_peak_block_prepare_bytes(source_block.raw_block_bytes.capacity());
        let watermark = ByteWatermark::new(
            "test_block_prepare",
            NonZeroU64::new(64 * 1_024 * 1_024).ok_or("invalid test watermark")?,
        );
        let tempdir = tempfile::tempdir()?;
        let store = PrimaryChainStore::open(
            tempdir.path(),
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let activations = Arc::new(sample_regtest_upgrade_activations());
        let prepare_fn = move |source_block: SourceBlock| {
            let activations = Arc::clone(&activations);
            async move {
                prepare_canonical_block(&source_block, &activations, RawBlobPolicy::None)
                    .map_err(IngestError::from)
            }
        };
        let mut state =
            test_block_prepare_state(prepare_fn, watermark.clone(), store, source_block.height);

        schedule_block_prepare(&mut state, source_block)
            .map_err(|_| "block prepare should be admitted")?;
        assert_eq!(watermark.snapshot().reserved_bytes, prepared_peak_bytes);

        let prepared_block = state
            .in_flight_block_prepares
            .next()
            .await
            .ok_or("scheduled block prepare should complete")??;
        assert_eq!(
            watermark.snapshot().reserved_bytes,
            prepared_peak_bytes.max(prepared_block.resident_bytes)
        );

        drop(prepared_block);
        assert_eq!(watermark.snapshot().reserved_bytes, 0);
        Ok(())
    }

    fn test_block_prepare_state<F>(
        prepare_fn: F,
        watermark: ByteWatermark,
        store: PrimaryChainStore,
        to_height: BlockHeight,
    ) -> BlockPrepareStreamState<'static, F> {
        BlockPrepareStreamState {
            source_blocks: stream::empty().boxed(),
            in_flight_block_prepares: FuturesUnordered::new(),
            completed_block_prepares: BTreeMap::default(),
            completed_block_prepare_resident_bytes: 0,
            pending_source_chunk: None,
            prepare_fn,
            store,
            block_prepare_concurrency: 1,
            block_prepare_watermark: watermark,
            recent_outputs: None,
            prevout_coalesce_deadline: None,
            next_emit_height: None,
            to_height,
            source_exhausted: true,
        }
    }

    fn regtest_fixture_block() -> Result<SourceBlock, Box<dyn Error>> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/z3-regtest-block-1.json"))?;
        let raw_block_hex = fixture
            .get("raw_block_hex")
            .and_then(Value::as_str)
            .ok_or("fixture raw_block_hex is missing")?;
        let raw_block_bytes = hex::decode(raw_block_hex)?;
        let height = fixture
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|height| u32::try_from(height).ok())
            .ok_or("fixture height is missing")?;
        Ok(SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(height),
            raw_block_bytes,
        )?)
    }
}
