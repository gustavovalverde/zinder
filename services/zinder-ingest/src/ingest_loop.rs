//! Unified ingest loop.
//!
//! Per [ADR-0015](../../../docs/adrs/0015-unified-phase-driven-ingest.md),
//! `zinder-ingest` is a single long-running command. On each iteration the
//! loop observes the upstream tip, classifies the gap into one of three
//! phases via [`crate::classify_phase`], and dispatches to the matching
//! handler:
//!
//! - [`IngestPhase::AwaitingUpstream`] parks until the upstream tip rises
//!   above genesis;
//! - [`IngestPhase::BulkCatchup`] runs one pipelined batch via
//!   `run_bulk_catchup_until_complete_with_flush_state` and re-classifies after
//!   each batch;
//! - [`IngestPhase::FollowingTip`] runs the serial
//!   [`tip_follow_with_primary_store`] loop until the classifier would
//!   bounce back to bulk catch-up.
//!
//! The mempool orchestrator, retention worker, and chain-tip notification
//! stream spawn once on the first entry into `FollowingTip` and stay
//! running across subsequent bounces. The caller passes the spawner as a
//! `FnOnce` closure; the unified loop calls it the first time the loop
//! enters `FollowingTip`.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, NetworkUpgradeActivations};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_source::{ChainTipNotificationSource, NodeSource, NodeTarget, SourceChainCheckpoint};
use zinder_store::PrimaryChainStore;

use crate::bulk_catchup::{
    BulkCatchupFlushState, BulkCatchupRunContext, flush_pending_bulk_catchup_writes,
    run_bulk_catchup_until_complete_with_flush_state,
};
use crate::memory_pressure::wait_for_bulk_catchup_memory_headroom;
use crate::{
    BulkCatchupRunConfig, CommitmentRootBackfillConfig, IngestError, MempoolReadyGate,
    NodeSourceKind, RawBlobPolicy, TipFollowConfig, classify_phase, current_chain_height,
    tip_follow_with_primary_store,
};

/// Backoff applied when the source's `tip_id()` call fails at the unified
/// classifier.
///
/// The inner phase handlers own their own retry-and-readiness dance; the
/// unified loop just needs to avoid a hot-loop on transient upstream
/// failures while it is between handlers.
const TIP_OBSERVATION_FAILURE_BACKOFF: Duration = Duration::from_secs(2);

/// Cadence at which the bounce-back watcher re-evaluates the classifier
/// while the `FollowingTip` handler runs.
///
/// Short enough to catch an upstream burst within a minute; long enough
/// to keep watcher load negligible against the upstream node. Operators
/// rarely need to tune this; the value is intentionally a private
/// constant rather than a configuration knob.
const PHASE_BOUNCE_BACK_WATCH_INTERVAL: Duration = Duration::from_mins(1);

/// Resolved configuration consumed by [`run_ingest_loop`].
///
/// All fields are post-default-application. The TOML schema and the
/// validation rules live in `services/zinder-ingest/src/config.rs`; this
/// type is the seam between configuration and the loop runtime.
#[derive(Clone, Debug)]
pub struct IngestLoopConfig {
    /// Resolved upstream node endpoint.
    pub node: NodeTarget,
    /// Upstream node source adapter selector.
    pub node_source: NodeSourceKind,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// Bounded `RocksDB` resource budget for the canonical store.
    pub canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// Bounded `RocksDB` resource budget for the derive store.
    pub derive_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// Optional raw-byte blob write policy for canonical ingest.
    pub raw_blob_policy: RawBlobPolicy,
    /// Reorg-window invariant. Bulk catch-up never finalizes blocks inside
    /// this window unless `modifiers.allow_near_tip_finalize` is true.
    pub reorg_window_blocks: u32,
    /// Phase-classifier knobs (`[ingest.phases]`).
    pub phases: PhasesConfig,
    /// Shared derive execution knobs (`[ingest.derive]`).
    pub derive: IngestDeriveConfig,
    /// Settled-history root enrichment (`[ingest.commitment_root_backfill]`).
    pub commitment_root_backfill: CommitmentRootBackfillConfig,
    /// Pipelined-fetch and commit knobs (`[ingest.bulk_catchup]`).
    pub bulk_catchup: BulkCatchupConfig,
    /// Serial-loop knobs (`[ingest.tip_follow]`).
    pub tip_follow: TipFollowPhaseConfig,
    /// One-shot or disposable-store modifiers (`[ingest.modifiers]`).
    pub modifiers: IngestModifiers,
}

