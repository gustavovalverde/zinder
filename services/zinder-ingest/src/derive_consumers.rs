//! In-process derive consumer dispatch driven by canonical chain events.
//!
//! `zinder-ingest` opens the derive store as a primary, tails durable
//! canonical chain events, hydrates each event's committed block contexts,
//! and hands those contexts to [`zinder_derive::DeriveStore::write_chain_event`].
//! Consumer writes and cursor advances land in one derive-store write batch
//! per chain epoch.
//!
//! Reader processes (`zinder-query`, `zinder-compat-lightwalletd`, and
//! `zinder-explorer`) open the same derive store path in secondary mode (per
//! [`zinder_derive::DeriveStore::open_secondary`]) and advance their view via
//! [`zinder_derive::DeriveStore::try_catch_up`].

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use prost::Message as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, ChainEpochId,
    TransactionFactsArtifact, TransactionId, TransparentOutPoint, TransparentSpendFact,
};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BlockCommitContext, BlockCommitPayload, BlockSummaryConsumer,
    ChainEventDispatchInputs, DeriveStore, DeriveStoreOptions, IronwoodMigrationConsumer,
    MempoolConsumerEvent, MempoolConsumerEventVariant, MempoolEventCountsConsumer,
    RecentTransactionsConsumer, ReorgIncidentsConsumer,
    TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY, TransactionFeesConsumer,
    TransparentAddressActivityConsumer, TransparentAddressDeltasConsumer,
    TransparentAddressTransactionHistoryConsumer, TransparentOutpointSpendConsumer,
    TransparentSpendFacts,
};
use zinder_proto::v1::wallet::{DeriveHealth, DeriveStatus};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_store::{
    ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest, MempoolEvent, MempoolEventEnvelope,
    PrimaryChainStore, RocksDbResourceBudget, StoreReadCaller, StreamCursorTokenV1,
};

use crate::{
    DeriveReplayPolicy, IngestDeriveConfig, IngestError,
    chain_ingest::{ingest_error_class, outcome_status},
    memory_pressure::RuntimeMemorySnapshot,
};

const DERIVE_REPLAY_STAGE_READ_EVENTS: &str = "read_events";
const DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS: &str = "hydrate_blocks";
const DERIVE_REPLAY_STAGE_BUILD_BLOCK_CONTEXTS: &str = "build_block_contexts";
const DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS: &str = "read_transparent_spend_facts";
const DERIVE_REPLAY_STAGE_DISPATCH_EVENT: &str = "dispatch_event";

/// Default poll cadence for the derive tailer when the canonical store is
/// ingesting faster than chain-event notifications arrive.
pub const DEFAULT_DERIVE_TAILER_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cadence for refreshing the persisted [`DeriveStatus`] head/lag while the
/// tailer stays inside one long catch-up pass.
///
/// A from-genesis rebuild keeps the tailer inside a single
/// [`catch_up_derive_store_to_canonical_with_budget`] call for hours, so a
/// status record written only when that call starts would freeze at the
/// pass's opening head. Re-persisting on this throttle keeps the
/// operator-facing health and indexed head truthful during the pass.
const DERIVE_STATUS_PERSIST_INTERVAL: Duration = Duration::from_secs(1);

/// Cadence for republishing the canonical retention release floor while the
/// tailer stays inside one long catch-up pass.
///
/// Each publish fsyncs the derive write-ahead log and issues one synced
/// canonical write, so publishing on every replayed event would add a steady
/// fsync load on the canonical write path during bulk catch-up. A floor that
/// lags by this interval only defers a sweep, which the design tolerates.
const RETENTION_RELEASE_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// Internal cap on variable fan-out rows one derive replay chunk should stage.
///
/// `replay_batch_blocks` bounds block count, but `recent_transactions` and
/// `transparent_address_transaction_history` scale with transaction/address
/// fan-out. This cap keeps one derive write batch from growing with a dense
/// multi-block event. A single dense block is still admitted because replay
/// cannot split below the block boundary.
const DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK: usize = 50_000;

/// Read-ahead keeps at most one extra hydrated batch in memory, and only when
/// the current batch is comfortably below the projection-row cap.
const DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS: usize =
    DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK / 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeriveReplayBudgetState {
    Normal,
    Degraded,
    Paused,
}

