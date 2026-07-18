//! Resolved ingest configuration types shared by the canonical writer and
//! its historical-work gate.
//!
//! [`IngestLoopConfig`] is the seam between `services/zinder-ingest/src/config.rs`'s
//! TOML schema and the runtime; [`HistoricalWorkGate`] is the process-local
//! admission gate that decides when rebuildable historical work may run.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, CommitmentTreeCheckpoint};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_source::NodeTarget;

use crate::{CanonicalPipelineLimits, NodeSourceKind, RawBlobPolicy};

const BACKGROUND_WORK_PHASE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Process-local admission gate for rebuildable historical work.
///
/// Canonical bulk catch-up owns the storage budget first. Once canonical is
/// following tip, derive replay owns it until every canonical block is
/// materialized. Historical backfills and verifiers may run only after both
/// conditions hold. Keeping this state out of [`Readiness`] avoids exposing an
/// internal scheduler concern as part of the service's public health contract.
#[derive(Clone, Debug)]
pub struct HistoricalWorkGate {
    readiness: Readiness,
    derive_caught_up: Arc<AtomicBool>,
}

impl HistoricalWorkGate {
    /// Creates a closed historical-work gate tied to canonical phase state.
    #[must_use]
    pub fn new(readiness: Readiness) -> Self {
        metrics::gauge!("zinder_ingest_historical_work_gate_open").set(0.0);
        metrics::gauge!("zinder_ingest_derive_replay_caught_up").set(0.0);
        Self {
            readiness,
            derive_caught_up: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns the shared canonical readiness handle used by derive replay's
    /// bulk-catchup phase gate.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    /// Publishes whether derive replay has materialized the canonical tip.
    pub fn set_derive_caught_up(&self, caught_up: bool) {
        let was_caught_up = self.derive_caught_up.swap(caught_up, Ordering::AcqRel);
        metrics::gauge!("zinder_ingest_derive_replay_caught_up").set(if caught_up {
            1.0
        } else {
            0.0
        });
        self.record_open_metric();
        if was_caught_up == caught_up {
            return;
        }

        if caught_up {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_replay_caught_up",
                "derive replay reached the canonical tip; historical work may run during tip follow"
            );
        } else {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_replay_fell_behind",
                "derive replay no longer covers the canonical tip; historical work is deferred"
            );
        }
    }

    /// Returns whether historical work currently owns any storage budget.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.readiness.phase(), Some(IngestPhase::FollowingTip))
            && self.derive_caught_up.load(Ordering::Acquire)
    }

    fn record_open_metric(&self) {
        metrics::gauge!("zinder_ingest_historical_work_gate_open").set(if self.is_open() {
            1.0
        } else {
            0.0
        });
    }
}

/// Resolved configuration parsed from `services/zinder-ingest/src/config.rs`'s
/// TOML schema.
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
    /// Pipelined-fetch and commit knobs (`[ingest.bulk_catchup]`).
    pub bulk_catchup: BulkCatchupConfig,
    /// Serial-loop knobs (`[ingest.tip_follow]`).
    pub tip_follow: TipFollowPhaseConfig,
    /// One-shot or disposable-store modifiers (`[ingest.modifiers]`).
    pub modifiers: IngestModifiers,
}

impl IngestLoopConfig {
    /// Returns the canonical writer options shared by run, probe, and both ingest phases.
    #[must_use]
    pub fn canonical_store_options(&self) -> zinder_store::ChainStoreOptions {
        crate::chain_ingest::canonical_writer_store_options(
            self.node.network,
            self.reorg_window_blocks,
            self.canonical_rocksdb_budget,
            self.raw_blob_policy,
        )
    }
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
    /// Residual derive lag, in blocks, at which the startup catch-up stops
    /// replaying synchronously and hands the remainder to the always-on
    /// tailer. Startup returns once the derive plane is within this many
    /// blocks of the canonical tip, letting the API and ops surfaces come up
    /// while the tailer drains the rest.
    pub startup_handoff_lag_blocks: u64,
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
    /// Shared source-fetch and block-preparation limits.
    pub pipeline_limits: CanonicalPipelineLimits,
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
    pub checkpoint: Option<CommitmentTreeCheckpoint>,
}

/// Waits until canonical ingest and derive replay release the storage budget
/// to rebuildable historical work.
///
/// Returns `true` when cancellation wins and the caller should exit.
pub(crate) async fn wait_until_historical_work_or_cancelled(
    gate: &HistoricalWorkGate,
    cancel: &CancellationToken,
) -> bool {
    loop {
        gate.record_open_metric();
        if gate.is_open() {
            return false;
        }
        tokio::select! {
            () = cancel.cancelled() => return true,
            () = tokio::time::sleep(BACKGROUND_WORK_PHASE_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn background_work_gate_requires_derive_to_cover_tip() {
        let readiness = Readiness::default();
        readiness.set_phase(IngestPhase::FollowingTip);
        let gate = HistoricalWorkGate::new(readiness);
        let cancel = CancellationToken::new();
        let gate_for_waiter = gate.clone();
        let cancel_for_waiter = cancel.clone();
        let waiter = tokio::spawn(async move {
            wait_until_historical_work_or_cancelled(&gate_for_waiter, &cancel_for_waiter).await
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        gate.set_derive_caught_up(true);
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter).await;
        assert!(matches!(outcome, Ok(Ok(false))));
    }

    #[tokio::test]
    async fn background_work_gate_resumes_after_bulk_catchup_and_derive_catchup() {
        let readiness = Readiness::default();
        readiness.set_phase(IngestPhase::BulkCatchup);
        let gate = HistoricalWorkGate::new(readiness.clone());
        gate.set_derive_caught_up(true);
        let cancel = CancellationToken::new();
        let waiter_gate = gate.clone();
        let gate_cancel = cancel.clone();
        let waiter = tokio::spawn(async move {
            wait_until_historical_work_or_cancelled(&waiter_gate, &gate_cancel).await
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        readiness.set_phase(IngestPhase::FollowingTip);
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter).await;
        assert!(matches!(outcome, Ok(Ok(false))));
    }

    #[tokio::test]
    async fn background_work_gate_exits_when_cancelled_before_tip_follow() {
        let readiness = Readiness::default();
        readiness.set_phase(IngestPhase::BulkCatchup);
        let gate = HistoricalWorkGate::new(readiness);
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(wait_until_historical_work_or_cancelled(&gate, &cancel).await);
    }
}