/// Resolved `[ingest.phases]` configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhasesConfig {
    /// Boundary between `BulkCatchup` and `FollowingTip`. Defaults to
    /// `reorg_window_blocks` so bulk catch-up's `AdvanceSafeTipTo`
    /// horizon never crosses the reorg cliff.
    pub catchup_threshold_blocks: u32,
}

/// Resolved `[ingest.derive]` configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IngestDeriveConfig {
    /// Maximum block contexts hydrated and dispatched in one derive replay
    /// write.
    ///
    /// This bound is independent of `ingest.bulk_catchup.canonical_batch_max_blocks`
    /// because replay may resume from retained chain events after the writer
    /// has already committed them. Keeping replay memory bounded here prevents
    /// a large retained event from becoming one unbounded in-memory hydration.
    pub replay_batch_blocks: NonZeroU32,
    /// Replay pressure policy for the async derive tailer.
    ///
    /// `CanonicalFirst` keeps canonical ingest ahead of rebuildable derive
    /// projections under memory pressure. `Continuous` always lets the tailer
    /// replay retained chain events as soon as it observes them.
    pub replay_policy: DeriveReplayPolicy,
    /// Optional explicit memory budget for derive replay pressure decisions.
    ///
    /// When unset, the replay budget uses the runtime cgroup `memory.high` or
    /// `memory.max` value reported by the runtime memory-pressure sampler.
    pub memory_budget_bytes: Option<NonZeroU64>,
    /// Memory ratio at which derive replay starts shrinking the effective
    /// replay batch size.
    pub memory_degrade_ratio: f64,
    /// Memory ratio at which derive replay stops until pressure drops below
    /// [`Self::memory_resume_ratio`].
    pub memory_pause_ratio: f64,
    /// Memory ratio below which derive replay resumes normal batch sizing.
    pub memory_resume_ratio: f64,
    /// Smallest effective replay batch while memory pressure is degraded.
    pub min_replay_batch_blocks: NonZeroU32,
}

/// Runtime policy for replaying retained chain events into derive consumers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeriveReplayPolicy {
    /// Pause rebuildable derive replay when process or cgroup memory pressure
    /// would compete with canonical ingest.
    CanonicalFirst,
    /// Continuously replay retained chain events regardless of memory pressure.
    Continuous,
}

impl DeriveReplayPolicy {
    /// Default derive replay policy for performance-first indexing.
    pub const DEFAULT: Self = Self::CanonicalFirst;

    /// Stable TOML/env rendering.
    #[must_use]
    pub const fn as_kebab_case(self) -> &'static str {
        match self {
            Self::CanonicalFirst => "canonical-first",
            Self::Continuous => "continuous",
        }
    }
}

/// Resolved `[ingest.bulk_catchup]` configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkCatchupConfig {
    /// Maximum number of blocks committed in one bulk-catchup batch.
    pub canonical_batch_max_blocks: NonZeroU32,
    /// Maximum canonical artifact bytes accumulated before closing a batch.
    pub canonical_batch_max_artifact_bytes: NonZeroU64,
    /// Maximum estimated canonical write bytes accumulated before closing a batch.
    pub canonical_batch_max_estimated_write_bytes: NonZeroU64,
    /// Minimum batch size before estimated write bytes can close the batch.
    pub canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32,
    /// Maximum connected blocks requested from the source in one segment.
    pub source_segment_max_blocks: NonZeroU32,
    /// Target source response bytes for adaptive segment sizing.
    pub source_segment_target_response_bytes: NonZeroU64,
    /// Maximum concurrent source segment fetches.
    pub source_fetch_max_in_flight_requests: NonZeroU32,
    /// Maximum reserved response bytes across source fetches.
    pub source_fetch_max_in_flight_bytes: NonZeroU64,
    /// Parallel canonical block-prepare slots.
    pub block_prepare_concurrency: NonZeroU32,
    /// Maximum reserved derived artifact bytes across active and completed
    /// canonical block prepares.
    pub block_prepare_max_in_flight_artifact_bytes: NonZeroU64,
    /// Maximum safe-tip artifact bytes allowed to queue while the previous
    /// canonical batch is attaching metadata, committing, or flushing.
    pub commit_reassembly_max_queued_artifact_bytes: NonZeroU64,
    /// Force a `RocksDB` flush every N committed epochs.
    ///
    /// Caps the live WAL by writer cadence rather than `RocksDB`'s WAL-size
    /// safety trigger. With the default `canonical_batch_max_blocks = 1000` and
    /// `flush_interval_epochs = 5`, the writer truncates the WAL after
    /// every 5,000 committed blocks, bounding crash-recovery RAM
    /// proportionally. See
    /// [the OOM-recovery runbook](../../../docs/runbooks/bulk-catchup-oom-recovery.md).
    pub flush_interval_epochs: NonZeroU32,
}

