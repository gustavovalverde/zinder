use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use parking_lot::Mutex;
use zinder_core::{
    BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, Network, NetworkUpgradeActivations,
};
use zinder_runtime::{NodeUnavailableDetail, Readiness, ReadinessState};
use zinder_source::{NodeSource, NodeTarget, SourceChainCheckpoint, SourceFailureClass};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::artifact_builder::{RawBlobPolicy, derive_block_with_raw_blob_policy};
use crate::chain_ingest::{IngestError, NodeSourceKind, current_unix_millis};
use crate::phase::current_chain_height;
use crate::source_recovery::{
    SourceRecoveryDecision, decide_recovery, default_recovery_backoff, detail_for_new_outage,
    detail_for_ongoing_outage,
};
use block_prepare::{BulkCatchupBlockPrepareStreamConfig, build_block_prepare_stream};
use commit_reassembly::run_commit_reassembly;
pub(crate) use flush::flush_pending_bulk_catchup_writes;
use source_fetch::SourceSegmentSizer;
use watermark::record_stage_duration;

#[cfg(test)]
use crate::artifact_builder::{CommitmentTreeSizes, DerivedBlockArtifacts};

mod block_prepare;
mod commit_reassembly;
mod flush;
mod source_fetch;
mod watermark;

const SOURCE_SEGMENT_DENSITY_SAMPLE_LIMIT: usize = 64;
const SOURCE_SEGMENT_GROW_AFTER_SUCCESS_COUNT: u32 = 8;
const SOURCE_SEGMENT_GROW_NUMERATOR: u32 = 5;
const SOURCE_SEGMENT_GROW_DENOMINATOR: u32 = 4;

const BULK_STAGE_SOURCE_FETCH: &str = "source_fetch";
const BULK_STAGE_CANONICAL_BLOCK_PREPARE: &str = "canonical_block_prepare";
const BULK_STAGE_CANONICAL_FINALIZE: &str = "canonical_finalize";
const BULK_STAGE_SUBTREE_ROOT_ATTACHMENT: &str = "subtree_root_attachment";
const BULK_STAGE_CHECKPOINT_TREE_STATE: &str = "checkpoint_tree_state";
const BULK_STAGE_COMMIT_REASSEMBLY: &str = "commit_reassembly";
const BULK_STAGE_CANONICAL_COMMIT: &str = "canonical_commit";
const BULK_STAGE_CANONICAL_FLUSH: &str = "canonical_flush";

/// Configuration for one bounded historical bulk-catchup run.
#[derive(Clone, Debug)]
pub struct BulkCatchupRunConfig {
    /// Resolved upstream node endpoint (network, JSON-RPC URL, auth, timeout,
    /// response-size cap). See [`NodeTarget`].
    pub node: NodeTarget,
    /// Upstream node source implementation.
    pub node_source: NodeSourceKind,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// Bounded `RocksDB` resource budget applied when opening the canonical store.
    pub canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// First block height to ingest.
    pub from_height: BlockHeight,
    /// Last block height to ingest.
    pub to_height: BlockHeight,
    /// Maximum number of blocks committed in one chain epoch.
    pub canonical_batch_max_blocks: NonZeroU32,
    /// Maximum in-memory canonical artifact bytes accumulated before commit.
    pub canonical_batch_max_artifact_bytes: NonZeroU64,
    /// Maximum estimated canonical write bytes accumulated before commit.
    pub canonical_batch_max_estimated_write_bytes: NonZeroU64,
    /// Minimum batch size before estimated write bytes can close the batch.
    pub canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32,
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
    /// Operator-tunable via `ingest.bulk_catchup.block_prepare_concurrency`.
    /// See [ADR-0021](../../../../docs/adrs/0021-parallel-block-derivation.md).
    pub block_prepare_concurrency: NonZeroU32,
    /// Maximum reserved derived artifact bytes across active and completed
    /// block-prepare work.
    pub block_prepare_max_in_flight_artifact_bytes: NonZeroU64,
    /// Maximum safe-tip artifact bytes that can accumulate while the previous
    /// batch is attaching metadata, committing, or flushing.
    pub commit_reassembly_max_queued_artifact_bytes: NonZeroU64,
    /// Force a `RocksDB` flush after committing this many epochs. See
    /// [`crate::BulkCatchupConfig::flush_interval_epochs`].
    pub flush_interval_epochs: NonZeroU32,
    /// Optional raw-byte blob write policy.
    pub raw_blob_policy: RawBlobPolicy,
    /// Node-discovered consensus upgrade activations used for transaction facts.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    /// Pre-observed upstream tip height. When set, `run_bulk_catchup_with_store`
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
    /// `tip_metadata`, then begins bulk catchup from `checkpoint.height + 1`.
    /// `from_height` must equal `checkpoint.height + 1` in this mode. Reads
    /// at heights below the checkpoint return `ArtifactUnavailable`.
    pub checkpoint: Option<SourceChainCheckpoint>,
}

