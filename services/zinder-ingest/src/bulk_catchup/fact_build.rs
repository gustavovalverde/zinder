use std::{
    collections::{BTreeMap, VecDeque},
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
use zinder_core::BlockHeight;
use zinder_source::{NodeSource, SourceBlock, SourceError};

use super::source_fetch::{SourceSegmentSizer, build_source_block_stream};
use super::watermark::{ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer};
use super::{
    BULK_STAGE_CANONICAL_FACT_BUILD, IngestError, usize_to_u32_saturating, usize_to_u64_saturating,
};
use crate::artifact_builder::DerivedBlockArtifacts;
use crate::chain_ingest::record_ingest_fact_build_outcome;

pub(super) struct BulkCatchupFactBuildStreamConfig {
    pub(super) request_timeout: Duration,
    pub(super) from_height: BlockHeight,
    pub(super) to_height: BlockHeight,
    pub(super) max_response_bytes: NonZeroU64,
    pub(super) target_response_payload_bytes: NonZeroU64,
    pub(super) source_fetch_max_in_flight_requests: NonZeroU32,
    pub(super) source_fetch_max_in_flight_bytes: NonZeroU64,
    pub(super) source_segment_sizer: Arc<Mutex<SourceSegmentSizer>>,
    pub(super) fact_build_concurrency: usize,
    pub(super) fact_build_max_in_flight_artifact_bytes: NonZeroU64,
}

pub(super) fn build_fact_build_stream<'a, Source, F, Fut>(
    source: &'a Source,
    config: BulkCatchupFactBuildStreamConfig,
    derive_fn: F,
) -> impl Stream<Item = Result<DerivedBlockArtifacts, IngestError>> + Send + 'a
where
    Source: NodeSource + 'a,
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'a,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'a,
{
    let fact_build_concurrency = config.fact_build_concurrency.max(1);
    let from_height = config.from_height;
    let to_height = config.to_height;
    let fact_build_max_in_flight_artifact_bytes = config.fact_build_max_in_flight_artifact_bytes;
    let state = FactBuildStreamState {
        source_blocks: build_source_block_stream(source, config).boxed(),
        in_flight_fact_builds: FuturesUnordered::new(),
        completed_fact_builds: BTreeMap::new(),
        completed_fact_build_bytes: 0,
        pending_source_blocks: VecDeque::new(),
        derive_fn,
        fact_build_concurrency,
        fact_build_watermark: ByteWatermark::new(
            BULK_STAGE_CANONICAL_FACT_BUILD,
            fact_build_max_in_flight_artifact_bytes,
        ),
        next_emit_height: Some(from_height),
        to_height,
        source_exhausted: false,
    };

    stream::unfold(state, |mut state| async move {
        let next_derived_block = next_derived_block_from_fact_build_stream(&mut state).await;
        next_derived_block.map(|derived_result| (derived_result, state))
    })
}

struct PrefetchedDerivedBlock {
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

struct FactBuildStreamState<'a, F> {
    source_blocks: BoxStream<'a, Result<SourceBlock, IngestError>>,
    in_flight_fact_builds:
        FuturesUnordered<BoxFuture<'a, Result<PrefetchedDerivedBlock, IngestError>>>,
    completed_fact_builds: BTreeMap<BlockHeight, QueuedDerivedBlock>,
    completed_fact_build_bytes: u64,
    pending_source_blocks: VecDeque<SourceBlock>,
    derive_fn: F,
    fact_build_concurrency: usize,
    fact_build_watermark: ByteWatermark,
    next_emit_height: Option<BlockHeight>,
    to_height: BlockHeight,
    source_exhausted: bool,
}