/// Resolved `[ingest.tip_follow]` configuration.
#[derive(Clone, Copy, Debug)]
pub struct TipFollowPhaseConfig {
    /// Maximum delay between tip observations when no chain-tip
    /// notification stream is available.
    pub poll_interval: Duration,
    /// Threshold below which the loop reports `cause=ready` instead of
    /// `cause=syncing` while in [`IngestPhase::FollowingTip`].
    pub lag_threshold_blocks: u64,
}

/// Resolved `[ingest.modifiers]` configuration.
///
/// Optional knobs that scope or bound a single ingest run. Operators
/// rarely set these; they exist for forked-store recovery and the
/// historical wallet-serving bootstrap.
#[derive(Clone, Debug, Default)]
pub struct IngestModifiers {
    /// Stop committing after reaching this height. `None` means run
    /// indefinitely.
    pub target_height: Option<BlockHeight>,
    /// Seed an empty store from this height when set. The unified loop
    /// looks the corresponding checkpoint up against the upstream node
    /// before entering the first phase.
    pub checkpoint_height: Option<BlockHeight>,
    /// Allow bulk-catchup batches to finalize blocks inside the upstream
    /// node's reorg window. Disposable-store recovery only.
    pub allow_near_tip_finalize: bool,
    /// Optional starting checkpoint resolved against the upstream node
    /// before the loop starts. Populated by the binary entrypoint after
    /// the `checkpoint_height` lookup completes.
    pub checkpoint: Option<SourceChainCheckpoint>,
}

/// Subsystem handles surfaced by the spawn-once gate.
///
/// Returned by [`TipFollowSubsystemsLauncher`] the first time the
/// unified loop enters [`IngestPhase::FollowingTip`]. The launcher is
/// responsible for spawning the mempool orchestrator, retention workers,
/// and any chain-tip notification subscription; the returned handles +
/// channel ends are what `tip_follow_with_primary_store` consumes on
/// every subsequent entry into the phase.
pub struct TipFollowSubsystems {
    /// Ready gate the tip-follow loop checks before transitioning to
    /// `cause=ready`; `None` means the mempool orchestrator is not part
    /// of this deployment.
    pub mempool_ready_gate: Option<MempoolReadyGate>,
    /// Optional chain-tip notification source for sub-poll-interval
    /// wakeups. `None` means tip-follow falls back to its poll loop.
    pub chain_tip_source: Option<Arc<dyn ChainTipNotificationSource>>,
    /// Spawned task handles to keep alive for the life of the process.
    /// The unified loop holds these so the tasks are not detached.
    pub spawned_tasks: Vec<JoinHandle<()>>,
}

/// Boxed launcher closure the binary passes in to defer subsystem spawn
/// until the unified loop's first `FollowingTip` entry.
///
/// The closure runs at most once per call to [`run_ingest_loop`]; it
/// captures the dependencies (mempool source factories, retention
/// configs, etc.) that the unified loop should not know about. The
/// closure returns the [`TipFollowSubsystems`] handles + channel ends
/// the loop holds for the life of the process.
pub type TipFollowSubsystemsLauncher = Box<dyn FnOnce() -> TipFollowSubsystems + Send>;

