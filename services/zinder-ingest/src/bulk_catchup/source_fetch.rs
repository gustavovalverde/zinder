use std::{
    collections::{BTreeMap, VecDeque},
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{
    FutureExt,
    future::BoxFuture,
    stream::{self, FuturesUnordered, Stream, StreamExt},
};
use parking_lot::Mutex;
use zinder_core::{BlockHeight, BlockId, ConsensusBranchId, NetworkUpgradeActivations};
use zinder_source::{
    NodeSource, SourceBlock, SourceChainCursor, SourceChainSegment, SourceChainSegmentLimits,
    SourceChainSegmentStats, SourceChainUpdate, SourceError,
};

use super::fact_build::BulkCatchupFactBuildStreamConfig;
use super::watermark::{ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer};
use super::{
    BULK_STAGE_SOURCE_FETCH, IngestError, SOURCE_SEGMENT_DENSITY_SAMPLE_LIMIT,
    SOURCE_SEGMENT_GROW_AFTER_SUCCESS_COUNT, SOURCE_SEGMENT_GROW_DENOMINATOR,
    SOURCE_SEGMENT_GROW_NUMERATOR, nonzero_u32, nonzero_u32_to_usize,
    record_source_segment_sizer_adjustment, record_source_segment_sizer_state, u64_to_f64,
    usize_to_u32_saturating,
};
use crate::chain_ingest::{IngestRetryState, fetch_chain_segment_with_retry};

struct SourceBlockStreamState<'a, Source> {
    source: &'a Source,
    request_timeout: Duration,
    from_height: BlockHeight,
    to_height: BlockHeight,
    source_segment_sizer: Arc<Mutex<SourceSegmentSizer>>,
    max_response_bytes: NonZeroU64,
    target_response_payload_bytes: NonZeroU64,
    source_fetch_max_in_flight_requests: NonZeroU32,
    source_fetch_watermark: ByteWatermark,
    completed_segment_bytes: u64,
    source_head_of_line_started_at: Option<Instant>,
    next_fetch_height: Option<BlockHeight>,
    next_emit_height: Option<BlockHeight>,
    in_flight_segments:
        FuturesUnordered<BoxFuture<'a, Result<PrefetchedSourceSegment, IngestError>>>,
    completed_segments: BTreeMap<BlockHeight, PrefetchedSourceSegment>,
    pending_blocks: VecDeque<SourceBlock>,
    last_connected_block_id: Option<BlockId>,
}

pub(super) fn build_source_block_stream<'a, Source>(
    source: &'a Source,
    config: BulkCatchupFactBuildStreamConfig,
) -> impl Stream<Item = Result<SourceBlock, IngestError>> + Send + 'a
where
    Source: NodeSource + 'a,
{
    let state = SourceBlockStreamState {
        source,
        request_timeout: config.request_timeout,
        from_height: config.from_height,
        to_height: config.to_height,
        source_segment_sizer: config.source_segment_sizer,
        max_response_bytes: config.max_response_bytes,
        target_response_payload_bytes: config.target_response_payload_bytes,
        source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
        source_fetch_watermark: ByteWatermark::new(
            BULK_STAGE_SOURCE_FETCH,
            config.source_fetch_max_in_flight_bytes,
        ),
        completed_segment_bytes: 0,
        source_head_of_line_started_at: None,
        next_fetch_height: Some(config.from_height),
        next_emit_height: Some(config.from_height),
        in_flight_segments: FuturesUnordered::new(),
        completed_segments: BTreeMap::new(),
        pending_blocks: VecDeque::new(),
        last_connected_block_id: None,
    };

    stream::unfold(state, |mut state| async move {
        let next_block = next_source_block_from_segment(&mut state).await;
        next_block.map(|block_result| (block_result, state))
    })
}

async fn next_source_block_from_segment<Source>(
    state: &mut SourceBlockStreamState<'_, Source>,
) -> Option<Result<SourceBlock, IngestError>>
where
    Source: NodeSource,
{
    loop {
        if let Some(block) = state.pending_blocks.pop_front() {
            return Some(Ok(block));
        }

        fill_source_segment_prefetch_queue(state);
        if let Some(prefetched_segment) = pop_next_completed_source_segment(state) {
            let segment = prefetched_segment.segment;
            if segment.is_empty() {
                return None;
            }

            state
                .source_segment_sizer
                .lock()
                .record_segment(segment.stats());
            if let Err(error) = enqueue_source_segment_max_blocks(state, segment) {
                return Some(Err(error));
            }
            continue;
        }
        record_source_head_of_line_wait_started(state);

        let prefetched_segment = match state.in_flight_segments.next().await {
            Some(Ok(prefetched_segment)) => prefetched_segment,
            Some(Err(error)) => {
                return Some(Err(error));
            }
            None => return None,
        };
        if let Err(error) = insert_completed_source_segment(state, prefetched_segment) {
            return Some(Err(error));
        }
        record_source_fetch_queue_state(state);
    }
}