async fn next_derived_block_from_fact_build_stream<'a, F, Fut>(
    state: &mut FactBuildStreamState<'a, F>,
) -> Option<Result<DerivedBlockArtifacts, IngestError>>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'a,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'a,
{
    loop {
        if let Some(next_emit_height) = state.next_emit_height
            && let Some(queued) = state.completed_fact_builds.remove(&next_emit_height)
        {
            state.next_emit_height = next_emit_height
                .next()
                .filter(|height| *height <= state.to_height);
            let QueuedDerivedBlock {
                derived,
                artifact_bytes,
                reservation,
            } = queued;
            state.completed_fact_build_bytes = state
                .completed_fact_build_bytes
                .saturating_sub(artifact_bytes);
            record_fact_build_reassembly_state(state);
            drop(reservation);
            return Some(Ok(derived));
        }

        if let Some(source_block) = state.pending_source_blocks.pop_front() {
            match schedule_fact_build(state, source_block) {
                Ok(()) => continue,
                Err(source_block) => state.pending_source_blocks.push_front(source_block),
            }
        }

        let can_schedule_fact_build = state.can_schedule_fact_build();
        if !can_schedule_fact_build && state.in_flight_fact_builds.is_empty() {
            return None;
        }

        tokio::select! {
            source_block_result = state.source_blocks.next(), if can_schedule_fact_build => {
                match source_block_result {
                    Some(Ok(source_block)) => {
                        if let Err(source_block) = schedule_fact_build(state, source_block) {
                            state.pending_source_blocks.push_front(source_block);
                        }
                    }
                    Some(Err(error)) => return Some(Err(error)),
                    None => state.source_exhausted = true,
                }
            }
            fact_build_result = state.in_flight_fact_builds.next(), if !state.in_flight_fact_builds.is_empty() => {
                let prefetched_derived = match fact_build_result {
                    Some(Ok(prefetched_derived)) => prefetched_derived,
                    Some(Err(error)) => return Some(Err(error)),
                    None => continue,
                };
                if let Err(error) = insert_completed_fact_build(state, prefetched_derived) {
                    return Some(Err(error));
                }
                record_fact_build_reassembly_state(state);
            }
        }
    }
}

impl<F> FactBuildStreamState<'_, F> {
    fn can_schedule_fact_build(&self) -> bool {
        !self.source_exhausted
            && self.pending_source_blocks.is_empty()
            && self.in_flight_fact_builds.len() < self.fact_build_concurrency
            && self.completed_fact_builds.len() < self.fact_build_concurrency
    }
}

fn schedule_fact_build<'a, F, Fut>(
    state: &FactBuildStreamState<'a, F>,
    source_block: SourceBlock,
) -> Result<(), SourceBlock>
where
    F: Fn(SourceBlock) -> Fut + Clone + Send + Sync + 'a,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + Send + 'a,
{
    if state.in_flight_fact_builds.len() >= state.fact_build_concurrency {
        return Err(source_block);
    }
    let estimated_artifact_bytes = source_block.raw_block_bytes.len().max(1);
    let Some(reservation) = state
        .fact_build_watermark
        .try_reserve(usize_to_u64_saturating(estimated_artifact_bytes))
    else {
        return Err(source_block);
    };
    let derive_fn = state.derive_fn.clone();
    state.in_flight_fact_builds.push(
        async move {
            let height = source_block.height;
            let fact_build_started_at = Instant::now();
            let fact_build_outcome = derive_fn(source_block).await;
            record_ingest_fact_build_outcome(fact_build_started_at, &fact_build_outcome);
            fact_build_outcome.map(|derived| {
                let artifact_bytes = derived_block_artifact_bytes(&derived);
                let mut reservation = reservation;
                reservation.resize(artifact_bytes);
                PrefetchedDerivedBlock {
                    height,
                    derived,
                    artifact_bytes,
                    reservation,
                }
            })
        }
        .boxed(),
    );
    Ok(())
}

fn insert_completed_fact_build<F>(
    state: &mut FactBuildStreamState<'_, F>,
    prefetched_derived: PrefetchedDerivedBlock,
) -> Result<(), IngestError> {
    if prefetched_derived.height > state.to_height {
        return Err(IngestError::from(SourceError::SourceProtocolMismatch {
            reason: "derived block completed outside the requested bulk-catchup range",
        }));
    }
    state.completed_fact_build_bytes = state
        .completed_fact_build_bytes
        .saturating_add(prefetched_derived.artifact_bytes);
    if state
        .completed_fact_builds
        .insert(
            prefetched_derived.height,
            QueuedDerivedBlock {
                derived: prefetched_derived.derived,
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

fn record_fact_build_reassembly_state<F>(state: &FactBuildStreamState<'_, F>) {
    metrics::gauge!("zinder_ingest_fact_build_reassembly_blocks").set(f64::from(
        usize_to_u32_saturating(state.completed_fact_builds.len()),
    ));
    record_queue_depth(
        BULK_STAGE_CANONICAL_FACT_BUILD,
        state.completed_fact_builds.len(),
    );
    record_reorder_buffer(
        BULK_STAGE_CANONICAL_FACT_BUILD,
        state.completed_fact_builds.len(),
        state.completed_fact_build_bytes,
    );
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
