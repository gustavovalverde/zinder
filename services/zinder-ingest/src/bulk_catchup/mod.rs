use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
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
use zinder_core::{
    BlockHeight, BlockId, ChainEpoch, ChainEpochId, ChainTipMetadata, ConsensusBranchId, Network,
    NetworkUpgradeActivations, TreeStateArtifact,
};
use zinder_runtime::{NodeUnavailableDetail, Readiness, ReadinessState};
use zinder_source::{
    NodeSource, NodeTarget, SourceBlock, SourceChainCheckpoint, SourceChainCursor,
    SourceChainSegment, SourceChainSegmentLimits, SourceChainSegmentStats, SourceChainUpdate,
    SourceError, SourceFailureClass,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::artifact_builder::{
    CommitmentTreeSizes, DerivedBlockArtifacts, RawBlobPolicy, derive_block_with_raw_blob_policy,
    finalize_derived_block,
};
use crate::chain_ingest::{
    CanonicalBatch, CanonicalBatchBudget, CanonicalBatchCloseTrigger, CanonicalBatchCost,
    IngestError, IngestRetryState, IngestSubtreeRootIndexes, NodeSourceKind, commit_ingest_batch,
    current_unix_millis, fetch_chain_segment_with_retry, fetch_tree_state_for_block_with_retry,
    next_chain_epoch_id, next_chain_epoch_id_after, populate_subtree_root_artifacts,
    record_ingest_batch_commit_trigger, record_ingest_batch_work_cost,
    record_ingest_fact_build_outcome,
};
use crate::phase::current_chain_height;
use crate::source_recovery::{
    SourceRecoveryDecision, decide_recovery, default_recovery_backoff, detail_for_new_outage,
    detail_for_ongoing_outage,
};
use watermark::{
    ByteReservation, ByteWatermark, record_queue_depth, record_reorder_buffer,
    record_stage_duration,
};

mod watermark;

const SOURCE_SEGMENT_DENSITY_SAMPLE_LIMIT: usize = 64;
const SOURCE_SEGMENT_GROW_AFTER_SUCCESS_COUNT: u32 = 8;
const SOURCE_SEGMENT_GROW_NUMERATOR: u32 = 5;
const SOURCE_SEGMENT_GROW_DENOMINATOR: u32 = 4;

const BULK_STAGE_SOURCE_FETCH: &str = "source_fetch";
const BULK_STAGE_CANONICAL_FACT_BUILD: &str = "canonical_fact_build";
const BULK_STAGE_CANONICAL_FINALIZE: &str = "canonical_finalize";
const BULK_STAGE_SUBTREE_ROOT_ATTACHMENT: &str = "subtree_root_attachment";
const BULK_STAGE_CHECKPOINT_TREE_STATE: &str = "checkpoint_tree_state";
const BULK_STAGE_COMMIT_REASSEMBLY: &str = "commit_reassembly";
const BULK_STAGE_CANONICAL_COMMIT: &str = "canonical_commit";
const BULK_STAGE_CANONICAL_FLUSH: &str = "canonical_flush";

/// Configuration for a one-shot historical backfill outside the reorg window.
#[derive(Clone, Debug)]
pub struct BackfillConfig {
    /// Resolved upstream node endpoint (network, JSON-RPC URL, auth, timeout,
    /// response-size cap). See [`NodeTarget`].
    pub node: NodeTarget,
    /// Upstream node source implementation.
    pub node_source: NodeSourceKind,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// Bounded `RocksDB` resource budget applied when opening the store.
    pub storage_tuning: zinder_store::StorageTuning,
    /// First block height to backfill.
    pub from_height: BlockHeight,
    /// Last block height to backfill.
    pub to_height: BlockHeight,
    /// Maximum number of blocks committed in one chain epoch.
    pub canonical_batch_max_blocks: NonZeroU32,
    /// Maximum in-memory canonical artifact bytes accumulated before commit.
    pub canonical_batch_max_artifact_bytes: NonZeroU64,
    /// Maximum connected blocks requested from the source adapter in one
    /// bounded bulk-catchup segment.
    ///
    /// Zebra JSON-RPC batches the segment into one JSON-RPC request containing
    /// raw `getblock` calls. Checkpoint tree state is fetched separately for
    /// committed epoch tips. Future streaming sources can satisfy the same
    /// boundary without changing canonical ingest.
    /// Operator-tunable via `ingest.bulk_catchup.source_segment_max_blocks`.
    pub source_segment_max_blocks: NonZeroU32,
    /// Target JSON-RPC response body size for adaptive source segments.
    pub source_segment_target_response_bytes: NonZeroU64,
    /// Maximum concurrent source segment requests.
    pub source_fetch_max_in_flight_requests: NonZeroU32,
    /// Maximum reserved response bytes across source segment requests.
    pub source_fetch_max_in_flight_bytes: NonZeroU64,
    /// Number of parallel `derive_block` invocations kept in flight on the
    /// Tokio blocking pool. Per-block derivation is CPU-bound (block
    /// deserialization, per-tx canonical re-serialization, compact-block
    /// proto encoding, per-output `SHA256(script_pub_key)`); parallelism
    /// scales nearly linearly with cores up to the commit-batch boundary.
    /// Operator-tunable via `ingest.bulk_catchup.fact_build_concurrency`.
    /// See [ADR-0021](../../../../docs/adrs/0021-parallel-block-derivation.md).
    pub fact_build_concurrency: NonZeroU32,
    /// Maximum reserved derived artifact bytes across active and completed
    /// fact-build work.
    pub fact_build_max_in_flight_artifact_bytes: NonZeroU64,
    /// Maximum finalized artifact bytes that can accumulate while the previous
    /// batch is attaching metadata, committing, or flushing.
    pub commit_reassembly_max_queued_artifact_bytes: NonZeroU64,
    /// Force a `RocksDB` flush after committing this many epochs. See
    /// [`crate::BulkCatchupConfig::flush_interval_epochs`].
    pub flush_interval_epochs: NonZeroU32,
    /// Optional raw-byte blob write policy.
    pub raw_blob_policy: RawBlobPolicy,
    /// Node-discovered consensus upgrade activations used for transaction facts.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    /// Pre-observed upstream tip height. When set, `backfill_with_store`
    /// uses this in place of an internal `tip_id()` round-trip for its
    /// finality-bound validation. The unified ingest loop reuses the tip
    /// it observed at the top of each iteration; one-shot callers leave
    /// this `None` so the call observes the tip itself.
    pub upstream_tip_hint: Option<BlockHeight>,
    /// Allows finalizing blocks inside the upstream node's current reorg window.
    pub allow_near_tip_finalize: bool,
    /// Optional starting checkpoint for an empty store.
    ///
    /// When present and the store is empty, ingest seeds a stub chain epoch
    /// at `checkpoint.height` carrying the node-supplied
    /// `tip_metadata`, then begins backfill from `checkpoint.height + 1`.
    /// `from_height` must equal `checkpoint.height + 1` in this mode. Reads
    /// at heights below the checkpoint return `ArtifactUnavailable`.
    pub checkpoint: Option<SourceChainCheckpoint>,
}

/// Mutable bulk-catchup state carried across backfill batches.
///
/// The unified ingest loop invokes `backfill_until_complete` once per
/// bulk-catchup batch so it can re-classify the phase after each commit.
/// This state keeps the WAL flush cadence and source-density sizing tied to
/// the continuous bulk range rather than to that one-batch call boundary.
#[derive(Default)]
pub(crate) struct BackfillFlushState {
    epochs_since_last_flush: u32,
    source_segment_sizer: Option<Arc<Mutex<SourceSegmentSizer>>>,
}

impl BackfillFlushState {
    fn record_committed_epoch(&mut self) {
        self.epochs_since_last_flush = self.epochs_since_last_flush.saturating_add(1);
    }

    fn should_flush(&self, flush_interval_epochs: NonZeroU32) -> bool {
        self.epochs_since_last_flush >= flush_interval_epochs.get()
    }

    fn mark_flushed(&mut self) {
        self.epochs_since_last_flush = 0;
    }

    fn has_pending_epochs(&self) -> bool {
        self.epochs_since_last_flush > 0
    }

    fn source_segment_sizer(
        &mut self,
        config: &BackfillConfig,
        from_height: BlockHeight,
    ) -> Arc<Mutex<SourceSegmentSizer>> {
        Arc::clone(self.source_segment_sizer.get_or_insert_with(|| {
            Arc::new(Mutex::new(SourceSegmentSizer::new(
                config.source_segment_max_blocks,
                config.source_segment_target_response_bytes,
                Arc::clone(&config.network_upgrade_activations),
                from_height,
            )))
        }))
    }

    #[cfg(test)]
    fn pending_epoch_count(&self) -> u32 {
        self.epochs_since_last_flush
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackfillCompletionFlush {
    FlushPending,
    PreservePending,
}

impl BackfillCompletionFlush {
    const fn flushes_pending(self) -> bool {
        matches!(self, Self::FlushPending)
    }
}

/// Stable dependencies for one backfill run.
///
/// Keeping these handles together prevents the bulk-catchup hot path from
/// growing long positional argument lists as the writer gains operational
/// state such as flush cadence and readiness reporting.
pub(crate) struct BackfillRunContext<'a, Source> {
    config: &'a BackfillConfig,
    source: &'a Source,
    store: &'a PrimaryChainStore,
}

impl<'a, Source> BackfillRunContext<'a, Source> {
    pub(crate) const fn new(
        config: &'a BackfillConfig,
        source: &'a Source,
        store: &'a PrimaryChainStore,
    ) -> Self {
        Self {
            config,
            source,
            store,
        }
    }
}

/// Runs a historical backfill and commits the requested range to canonical storage.
///
/// Returns `Some(commit_outcome)` when at least one chain epoch was
/// committed and `None` when the requested range was already present
/// in the canonical store. Opens the [`PrimaryChainStore`] internally;
/// callers that want to share the store with other writers (e.g. an
/// `IngestControl` gRPC server that reads chain events during backfill)
/// should open the store themselves and call [`backfill_with_store`]
/// instead.
pub async fn backfill<Source>(
    config: &BackfillConfig,
    source: &Source,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let store_options = ChainStoreOptions {
        tuning: config.storage_tuning,
        ..ChainStoreOptions::for_network(config.node.network)
    };
    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    backfill_with_store(config, source, &store).await
}

/// Runs a historical backfill against a caller-owned [`PrimaryChainStore`].
///
/// Returns `Some(commit_outcome)` when at least one chain epoch was
/// committed and `None` when the requested range was already present in
/// the store. The supplied store must have been opened with the same
/// [`ChainStoreOptions`] backfill expects
/// (`ChainStoreOptions::for_network(config.node.network)`); `RocksDB`
/// enforces a single primary handle per database, so a caller that
/// needs to expose readable surfaces (the `IngestControl` gRPC service)
/// during backfill must open the store once and pass it to this entry
/// point.
///
/// When [`BackfillConfig::upstream_tip_hint`] is `Some`, the call skips
/// its own `tip_id()` round-trip and uses the caller-supplied tip for
/// the finality-bound validation. The unified ingest loop sets the hint
/// from the tip it already observed at the top of each iteration, which
/// removes a serial RPC per batch on the bulk-catchup hot path.
pub async fn backfill_with_store<Source>(
    config: &BackfillConfig,
    source: &Source,
    store: &PrimaryChainStore,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BackfillFlushState::default();
    let run = BackfillRunContext::new(config, source, store);
    backfill_with_store_inner(
        &run,
        &mut flush_state,
        BackfillCompletionFlush::FlushPending,
    )
    .await
}

async fn backfill_with_store_inner<Source>(
    run: &BackfillRunContext<'_, Source>,
    flush_state: &mut BackfillFlushState,
    completion_flush: BackfillCompletionFlush,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let config = run.config;
    let store_options = ChainStoreOptions::for_network(config.node.network);
    let node_tip_height = match config.upstream_tip_hint {
        Some(height) => height,
        None => run.source.tip_id().await?.height,
    };
    validate_backfill_finality_bound(config, node_tip_height, store_options.reorg_window_blocks)?;
    warn_if_checkpoint_within_reorg_window(
        config,
        node_tip_height,
        store_options.reorg_window_blocks,
    );

    let current_chain_epoch = match bootstrap_from_checkpoint_if_needed(
        run.store,
        config.node.network,
        config.checkpoint,
        config.from_height,
    )? {
        Some(bootstrapped) => Some(bootstrapped),
        None => run.store.current_chain_epoch()?,
    };
    let Some(backfill_start) =
        backfill_start(current_chain_epoch, config.from_height, config.to_height)?
    else {
        // Range already covered; no new commit. Callers that need the
        // current chain epoch read it from `store.current_chain_epoch()`.
        let _ = current_chain_epoch.ok_or(IngestError::BackfillProducedNoCommit)?;
        return Ok(None);
    };

    backfill_from_source_with_store(run, backfill_start, flush_state, completion_flush)
        .await
        .map(Some)
}

/// Runs a historical backfill until the requested range is covered.
///
/// Returns the commit outcome (when at least one chain epoch was
/// committed) or `None` when the range was already covered. Retryable
/// upstream-node failures move readiness to `node_unavailable` and keep
/// polling instead of ending the writer process. Fatal configuration,
/// source protocol, storage, and artifact errors still return
/// immediately. One-shot callers that want process-fatal retry
/// deadlines should call [`backfill_with_store`] directly.
pub async fn backfill_until_complete<Source>(
    config: &BackfillConfig,
    source: &Source,
    store: &PrimaryChainStore,
    readiness: &Readiness,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BackfillFlushState::default();
    let run = BackfillRunContext::new(config, source, store);
    backfill_until_complete_inner(
        &run,
        readiness,
        &mut flush_state,
        BackfillCompletionFlush::FlushPending,
    )
    .await
}

pub(crate) async fn backfill_until_complete_with_flush_state<Source>(
    run: BackfillRunContext<'_, Source>,
    readiness: &Readiness,
    flush_state: &mut BackfillFlushState,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    backfill_until_complete_inner(
        &run,
        readiness,
        flush_state,
        BackfillCompletionFlush::PreservePending,
    )
    .await
}

async fn backfill_until_complete_inner<Source>(
    run: &BackfillRunContext<'_, Source>,
    readiness: &Readiness,
    flush_state: &mut BackfillFlushState,
    completion_flush: BackfillCompletionFlush,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let recovery_backoff = default_recovery_backoff();
    let mut outage: Option<(NodeUnavailableDetail, Instant)> = None;

    loop {
        match backfill_with_store_inner(run, flush_state, completion_flush).await {
            Ok(commit_outcome) => {
                let tip_height = match &commit_outcome {
                    Some(commit) => Some(commit.chain_epoch.tip_height.value()),
                    None => run
                        .store
                        .current_chain_epoch()?
                        .map(|chain_epoch| chain_epoch.tip_height.value()),
                };
                readiness.set(backfill_readiness_state(run.config, tip_height));
                if outage.take().is_some() {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "backfill_source_recovered",
                        "backfill source recovered"
                    );
                }
                return Ok(commit_outcome);
            }
            Err(error) => match decide_recovery(&error, recovery_backoff) {
                SourceRecoveryDecision::Recover {
                    failure_class,
                    last_reason,
                    backoff,
                } => {
                    let detail =
                        advance_backfill_outage(&mut outage, failure_class, last_reason.clone());
                    if detail.consecutive_failures == 1 {
                        tracing::warn!(
                            target: "zinder::ingest",
                            event = "backfill_source_unavailable",
                            failure_class = failure_class.label(),
                            error = %error,
                            "backfill source is unavailable; keeping the writer alive and retrying"
                        );
                    }
                    readiness.set(ReadinessState::node_unavailable_with_detail(
                        detail,
                        current_chain_height(run.store),
                    ));
                    tokio::time::sleep(backoff).await;
                }
                SourceRecoveryDecision::Exit => return Err(error),
            },
        }
    }
}

pub(crate) async fn flush_pending_backfill_writes(
    store: &PrimaryChainStore,
    flush_state: &mut BackfillFlushState,
) -> Result<(), IngestError> {
    if !flush_state.has_pending_epochs() {
        return Ok(());
    }
    flush_primary_chain_store(store).await?;
    flush_state.mark_flushed();
    Ok(())
}

fn backfill_readiness_state(
    config: &BackfillConfig,
    current_height: Option<u32>,
) -> ReadinessState {
    let Some(current_height) = current_height else {
        return ReadinessState::ready(None);
    };
    let Some(upstream_tip) = config.upstream_tip_hint.map(BlockHeight::value) else {
        return ReadinessState::ready(Some(current_height));
    };
    let lag_blocks = u64::from(upstream_tip.saturating_sub(current_height));
    if lag_blocks == 0 {
        ReadinessState::ready(Some(current_height))
    } else {
        ReadinessState::syncing(Some(lag_blocks), Some(current_height), Some(upstream_tip))
    }
}

fn advance_backfill_outage(
    outage: &mut Option<(NodeUnavailableDetail, Instant)>,
    failure_class: SourceFailureClass,
    last_reason: std::borrow::Cow<'static, str>,
) -> NodeUnavailableDetail {
    if let Some((existing, started_at)) = outage {
        let outage_seconds = u32::try_from(started_at.elapsed().as_secs()).unwrap_or(u32::MAX);
        let detail =
            detail_for_ongoing_outage(existing, failure_class, last_reason, outage_seconds);
        *existing = detail.clone();
        detail
    } else {
        let detail = detail_for_new_outage(failure_class, last_reason);
        *outage = Some((detail.clone(), Instant::now()));
        detail
    }
}

async fn backfill_from_source_with_store<Source>(
    run: &BackfillRunContext<'_, Source>,
    backfill_start: BackfillStart,
    flush_state: &mut BackfillFlushState,
    completion_flush: BackfillCompletionFlush,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
{
    let config = run.config;
    let request_timeout = config.node.request_timeout;
    let raw_blob_policy = config.raw_blob_policy;
    let network_upgrade_activations = Arc::clone(&config.network_upgrade_activations);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let fact_build_concurrency = config.fact_build_concurrency.get() as usize;
    let fact_build_stream = build_fact_build_stream(
        run.source,
        BackfillFactBuildStreamConfig {
            request_timeout,
            from_height: backfill_start.from_height,
            to_height: config.to_height,
            max_response_bytes: config.node.max_response_bytes,
            target_response_payload_bytes: config.source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes: config.source_fetch_max_in_flight_bytes,
            source_segment_sizer: flush_state
                .source_segment_sizer(config, backfill_start.from_height),
            fact_build_concurrency,
            fact_build_max_in_flight_artifact_bytes: config.fact_build_max_in_flight_artifact_bytes,
        },
        move |source_block| {
            let activations = Arc::clone(&network_upgrade_activations);
            async move {
                tokio::task::spawn_blocking(move || {
                    derive_block_with_raw_blob_policy(&source_block, &activations, raw_blob_policy)
                        .map_err(IngestError::from)
                })
                .await
                .map_err(|join_error| IngestError::BlockingTaskFailed {
                    reason: join_error.to_string(),
                })?
            }
        },
    );

    run_backfill_commit_loop(
        run,
        fact_build_stream,
        backfill_start,
        flush_state,
        completion_flush,
    )
    .await
}

struct BackfillFactBuildStreamConfig {
    request_timeout: Duration,
    from_height: BlockHeight,
    to_height: BlockHeight,
    max_response_bytes: NonZeroU64,
    target_response_payload_bytes: NonZeroU64,
    source_fetch_max_in_flight_requests: NonZeroU32,
    source_fetch_max_in_flight_bytes: NonZeroU64,
    source_segment_sizer: Arc<Mutex<SourceSegmentSizer>>,
    fact_build_concurrency: usize,
    fact_build_max_in_flight_artifact_bytes: NonZeroU64,
}

fn build_fact_build_stream<'a, Source, F, Fut>(
    source: &'a Source,
    config: BackfillFactBuildStreamConfig,
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

fn build_source_block_stream<'a, Source>(
    source: &'a Source,
    config: BackfillFactBuildStreamConfig,
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

struct SourceSegmentSizer {
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
    fn new(
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

    fn blocks_for_remaining_range(
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

    fn record_segment(&mut self, stats: SourceChainSegmentStats) {
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

fn nonzero_u32(amount: u32) -> NonZeroU32 {
    NonZeroU32::new(amount.max(1)).unwrap_or(NonZeroU32::MIN)
}

fn nonzero_u32_to_usize(amount: NonZeroU32) -> usize {
    usize::try_from(amount.get()).unwrap_or(usize::MAX)
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn nonzero_u64_to_usize(amount: NonZeroU64) -> usize {
    usize::try_from(amount.get()).unwrap_or(usize::MAX)
}

fn record_source_segment_sizer_state(
    current_blocks: NonZeroU32,
    max_blocks: NonZeroU32,
    target_response_payload_bytes: NonZeroU64,
) {
    metrics::gauge!("zinder_ingest_source_segment_next_blocks")
        .set(f64::from(current_blocks.get()));
    metrics::gauge!("zinder_ingest_source_segment_max_blocks").set(f64::from(max_blocks.get()));
    metrics::gauge!("zinder_ingest_source_segment_target_response_payload_bytes")
        .set(u64_to_f64(target_response_payload_bytes.get()));
}

fn record_source_segment_sizer_adjustment(
    reason: &'static str,
    previous_blocks: NonZeroU32,
    current_blocks: NonZeroU32,
) {
    if previous_blocks == current_blocks && reason != "network_upgrade" {
        return;
    }
    metrics::counter!(
        "zinder_ingest_source_segment_sizing_adjustment_total",
        "reason" => reason
    )
    .increment(1);
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges and histograms use f64 samples; byte counts are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[cfg(test)]
async fn backfill_from_source_with_mock_derive<Source, F>(
    config: &BackfillConfig,
    source: &Source,
    derive_fn: F,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    F: Fn(&zinder_source::SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError>
        + Copy
        + Send
        + Sync,
{
    let store_options = ChainStoreOptions {
        tuning: config.storage_tuning,
        ..ChainStoreOptions::for_network(config.node.network)
    };
    validate_backfill_finality_bound(
        config,
        source.tip_id().await?.height,
        store_options.reorg_window_blocks,
    )?;

    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    backfill_from_source_with_store_using_derive_fn(
        config,
        source,
        &store,
        derive_fn,
        BackfillStart {
            from_height: config.from_height,
            initial_tip_metadata: ChainTipMetadata::empty(),
        },
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "test seam mirrors the production backfill path plus an injected derive function"
)]
async fn backfill_from_source_with_store_using_derive_fn<Source, F>(
    config: &BackfillConfig,
    source: &Source,
    store: &PrimaryChainStore,
    derive_fn: F,
    backfill_start: BackfillStart,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    F: Fn(&zinder_source::SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError>
        + Copy
        + Send
        + Sync,
{
    let request_timeout = config.node.request_timeout;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let fact_build_concurrency = config.fact_build_concurrency.get() as usize;
    let mut flush_state = BackfillFlushState::default();
    let fact_build_stream = build_fact_build_stream(
        source,
        BackfillFactBuildStreamConfig {
            request_timeout,
            from_height: backfill_start.from_height,
            to_height: config.to_height,
            max_response_bytes: config.node.max_response_bytes,
            target_response_payload_bytes: config.source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes: config.source_fetch_max_in_flight_bytes,
            source_segment_sizer: flush_state
                .source_segment_sizer(config, backfill_start.from_height),
            fact_build_concurrency,
            fact_build_max_in_flight_artifact_bytes: config.fact_build_max_in_flight_artifact_bytes,
        },
        move |source_block| async move { derive_fn(&source_block).map_err(IngestError::from) },
    );

    let run = BackfillRunContext::new(config, source, store);
    run_backfill_commit_loop(
        &run,
        fact_build_stream,
        backfill_start,
        &mut flush_state,
        BackfillCompletionFlush::FlushPending,
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "bulk catchup orchestration keeps the ordered finalization, in-flight commit, and flush-state transitions visible in one state machine"
)]
async fn run_backfill_commit_loop<Source>(
    run: &BackfillRunContext<'_, Source>,
    fact_build_stream: impl Stream<Item = Result<DerivedBlockArtifacts, IngestError>> + Send,
    backfill_start: BackfillStart,
    flush_state: &mut BackfillFlushState,
    completion_flush: BackfillCompletionFlush,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
{
    let mut chain_epoch_id = next_chain_epoch_id(run.store)?;
    let mut batch = CanonicalBatch::default();
    let mut next_subtree_root_indexes =
        IngestSubtreeRootIndexes::from_tip_metadata(backfill_start.initial_tip_metadata);
    let mut last_commit_outcome = None;
    let mut retry_state = Some(IngestRetryState::default());
    let mut loop_flush_state = Some(std::mem::take(flush_state));
    let mut in_flight_commit = None;
    let mut running_tree_sizes =
        CommitmentTreeSizes::from_tip_metadata(backfill_start.initial_tip_metadata);
    let batch_budget = CanonicalBatchBudget::new(
        run.config.canonical_batch_max_blocks,
        run.config.canonical_batch_max_artifact_bytes,
    );
    futures_util::pin_mut!(fact_build_stream);

    loop {
        if in_flight_commit.is_some()
            && commit_reassembly_should_wait(run.config, batch.work_cost())
        {
            record_backfill_commit_reassembly_blocked();
            if let Err(error) = wait_for_in_flight_backfill_commit(
                &mut in_flight_commit,
                &mut next_subtree_root_indexes,
                &mut retry_state,
                &mut loop_flush_state,
                &mut last_commit_outcome,
            )
            .await
            {
                restore_backfill_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
        }

        let await_fact_build_started_at = Instant::now();
        let Some(fact_build_result) = fact_build_stream.next().await else {
            break;
        };
        record_backfill_stage_duration(
            BULK_STAGE_CANONICAL_FACT_BUILD,
            await_fact_build_started_at,
            fact_build_result.as_ref().err(),
        );
        let finalize_started_at = Instant::now();
        let built_outcome = fact_build_result.and_then(|derived| {
            finalize_derived_block(derived, &mut running_tree_sizes).map_err(IngestError::from)
        });
        record_backfill_stage_duration(
            BULK_STAGE_CANONICAL_FINALIZE,
            finalize_started_at,
            built_outcome.as_ref().err(),
        );
        let built = match built_outcome {
            Ok(built) => built,
            Err(error) => {
                if let Err(commit_error) = wait_for_in_flight_backfill_commit(
                    &mut in_flight_commit,
                    &mut next_subtree_root_indexes,
                    &mut retry_state,
                    &mut loop_flush_state,
                    &mut last_commit_outcome,
                )
                .await
                {
                    restore_backfill_flush_state(flush_state, &mut loop_flush_state);
                    return Err(commit_error);
                }
                restore_backfill_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
        };

        batch.absorb(built);
        let batch_cost = batch.work_cost();
        record_ingest_batch_work_cost(batch_cost);
        record_commit_reassembly_state(&batch);

        if let Some(commit_trigger) = batch_budget.commit_trigger(batch_cost) {
            if let Err(error) = wait_for_in_flight_backfill_commit(
                &mut in_flight_commit,
                &mut next_subtree_root_indexes,
                &mut retry_state,
                &mut loop_flush_state,
                &mut last_commit_outcome,
            )
            .await
            {
                restore_backfill_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
            record_backfill_batch_commit_trigger(run.config, batch_cost, commit_trigger);
            let commit_batch = std::mem::take(&mut batch);
            let commit_retry_state = retry_state
                .take()
                .ok_or(IngestError::BackfillProducedNoCommit)?;
            let commit_flush_state = loop_flush_state
                .take()
                .ok_or(IngestError::BackfillProducedNoCommit)?;
            in_flight_commit = Some(commit_backfill_batch(
                run,
                BackfillCommitRequest {
                    batch: commit_batch,
                    next_subtree_root_indexes,
                    retry_state: commit_retry_state,
                    flush_state: commit_flush_state,
                    chain_epoch_id,
                },
            ));
            record_backfill_commit_active(true);
            record_commit_reassembly_state(&batch);
            chain_epoch_id = next_chain_epoch_id_after(chain_epoch_id)?;
        }
    }

    if let Err(error) = wait_for_in_flight_backfill_commit(
        &mut in_flight_commit,
        &mut next_subtree_root_indexes,
        &mut retry_state,
        &mut loop_flush_state,
        &mut last_commit_outcome,
    )
    .await
    {
        restore_backfill_flush_state(flush_state, &mut loop_flush_state);
        return Err(error);
    }

    if !batch.is_empty() {
        let commit_retry_state = retry_state
            .take()
            .ok_or(IngestError::BackfillProducedNoCommit)?;
        let commit_flush_state = loop_flush_state
            .take()
            .ok_or(IngestError::BackfillProducedNoCommit)?;
        let committed_batch = match commit_backfill_batch(
            run,
            BackfillCommitRequest {
                batch,
                next_subtree_root_indexes,
                retry_state: commit_retry_state,
                flush_state: commit_flush_state,
                chain_epoch_id,
            },
        )
        .await
        {
            Ok(committed_batch) => committed_batch,
            Err(failure) => {
                loop_flush_state = Some(failure.flush_state);
                restore_backfill_flush_state(flush_state, &mut loop_flush_state);
                return Err(failure.error);
            }
        };
        apply_committed_backfill_batch(
            committed_batch,
            &mut next_subtree_root_indexes,
            &mut retry_state,
            &mut loop_flush_state,
            &mut last_commit_outcome,
        );
    }

    let mut restored_flush_state = loop_flush_state
        .take()
        .ok_or(IngestError::BackfillProducedNoCommit)?;
    if completion_flush.flushes_pending()
        && last_commit_outcome.is_some()
        && let Err(error) =
            flush_pending_backfill_writes(run.store, &mut restored_flush_state).await
    {
        *flush_state = restored_flush_state;
        return Err(error);
    }
    *flush_state = restored_flush_state;

    last_commit_outcome.ok_or(IngestError::BackfillProducedNoCommit)
}

type InFlightBackfillCommit<'a> =
    BoxFuture<'a, Result<CommittedBackfillBatch, BackfillCommitFailure>>;

struct CommittedBackfillBatch {
    commit_outcome: ChainEpochCommitOutcome,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: IngestRetryState,
    flush_state: BackfillFlushState,
}

struct BackfillCommitFailure {
    error: IngestError,
    retry_state: IngestRetryState,
    flush_state: BackfillFlushState,
}

struct BackfillCommitRequest {
    batch: CanonicalBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: IngestRetryState,
    flush_state: BackfillFlushState,
    chain_epoch_id: ChainEpochId,
}

fn commit_backfill_batch<'a, Source>(
    run: &'a BackfillRunContext<'_, Source>,
    request: BackfillCommitRequest,
) -> InFlightBackfillCommit<'a>
where
    Source: NodeSource,
{
    async move {
        let mut batch = request.batch;
        let mut retry_state = request.retry_state;
        let mut flush_state = request.flush_state;
        let updated_subtree_root_indexes = match populate_backfill_subtree_roots(
            run,
            &mut batch,
            request.next_subtree_root_indexes,
            &mut retry_state,
        )
        .await
        {
            Ok(updated_subtree_root_indexes) => updated_subtree_root_indexes,
            Err(error) => {
                return Err(BackfillCommitFailure {
                    error,
                    retry_state,
                    flush_state,
                });
            }
        };

        if let Err(error) =
            populate_backfill_tree_state_checkpoint(run, &mut batch, &mut retry_state).await
        {
            return Err(BackfillCommitFailure {
                error,
                retry_state,
                flush_state,
            });
        }

        let commit_outcome = match commit_finalized_backfill_batch(
            run.store,
            run.config.node.network,
            request.chain_epoch_id,
            batch,
        )
        .await
        {
            Ok((commit_outcome, _drained_batch)) => commit_outcome,
            Err(error) => {
                return Err(BackfillCommitFailure {
                    error,
                    retry_state,
                    flush_state,
                });
            }
        };

        flush_state.record_committed_epoch();
        if let Err(error) = flush_backfill_writes_if_due(run, &mut flush_state).await {
            return Err(BackfillCommitFailure {
                error,
                retry_state,
                flush_state,
            });
        }

        Ok(CommittedBackfillBatch {
            commit_outcome,
            next_subtree_root_indexes: updated_subtree_root_indexes,
            retry_state,
            flush_state,
        })
    }
    .boxed()
}

async fn wait_for_in_flight_backfill_commit(
    in_flight_commit: &mut Option<InFlightBackfillCommit<'_>>,
    next_subtree_root_indexes: &mut IngestSubtreeRootIndexes,
    retry_state: &mut Option<IngestRetryState>,
    flush_state: &mut Option<BackfillFlushState>,
    last_commit_outcome: &mut Option<ChainEpochCommitOutcome>,
) -> Result<(), IngestError> {
    let Some(commit) = in_flight_commit.take() else {
        return Ok(());
    };
    match commit.await {
        Ok(committed_batch) => {
            apply_committed_backfill_batch(
                committed_batch,
                next_subtree_root_indexes,
                retry_state,
                flush_state,
                last_commit_outcome,
            );
            record_backfill_commit_active(false);
            Ok(())
        }
        Err(failure) => {
            *retry_state = Some(failure.retry_state);
            *flush_state = Some(failure.flush_state);
            record_backfill_commit_active(false);
            Err(failure.error)
        }
    }
}

fn apply_committed_backfill_batch(
    committed_batch: CommittedBackfillBatch,
    next_subtree_root_indexes: &mut IngestSubtreeRootIndexes,
    retry_state: &mut Option<IngestRetryState>,
    flush_state: &mut Option<BackfillFlushState>,
    last_commit_outcome: &mut Option<ChainEpochCommitOutcome>,
) {
    *next_subtree_root_indexes = committed_batch.next_subtree_root_indexes;
    *retry_state = Some(committed_batch.retry_state);
    *flush_state = Some(committed_batch.flush_state);
    *last_commit_outcome = Some(committed_batch.commit_outcome);
}

fn restore_backfill_flush_state(
    target: &mut BackfillFlushState,
    source: &mut Option<BackfillFlushState>,
) {
    if let Some(flush_state) = source.take() {
        *target = flush_state;
    }
}

fn commit_reassembly_should_wait(config: &BackfillConfig, batch_cost: CanonicalBatchCost) -> bool {
    batch_cost.blocks > 0
        && batch_cost.artifact_bytes
            >= nonzero_u64_to_usize(config.commit_reassembly_max_queued_artifact_bytes)
}

fn record_commit_reassembly_state(batch: &CanonicalBatch) {
    let batch_cost = batch.work_cost();
    record_queue_depth(BULK_STAGE_COMMIT_REASSEMBLY, batch_cost.blocks);
    record_reorder_buffer(
        BULK_STAGE_COMMIT_REASSEMBLY,
        batch_cost.blocks,
        usize_to_u64_saturating(batch_cost.artifact_bytes),
    );
}

fn record_backfill_commit_reassembly_blocked() {
    metrics::counter!(
        "zinder_ingest_bulk_pipeline_watermark_blocked_total",
        "stage" => BULK_STAGE_COMMIT_REASSEMBLY
    )
    .increment(1);
}

fn record_backfill_commit_active(is_active: bool) {
    let active = if is_active { 1.0 } else { 0.0 };
    metrics::gauge!(
        "zinder_ingest_bulk_pipeline_active",
        "stage" => BULK_STAGE_CANONICAL_COMMIT
    )
    .set(active);
}

async fn flush_backfill_writes_if_due<Source>(
    run: &BackfillRunContext<'_, Source>,
    flush_state: &mut BackfillFlushState,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    if flush_state.should_flush(run.config.flush_interval_epochs) {
        flush_pending_backfill_writes(run.store, flush_state).await?;
    }
    Ok(())
}

async fn populate_backfill_subtree_roots<Source>(
    run: &BackfillRunContext<'_, Source>,
    batch: &mut CanonicalBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: &mut IngestRetryState,
) -> Result<IngestSubtreeRootIndexes, IngestError>
where
    Source: NodeSource,
{
    let started_at = Instant::now();
    let outcome = populate_subtree_root_artifacts(
        run.config.node.request_timeout,
        run.source,
        batch,
        next_subtree_root_indexes,
        retry_state,
    )
    .await;
    record_backfill_stage_duration(
        BULK_STAGE_SUBTREE_ROOT_ATTACHMENT,
        started_at,
        outcome.as_ref().err(),
    );
    outcome
}

async fn populate_backfill_tree_state_checkpoint<Source>(
    run: &BackfillRunContext<'_, Source>,
    batch: &mut CanonicalBatch,
    retry_state: &mut IngestRetryState,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    if !batch.tree_states.is_empty() {
        return Ok(());
    }
    let Some(tip_block) = batch.block_headers.last() else {
        return Ok(());
    };
    if !run
        .source
        .capabilities()
        .supports(zinder_source::NodeCapability::TreeState)
    {
        return Ok(());
    }

    let block_id = BlockId::new(tip_block.height, tip_block.block_hash);
    let started_at = Instant::now();
    let outcome = fetch_tree_state_for_block_with_retry(
        run.config.node.request_timeout,
        run.source,
        block_id,
        retry_state,
    )
    .await;
    record_backfill_stage_duration(
        BULK_STAGE_CHECKPOINT_TREE_STATE,
        started_at,
        outcome.as_ref().err(),
    );
    let source_tree_state = match outcome {
        Ok(source_tree_state) => source_tree_state,
        Err(IngestError::Source(SourceError::NodeCapabilityMissing { .. })) => return Ok(()),
        Err(error) => return Err(error),
    };
    batch.push_tree_state_checkpoint(TreeStateArtifact::new(
        source_tree_state.block_id.height,
        source_tree_state.block_id.hash,
        source_tree_state.payload_bytes,
    ));
    Ok(())
}

/// Wraps the synchronous `PrimaryChainStore::flush` in a `spawn_blocking`
/// so a multi-second `RocksDB` flush during `BulkCatchup` does not stall
/// the Tokio worker the backfill loop runs on.
async fn flush_primary_chain_store(store: &PrimaryChainStore) -> Result<(), IngestError> {
    let flush_started_at = Instant::now();
    let store = store.clone();
    let flush_outcome = tokio::task::spawn_blocking(move || store.flush())
        .await
        .map_err(|join_error| IngestError::BlockingTaskFailed {
            reason: join_error.to_string(),
        })?
        .map_err(IngestError::from);
    record_backfill_stage_duration(
        BULK_STAGE_CANONICAL_FLUSH,
        flush_started_at,
        flush_outcome.as_ref().err(),
    );
    flush_outcome
}

fn record_backfill_stage_duration(
    stage: &'static str,
    started_at: Instant,
    stage_error: Option<&IngestError>,
) {
    record_stage_duration(stage, started_at, stage_error);
}

fn record_backfill_batch_commit_trigger(
    config: &BackfillConfig,
    batch_cost: CanonicalBatchCost,
    commit_trigger: CanonicalBatchCloseTrigger,
) {
    record_ingest_batch_commit_trigger(commit_trigger);
    tracing::info!(
        target: "zinder::ingest",
        event = "bulk_catchup_batch_budget_reached",
        trigger = commit_trigger.metric_label(),
        block_count = batch_cost.blocks,
        transaction_count = batch_cost.transactions,
        transparent_output_count = batch_cost.transparent_outputs,
        transparent_spend_reference_count = batch_cost.transparent_spend_references,
        max_blocks = config.canonical_batch_max_blocks.get(),
        "bulk-catchup batch budget reached; committing accumulated artifacts"
    );
}

/// Commits a finalized backfill batch and returns the drained batch buffer.
async fn commit_finalized_backfill_batch(
    store: &PrimaryChainStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: CanonicalBatch,
) -> Result<(ChainEpochCommitOutcome, CanonicalBatch), IngestError> {
    let mut batch = batch;
    let outcome =
        commit_finalized_backfill_batch_inner(store, network, chain_epoch_id, &mut batch).await?;
    Ok((outcome, batch))
}

async fn commit_finalized_backfill_batch_inner(
    store: &PrimaryChainStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: &mut CanonicalBatch,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let tip_block = batch
        .block_headers
        .last()
        .ok_or(IngestError::EmptyCanonicalBatch)?;
    let tip_height = tip_block.height;
    let tip_hash = tip_block.block_hash;
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyCanonicalBatch)?;
    let chain_epoch = ChainEpoch {
        id: chain_epoch_id,
        network,
        tip_height,
        tip_hash,
        finalized_height: tip_height,
        finalized_hash: tip_hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata,
        created_at: current_unix_millis()?,
    };
    let commit_started_at = Instant::now();
    let commit_outcome = commit_ingest_batch(
        store,
        chain_epoch,
        batch,
        ReorgWindowChange::FinalizeThrough { height: tip_height },
    )
    .await;
    record_backfill_stage_duration(
        BULK_STAGE_CANONICAL_COMMIT,
        commit_started_at,
        commit_outcome.as_ref().err(),
    );
    commit_outcome
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackfillStart {
    from_height: BlockHeight,
    initial_tip_metadata: ChainTipMetadata,
}

/// Seeds an empty store with a stub chain epoch derived from the operator's
/// checkpoint, so backfill can start at `checkpoint.height + 1` without
/// replaying every block from genesis.
///
/// Returns `Ok(Some(chain_epoch))` after a successful bootstrap commit.
/// Returns `Ok(None)` when no bootstrap is needed (no checkpoint provided,
/// or store already has a chain epoch).
/// Returns `Err(BackfillCheckpointMisaligned)` when `from_height` does not
/// match `checkpoint.height + 1`.
fn bootstrap_from_checkpoint_if_needed(
    store: &PrimaryChainStore,
    network: Network,
    checkpoint: Option<SourceChainCheckpoint>,
    from_height: BlockHeight,
) -> Result<Option<ChainEpoch>, IngestError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    if store.current_chain_epoch()?.is_some() {
        return Ok(None);
    }
    let expected_from_height = checkpoint.height.next().unwrap_or(checkpoint.height);
    if from_height != expected_from_height {
        return Err(IngestError::BackfillCheckpointMisaligned {
            checkpoint_height: checkpoint.height,
            from_height,
        });
    }

    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network,
        tip_height: checkpoint.height,
        tip_hash: checkpoint.hash,
        finalized_height: checkpoint.height,
        finalized_hash: checkpoint.hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: checkpoint.tip_metadata,
        created_at: current_unix_millis()?,
    };
    let outcome = store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            bootstrap_chain_epoch,
            Vec::<zinder_core::BlockHeaderArtifact>::new(),
            Vec::new(),
        )
        .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
            height: checkpoint.height,
        }),
    )?;
    Ok(Some(outcome.chain_epoch))
}

fn backfill_start(
    current_chain_epoch: Option<ChainEpoch>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<Option<BackfillStart>, IngestError> {
    let Some(current_chain_epoch) = current_chain_epoch else {
        if from_height == BlockHeight::new(1) {
            return Ok(Some(BackfillStart {
                from_height,
                initial_tip_metadata: ChainTipMetadata::empty(),
            }));
        }

        return Err(IngestError::BackfillRequiresContiguousTipMetadata {
            from_height,
            current_tip_height: None,
        });
    };

    if current_chain_epoch.tip_height >= to_height {
        return Ok(None);
    }

    if let Some(next_height) = current_chain_epoch.tip_height.next()
        && from_height <= next_height
    {
        return Ok(Some(BackfillStart {
            from_height: next_height,
            initial_tip_metadata: current_chain_epoch.tip_metadata,
        }));
    }

    Err(IngestError::BackfillRequiresContiguousTipMetadata {
        from_height,
        current_tip_height: Some(current_chain_epoch.tip_height),
    })
}

fn validate_backfill_finality_bound(
    config: &BackfillConfig,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) -> Result<(), IngestError> {
    if config.allow_near_tip_finalize {
        return Ok(());
    }

    let maximum_historical_height =
        BlockHeight::new(tip_height.value().saturating_sub(reorg_window_blocks));
    if config.to_height <= maximum_historical_height {
        return Ok(());
    }

    Err(IngestError::NearTipBackfillRequiresExplicitFinalize {
        to_height: config.to_height,
        tip_height,
        reorg_window_blocks,
        maximum_historical_height,
    })
}

/// Emits a warning when the resolved checkpoint sits inside the upstream node's
/// reorg window.
///
/// The first reorg deeper than `tip - checkpoint_height` would surface
/// `ReorgWindowExceeded` with no recovery short of re-bootstrapping at a
/// deeper checkpoint, so operators need a heads-up before any state lands.
fn warn_if_checkpoint_within_reorg_window(
    config: &BackfillConfig,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) {
    let Some(checkpoint) = config.checkpoint.as_ref() else {
        return;
    };
    if !checkpoint_within_reorg_window(checkpoint.height, tip_height, reorg_window_blocks) {
        return;
    }
    let safe_floor = tip_height.value().saturating_sub(reorg_window_blocks);
    tracing::warn!(
        target: "zinder::ingest",
        event = "backfill_checkpoint_within_reorg_window",
        checkpoint_height = checkpoint.height.value(),
        tip_height = tip_height.value(),
        reorg_window_blocks,
        safe_checkpoint_floor = safe_floor,
        "checkpoint sits inside the node-reported reorg window; first reorg may surface ReorgWindowExceeded"
    );
}

const fn checkpoint_within_reorg_window(
    checkpoint_height: BlockHeight,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) -> bool {
    checkpoint_height.value() > tip_height.value().saturating_sub(reorg_window_blocks)
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        num::{NonZeroU32, NonZeroU64},
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
    };

    use futures_util::StreamExt as _;
    use parking_lot::Mutex;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockId, NetworkUpgradeActivation, SUBTREE_LEAF_COUNT, ShieldedProtocol,
        SubtreeRootHash, SubtreeRootIndex, UnixTimestampMillis, wire::encode_internal_block_hash,
    };
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
    use zinder_source::{
        NodeCapabilities, SourceBlock, SourceBlockHeader, SourceChainSegment,
        SourceChainSegmentLimits, SourceError, SourceSubtreeRoot, SourceSubtreeRoots,
        ZebraJsonRpcSource,
    };
    use zinder_store::ChainEventHistoryRequest;

    use crate::ArtifactDeriveError;

    use super::*;

    #[test]
    fn backfill_flush_state_preserves_epoch_cadence() -> Result<(), Box<dyn Error>> {
        let flush_interval = NonZeroU32::new(3).ok_or("invalid flush interval")?;
        let mut flush_state = BackfillFlushState::default();

        flush_state.record_committed_epoch();
        assert!(!flush_state.should_flush(flush_interval));
        assert_eq!(flush_state.pending_epoch_count(), 1);

        flush_state.record_committed_epoch();
        assert!(!flush_state.should_flush(flush_interval));
        assert_eq!(flush_state.pending_epoch_count(), 2);

        flush_state.record_committed_epoch();
        assert!(flush_state.should_flush(flush_interval));
        assert_eq!(flush_state.pending_epoch_count(), 3);

        flush_state.mark_flushed();
        assert_eq!(flush_state.pending_epoch_count(), 0);
        assert!(!flush_state.should_flush(flush_interval));
        Ok(())
    }

    #[test]
    fn source_segment_sizer_shrinks_after_response_split() -> Result<(), Box<dyn Error>> {
        let mut sizer = SourceSegmentSizer::new(
            NonZeroU32::new(32).ok_or("invalid max segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid target bytes")?,
            Arc::new(NetworkUpgradeActivations::empty(Network::ZcashRegtest)),
            BlockHeight::new(1),
        );

        sizer.record_segment(
            SourceChainSegmentStats::from_response_payload_bytes(20 * 1024 * 1024)
                .with_connected_blocks(32)
                .with_added_splits(1),
        );

        assert_eq!(
            sizer.blocks_for_remaining_range(BlockHeight::new(33), BlockHeight::new(100)),
            NonZeroU32::new(16)
        );
        Ok(())
    }

    #[test]
    fn source_segment_sizer_resets_density_at_network_upgrade() -> Result<(), Box<dyn Error>> {
        let activations = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![NetworkUpgradeActivation {
                branch_id: ConsensusBranchId::new(0x76b8_09bb),
                activation_height: BlockHeight::new(5),
                name: "Sapling".to_owned(),
            }],
        )?;
        let mut sizer = SourceSegmentSizer::new(
            NonZeroU32::new(32).ok_or("invalid max segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid target bytes")?,
            Arc::new(activations),
            BlockHeight::new(1),
        );
        sizer.record_segment(
            SourceChainSegmentStats::from_response_payload_bytes(20 * 1024 * 1024)
                .with_connected_blocks(32)
                .with_added_splits(1),
        );

        assert_eq!(
            sizer.blocks_for_remaining_range(BlockHeight::new(4), BlockHeight::new(100)),
            NonZeroU32::new(16)
        );
        assert_eq!(
            sizer.blocks_for_remaining_range(BlockHeight::new(5), BlockHeight::new(100)),
            NonZeroU32::new(32)
        );
        Ok(())
    }

    #[test]
    fn source_segment_sizer_uses_heaviest_density_sample() -> Result<(), Box<dyn Error>> {
        let mut sizer = SourceSegmentSizer::new(
            NonZeroU32::new(32).ok_or("invalid max segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid target bytes")?,
            Arc::new(NetworkUpgradeActivations::empty(Network::ZcashRegtest)),
            BlockHeight::new(1),
        );

        sizer.record_segment(
            SourceChainSegmentStats::from_response_payload_bytes(12 * 1024 * 1024)
                .with_connected_blocks(12),
        );
        sizer.record_segment(
            SourceChainSegmentStats::from_response_payload_bytes(12 * 1024 * 1024)
                .with_connected_blocks(4),
        );

        assert_eq!(
            sizer.blocks_for_remaining_range(BlockHeight::new(17), BlockHeight::new(100)),
            NonZeroU32::new(4)
        );
        Ok(())
    }

    #[test]
    fn backfill_readiness_state_reports_syncing_until_upstream_tip_hint()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("readiness-syncing-store");
        let mut config = test_backfill_config(&storage_path, 101, 150, 50, false)?;
        config.upstream_tip_hint = Some(BlockHeight::new(200));

        let state = backfill_readiness_state(&config, Some(150));

        assert!(matches!(
            state.cause,
            zinder_runtime::ReadinessCause::Syncing {
                lag_blocks: Some(50)
            }
        ));
        assert_eq!(state.current_height, Some(150));
        assert_eq!(state.target_height, Some(200));
        Ok(())
    }

    #[test]
    fn backfill_readiness_state_reports_ready_at_upstream_tip_hint() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("readiness-ready-store");
        let mut config = test_backfill_config(&storage_path, 101, 200, 50, false)?;
        config.upstream_tip_hint = Some(BlockHeight::new(200));

        let state = backfill_readiness_state(&config, Some(200));

        assert_eq!(state.cause, zinder_runtime::ReadinessCause::Ready);
        assert_eq!(state.current_height, Some(200));
        assert_eq!(state.target_height, Some(200));
        Ok(())
    }

    #[tokio::test]
    async fn backfill_rejects_near_tip_finalize_without_explicit_override()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("near-tip-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 101, 150, 50, false)?;

        let error = match backfill_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await
        {
            Ok(commit_outcome) => {
                return Err(format!("expected near-tip rejection, got {commit_outcome:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::NearTipBackfillRequiresExplicitFinalize {
                to_height,
                tip_height,
                reorg_window_blocks: 100,
                maximum_historical_height,
            } if to_height == BlockHeight::new(150)
                && tip_height == BlockHeight::new(200)
                && maximum_historical_height == BlockHeight::new(100)
        ));
        assert!(!storage_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn exact_divisor_backfill_returns_last_full_batch_outcome() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("exact-divisor-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 1, 10, 5, false)?;

        let commit_outcome = backfill_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.id, ChainEpochId::new(2));
        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(10));
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        assert_eq!(
            store
                .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?
                .len(),
            2
        );

        Ok(())
    }

    #[tokio::test]
    async fn fact_build_stream_fetches_source_segments_and_yields_ordered_blocks()
    -> Result<(), Box<dyn Error>> {
        let requested_segments = Arc::new(Mutex::new(Vec::new()));
        let source = RecordingSegmentSource {
            requested_segments: Arc::clone(&requested_segments),
            network: Network::ZcashRegtest,
        };
        let source_segment_sizer = Arc::new(Mutex::new(SourceSegmentSizer::new(
            NonZeroU32::new(2).ok_or("invalid segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid segment target bytes")?,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            BlockHeight::new(1),
        )));
        let fact_build_stream = build_fact_build_stream(
            &source,
            BackfillFactBuildStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                fact_build_concurrency: 2,
                fact_build_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid fact build artifact bytes")?,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(fact_build_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = fact_build_stream.next().await {
            observed_heights.push(next_block?.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(*requested_segments.lock(), vec![(1, 2), (3, 2), (5, 2)]);

        Ok(())
    }

    #[tokio::test]
    async fn source_fetch_schedules_past_slow_earlier_segment() -> Result<(), Box<dyn Error>> {
        let fetch_events = Arc::new(Mutex::new(Vec::new()));
        let source = DelayedSegmentSource {
            fetch_events: Arc::clone(&fetch_events),
            network: Network::ZcashRegtest,
        };
        let source_segment_sizer = Arc::new(Mutex::new(SourceSegmentSizer::new(
            NonZeroU32::new(2).ok_or("invalid segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid segment target bytes")?,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            BlockHeight::new(1),
        )));
        let fact_build_stream = build_fact_build_stream(
            &source,
            BackfillFactBuildStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(2)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(25 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                fact_build_concurrency: 2,
                fact_build_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid fact build artifact bytes")?,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(fact_build_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = fact_build_stream.next().await {
            observed_heights.push(next_block?.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        let fetch_events = fetch_events.lock().clone();
        let start_third_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Started {
                start_height: BlockHeight::new(5),
            },
        )?;
        let finish_first_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Finished {
                start_height: BlockHeight::new(1),
            },
        )?;
        assert!(
            start_third_segment < finish_first_segment,
            "expected later segment to start before the slow first segment finished; events: {fetch_events:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn source_fetch_watermark_blocks_segments_until_active_bytes_release()
    -> Result<(), Box<dyn Error>> {
        let fetch_events = Arc::new(Mutex::new(Vec::new()));
        let source = DelayedSegmentSource {
            fetch_events: Arc::clone(&fetch_events),
            network: Network::ZcashRegtest,
        };
        let source_segment_sizer = Arc::new(Mutex::new(SourceSegmentSizer::new(
            NonZeroU32::new(2).ok_or("invalid segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid segment target bytes")?,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            BlockHeight::new(1),
        )));
        let fact_build_stream = build_fact_build_stream(
            &source,
            BackfillFactBuildStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                fact_build_concurrency: 2,
                fact_build_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid fact build artifact bytes")?,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(fact_build_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = fact_build_stream.next().await {
            observed_heights.push(next_block?.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        let fetch_events = fetch_events.lock().clone();
        let finish_first_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Finished {
                start_height: BlockHeight::new(1),
            },
        )?;
        let start_second_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Started {
                start_height: BlockHeight::new(3),
            },
        )?;
        assert!(
            finish_first_segment < start_second_segment,
            "expected source byte watermark to block segment 3 until segment 1 released active bytes; events: {fetch_events:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn fact_build_schedules_past_slow_earlier_block() -> Result<(), Box<dyn Error>> {
        let derive_events = Arc::new(Mutex::new(Vec::new()));
        let source = RecordingSegmentSource {
            requested_segments: Arc::new(Mutex::new(Vec::new())),
            network: Network::ZcashRegtest,
        };
        let source_segment_sizer = Arc::new(Mutex::new(SourceSegmentSizer::new(
            NonZeroU32::new(2).ok_or("invalid segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid segment target bytes")?,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            BlockHeight::new(1),
        )));
        let derive_events_for_stream = Arc::clone(&derive_events);
        let fact_build_stream = build_fact_build_stream(
            &source,
            BackfillFactBuildStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                fact_build_concurrency: 2,
                fact_build_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid fact build artifact bytes")?,
            },
            move |source_block| {
                let derive_events = Arc::clone(&derive_events_for_stream);
                async move {
                    let height = source_block.height;
                    derive_events
                        .lock()
                        .push(FactBuildEvent::Started { height });
                    match height.value() {
                        1 => tokio::time::sleep(Duration::from_millis(80)).await,
                        2 => tokio::time::sleep(Duration::from_millis(10)).await,
                        _ => {}
                    }
                    derive_events
                        .lock()
                        .push(FactBuildEvent::Finished { height });
                    Ok(test_derived_block(&source_block, 0, 0))
                }
            },
        );
        futures_util::pin_mut!(fact_build_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = fact_build_stream.next().await {
            observed_heights.push(next_block?.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        let derive_events = derive_events.lock().clone();
        let start_third_block = fact_build_event_index(
            &derive_events,
            FactBuildEvent::Started {
                height: BlockHeight::new(3),
            },
        )?;
        let finish_first_block = fact_build_event_index(
            &derive_events,
            FactBuildEvent::Finished {
                height: BlockHeight::new(1),
            },
        )?;
        assert!(
            start_third_block < finish_first_block,
            "expected later fact build to start before the slow first block finished; events: {derive_events:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn backfill_retries_retryable_block_fetch_failures() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("retryable-source-store");
        let source = FlakyNodeSource {
            delegate: TestNodeSource {
                tip_height: BlockHeight::new(200),
                network: Network::ZcashRegtest,
            },
            failure: FlakySourceFailure::NodeUnavailable,
            retryable_failures_before_success: AtomicU32::new(2),
            fetch_attempts: AtomicU32::new(0),
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let commit_outcome = backfill_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        assert_eq!(source.fetch_attempts.load(Ordering::SeqCst), 3);

        Ok(())
    }

    // `BlockUnavailable` retry-then-success is intentionally NOT covered as a
    // per-call test: per [ADR-0013](../../../../docs/adrs/0013-source-failure-recovery-topology.md)
    // the per-call retry window only covers transient transport failures
    // (`NodeUnreachable`, `StreamDisconnected`). `UpstreamViewChanged` (which
    // `BlockUnavailable` maps to) is recoverable at the loop layer because
    // retrying the same RPC against a stale view cannot succeed; only
    // re-observing the upstream tip can. The loop-layer recovery contract
    // for `BlockUnavailable` is tested by
    // `source_recovery::tests::view_stale_block_unavailable_is_recoverable`.

    #[tokio::test]
    async fn backfill_does_not_retry_protocol_mismatch_failures() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("protocol-mismatch-store");
        let source = FlakyNodeSource {
            delegate: TestNodeSource {
                tip_height: BlockHeight::new(200),
                network: Network::ZcashRegtest,
            },
            failure: FlakySourceFailure::ProtocolMismatch,
            retryable_failures_before_success: AtomicU32::new(2),
            fetch_attempts: AtomicU32::new(0),
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let error = match backfill_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await
        {
            Ok(commit_outcome) => {
                return Err(format!("expected source error, got {commit_outcome:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::Source(SourceError::SourceProtocolMismatch { .. })
        ));
        assert_eq!(source.fetch_attempts.load(Ordering::SeqCst), 1);

        Ok(())
    }

    #[tokio::test]
    async fn source_retry_classification_uses_upstream_classification() {
        use zinder_source::SourceFailureClass;
        assert_eq!(
            SourceError::NodeUnavailable {
                reason: "connection reset".to_owned(),
            }
            .upstream_classification(),
            SourceFailureClass::NodeUnreachable,
        );
        assert_eq!(
            SourceError::BlockUnavailable {
                height: BlockHeight::new(1),
                reason: "block height not in best chain".to_owned(),
            }
            .upstream_classification(),
            SourceFailureClass::UpstreamViewChanged,
        );
        assert_eq!(
            SourceError::SourceProtocolMismatch {
                reason: "missing block hash",
            }
            .upstream_classification(),
            SourceFailureClass::ProtocolMismatch,
        );
    }

    #[tokio::test]
    async fn backfill_commits_newly_completed_subtree_roots() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("subtree-root-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let commit_outcome = backfill_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, SUBTREE_LEAF_COUNT, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let reader = store.current_chain_epoch_reader()?;
        let subtree_roots = reader.subtree_roots(zinder_core::SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(1).ok_or("invalid max entries")?,
        ))?;
        let subtree_root = subtree_roots
            .first()
            .and_then(Option::as_ref)
            .ok_or("missing committed subtree root")?;

        assert_eq!(subtree_root.protocol, ShieldedProtocol::Sapling);
        assert_eq!(subtree_root.subtree_index, SubtreeRootIndex::new(0));
        assert_eq!(subtree_root.root_hash.as_bytes(), [0x33; 32]);
        assert_eq!(subtree_root.completing_block_height, BlockHeight::new(1));
        assert_eq!(subtree_root.completing_block_hash, block_hash(1));

        Ok(())
    }

    #[tokio::test]
    async fn canonical_backfill_requires_genesis_or_contiguous_tree_size_base()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("non-genesis-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 2, 2, 1, false)?;

        let error = match backfill(&config, &source).await {
            Ok(commit_outcome) => {
                return Err(
                    format!("expected tree-size base rejection, got {commit_outcome:?}").into(),
                );
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::BackfillRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: None,
            } if from_height == BlockHeight::new(2)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn backfill_start_resumes_or_completes_from_current_tip() -> Result<(), Box<dyn Error>> {
        let tip_metadata = ChainTipMetadata::new(123, 456);
        let current_chain_epoch = test_chain_epoch(BlockHeight::new(9), tip_metadata);

        let contiguous_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(10),
            BlockHeight::new(20),
        )?
        .ok_or("contiguous range should need work")?;
        let resumed_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(20),
        )?
        .ok_or("partial rerun should need work")?;
        let completed_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(9),
        )?;
        let error = match backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(11),
            BlockHeight::new(20),
        ) {
            Ok(start) => {
                return Err(format!("expected non-contiguous rejection, got {start:?}").into());
            }
            Err(error) => error,
        };

        assert_eq!(contiguous_start.from_height, BlockHeight::new(10));
        assert_eq!(contiguous_start.initial_tip_metadata, tip_metadata);
        assert_eq!(resumed_start.from_height, BlockHeight::new(10));
        assert_eq!(resumed_start.initial_tip_metadata, tip_metadata);
        assert_eq!(completed_start, None);
        assert!(matches!(
            error,
            IngestError::BackfillRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: Some(current_tip_height),
            } if from_height == BlockHeight::new(11)
                && current_tip_height == BlockHeight::new(9)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn backfill_seeds_chain_epoch_from_checkpoint_then_extends() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-bootstrap-store");
        let checkpoint_height = BlockHeight::new(10);
        // Match the TestNodeSource's block hash convention so the first
        // backfilled block (height 11) finds the right parent linkage.
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Tree sizes well below SUBTREE_LEAF_COUNT so no subtree completes
        // during backfill; the unit test validates the bootstrap + extend
        // round-trip without spawning a real source subtree path.
        let checkpoint_tip_metadata = ChainTipMetadata::new(0, 0);
        let mut config = test_backfill_config(&storage_path, 11, 12, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            checkpoint_height,
            checkpoint_hash,
            checkpoint_tip_metadata,
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let commit_outcome = backfill_with_bootstrap_using_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(12));

        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let event_history =
            store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;
        // 1 bootstrap commit + 2 single-block backfill commits (heights 11
        // and 12 with canonical_batch_max_blocks = 1).
        assert_eq!(
            event_history.len(),
            3,
            "checkpoint bootstrap commit plus per-block backfill commits"
        );

        Ok(())
    }

    #[tokio::test]
    async fn backfill_from_checkpoint_skips_pre_checkpoint_subtree_root_indexes()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-subtree-indexes-store");
        let checkpoint_height = BlockHeight::new(10);
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Checkpoint encodes one already-completed Sapling subtree. Without
        // seeding `IngestSubtreeRootIndexes` from `tip_metadata`, the backfill
        // would ask the node for subtree 0 (completing far below the
        // batch range) and surface SubtreeRootCompletingBlockMissing. This
        // mirrors the live mainnet failure observed when calibrating against
        // a checkpoint at `tip - 1000`.
        let checkpoint_tip_metadata = ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0);
        let mut config = test_backfill_config(&storage_path, 11, 11, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            checkpoint_height,
            checkpoint_hash,
            checkpoint_tip_metadata,
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        // Checkpoint already carries SUBTREE_LEAF_COUNT outputs, so the
        // post-checkpoint block contributes a zero delta. The defense
        // under test is that the writer does not re-fetch the
        // already-recorded subtree root for the checkpoint range.
        let commit_outcome = backfill_with_bootstrap_using_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(11));
        Ok(())
    }

    #[test]
    fn checkpoint_within_reorg_window_marks_only_inside_window() {
        // Tip 200, window 100 -> safe historical floor at height 100. A
        // checkpoint at 99 finalizes outside the window; 100 sits exactly on
        // the floor so the next commit needs no rewind. Anything above 100 is
        // inside the window and should warn.
        assert!(!checkpoint_within_reorg_window(
            BlockHeight::new(99),
            BlockHeight::new(200),
            100,
        ));
        assert!(!checkpoint_within_reorg_window(
            BlockHeight::new(100),
            BlockHeight::new(200),
            100,
        ));
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(101),
            BlockHeight::new(200),
            100,
        ));
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(200),
            BlockHeight::new(200),
            100,
        ));
        // Tip below the window: the safe floor saturates at 0, so every
        // checkpoint above 0 is inside the window.
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(1),
            BlockHeight::new(50),
            100,
        ));
    }

    #[tokio::test]
    async fn backfill_rejects_misaligned_checkpoint() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("misaligned-checkpoint-store");
        let mut config = test_backfill_config(&storage_path, 50, 60, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            BlockHeight::new(10),
            BlockHash::from_bytes([0xa5; 32]),
            ChainTipMetadata::empty(),
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let error = match backfill(&config, &source).await {
            Ok(commit_outcome) => {
                return Err(format!(
                    "expected misaligned-checkpoint rejection, got {commit_outcome:?}"
                )
                .into());
            }
            Err(error) => error,
        };
        assert!(matches!(
            error,
            IngestError::BackfillCheckpointMisaligned {
                checkpoint_height,
                from_height,
            } if checkpoint_height == BlockHeight::new(10)
                && from_height == BlockHeight::new(50)
        ));

        Ok(())
    }

    /// Test helper that runs the checkpoint bootstrap and then the
    /// commit loop with `derive_fn` substituted for `derive_block`, so
    /// unit tests can exercise both phases without parsing real Zcash
    /// block bytes.
    #[cfg(test)]
    async fn backfill_with_bootstrap_using_mock_derive<Source, F>(
        config: &BackfillConfig,
        source: &Source,
        derive_fn: F,
    ) -> Result<ChainEpochCommitOutcome, IngestError>
    where
        Source: NodeSource,
        F: Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, ArtifactDeriveError>
            + Copy
            + Send
            + Sync,
    {
        let store_options = ChainStoreOptions::for_network(config.node.network);
        validate_backfill_finality_bound(
            config,
            source.tip_id().await?.height,
            store_options.reorg_window_blocks,
        )?;
        let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
        let bootstrapped = bootstrap_from_checkpoint_if_needed(
            &store,
            config.node.network,
            config.checkpoint,
            config.from_height,
        )?;
        let initial_tip_metadata = bootstrapped
            .map_or_else(ChainTipMetadata::empty, |chain_epoch| {
                chain_epoch.tip_metadata
            });
        backfill_from_source_with_store_using_derive_fn(
            config,
            source,
            &store,
            derive_fn,
            BackfillStart {
                from_height: config.from_height,
                initial_tip_metadata,
            },
        )
        .await
    }

    fn test_backfill_config(
        storage_path: &Path,
        from_height: u32,
        to_height: u32,
        canonical_batch_max_blocks: u32,
        allow_near_tip_finalize: bool,
    ) -> Result<BackfillConfig, Box<dyn Error>> {
        Ok(BackfillConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:39232".to_owned(),
                zinder_source::NodeAuth::None,
                std::time::Duration::from_secs(30),
                zinder_source::DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            node_source: NodeSourceKind::ZebraJsonRpc,
            storage_path: storage_path.to_owned(),
            storage_tuning: zinder_store::StorageTuning::for_local_tests(),
            raw_blob_policy: RawBlobPolicy::All,
            network_upgrade_activations: Arc::new(
                zinder_testkit::sample_regtest_upgrade_activations(),
            ),
            from_height: BlockHeight::new(from_height),
            to_height: BlockHeight::new(to_height),
            canonical_batch_max_blocks: NonZeroU32::new(canonical_batch_max_blocks)
                .ok_or("invalid test batch size")?,
            canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
                .ok_or("invalid test batch artifact bytes")?,
            source_segment_max_blocks: NonZeroU32::new(4)
                .ok_or("invalid test source segment blocks")?,
            source_segment_target_response_bytes: NonZeroU64::new(12 * 1024 * 1024)
                .ok_or("invalid test source segment target bytes")?,
            source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                .ok_or("invalid test source fetch requests")?,
            source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
                .ok_or("invalid test source fetch bytes")?,
            fact_build_concurrency: NonZeroU32::new(4).ok_or("invalid test derive concurrency")?,
            fact_build_max_in_flight_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                .ok_or("invalid test fact build artifact bytes")?,
            commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                .ok_or("invalid test commit reassembly bytes")?,
            flush_interval_epochs: NonZeroU32::new(5).ok_or("invalid test flush cadence")?,
            upstream_tip_hint: None,
            allow_near_tip_finalize,
            checkpoint: None,
        })
    }

    struct TestNodeSource {
        tip_height: BlockHeight,
        network: Network,
    }

    struct RecordingSegmentSource {
        requested_segments: Arc<Mutex<Vec<(u32, u32)>>>,
        network: Network,
    }

    struct DelayedSegmentSource {
        fetch_events: Arc<Mutex<Vec<SegmentFetchEvent>>>,
        network: Network,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SegmentFetchEvent {
        Started { start_height: BlockHeight },
        Finished { start_height: BlockHeight },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FactBuildEvent {
        Started { height: BlockHeight },
        Finished { height: BlockHeight },
    }

    struct FlakyNodeSource {
        delegate: TestNodeSource,
        failure: FlakySourceFailure,
        retryable_failures_before_success: AtomicU32,
        fetch_attempts: AtomicU32,
    }

    #[derive(Clone, Copy)]
    enum FlakySourceFailure {
        NodeUnavailable,
        ProtocolMismatch,
    }

    impl FlakySourceFailure {
        fn source_error(self, _height: BlockHeight) -> SourceError {
            match self {
                Self::NodeUnavailable => SourceError::NodeUnavailable {
                    reason: "temporary node outage".to_owned(),
                },
                Self::ProtocolMismatch => SourceError::SourceProtocolMismatch {
                    reason: "node payload missing required field",
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for DelayedSegmentSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_chain_segment(
            &self,
            limits: SourceChainSegmentLimits,
        ) -> Result<SourceChainSegment, SourceError> {
            let Some(start_height) = limits.cursor.next_connected_height() else {
                return Ok(SourceChainSegment::default());
            };
            self.fetch_events
                .lock()
                .push(SegmentFetchEvent::Started { start_height });

            match start_height.value() {
                1 => tokio::time::sleep(Duration::from_millis(80)).await,
                3 => tokio::time::sleep(Duration::from_millis(10)).await,
                _ => {}
            }

            self.fetch_events
                .lock()
                .push(SegmentFetchEvent::Finished { start_height });

            let end_height = BlockHeight::new(
                start_height
                    .value()
                    .saturating_add(limits.max_connected_blocks.get())
                    .saturating_sub(1)
                    .min(6),
            );
            let mut blocks = Vec::new();
            let mut next_height = Some(start_height);
            while let Some(height) = next_height {
                if height > end_height {
                    break;
                }
                blocks.push(test_source_block(self.network, height));
                next_height = height.next();
            }

            Ok(SourceChainSegment::connected_blocks(blocks))
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            let _ = height;
            Err(SourceError::SourceProtocolMismatch {
                reason: "single-block fetch should not be used by segment backfill",
            })
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(BlockHeight::new(6), block_hash(6)))
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for RecordingSegmentSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_chain_segment(
            &self,
            limits: SourceChainSegmentLimits,
        ) -> Result<SourceChainSegment, SourceError> {
            let Some(start_height) = limits.cursor.next_connected_height() else {
                return Ok(SourceChainSegment::default());
            };
            if start_height > BlockHeight::new(6) {
                return Ok(SourceChainSegment::default());
            }

            self.requested_segments
                .lock()
                .push((start_height.value(), limits.max_connected_blocks.get()));
            let end_height = BlockHeight::new(
                start_height
                    .value()
                    .saturating_add(limits.max_connected_blocks.get())
                    .saturating_sub(1)
                    .min(6),
            );
            let mut blocks = Vec::new();
            let mut next_height = Some(start_height);
            while let Some(height) = next_height {
                if height > end_height {
                    break;
                }
                blocks.push(test_source_block(self.network, height));
                next_height = height.next();
            }

            Ok(SourceChainSegment::connected_blocks(blocks))
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            let _ = height;
            Err(SourceError::SourceProtocolMismatch {
                reason: "single-block fetch should not be used by segment backfill",
            })
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(BlockHeight::new(6), block_hash(6)))
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for FlakyNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            self.delegate.capabilities()
        }

        async fn fetch_chain_segment(
            &self,
            limits: SourceChainSegmentLimits,
        ) -> Result<SourceChainSegment, SourceError> {
            self.fetch_attempts.fetch_add(1, Ordering::SeqCst);
            let start_height = limits.cursor.next_connected_height().ok_or(
                SourceError::SourceProtocolMismatch {
                    reason: "test segment cursor cannot connect a block",
                },
            )?;
            if self
                .retryable_failures_before_success
                .load(Ordering::SeqCst)
                > 0
            {
                self.retryable_failures_before_success
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(self.failure.source_error(start_height));
            }

            let end_height = BlockHeight::new(
                start_height
                    .value()
                    .saturating_add(limits.max_connected_blocks.get())
                    .saturating_sub(1),
            );
            let mut blocks = Vec::new();
            let mut next_height = Some(start_height);
            while let Some(height) = next_height {
                if height > end_height {
                    break;
                }
                blocks.push(self.delegate.fetch_block_at(height).await?);
                next_height = height.next();
            }
            Ok(SourceChainSegment::connected_blocks(blocks))
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            if self
                .retryable_failures_before_success
                .load(Ordering::SeqCst)
                > 0
            {
                self.retryable_failures_before_success
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(self.failure.source_error(height));
            }

            self.delegate.fetch_block_at(height).await
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            self.delegate.tip_id().await
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: ShieldedProtocol,
            start_index: SubtreeRootIndex,
            max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            self.delegate
                .fetch_subtree_roots(protocol, start_index, max_entries)
                .await
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for TestNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            Ok(test_source_block(self.network, height))
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(
                self.tip_height,
                block_hash(self.tip_height.value()),
            ))
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: ShieldedProtocol,
            start_index: SubtreeRootIndex,
            max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            let subtree_roots = (0..max_entries.get())
                .map(|offset| {
                    start_index
                        .value()
                        .checked_add(offset)
                        .map(SubtreeRootIndex::new)
                        .map(|index| {
                            SourceSubtreeRoot::new(
                                index,
                                SubtreeRootHash::from_bytes([0x33; 32]),
                                BlockHeight::new(1),
                            )
                        })
                        .ok_or(SourceError::SourceProtocolMismatch {
                            reason: "subtree roots response exceeds the SubtreeRootIndex range",
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(SourceSubtreeRoots::new(
                protocol,
                start_index,
                subtree_roots,
            ))
        }
    }

    fn fetch_event_index(
        events: &[SegmentFetchEvent],
        expected_event: SegmentFetchEvent,
    ) -> Result<usize, Box<dyn Error>> {
        events
            .iter()
            .position(|event| *event == expected_event)
            .ok_or_else(|| {
                format!("missing expected source fetch event: {expected_event:?}").into()
            })
    }

    fn fact_build_event_index(
        events: &[FactBuildEvent],
        expected_event: FactBuildEvent,
    ) -> Result<usize, Box<dyn Error>> {
        events
            .iter()
            .position(|event| *event == expected_event)
            .ok_or_else(|| format!("missing expected fact build event: {expected_event:?}").into())
    }

    fn test_source_block(network: Network, height: BlockHeight) -> SourceBlock {
        let source_hash = block_hash(height.value());
        let parent_hash = block_hash(height.value().saturating_sub(1));
        let header = SourceBlockHeader {
            network,
            height,
            hash: source_hash,
            parent_hash,
            block_time_seconds: 1_774_668_400,
        };

        SourceBlock::new(header, format!("raw-block-{}", height.value()).into_bytes())
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }

    fn test_chain_epoch(tip_height: BlockHeight, tip_metadata: ChainTipMetadata) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            tip_height,
            tip_hash: block_hash(tip_height.value()),
            finalized_height: tip_height,
            finalized_hash: block_hash(tip_height.value()),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata,
            created_at: UnixTimestampMillis::new(1_774_669_000_000),
        }
    }

    /// Constructs a synthetic [`DerivedBlockArtifacts`] for tests that
    /// drive the commit loop without parsing real Zcash bytes.
    ///
    /// The mock partial compact block carries the same identifiers the
    /// production derive would emit; `finalize_derived_block` then folds
    /// the supplied tree-size additions and stamps the final
    /// `chain_metadata` before encoding the proto.
    fn test_derived_block(
        source_block: &SourceBlock,
        sapling_tree_size_addition: u32,
        orchard_tree_size_addition: u32,
    ) -> DerivedBlockArtifacts {
        DerivedBlockArtifacts {
            block_header: zinder_core::BlockHeaderArtifact::new(
                source_block.height,
                source_block.hash,
                source_block.parent_hash,
                [0; 32],
                [0; 32],
                i64::from(source_block.block_time_seconds),
                0,
                [0; 32],
                0,
                u64::try_from(source_block.raw_block_bytes.len()).unwrap_or(u64::MAX),
            ),
            block_blob: Some(zinder_core::BlockBlobArtifact::new(
                source_block.height,
                source_block.hash,
                source_block.parent_hash,
                source_block.raw_block_bytes.clone(),
            )),
            partial_compact_block: LightwalletdCompactBlock {
                proto_version: 1,
                height: u64::from(source_block.height.value()),
                hash: encode_internal_block_hash(source_block.hash).to_vec(),
                prev_hash: encode_internal_block_hash(source_block.parent_hash).to_vec(),
                time: source_block.block_time_seconds,
                header: Vec::new(),
                vtx: Vec::new(),
                chain_metadata: None,
            },
            tree_size_additions: CommitmentTreeSizes {
                sapling: sapling_tree_size_addition,
                orchard: orchard_tree_size_addition,
            },
            block_transaction_index: Vec::new(),
            transaction_locations: Vec::new(),
            transaction_facts: Vec::new(),
            transaction_blobs: Vec::new(),
            address_output_index: Vec::new(),
            transparent_outputs_by_outpoint: Vec::new(),
        }
    }
}