impl DeriveReplayBudgetState {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Degraded => "degraded",
            Self::Paused => "paused",
        }
    }

    const fn is_paused(self) -> bool {
        matches!(self, Self::Paused)
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Degraded => 1,
            Self::Paused => 2,
        }
    }

    const fn max_severity(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EffectiveDeriveReplayLimits {
    state: DeriveReplayBudgetState,
    batch_blocks: u32,
    memory_budget_bytes: Option<u64>,
    memory_pressure_ratio: Option<f64>,
    phase_gate_engaged: bool,
}

/// Live source of the ingest loop phase for the derive replay phase gate.
///
/// The unified ingest loop stamps [`IngestPhase`] on the shared readiness
/// handle every iteration; the derive tailer runs as an independent task and
/// reads the current phase through this handle to decide whether canonical
/// bulk catch-up should own the storage budget.
#[derive(Clone, Debug)]
struct PhaseGateSignal {
    readiness: Readiness,
}

impl PhaseGateSignal {
    const fn new(readiness: Readiness) -> Self {
        Self { readiness }
    }

    fn phase(&self) -> Option<IngestPhase> {
        self.readiness.phase()
    }
}

/// Returns whether the current ingest phase cedes the storage budget to
/// canonical work, throttling derive replay to residual capacity.
///
/// [`IngestPhase::BulkCatchup`] is entered precisely when the canonical gap
/// exceeds `ingest.phases.catchup_threshold_blocks`, so it is the
/// canonical-lag-exceeds-threshold signal the gate needs.
const fn phase_engages_replay_gate(phase: Option<IngestPhase>) -> bool {
    matches!(phase, Some(IngestPhase::BulkCatchup))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DeriveReplayProjectionRows {
    recent_transactions: usize,
    transparent_address_transaction_history: usize,
}

impl DeriveReplayProjectionRows {
    const fn is_empty(self) -> bool {
        self.recent_transactions == 0 && self.transparent_address_transaction_history == 0
    }

    const fn total(self) -> usize {
        self.recent_transactions
            .saturating_add(self.transparent_address_transaction_history)
    }

    const fn saturating_add(self, other: Self) -> Self {
        Self {
            recent_transactions: self
                .recent_transactions
                .saturating_add(other.recent_transactions),
            transparent_address_transaction_history: self
                .transparent_address_transaction_history
                .saturating_add(other.transparent_address_transaction_history),
        }
    }
}

fn projection_rows_for_transactions(
    transactions: &[TransactionFactsArtifact],
) -> DeriveReplayProjectionRows {
    DeriveReplayProjectionRows {
        recent_transactions: RecentTransactionsConsumer::projected_row_count_for_transactions(
            transactions,
        ),
        transparent_address_transaction_history:
            TransparentAddressTransactionHistoryConsumer::projected_row_count_upper_bound_for_transactions(
                transactions,
            ),
    }
}

fn should_start_new_projection_chunk(
    current_rows: DeriveReplayProjectionRows,
    next_block_rows: DeriveReplayProjectionRows,
) -> bool {
    !current_rows.is_empty()
        && current_rows.saturating_add(next_block_rows).total()
            > DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK
}

#[derive(Clone, Debug)]
struct DeriveReplayBudget {
    config: IngestDeriveConfig,
    memory_state: DeriveReplayBudgetState,
    applied_state: DeriveReplayBudgetState,
    phase_gate: Option<PhaseGateSignal>,
}

impl DeriveReplayBudget {
    const fn new(config: IngestDeriveConfig) -> Self {
        Self {
            config,
            memory_state: DeriveReplayBudgetState::Normal,
            applied_state: DeriveReplayBudgetState::Normal,
            phase_gate: None,
        }
    }

    const fn with_phase_gate(config: IngestDeriveConfig, phase_gate: PhaseGateSignal) -> Self {
        Self {
            config,
            memory_state: DeriveReplayBudgetState::Normal,
            applied_state: DeriveReplayBudgetState::Normal,
            phase_gate: Some(phase_gate),
        }
    }

    fn evaluate_current(&mut self) -> EffectiveDeriveReplayLimits {
        let phase = self.phase_gate.as_ref().and_then(PhaseGateSignal::phase);
        self.evaluate(RuntimeMemorySnapshot::sample(), phase)
    }

    fn evaluate(
        &mut self,
        memory_snapshot: RuntimeMemorySnapshot,
        phase: Option<IngestPhase>,
    ) -> EffectiveDeriveReplayLimits {
        let memory_budget_bytes = self
            .config
            .memory_budget_bytes
            .map(std::num::NonZeroU64::get)
            .or(memory_snapshot.cgroup_high_bytes)
            .or(memory_snapshot.cgroup_max_bytes);
        let memory_pressure_bytes = memory_snapshot.non_reclaimable_bytes();
        let memory_pressure_ratio = memory_pressure_bytes.zip(memory_budget_bytes).and_then(
            |(current_bytes, budget_bytes)| {
                (budget_bytes > 0).then(|| u64_to_f64(current_bytes) / u64_to_f64(budget_bytes))
            },
        );
        self.memory_state =
            next_memory_budget_state(self.config, self.memory_state, memory_pressure_ratio);
        let phase_gate_engaged = phase_engages_replay_gate(phase);
        let state = compose_replay_state(self.config, self.memory_state, phase_gate_engaged);
        self.applied_state = state;
        EffectiveDeriveReplayLimits {
            state,
            batch_blocks: effective_replay_batch_blocks(
                self.config,
                state,
                memory_pressure_ratio,
                phase_gate_engaged,
            ),
            memory_budget_bytes,
            memory_pressure_ratio,
            phase_gate_engaged,
        }
    }
}

/// Advances the memory-pressure hysteresis machine independent of policy and
/// phase, so a later gate engagement inherits a warm state instead of a cold
/// `Normal`.
fn next_memory_budget_state(
    config: IngestDeriveConfig,
    current_state: DeriveReplayBudgetState,
    memory_pressure_ratio: Option<f64>,
) -> DeriveReplayBudgetState {
    let Some(pressure_ratio) = memory_pressure_ratio else {
        return DeriveReplayBudgetState::Normal;
    };
    match current_state {
        DeriveReplayBudgetState::Normal => {
            if pressure_ratio >= config.memory_pause_ratio {
                DeriveReplayBudgetState::Paused
            } else if pressure_ratio >= config.memory_degrade_ratio {
                DeriveReplayBudgetState::Degraded
            } else {
                DeriveReplayBudgetState::Normal
            }
        }
        DeriveReplayBudgetState::Degraded => {
            if pressure_ratio >= config.memory_pause_ratio {
                DeriveReplayBudgetState::Paused
            } else if pressure_ratio < config.memory_resume_ratio {
                DeriveReplayBudgetState::Normal
            } else {
                DeriveReplayBudgetState::Degraded
            }
        }
        DeriveReplayBudgetState::Paused => {
            if pressure_ratio >= config.memory_pause_ratio {
                DeriveReplayBudgetState::Paused
            } else if pressure_ratio >= config.memory_resume_ratio {
                DeriveReplayBudgetState::Degraded
            } else {
                DeriveReplayBudgetState::Normal
            }
        }
    }
}

/// Composes the applied replay state from the memory hysteresis machine and the
/// canonical-phase gate, letting the stricter of the two win.
///
/// The gate throttles derive replay to residual (a [`DeriveReplayBudgetState::Degraded`]
/// floor) during canonical bulk catch-up for every policy, so `continuous`
/// keeps its meaning only as an at-tip override. Memory pressure still applies
/// whenever the policy is `canonical-first` or the gate is engaged, and can
/// escalate to [`DeriveReplayBudgetState::Paused`] below the gate's residual
/// level.
fn compose_replay_state(
    config: IngestDeriveConfig,
    memory_state: DeriveReplayBudgetState,
    phase_gate_engaged: bool,
) -> DeriveReplayBudgetState {
    let memory_applies =
        config.replay_policy == DeriveReplayPolicy::CanonicalFirst || phase_gate_engaged;
    let applied_memory_state = if memory_applies {
        memory_state
    } else {
        DeriveReplayBudgetState::Normal
    };
    let gate_floor = if phase_gate_engaged {
        DeriveReplayBudgetState::Degraded
    } else {
        DeriveReplayBudgetState::Normal
    };
    applied_memory_state.max_severity(gate_floor)
}

fn effective_replay_batch_blocks(
    config: IngestDeriveConfig,
    state: DeriveReplayBudgetState,
    memory_pressure_ratio: Option<f64>,
    phase_gate_engaged: bool,
) -> u32 {
    let configured_blocks = config.replay_batch_blocks.get();
    let min_blocks = config.min_replay_batch_blocks.get();
    if state == DeriveReplayBudgetState::Normal {
        return configured_blocks;
    }
    if state == DeriveReplayBudgetState::Paused {
        return 0;
    }
    if phase_gate_engaged {
        return min_blocks;
    }
    let midpoint = config.memory_degrade_ratio
        + ((config.memory_pause_ratio - config.memory_degrade_ratio) / 2.0);
    if memory_pressure_ratio.is_some_and(|ratio| ratio >= midpoint) {
        min_blocks
    } else {
        configured_blocks.saturating_div(2).max(min_blocks)
    }
}

/// Opens the ingest-owned derive store primary for a canonical store path.
pub fn open_primary_derive_store_for_canonical(
    canonical_path: &Path,
    rocksdb_resource_budget: RocksDbResourceBudget,
) -> Result<DeriveStore, zinder_derive::DeriveStoreError> {
    DeriveStore::open(
        DeriveStore::path_for_canonical(canonical_path),
        DeriveStoreOptions {
            sync_writes: false,
            consumers: DeriveStore::bundled_consumers(),
            rocksdb_resource_budget,
        },
    )
}

/// Refuses to run when the durable transparent-outpoint-spend projection is
/// behind the canonical retention sweep.
///
/// The projection sources its spender identities from canonical spend-fact
/// rows, which the safe-tip sweep deletes. Every batch that deletes a fact
/// records the highest deleted height in the canonical deleted-through marker
/// (a checkpoint bootstrap that only advanced the swept cursor leaves it
/// unset). If the projection's durable height fell below that marker (a
/// derive-schema rebuild or wipe after the canonical already swept), those
/// spender identities can never be re-derived: the only remedy is a full
/// canonical re-ingest. Crash loudly rather than serve a silently incomplete
/// projection.
pub fn ensure_spend_projection_not_behind_retention_sweep(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<(), IngestError> {
    let deleted_through = chain_store
        .transparent_retention_deleted_through_height()?
        .map_or(0, BlockHeight::value);
    let projection_height = derive_store
        .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)?
        .map_or(0, BlockHeight::value);
    if projection_height < deleted_through {
        return Err(IngestError::SpendProjectionBehindRetentionSweep {
            projection_height,
            deleted_through,
        });
    }
    Ok(())
}

/// Spawns the ingest-owned chain-event tailer for derive consumers.
///
/// The task is intentionally best-effort from the canonical ingest point of
/// view: canonical commits have already succeeded before the tailer sees an
/// event, so a derive failure is exposed through lag/error metrics and logs
/// without blocking new chain facts from being indexed.
#[allow(
    clippy::too_many_arguments,
    reason = "the tailer binds two stores, the derive config, the poll cadence, the shared readiness handle for the canonical-phase gate, and the cancel token; a spec struct would only relay bindings the binary already holds"
)]
#[must_use = "drop the handle to detach the derive tailer or await it for symmetric shutdown"]
pub fn spawn_derive_tailer_task(
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    readiness: Readiness,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !derive_store.has_consumer_column_families() {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_tailer_disabled",
                "derive tailer disabled because the derive store has no chain-event consumers"
            );
            return;
        }

        tracing::info!(
            target: "zinder::ingest",
            event = "derive_tailer_started",
            poll_interval_ms = u64::try_from(poll_interval.as_millis()).unwrap_or(u64::MAX),
            replay_batch_blocks = derive_config.replay_batch_blocks.get(),
            min_replay_batch_blocks = derive_config.min_replay_batch_blocks.get(),
            replay_policy = derive_config.replay_policy.as_kebab_case(),
            "derive chain-event tailer started"
        );

        let mut replay_budget =
            DeriveReplayBudget::with_phase_gate(derive_config, PhaseGateSignal::new(readiness));
        let mut last_phase_gate_engaged: Option<bool> = None;
        loop {
            let effective_limits = replay_budget.evaluate_current();
            record_derive_replay_budget(
                derive_config.replay_policy,
                effective_limits,
                poll_interval,
            );
            log_phase_gate_transition(
                derive_config.replay_policy,
                &mut last_phase_gate_engaged,
                effective_limits.phase_gate_engaged,
            );
            persist_derive_status(&chain_store, &derive_store, effective_limits.state);
            if effective_limits.state.is_paused() {
                tracing::debug!(
                    target: "zinder::ingest",
                    event = "derive_tailer_replay_paused",
                    replay_policy = derive_config.replay_policy.as_kebab_case(),
                    budget_state = effective_limits.state.as_label(),
                    memory_pressure_ratio = ?effective_limits.memory_pressure_ratio,
                    "derive replay paused so canonical ingest keeps the memory budget"
                );
                if derive_tailer_sleep_or_cancelled(poll_interval, &cancel).await {
                    return;
                }
                continue;
            }

            let started_at = Instant::now();
            let outcome = catch_up_derive_store_to_canonical_with_budget(
                &chain_store,
                &derive_store,
                &mut replay_budget,
            )
            .await;
            record_derive_tailer_tick(started_at, &outcome);
            if let Err(error) = outcome {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "derive_tailer_replay_failed",
                    error = %error,
                    "derive tailer failed to replay canonical chain events; retrying"
                );
            }

            if derive_tailer_sleep_or_cancelled(poll_interval, &cancel).await {
                return;
            }
        }
    })
}