/// Mutable state carried across bulk-catchup batches.
///
/// The unified ingest loop invokes `run_bulk_catchup_until_complete` once per
/// bulk-catchup batch so it can re-classify the phase after each commit.
/// This state keeps the WAL flush cadence and source-density sizing tied to
/// the continuous bulk range rather than to that one-batch call boundary.
#[derive(Default)]
pub(crate) struct BulkCatchupFlushState {
    epochs_since_last_flush: u32,
    source_segment_sizer: Option<Arc<Mutex<SourceSegmentSizer>>>,
}

impl BulkCatchupFlushState {
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
        config: &BulkCatchupRunConfig,
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
enum BulkCatchupCompletionFlush {
    FlushPending,
    PreservePending,
}

impl BulkCatchupCompletionFlush {
    const fn flushes_pending(self) -> bool {
        matches!(self, Self::FlushPending)
    }
}

/// Stable dependencies for one bulk-catchup run.
///
/// Keeping these handles together prevents the bulk-catchup hot path from
/// growing long positional argument lists as the writer gains operational
/// state such as flush cadence and readiness reporting.
pub(crate) struct BulkCatchupRunContext<'a, Source> {
    config: &'a BulkCatchupRunConfig,
    source: &'a Source,
    store: &'a PrimaryChainStore,
}

impl<'a, Source> BulkCatchupRunContext<'a, Source> {
    pub(crate) const fn new(
        config: &'a BulkCatchupRunConfig,
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

/// Runs bulk catchup and commits the requested range to canonical storage.
///
/// Returns `Some(commit_outcome)` when at least one chain epoch was
/// committed and `None` when the requested range was already present
/// in the canonical store. Opens the [`PrimaryChainStore`] internally;
/// callers that want to share the store with other writers (e.g. an
/// `IngestControl` gRPC server that reads chain events during bulk catchup)
/// should open the store themselves and call [`run_bulk_catchup_with_store`]
/// instead.
pub async fn run_bulk_catchup<Source>(
    config: &BulkCatchupRunConfig,
    source: &Source,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let store_options = ChainStoreOptions {
        rocksdb_resource_budget: config.canonical_rocksdb_budget,
        raw_blob_retention: config.raw_blob_policy.to_retention(),
        ..ChainStoreOptions::for_network(config.node.network)
    };
    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    run_bulk_catchup_with_store(config, source, &store).await
}

/// Runs bulk catchup against a caller-owned [`PrimaryChainStore`].
///
/// Returns `Some(commit_outcome)` when at least one chain epoch was
/// committed and `None` when the requested range was already present in
/// the store. The supplied store must have been opened with the same
/// [`ChainStoreOptions`] bulk catchup expects
/// (`ChainStoreOptions::for_network(config.node.network)`); `RocksDB`
/// enforces a single primary handle per database, so a caller that
/// needs to expose readable surfaces (the `IngestControl` gRPC service)
/// during bulk catchup must open the store once and pass it to this entry
/// point.
///
/// When [`BulkCatchupRunConfig::upstream_tip_hint`] is `Some`, the call skips
/// its own `tip_id()` round-trip and uses the caller-supplied tip for
/// the finality-bound validation. The unified ingest loop sets the hint
/// from the tip it already observed at the top of each iteration, which
/// removes a serial RPC per batch on the bulk-catchup hot path.
pub async fn run_bulk_catchup_with_store<Source>(
    config: &BulkCatchupRunConfig,
    source: &Source,
    store: &PrimaryChainStore,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BulkCatchupFlushState::default();
    let run = BulkCatchupRunContext::new(config, source, store);
    run_bulk_catchup_with_store_inner(
        &run,
        &mut flush_state,
        BulkCatchupCompletionFlush::FlushPending,
    )
    .await
}

async fn run_bulk_catchup_with_store_inner<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    flush_state: &mut BulkCatchupFlushState,
    completion_flush: BulkCatchupCompletionFlush,
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
    validate_bulk_catchup_finality_bound(
        config,
        node_tip_height,
        store_options.reorg_window_blocks,
    )?;
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
    let Some(bulk_catchup_start) =
        bulk_catchup_start(current_chain_epoch, config.from_height, config.to_height)?
    else {
        // Range already covered; no new commit. Callers that need the
        // current chain epoch read it from `store.current_chain_epoch()`.
        let _ = current_chain_epoch.ok_or(IngestError::BulkCatchupProducedNoCommit)?;
        return Ok(None);
    };

    bulk_catchup_from_source_with_store(run, bulk_catchup_start, flush_state, completion_flush)
        .await
        .map(Some)
}

/// Runs a historical bulk catchup until the requested range is covered.
///
/// Returns the commit outcome (when at least one chain epoch was
/// committed) or `None` when the range was already covered. Retryable
/// upstream-node failures move readiness to `node_unavailable` and keep
/// polling instead of ending the writer process. Fatal configuration,
/// source protocol, storage, and artifact errors still return
/// immediately. One-shot callers that want process-fatal retry
/// deadlines should call [`run_bulk_catchup_with_store`] directly.
pub async fn run_bulk_catchup_until_complete<Source>(
    config: &BulkCatchupRunConfig,
    source: &Source,
    store: &PrimaryChainStore,
    readiness: &Readiness,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BulkCatchupFlushState::default();
    let run = BulkCatchupRunContext::new(config, source, store);
    run_bulk_catchup_until_complete_inner(
        &run,
        readiness,
        &mut flush_state,
        BulkCatchupCompletionFlush::FlushPending,
    )
    .await
}

pub(crate) async fn run_bulk_catchup_until_complete_with_flush_state<Source>(
    run: BulkCatchupRunContext<'_, Source>,
    readiness: &Readiness,
    flush_state: &mut BulkCatchupFlushState,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    run_bulk_catchup_until_complete_inner(
        &run,
        readiness,
        flush_state,
        BulkCatchupCompletionFlush::PreservePending,
    )
    .await
}

async fn run_bulk_catchup_until_complete_inner<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    readiness: &Readiness,
    flush_state: &mut BulkCatchupFlushState,
    completion_flush: BulkCatchupCompletionFlush,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let recovery_backoff = default_recovery_backoff();
    let mut outage: Option<(NodeUnavailableDetail, Instant)> = None;