fn fill_source_segment_prefetch_queue<'a, Source>(state: &mut SourceBlockStreamState<'a, Source>)
where
    Source: NodeSource + 'a,
{
    while state.in_flight_segments.len()
        < nonzero_u32_to_usize(state.source_fetch_max_in_flight_requests)
    {
        let Some(next_height) = state.next_fetch_height else {
            break;
        };
        let Some(source_segment_max_blocks) = state
            .source_segment_sizer
            .lock()
            .blocks_for_remaining_range(next_height, state.to_height)
        else {
            state.next_fetch_height = None;
            break;
        };

        let cursor = SourceChainCursor::before_height(next_height);
        let reserved_response_bytes = state
            .target_response_payload_bytes
            .get()
            .min(state.max_response_bytes.get());
        let Some(reservation) = state
            .source_fetch_watermark
            .try_reserve(reserved_response_bytes)
        else {
            break;
        };
        state
            .in_flight_segments
            .push(fetch_prefetched_chain_segment(SourceFetchRequest {
                request_timeout: state.request_timeout,
                source: state.source,
                start_height: next_height,
                cursor,
                max_connected_blocks: source_segment_max_blocks,
                target_response_bytes: state.target_response_payload_bytes,
                max_response_bytes: state.max_response_bytes,
                reserved_response_bytes,
                reservation,
            }));
        record_source_fetch_queue_state(state);
        state.next_fetch_height = next_height_after_segment(next_height, source_segment_max_blocks)
            .filter(|height| *height <= state.to_height);
    }
}

struct SourceFetchRequest<'a, Source> {
    request_timeout: Duration,
    source: &'a Source,
    start_height: BlockHeight,
    cursor: SourceChainCursor,
    max_connected_blocks: NonZeroU32,
    target_response_bytes: NonZeroU64,
    max_response_bytes: NonZeroU64,
    reserved_response_bytes: u64,
    reservation: ByteReservation,
}

struct PrefetchedSourceSegment {
    start_height: BlockHeight,
    max_connected_blocks: NonZeroU32,
    segment: SourceChainSegment,
    queued_response_bytes: u64,
}

fn fetch_prefetched_chain_segment<'a, Source>(
    request: SourceFetchRequest<'a, Source>,
) -> BoxFuture<'a, Result<PrefetchedSourceSegment, IngestError>>
where
    Source: NodeSource + 'a,
{
    let request_timeout = request.request_timeout;
    let source = request.source;
    let start_height = request.start_height;
    let cursor = request.cursor;
    let max_connected_blocks = request.max_connected_blocks;
    let target_response_bytes = request.target_response_bytes;
    let max_response_bytes = request.max_response_bytes;
    let reserved_response_bytes = request.reserved_response_bytes;
    let reservation = request.reservation;

    async move {
        let mut retry_state = IngestRetryState::default();
        let limits = SourceChainSegmentLimits::new(
            cursor,
            max_connected_blocks,
            target_response_bytes.get(),
            max_response_bytes.get(),
        );
        let segment =
            fetch_chain_segment_with_retry(request_timeout, source, limits, &mut retry_state)
                .await?;
        let queued_response_bytes = queued_source_segment_bytes(&segment, reserved_response_bytes);
        reservation.release();
        Ok(PrefetchedSourceSegment {
            start_height,
            max_connected_blocks,
            segment,
            queued_response_bytes,
        })
    }
    .boxed()
}

fn queued_source_segment_bytes(segment: &SourceChainSegment, reserved_response_bytes: u64) -> u64 {
    let measured_response_bytes = segment.stats().response_payload_bytes();
    if measured_response_bytes == 0 && !segment.is_empty() {
        reserved_response_bytes
    } else {
        measured_response_bytes
    }
}

