use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
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
use prost::Message as _;
use zinder_core::{BlockHeight, TransparentOutPoint, TransparentOutputArtifact};
use zinder_source::{NodeSource, SourceBlock, SourceError};
use zinder_store::{PrimaryChainStore, StoreReadCaller};

use super::abort_on_drop::AbortOnDropTask;
use super::source_fetch::{
    BulkCatchupSourceFetchStreamConfig, SourceSegmentSizer, build_source_block_stream,
};
use super::watermark::{ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer};
use super::{
    BULK_STAGE_CANONICAL_BLOCK_PREPARE, BULK_STAGE_CANONICAL_PREVOUT_RESOLVE, IngestError,
    record_bulk_pipeline_stage_duration, usize_to_u32_saturating, usize_to_u64_saturating,
};
use crate::artifact_builder::DerivedBlockArtifacts;
use crate::chain_ingest::{
    prefetched_spent_transparent_output_bytes, record_ingest_block_prepare_outcome,
};

const LIGHT_PREVOUT_COALESCE_DELAY: Duration = Duration::from_millis(2);
const DENSE_PREVOUT_COALESCE_DELAY: Duration = Duration::from_millis(20);
const DENSE_BLOCK_TRANSPARENT_INPUTS: usize = 128;
const PREVOUT_WINDOW_TARGET_TRANSPARENT_INPUTS: usize = 2_048;

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
) -> impl Stream<Item = Result<Vec<PreparedBlockArtifacts>, IngestError>> + Send + 'a
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

struct DerivedBlock {
    height: BlockHeight,
    derived: DerivedBlockArtifacts,
    artifact_bytes: u64,
    reservation: ByteReservation,
}

struct QueuedDerivedBlock {
    derived: DerivedBlockArtifacts,
    artifact_bytes: u64,
    reservation: ByteReservation,
}

enum PendingBlockPrepareSchedule {
    Scheduled,
    WatermarkBlocked,
    Idle,
}