/// Runs the unified ingest loop until `cancel` fires.
///
/// On every iteration the loop:
///
/// 1. Calls [`source.tip_id()`](NodeSource::tip_id) to observe the
///    upstream tip, with a short backoff on failure so the unified loop
///    does not spin under transient upstream outages;
/// 2. Calls [`classify_phase`] to pick the phase for the current gap;
/// 3. Stamps the phase on `readiness` via [`Readiness::set_phase`];
/// 4. Dispatches to the per-phase handler.
///
/// Phase handlers:
///
/// - [`IngestPhase::AwaitingUpstream`] sleeps for
///   [`TipFollowPhaseConfig::poll_interval`].
/// - [`IngestPhase::BulkCatchup`] computes the per-batch target
///   `min(upstream_tip - reorg_window_blocks,
///       store_tip + canonical_batch_max_blocks)`, calls
///   `run_bulk_catchup_until_complete_with_flush_state` for one batch, then
///   calls `wait_for_bulk_catchup_memory_headroom` so back-to-back batches
///   yield a flush-and-reclaim window once memory pressure reaches the
///   configured derive-replay degrade/pause ratios.
/// - [`IngestPhase::FollowingTip`] calls
///   [`tip_follow_with_primary_store`] with a child cancel token; a
///   parallel watcher task fires the child token if re-classification
///   would yield a different phase, so an upstream burst bounces the
///   loop back into bulk catch-up without a manual restart.
///
/// On first entry to `FollowingTip` the loop invokes
/// `tip_follow_subsystems` to spawn the mempool orchestrator, retention
/// workers, and any chain-tip notification subscription. The launcher
/// runs at most once; subsequent re-entries reuse the handles it
/// returned.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the unified ingest loop deliberately exposes its caller-owned dependencies positionally and its iteration composes three phase handlers, the spawn-once gate, and the failure recovery hop in one auditable sequence."
)]
pub async fn run_ingest_loop<Source>(
    config: &IngestLoopConfig,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    source: Arc<Source>,
    store: PrimaryChainStore,
    readiness: &Readiness,
    cancel: CancellationToken,
    mut tip_follow_subsystems: Option<TipFollowSubsystemsLauncher>,
) -> Result<(), IngestError>
where
    Source: NodeSource + Clone,
{
    let mut tip_subsystems: Option<TipFollowSubsystems> = None;
    let mut bulk_flush_state = BulkCatchupFlushState::default();

    loop {
        if cancel.is_cancelled() {
            flush_pending_bulk_catchup_writes(&store, &mut bulk_flush_state).await?;
            return Ok(());
        }
        if reached_target_height(config.modifiers.target_height, &store) {
            tracing::info!(
                target: "zinder::ingest",
                event = "ingest_loop_target_reached",
                target_height = config
                    .modifiers
                    .target_height
                    .as_ref()
                    .map(|height| height.value()),
                "unified loop reached configured target_height; exiting"
            );
            flush_pending_bulk_catchup_writes(&store, &mut bulk_flush_state).await?;
            return Ok(());
        }

        let upstream_tip = match source.as_ref().tip_id().await {
            Ok(tip) => tip.height.value(),
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "ingest_loop_tip_observation_failed",
                    error = %error,
                    "upstream tip observation failed; retrying after backoff"
                );
                flush_pending_bulk_catchup_writes(&store, &mut bulk_flush_state).await?;
                if sleep_or_cancel(TIP_OBSERVATION_FAILURE_BACKOFF, &cancel)
                    .await
                    .is_break()
                {
                    return Ok(());
                }
                continue;
            }
        };

        let store_tip = current_chain_height(&store);
        record_canonical_lag_blocks(upstream_tip, store_tip);
        let phase = classify_phase(
            store_tip,
            upstream_tip,
            config.phases.catchup_threshold_blocks,
        );
        readiness.set_phase(phase);
        if !matches!(phase, IngestPhase::BulkCatchup) {
            flush_pending_bulk_catchup_writes(&store, &mut bulk_flush_state).await?;
        }

        #[allow(
            clippy::wildcard_enum_match_arm,
            clippy::match_same_arms,
            reason = "IngestPhase is #[non_exhaustive]; future variants default to the AwaitingUpstream park so the loop never races a partially-recognized classifier."
        )]
        match phase {
            IngestPhase::AwaitingUpstream => {
                if sleep_or_cancel(config.tip_follow.poll_interval, &cancel)
                    .await
                    .is_break()
                {
                    return Ok(());
                }
            }
            IngestPhase::BulkCatchup => {
                let store_tip = bulk_catchup_progress_tip(store_tip, config.modifiers.checkpoint);
                let Some(batch_target) = compute_bulk_catchup_target(BulkCatchupTargetInput {
                    upstream_tip,
                    progress_tip: store_tip,
                    reorg_window_blocks: config.reorg_window_blocks,
                    canonical_batch_max_blocks: config
                        .bulk_catchup
                        .canonical_batch_max_blocks
                        .get(),
                    allow_near_tip_finalize: config.modifiers.allow_near_tip_finalize,
                    target_height: config.modifiers.target_height.map(BlockHeight::value),
                }) else {
                    // No height within the reorg window remains to be
                    // committed in bulk; let the next iteration re-
                    // classify (it will fall into `FollowingTip`).
                    flush_pending_bulk_catchup_writes(&store, &mut bulk_flush_state).await?;
                    if sleep_or_cancel(config.tip_follow.poll_interval, &cancel)
                        .await
                        .is_break()
                    {
                        return Ok(());
                    }
                    continue;
                };

                let batch_config = build_bulk_catchup_batch_config(
                    config,
                    Arc::clone(&network_upgrade_activations),
                    store_tip,
                    batch_target,
                    BlockHeight::new(upstream_tip),
                );
                let bulk_catchup_run =
                    BulkCatchupRunContext::new(&batch_config, source.as_ref(), &store);
                run_bulk_catchup_until_complete_with_flush_state(
                    bulk_catchup_run,
                    readiness,
                    &mut bulk_flush_state,
                )
                .await?;
                wait_for_bulk_catchup_memory_headroom(
                    config.derive.memory_degrade_ratio,
                    config.derive.memory_pause_ratio,
                    config.derive.memory_resume_ratio,
                    &cancel,
                )
                .await;
            }
            IngestPhase::FollowingTip => {
                if tip_subsystems.is_none()
                    && let Some(launcher) = tip_follow_subsystems.take()
                {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "ingest_loop_following_tip_subsystems_launched",
                        "spawning tip-follow subsystems on first FollowingTip entry"
                    );
                    tip_subsystems = Some(launcher());
                }

                let tip_follow_config =
                    build_tip_follow_config(config, Arc::clone(&network_upgrade_activations));
                let phase_cancel = cancel.child_token();
                let watcher_handle = spawn_following_tip_exit_watcher(
                    Arc::clone(&source),
                    store.clone(),
                    config.phases.catchup_threshold_blocks,
                    config.modifiers.target_height,
                    phase_cancel.clone(),
                );
                let tip_follow_outcome = tip_follow_with_primary_store(
                    &tip_follow_config,
                    source.as_ref(),
                    store.clone(),
                    readiness,
                    tip_subsystems
                        .as_ref()
                        .and_then(|subsystems| subsystems.mempool_ready_gate.as_ref()),
                    tip_subsystems
                        .as_ref()
                        .and_then(|subsystems| subsystems.chain_tip_source.clone()),
                    phase_cancel,
                )
                .await;
                watcher_handle.abort();
                tip_follow_outcome?;
                // tip_follow_with_primary_store returns either when the
                // top-level cancel fired (operator shutdown) or when the
                // bounce-back watcher cancelled the child token (upstream
                // burst pushed the classifier out of FollowingTip). The
                // top-of-loop `cancel.is_cancelled()` guard distinguishes
                // the two and re-dispatches in the burst case.
            }
            _ => {
                if sleep_or_cancel(config.tip_follow.poll_interval, &cancel)
                    .await
                    .is_break()
                {
                    return Ok(());
                }
            }
        }
    }
}