/// Sleeps for `poll_interval` or returns early on cancellation.
///
/// Returns `true` when the tailer was cancelled and should stop.
async fn derive_tailer_sleep_or_cancelled(
    poll_interval: Duration,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        () = cancel.cancelled() => {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_tailer_cancelled",
                "derive chain-event tailer cancelled"
            );
            true
        }
        () = tokio::time::sleep(poll_interval) => false,
    }
}

/// Logs the canonical-phase gate engage/disengage transition once per flip.
///
/// A `continuous` policy is configured never to throttle, so its engagement is
/// a `WARN` that the configured behavior is overridden while canonical bulk
/// catch-up owns the storage budget; every other transition is an `INFO`.
fn log_phase_gate_transition(
    replay_policy: DeriveReplayPolicy,
    last_engaged: &mut Option<bool>,
    engaged: bool,
) {
    if *last_engaged == Some(engaged) {
        return;
    }
    let had_prior_state = last_engaged.is_some();
    *last_engaged = Some(engaged);
    if engaged {
        if replay_policy == DeriveReplayPolicy::Continuous {
            tracing::warn!(
                target: "zinder::ingest",
                event = "derive_replay_phase_gate_engaged",
                replay_policy = replay_policy.as_kebab_case(),
                "continuous derive replay throttled to residual capacity while canonical bulk catch-up owns the storage budget"
            );
        } else {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_replay_phase_gate_engaged",
                replay_policy = replay_policy.as_kebab_case(),
                "derive replay throttled to residual capacity while canonical bulk catch-up owns the storage budget"
            );
        }
    } else if had_prior_state {
        tracing::info!(
            target: "zinder::ingest",
            event = "derive_replay_phase_gate_disengaged",
            replay_policy = replay_policy.as_kebab_case(),
            "derive replay resumed full scheduling after canonical bulk catch-up completed"
        );
    }
}

/// Spawns the derive replay budget metric sampler.
///
/// The derive tailer can spend multiple seconds replaying retained events. This
/// task keeps replay budget gauges tied to current memory pressure instead of
/// to the tailer's next scheduling boundary.
#[must_use = "drop the handle to detach the derive replay budget sampler or await it for symmetric shutdown"]
pub fn spawn_derive_replay_budget_metrics_task(
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    sample_interval: Duration,
    readiness: Readiness,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut replay_budget =
            DeriveReplayBudget::with_phase_gate(derive_config, PhaseGateSignal::new(readiness));
        loop {
            let effective_limits = replay_budget.evaluate_current();
            record_derive_replay_budget(
                derive_config.replay_policy,
                effective_limits,
                poll_interval,
            );

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(sample_interval) => {}
            }
        }
    })
}

/// Replays retained canonical chain events that have not reached the
/// ingest-owned derive store.
///
/// The canonical store commits before the derive store because they are
/// separate `RocksDB` instances. Persisting the canonical chain-event
/// cursor in every chain consumer lets startup repair the only crash gap:
/// the canonical event is durable while the derive cursor still lags.
pub async fn catch_up_derive_store_to_canonical(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    derive_config: IngestDeriveConfig,
) -> Result<(), IngestError> {
    let mut replay_budget = DeriveReplayBudget::new(derive_config);
    catch_up_derive_store_to_canonical_with_budget(chain_store, derive_store, &mut replay_budget)
        .await
}

async fn catch_up_derive_store_to_canonical_with_budget(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    replay_budget: &mut DeriveReplayBudget,
) -> Result<(), IngestError> {
    if !derive_store.has_consumer_column_families() {
        return Ok(());
    }

    catch_up_event_only_chain_event_consumers_to_canonical(chain_store, derive_store)?;
    record_current_derive_replay_tip(chain_store)?;

    let mut cursor = persisted_chain_event_cursor(derive_store)?;
    let mut last_status_persist: Option<Instant> = None;
    let mut last_release_publish: Option<Instant> = None;
    loop {
        let effective_limits = replay_budget.evaluate_current();
        record_derive_replay_budget(
            replay_budget.config.replay_policy,
            effective_limits,
            DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
        );
        if effective_limits.state.is_paused() {
            return Ok(());
        }

        let read_started_at = Instant::now();
        let page_outcome = chain_store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(
                cursor.as_ref(),
            ))
            .map_err(IngestError::from);
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_READ_EVENTS,
            read_started_at,
            &page_outcome,
        );
        let page = page_outcome?;
        if page.is_empty() {
            return Ok(());
        }

        for envelope in page {
            let effective_limits = replay_budget.evaluate_current();
            record_derive_replay_budget(
                replay_budget.config.replay_policy,
                effective_limits,
                DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
            );
            if effective_limits.state.is_paused() {
                return Ok(());
            }
            match replay_chain_event_to_derive(chain_store, derive_store, envelope, replay_budget)
                .await?
            {
                DeriveReplayProgress::Advanced(next_cursor) => {
                    cursor = Some(next_cursor);
                    maybe_publish_retention_release_floor(
                        chain_store,
                        derive_store,
                        &mut last_release_publish,
                    );
                    // Use the budget state the inner replay last evaluated, not
                    // the pre-replay snapshot, so the persisted health reflects
                    // memory pressure as of the just-finished event.
                    maybe_persist_derive_status(
                        chain_store,
                        derive_store,
                        replay_budget.applied_state,
                        &mut last_status_persist,
                    );
                }
                DeriveReplayProgress::Yielded => return Ok(()),
            }
        }
    }
}

/// Publishes the durable transparent-outpoint-spend height as the canonical
/// retention release floor, at most once per
/// [`RETENTION_RELEASE_PUBLISH_INTERVAL`].
fn maybe_publish_retention_release_floor(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    last_publish: &mut Option<Instant>,
) {
    if last_publish.is_some_and(|at| at.elapsed() < RETENTION_RELEASE_PUBLISH_INTERVAL) {
        return;
    }
    *last_publish = Some(Instant::now());
    publish_retention_release_floor(chain_store, derive_store);
}

/// Publishes the durable transparent-outpoint-spend height as the canonical
/// retention release floor.
///
/// The safe-tip sweep releases a spend fact only once this projection has
/// durably recorded its spender identity, so the canonical store never deletes
/// a fact the projection cannot yet resolve. The derive write-ahead log is
/// fsynced before the floor is published: the derive store writes unsynced, so
/// without this a host crash could lose projection rows the floor already
/// authorized the canonical sweep to delete. Best-effort: a failure is logged,
/// never fatal, because the sweep clamps to the last published floor and a
/// missed update only defers a sweep by one cycle.
fn publish_retention_release_floor(chain_store: &PrimaryChainStore, derive_store: &DeriveStore) {
    if let Err(error) = derive_store.flush_wal_to_disk() {
        tracing::warn!(
            target: "zinder::ingest",
            event = "retention_release_floor_flush_failed",
            error = %error,
            "failed to fsync the derive write-ahead log before publishing the retention release floor",
        );
        return;
    }
    let durable_height = match derive_store
        .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)
    {
        Ok(durable_height) => durable_height,
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "retention_release_floor_read_failed",
                error = %error,
                "failed to read the durable transparent-outpoint-spend height",
            );
            return;
        }
    };
    let Some(durable_height) = durable_height else {
        return;
    };
    if let Err(error) = chain_store.set_transparent_retention_release_height(durable_height) {
        tracing::warn!(
            target: "zinder::ingest",
            event = "retention_release_floor_publish_failed",
            error = %error,
            "failed to publish the canonical retention release floor",
        );
    }
}

/// Re-persists [`DeriveStatus`] at most once per
/// [`DERIVE_STATUS_PERSIST_INTERVAL`] so a long catch-up pass keeps the
/// operator-facing head and lag fresh instead of frozen at the pass's start.
fn maybe_persist_derive_status(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    budget_state: DeriveReplayBudgetState,
    last_persist: &mut Option<Instant>,
) {
    if last_persist.is_none_or(|at| at.elapsed() >= DERIVE_STATUS_PERSIST_INTERVAL) {
        persist_derive_status(chain_store, derive_store, budget_state);
        *last_persist = Some(Instant::now());
    }
}

enum DeriveReplayProgress {
    Advanced(StreamCursorTokenV1),
    Yielded,
}

async fn replay_chain_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    replay_budget: &mut DeriveReplayBudget,
) -> Result<DeriveReplayProgress, IngestError> {
    if matches!(envelope.event, ChainEvent::ChainReorged { .. }) {
        return replay_reorg_event_to_derive(chain_store, derive_store, envelope, replay_budget)
            .await;
    }

    let committed_range = committed_block_range_for_chain_event(&envelope)?;
    let block_count = block_height_range_len(committed_range);
    if block_count == 0 {
        return replay_empty_committed_event(chain_store, derive_store, envelope, committed_range)
            .map(DeriveReplayProgress::Advanced);
    }

    replay_committed_event_to_derive_in_batches(
        chain_store,
        derive_store,
        envelope,
        committed_range,
        replay_budget,
    )
    .await
}