fn pop_next_completed_source_segment<Source>(
    state: &mut SourceBlockStreamState<'_, Source>,
) -> Option<PrefetchedSourceSegment> {
    let next_emit_height = state.next_emit_height?;
    let prefetched_segment = state.completed_segments.remove(&next_emit_height)?;
    state.completed_segment_bytes = state
        .completed_segment_bytes
        .saturating_sub(prefetched_segment.queued_response_bytes);
    state.next_emit_height =
        next_height_after_segment(next_emit_height, prefetched_segment.max_connected_blocks)
            .filter(|height| *height <= state.to_height);
    record_source_head_of_line_wait_completed(state);
    record_source_fetch_queue_state(state);
    Some(prefetched_segment)
}

fn insert_completed_source_segment<Source>(
    state: &mut SourceBlockStreamState<'_, Source>,
    prefetched_segment: PrefetchedSourceSegment,
) -> Result<(), IngestError> {
    if prefetched_segment.start_height < state.from_height
        || prefetched_segment.start_height > state.to_height
    {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "source chain segment completed outside the requested bulk-catchup range",
        }));
    }
    let queued_response_bytes = prefetched_segment.queued_response_bytes;
    if state
        .completed_segments
        .insert(prefetched_segment.start_height, prefetched_segment)
        .is_some()
    {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "source chain segment completed twice during bulk catchup",
        }));
    }
    state.completed_segment_bytes = state
        .completed_segment_bytes
        .saturating_add(queued_response_bytes);
    Ok(())
}

fn record_source_fetch_queue_state<Source>(state: &SourceBlockStreamState<'_, Source>) {
    let source_fetch_snapshot = state.source_fetch_watermark.snapshot();
    metrics::gauge!("zinder_ingest_source_fetch_queue_requests").set(f64::from(
        usize_to_u32_saturating(state.in_flight_segments.len()),
    ));
    metrics::gauge!("zinder_ingest_source_fetch_queue_bytes").set(u64_to_f64(
        source_fetch_snapshot
            .reserved_bytes
            .saturating_add(state.completed_segment_bytes),
    ));
    metrics::gauge!("zinder_ingest_source_segment_reassembly_segments").set(f64::from(
        usize_to_u32_saturating(state.completed_segments.len()),
    ));
    metrics::gauge!("zinder_ingest_source_segment_reassembly_bytes")
        .set(u64_to_f64(state.completed_segment_bytes));
    record_queue_depth(BULK_STAGE_SOURCE_FETCH, state.in_flight_segments.len());
    record_reorder_buffer(
        BULK_STAGE_SOURCE_FETCH,
        state.completed_segments.len(),
        state.completed_segment_bytes,
    );
}

fn record_source_head_of_line_wait_started<Source>(state: &mut SourceBlockStreamState<'_, Source>) {
    if state.completed_segments.is_empty() {
        state.source_head_of_line_started_at = None;
        return;
    }
    if state.source_head_of_line_started_at.is_none() {
        state.source_head_of_line_started_at = Some(Instant::now());
    }
}

fn record_source_head_of_line_wait_completed<Source>(
    state: &mut SourceBlockStreamState<'_, Source>,
) {
    let Some(started_at) = state.source_head_of_line_started_at.take() else {
        return;
    };
    metrics::histogram!(
        "zinder_ingest_bulk_pipeline_head_of_line_wait_seconds",
        "stage" => BULK_STAGE_SOURCE_FETCH
    )
    .record(started_at.elapsed());
}

fn enqueue_source_segment_max_blocks<Source>(
    state: &mut SourceBlockStreamState<'_, Source>,
    segment: SourceChainSegment,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    let mut connected_blocks = 0_u32;
    for update in segment.into_updates() {
        match update {
            SourceChainUpdate::ConnectedBlock { block, .. } => {
                validate_prefetched_block_link(state.last_connected_block_id, &block)?;
                state.last_connected_block_id = Some(BlockId::new(block.height, block.hash));
                if block.height <= state.to_height {
                    connected_blocks = connected_blocks.saturating_add(1);
                    state.pending_blocks.push_back(block);
                }
            }
            SourceChainUpdate::RevertedBlock { block_id, .. } => {
                return Err(IngestError::from(SourceError::BlockReorgDuringFetch {
                    height: block_id.height,
                    reason: "source chain segment reverted during bulk catchup",
                }));
            }
            SourceChainUpdate::FinalizedTip { .. } => {}
        }
    }

    if connected_blocks == 0 {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "source chain segment did not contain connected blocks during bulk catchup",
        }));
    }
    Ok(())
}

