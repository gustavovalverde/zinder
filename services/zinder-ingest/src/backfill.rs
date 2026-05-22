use std::{
    future::Future,
    num::NonZeroU32,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures_util::stream::StreamExt;
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
};
use zinder_runtime::{NodeUnavailableDetail, Readiness, ReadinessState};
use zinder_source::{
    NodeSource, NodeTarget, SourceBlock, SourceChainCheckpoint, SourceFailureClass,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::artifact_builder::{
    CommitmentTreeSizes, DerivedBlockArtifacts, derive_block, finalize_derived_block,
};
use crate::chain_ingest::{
    IngestBatch, IngestBatchBudget, IngestBatchCommitTrigger, IngestBatchWorkCost, IngestError,
    IngestRetryState, IngestSubtreeRootIndexes, NodeSourceKind, commit_ingest_batch,
    current_unix_millis, fetch_block_with_retry, ingest_error_class, next_chain_epoch_id,
    next_chain_epoch_id_after, populate_subtree_root_artifacts, record_ingest_batch_commit_trigger,
    record_ingest_batch_work_cost, record_ingest_derive_outcome,
};
use crate::phase::current_chain_height;
use crate::source_recovery::{
    SourceRecoveryDecision, decide_recovery, default_recovery_backoff, detail_for_new_outage,
    detail_for_ongoing_outage,
};

const BACKFILL_STAGE_AWAIT_DERIVED_BLOCK: &str = "await_derived_block";
const BACKFILL_STAGE_POPULATE_SUBTREE_ROOTS: &str = "populate_subtree_roots";
const BACKFILL_STAGE_FLUSH_STORE: &str = "flush_store";

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
    pub commit_batch_blocks: NonZeroU32,
    /// Maximum unique transparent prevouts read from the store per chain epoch.
    pub max_transparent_prevout_store_lookups_per_batch: NonZeroU32,
    /// Number of historical block fetches kept in flight against the
    /// upstream node. Zebra's JSON-RPC serves one fetch per request and
    /// each block costs three concurrent calls (`getblockheader`,
    /// `getblock`, `z_gettreestate`); buffering hides the round-trip
    /// latency until the node's connection pool or CPU saturates.
    /// Operator-tunable via `ingest.bulk_catchup.fetch_concurrency`.
    pub fetch_concurrency: NonZeroU32,
    /// Number of parallel `derive_block` invocations kept in flight on the
    /// Tokio blocking pool. Per-block derivation is CPU-bound (block
    /// deserialization, per-tx canonical re-serialization, compact-block
    /// proto encoding, per-output `SHA256(script_pub_key)`); parallelism
    /// scales nearly linearly with cores up to the commit-batch boundary.
    /// Operator-tunable via `ingest.derive.concurrency`.
    /// See [ADR-0021](../../../docs/adrs/0021-parallel-block-derivation.md).
    pub derive_concurrency: NonZeroU32,
    /// Force a `RocksDB` flush after committing this many epochs. See
    /// [`crate::BulkCatchupConfig::flush_interval_epochs`].
    pub flush_interval_epochs: NonZeroU32,
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

/// Mutable flush cadence carried across bulk-catchup backfill batches.
///
/// The unified ingest loop invokes `backfill_until_complete` once per
/// bulk-catchup batch so it can re-classify the phase after each commit.
/// This state keeps the WAL flush cadence tied to committed epochs rather
/// than to that one-batch call boundary.
#[derive(Debug, Default)]
pub(crate) struct BackfillFlushState {
    epochs_since_last_flush: u32,
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
    derive_store: &'a zinder_derive::DeriveStore,
}

impl<'a, Source> BackfillRunContext<'a, Source> {
    pub(crate) const fn new(
        config: &'a BackfillConfig,
        source: &'a Source,
        store: &'a PrimaryChainStore,
        derive_store: &'a zinder_derive::DeriveStore,
    ) -> Self {
        Self {
            config,
            source,
            store,
            derive_store,
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
    let derive_store = crate::derive_consumers::open_primary_derive_store_for_canonical(
        &config.storage_path,
        config.storage_tuning,
    )?;
    backfill_with_store(config, source, &store, &derive_store).await
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
    derive_store: &zinder_derive::DeriveStore,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BackfillFlushState::default();
    let run = BackfillRunContext::new(config, source, store, derive_store);
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
    derive_store: &zinder_derive::DeriveStore,
    readiness: &Readiness,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError>
where
    Source: NodeSource,
{
    let mut flush_state = BackfillFlushState::default();
    let run = BackfillRunContext::new(config, source, store, derive_store);
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
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let fetch_concurrency = config.fetch_concurrency.get() as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let derive_concurrency = config.derive_concurrency.get() as usize;
    let derive_stream = build_derive_stream(
        run.source,
        BackfillDeriveStreamConfig {
            request_timeout,
            from_height: backfill_start.from_height,
            to_height: config.to_height,
            fetch_concurrency,
            derive_concurrency,
        },
        |source_block| async move {
            tokio::task::spawn_blocking(move || {
                derive_block(&source_block).map_err(IngestError::from)
            })
            .await
            .map_err(|join_error| IngestError::BlockingTaskFailed {
                reason: join_error.to_string(),
            })?
        },
    );

    run_backfill_commit_loop(
        run,
        derive_stream,
        backfill_start,
        flush_state,
        completion_flush,
    )
    .await
}

#[derive(Clone, Copy)]
struct BackfillDeriveStreamConfig {
    request_timeout: Duration,
    from_height: BlockHeight,
    to_height: BlockHeight,
    fetch_concurrency: usize,
    derive_concurrency: usize,
}

fn build_derive_stream<'a, Source, F, Fut>(
    source: &'a Source,
    config: BackfillDeriveStreamConfig,
    derive_fn: F,
) -> impl futures_util::Stream<Item = Result<DerivedBlockArtifacts, IngestError>> + Unpin + 'a
where
    Source: NodeSource + 'a,
    F: Fn(SourceBlock) -> Fut + Copy + 'a,
    Fut: Future<Output = Result<DerivedBlockArtifacts, IngestError>> + 'a,
{
    // Pipeline per-block fetches and derives with bounded concurrency.
    // The commit path stays strictly ordered because both `buffered`
    // slot pools yield completed futures in submission order.
    futures_util::stream::iter(BlockHeightRange::inclusive(
        config.from_height,
        config.to_height,
    ))
    .map(move |height| {
        let mut fetch_retry_state = IngestRetryState::default();
        async move {
            fetch_block_with_retry(
                config.request_timeout,
                source,
                height,
                &mut fetch_retry_state,
            )
            .await
        }
    })
    .buffered(config.fetch_concurrency)
    .map(move |fetch_result| {
        let derive_fn = derive_fn;
        async move {
            let source_block = fetch_result?;
            let derive_started_at = Instant::now();
            let derive_outcome = derive_fn(source_block).await;
            record_ingest_derive_outcome(derive_started_at, &derive_outcome);
            derive_outcome
        }
    })
    .buffered(config.derive_concurrency)
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
        + Copy,
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
    let derive_store = zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(&config.storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: &[],
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?;
    backfill_from_source_with_store_using_derive_fn(
        config,
        source,
        &store,
        &derive_store,
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
    derive_store: &zinder_derive::DeriveStore,
    derive_fn: F,
    backfill_start: BackfillStart,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    F: Fn(&zinder_source::SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError>
        + Copy,
{
    let request_timeout = config.node.request_timeout;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let fetch_concurrency = config.fetch_concurrency.get() as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let derive_concurrency = config.derive_concurrency.get() as usize;
    let derive_stream = build_derive_stream(
        source,
        BackfillDeriveStreamConfig {
            request_timeout,
            from_height: backfill_start.from_height,
            to_height: config.to_height,
            fetch_concurrency,
            derive_concurrency,
        },
        move |source_block| async move { derive_fn(&source_block).map_err(IngestError::from) },
    );

    let mut flush_state = BackfillFlushState::default();
    let run = BackfillRunContext::new(config, source, store, derive_store);
    run_backfill_commit_loop(
        &run,
        derive_stream,
        backfill_start,
        &mut flush_state,
        BackfillCompletionFlush::FlushPending,
    )
    .await
}

async fn run_backfill_commit_loop<Source>(
    run: &BackfillRunContext<'_, Source>,
    derive_stream: impl futures_util::Stream<Item = Result<DerivedBlockArtifacts, IngestError>> + Unpin,
    backfill_start: BackfillStart,
    flush_state: &mut BackfillFlushState,
    completion_flush: BackfillCompletionFlush,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
{
    let mut chain_epoch_id = next_chain_epoch_id(run.store)?;
    let mut batch = IngestBatch::default();
    let mut next_subtree_root_indexes =
        IngestSubtreeRootIndexes::from_tip_metadata(backfill_start.initial_tip_metadata);
    let mut last_commit_outcome = None;
    let mut retry_state = IngestRetryState::default();
    let mut running_tree_sizes =
        CommitmentTreeSizes::from_tip_metadata(backfill_start.initial_tip_metadata);
    let batch_budget = IngestBatchBudget::new(
        run.config.commit_batch_blocks,
        run.config.max_transparent_prevout_store_lookups_per_batch,
    );
    let mut derive_stream = derive_stream;

    loop {
        let await_derived_block_started_at = Instant::now();
        let Some(derive_result) = derive_stream.next().await else {
            break;
        };
        record_backfill_stage_duration(
            BACKFILL_STAGE_AWAIT_DERIVED_BLOCK,
            await_derived_block_started_at,
            derive_result.as_ref().err(),
        );
        let built_outcome = derive_result.and_then(|derived| {
            finalize_derived_block(derived, &mut running_tree_sizes).map_err(IngestError::from)
        });
        let built = built_outcome?;

        batch.absorb(built);
        let batch_cost = batch.work_cost();
        record_ingest_batch_work_cost(batch_cost);

        if let Some(commit_trigger) = batch_budget.commit_trigger(batch_cost) {
            record_backfill_batch_commit_trigger(run.config, batch_cost, commit_trigger);
            let updated_subtree_root_indexes = populate_backfill_subtree_roots(
                run,
                &mut batch,
                next_subtree_root_indexes,
                &mut retry_state,
            )
            .await?;
            let (commit_outcome, returned_batch) = commit_finalized_backfill_batch(
                run.store,
                run.derive_store,
                run.config.node.network,
                chain_epoch_id,
                batch,
            )
            .await?;
            batch = returned_batch;
            next_subtree_root_indexes = updated_subtree_root_indexes;
            chain_epoch_id = next_chain_epoch_id_after(chain_epoch_id)?;
            last_commit_outcome = Some(commit_outcome);
            flush_state.record_committed_epoch();
            flush_backfill_writes_if_due(run, flush_state).await?;
        }
    }

    if !batch.is_empty() {
        let _updated_subtree_root_indexes = populate_backfill_subtree_roots(
            run,
            &mut batch,
            next_subtree_root_indexes,
            &mut retry_state,
        )
        .await?;
        let (commit_outcome, _drained_batch) = commit_finalized_backfill_batch(
            run.store,
            run.derive_store,
            run.config.node.network,
            chain_epoch_id,
            batch,
        )
        .await?;
        last_commit_outcome = Some(commit_outcome);
        flush_state.record_committed_epoch();
    }

    if completion_flush.flushes_pending() && last_commit_outcome.is_some() {
        flush_pending_backfill_writes(run.store, flush_state).await?;
    }

    last_commit_outcome.ok_or(IngestError::BackfillProducedNoCommit)
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
    batch: &mut IngestBatch,
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
        BACKFILL_STAGE_POPULATE_SUBTREE_ROOTS,
        started_at,
        outcome.as_ref().err(),
    );
    outcome
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
        BACKFILL_STAGE_FLUSH_STORE,
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
    let status = if stage_error.is_some() { "error" } else { "ok" };
    metrics::histogram!(
        "zinder_ingest_backfill_stage_duration_seconds",
        "stage" => stage,
        "status" => status,
        "error_class" => ingest_error_class(stage_error)
    )
    .record(started_at.elapsed());
}

fn record_backfill_batch_commit_trigger(
    config: &BackfillConfig,
    batch_cost: IngestBatchWorkCost,
    commit_trigger: IngestBatchCommitTrigger,
) {
    record_ingest_batch_commit_trigger(commit_trigger);
    tracing::info!(
        target: "zinder::ingest",
        event = "bulk_catchup_batch_budget_reached",
        trigger = commit_trigger.metric_label(),
        block_count = batch_cost.block_count,
        transparent_prevout_store_lookup_count =
            batch_cost.transparent_prevout_store_lookup_count,
        max_blocks = config.commit_batch_blocks.get(),
        max_transparent_prevout_store_lookups = config
            .max_transparent_prevout_store_lookups_per_batch
            .get(),
        "bulk-catchup batch budget reached; committing accumulated artifacts"
    );
}

/// Commits a finalized backfill batch and returns the drained batch buffer.
async fn commit_finalized_backfill_batch(
    store: &PrimaryChainStore,
    derive_store: &zinder_derive::DeriveStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: IngestBatch,
) -> Result<(ChainEpochCommitOutcome, IngestBatch), IngestError> {
    let mut batch = batch;
    let outcome = commit_finalized_backfill_batch_inner(
        store,
        derive_store,
        network,
        chain_epoch_id,
        &mut batch,
    )
    .await?;
    Ok((outcome, batch))
}

async fn commit_finalized_backfill_batch_inner(
    store: &PrimaryChainStore,
    derive_store: &zinder_derive::DeriveStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: &mut IngestBatch,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let tip_block = batch
        .finalized_blocks
        .last()
        .ok_or(IngestError::EmptyIngestBatch)?;
    let tip_height = tip_block.height;
    let tip_hash = tip_block.block_hash;
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyIngestBatch)?;
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
    commit_ingest_batch(
        store,
        derive_store,
        chain_epoch,
        batch,
        ReorgWindowChange::FinalizeThrough { height: tip_height },
    )
    .await
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
        ChainEpochArtifacts::new(bootstrap_chain_epoch, Vec::new(), Vec::new())
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
        num::NonZeroU32,
        path::Path,
        sync::atomic::{AtomicU32, Ordering},
    };

    use tempfile::tempdir;
    use zinder_core::{
        BlockArtifact, BlockHash, BlockId, SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootHash,
        SubtreeRootIndex, TransactionId, TransparentOutPoint, TransparentUtxoSpendArtifact,
        UnixTimestampMillis, wire::encode_internal_block_hash,
    };
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
    use zinder_source::{
        NodeCapabilities, SourceBlock, SourceBlockHeader, SourceError, SourceSubtreeRoot,
        SourceSubtreeRoots, ZebraJsonRpcSource,
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
    async fn backfill_commits_when_prevout_store_lookup_budget_is_reached()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("prevout-budget-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let mut config = test_backfill_config(&storage_path, 1, 3, 10, true)?;
        config.max_transparent_prevout_store_lookups_per_batch =
            NonZeroU32::new(2).ok_or("invalid prevout budget")?;

        let commit_outcome = backfill_from_source_with_mock_derive(&config, &source, |sb| {
            let mut derived = test_derived_block(sb, 0, 0);
            let mut transaction_id_bytes = [0; 32];
            transaction_id_bytes[..4].copy_from_slice(&sb.height.value().to_be_bytes());
            derived
                .transparent_utxo_spends
                .push(TransparentUtxoSpendArtifact::new(
                    TransparentOutPoint::new(TransactionId::from_bytes(transaction_id_bytes), 0),
                    sb.height,
                    sb.hash,
                ));
            Ok(derived)
        })
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(3));
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
    // per-call test: per [ADR-0013](../../../docs/adrs/0013-source-failure-recovery-topology.md)
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
        // and 12 with commit_batch_blocks = 1).
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
        F: Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, ArtifactDeriveError> + Copy,
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
        let derive_store = zinder_derive::DeriveStore::open(
            zinder_derive::DeriveStore::path_for_canonical(&config.storage_path),
            zinder_derive::DeriveStoreOptions {
                sync_writes: false,
                consumer_column_families: &[],
                tuning: zinder_store::StorageTuning::for_local_tests(),
            },
        )?;
        backfill_from_source_with_store_using_derive_fn(
            config,
            source,
            &store,
            &derive_store,
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
        commit_batch_blocks: u32,
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
            from_height: BlockHeight::new(from_height),
            to_height: BlockHeight::new(to_height),
            commit_batch_blocks: NonZeroU32::new(commit_batch_blocks)
                .ok_or("invalid test batch size")?,
            max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
                .ok_or("invalid test prevout budget")?,
            fetch_concurrency: NonZeroU32::new(4).ok_or("invalid test fetch concurrency")?,
            derive_concurrency: NonZeroU32::new(4).ok_or("invalid test derive concurrency")?,
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
    impl NodeSource for FlakyNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            self.delegate.capabilities()
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            self.fetch_attempts.fetch_add(1, Ordering::SeqCst);
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
            let source_hash = block_hash(height.value());
            let parent_hash = block_hash(height.value().saturating_sub(1));
            let header = SourceBlockHeader {
                network: self.network,
                height,
                hash: source_hash,
                parent_hash,
                block_time_seconds: 1_774_668_400,
            };

            Ok(
                SourceBlock::new(header, format!("raw-block-{}", height.value()).into_bytes())
                    .with_tree_state_payload_bytes(
                        format!("tree-state-{}", height.value()).into_bytes(),
                    ),
            )
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
            block: BlockArtifact::new(
                source_block.height,
                source_block.hash,
                source_block.parent_hash,
                source_block.raw_block_bytes.clone(),
            ),
            parsed_block: None,
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
            observed_tree_sizes: None,
            tree_state: None,
            transactions: Vec::new(),
            transparent_address_utxos: Vec::new(),
            transparent_prevouts: Vec::new(),
            transparent_utxo_spends: Vec::new(),
            transparent_address_tx_index: Vec::new(),
            transparent_address_tx_index_spend_candidates: Vec::new(),
        }
    }
}