fn replay_empty_committed_event(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    committed_range: BlockHeightRange,
) -> Result<StreamCursorTokenV1, IngestError> {
    let contexts = HashMap::new();
    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: &envelope.event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        safe_tip_height: envelope.safe_tip_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, true);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    if let Err(error) = dispatch_outcome {
        record_derive_replay_event(0, Some(&error));
        return Err(error);
    }

    record_derive_replay_event(0, None);
    record_committed_replay_progress(chain_store, committed_range.end)?;
    Ok(envelope.cursor)
}

fn spawn_hydrate_committed_block_replay_batch(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    effective_limits: EffectiveDeriveReplayLimits,
) -> JoinHandle<Result<CanonicalReplayBatch, IngestError>> {
    let chain_store = chain_store.clone();
    let envelope = envelope.clone();
    tokio::task::spawn_blocking(move || {
        let hydrate_started_at = Instant::now();
        let replay_blocks_outcome = hydrate_committed_block_replay_batch(
            &chain_store,
            &envelope,
            start_height,
            end_height,
            effective_limits,
        );
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS,
            hydrate_started_at,
            &replay_blocks_outcome,
        );
        replay_blocks_outcome
    })
}

async fn await_hydrated_replay_batch(
    handle: JoinHandle<Result<CanonicalReplayBatch, IngestError>>,
) -> Result<CanonicalReplayBatch, IngestError> {
    handle
        .await
        .map_err(|join_error| IngestError::BlockingTaskFailed {
            reason: join_error.to_string(),
        })?
}

fn should_read_ahead_derive_replay(
    effective_limits: EffectiveDeriveReplayLimits,
    projection_rows: DeriveReplayProjectionRows,
) -> bool {
    effective_limits.state == DeriveReplayBudgetState::Normal
        && projection_rows.total() <= DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS
}

async fn replay_committed_event_to_derive_in_batches(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    committed_range: BlockHeightRange,
    replay_budget: &mut DeriveReplayBudget,
) -> Result<DeriveReplayProgress, IngestError> {
    let block_count = block_height_range_len(committed_range);
    let mut next_height = committed_range.start;
    let mut pending_replay_batch: Option<JoinHandle<Result<CanonicalReplayBatch, IngestError>>> =
        None;
    while next_height <= committed_range.end {
        catch_up_event_only_chain_event_consumers_to_canonical(chain_store, derive_store)?;
        let effective_limits = evaluate_and_record_replay_budget(replay_budget);
        if effective_limits.state.is_paused() {
            abort_pending_replay_batch(&mut pending_replay_batch);
            return Ok(DeriveReplayProgress::Yielded);
        }

        let replay_batch_handle = pending_replay_batch.take().unwrap_or_else(|| {
            spawn_hydrate_committed_block_replay_batch(
                chain_store,
                &envelope,
                next_height,
                committed_range.end,
                effective_limits,
            )
        });
        let replay_batch =
            await_expected_replay_batch(replay_batch_handle, next_height, block_count).await?;

        let replay_range = replay_batch.block_range;
        let final_chunk = replay_range.end >= committed_range.end;
        let finalized = replay_range.end <= envelope.safe_tip_height;
        let chunk_event = committed_chain_event_chunk(&envelope.event, replay_range);
        let following_height = replay_range.end.next().ok_or_else(|| {
            IngestError::DeriveDispatch("derive replay height overflow".to_owned())
        })?;
        pending_replay_batch = maybe_spawn_read_ahead_replay_batch(ReadAheadReplayBatchInputs {
            chain_store,
            replay_budget,
            envelope: &envelope,
            projection_rows: replay_batch.projection_rows,
            following_height,
            committed_end: committed_range.end,
            final_chunk,
            effective_limits,
        });

        let contexts_outcome =
            build_contexts_for_replay_batch(chain_store, &envelope, replay_batch.blocks, finalized)
                .await;
        let contexts = match contexts_outcome {
            Ok(contexts) => contexts,
            Err(error) => {
                abort_pending_replay_batch(&mut pending_replay_batch);
                record_derive_replay_event(block_count, Some(&error));
                return Err(error);
            }
        };

        let inputs = ChainEventDispatchInputs {
            chain_epoch: envelope.chain_epoch,
            chain_event: &chunk_event,
            chain_cursor: envelope.cursor.as_bytes(),
            event_sequence: envelope.event_sequence,
            safe_tip_height: envelope.safe_tip_height,
        };
        let dispatch_started_at = Instant::now();
        let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, final_chunk);
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
            dispatch_started_at,
            &dispatch_outcome,
        );
        if let Err(error) = dispatch_outcome {
            abort_pending_replay_batch(&mut pending_replay_batch);
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }

        next_height = following_height;
    }

    record_derive_replay_event(block_count, None);
    record_committed_replay_progress(chain_store, committed_range.end)?;
    Ok(DeriveReplayProgress::Advanced(envelope.cursor))
}

fn abort_pending_replay_batch(
    pending_replay_batch: &mut Option<JoinHandle<Result<CanonicalReplayBatch, IngestError>>>,
) {
    if let Some(handle) = pending_replay_batch.take() {
        handle.abort();
    }
}

async fn await_expected_replay_batch(
    replay_batch_handle: JoinHandle<Result<CanonicalReplayBatch, IngestError>>,
    expected_start: BlockHeight,
    block_count: usize,
) -> Result<CanonicalReplayBatch, IngestError> {
    let replay_batch = match await_hydrated_replay_batch(replay_batch_handle).await {
        Ok(replay_batch) => replay_batch,
        Err(error) => {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }
    };
    let replay_range = replay_batch.block_range;
    if replay_range.start == expected_start {
        return Ok(replay_batch);
    }

    let error = IngestError::DeriveDispatch(format!(
        "derive replay read-ahead returned height {} while replay expected {}",
        replay_range.start.value(),
        expected_start.value()
    ));
    record_derive_replay_event(block_count, Some(&error));
    Err(error)
}

fn maybe_spawn_read_ahead_replay_batch(
    inputs: ReadAheadReplayBatchInputs<'_>,
) -> Option<JoinHandle<Result<CanonicalReplayBatch, IngestError>>> {
    let ReadAheadReplayBatchInputs {
        chain_store,
        replay_budget,
        envelope,
        projection_rows,
        following_height,
        committed_end,
        final_chunk,
        effective_limits,
    } = inputs;
    if final_chunk || !should_read_ahead_derive_replay(effective_limits, projection_rows) {
        return None;
    }
    let read_ahead_limits = evaluate_and_record_replay_budget(replay_budget);
    if read_ahead_limits.state.is_paused() {
        return None;
    }
    Some(spawn_hydrate_committed_block_replay_batch(
        chain_store,
        envelope,
        following_height,
        committed_end,
        read_ahead_limits,
    ))
}

async fn build_contexts_for_replay_batch(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    replay_blocks: Vec<CanonicalReplayBlock>,
    finalized: bool,
) -> Result<HashMap<BlockHeight, Arc<BlockCommitContext>>, IngestError> {
    let resolve_started_at = Instant::now();
    let contexts_outcome = build_block_contexts_from_committed_event(
        chain_store,
        envelope.chain_epoch.id,
        replay_blocks,
        finalized,
    )
    .await;
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_BUILD_BLOCK_CONTEXTS,
        resolve_started_at,
        &contexts_outcome,
    );
    contexts_outcome
}

fn evaluate_and_record_replay_budget(
    replay_budget: &mut DeriveReplayBudget,
) -> EffectiveDeriveReplayLimits {
    let effective_limits = replay_budget.evaluate_current();
    record_derive_replay_budget(
        replay_budget.config.replay_policy,
        effective_limits,
        DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
    );
    effective_limits
}

fn record_committed_replay_progress(
    chain_store: &PrimaryChainStore,
    replayed_height: BlockHeight,
) -> Result<(), IngestError> {
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(replayed_height, tip_height);
    }
    Ok(())
}