fn validate_prefetched_block_link(
    previous_block_id: Option<BlockId>,
    block: &SourceBlock,
) -> Result<(), IngestError> {
    let Some(previous_block_id) = previous_block_id else {
        return Ok(());
    };
    let Some(expected_height) = previous_block_id.height.next() else {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "source chain segment continued after maximum block height",
        }));
    };
    if block.height != expected_height {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "source chain segment skipped a height during bulk catchup",
        }));
    }
    if block.parent_hash != previous_block_id.hash {
        return Err(IngestError::from(SourceError::BlockReorgDuringFetch {
            height: block.height,
            reason: "prefetched source chain segment did not connect to the previous segment",
        }));
    }
    Ok(())
}

fn next_height_after_segment(
    start_height: BlockHeight,
    source_segment_max_blocks: NonZeroU32,
) -> Option<BlockHeight> {
    start_height
        .value()
        .checked_add(source_segment_max_blocks.get())
        .map(BlockHeight::new)
}

#[derive(Clone, Copy)]
struct SourceSegmentDensitySample {
    response_payload_bytes: u64,
    connected_blocks: u32,
}

pub(super) struct SourceSegmentSizer {
    max_blocks: NonZeroU32,
    target_response_payload_bytes: NonZeroU64,
    current_blocks: NonZeroU32,
    success_count: u32,
    overshoot_clear_success_count: u32,
    overshoot_bytes_per_block: Option<u64>,
    active_branch_id: ConsensusBranchId,
    activations: Arc<NetworkUpgradeActivations>,
    density_samples: VecDeque<SourceSegmentDensitySample>,
}

impl SourceSegmentSizer {
    pub(super) fn new(
        max_blocks: NonZeroU32,
        target_response_payload_bytes: NonZeroU64,
        activations: Arc<NetworkUpgradeActivations>,
        from_height: BlockHeight,
    ) -> Self {
        let current_blocks = max_blocks;
        let active_branch_id = activations.consensus_branch_id_at(from_height);
        record_source_segment_sizer_state(
            current_blocks,
            max_blocks,
            target_response_payload_bytes,
        );
        Self {
            max_blocks,
            target_response_payload_bytes,
            current_blocks,
            success_count: 0,
            overshoot_clear_success_count: 0,
            overshoot_bytes_per_block: None,
            active_branch_id,
            activations,
            density_samples: VecDeque::new(),
        }
    }

    pub(super) fn blocks_for_remaining_range(
        &mut self,
        next_height: BlockHeight,
        to_height: BlockHeight,
    ) -> Option<NonZeroU32> {
        if next_height > to_height {
            return None;
        }
        self.reset_after_network_upgrade_if_needed(next_height);
        let remaining_blocks = to_height
            .value()
            .saturating_sub(next_height.value())
            .saturating_add(1);
        NonZeroU32::new(remaining_blocks.min(self.current_blocks.get()))
    }

    pub(super) fn record_segment(&mut self, stats: SourceChainSegmentStats) {
        self.record_density_sample(stats);
        self.record_overshoot_memory(stats);
        let previous_blocks = self.current_blocks;
        let next_blocks = if stats.split_count() > 0 {
            self.shrunk_blocks_after_overshoot()
        } else {
            self.blocks_after_success()
        };
        self.current_blocks = next_blocks;
        record_source_segment_sizer_state(
            self.current_blocks,
            self.max_blocks,
            self.target_response_payload_bytes,
        );
        if previous_blocks != self.current_blocks {
            let reason = if stats.split_count() > 0 {
                "response_too_large"
            } else if self.current_blocks < previous_blocks {
                "density"
            } else {
                "success"
            };
            record_source_segment_sizer_adjustment(reason, previous_blocks, self.current_blocks);
        }
    }

    fn record_density_sample(&mut self, stats: SourceChainSegmentStats) {
        if stats.connected_blocks() == 0 || stats.response_payload_bytes() == 0 {
            return;
        }
        self.density_samples.push_back(SourceSegmentDensitySample {
            response_payload_bytes: stats.response_payload_bytes(),
            connected_blocks: stats.connected_blocks(),
        });
        while self.density_samples.len() > SOURCE_SEGMENT_DENSITY_SAMPLE_LIMIT {
            self.density_samples.pop_front();
        }
    }

    fn shrunk_blocks_after_overshoot(&mut self) -> NonZeroU32 {
        self.success_count = 0;
        self.overshoot_clear_success_count = 0;
        let halved = self.current_blocks.get().saturating_div(2).max(1);
        nonzero_u32(
            self.blocks_allowed_by_density()
                .map_or(halved, |blocks| halved.min(blocks.get()))
                .max(1),
        )
    }

