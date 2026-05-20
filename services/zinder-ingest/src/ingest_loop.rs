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
//!   [`backfill_until_complete`] and re-classifies after each batch;
//! - [`IngestPhase::FollowingTip`] runs the serial
//!   [`tip_follow_with_primary_store`] loop until the classifier would
//!   bounce back to bulk catch-up.
//!
//! The mempool orchestrator, retention worker, and chain-tip notification
//! stream spawn once on the first entry into `FollowingTip` and stay
//! running across subsequent bounces. The caller passes the spawner as a
//! `FnOnce` closure; the unified loop calls it the first time the loop
//! enters `FollowingTip`.

use std::{num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_runtime::{IngestPhase, Readiness};
use zinder_source::{ChainTipNotificationSource, NodeSource, NodeTarget, SourceChainCheckpoint};
use zinder_store::PrimaryChainStore;

use crate::{
    BackfillConfig, IngestError, MempoolReadyGate, NodeSourceKind, TipFollowConfig,
    backfill_until_complete, classify_phase, current_chain_height, tip_follow_with_primary_store,
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
    /// Bounded `RocksDB` resource budget applied when opening the store.
    pub storage_tuning: zinder_store::StorageTuning,
    /// Reorg-window invariant. Bulk catch-up never finalizes blocks inside
    /// this window unless `modifiers.allow_near_tip_finalize` is true.
    pub reorg_window_blocks: u32,
    /// Phase-classifier knobs (`[ingest.phases]`).
    pub phases: PhasesConfig,
    /// Pipelined-fetch knobs (`[ingest.bulk_catchup]`).
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
    /// `reorg_window_blocks` so bulk catch-up's `FinalizeThrough`
    /// horizon never crosses the reorg cliff.
    pub catchup_threshold_blocks: u32,
}

/// Resolved `[ingest.bulk_catchup]` configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkCatchupConfig {
    /// Maximum number of blocks committed in one bulk-catchup batch.
    pub commit_batch_blocks: NonZeroU32,
    /// Number of concurrent block fetches in the pipelined fetcher.
    pub fetch_concurrency: NonZeroU32,
    /// Force a `RocksDB` flush every N committed epochs.
    ///
    /// Caps the live WAL by writer cadence rather than `RocksDB`'s WAL-size
    /// safety trigger. With the default `commit_batch_blocks = 1000` and
    /// `flush_every_n_epochs = 5`, the writer truncates the WAL after
    /// every 5,000 committed blocks, bounding crash-recovery RAM
    /// proportionally. See
    /// [the OOM-recovery runbook](../../../docs/runbooks/bulk-catchup-oom-recovery.md).
    pub flush_every_n_epochs: NonZeroU32,
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
///       store_tip + commit_batch_blocks)` and calls
///   [`backfill_until_complete`] for one batch.
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
    source: Arc<Source>,
    store: PrimaryChainStore,
    readiness: &Readiness,
    cancel: CancellationToken,
    mut tip_follow_subsystems: Option<TipFollowSubsystemsLauncher>,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    let mut tip_subsystems: Option<TipFollowSubsystems> = None;

    loop {
        if cancel.is_cancelled() {
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
        let phase = classify_phase(
            store_tip,
            upstream_tip,
            config.phases.catchup_threshold_blocks,
        );
        readiness.set_phase(phase);

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
                let store_tip = store_tip.unwrap_or(0);
                let Some(batch_target) = compute_bulk_catchup_target(
                    upstream_tip,
                    store_tip,
                    config.reorg_window_blocks,
                    config.bulk_catchup.commit_batch_blocks.get(),
                    config.modifiers.target_height.map(BlockHeight::value),
                ) else {
                    // No height within the reorg window remains to be
                    // committed in bulk; let the next iteration re-
                    // classify (it will fall into `FollowingTip`).
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
                    store_tip,
                    batch_target,
                    BlockHeight::new(upstream_tip),
                );
                backfill_until_complete(&batch_config, source.as_ref(), &store, readiness).await?;
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

                let tip_follow_config = build_tip_follow_config(config);
                let phase_cancel = cancel.child_token();
                let watcher_handle = spawn_phase_change_watcher(
                    Arc::clone(&source),
                    store.clone(),
                    config.phases.catchup_threshold_blocks,
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

/// Spawns the phase-change watcher that fires `cancel` when the
/// classifier would bounce out of `FollowingTip`.
///
/// Runs alongside `tip_follow_with_primary_store`; the parent loop
/// aborts the watcher's join handle as soon as the tip-follow handler
/// returns. The watcher does not own readiness updates: it only
/// signals the cancellation that lets the unified loop re-classify.
fn spawn_phase_change_watcher<Source>(
    source: Arc<Source>,
    store: PrimaryChainStore,
    catchup_threshold_blocks: u32,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    Source: NodeSource,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(PHASE_BOUNCE_BACK_WATCH_INTERVAL) => {}
            }
            let Ok(tip_id) = source.tip_id().await else {
                continue;
            };
            let store_tip = current_chain_height(&store);
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
fn compute_bulk_catchup_target(
    upstream_tip: u32,
    store_tip: u32,
    reorg_window_blocks: u32,
    commit_batch_blocks: u32,
    target_height_modifier: Option<u32>,
) -> Option<u32> {
    let outside_reorg_window = upstream_tip.checked_sub(reorg_window_blocks)?;
    let batch_ceiling = store_tip.checked_add(commit_batch_blocks)?;
    let mut target = outside_reorg_window.min(batch_ceiling);
    if let Some(modifier_ceiling) = target_height_modifier {
        target = target.min(modifier_ceiling);
    }
    if target <= store_tip {
        None
    } else {
        Some(target)
    }
}

fn build_bulk_catchup_batch_config(
    config: &IngestLoopConfig,
    store_tip: u32,
    batch_target: u32,
    upstream_tip: BlockHeight,
) -> BackfillConfig {
    let from_height = match config.modifiers.checkpoint {
        Some(checkpoint) if store_tip == 0 => checkpoint.height.next().unwrap_or(checkpoint.height),
        _ => BlockHeight::new(store_tip.saturating_add(1)),
    };
    BackfillConfig {
        node: config.node.clone(),
        node_source: config.node_source,
        storage_path: config.storage_path.clone(),
        storage_tuning: config.storage_tuning,
        from_height,
        to_height: BlockHeight::new(batch_target),
        commit_batch_blocks: config.bulk_catchup.commit_batch_blocks,
        fetch_concurrency: config.bulk_catchup.fetch_concurrency,
        flush_every_n_epochs: config.bulk_catchup.flush_every_n_epochs,
        upstream_tip_hint: Some(upstream_tip),
        allow_near_tip_finalize: config.modifiers.allow_near_tip_finalize,
        checkpoint: config.modifiers.checkpoint,
    }
}

fn build_tip_follow_config(config: &IngestLoopConfig) -> TipFollowConfig {
    TipFollowConfig {
        node: config.node.clone(),
        storage_path: config.storage_path.clone(),
        storage_tuning: config.storage_tuning,
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
            storage_tuning: zinder_store::StorageTuning::for_local_tests(),
            reorg_window_blocks: 100,
            phases: PhasesConfig {
                catchup_threshold_blocks: 100,
            },
            bulk_catchup: BulkCatchupConfig {
                commit_batch_blocks: NonZeroU32::new(1_000).ok_or("invalid batch size")?,
                fetch_concurrency: NonZeroU32::new(32).ok_or("invalid fetch concurrency")?,
                flush_every_n_epochs: NonZeroU32::new(5).ok_or("invalid flush cadence")?,
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
        // the store is empty. Without this, `backfill` rejects the batch
        // with `BackfillCheckpointMisaligned`.
        let checkpoint = SourceChainCheckpoint::new(
            BlockHeight::new(279_999),
            BlockHash::from_bytes([0xAB; 32]),
            ChainTipMetadata::empty(),
        );
        let config = sample_loop_config(IngestModifiers {
            checkpoint: Some(checkpoint),
            ..IngestModifiers::default()
        })?;
        let batch = build_bulk_catchup_batch_config(&config, 0, 280_050, BlockHeight::new(280_200));
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
        let batch =
            build_bulk_catchup_batch_config(&config, 280_500, 281_500, BlockHeight::new(281_700));
        assert_eq!(batch.from_height, BlockHeight::new(280_501));
        assert_eq!(batch.to_height, BlockHeight::new(281_500));
        Ok(())
    }

    #[test]
    fn empty_store_without_checkpoint_starts_at_genesis() -> Result<(), &'static str> {
        let config = sample_loop_config(IngestModifiers::default())?;
        let batch = build_bulk_catchup_batch_config(&config, 0, 1_000, BlockHeight::new(1_200));
        assert_eq!(batch.from_height, BlockHeight::new(1));
        assert_eq!(batch.to_height, BlockHeight::new(1_000));
        Ok(())
    }

    #[test]
    fn bulk_catchup_target_picks_batch_ceiling_when_below_reorg_edge() {
        // store=0, upstream=10000, reorg=100, batch=1000:
        // outside_reorg = 9900, batch_ceiling = 1000 → target = 1000
        assert_eq!(
            compute_bulk_catchup_target(10_000, 0, 100, 1_000, None),
            Some(1_000)
        );
    }

    #[test]
    fn bulk_catchup_target_picks_reorg_edge_when_below_batch_ceiling() {
        // store=900, upstream=1000, reorg=100, batch=1000:
        // outside_reorg = 900, batch_ceiling = 1900 → target = 900
        // But 900 <= store_tip (900), so None.
        assert_eq!(
            compute_bulk_catchup_target(1_000, 900, 100, 1_000, None),
            None
        );
    }

    #[test]
    fn bulk_catchup_target_honours_modifier_ceiling() {
        // store=0, upstream=10000, reorg=100, batch=5000, modifier=2000:
        // outside_reorg = 9900, batch = 5000, modifier = 2000 → target = 2000
        assert_eq!(
            compute_bulk_catchup_target(10_000, 0, 100, 5_000, Some(2_000)),
            Some(2_000)
        );
    }

    #[test]
    fn bulk_catchup_target_returns_none_when_upstream_inside_reorg_window() {
        // upstream=50, reorg=100: subtraction underflows → None
        assert_eq!(compute_bulk_catchup_target(50, 0, 100, 1_000, None), None);
    }
}