async fn replay_reorg_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    replay_budget: &mut DeriveReplayBudget,
) -> Result<DeriveReplayProgress, IngestError> {
    let committed_range = committed_block_range_for_chain_event(&envelope)?;
    let block_count = block_height_range_len(committed_range);
    let effective_limits = replay_budget.evaluate_current();
    record_derive_replay_budget(
        replay_budget.config.replay_policy,
        effective_limits,
        DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
    );
    if effective_limits.state.is_paused() {
        return Ok(DeriveReplayProgress::Yielded);
    }

    let hydrate_started_at = Instant::now();
    let replay_blocks_outcome =
        hydrate_committed_blocks_for_reorg_event(chain_store, &envelope, committed_range);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS,
        hydrate_started_at,
        &replay_blocks_outcome,
    );
    let replay_blocks = match replay_blocks_outcome {
        Ok(replay_blocks) => replay_blocks,
        Err(error) => {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }
    };

    let resolve_started_at = Instant::now();
    // Reorg events touch reorg-window blocks that can still change, so keep the
    // per-outpoint visibility check (finalized = false).
    let contexts_outcome = build_block_contexts_from_committed_event(
        chain_store,
        envelope.chain_epoch.id,
        replay_blocks,
        false,
    )
    .await;
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_BUILD_BLOCK_CONTEXTS,
        resolve_started_at,
        &contexts_outcome,
    );
    let contexts = match contexts_outcome {
        Ok(contexts) => contexts,
        Err(error) => {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }
    };

    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: &envelope.event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        safe_tip_height: envelope.safe_tip_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, true);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    if let Err(error) = dispatch_outcome {
        record_derive_replay_event(block_count, Some(&error));
        return Err(error);
    }

    record_derive_replay_event(block_count, None);
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(committed_range.end, tip_height);
    }
    Ok(DeriveReplayProgress::Advanced(envelope.cursor))
}

fn catch_up_event_only_chain_event_consumers_to_canonical(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<(), IngestError> {
    if DeriveStore::bundled_event_only_chain_event_consumer_names().is_empty() {
        return Ok(());
    }

    let mut cursor = persisted_event_only_chain_event_cursor(derive_store)?;
    loop {
        let read_started_at = Instant::now();
        let page_outcome = chain_store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(
                cursor.as_ref(),
            ))
            .map_err(IngestError::from);
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_READ_EVENTS,
            read_started_at,
            &page_outcome,
        );
        let page = page_outcome?;
        if page.is_empty() {
            return Ok(());
        }

        for envelope in page {
            cursor = Some(replay_event_only_chain_event_to_derive(
                derive_store,
                envelope,
            )?);
        }
    }
}

fn replay_event_only_chain_event_to_derive(
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
) -> Result<StreamCursorTokenV1, IngestError> {
    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: &envelope.event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        safe_tip_height: envelope.safe_tip_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_event_only_chain_event(derive_store, inputs);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    dispatch_outcome?;
    Ok(envelope.cursor)
}

fn dispatch_event_only_chain_event(
    derive_store: &DeriveStore,
    inputs: ChainEventDispatchInputs<'_>,
) -> Result<(), IngestError> {
    let mut reorg_incidents = ReorgIncidentsConsumer::new();
    let mut block_consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 0] = [];
    let mut event_consumers: [&mut dyn zinder_derive::DeriveConsumer; 1] = [&mut reorg_incidents];
    let blocks = HashMap::<BlockHeight, Arc<BlockCommitContext>>::new();
    derive_store
        .write_chain_event_chunk_with_event_consumers(
            zinder_derive::ChainEventDispatchConsumers {
                block_consumers: &mut block_consumers,
                event_consumers: &mut event_consumers,
            },
            inputs,
            &blocks,
            true,
        )
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(())
}

fn persisted_event_only_chain_event_cursor(
    derive_store: &DeriveStore,
) -> Result<Option<StreamCursorTokenV1>, IngestError> {
    let mut cursor: Option<Vec<u8>> = None;
    for consumer_name in DeriveStore::bundled_event_only_chain_event_consumer_names() {
        let Some(candidate) = derive_store.get_chain_event_cursor(*consumer_name)? else {
            return Ok(None);
        };
        if let Some(existing) = cursor.as_ref() {
            if existing != &candidate {
                return Err(IngestError::DeriveDispatch(
                    "event-only derive consumer cursors disagree".to_owned(),
                ));
            }
        } else {
            cursor = Some(candidate);
        }
    }
    Ok(cursor.map(StreamCursorTokenV1::from_bytes))
}

fn persisted_chain_event_cursor(
    derive_store: &DeriveStore,
) -> Result<Option<StreamCursorTokenV1>, IngestError> {
    let mut cursor: Option<Vec<u8>> = None;
    for consumer_name in DeriveStore::bundled_chain_event_consumer_names() {
        let Some(candidate) = derive_store.get_chain_event_cursor(*consumer_name)? else {
            // A consumer without a cursor is fresh or was reset by a scoped
            // schema rebuild; it must replay from the earliest retained event
            // while the others re-apply the same deterministic rows idempotently.
            return Ok(None);
        };
        if let Some(existing) = cursor.as_ref() {
            if existing != &candidate {
                return Err(IngestError::DeriveDispatch(
                    "chain derive consumer cursors disagree".to_owned(),
                ));
            }
        } else {
            cursor = Some(candidate);
        }
    }
    Ok(cursor.map(StreamCursorTokenV1::from_bytes))
}

fn committed_block_range_for_chain_event(
    envelope: &ChainEventEnvelope,
) -> Result<BlockHeightRange, IngestError> {
    let (ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. }) =
        &envelope.event
    else {
        return Err(IngestError::DeriveDispatch(
            "unsupported chain event variant".to_owned(),
        ));
    };
    Ok(committed.block_range)
}

/// One block staged for derive replay before its transaction facts are read.
///
/// Phase 1 of [`hydrate_committed_block_replay_batch`] collects these so the
/// facts read can collapse into one batched store read for the whole replay
/// batch instead of one read per block.
struct StagedReplayBlock {
    height: BlockHeight,
    header: BlockHeaderArtifact,
    transaction_ids: Vec<TransactionId>,
}

/// Reads each block's header and ordered transaction ids for the replay batch,
/// returning the staged blocks plus the flat list of every transaction id.
fn stage_committed_replay_blocks(
    reader: &zinder_store::ChainEpochReader<'_>,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    max_blocks: usize,
) -> Result<(Vec<StagedReplayBlock>, Vec<TransactionId>), IngestError> {
    let capacity = usize::try_from(
        end_height
            .value()
            .saturating_sub(start_height.value())
            .saturating_add(1),
    )
    .unwrap_or(usize::MAX)
    .min(max_blocks);
    let mut staged = Vec::with_capacity(capacity);
    let mut batch_transaction_ids = Vec::with_capacity(capacity);
    let mut next_height = start_height;
    while next_height <= end_height && staged.len() < max_blocks {
        let height = next_height;
        let Some(header) = reader.block_header_at(height)? else {
            return Err(IngestError::DeriveDispatch(format!(
                "committed chain event {} references unavailable block-header facts {}",
                envelope.event_sequence,
                height.value()
            )));
        };
        let transaction_ids = reader.transaction_ids_at_height(height)?;
        batch_transaction_ids.extend_from_slice(&transaction_ids);
        staged.push(StagedReplayBlock {
            height,
            header,
            transaction_ids,
        });
        next_height = height.next().ok_or_else(|| {
            IngestError::DeriveDispatch("derive replay height overflow".to_owned())
        })?;
    }
    Ok((staged, batch_transaction_ids))
}

fn hydrate_committed_block_replay_batch(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    effective_limits: EffectiveDeriveReplayLimits,
) -> Result<CanonicalReplayBatch, IngestError> {
    let reader = chain_store
        .chain_epoch_reader_at_for(StoreReadCaller::DeriveHydration, envelope.chain_epoch.id)?;
    let max_blocks = usize::try_from(effective_limits.batch_blocks).unwrap_or(usize::MAX);
    if max_blocks == 0 {
        return Err(IngestError::DeriveDispatch(
            "derive replay batch cannot hydrate while paused".to_owned(),
        ));
    }

    // Phase 1: read each block's header and ordered transaction ids.
    let (staged, batch_transaction_ids) =
        stage_committed_replay_blocks(&reader, envelope, start_height, end_height, max_blocks)?;
    let staged_headers_by_height: HashMap<BlockHeight, BlockHeaderArtifact> = staged
        .iter()
        .map(|staged_block| (staged_block.height, staged_block.header.clone()))
        .collect();

    // Phase 2: one batched facts read for the whole replay batch. Canonical
    // transaction ids are chain-unique, so a single map distributes back to
    // each block without collision. The reorg-safety cross-check reuses the
    // headers staged above instead of re-reading them per unique height.
    let mut facts_by_id = reader.transaction_facts_by_ids_with_known_headers(
        &batch_transaction_ids,
        &staged_headers_by_height,
    )?;

    // Phase 3: assemble each block's ordered transactions from the shared map.
    let mut replay_blocks = Vec::with_capacity(staged.len());
    let mut projection_rows = DeriveReplayProjectionRows::default();
    for staged_block in staged {
        let mut transactions = Vec::with_capacity(staged_block.transaction_ids.len());
        for transaction_id in staged_block.transaction_ids {
            let Some(transaction) = facts_by_id.remove(&transaction_id).flatten() else {
                return Err(IngestError::DeriveDispatch(format!(
                    "committed chain event {} references unavailable transaction facts {}",
                    envelope.event_sequence,
                    hex::encode(transaction_id.as_bytes())
                )));
            };
            transactions.push(transaction);
        }
        let block_projection_rows = projection_rows_for_transactions(&transactions);
        if should_start_new_projection_chunk(projection_rows, block_projection_rows) {
            break;
        }
        projection_rows = projection_rows.saturating_add(block_projection_rows);
        let transparent_spends = transparent_spent_outpoints_for_transactions(&transactions);
        replay_blocks.push(CanonicalReplayBlock {
            height: staged_block.height,
            block_hash: staged_block.header.block_hash,
            previous_block_hash: staged_block.header.parent_hash,
            block_time_unix_seconds: staged_block.header.block_time,
            block_size_bytes: staged_block.header.block_size_bytes,
            transactions,
            transparent_spends,
        });
    }

    let Some(first) = replay_blocks.first() else {
        return Err(IngestError::DeriveDispatch(
            "derive replay batch did not hydrate any blocks".to_owned(),
        ));
    };
    let last = replay_blocks
        .last()
        .ok_or_else(|| IngestError::DeriveDispatch("derive replay batch empty".to_owned()))?;
    Ok(CanonicalReplayBatch {
        block_range: BlockHeightRange::inclusive(first.height, last.height),
        blocks: replay_blocks,
        projection_rows,
    })
}