    fn blocks_after_success(&mut self) -> NonZeroU32 {
        let density_blocks = self.blocks_allowed_by_density();
        if let Some(density_blocks) = density_blocks
            && density_blocks < self.current_blocks
        {
            self.success_count = 0;
            return density_blocks;
        }

        self.success_count = self.success_count.saturating_add(1);
        if self.success_count < SOURCE_SEGMENT_GROW_AFTER_SUCCESS_COUNT {
            return self.current_blocks;
        }

        self.success_count = 0;
        let grown = self
            .current_blocks
            .get()
            .saturating_mul(SOURCE_SEGMENT_GROW_NUMERATOR)
            .saturating_div(SOURCE_SEGMENT_GROW_DENOMINATOR)
            .max(self.current_blocks.get().saturating_add(1));
        let capped = grown.min(self.max_blocks.get());
        nonzero_u32(density_blocks.map_or(capped, |blocks| capped.min(blocks.get())))
    }

    fn blocks_allowed_by_density(&self) -> Option<NonZeroU32> {
        let bytes_per_block = self.estimated_response_payload_bytes_per_block()?;
        let target_blocks = self
            .target_response_payload_bytes
            .get()
            .saturating_div(bytes_per_block)
            .max(1)
            .min(u64::from(self.max_blocks.get()));
        Some(nonzero_u32(
            u32::try_from(target_blocks).unwrap_or(u32::MAX),
        ))
    }

    fn estimated_response_payload_bytes_per_block(&self) -> Option<u64> {
        let p95 = self.p95_response_payload_bytes_per_block();
        match (p95, self.overshoot_bytes_per_block) {
            (Some(p95), Some(overshoot)) => Some(p95.max(overshoot)),
            (Some(p95), None) => Some(p95),
            (None, Some(overshoot)) => Some(overshoot),
            (None, None) => None,
        }
    }

    fn p95_response_payload_bytes_per_block(&self) -> Option<u64> {
        let mut samples = self
            .density_samples
            .iter()
            .map(|sample| {
                let blocks = u64::from(sample.connected_blocks);
                sample
                    .response_payload_bytes
                    .saturating_add(blocks.saturating_sub(1))
                    / blocks
            })
            .collect::<Vec<_>>();
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let percentile_index = samples
            .len()
            .saturating_mul(95)
            .saturating_add(99)
            .saturating_div(100)
            .saturating_sub(1);
        samples.get(percentile_index).copied()
    }

    fn record_overshoot_memory(&mut self, stats: SourceChainSegmentStats) {
        if stats.split_count() > 0 {
            let Some(bytes_per_block) = response_payload_bytes_per_block(stats) else {
                return;
            };
            self.overshoot_bytes_per_block = Some(
                self.overshoot_bytes_per_block
                    .map_or(bytes_per_block, |current| current.max(bytes_per_block)),
            );
            self.overshoot_clear_success_count = 0;
            return;
        }

        if stats.response_payload_bytes() > self.target_response_payload_bytes.get() {
            self.overshoot_clear_success_count = 0;
            return;
        }
        self.overshoot_clear_success_count = self.overshoot_clear_success_count.saturating_add(1);
        if self.overshoot_clear_success_count >= SOURCE_SEGMENT_GROW_AFTER_SUCCESS_COUNT {
            self.overshoot_bytes_per_block = None;
            self.overshoot_clear_success_count = 0;
        }
    }

    fn reset_after_network_upgrade_if_needed(&mut self, next_height: BlockHeight) {
        let active_branch_id = self.activations.consensus_branch_id_at(next_height);
        if active_branch_id == self.active_branch_id {
            return;
        }
        let previous_blocks = self.current_blocks;
        self.active_branch_id = active_branch_id;
        self.current_blocks = self.max_blocks;
        self.success_count = 0;
        self.overshoot_clear_success_count = 0;
        self.overshoot_bytes_per_block = None;
        self.density_samples.clear();
        record_source_segment_sizer_state(
            self.current_blocks,
            self.max_blocks,
            self.target_response_payload_bytes,
        );
        record_source_segment_sizer_adjustment(
            "network_upgrade",
            previous_blocks,
            self.current_blocks,
        );
    }
}

fn response_payload_bytes_per_block(stats: SourceChainSegmentStats) -> Option<u64> {
    if stats.connected_blocks() == 0 || stats.response_payload_bytes() == 0 {
        return None;
    }
    let blocks = u64::from(stats.connected_blocks());
    Some(
        stats
            .response_payload_bytes()
            .saturating_add(blocks.saturating_sub(1))
            / blocks,
    )
}