/// Spawns the watcher that fires `cancel` when `FollowingTip` should return
/// control to the unified loop.
///
/// Runs alongside `tip_follow_with_primary_store`; the parent loop
/// aborts the watcher's join handle as soon as the tip-follow handler returns.
/// The watcher does not own readiness updates: it only signals the
/// cancellation that lets the unified loop re-classify or honor
/// `--target-height`.
fn spawn_following_tip_exit_watcher<Source>(
    source: Arc<Source>,
    store: PrimaryChainStore,
    catchup_threshold_blocks: u32,
    target_height: Option<BlockHeight>,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    Source: NodeSource + Clone,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(PHASE_BOUNCE_BACK_WATCH_INTERVAL) => {}
            }
            if reached_target_height(target_height, &store) {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "ingest_loop_following_tip_target_reached",
                    target_height = target_height.as_ref().map(|height| height.value()),
                    "target height reached inside FollowingTip; cancelling inner loop"
                );
                cancel.cancel();
                return;
            }
            let Ok(tip_id) = source.tip_id().await else {
                continue;
            };
            let store_tip = current_chain_height(&store);
            record_canonical_lag_blocks(tip_id.height.value(), store_tip);
            let phase = classify_phase(store_tip, tip_id.height.value(), catchup_threshold_blocks);
            if !matches!(phase, IngestPhase::FollowingTip) {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "ingest_loop_phase_bounce_back",
                    new_phase = phase.wire_label(),
                    store_tip = store_tip,
                    upstream_tip = tip_id.height.value(),
                    "phase classifier requests bounce-back from FollowingTip; cancelling inner loop"
                );
                cancel.cancel();
                return;
            }
        }
    })
}

/// Computes the per-batch finalization target for the `BulkCatchup` phase.
///
/// Returns `None` when no committable height remains inside the bulk
/// window: either the upstream tip itself sits inside the reorg window
/// or the operator-set `target_height` modifier is already covered.
fn compute_bulk_catchup_target(input: BulkCatchupTargetInput) -> Option<u32> {
    let outside_reorg_window = if input.allow_near_tip_finalize {
        input.upstream_tip
    } else {
        input.upstream_tip.checked_sub(input.reorg_window_blocks)?
    };
    let batch_ceiling = input
        .progress_tip
        .checked_add(input.canonical_batch_max_blocks)?;
    let mut target = outside_reorg_window.min(batch_ceiling);
    if let Some(modifier_ceiling) = input.target_height {
        target = target.min(modifier_ceiling);
    }
    if target <= input.progress_tip {
        None
    } else {
        Some(target)
    }
}