fn hydrate_committed_blocks_for_reorg_event(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    committed_range: BlockHeightRange,
) -> Result<Vec<CanonicalReplayBlock>, IngestError> {
    let reader = chain_store
        .chain_epoch_reader_at_for(StoreReadCaller::DeriveHydration, envelope.chain_epoch.id)?;
    let mut replay_blocks = Vec::with_capacity(committed_range.into_iter().len());
    for height in committed_range {
        replay_blocks.push(hydrate_committed_block(&reader, envelope, height)?);
    }
    Ok(replay_blocks)
}

fn hydrate_committed_block(
    reader: &zinder_store::ChainEpochReader<'_>,
    envelope: &ChainEventEnvelope,
    height: BlockHeight,
) -> Result<CanonicalReplayBlock, IngestError> {
    let Some(header) = reader.block_header_at(height)? else {
        return Err(IngestError::DeriveDispatch(format!(
            "committed chain event {} references unavailable block-header facts {}",
            envelope.event_sequence,
            height.value()
        )));
    };
    let transaction_ids = reader.transaction_ids_at_height(height)?;
    let known_block_headers = HashMap::from([(height, header.clone())]);
    let mut facts_by_id = reader
        .transaction_facts_by_ids_with_known_headers(&transaction_ids, &known_block_headers)?;
    let mut transactions = Vec::with_capacity(transaction_ids.len());
    for transaction_id in transaction_ids {
        let Some(transaction) = facts_by_id.remove(&transaction_id).flatten() else {
            return Err(IngestError::DeriveDispatch(format!(
                "committed chain event {} references unavailable transaction facts {}",
                envelope.event_sequence,
                hex::encode(transaction_id.as_bytes())
            )));
        };
        transactions.push(transaction);
    }
    let transparent_spends = transparent_spent_outpoints_for_transactions(&transactions);
    Ok(CanonicalReplayBlock {
        height,
        block_hash: header.block_hash,
        previous_block_hash: header.parent_hash,
        block_time_unix_seconds: header.block_time,
        block_size_bytes: header.block_size_bytes,
        transactions,
        transparent_spends,
    })
}

fn committed_chain_event_chunk(event: &ChainEvent, replay_range: BlockHeightRange) -> ChainEvent {
    match event {
        ChainEvent::ChainCommitted { committed } => ChainEvent::ChainCommitted {
            committed: zinder_store::ChainEpochCommitted {
                chain_epoch: committed.chain_epoch,
                block_range: replay_range,
            },
        },
        ChainEvent::ChainReorged { .. } | _ => event.clone(),
    }
}

/// Dispatches block-keyed chain-event consumers against parsed block contexts
/// and lets `DeriveStore` own the write-batch boundary.
pub(crate) fn dispatch_chain_event(
    derive_store: &DeriveStore,
    inputs: ChainEventDispatchInputs<'_>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>>,
    advance_cursor: bool,
) -> Result<(), IngestError> {
    let mut block_summary = BlockSummaryConsumer::new();
    let mut ironwood_migration = IronwoodMigrationConsumer::new();
    let mut transaction_fees = TransactionFeesConsumer::new();
    let mut recent_transactions = RecentTransactionsConsumer::new();
    let mut transparent_activity = TransparentAddressActivityConsumer::new();
    let mut transparent_deltas = TransparentAddressDeltasConsumer::new();
    let mut transparent_transaction_history = TransparentAddressTransactionHistoryConsumer::new();
    let mut transparent_outpoint_spend = TransparentOutpointSpendConsumer::new();
    let mut consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 8] = [
        &mut block_summary,
        &mut ironwood_migration,
        &mut transaction_fees,
        &mut recent_transactions,
        &mut transparent_activity,
        &mut transparent_deltas,
        &mut transparent_transaction_history,
        &mut transparent_outpoint_spend,
    ];
    derive_store
        .write_chain_event_chunk(&mut consumers, inputs, blocks, advance_cursor)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(())
}