struct BlockPrepareStreamState<'a, F> {
    source_blocks: BoxStream<'a, Result<Vec<SourceBlock>, IngestError>>,
    in_flight_block_prepares:
        FuturesUnordered<BoxFuture<'static, Result<DerivedBlock, IngestError>>>,
    completed_block_prepares: BTreeMap<BlockHeight, QueuedDerivedBlock>,
    completed_block_prepare_bytes: u64,
    pending_source_blocks: VecDeque<SourceBlock>,
    derive_fn: F,
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
) -> Option<Result<Vec<PreparedBlockArtifacts>, IngestError>>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
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
                    Some(Ok(source_chunk)) => state.pending_source_blocks.extend(source_chunk),
                    Some(Err(error)) => return Some(Err(error)),
                    None => state.source_exhausted = true,
                }
            }
            block_prepare_result = state.in_flight_block_prepares.next(), if !state.in_flight_block_prepares.is_empty() => {
                let derived_block = match block_prepare_result {
                    Some(Ok(derived_block)) => derived_block,
                    Some(Err(error)) => return Some(Err(error)),
                    None => continue,
                };
                if let Err(error) = insert_completed_block_prepare(state, derived_block) {
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
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
{
    if state.in_flight_block_prepares.len() >= state.block_prepare_concurrency {
        return PendingBlockPrepareSchedule::Idle;
    }
    let Some(source_block) = state.pending_source_blocks.pop_front() else {
        return PendingBlockPrepareSchedule::Idle;
    };
    match schedule_block_prepare(state, source_block) {
        Ok(()) => PendingBlockPrepareSchedule::Scheduled,
        Err(source_block) => {
            state.pending_source_blocks.push_front(source_block);
            PendingBlockPrepareSchedule::WatermarkBlocked
        }
    }
}

fn take_contiguous_completed_block_prepares<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
) -> Vec<QueuedDerivedBlock> {
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
        let next_transparent_input_count = derived_block_transparent_input_count(&queued.derived);
        transparent_input_count =
            transparent_input_count.saturating_add(next_transparent_input_count);
        let Some(queued) = state.completed_block_prepares.remove(&next_emit_height) else {
            break;
        };
        state.next_emit_height = next_emit_height
            .next()
            .filter(|height| *height <= state.to_height);
        let artifact_bytes = queued.artifact_bytes;
        state.completed_block_prepare_bytes = state
            .completed_block_prepare_bytes
            .saturating_sub(artifact_bytes);
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
                .saturating_add(derived_block_transparent_input_count(&queued.derived));
            next_height = height.next().filter(|height| *height <= self.to_height);
        }
        (count, transparent_input_count)
    }

    fn next_completed_block_is_dense(&self) -> bool {
        self.next_emit_height
            .and_then(|height| self.completed_block_prepares.get(&height))
            .is_some_and(|queued| {
                derived_block_transparent_input_count(&queued.derived)
                    >= DENSE_BLOCK_TRANSPARENT_INPUTS
            })
    }

    fn can_schedule_block_prepare(&self) -> bool {
        !self.source_exhausted
            && self.pending_source_blocks.is_empty()
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
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'static,
{
    if state.in_flight_block_prepares.len() >= state.block_prepare_concurrency {
        return Err(source_block);
    }
    let estimated_artifact_bytes = source_block.raw_block_bytes.len().max(1);
    let reservation_bytes = usize_to_u64_saturating(estimated_artifact_bytes);
    let Some(reservation) = state.reserve_block_prepare_bytes(reservation_bytes) else {
        return Err(source_block);
    };
    let derive_fn = state.derive_fn.clone();
    // Spawned so per-block artifact derivation progresses on runtime workers
    // instead of only while this stream is polled by the commit consumer.
    let block_prepare_task = AbortOnDropTask::spawn(async move {
        let height = source_block.height;
        let block_prepare_started_at = Instant::now();
        let block_prepare_outcome = async {
            let artifact_derive_started_at = Instant::now();
            let artifact_derive_outcome = derive_fn(source_block).await;
            record_block_prepare_stage(
                "artifact_derive",
                artifact_derive_started_at,
                &artifact_derive_outcome,
            );
            let derived = artifact_derive_outcome?;
            let artifact_bytes = derived_block_artifact_bytes(&derived);
            let mut reservation = reservation;
            reservation.resize(artifact_bytes);
            Ok(DerivedBlock {
                height,
                derived,
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
    derived_block: DerivedBlock,
) -> Result<(), IngestError> {
    if derived_block.height > state.to_height {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "derived block completed outside the requested bulk-catchup range",
        }));
    }
    state.completed_block_prepare_bytes = state
        .completed_block_prepare_bytes
        .saturating_add(derived_block.artifact_bytes);
    if state
        .completed_block_prepares
        .insert(
            derived_block.height,
            QueuedDerivedBlock {
                derived: derived_block.derived,
                artifact_bytes: derived_block.artifact_bytes,
                reservation: derived_block.reservation,
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
    queued_blocks: Vec<QueuedDerivedBlock>,
) -> Result<Vec<PreparedBlockArtifacts>, IngestError> {
    metrics::histogram!("zinder_ingest_prevout_resolver_window_blocks")
        .record(usize_to_u32_saturating(queued_blocks.len()));

    let mut prefetched_by_block: Vec<Vec<TransparentOutputArtifact>> =
        vec![Vec::new(); queued_blocks.len()];
    let mut window_output_locations: HashMap<TransparentOutPoint, (usize, usize)> = HashMap::new();
    let mut created_outputs_spent = HashSet::new();
    let mut cold_consumers = HashMap::<TransparentOutPoint, Vec<usize>>::new();
    let mut resolution_stats = PrevoutResolutionStats::default();

    for (block_index, queued) in queued_blocks.iter().enumerate() {
        let same_block_outputs = queued
            .derived
            .transparent_outputs_by_outpoint
            .iter()
            .map(|output| output.outpoint)
            .collect::<HashSet<_>>();
        for spent_outpoint in spent_outpoints_for_derived_block(&queued.derived) {
            if same_block_outputs.contains(&spent_outpoint) {
                created_outputs_spent.insert(spent_outpoint);
                resolution_stats.same_block = resolution_stats.same_block.saturating_add(1);
                continue;
            }
            if let Some(&(producer_block_index, producer_output_index)) =
                window_output_locations.get(&spent_outpoint)
            {
                let output = queued_blocks[producer_block_index]
                    .derived
                    .transparent_outputs_by_outpoint[producer_output_index]
                    .clone();
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

        for (output_index, output) in queued
            .derived
            .transparent_outputs_by_outpoint
            .iter()
            .enumerate()
        {
            window_output_locations.insert(output.outpoint, (block_index, output_index));
        }
    }

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

    let prepared_blocks = prepare_resolved_window(
        state,
        queued_blocks,
        prefetched_by_block,
        &created_outputs_spent,
    );
    record_prevout_resolution_stats(&resolution_stats);
    Ok(prepared_blocks)
}

fn prepare_resolved_window<F>(
    state: &mut BlockPrepareStreamState<'_, F>,
    queued_blocks: Vec<QueuedDerivedBlock>,
    prefetched_by_block: Vec<Vec<TransparentOutputArtifact>>,
    created_outputs_spent: &HashSet<TransparentOutPoint>,
) -> Vec<PreparedBlockArtifacts> {
    let outputs_to_cache = queued_blocks
        .iter()
        .flat_map(|queued| queued.derived.transparent_outputs_by_outpoint.iter())
        .filter(|output| !created_outputs_spent.contains(&output.outpoint))
        .cloned()
        .collect::<Vec<_>>();
    let prepared_blocks = queued_blocks
        .into_iter()
        .zip(prefetched_by_block)
        .map(|(queued, mut prefetched_spent_transparent_outputs)| {
            sort_outputs(&mut prefetched_spent_transparent_outputs);
            let prepared = PreparedBlockArtifacts {
                derived: queued.derived,
                prefetched_spent_transparent_outputs,
            };
            let mut reservation = queued.reservation;
            reservation.resize(prepared_block_artifact_bytes(&prepared));
            drop(reservation);
            prepared
        })
        .collect::<Vec<_>>();

    for output in outputs_to_cache {
        state.recent_outputs().insert(output);
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

fn spent_outpoints_for_derived_block(derived: &DerivedBlockArtifacts) -> Vec<TransparentOutPoint> {
    let mut spent_outpoints = derived
        .transaction_facts
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

fn derived_block_transparent_input_count(derived: &DerivedBlockArtifacts) -> usize {
    derived
        .transaction_facts
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