#[derive(Clone, Copy, Debug)]
struct BulkCatchupTargetInput {
    upstream_tip: u32,
    progress_tip: u32,
    reorg_window_blocks: u32,
    canonical_batch_max_blocks: u32,
    allow_near_tip_finalize: bool,
    target_height: Option<u32>,
}

fn bulk_catchup_progress_tip(
    store_tip: Option<u32>,
    checkpoint: Option<zinder_source::SourceChainCheckpoint>,
) -> u32 {
    store_tip
        .or_else(|| checkpoint.map(|checkpoint| checkpoint.height.value()))
        .unwrap_or(0)
}

fn build_bulk_catchup_batch_config(
    config: &IngestLoopConfig,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    store_tip: u32,
    batch_target: u32,
    upstream_tip: BlockHeight,
) -> BulkCatchupRunConfig {
    let from_height = match config.modifiers.checkpoint {
        Some(checkpoint) if store_tip == 0 => checkpoint.height.next().unwrap_or(checkpoint.height),
        _ => BlockHeight::new(store_tip.saturating_add(1)),
    };
    BulkCatchupRunConfig {
        node: config.node.clone(),
        node_source: config.node_source,
        storage_path: config.storage_path.clone(),
        canonical_rocksdb_budget: config.canonical_rocksdb_budget,
        raw_blob_policy: config.raw_blob_policy,
        network_upgrade_activations,
        from_height,
        to_height: BlockHeight::new(batch_target),
        canonical_batch_max_blocks: config.bulk_catchup.canonical_batch_max_blocks,
        canonical_batch_max_artifact_bytes: config.bulk_catchup.canonical_batch_max_artifact_bytes,
        canonical_batch_max_estimated_write_bytes: config
            .bulk_catchup
            .canonical_batch_max_estimated_write_bytes,
        canonical_batch_min_blocks_before_estimated_write_close: config
            .bulk_catchup
            .canonical_batch_min_blocks_before_estimated_write_close,
        source_segment_max_blocks: config.bulk_catchup.source_segment_max_blocks,
        source_segment_target_response_bytes: config
            .bulk_catchup
            .source_segment_target_response_bytes,
        source_fetch_max_in_flight_requests: config
            .bulk_catchup
            .source_fetch_max_in_flight_requests,
        source_fetch_max_in_flight_bytes: config.bulk_catchup.source_fetch_max_in_flight_bytes,
        block_prepare_concurrency: config.bulk_catchup.block_prepare_concurrency,
        block_prepare_max_in_flight_artifact_bytes: config
            .bulk_catchup
            .block_prepare_max_in_flight_artifact_bytes,
        commit_reassembly_max_queued_artifact_bytes: config
            .bulk_catchup
            .commit_reassembly_max_queued_artifact_bytes,
        flush_interval_epochs: config.bulk_catchup.flush_interval_epochs,
        upstream_tip_hint: Some(upstream_tip),
        allow_near_tip_finalize: config.modifiers.allow_near_tip_finalize,
        checkpoint: config.modifiers.checkpoint,
    }
}

fn build_tip_follow_config(
    config: &IngestLoopConfig,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
) -> TipFollowConfig {
    TipFollowConfig {
        node: config.node.clone(),
        storage_path: config.storage_path.clone(),
        canonical_rocksdb_budget: config.canonical_rocksdb_budget,
        raw_blob_policy: config.raw_blob_policy,
        network_upgrade_activations,
        reorg_window_blocks: config.reorg_window_blocks,
        poll_interval: config.tip_follow.poll_interval,
        lag_threshold_blocks: config.tip_follow.lag_threshold_blocks,
    }
}

fn reached_target_height(target: Option<BlockHeight>, store: &PrimaryChainStore) -> bool {
    let Some(target) = target else {
        return false;
    };
    let Some(current) = current_chain_height(store) else {
        return false;
    };
    current >= target.value()
}

/// Records how many blocks the canonical writer trails the upstream tip by,
/// saturating at zero.
///
/// A missing `store_tip` (no committed chain yet) counts as height zero, so the
/// gap is the full upstream tip.
fn record_canonical_lag_blocks(upstream_tip: u32, store_tip: Option<u32>) {
    metrics::gauge!("zinder_ingest_canonical_lag_blocks").set(f64::from(
        upstream_tip.saturating_sub(store_tip.unwrap_or(0)),
    ));
}

enum CancelOutcome {
    Slept,
    Cancelled,
}