/// Dispatches one committed mempool event into ingest-owned derive
/// consumers.
pub(crate) fn dispatch_mempool_event(
    derive_store: &DeriveStore,
    envelope: &MempoolEventEnvelope,
) -> Result<(), IngestError> {
    match &envelope.event {
        MempoolEvent::Added { entry } => {
            let transaction_id = entry.transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Added {
                    transaction_id: &transaction_id,
                    raw_transaction_bytes: entry.raw_transaction_bytes.as_slice(),
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Invalidated { transaction_id, .. } => {
            let transaction_id = transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Invalidated {
                    transaction_id: &transaction_id,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => {
            let transaction_id = transaction_id.as_bytes();
            let block_hash = block_hash.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Mined {
                    transaction_id: &transaction_id,
                    mined_height: *mined_height,
                    block_hash: &block_hash,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Suppressed { transaction_id } => {
            let transaction_id = transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Suppressed {
                    transaction_id: &transaction_id,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        _ => Err(IngestError::DeriveDispatch(
            "unsupported mempool event variant".to_owned(),
        )),
    }
}

fn apply_mempool_event(
    derive_store: &DeriveStore,
    event: &MempoolConsumerEvent<'_>,
    cursor_bytes: &[u8],
) -> Result<(), IngestError> {
    let mut event_counts = MempoolEventCountsConsumer::new();
    derive_store
        .write_mempool_event(&mut event_counts, event, cursor_bytes)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(())
}

async fn build_block_contexts_from_committed_event(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: Vec<CanonicalReplayBlock>,
    finalized: bool,
) -> Result<HashMap<BlockHeight, Arc<BlockCommitContext>>, IngestError> {
    let transparent_spends = read_transparent_spend_facts_for_committed_blocks(
        chain_store,
        chain_epoch_id,
        &replay_blocks,
        finalized,
    )
    .await?;
    let mut out = HashMap::with_capacity(replay_blocks.len());
    for block in replay_blocks {
        let context = BlockCommitContext::new(
            BlockCommitPayload {
                height: block.height,
                block_hash: block.block_hash,
                previous_block_hash: block.previous_block_hash,
                block_time_unix_seconds: block.block_time_unix_seconds,
                block_size_bytes: block.block_size_bytes,
                transactions: block.transactions,
            },
            TransparentSpendFacts::from_map(Arc::clone(&transparent_spends)),
        );
        out.insert(block.height, Arc::new(context));
    }
    Ok(out)
}

struct CanonicalReplayBlock {
    height: BlockHeight,
    block_hash: BlockHash,
    previous_block_hash: BlockHash,
    block_time_unix_seconds: i64,
    block_size_bytes: u64,
    transactions: Vec<TransactionFactsArtifact>,
    transparent_spends: Vec<TransparentOutPoint>,
}

struct CanonicalReplayBatch {
    block_range: BlockHeightRange,
    blocks: Vec<CanonicalReplayBlock>,
    projection_rows: DeriveReplayProjectionRows,
}

struct ReadAheadReplayBatchInputs<'event> {
    chain_store: &'event PrimaryChainStore,
    replay_budget: &'event mut DeriveReplayBudget,
    envelope: &'event ChainEventEnvelope,
    projection_rows: DeriveReplayProjectionRows,
    following_height: BlockHeight,
    committed_end: BlockHeight,
    final_chunk: bool,
    effective_limits: EffectiveDeriveReplayLimits,
}

fn transparent_spent_outpoints_for_transactions(
    transactions: &[TransactionFactsArtifact],
) -> Vec<TransparentOutPoint> {
    let mut spends = Vec::new();
    for transaction in transactions {
        for input in &transaction.transparent_inputs {
            if !input.spent_outpoint.is_coinbase_sentinel() {
                spends.push(input.spent_outpoint);
            }
        }
    }
    spends
}

/// Concurrent blocking reads used to resolve a replay batch's spend facts.
///
/// The spend-fact `multi_get` is disk-seek-bound and serial per call. Splitting
/// the batch's outpoints across this many `spawn_blocking` readers overlaps the
/// seeks across cores, which is the dominant cost of from-genesis derive replay
/// on a multi-core host with idle IO bandwidth.
const SPEND_FACT_RESOLVE_CONCURRENCY: usize = 16;

/// Resolves a batch's transparent spend facts across several blocking readers.
///
/// Outpoints are chain-unique, so chunk result maps have disjoint keys and
/// merge without collision. `finalized` selects the visibility-skipping
/// current-projection read; see
/// [`read_transparent_spend_facts_for_committed_blocks`].
async fn resolve_spend_facts_concurrently(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    outpoints: Vec<TransparentOutPoint>,
    finalized: bool,
) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, IngestError> {
    if outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    let chunk_size = outpoints
        .len()
        .div_ceil(SPEND_FACT_RESOLVE_CONCURRENCY)
        .max(1);
    let mut handles = Vec::with_capacity(SPEND_FACT_RESOLVE_CONCURRENCY);
    for chunk in outpoints.chunks(chunk_size) {
        let chunk = chunk.to_vec();
        let store = chain_store.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let reader = store
                .chain_epoch_reader_at_for(StoreReadCaller::DeriveHydration, chain_epoch_id)?;
            if finalized {
                reader.current_transparent_spend_facts_by_outpoints(&chunk)
            } else {
                reader.transparent_spend_facts_by_outpoints(&chunk)
            }
        }));
    }
    let mut resolved = HashMap::with_capacity(outpoints.len());
    for handle in handles {
        let chunk_map = handle
            .await
            .map_err(|join_error| IngestError::BlockingTaskFailed {
                reason: join_error.to_string(),
            })?
            .map_err(IngestError::from)?;
        resolved.extend(chunk_map);
    }
    Ok(resolved)
}

async fn read_transparent_spend_facts_for_committed_blocks(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: &[CanonicalReplayBlock],
    finalized: bool,
) -> Result<Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>, IngestError> {
    let mut requested_outpoints = HashSet::<TransparentOutPoint>::new();
    for block in replay_blocks {
        requested_outpoints.extend(block.transparent_spends.iter().copied());
    }

    let unique_spent_outpoint_count = requested_outpoints.len();
    record_transparent_spend_fact_requested_outpoints(unique_spent_outpoint_count);
    let outpoints = requested_outpoints.into_iter().collect::<Vec<_>>();
    let read_started_at = Instant::now();
    let read_outcome =
        resolve_spend_facts_concurrently(chain_store, chain_epoch_id, outpoints, finalized).await;
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS,
        read_started_at,
        &read_outcome,
    );
    let resolved = read_outcome?;
    record_transparent_spend_fact_count("resolved", resolved.len());
    record_transparent_spend_fact_count(
        "unresolved",
        unique_spent_outpoint_count.saturating_sub(resolved.len()),
    );
    Ok(Arc::new(resolved))
}

fn record_derive_replay_stage<T>(
    stage: &'static str,
    started_at: Instant,
    outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_derive_replay_stage_duration_seconds",
        "stage" => stage,
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
}

fn record_transparent_spend_fact_count(status: &'static str, count: usize) {
    if count == 0 {
        return;
    }
    metrics::counter!(
        "zinder_ingest_transparent_spend_fact_read_total",
        "status" => status
    )
    .increment(usize_to_u64_saturating(count));
}

fn record_transparent_spend_fact_requested_outpoints(count: usize) {
    metrics::histogram!("zinder_ingest_transparent_spend_fact_requested_outpoint_count")
        .record(usize_to_u32_saturating(count));
}

fn record_derive_replay_event(block_count: usize, error: Option<&IngestError>) {
    let status = if error.is_some() { "error" } else { "ok" };
    let error_class = ingest_error_class(error);
    metrics::counter!(
        "zinder_ingest_derive_replay_events_total",
        "status" => status,
        "error_class" => error_class
    )
    .increment(1);
    metrics::counter!(
        "zinder_ingest_derive_replay_blocks_total",
        "status" => status,
        "error_class" => error_class
    )
    .increment(usize_to_u64_saturating(block_count));
}

fn record_derive_replay_progress(progress_height: BlockHeight, canonical_tip_height: BlockHeight) {
    metrics::gauge!("zinder_ingest_derive_replay_height").set(f64::from(progress_height.value()));
    metrics::gauge!("zinder_ingest_derive_replay_lag_blocks").set(f64::from(
        canonical_tip_height
            .value()
            .saturating_sub(progress_height.value()),
    ));
}

fn record_current_derive_replay_tip(
    chain_store: &PrimaryChainStore,
) -> Result<Option<BlockHeight>, IngestError> {
    let canonical_tip_height = chain_store
        .current_chain_epoch()?
        .map(|epoch| epoch.visible_tip_height);
    if let Some(tip_height) = canonical_tip_height {
        metrics::gauge!("zinder_ingest_derive_replay_tip_height")
            .set(f64::from(tip_height.value()));
    }
    Ok(canonical_tip_height)
}

/// Persists the derive plane's status into the shared derive store each tick.
///
/// The explorer plane surfaces it on `ServerInfo`. Written on the paused branch
/// too, so a stalled derive plane is observable on the wire instead of silent.
/// Best-effort: a write failure is logged, never fatal.
fn persist_derive_status(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    budget_state: DeriveReplayBudgetState,
) {
    let indexed_height = derive_store
        .last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)
        .ok()
        .flatten()
        .map(BlockHeight::value);
    let canonical_tip = record_current_derive_replay_tip(chain_store)
        .ok()
        .flatten()
        .map(BlockHeight::value);
    let lag_blocks = match (canonical_tip, indexed_height) {
        (Some(tip), Some(indexed)) => u64::from(tip.saturating_sub(indexed)),
        (Some(tip), None) => u64::from(tip),
        (None, _) => 0,
    };
    let health = if budget_state.is_paused() {
        DeriveHealth::Paused
    } else if indexed_height.is_some() && lag_blocks == 0 {
        DeriveHealth::Live
    } else {
        DeriveHealth::CatchingUp
    };
    let status = DeriveStatus {
        health: health as i32,
        indexed_height: indexed_height.unwrap_or(0),
        lag_blocks,
        observed_at_millis: now_unix_millis(),
    };
    let mut bytes = Vec::with_capacity(status.encoded_len());
    if let Err(error) = status.encode(&mut bytes) {
        tracing::warn!(
            target: "zinder::ingest",
            event = "derive_status_encode_failed",
            error = %error,
            "failed to encode derive status record",
        );
        return;
    }
    if let Err(error) = derive_store.put_derive_status(&bytes) {
        tracing::warn!(
            target: "zinder::ingest",
            event = "derive_status_persist_failed",
            error = %error,
            "failed to persist derive status record",
        );
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn record_derive_replay_budget(
    replay_policy: DeriveReplayPolicy,
    effective_limits: EffectiveDeriveReplayLimits,
    poll_interval: Duration,
) {
    metrics::gauge!(
        "zinder_ingest_derive_replay_policy",
        "policy" => replay_policy.as_kebab_case()
    )
    .set(1.0);
    metrics::gauge!(
        "zinder_ingest_derive_replay_budget_state",
        "state" => DeriveReplayBudgetState::Normal.as_label()
    )
    .set(
        if effective_limits.state == DeriveReplayBudgetState::Normal {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!(
        "zinder_ingest_derive_replay_budget_state",
        "state" => DeriveReplayBudgetState::Degraded.as_label()
    )
    .set(
        if effective_limits.state == DeriveReplayBudgetState::Degraded {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!(
        "zinder_ingest_derive_replay_budget_state",
        "state" => DeriveReplayBudgetState::Paused.as_label()
    )
    .set(
        if effective_limits.state == DeriveReplayBudgetState::Paused {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!("zinder_ingest_derive_replay_effective_batch_blocks")
        .set(f64::from(effective_limits.batch_blocks));
    if let Some(memory_budget_bytes) = effective_limits.memory_budget_bytes {
        metrics::gauge!("zinder_ingest_derive_replay_memory_budget_bytes")
            .set(u64_to_f64(memory_budget_bytes));
    }
    metrics::gauge!("zinder_ingest_derive_replay_paused").set(
        if effective_limits.state.is_paused() {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!("zinder_ingest_derive_replay_phase_gate").set(
        if effective_limits.phase_gate_engaged {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!("zinder_ingest_derive_replay_budget_seconds").set(
        if effective_limits.state.is_paused() {
            0.0
        } else {
            poll_interval.as_secs_f64()
        },
    );
}

fn record_derive_tailer_tick(started_at: Instant, outcome: &Result<(), IngestError>) {
    metrics::histogram!(
        "zinder_ingest_derive_tailer_tick_duration_seconds",
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_derive_tailer_ticks_total",
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .increment(1);
}

fn block_height_range_len(block_range: BlockHeightRange) -> usize {
    if block_range.start > block_range.end {
        return 0;
    }
    let length = block_range
        .end
        .value()
        .saturating_sub(block_range.start.value())
        .saturating_add(1);
    usize::try_from(length).map_or(usize::MAX, |converted| converted)
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; memory byte counts are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;

    fn replay_config() -> IngestDeriveConfig {
        IngestDeriveConfig {
            replay_batch_blocks: NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN),
            replay_policy: DeriveReplayPolicy::CanonicalFirst,
            memory_budget_bytes: NonZeroU64::new(1_000),
            memory_degrade_ratio: 0.85,
            memory_pause_ratio: 0.95,
            memory_resume_ratio: 0.75,
            min_replay_batch_blocks: NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN),
        }
    }

    fn memory_snapshot(current_bytes: u64) -> RuntimeMemorySnapshot {
        RuntimeMemorySnapshot {
            cgroup_current_bytes: Some(current_bytes),
            cgroup_anon_bytes: Some(current_bytes),
            cgroup_max_bytes: Some(1_000),
            ..RuntimeMemorySnapshot::default()
        }
    }

    #[test]
    fn replay_budget_degrades_batch_before_pause() {
        let mut budget = DeriveReplayBudget::new(replay_config());

        let normal = budget.evaluate(memory_snapshot(800), None);
        assert_eq!(normal.state, DeriveReplayBudgetState::Normal);
        assert_eq!(normal.batch_blocks, 100);

        let degraded = budget.evaluate(memory_snapshot(875), None);
        assert_eq!(degraded.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(degraded.batch_blocks, 50);

        let minimum = budget.evaluate(memory_snapshot(925), None);
        assert_eq!(minimum.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(minimum.batch_blocks, 10);

        let paused = budget.evaluate(memory_snapshot(950), None);
        assert_eq!(paused.state, DeriveReplayBudgetState::Paused);
        assert_eq!(paused.batch_blocks, 0);
    }

    #[test]
    fn replay_budget_resumes_paused_replay_as_degraded_work() {
        let mut budget = DeriveReplayBudget::new(replay_config());

        assert_eq!(
            budget.evaluate(memory_snapshot(960), None).state,
            DeriveReplayBudgetState::Paused
        );
        assert_eq!(
            budget.evaluate(memory_snapshot(900), None).state,
            DeriveReplayBudgetState::Degraded
        );
        assert_eq!(
            budget.evaluate(memory_snapshot(800), None).state,
            DeriveReplayBudgetState::Degraded
        );
        assert_eq!(
            budget.evaluate(memory_snapshot(700), None).state,
            DeriveReplayBudgetState::Normal
        );
    }

    #[test]
    fn replay_budget_uses_anon_pressure_when_cgroup_stat_is_available() {
        let mut budget = DeriveReplayBudget::new(replay_config());
        let snapshot = RuntimeMemorySnapshot {
            cgroup_current_bytes: Some(980),
            cgroup_max_bytes: Some(1_000),
            cgroup_anon_bytes: Some(200),
            cgroup_active_file_bytes: Some(480),
            cgroup_inactive_file_bytes: Some(300),
            ..RuntimeMemorySnapshot::default()
        };

        let limits = budget.evaluate(snapshot, None);

        assert_eq!(limits.memory_pressure_ratio, Some(0.2));
        assert_eq!(limits.state, DeriveReplayBudgetState::Normal);
        assert_eq!(limits.batch_blocks, 100);
    }

    #[test]
    fn replay_budget_uses_process_rss_anon_when_cgroup_anon_is_absent() {
        let mut budget = DeriveReplayBudget::new(replay_config());
        let snapshot = RuntimeMemorySnapshot {
            cgroup_current_bytes: Some(980),
            cgroup_max_bytes: Some(1_000),
            cgroup_anon_bytes: None,
            cgroup_active_file_bytes: Some(480),
            cgroup_inactive_file_bytes: Some(300),
            process_rss_anon_bytes: Some(875),
            ..RuntimeMemorySnapshot::default()
        };

        let limits = budget.evaluate(snapshot, None);

        assert_eq!(limits.memory_pressure_ratio, Some(0.875));
        assert_eq!(limits.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(limits.batch_blocks, 50);
    }

    fn continuous_config() -> IngestDeriveConfig {
        IngestDeriveConfig {
            replay_policy: DeriveReplayPolicy::Continuous,
            ..replay_config()
        }
    }

    #[test]
    fn continuous_bulk_catchup_engages_residual_replay() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate(memory_snapshot(800), Some(IngestPhase::BulkCatchup));

        assert!(limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(limits.batch_blocks, 10);
    }

    #[test]
    fn continuous_following_tip_stays_unthrottled_under_memory_pressure() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate(memory_snapshot(950), Some(IngestPhase::FollowingTip));

        assert!(!limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Normal);
        assert_eq!(limits.batch_blocks, 100);
    }

    #[test]
    fn canonical_first_composes_memory_pause_with_bulk_catchup_gate() {
        let mut budget = DeriveReplayBudget::new(replay_config());

        let residual = budget.evaluate(memory_snapshot(800), Some(IngestPhase::BulkCatchup));
        assert!(residual.phase_gate_engaged);
        assert_eq!(residual.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(residual.batch_blocks, 10);

        let paused = budget.evaluate(memory_snapshot(950), Some(IngestPhase::BulkCatchup));
        assert!(paused.phase_gate_engaged);
        assert_eq!(paused.state, DeriveReplayBudgetState::Paused);
        assert_eq!(paused.batch_blocks, 0);
    }

    #[test]
    fn phase_gate_disengages_when_phase_leaves_bulk_catchup() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let engaged = budget.evaluate(memory_snapshot(800), Some(IngestPhase::BulkCatchup));
        assert!(engaged.phase_gate_engaged);
        assert_eq!(engaged.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(engaged.batch_blocks, 10);

        let disengaged = budget.evaluate(memory_snapshot(800), Some(IngestPhase::FollowingTip));
        assert!(!disengaged.phase_gate_engaged);
        assert_eq!(disengaged.state, DeriveReplayBudgetState::Normal);
        assert_eq!(disengaged.batch_blocks, 100);
    }

    #[test]
    fn phase_gate_transition_logs_once_per_flip() {
        let mut last_engaged = None;

        log_phase_gate_transition(DeriveReplayPolicy::Continuous, &mut last_engaged, false);
        assert_eq!(last_engaged, Some(false));

        log_phase_gate_transition(DeriveReplayPolicy::Continuous, &mut last_engaged, true);
        assert_eq!(last_engaged, Some(true));

        log_phase_gate_transition(DeriveReplayPolicy::Continuous, &mut last_engaged, true);
        assert_eq!(last_engaged, Some(true));

        log_phase_gate_transition(DeriveReplayPolicy::Continuous, &mut last_engaged, false);
        assert_eq!(last_engaged, Some(false));
    }

    #[test]
    fn projection_row_cap_keeps_at_least_one_block_per_chunk() {
        let oversized_block_rows = DeriveReplayProjectionRows {
            recent_transactions: DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK
                .saturating_add(1),
            transparent_address_transaction_history: 0,
        };

        assert!(!should_start_new_projection_chunk(
            DeriveReplayProjectionRows::default(),
            oversized_block_rows,
        ));
    }

    #[test]
    fn projection_row_cap_closes_chunk_before_next_block_exceeds_limit() {
        let current_rows = DeriveReplayProjectionRows {
            recent_transactions: DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK
                .saturating_sub(1),
            transparent_address_transaction_history: 0,
        };
        let next_block_rows = DeriveReplayProjectionRows {
            recent_transactions: 2,
            transparent_address_transaction_history: 0,
        };

        assert!(should_start_new_projection_chunk(
            current_rows,
            next_block_rows,
        ));
    }

    #[test]
    fn read_ahead_only_runs_for_normal_small_projection_batches() {
        let normal_limits = EffectiveDeriveReplayLimits {
            state: DeriveReplayBudgetState::Normal,
            batch_blocks: 100,
            memory_budget_bytes: None,
            memory_pressure_ratio: None,
            phase_gate_engaged: false,
        };
        let degraded_limits = EffectiveDeriveReplayLimits {
            state: DeriveReplayBudgetState::Degraded,
            ..normal_limits
        };
        let small_rows = DeriveReplayProjectionRows {
            recent_transactions: DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS,
            transparent_address_transaction_history: 0,
        };
        let dense_rows = DeriveReplayProjectionRows {
            recent_transactions: DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS
                .saturating_add(1),
            transparent_address_transaction_history: 0,
        };

        assert!(should_read_ahead_derive_replay(normal_limits, small_rows));
        assert!(!should_read_ahead_derive_replay(normal_limits, dense_rows));
        assert!(!should_read_ahead_derive_replay(
            degraded_limits,
            small_rows
        ));
    }
}