    loop {
        match run_bulk_catchup_with_store_inner(run, flush_state, completion_flush).await {
            Ok(commit_outcome) => {
                let tip_height = match &commit_outcome {
                    Some(commit) => Some(commit.chain_epoch.visible_tip_height.value()),
                    None => run
                        .store
                        .current_chain_epoch()?
                        .map(|chain_epoch| chain_epoch.visible_tip_height.value()),
                };
                readiness.set(bulk_catchup_readiness_state(run.config, tip_height));
                if outage.take().is_some() {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "bulk_catchup_source_recovered",
                        "bulk catchup source recovered"
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
                    let detail = advance_bulk_catchup_outage(
                        &mut outage,
                        failure_class,
                        last_reason.clone(),
                    );
                    if detail.consecutive_failures == 1 {
                        tracing::warn!(
                            target: "zinder::ingest",
                            event = "bulk_catchup_source_unavailable",
                            failure_class = failure_class.label(),
                            error = %error,
                            "bulk catchup source is unavailable; keeping the writer alive and retrying"
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

fn bulk_catchup_readiness_state(
    config: &BulkCatchupRunConfig,
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

fn advance_bulk_catchup_outage(
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

async fn bulk_catchup_from_source_with_store<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    bulk_catchup_start: BulkCatchupStart,
    flush_state: &mut BulkCatchupFlushState,
    completion_flush: BulkCatchupCompletionFlush,
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
    let block_prepare_concurrency = config.block_prepare_concurrency.get() as usize;
    let block_prepare_stream = build_block_prepare_stream(
        run.source,
        BulkCatchupBlockPrepareStreamConfig {
            request_timeout,
            from_height: bulk_catchup_start.from_height,
            to_height: config.to_height,
            max_response_bytes: config.node.max_response_bytes,
            target_response_payload_bytes: config.source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes: config.source_fetch_max_in_flight_bytes,
            source_segment_sizer: flush_state
                .source_segment_sizer(config, bulk_catchup_start.from_height),
            block_prepare_concurrency,
            block_prepare_max_in_flight_artifact_bytes: config
                .block_prepare_max_in_flight_artifact_bytes,
            store: run.store.clone(),
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

    run_commit_reassembly(
        run,
        block_prepare_stream,
        bulk_catchup_start,
        flush_state,
        completion_flush,
    )
    .await
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

fn record_bulk_pipeline_stage_duration(
    stage: &'static str,
    started_at: Instant,
    stage_error: Option<&IngestError>,
) {
    record_stage_duration(stage, started_at, stage_error);
}

#[cfg(test)]
async fn bulk_catchup_from_source_with_mock_derive<Source, F>(
    config: &BulkCatchupRunConfig,
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
        rocksdb_resource_budget: config.canonical_rocksdb_budget,
        raw_blob_retention: config.raw_blob_policy.to_retention(),
        ..ChainStoreOptions::for_network(config.node.network)
    };
    validate_bulk_catchup_finality_bound(
        config,
        source.tip_id().await?.height,
        store_options.reorg_window_blocks,
    )?;

    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    bulk_catchup_from_source_with_store_using_derive_fn(
        config,
        source,
        &store,
        derive_fn,
        BulkCatchupStart {
            from_height: config.from_height,
            initial_tip_metadata: ChainTipMetadata::empty(),
        },
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "test seam mirrors the production bulk-catchup path plus an injected derive function"
)]
async fn bulk_catchup_from_source_with_store_using_derive_fn<Source, F>(
    config: &BulkCatchupRunConfig,
    source: &Source,
    store: &PrimaryChainStore,
    derive_fn: F,
    bulk_catchup_start: BulkCatchupStart,
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
    let block_prepare_concurrency = config.block_prepare_concurrency.get() as usize;
    let mut flush_state = BulkCatchupFlushState::default();
    let block_prepare_stream = build_block_prepare_stream(
        source,
        BulkCatchupBlockPrepareStreamConfig {
            request_timeout,
            from_height: bulk_catchup_start.from_height,
            to_height: config.to_height,
            max_response_bytes: config.node.max_response_bytes,
            target_response_payload_bytes: config.source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes: config.source_fetch_max_in_flight_bytes,
            source_segment_sizer: flush_state
                .source_segment_sizer(config, bulk_catchup_start.from_height),
            block_prepare_concurrency,
            block_prepare_max_in_flight_artifact_bytes: config
                .block_prepare_max_in_flight_artifact_bytes,
            store: store.clone(),
        },
        move |source_block| async move { derive_fn(&source_block).map_err(IngestError::from) },
    );

    let run = BulkCatchupRunContext::new(config, source, store);
    run_commit_reassembly(
        &run,
        block_prepare_stream,
        bulk_catchup_start,
        &mut flush_state,
        BulkCatchupCompletionFlush::FlushPending,
    )
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BulkCatchupStart {
    from_height: BlockHeight,
    initial_tip_metadata: ChainTipMetadata,
}

/// Seeds an empty store with a stub chain epoch derived from the operator's
/// checkpoint, so bulk catchup can start at `checkpoint.height + 1` without
/// replaying every block from genesis.
///
/// Returns `Ok(Some(chain_epoch))` after a successful bootstrap commit.
/// Returns `Ok(None)` when no bootstrap is needed (no checkpoint provided,
/// or store already has a chain epoch).
/// Returns `Err(BulkCatchupCheckpointMisaligned)` when `from_height` does not
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
        return Err(IngestError::BulkCatchupCheckpointMisaligned {
            checkpoint_height: checkpoint.height,
            from_height,
        });
    }

    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network,
        visible_tip_height: checkpoint.height,
        visible_tip_hash: checkpoint.hash,
        settled_tip_height: checkpoint.height,
        settled_tip_hash: checkpoint.hash,
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
        .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo {
            height: checkpoint.height,
        }),
    )?;
    Ok(Some(outcome.chain_epoch))
}

fn bulk_catchup_start(
    current_chain_epoch: Option<ChainEpoch>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<Option<BulkCatchupStart>, IngestError> {
    let Some(current_chain_epoch) = current_chain_epoch else {
        if from_height == BlockHeight::new(1) {
            return Ok(Some(BulkCatchupStart {
                from_height,
                initial_tip_metadata: ChainTipMetadata::empty(),
            }));
        }

        return Err(IngestError::BulkCatchupRequiresContiguousTipMetadata {
            from_height,
            current_tip_height: None,
        });
    };

    if current_chain_epoch.visible_tip_height >= to_height {
        return Ok(None);
    }

    if let Some(next_height) = current_chain_epoch.visible_tip_height.next()
        && from_height <= next_height
    {
        return Ok(Some(BulkCatchupStart {
            from_height: next_height,
            initial_tip_metadata: current_chain_epoch.tip_metadata,
        }));
    }

    Err(IngestError::BulkCatchupRequiresContiguousTipMetadata {
        from_height,
        current_tip_height: Some(current_chain_epoch.visible_tip_height),
    })
}

fn validate_bulk_catchup_finality_bound(
    config: &BulkCatchupRunConfig,
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

    Err(IngestError::NearTipBulkCatchupRequiresExplicitFinalize {
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
    config: &BulkCatchupRunConfig,
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
        event = "bulk_catchup_checkpoint_within_reorg_window",
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
        time::Duration,
    };

    use futures_util::StreamExt as _;
    use parking_lot::Mutex;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockId, ConsensusBranchId, NetworkUpgradeActivation, SUBTREE_LEAF_COUNT,
        ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex, TransactionFactsArtifact,
        TransactionId, TransactionLocation, TransparentAddressScriptHash, TransparentInputFact,
        TransparentOutPoint, TransparentUnspentOutput, UnixTimestampMillis,
        wire::encode_internal_block_hash,
    };
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
    use zinder_source::{
        NodeCapabilities, SourceBlock, SourceBlockHeader, SourceChainSegment,
        SourceChainSegmentLimits, SourceChainSegmentStats, SourceError, SourceSubtreeRoot,
        SourceSubtreeRoots, ZebraJsonRpcSource,
    };
    use zinder_store::ChainEventHistoryRequest;

    use crate::ArtifactDeriveError;

    use super::*;

    #[test]
    fn bulk_catchup_flush_state_preserves_epoch_cadence() -> Result<(), Box<dyn Error>> {
        let flush_interval = NonZeroU32::new(3).ok_or("invalid flush interval")?;
        let mut flush_state = BulkCatchupFlushState::default();

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
    fn bulk_catchup_readiness_state_reports_syncing_until_upstream_tip_hint()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("readiness-syncing-store");
        let mut config = test_bulk_catchup_run_config(&storage_path, 101, 150, 50, false)?;
        config.upstream_tip_hint = Some(BlockHeight::new(200));

        let state = bulk_catchup_readiness_state(&config, Some(150));

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
    fn bulk_catchup_readiness_state_reports_ready_at_upstream_tip_hint()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("readiness-ready-store");
        let mut config = test_bulk_catchup_run_config(&storage_path, 101, 200, 50, false)?;
        config.upstream_tip_hint = Some(BlockHeight::new(200));

        let state = bulk_catchup_readiness_state(&config, Some(200));

        assert_eq!(state.cause, zinder_runtime::ReadinessCause::Ready);
        assert_eq!(state.current_height, Some(200));
        assert_eq!(state.target_height, Some(200));
        Ok(())
    }

    #[tokio::test]
    async fn bulk_catchup_rejects_near_tip_finalize_without_explicit_override()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("near-tip-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_bulk_catchup_run_config(&storage_path, 101, 150, 50, false)?;

        let error = match bulk_catchup_from_source_with_mock_derive(&config, &source, |sb| {
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
            IngestError::NearTipBulkCatchupRequiresExplicitFinalize {
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
    async fn exact_divisor_bulk_catchup_returns_last_full_batch_outcome()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("exact-divisor-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_bulk_catchup_run_config(&storage_path, 1, 10, 5, false)?;

        let commit_outcome = bulk_catchup_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.id, ChainEpochId::new(2));
        assert_eq!(
            commit_outcome.chain_epoch.visible_tip_height,
            BlockHeight::new(10)
        );
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
    async fn block_prepare_stream_fetches_source_segments_and_yields_ordered_blocks()
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
        let (_store_tempdir, store) = test_primary_chain_store("block-prepare-ordered-store")?;
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
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
                block_prepare_concurrency: 2,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(block_prepare_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = block_prepare_stream.next().await {
            observed_heights.push(next_block?.derived.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(*requested_segments.lock(), vec![(1, 2), (3, 2), (5, 2)]);

        Ok(())
    }

    #[tokio::test]
    async fn block_prepare_stream_prefetches_spent_transparent_outputs()
    -> Result<(), Box<dyn Error>> {
        let (_store_tempdir, store) = test_primary_chain_store("prefetched-prevout-store")?;
        let funding_transaction_id = TransactionId::from_bytes([0x11; 32]);
        let spent_outpoint = TransparentOutPoint::new(funding_transaction_id, 0);
        seed_prefetched_output(&store, spent_outpoint)?;

        let requested_segments = Arc::new(Mutex::new(Vec::new()));
        let source = RecordingSegmentSource {
            requested_segments: Arc::clone(&requested_segments),
            network: Network::ZcashRegtest,
        };
        let source_segment_sizer = test_source_segment_sizer(BlockHeight::new(2), 1)?;
        let spending_transaction_id = TransactionId::from_bytes([0x22; 32]);
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(2),
                to_height: BlockHeight::new(2),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(1)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                block_prepare_concurrency: 1,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store: store.clone(),
            },
            move |source_block| async move {
                Ok(test_transparent_spend_block(
                    &source_block,
                    spending_transaction_id,
                    spent_outpoint,
                ))
            },
        );
        futures_util::pin_mut!(block_prepare_stream);

        let prepared = block_prepare_stream
            .next()
            .await
            .ok_or("missing prepared block")??;
        assert_eq!(prepared.derived.block_header.height, BlockHeight::new(2));
        assert_eq!(
            prepared.prefetched_spent_transparent_outputs,
            store
                .transparent_outputs_by_outpoints_for_writer_commit(
                    store
                        .current_chain_epoch()?
                        .ok_or("missing current epoch")?,
                    &[spent_outpoint],
                )?
                .into_values()
                .collect::<Vec<_>>()
        );
        assert!(block_prepare_stream.next().await.is_none());

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
        let (_store_tempdir, store) = test_primary_chain_store("slow-segment-prefetch-store")?;
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(2)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(48 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                block_prepare_concurrency: 2,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(block_prepare_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = block_prepare_stream.next().await {
            observed_heights.push(next_block?.derived.block_header.height.value());
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
    async fn source_fetch_counts_completed_reassembly_bytes_against_watermark()
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
        let (_store_tempdir, store) = test_primary_chain_store("reassembly-bytes-prefetch-store")?;
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(36 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                block_prepare_concurrency: 2,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(block_prepare_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = block_prepare_stream.next().await {
            observed_heights.push(next_block?.derived.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        let fetch_events = fetch_events.lock().clone();
        let finish_first_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Finished {
                start_height: BlockHeight::new(1),
            },
        )?;
        let start_third_segment = fetch_event_index(
            &fetch_events,
            SegmentFetchEvent::Started {
                start_height: BlockHeight::new(5),
            },
        )?;
        assert!(
            finish_first_segment < start_third_segment,
            "expected completed out-of-order segment bytes to block segment 5 until segment 1 emitted; events: {fetch_events:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn source_fetch_watermark_covers_active_and_completed_bytes() -> Result<(), Box<dyn Error>>
    {
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
        let (_store_tempdir, store) = test_primary_chain_store("watermark-prefetch-store")?;
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
                request_timeout: Duration::from_secs(30),
                from_height: BlockHeight::new(1),
                to_height: BlockHeight::new(6),
                max_response_bytes: NonZeroU64::new(16 * 1024 * 1024)
                    .ok_or("invalid max response bytes")?,
                target_response_payload_bytes: NonZeroU64::new(12 * 1024 * 1024)
                    .ok_or("invalid target response bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(24 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                source_segment_sizer,
                block_prepare_concurrency: 2,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store,
            },
            |source_block| async move { Ok(test_derived_block(&source_block, 0, 0)) },
        );
        futures_util::pin_mut!(block_prepare_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = block_prepare_stream.next().await {
            observed_heights.push(next_block?.derived.block_header.height.value());
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
    async fn block_prepare_schedules_past_slow_earlier_block() -> Result<(), Box<dyn Error>> {
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
        let (_store_tempdir, store) =
            test_primary_chain_store("slow-block-prepare-prefetch-store")?;
        let block_prepare_stream = build_block_prepare_stream(
            &source,
            BulkCatchupBlockPrepareStreamConfig {
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
                block_prepare_concurrency: 2,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(32 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                store,
            },
            move |source_block| {
                let derive_events = Arc::clone(&derive_events_for_stream);
                async move {
                    let height = source_block.height;
                    derive_events
                        .lock()
                        .push(BlockPrepareEvent::Started { height });
                    match height.value() {
                        1 => tokio::time::sleep(Duration::from_millis(80)).await,
                        2 => tokio::time::sleep(Duration::from_millis(10)).await,
                        _ => {}
                    }
                    derive_events
                        .lock()
                        .push(BlockPrepareEvent::Finished { height });
                    Ok(test_derived_block(&source_block, 0, 0))
                }
            },
        );
        futures_util::pin_mut!(block_prepare_stream);
        let mut observed_heights = Vec::new();
        while let Some(next_block) = block_prepare_stream.next().await {
            observed_heights.push(next_block?.derived.block_header.height.value());
        }

        assert_eq!(observed_heights, vec![1, 2, 3, 4, 5, 6]);
        let derive_events = derive_events.lock().clone();
        let start_third_block = block_prepare_event_index(
            &derive_events,
            BlockPrepareEvent::Started {
                height: BlockHeight::new(3),
            },
        )?;
        let finish_first_block = block_prepare_event_index(
            &derive_events,
            BlockPrepareEvent::Finished {
                height: BlockHeight::new(1),
            },
        )?;
        assert!(
            start_third_block < finish_first_block,
            "expected later block prepare to start before the slow first block finished; events: {derive_events:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn bulk_catchup_retries_retryable_block_fetch_failures() -> Result<(), Box<dyn Error>> {
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
        let config = test_bulk_catchup_run_config(&storage_path, 1, 1, 1, false)?;

        let commit_outcome = bulk_catchup_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, 0, 0))
        })
        .await?;

        assert_eq!(
            commit_outcome.chain_epoch.visible_tip_height,
            BlockHeight::new(1)
        );
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
    async fn bulk_catchup_does_not_retry_protocol_mismatch_failures() -> Result<(), Box<dyn Error>>
    {
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
        let config = test_bulk_catchup_run_config(&storage_path, 1, 1, 1, false)?;

        let error = match bulk_catchup_from_source_with_mock_derive(&config, &source, |sb| {
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
    async fn bulk_catchup_commits_newly_completed_subtree_roots() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("subtree-root-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_bulk_catchup_run_config(&storage_path, 1, 1, 1, false)?;

        let commit_outcome = bulk_catchup_from_source_with_mock_derive(&config, &source, |sb| {
            Ok(test_derived_block(sb, SUBTREE_LEAF_COUNT, 0))
        })
        .await?;

        assert_eq!(
            commit_outcome.chain_epoch.visible_tip_height,
            BlockHeight::new(1)
        );
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
    async fn canonical_bulk_catchup_requires_genesis_or_contiguous_tree_size_base()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("non-genesis-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_bulk_catchup_run_config(&storage_path, 2, 2, 1, false)?;

        let error = match run_bulk_catchup(&config, &source).await {
            Ok(commit_outcome) => {
                return Err(
                    format!("expected tree-size base rejection, got {commit_outcome:?}").into(),
                );
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::BulkCatchupRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: None,
            } if from_height == BlockHeight::new(2)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn bulk_catchup_start_resumes_or_completes_from_current_tip() -> Result<(), Box<dyn Error>>
    {
        let tip_metadata = ChainTipMetadata::new(123, 456);
        let current_chain_epoch = test_chain_epoch(BlockHeight::new(9), tip_metadata);

        let contiguous_start = bulk_catchup_start(
            Some(current_chain_epoch),
            BlockHeight::new(10),
            BlockHeight::new(20),
        )?
        .ok_or("contiguous range should need work")?;
        let resumed_start = bulk_catchup_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(20),
        )?
        .ok_or("partial rerun should need work")?;
        let completed_start = bulk_catchup_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(9),
        )?;
        let error = match bulk_catchup_start(
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
            IngestError::BulkCatchupRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: Some(current_tip_height),
            } if from_height == BlockHeight::new(11)
                && current_tip_height == BlockHeight::new(9)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn bulk_catchup_seeds_chain_epoch_from_checkpoint_then_extends()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-bootstrap-store");
        let checkpoint_height = BlockHeight::new(10);
        // Match the TestNodeSource's block hash convention so the first
        // bulk-caught-up block (height 11) finds the right parent linkage.
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Tree sizes well below SUBTREE_LEAF_COUNT so no subtree completes
        // during bulk catchup; the unit test validates the bootstrap + extend
        // round-trip without spawning a real source subtree path.
        let checkpoint_tip_metadata = ChainTipMetadata::new(0, 0);
        let mut config = test_bulk_catchup_run_config(&storage_path, 11, 12, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            checkpoint_height,
            checkpoint_hash,
            checkpoint_tip_metadata,
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let commit_outcome =
            bulk_catchup_with_bootstrap_using_mock_derive(&config, &source, |sb| {
                Ok(test_derived_block(sb, 0, 0))
            })
            .await?;

        assert_eq!(
            commit_outcome.chain_epoch.visible_tip_height,
            BlockHeight::new(12)
        );

        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let event_history =
            store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;
        // 1 bootstrap commit + 2 single-block bulk catchup commits (heights 11
        // and 12 with canonical_batch_max_blocks = 1).
        assert_eq!(
            event_history.len(),
            3,
            "checkpoint bootstrap commit plus per-block bulk catchup commits"
        );

        Ok(())
    }

    #[tokio::test]
    async fn bulk_catchup_from_checkpoint_skips_pre_checkpoint_subtree_root_indexes()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-subtree-indexes-store");
        let checkpoint_height = BlockHeight::new(10);
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Checkpoint encodes one already-completed Sapling subtree. Without
        // seeding `IngestSubtreeRootIndexes` from `tip_metadata`, the bulk catchup
        // would ask the node for subtree 0 (completing far below the
        // batch range) and surface SubtreeRootCompletingBlockMissing. This
        // mirrors the live mainnet failure observed when calibrating against
        // a checkpoint at `tip - 1000`.
        let checkpoint_tip_metadata = ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0);
        let mut config = test_bulk_catchup_run_config(&storage_path, 11, 11, 1, true)?;
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
        let commit_outcome =
            bulk_catchup_with_bootstrap_using_mock_derive(&config, &source, |sb| {
                Ok(test_derived_block(sb, 0, 0))
            })
            .await?;

        assert_eq!(
            commit_outcome.chain_epoch.visible_tip_height,
            BlockHeight::new(11)
        );
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
    async fn bulk_catchup_rejects_misaligned_checkpoint() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("misaligned-checkpoint-store");
        let mut config = test_bulk_catchup_run_config(&storage_path, 50, 60, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            BlockHeight::new(10),
            BlockHash::from_bytes([0xa5; 32]),
            ChainTipMetadata::empty(),
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let error = match run_bulk_catchup(&config, &source).await {
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
            IngestError::BulkCatchupCheckpointMisaligned {
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
    async fn bulk_catchup_with_bootstrap_using_mock_derive<Source, F>(
        config: &BulkCatchupRunConfig,
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
        validate_bulk_catchup_finality_bound(
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
        bulk_catchup_from_source_with_store_using_derive_fn(
            config,
            source,
            &store,
            derive_fn,
            BulkCatchupStart {
                from_height: config.from_height,
                initial_tip_metadata,
            },
        )
        .await
    }

    fn test_bulk_catchup_run_config(
        storage_path: &Path,
        from_height: u32,
        to_height: u32,
        canonical_batch_max_blocks: u32,
        allow_near_tip_finalize: bool,
    ) -> Result<BulkCatchupRunConfig, Box<dyn Error>> {
        Ok(BulkCatchupRunConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:39232".to_owned(),
                zinder_source::NodeAuth::None,
                std::time::Duration::from_secs(30),
                zinder_source::DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            node_source: NodeSourceKind::ZebraJsonRpc,
            storage_path: storage_path.to_owned(),
            canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
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
            canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
                crate::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
            )
            .ok_or("invalid test estimated write byte budget")?,
            canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
                crate::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
            )
            .ok_or("invalid test estimated write close floor")?,
            source_segment_max_blocks: NonZeroU32::new(4)
                .ok_or("invalid test source segment blocks")?,
            source_segment_target_response_bytes: NonZeroU64::new(12 * 1024 * 1024)
                .ok_or("invalid test source segment target bytes")?,
            source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                .ok_or("invalid test source fetch requests")?,
            source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
                .ok_or("invalid test source fetch bytes")?,
            block_prepare_concurrency: NonZeroU32::new(4)
                .ok_or("invalid test derive concurrency")?,
            block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                .ok_or("invalid test block prepare artifact bytes")?,
            commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                .ok_or("invalid test commit reassembly bytes")?,
            flush_interval_epochs: NonZeroU32::new(5).ok_or("invalid test flush cadence")?,
            upstream_tip_hint: None,
            allow_near_tip_finalize,
            checkpoint: None,
        })
    }

    fn test_primary_chain_store(
        storage_name: &str,
    ) -> Result<(tempfile::TempDir, PrimaryChainStore), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join(storage_name);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        Ok((tempdir, store))
    }

    fn seed_prefetched_output(
        store: &PrimaryChainStore,
        spent_outpoint: TransparentOutPoint,
    ) -> Result<(), Box<dyn Error>> {
        let funding_fixture =
            zinder_testkit::ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
        let funding_block = funding_fixture
            .block_at(BlockHeight::new(1))
            .ok_or("missing funding block")?;
        let funding_block_height = funding_block.height;
        let funding_block_hash = funding_block.hash;
        let script_pub_key = vec![0x76, 0xa9, 0x14, 0x88];
        let funding_fixture =
            funding_fixture.with_address_output_index(TransparentUnspentOutput::new(
                TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                script_pub_key,
                spent_outpoint,
                42,
                funding_block_height,
                funding_block_hash,
            ));
        let funding_artifacts = funding_fixture
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("missing funding artifacts")?;
        store.commit_chain_epoch(funding_artifacts)?;
        Ok(())
    }

    fn test_source_segment_sizer(
        from_height: BlockHeight,
        max_segment_blocks: u32,
    ) -> Result<Arc<Mutex<SourceSegmentSizer>>, Box<dyn Error>> {
        Ok(Arc::new(Mutex::new(SourceSegmentSizer::new(
            NonZeroU32::new(max_segment_blocks).ok_or("invalid segment blocks")?,
            NonZeroU64::new(12 * 1024 * 1024).ok_or("invalid segment target bytes")?,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            from_height,
        ))))
    }

    fn test_transparent_spend_block(
        source_block: &SourceBlock,
        spending_transaction_id: TransactionId,
        spent_outpoint: TransparentOutPoint,
    ) -> DerivedBlockArtifacts {
        let mut derived = test_derived_block(source_block, 0, 0);
        let location = TransactionLocation::new(
            spending_transaction_id,
            source_block.height,
            source_block.hash,
            0,
        );
        let transaction_facts = TransactionFactsArtifact::new(
            location,
            zinder_testkit::synthetic_transaction_public_facts(spending_transaction_id, 64),
        )
        .with_transparent_facts(
            vec![TransparentInputFact::new(0, spent_outpoint)],
            Vec::new(),
        );
        derived.transaction_facts.push(transaction_facts);
        derived
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
    enum BlockPrepareEvent {
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

            Ok(SourceChainSegment::connected_blocks_with_stats(
                blocks,
                SourceChainSegmentStats::from_response_payload_bytes(12 * 1024 * 1024),
            ))
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            let _ = height;
            Err(SourceError::SourceProtocolMismatch {
                reason: "single-block fetch should not be used by segment bulk catchup",
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
                reason: "single-block fetch should not be used by segment bulk catchup",
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

    fn block_prepare_event_index(
        events: &[BlockPrepareEvent],
        expected_event: BlockPrepareEvent,
    ) -> Result<usize, Box<dyn Error>> {
        events
            .iter()
            .position(|event| *event == expected_event)
            .ok_or_else(|| {
                format!("missing expected block prepare event: {expected_event:?}").into()
            })
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
            visible_tip_height: tip_height,
            visible_tip_hash: block_hash(tip_height.value()),
            settled_tip_height: tip_height,
            settled_tip_hash: block_hash(tip_height.value()),
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
            transparent_outputs_by_outpoint: Vec::new(),
        }
    }
}