impl CancelOutcome {
    const fn is_break(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> CancelOutcome {
    tokio::select! {
        () = cancel.cancelled() => CancelOutcome::Cancelled,
        () = tokio::time::sleep(duration) => CancelOutcome::Slept,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zinder_core::{BlockHash, ChainTipMetadata, Network};
    use zinder_source::{
        DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeTarget, SourceChainCheckpoint,
    };

    use super::*;

    fn sample_loop_config(modifiers: IngestModifiers) -> Result<IngestLoopConfig, &'static str> {
        Ok(IngestLoopConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:0".to_owned(),
                NodeAuth::None,
                Duration::from_secs(5),
                DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            node_source: NodeSourceKind::ZebraJsonRpc,
            storage_path: PathBuf::from("/tmp/unit-test"),
            canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            derive_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            raw_blob_policy: RawBlobPolicy::None,
            reorg_window_blocks: 100,
            phases: PhasesConfig {
                catchup_threshold_blocks: 100,
            },
            derive: IngestDeriveConfig {
                replay_batch_blocks: NonZeroU32::new(100).ok_or("invalid replay batch blocks")?,
                replay_policy: DeriveReplayPolicy::DEFAULT,
                memory_budget_bytes: None,
                memory_degrade_ratio: 0.85,
                memory_pause_ratio: 0.95,
                memory_resume_ratio: 0.75,
                min_replay_batch_blocks: NonZeroU32::new(10)
                    .ok_or("invalid minimum replay batch blocks")?,
            },
            commitment_root_backfill: CommitmentRootBackfillConfig {
                enabled: true,
                batch_blocks: NonZeroU32::new(256).ok_or("invalid root batch blocks")?,
                fetch_concurrency: NonZeroU32::new(8).ok_or("invalid root fetch concurrency")?,
            },
            bulk_catchup: BulkCatchupConfig {
                canonical_batch_max_blocks: NonZeroU32::new(1_000).ok_or("invalid batch size")?,
                canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
                    .ok_or("invalid batch artifact bytes")?,
                canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
                    crate::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
                )
                .ok_or("invalid estimated write byte budget")?,
                canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
                    crate::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
                )
                .ok_or("invalid estimated write close floor")?,
                source_segment_max_blocks: NonZeroU32::new(128)
                    .ok_or("invalid source segment blocks")?,
                source_segment_target_response_bytes: NonZeroU64::new(48 * 1024 * 1024)
                    .ok_or("invalid source target bytes")?,
                source_fetch_max_in_flight_requests: NonZeroU32::new(8)
                    .ok_or("invalid source fetch requests")?,
                source_fetch_max_in_flight_bytes: NonZeroU64::new(256 * 1024 * 1024)
                    .ok_or("invalid source fetch bytes")?,
                block_prepare_concurrency: NonZeroU32::new(4)
                    .ok_or("invalid block prepare slots")?,
                block_prepare_max_in_flight_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                    .ok_or("invalid block prepare artifact bytes")?,
                commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
                    .ok_or("invalid commit reassembly bytes")?,
                flush_interval_epochs: NonZeroU32::new(5).ok_or("invalid flush cadence")?,
            },
            tip_follow: TipFollowPhaseConfig {
                poll_interval: Duration::from_millis(10),
                lag_threshold_blocks: 1,
            },
            modifiers,
        })
    }

    #[test]
    fn empty_store_with_checkpoint_starts_at_checkpoint_plus_one() -> Result<(), &'static str> {
        // Regression guard: under the unified loop, a wallet-serving (or
        // operator-supplied) checkpoint must seed `from_height` even when
        // the store is empty. Without this, `bulk catchup` rejects the batch
        // with `BulkCatchupCheckpointMisaligned`.
        let checkpoint = SourceChainCheckpoint::new(
            BlockHeight::new(279_999),
            BlockHash::from_bytes([0xAB; 32]),
            ChainTipMetadata::empty(),
        );
        let config = sample_loop_config(IngestModifiers {
            checkpoint: Some(checkpoint),
            ..IngestModifiers::default()
        })?;
        let batch = build_bulk_catchup_batch_config(
            &config,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            0,
            280_050,
            BlockHeight::new(280_200),
        );
        assert_eq!(batch.from_height, BlockHeight::new(280_000));
        assert_eq!(batch.to_height, BlockHeight::new(280_050));
        assert_eq!(batch.upstream_tip_hint, Some(BlockHeight::new(280_200)));
        Ok(())
    }

    #[test]
    fn non_empty_store_starts_at_store_tip_plus_one_even_with_checkpoint()
    -> Result<(), &'static str> {
        let checkpoint = SourceChainCheckpoint::new(
            BlockHeight::new(279_999),
            BlockHash::from_bytes([0xAB; 32]),
            ChainTipMetadata::empty(),
        );
        let config = sample_loop_config(IngestModifiers {
            checkpoint: Some(checkpoint),
            ..IngestModifiers::default()
        })?;
        let batch = build_bulk_catchup_batch_config(
            &config,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            280_500,
            281_500,
            BlockHeight::new(281_700),
        );
        assert_eq!(batch.from_height, BlockHeight::new(280_501));
        assert_eq!(batch.to_height, BlockHeight::new(281_500));
        Ok(())
    }

    #[test]
    fn empty_store_without_checkpoint_starts_at_genesis() -> Result<(), &'static str> {
        let config = sample_loop_config(IngestModifiers::default())?;
        let batch = build_bulk_catchup_batch_config(
            &config,
            Arc::new(zinder_testkit::sample_regtest_upgrade_activations()),
            0,
            1_000,
            BlockHeight::new(1_200),
        );
        assert_eq!(batch.from_height, BlockHeight::new(1));
        assert_eq!(batch.to_height, BlockHeight::new(1_000));
        Ok(())
    }

    #[test]
    fn bulk_catchup_target_picks_batch_ceiling_when_below_reorg_edge() {
        // store=0, upstream=10000, reorg=100, batch=1000:
        // outside_reorg = 9900, batch_ceiling = 1000 → target = 1000
        assert_eq!(
            compute_bulk_catchup_target(BulkCatchupTargetInput {
                upstream_tip: 10_000,
                progress_tip: 0,
                reorg_window_blocks: 100,
                canonical_batch_max_blocks: 1_000,
                allow_near_tip_finalize: false,
                target_height: None,
            }),
            Some(1_000)
        );
    }

    #[test]
    fn bulk_catchup_target_picks_reorg_edge_when_below_batch_ceiling() {
        // store=900, upstream=1000, reorg=100, batch=1000:
        // outside_reorg = 900, batch_ceiling = 1900 → target = 900
        // But 900 <= store_tip (900), so None.
        assert_eq!(
            compute_bulk_catchup_target(BulkCatchupTargetInput {
                upstream_tip: 1_000,
                progress_tip: 900,
                reorg_window_blocks: 100,
                canonical_batch_max_blocks: 1_000,
                allow_near_tip_finalize: false,
                target_height: None,
            }),
            None
        );
    }

    #[test]
    fn bulk_catchup_target_honours_modifier_ceiling() {
        // store=0, upstream=10000, reorg=100, batch=5000, modifier=2000:
        // outside_reorg = 9900, batch = 5000, modifier = 2000 → target = 2000
        assert_eq!(
            compute_bulk_catchup_target(BulkCatchupTargetInput {
                upstream_tip: 10_000,
                progress_tip: 0,
                reorg_window_blocks: 100,
                canonical_batch_max_blocks: 5_000,
                allow_near_tip_finalize: false,
                target_height: Some(2_000),
            }),
            Some(2_000)
        );
    }

    #[test]
    fn bulk_catchup_target_returns_none_when_upstream_inside_reorg_window() {
        // upstream=50, reorg=100: subtraction underflows → None
        assert_eq!(
            compute_bulk_catchup_target(BulkCatchupTargetInput {
                upstream_tip: 50,
                progress_tip: 0,
                reorg_window_blocks: 100,
                canonical_batch_max_blocks: 1_000,
                allow_near_tip_finalize: false,
                target_height: None,
            }),
            None
        );
    }

    #[test]
    fn bulk_catchup_target_honours_near_tip_finalize_override() {
        assert_eq!(
            compute_bulk_catchup_target(BulkCatchupTargetInput {
                upstream_tip: 1_642,
                progress_tip: 1_592,
                reorg_window_blocks: 100,
                canonical_batch_max_blocks: 25,
                allow_near_tip_finalize: true,
                target_height: Some(1_642),
            }),
            Some(1_617)
        );
    }

    #[test]
    fn bulk_catchup_progress_tip_uses_checkpoint_for_empty_store() {
        let checkpoint = SourceChainCheckpoint::new(
            BlockHeight::new(1_592),
            BlockHash::from_bytes([0xAB; 32]),
            ChainTipMetadata::empty(),
        );

        assert_eq!(bulk_catchup_progress_tip(None, Some(checkpoint)), 1_592);
        assert_eq!(
            bulk_catchup_progress_tip(Some(1_617), Some(checkpoint)),
            1_617
        );
        assert_eq!(bulk_catchup_progress_tip(None, None), 0);
    }
}
