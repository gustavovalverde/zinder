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
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::{Mutex, MutexGuard};
use prost::Message as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockFinalNoteCommitmentRoots, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange,
    ChainEpochId, TransactionFactsArtifact, TransactionId, TransactionIntrinsicValueBalances,
    TransparentOutPoint, TransparentSpendFact,
};
use zinder_derive::{
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME, BLOCK_SUMMARY_COLUMN_FAMILY, BlockCommitContext,
    BlockCommitPayload, BlockProductionTimeConsumer, BlockSummaryConsumer,
    COMMITMENT_ROOT_SEARCH_CONSUMER_NAME, CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
    ChainEventDispatchInputs, CommitmentRootSearchConsumer, ConsumerProjectionState,
    ConventionalFeeDistributionConsumer, DeriveConsumerName, DeriveStore, DeriveStoreOptions,
    IronwoodMigrationConsumer, MEMPOOL_EVENT_COUNTS_CONSUMER_NAME, MempoolConsumerEvent,
    MempoolConsumerEventVariant, MempoolEventCountsConsumer, PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
    PaidFeeDistributionConsumer, ProjectionPreset, ProjectionWriteMeasurement,
    RecentTransactionsConsumer, ReorgIncidentsConsumer,
    TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
    TransactionComponentSummaryConsumer, TransactionFeesConsumer, TransactionHistoryConsumer,
    TransactionIntrinsicValueBalanceFacts, TransparentAddressActivityConsumer,
    TransparentAddressDeltasConsumer, TransparentAddressRankingConsumer,
    TransparentAddressTransactionHistoryConsumer, TransparentOutpointSpendConsumer,
    TransparentSpendFacts, VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME, ValuePoolFlowHistoryConsumer,
};
use zinder_proto::v1::wallet::{DeriveHealth, DeriveStatus};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_store::{
    ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest, MempoolEvent, MempoolEventEnvelope,
    PrimaryChainStore, RocksDbResourceBudget, StoreReadCaller, StreamCursorTokenV1,
    TransparentSpendReplayBlock,
};

use crate::{
    DeriveReplayPolicy, IngestDeriveConfig, IngestError,
    chain_ingest::{ingest_error_class, outcome_status},
    conventional_fee_distribution_backfill::seed_conventional_fee_distribution_visible_tail,
    ingest_loop::HistoricalWorkGate,
    memory_pressure::RuntimeMemorySnapshot,
    transaction_component_backfill::seed_transaction_component_visible_tail,
};

const DERIVE_REPLAY_STAGE_READ_EVENTS: &str = "read_events";
const DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS: &str = "hydrate_blocks";
const DERIVE_REPLAY_STAGE_BUILD_BLOCK_CONTEXTS: &str = "build_block_contexts";
const DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS: &str = "read_transparent_spend_facts";
const DERIVE_REPLAY_STAGE_DISPATCH_EVENT: &str = "dispatch_event";
const PROJECTION_WRITE_SOURCE_CHAIN_EVENT: &str = "chain_event";
const PROJECTION_WRITE_SOURCE_MEMPOOL_EVENT: &str = "mempool_event";
static DERIVE_PROJECTION_WRITE_LOCK: Mutex<()> = parking_lot::const_mutex(());

pub(crate) fn derive_projection_write_guard() -> MutexGuard<'static, ()> {
    DERIVE_PROJECTION_WRITE_LOCK.lock()
}

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
/// `replay_batch_blocks` bounds block count, but transaction projections and
/// `transparent_address_transaction_history` scale with transaction/address
/// fan-out. This cap keeps one derive write batch from growing with a dense
/// multi-block event. A single dense block is still admitted because replay
/// cannot split below the block boundary.
const DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK: usize = 50_000;

/// Maximum blocks whose transaction facts one derive hydration read may hold.
///
/// The projection-row cap is evaluated only after transaction facts are
/// decoded. Reading every configured replay block before applying that cap
/// lets a dense historical span retain far more decoded facts than the chunk
/// will dispatch. This independent prefetch bound limits that unavoidable
/// look-ahead while preserving batched canonical reads.
const DERIVE_REPLAY_FACTS_READ_MAX_BLOCKS: usize = 10;

fn bounded_facts_read_groups<T>(staged_blocks: &[T]) -> std::slice::Chunks<'_, T> {
    staged_blocks.chunks(DERIVE_REPLAY_FACTS_READ_MAX_BLOCKS)
}

/// Read-ahead keeps at most one extra hydrated batch in memory, and only when
/// the current batch is comfortably below the projection-row cap.
const DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS: usize =
    DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK / 2;

// Variant order is throttle severity; `Ord` picks the stricter state.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EffectiveDeriveReplayLimits {
    state: DeriveReplayBudgetState,
    batch_blocks: u32,
    memory_budget_bytes: Option<u64>,
    memory_pressure_ratio: Option<f64>,
    phase_gate_engaged: bool,
}

/// Returns whether the current ingest phase gives the storage budget
/// exclusively to canonical work by pausing derive replay.
///
/// Replay fails closed until the unified loop has positively classified
/// [`IngestPhase::FollowingTip`]. This prevents startup and upstream-wait
/// windows from admitting derive work before canonical ownership is known.
const fn phase_engages_replay_gate(phase: Option<IngestPhase>) -> bool {
    !matches!(phase, Some(IngestPhase::FollowingTip))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DeriveReplayProjectionRows {
    transaction_rows: usize,
    transparent_address_transaction_history: usize,
}

impl DeriveReplayProjectionRows {
    const fn is_empty(self) -> bool {
        self.transaction_rows == 0 && self.transparent_address_transaction_history == 0
    }

    const fn total(self) -> usize {
        self.transaction_rows
            .saturating_add(self.transparent_address_transaction_history)
    }

    const fn saturating_add(self, other: Self) -> Self {
        Self {
            transaction_rows: self.transaction_rows.saturating_add(other.transaction_rows),
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
        transaction_rows: RecentTransactionsConsumer::projected_row_count_for_transactions(
            transactions,
        )
        .saturating_add(TransactionHistoryConsumer::projected_row_count_for_transactions(
            transactions,
        )),
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
    /// Live source of the ingest loop phase for the canonical-phase gate; the
    /// unified ingest loop stamps [`IngestPhase`] on this shared handle every
    /// iteration.
    phase_gate: Option<Readiness>,
    /// Point at which the current pass stops draining and returns. The tailer
    /// drains fully; startup returns once the derive plane reaches the handoff
    /// lag or the wall-clock budget.
    bound: DeriveCatchUpBound,
    /// Process cancellation sampled at replay chunk boundaries.
    cancel: Option<CancellationToken>,
}

impl DeriveReplayBudget {
    const fn new(config: IngestDeriveConfig) -> Self {
        Self {
            config,
            memory_state: DeriveReplayBudgetState::Normal,
            applied_state: DeriveReplayBudgetState::Normal,
            phase_gate: None,
            bound: DeriveCatchUpBound::Drain,
            cancel: None,
        }
    }

    const fn with_phase_gate(config: IngestDeriveConfig, readiness: Readiness) -> Self {
        Self {
            config,
            memory_state: DeriveReplayBudgetState::Normal,
            applied_state: DeriveReplayBudgetState::Normal,
            phase_gate: Some(readiness),
            bound: DeriveCatchUpBound::Drain,
            cancel: None,
        }
    }

    fn with_phase_gate_and_cancel(
        config: IngestDeriveConfig,
        readiness: Readiness,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            memory_state: DeriveReplayBudgetState::Normal,
            applied_state: DeriveReplayBudgetState::Normal,
            phase_gate: Some(readiness),
            bound: DeriveCatchUpBound::Drain,
            cancel: Some(cancel),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn evaluate_current(&mut self) -> EffectiveDeriveReplayLimits {
        let phase = self
            .phase_gate
            .as_ref()
            .map_or(Some(IngestPhase::FollowingTip), Readiness::phase);
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
/// The gate pauses derive replay during canonical bulk catch-up for every
/// policy, so `continuous` keeps its meaning only as an at-tip override.
/// Rebuildable projections resume once canonical ingest enters tip follow.
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
        DeriveReplayBudgetState::Paused
    } else {
        DeriveReplayBudgetState::Normal
    };
    applied_memory_state.max(gate_floor)
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
    open_primary_derive_store_for_canonical_with_projection_preset(
        canonical_path,
        rocksdb_resource_budget,
        ProjectionPreset::Explorer,
    )
}

/// Opens the ingest-owned derive store primary with one closed projection
/// preset.
pub fn open_primary_derive_store_for_canonical_with_projection_preset(
    canonical_path: &Path,
    rocksdb_resource_budget: RocksDbResourceBudget,
    projection_preset: ProjectionPreset,
) -> Result<DeriveStore, zinder_derive::DeriveStoreError> {
    DeriveStore::open_with_projection_preset(
        DeriveStore::path_for_canonical(canonical_path),
        projection_preset,
        DeriveStoreOptions {
            sync_writes: false,
            rocksdb_resource_budget,
            ..DeriveStoreOptions::default()
        },
    )
}

const BACKFILL_OWNED_BLOCK_CONSUMERS: [DeriveConsumerName; 6] = [
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
    COMMITMENT_ROOT_SEARCH_CONSUMER_NAME,
    CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
    PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
    TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME,
    VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
];
fn backfill_tail_seed_batch_blocks() -> NonZeroU32 {
    NonZeroU32::new(256).unwrap_or(NonZeroU32::MIN)
}

/// Seeds missing event cursors for consumers with dedicated historical backfills.
///
/// A newly declared block consumer normally starts without a cursor, causing
/// every bundled block consumer to replay from retained history. Backfill-owned
/// consumers instead reconstruct settled history from canonical facts. When
/// every other bundled block consumer agrees on one cursor, each missing
/// backfill-owned consumer can join at that exact boundary. A fresh or partially
/// rebuilt derive store is left untouched so the normal replay contract applies.
pub fn seed_backfill_owned_consumer_cursors(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<(), IngestError> {
    let Some(cursor) = unanimous_existing_block_consumer_cursor(derive_store)? else {
        return Ok(());
    };
    let missing_consumers = missing_backfill_consumer_cursors(derive_store, &cursor)?;
    if missing_consumers.is_empty() {
        return Ok(());
    }
    let Some(authoritative_height) =
        derive_store.last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
    else {
        return Ok(());
    };
    seed_conventional_fee_distribution_cursor(
        chain_store,
        derive_store,
        &cursor,
        &missing_consumers,
        authoritative_height,
    )?;
    seed_block_production_time_cursor(
        chain_store,
        derive_store,
        &cursor,
        &missing_consumers,
        authoritative_height,
    )?;
    seed_transaction_component_cursor(
        chain_store,
        derive_store,
        &cursor,
        &missing_consumers,
        authoritative_height,
    )?;
    for consumer_name in missing_consumers.into_iter().filter(|name| {
        ![
            CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
            BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
            PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
            TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME,
            VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
        ]
        .contains(name)
    }) {
        derive_store.put_chain_event_cursor(consumer_name, &cursor)?;
        tracing::info!(
            target: "zinder::ingest",
            event = "backfill_owned_consumer_cursor_seeded",
            consumer = consumer_name.as_str(),
            "derive consumer joined the existing event boundary; historical coverage remains backfill-owned"
        );
    }
    Ok(())
}

fn seed_block_production_time_cursor(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    cursor: &[u8],
    missing_consumers: &[DeriveConsumerName],
    authoritative_height: BlockHeight,
) -> Result<(), IngestError> {
    let cursor_is_missing = missing_consumers.contains(&BLOCK_PRODUCTION_TIME_CONSUMER_NAME);
    if cursor_is_missing {
        let boundary_height = authoritative_height.next().ok_or_else(|| {
            IngestError::DeriveDispatch(
                "block-production time tail boundary height overflow".to_owned(),
            )
        })?;
        BlockProductionTimeConsumer::initialize_tail_boundary(derive_store, boundary_height)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
        derive_store.put_chain_event_cursor(BLOCK_PRODUCTION_TIME_CONSUMER_NAME, cursor)?;
        tracing::info!(
            target: "zinder::ingest",
            event = "block_production_time_tail_boundary_initialized",
            tail_boundary = boundary_height.value(),
            "block-production time consumer joined the existing derive event boundary"
        );
    }
    if derive_store
        .consumer_projection_state(BLOCK_PRODUCTION_TIME_CONSUMER_NAME)?
        .is_none()
    {
        let chain_epoch = chain_store.current_chain_epoch()?.ok_or_else(|| {
            IngestError::DeriveDispatch(
                "canonical chain epoch is missing while seeding block-production time state"
                    .to_owned(),
            )
        })?;
        let projection_tip_hash = chain_store
            .chain_epoch_reader_at(chain_epoch.id)?
            .block_header_at(authoritative_height)?
            .ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "canonical block {} is missing while seeding block-production time state",
                    authoritative_height.value(),
                ))
            })?
            .block_hash;
        derive_store.put_consumer_projection_state(
            BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
            ConsumerProjectionState {
                projection_epoch_id: chain_epoch.id,
                projection_tip_height: authoritative_height,
                projection_tip_hash,
                revision: 1,
                coverage: None,
            },
        )?;
    }
    Ok(())
}

fn seed_conventional_fee_distribution_cursor(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    cursor: &[u8],
    missing_consumers: &[DeriveConsumerName],
    authoritative_height: BlockHeight,
) -> Result<(), IngestError> {
    let cursor_is_missing =
        missing_consumers.contains(&CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME);
    let chain_epoch = chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical chain epoch is missing while seeding conventional-fee distribution tail"
                .to_owned(),
        )
    })?;
    let desired_tail_boundary = backfill_consumer_tail_boundary(
        chain_epoch.settled_tip_height,
        authoritative_height,
        "conventional-fee distribution",
    )?;
    let tail_boundary_changed =
        ConventionalFeeDistributionConsumer::widen_tail_boundary_for_startup(
            derive_store,
            desired_tail_boundary,
        )
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let tail_needs_seed = ConventionalFeeDistributionConsumer::tail_coverage(derive_store)?
        .is_some_and(|tail| {
            tail.complete_through_height
                .is_none_or(|through| through < authoritative_height)
        });
    if cursor_is_missing || tail_boundary_changed || tail_needs_seed {
        seed_conventional_fee_distribution_visible_tail(
            chain_store,
            derive_store,
            authoritative_height,
            backfill_tail_seed_batch_blocks(),
        )?;
    }
    if cursor_is_missing {
        derive_store.put_chain_event_cursor(CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME, cursor)?;
    }
    if cursor_is_missing || tail_boundary_changed || tail_needs_seed {
        let tail_boundary = ConventionalFeeDistributionConsumer::tail_coverage(derive_store)?
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "conventional-fee distribution tail coverage disappeared during startup"
                        .to_owned(),
                )
            })?
            .boundary_height;
        tracing::info!(
            target: "zinder::ingest",
            event = "conventional_fee_distribution_tail_boundary_initialized",
            cursor_seeded = cursor_is_missing,
            tail_boundary = tail_boundary.value(),
            "conventional-fee distribution consumer joined the existing derive event boundary"
        );
    }
    Ok(())
}

pub(crate) fn unanimous_existing_block_consumer_cursor(
    derive_store: &DeriveStore,
) -> Result<Option<Vec<u8>>, IngestError> {
    let mut agreed_cursor: Option<Vec<u8>> = None;
    for consumer_name in derive_store.chain_event_consumer_names().filter(|name| {
        !BACKFILL_OWNED_BLOCK_CONSUMERS.contains(name)
            && *name != TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME
    }) {
        let Some(candidate) = derive_store.get_chain_event_cursor(consumer_name)? else {
            return Ok(None);
        };
        if agreed_cursor
            .as_ref()
            .is_some_and(|existing| existing != &candidate)
        {
            return Err(IngestError::DeriveDispatch(
                "existing block derive consumer cursors disagree while seeding backfill-owned consumers"
                    .to_owned(),
            ));
        }
        agreed_cursor = Some(candidate);
    }
    Ok(agreed_cursor)
}

fn missing_backfill_consumer_cursors(
    derive_store: &DeriveStore,
    cursor: &[u8],
) -> Result<Vec<DeriveConsumerName>, IngestError> {
    let mut missing_consumers = Vec::new();
    for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS
        .into_iter()
        .filter(|consumer_name| derive_store.has_consumer(*consumer_name))
    {
        match derive_store.get_chain_event_cursor(consumer_name)? {
            Some(existing) if existing != cursor => {
                return Err(IngestError::DeriveDispatch(
                    "backfill-owned derive consumer cursor disagrees with the existing block consumer boundary"
                        .to_owned(),
                ));
            }
            Some(_) => {}
            None => missing_consumers.push(consumer_name),
        }
    }
    Ok(missing_consumers)
}

fn seed_transaction_component_cursor(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    cursor: &[u8],
    missing_consumers: &[DeriveConsumerName],
    authoritative_height: BlockHeight,
) -> Result<(), IngestError> {
    let component_cursor_is_missing =
        missing_consumers.contains(&TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME);
    let chain_epoch = chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical chain epoch is missing while seeding transaction-component tail".to_owned(),
        )
    })?;
    let desired_tail_boundary = backfill_consumer_tail_boundary(
        chain_epoch.settled_tip_height,
        authoritative_height,
        "transaction-component",
    )?;
    let tail_boundary_changed =
        TransactionComponentSummaryConsumer::widen_tail_boundary_for_startup(
            derive_store,
            desired_tail_boundary,
        )
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let tail_needs_seed = TransactionComponentSummaryConsumer::tail_coverage(derive_store)?
        .is_some_and(|tail| {
            tail.complete_through_height
                .is_none_or(|through| through < authoritative_height)
        });
    if component_cursor_is_missing || tail_boundary_changed || tail_needs_seed {
        seed_transaction_component_visible_tail(
            chain_store,
            derive_store,
            authoritative_height,
            backfill_tail_seed_batch_blocks(),
        )?;
    }
    if component_cursor_is_missing {
        derive_store.put_chain_event_cursor(TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME, cursor)?;
    }
    if component_cursor_is_missing || tail_boundary_changed || tail_needs_seed {
        let tail_boundary = TransactionComponentSummaryConsumer::tail_coverage(derive_store)?
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "transaction-component tail coverage disappeared during startup".to_owned(),
                )
            })?
            .boundary_height;
        tracing::info!(
            target: "zinder::ingest",
            event = "transaction_component_tail_boundary_initialized",
            cursor_seeded = component_cursor_is_missing,
            tail_boundary = tail_boundary.value(),
            "transaction-component consumer joined the existing derive event boundary"
        );
    }
    Ok(())
}

pub(crate) fn backfill_consumer_tail_boundary(
    settled_tip_height: BlockHeight,
    authoritative_height: BlockHeight,
    projection: &str,
) -> Result<BlockHeight, IngestError> {
    BlockHeight::new(settled_tip_height.value().min(authoritative_height.value()))
        .next()
        .ok_or_else(|| {
            IngestError::DeriveDispatch(format!("{projection} live-tail boundary height overflow"))
        })
}

/// Compatibility wrapper for callers using the original root-specific API.
pub fn seed_commitment_root_search_cursor_for_backfill(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<(), IngestError> {
    seed_backfill_owned_consumer_cursors(chain_store, derive_store)
}

/// Spawns the ingest-owned chain-event tailer for derive consumers.
///
/// The task is intentionally best-effort from the canonical ingest point of
/// view: canonical commits have already succeeded before the tailer sees an
/// event, so a derive failure is exposed through lag/error metrics and logs
/// without blocking new chain facts from being indexed.
#[allow(
    clippy::too_many_arguments,
    reason = "the tailer binds two stores, replay policy, poll cadence, the shared work scheduler, and cancellation; a spec struct would only relay bindings the binary already holds"
)]
#[must_use = "drop the handle to detach the derive tailer or await it for symmetric shutdown"]
pub fn spawn_derive_tailer_task(
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !derive_store.has_consumer_column_families() {
            historical_work_gate.set_derive_caught_up(true);
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

        let mut replay_budget = DeriveReplayBudget::with_phase_gate_and_cancel(
            derive_config,
            historical_work_gate.readiness(),
            cancel.clone(),
        );
        loop {
            refresh_historical_work_gate(&chain_store, &derive_store, &historical_work_gate);
            let effective_limits = replay_budget.evaluate_current();
            record_derive_replay_budget(
                derive_config.replay_policy,
                effective_limits,
                poll_interval,
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
            refresh_historical_work_gate(&chain_store, &derive_store, &historical_work_gate);
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

fn refresh_historical_work_gate(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    historical_work_gate: &HistoricalWorkGate,
) {
    let caught_up = derive_replay_caught_up(chain_store, derive_store).unwrap_or_else(|error| {
        tracing::warn!(
            target: "zinder::ingest",
            event = "derive_replay_gate_refresh_failed",
            error = %error,
            "failed to compare derive replay with the canonical tip; historical work remains deferred"
        );
        false
    });
    historical_work_gate.set_derive_caught_up(caught_up);
}

fn derive_replay_caught_up(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<bool, IngestError> {
    let canonical_tip = chain_store
        .current_chain_epoch()?
        .map(|epoch| epoch.visible_tip_height);
    let indexed_height = derive_store
        .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)?;
    Ok(canonical_tip.is_none_or(|tip| indexed_height.is_some_and(|indexed| indexed >= tip)))
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
                "continuous derive replay paused while canonical bulk catch-up owns the storage budget"
            );
        } else {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_replay_phase_gate_engaged",
                replay_policy = replay_policy.as_kebab_case(),
                "derive replay paused while canonical bulk catch-up owns the storage budget"
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
/// The derive tailer can stay inside one replay pass for a whole bulk catch-up
/// (hours), so its outer scheduling boundary is the wrong place to observe
/// phase-gate transitions. This task samples the budget on `sample_interval`,
/// keeping the replay budget gauges tied to current memory pressure and
/// emitting the phase-gate engage/disengage log within one sample of the flip
/// regardless of the tailer's progress. Owning both the gauge and the
/// transition log here keeps them from disagreeing.
#[must_use = "drop the handle to detach the derive replay budget sampler or await it for symmetric shutdown"]
pub fn spawn_derive_replay_budget_metrics_task(
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    sample_interval: Duration,
    readiness: Readiness,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut replay_budget = DeriveReplayBudget::with_phase_gate(derive_config, readiness);
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

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(sample_interval) => {}
            }
        }
    })
}

/// Wall-clock ceiling on the startup derive catch-up before it hands residual
/// replay to the always-on tailer.
///
/// A dense-band restart can leave the canonical store leading the derive plane
/// by tens of thousands of blocks. Draining that synchronously inside the fatal
/// `open_storage` phase kept the whole service unavailable while it ran; this
/// budget caps that window. The tailer resumes from the persisted consumer
/// cursors, so any residual drains without data loss.
const STARTUP_DERIVE_HANDOFF_BUDGET: Duration = Duration::from_secs(30);

/// Bound on how far the derive catch-up drains before returning.
#[derive(Clone, Copy, Debug)]
enum DeriveCatchUpBound {
    /// Drain every retained chain event. The always-on tailer runs this way.
    Drain,
    /// Return once the derive plane is within `max_lag_blocks` of the canonical
    /// tip or `deadline` passes, whichever comes first. Startup runs this way
    /// so the API and ops surfaces come up while the tailer drains the rest.
    Handoff {
        canonical_tip_height: Option<BlockHeight>,
        max_lag_blocks: u64,
        deadline: Instant,
    },
}

impl DeriveCatchUpBound {
    /// Returns whether the catch-up has drained enough to hand off, reading the
    /// current wallet-correctness head shared by every supported preset.
    fn handoff_reached(self, derive_store: &DeriveStore) -> Result<bool, IngestError> {
        let Self::Handoff {
            canonical_tip_height,
            max_lag_blocks,
            deadline,
        } = self
        else {
            return Ok(false);
        };
        if Instant::now() >= deadline {
            return Ok(true);
        }
        let head = derive_store
            .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)?;
        Ok(lag_within(canonical_tip_height, head, max_lag_blocks))
    }

    /// Returns whether the catch-up has drained enough to hand off given a
    /// selected projection head already known from the in-flight replay,
    /// sparing a store read.
    fn handoff_reached_at(self, replayed_through: BlockHeight) -> bool {
        let Self::Handoff {
            canonical_tip_height,
            max_lag_blocks,
            deadline,
        } = self
        else {
            return false;
        };
        Instant::now() >= deadline
            || lag_within(canonical_tip_height, Some(replayed_through), max_lag_blocks)
    }
}

fn lag_within(
    canonical_tip_height: Option<BlockHeight>,
    head: Option<BlockHeight>,
    max_lag_blocks: u64,
) -> bool {
    let Some(tip) = canonical_tip_height else {
        return true;
    };
    let head = head.map_or(0, BlockHeight::value);
    u64::from(tip.value().saturating_sub(head)) <= max_lag_blocks
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

/// Drains derive debt only to the handoff boundary, then returns.
///
/// Replay stops once the derive plane is within the configured handoff lag of
/// the canonical tip or a bounded wall-clock budget elapses. Callers that opt
/// into this explicit handoff can then start the always-on tailer from the
/// persisted consumer cursors. The persisted [`DeriveStatus`] is refreshed
/// before returning so the first readiness read reflects the residual lag as
/// catching-up rather than dark.
pub async fn catch_up_derive_store_to_canonical_until_handoff(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    derive_config: IngestDeriveConfig,
) -> Result<(), IngestError> {
    let mut replay_budget = DeriveReplayBudget::new(derive_config);
    replay_budget.bound = DeriveCatchUpBound::Handoff {
        canonical_tip_height: record_current_derive_replay_tip(chain_store)?,
        max_lag_blocks: derive_config.startup_handoff_lag_blocks,
        deadline: Instant::now() + STARTUP_DERIVE_HANDOFF_BUDGET,
    };
    let outcome = catch_up_derive_store_to_canonical_with_budget(
        chain_store,
        derive_store,
        &mut replay_budget,
    )
    .await;
    persist_derive_status(chain_store, derive_store, replay_budget.applied_state);
    outcome
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
        if replay_budget.is_cancelled() {
            return Ok(());
        }
        let effective_limits = replay_budget.evaluate_current();
        record_derive_replay_budget(
            replay_budget.config.replay_policy,
            effective_limits,
            DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
        );
        if effective_limits.state.is_paused() {
            return Ok(());
        }
        if replay_budget.bound.handoff_reached(derive_store)? {
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
            if replay_budget.is_cancelled() {
                return Ok(());
            }
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
                    if replay_budget.bound.handoff_reached(derive_store)? {
                        return Ok(());
                    }
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

/// Publishes verified contiguous transparent-outpoint-spend coverage as the
/// canonical retention release floor.
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
    let projection_state =
        match derive_store.consumer_projection_state(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME) {
            Ok(projection_state) => projection_state,
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "retention_release_floor_read_failed",
                    error = %error,
                    "failed to read verified transparent-outpoint-spend coverage",
                );
                return;
            }
        };
    let history_bounds = match chain_store.canonical_history_bounds() {
        Ok(history_bounds) => history_bounds,
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "retention_release_floor_history_bounds_failed",
                error = %error,
                "failed to read canonical history bounds before publishing the retention release floor",
            );
            return;
        }
    };
    let (Some(projection_state), Some(history_bounds)) = (projection_state, history_bounds) else {
        return;
    };
    let Some(coverage) = projection_state.coverage else {
        return;
    };
    if coverage.complete_from_height > history_bounds.first_available_height() {
        tracing::warn!(
            target: "zinder::ingest",
            event = "retention_release_floor_incomplete_coverage",
            coverage_from_height = coverage.complete_from_height.value(),
            required_from_height = history_bounds.first_available_height().value(),
            "verified transparent-outpoint-spend coverage starts after canonical history; retention remains held",
        );
        return;
    }
    let durable_height = coverage.complete_through_height;
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
        return replay_empty_committed_event(chain_store, derive_store, envelope)
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
    record_committed_replay_progress(
        chain_store,
        derive_store,
        chain_event_replay_progress_height(&envelope),
    )?;
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

type PreparedReplayBatchHandle = JoinHandle<Result<PreparedReplayBatch, IngestError>>;

fn spawn_prepare_committed_block_replay_batch(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    effective_limits: EffectiveDeriveReplayLimits,
) -> PreparedReplayBatchHandle {
    let chain_store = chain_store.clone();
    let envelope = envelope.clone();
    tokio::spawn(async move {
        let replay_batch = await_hydrated_replay_batch(spawn_hydrate_committed_block_replay_batch(
            &chain_store,
            &envelope,
            start_height,
            end_height,
            effective_limits,
        ))
        .await?;
        let finalized = replay_batch.block_range.end <= envelope.safe_tip_height;
        let contexts = build_contexts_for_replay_batch(
            &chain_store,
            &envelope,
            replay_batch.blocks,
            finalized,
        )
        .await?;
        Ok(PreparedReplayBatch {
            block_range: replay_batch.block_range,
            projection_rows: replay_batch.projection_rows,
            contexts,
        })
    })
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
    let mut pending_replay_batch: Option<PreparedReplayBatchHandle> = None;
    while next_height <= committed_range.end {
        if stop_replay_if(replay_budget.is_cancelled(), &mut pending_replay_batch) {
            return Ok(DeriveReplayProgress::Yielded);
        }
        catch_up_event_only_chain_event_consumers_to_canonical(chain_store, derive_store)?;
        let effective_limits = evaluate_and_record_replay_budget(replay_budget);
        let replay_paused = effective_limits.state.is_paused();
        if stop_replay_if(replay_paused, &mut pending_replay_batch) {
            return Ok(DeriveReplayProgress::Yielded);
        }

        let replay_batch_handle = pending_replay_batch.take().unwrap_or_else(|| {
            spawn_prepare_committed_block_replay_batch(
                chain_store,
                &envelope,
                next_height,
                committed_range.end,
                effective_limits,
            )
        });
        let prepared_batch =
            await_expected_replay_batch(replay_batch_handle, next_height, block_count).await?;

        let replay_range = prepared_batch.block_range;
        let final_chunk = replay_range.end >= committed_range.end;
        let chunk_event = committed_chain_event_chunk(&envelope.event, replay_range);
        let following_height = next_replay_height(replay_range.end)?;
        pending_replay_batch = maybe_spawn_read_ahead_replay_batch(ReadAheadReplayBatchInputs {
            chain_store,
            replay_budget,
            envelope: &envelope,
            projection_rows: prepared_batch.projection_rows,
            following_height,
            committed_end: committed_range.end,
            final_chunk,
            effective_limits,
        });

        if stop_replay_if(replay_budget.is_cancelled(), &mut pending_replay_batch) {
            return Ok(DeriveReplayProgress::Yielded);
        }
        if let Err(error) = dispatch_replay_chunk(
            derive_store,
            &envelope,
            &chunk_event,
            &prepared_batch.contexts,
            final_chunk,
        ) {
            abort_pending_replay_batch(&mut pending_replay_batch);
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }

        next_height = following_height;
        // Hand off mid-event once within the startup handoff bound. The cursor
        // is not advanced until the final chunk, so the tailer re-reads this
        // event and re-applies the already-written chunks idempotently.
        if next_height <= committed_range.end
            && replay_budget.bound.handoff_reached_at(replay_range.end)
        {
            abort_pending_replay_batch(&mut pending_replay_batch);
            return Ok(DeriveReplayProgress::Yielded);
        }
    }

    finish_derive_replay_event(chain_store, derive_store, envelope, block_count)
}

fn dispatch_replay_chunk(
    derive_store: &DeriveStore,
    envelope: &ChainEventEnvelope,
    chunk_event: &ChainEvent,
    contexts: &HashMap<BlockHeight, Arc<BlockCommitContext>>,
    final_chunk: bool,
) -> Result<(), IngestError> {
    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: chunk_event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        safe_tip_height: envelope.safe_tip_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, contexts, final_chunk);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    dispatch_outcome
}

fn next_replay_height(height: BlockHeight) -> Result<BlockHeight, IngestError> {
    height
        .next()
        .ok_or_else(|| IngestError::DeriveDispatch("derive replay height overflow".to_owned()))
}

fn stop_replay_if(
    should_stop: bool,
    pending_replay_batch: &mut Option<PreparedReplayBatchHandle>,
) -> bool {
    if should_stop {
        abort_pending_replay_batch(pending_replay_batch);
    }
    should_stop
}

fn abort_pending_replay_batch(pending_replay_batch: &mut Option<PreparedReplayBatchHandle>) {
    if let Some(handle) = pending_replay_batch.take() {
        handle.abort();
    }
}

async fn await_expected_replay_batch(
    replay_batch_handle: PreparedReplayBatchHandle,
    expected_start: BlockHeight,
    block_count: usize,
) -> Result<PreparedReplayBatch, IngestError> {
    let replay_batch_outcome = match replay_batch_handle.await {
        Ok(outcome) => outcome,
        Err(join_error) => Err(IngestError::BlockingTaskFailed {
            reason: join_error.to_string(),
        }),
    };
    let replay_batch = match replay_batch_outcome {
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
) -> Option<PreparedReplayBatchHandle> {
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
    Some(spawn_prepare_committed_block_replay_batch(
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
    derive_store: &DeriveStore,
    replayed_height: BlockHeight,
) -> Result<(), IngestError> {
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(derive_store, replayed_height, tip_height);
    }
    Ok(())
}

fn finish_derive_replay_event(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    block_count: usize,
) -> Result<DeriveReplayProgress, IngestError> {
    record_derive_replay_event(block_count, None);
    record_committed_replay_progress(
        chain_store,
        derive_store,
        chain_event_replay_progress_height(&envelope),
    )?;
    Ok(DeriveReplayProgress::Advanced(envelope.cursor))
}

async fn replay_reorg_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    replay_budget: &mut DeriveReplayBudget,
) -> Result<DeriveReplayProgress, IngestError> {
    if replay_budget.is_cancelled() {
        return Ok(DeriveReplayProgress::Yielded);
    }
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
    if replay_budget.is_cancelled() {
        return Ok(DeriveReplayProgress::Yielded);
    }

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

    finish_derive_replay_event(chain_store, derive_store, envelope, block_count)
}

fn catch_up_event_only_chain_event_consumers_to_canonical(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
) -> Result<(), IngestError> {
    if derive_store
        .event_only_chain_event_consumer_names()
        .next()
        .is_none()
    {
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
                chain_store,
                derive_store,
                envelope,
            )?);
        }
    }
}

fn replay_event_only_chain_event_to_derive(
    chain_store: &PrimaryChainStore,
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
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_projection_replay_progress(
            derive_store.event_only_chain_event_consumer_names(),
            chain_event_replay_progress_height(&envelope),
            tip_height,
        );
    }
    Ok(envelope.cursor)
}

fn dispatch_event_only_chain_event(
    derive_store: &DeriveStore,
    inputs: ChainEventDispatchInputs<'_>,
) -> Result<(), IngestError> {
    if !derive_store.has_consumer(zinder_derive::REORG_INCIDENTS_CONSUMER_NAME) {
        return Ok(());
    }
    let mut reorg_incidents = ReorgIncidentsConsumer::new();
    let mut block_consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 0] = [];
    let mut event_consumers: [&mut dyn zinder_derive::DeriveConsumer; 1] = [&mut reorg_incidents];
    let blocks = HashMap::<BlockHeight, Arc<BlockCommitContext>>::new();
    let measurements = derive_store
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
    record_projection_write_measurements(&measurements, PROJECTION_WRITE_SOURCE_CHAIN_EVENT, 0);
    Ok(())
}

fn persisted_event_only_chain_event_cursor(
    derive_store: &DeriveStore,
) -> Result<Option<StreamCursorTokenV1>, IngestError> {
    let mut cursor: Option<Vec<u8>> = None;
    for consumer_name in derive_store.event_only_chain_event_consumer_names() {
        let Some(candidate) = derive_store.get_chain_event_cursor(consumer_name)? else {
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
    let ranking_is_active = derive_store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)
        && TransparentAddressRankingConsumer::active_metadata(derive_store)?.is_some();
    for consumer_name in derive_store
        .chain_event_consumer_names()
        .filter(|name| ranking_is_active || *name != TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)
    {
        let Some(candidate) = derive_store.get_chain_event_cursor(consumer_name)? else {
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

/// Returns the canonical position authenticated by a fully consumed event.
///
/// A safe-tip-only commit can carry an empty committed range whose sentinel
/// end is below the visible tip. Derive cursors still advance through the
/// event's complete chain epoch, so replay progress follows that epoch instead
/// of regressing to the range sentinel.
fn chain_event_replay_progress_height(envelope: &ChainEventEnvelope) -> BlockHeight {
    envelope.chain_epoch.visible_tip_height
}

/// One block staged for derive replay before its transaction facts are read.
///
/// Phase 1 of [`hydrate_committed_block_replay_batch`] collects these so the
/// facts read can collapse into one batched store read for the whole replay
/// batch instead of one read per block.
struct StagedReplayBlock {
    height: BlockHeight,
    header: BlockHeaderArtifact,
    final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
    transaction_ids: Vec<TransactionId>,
}

/// Reads each block's header and ordered transaction ids for the replay batch.
fn stage_committed_replay_blocks(
    reader: &zinder_store::ChainEpochReader<'_>,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    max_blocks: usize,
) -> Result<Vec<StagedReplayBlock>, IngestError> {
    let capacity = usize::try_from(
        end_height
            .value()
            .saturating_sub(start_height.value())
            .saturating_add(1),
    )
    .unwrap_or(usize::MAX)
    .min(max_blocks);
    let mut staged = Vec::with_capacity(capacity);
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
        let final_note_commitment_roots = reader.final_note_commitment_roots_at(height)?;
        staged.push(StagedReplayBlock {
            height,
            header,
            final_note_commitment_roots,
            transaction_ids,
        });
        next_height = height.next().ok_or_else(|| {
            IngestError::DeriveDispatch("derive replay height overflow".to_owned())
        })?;
    }
    Ok(staged)
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

    // Phase 1: read each block's header and ordered transaction ids. These
    // artifacts are compact enough to stage up to the configured block bound.
    let staged =
        stage_committed_replay_blocks(&reader, envelope, start_height, end_height, max_blocks)?;

    // Phase 2: read and assemble transaction facts in bounded groups. The
    // projection-row cap cannot be evaluated until facts are decoded, so a
    // separate facts-read bound prevents dense history from hydrating the
    // entire configured replay batch only to discard most of it.
    let mut replay_blocks = Vec::with_capacity(staged.len());
    let mut projection_rows = DeriveReplayProjectionRows::default();
    'facts_groups: for staged_group in bounded_facts_read_groups(&staged) {
        let transaction_ids = staged_group
            .iter()
            .flat_map(|staged_block| staged_block.transaction_ids.iter().copied())
            .collect::<Vec<_>>();
        let headers_by_height = staged_group
            .iter()
            .map(|staged_block| (staged_block.height, staged_block.header.clone()))
            .collect::<HashMap<_, _>>();
        let mut facts_by_id = reader
            .transaction_facts_by_ids_with_known_headers(&transaction_ids, &headers_by_height)?;

        for staged_block in staged_group {
            let mut transactions = Vec::with_capacity(staged_block.transaction_ids.len());
            for transaction_id in &staged_block.transaction_ids {
                let Some(transaction) = facts_by_id.remove(transaction_id).flatten() else {
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
                break 'facts_groups;
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
                final_note_commitment_roots: staged_block.final_note_commitment_roots,
                transparent_spends,
            });
        }
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
    let final_note_commitment_roots = reader.final_note_commitment_roots_at(height)?;
    Ok(CanonicalReplayBlock {
        height,
        block_hash: header.block_hash,
        previous_block_hash: header.parent_hash,
        block_time_unix_seconds: header.block_time,
        block_size_bytes: header.block_size_bytes,
        transactions,
        final_note_commitment_roots,
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
    let _write_guard = derive_projection_write_guard();
    let mut block_production_time = BlockProductionTimeConsumer::new();
    let mut block_summary = BlockSummaryConsumer::new();
    let mut ironwood_migration = IronwoodMigrationConsumer::new();
    let mut commitment_root_search = CommitmentRootSearchConsumer::new();
    let mut conventional_fee_distribution = ConventionalFeeDistributionConsumer::new();
    let mut paid_fee_distribution = PaidFeeDistributionConsumer::new();
    let mut transaction_fees = TransactionFeesConsumer::new();
    let mut recent_transactions = RecentTransactionsConsumer::new();
    let mut transaction_history = TransactionHistoryConsumer::new();
    let mut transaction_component_summary = TransactionComponentSummaryConsumer::new();
    let mut transparent_activity = TransparentAddressActivityConsumer::new();
    let mut transparent_deltas = TransparentAddressDeltasConsumer::new();
    let mut transparent_ranking = TransparentAddressRankingConsumer::new();
    let mut transparent_transaction_history = TransparentAddressTransactionHistoryConsumer::new();
    let mut transparent_outpoint_spend = TransparentOutpointSpendConsumer::new();
    let mut value_pool_flow_history = ValuePoolFlowHistoryConsumer::new();
    let all_consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 15] = [
        &mut block_production_time,
        &mut block_summary,
        &mut ironwood_migration,
        &mut commitment_root_search,
        &mut conventional_fee_distribution,
        &mut paid_fee_distribution,
        &mut transaction_fees,
        &mut recent_transactions,
        &mut transaction_history,
        &mut transaction_component_summary,
        &mut transparent_activity,
        &mut transparent_deltas,
        &mut transparent_transaction_history,
        &mut transparent_outpoint_spend,
        &mut value_pool_flow_history,
    ];
    let mut consumers: Vec<&mut dyn zinder_derive::BlockKeyedConsumer> = all_consumers
        .into_iter()
        .filter(|consumer| derive_store.has_consumer(consumer.name()))
        .collect();
    if derive_store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)
        && TransparentAddressRankingConsumer::active_metadata(derive_store)?.is_some()
    {
        consumers.push(&mut transparent_ranking);
    }
    let measurements = derive_store
        .write_chain_event_chunk(consumers.as_mut_slice(), inputs, blocks, advance_cursor)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    record_projection_write_measurements(
        &measurements,
        PROJECTION_WRITE_SOURCE_CHAIN_EVENT,
        blocks.len(),
    );
    Ok(())
}

/// Dispatches one committed mempool event into ingest-owned derive
/// consumers.
pub(crate) fn dispatch_mempool_event(
    derive_store: &DeriveStore,
    envelope: &MempoolEventEnvelope,
) -> Result<(), IngestError> {
    if !derive_store.has_consumer(MEMPOOL_EVENT_COUNTS_CONSUMER_NAME) {
        return Ok(());
    }
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
    let measurement = derive_store
        .write_mempool_event(&mut event_counts, event, cursor_bytes)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    record_projection_write_measurements(
        std::slice::from_ref(&measurement),
        PROJECTION_WRITE_SOURCE_MEMPOOL_EVENT,
        0,
    );
    Ok(())
}

/// Hydrates one current canonical range for cursor-neutral startup projections.
pub(crate) async fn read_current_block_context_batch(
    chain_store: &PrimaryChainStore,
    start_height: BlockHeight,
    end_height: BlockHeight,
) -> Result<Vec<Arc<BlockCommitContext>>, IngestError> {
    let store = chain_store.clone();
    let (chain_epoch_id, replay_blocks) = tokio::task::spawn_blocking(move || {
        let reader = store.current_chain_epoch_reader()?;
        let chain_epoch_id = reader.chain_epoch().id;
        let mut replay_blocks = Vec::with_capacity(
            BlockHeightRange::inclusive(start_height, end_height)
                .into_iter()
                .len(),
        );
        for height in BlockHeightRange::inclusive(start_height, end_height) {
            let header = reader.block_header_at(height)?.ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "ranking startup references unavailable block-header facts {}",
                    height.value()
                ))
            })?;
            let transaction_ids = reader.transaction_ids_at_height(height)?;
            let mut facts_by_id = reader.transaction_facts_by_ids(&transaction_ids)?;
            let mut transactions = Vec::with_capacity(transaction_ids.len());
            for transaction_id in transaction_ids {
                let transaction =
                    facts_by_id
                        .remove(&transaction_id)
                        .flatten()
                        .ok_or_else(|| {
                            IngestError::DeriveDispatch(format!(
                                "ranking startup references unavailable transaction facts {}",
                                hex::encode(transaction_id.as_bytes())
                            ))
                        })?;
                transactions.push(transaction);
            }
            let transparent_spends = transparent_spent_outpoints_for_transactions(&transactions);
            replay_blocks.push(CanonicalReplayBlock {
                height,
                block_hash: header.block_hash,
                previous_block_hash: header.parent_hash,
                block_time_unix_seconds: header.block_time,
                block_size_bytes: header.block_size_bytes,
                transactions,
                final_note_commitment_roots: reader.final_note_commitment_roots_at(height)?,
                transparent_spends,
            });
        }
        Ok::<_, IngestError>((chain_epoch_id, replay_blocks))
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })??;
    let mut contexts = build_block_contexts_from_committed_event(
        chain_store,
        chain_epoch_id,
        replay_blocks,
        false,
    )
    .await?;
    let mut ordered = Vec::with_capacity(contexts.len());
    for height in BlockHeightRange::inclusive(start_height, end_height) {
        ordered.push(contexts.remove(&height).ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "ranking startup context is missing at height {}",
                height.value()
            ))
        })?);
    }
    Ok(ordered)
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
    let transaction_intrinsic_value_balances =
        read_transaction_intrinsic_value_balances_for_committed_blocks(
            chain_store,
            chain_epoch_id,
            &replay_blocks,
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
                final_note_commitment_roots: block.final_note_commitment_roots,
            },
            TransparentSpendFacts::from_map(Arc::clone(&transparent_spends)),
        )
        .with_transaction_intrinsic_value_balances(
            TransactionIntrinsicValueBalanceFacts::from_map(Arc::clone(
                &transaction_intrinsic_value_balances,
            )),
        );
        out.insert(block.height, Arc::new(context));
    }
    Ok(out)
}

async fn read_transaction_intrinsic_value_balances_for_committed_blocks(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: &[CanonicalReplayBlock],
) -> Result<Arc<HashMap<TransactionId, TransactionIntrinsicValueBalances>>, IngestError> {
    let transaction_ids: Vec<TransactionId> = replay_blocks
        .iter()
        .flat_map(|block| {
            block
                .transactions
                .iter()
                .map(|transaction| transaction.location.transaction_id)
        })
        .collect();
    let chain_store = chain_store.clone();
    tokio::task::spawn_blocking(move || {
        let reader = chain_store.chain_epoch_reader_at(chain_epoch_id)?;
        let mut balances = HashMap::with_capacity(transaction_ids.len());
        for transaction_id in transaction_ids {
            if let Some(artifact) =
                reader.transaction_intrinsic_value_balances_by_id(transaction_id)?
            {
                balances.insert(transaction_id, artifact.value_balances);
            }
        }
        Ok::<_, IngestError>(Arc::new(balances))
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })?
}

struct CanonicalReplayBlock {
    height: BlockHeight,
    block_hash: BlockHash,
    previous_block_hash: BlockHash,
    block_time_unix_seconds: i64,
    block_size_bytes: u64,
    transactions: Vec<TransactionFactsArtifact>,
    final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
    transparent_spends: Vec<TransparentOutPoint>,
}

struct CanonicalReplayBatch {
    block_range: BlockHeightRange,
    blocks: Vec<CanonicalReplayBlock>,
    projection_rows: DeriveReplayProjectionRows,
}

struct PreparedReplayBatch {
    block_range: BlockHeightRange,
    projection_rows: DeriveReplayProjectionRows,
    contexts: HashMap<BlockHeight, Arc<BlockCommitContext>>,
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

/// Resolves a non-finalized batch's transparent spend facts across several
/// blocking readers.
///
/// Outpoints are chain-unique, so chunk result maps have disjoint keys and
/// merge without collision. Reorg-window replay still needs point-row
/// visibility checks; finalized replay uses the block-local record below.
async fn resolve_spend_facts_concurrently(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    outpoints: Vec<TransparentOutPoint>,
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
            reader.transparent_spend_facts_by_outpoints(&chunk)
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

async fn resolve_finalized_spend_facts_by_block(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: &[CanonicalReplayBlock],
) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, IngestError> {
    let requested_by_block = replay_blocks
        .iter()
        .map(|block| {
            (
                block.height,
                block.block_hash,
                block
                    .transparent_spends
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let chain_store = chain_store.clone();
    tokio::task::spawn_blocking(move || {
        let reader = chain_store
            .chain_epoch_reader_at_for(StoreReadCaller::DeriveHydration, chain_epoch_id)?;
        let total_spends = requested_by_block
            .iter()
            .map(|(_, _, outpoints)| outpoints.len())
            .sum();
        let mut resolved = HashMap::with_capacity(total_spends);
        for (height, expected_block_hash, requested_outpoints) in requested_by_block {
            let replay = reader.current_transparent_spend_replay_at_height(height)?;
            for spend in validate_transparent_spend_replay_block(
                height,
                expected_block_hash,
                &requested_outpoints,
                replay,
            )? {
                if spend.block_height != height || spend.block_hash != expected_block_hash {
                    return Err(IngestError::DeriveDispatch(format!(
                        "block-local transparent spend replay fact has the wrong producing block at height {}",
                        height.value(),
                    )));
                }
                if resolved.insert(spend.spent_outpoint, spend).is_some() {
                    return Err(IngestError::DeriveDispatch(format!(
                        "block-local transparent spend replay fact is duplicated at height {}",
                        height.value(),
                    )));
                }
            }
        }
        Ok(resolved)
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
}

fn validate_transparent_spend_replay_block(
    height: BlockHeight,
    expected_block_hash: BlockHash,
    requested_outpoints: &HashSet<TransparentOutPoint>,
    replay: Option<TransparentSpendReplayBlock>,
) -> Result<Vec<TransparentSpendFact>, IngestError> {
    let Some(replay) = replay else {
        if requested_outpoints.is_empty() {
            return Ok(Vec::new());
        }
        return Err(IngestError::DeriveDispatch(format!(
            "block-local transparent spend replay record is missing at height {}",
            height.value(),
        )));
    };
    if replay.block_hash != expected_block_hash {
        return Err(IngestError::DeriveDispatch(format!(
            "block-local transparent spend replay record has the wrong block hash at height {}",
            height.value(),
        )));
    }
    let recorded_input_outpoints = replay
        .input_outpoints
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    if recorded_input_outpoints.len() != replay.input_outpoints.len()
        || &recorded_input_outpoints != requested_outpoints
    {
        return Err(IngestError::DeriveDispatch(format!(
            "block-local transparent spend replay inputs disagree with canonical transactions at height {} (requested {}, recorded {})",
            height.value(),
            requested_outpoints.len(),
            replay.input_outpoints.len(),
        )));
    }
    let resolved_outpoints = replay
        .spend_facts
        .iter()
        .map(|spend| spend.spent_outpoint)
        .collect::<HashSet<_>>();
    if resolved_outpoints.len() != replay.spend_facts.len()
        || !resolved_outpoints.is_subset(requested_outpoints)
    {
        return Err(IngestError::DeriveDispatch(format!(
            "block-local transparent spend replay facts contain duplicate or unknown inputs at height {} (requested {}, resolved {})",
            height.value(),
            requested_outpoints.len(),
            resolved_outpoints.len(),
        )));
    }
    Ok(replay.spend_facts)
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
    let read_outcome = if finalized {
        resolve_finalized_spend_facts_by_block(chain_store, chain_epoch_id, replay_blocks).await
    } else {
        resolve_spend_facts_concurrently(chain_store, chain_epoch_id, outpoints).await
    };
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

fn record_projection_write_measurements(
    measurements: &[ProjectionWriteMeasurement],
    source: &'static str,
    block_count: usize,
) {
    for measurement in measurements {
        let projection = measurement.projection.as_str();
        metrics::counter!(
            "zinder_ingest_projection_write_operations_total",
            "projection" => projection,
            "source" => source
        )
        .increment(measurement.operations);
        metrics::counter!(
            "zinder_ingest_projection_write_bytes_total",
            "projection" => projection,
            "source" => source
        )
        .increment(measurement.logical_bytes);
        metrics::histogram!(
            "zinder_ingest_projection_dispatch_duration_seconds",
            "projection" => projection,
            "source" => source
        )
        .record(measurement.dispatch_duration);
        if source == PROJECTION_WRITE_SOURCE_CHAIN_EVENT {
            metrics::counter!(
                "zinder_ingest_projection_replay_dispatches_total",
                "projection" => projection
            )
            .increment(1);
            metrics::counter!(
                "zinder_ingest_projection_replay_blocks_total",
                "projection" => projection
            )
            .increment(usize_to_u64_saturating(block_count));
        }
    }
}

fn record_derive_replay_progress(
    derive_store: &DeriveStore,
    progress_height: BlockHeight,
    canonical_tip_height: BlockHeight,
) {
    record_derive_replay_status_metrics(
        Some(progress_height.value()),
        Some(canonical_tip_height.value()),
    );
    record_projection_replay_progress(
        derive_store.chain_event_consumer_names(),
        progress_height,
        canonical_tip_height,
    );
}

fn record_projection_replay_progress(
    projections: impl IntoIterator<Item = DeriveConsumerName>,
    progress_height: BlockHeight,
    canonical_tip_height: BlockHeight,
) {
    let replay_lag_blocks = canonical_tip_height
        .value()
        .saturating_sub(progress_height.value());
    for projection in projections {
        let projection = projection.as_str();
        metrics::gauge!(
            "zinder_ingest_projection_replay_height",
            "projection" => projection
        )
        .set(f64::from(progress_height.value()));
        metrics::gauge!(
            "zinder_ingest_projection_replay_lag_blocks",
            "projection" => projection
        )
        .set(f64::from(replay_lag_blocks));
    }
}

fn record_derive_replay_status_metrics(
    indexed_height: Option<u32>,
    canonical_tip_height: Option<u32>,
) {
    let indexed_height = indexed_height.unwrap_or(0);
    let canonical_tip_height = canonical_tip_height.unwrap_or(0);
    metrics::gauge!("zinder_ingest_derive_replay_height").set(f64::from(indexed_height));
    metrics::gauge!("zinder_ingest_derive_replay_tip_height").set(f64::from(canonical_tip_height));
    metrics::gauge!("zinder_ingest_derive_replay_lag_blocks").set(f64::from(
        canonical_tip_height.saturating_sub(indexed_height),
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
    let indexed_height = match derive_store
        .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)
    {
        Ok(indexed_height) => indexed_height.map(BlockHeight::value),
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "derive_status_projection_head_read_failed",
                error = %error,
                "failed to read the shared wallet-correctness projection head",
            );
            return;
        }
    };
    let canonical_tip = record_current_derive_replay_tip(chain_store)
        .ok()
        .flatten()
        .map(BlockHeight::value);
    let lag_blocks = match (canonical_tip, indexed_height) {
        (Some(tip), Some(indexed)) => u64::from(tip.saturating_sub(indexed)),
        (Some(tip), None) => u64::from(tip),
        (None, _) => 0,
    };
    record_derive_replay_status_metrics(indexed_height, canonical_tip);
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

    use zinder_core::{ChainEpoch, ChainTipMetadata, Network, UnixTimestampMillis};

    use super::*;

    fn derive_store() -> Result<(tempfile::TempDir, DeriveStore), IngestError> {
        let tempdir = tempfile::tempdir().map_err(|error| {
            IngestError::DeriveDispatch(format!("create derive test directory: {error}"))
        })?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                sync_writes: false,
                consumers: DeriveStore::bundled_consumers(),
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        Ok((tempdir, store))
    }

    fn wallet_derive_store() -> Result<(tempfile::TempDir, DeriveStore), IngestError> {
        let tempdir = tempfile::tempdir().map_err(|error| {
            IngestError::DeriveDispatch(format!("create derive test directory: {error}"))
        })?;
        let store = DeriveStore::open_with_projection_preset(
            tempdir.path(),
            zinder_derive::ProjectionPreset::Wallet,
            DeriveStoreOptions {
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..DeriveStoreOptions::default()
            },
        )?;
        Ok((tempdir, store))
    }

    #[test]
    fn wallet_preset_ignores_optional_mempool_projection() -> Result<(), IngestError> {
        let (_tempdir, store) = wallet_derive_store()?;
        let envelope = MempoolEventEnvelope {
            cursor: StreamCursorTokenV1::from_bytes(vec![0xA5; 64]),
            event_sequence: 1,
            source_observed_unix_millis: 1,
            event: MempoolEvent::Suppressed {
                transaction_id: TransactionId::from_bytes([0x42; 32]),
            },
        };

        dispatch_mempool_event(&store, &envelope)?;
        Ok(())
    }

    #[test]
    fn wallet_correctness_head_can_open_the_historical_work_gate() -> Result<(), IngestError> {
        let canonical_tempdir = tempfile::tempdir().map_err(|error| {
            IngestError::DeriveDispatch(format!("create canonical test directory: {error}"))
        })?;
        let chain_store = PrimaryChainStore::open(
            canonical_tempdir.path(),
            zinder_store::ChainStoreOptions::for_local_tests(),
        )?;
        let tip_height = BlockHeight::new(10);
        let tip_hash = BlockHash::from_bytes([0x42; 32]);
        chain_store.commit_artifactless_checkpoint(ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            visible_tip_height: tip_height,
            visible_tip_hash: tip_hash,
            settled_tip_height: tip_height,
            settled_tip_hash: tip_hash,
            artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1),
        })?;
        let (_derive_tempdir, derive_store) = wallet_derive_store()?;
        derive_store.put_consumer(
            TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            &zinder_core::wire::encode_height_key_ascending(tip_height),
            &[],
        )?;

        assert!(derive_replay_caught_up(&chain_store, &derive_store)?);
        Ok(())
    }

    #[test]
    fn wallet_preset_skips_optional_startup_cursor_seeding() -> Result<(), IngestError> {
        let canonical_tempdir = tempfile::tempdir().map_err(|error| {
            IngestError::DeriveDispatch(format!("create canonical test directory: {error}"))
        })?;
        let chain_store = PrimaryChainStore::open(
            canonical_tempdir.path(),
            zinder_store::ChainStoreOptions::for_local_tests(),
        )?;
        let (_derive_tempdir, derive_store) = wallet_derive_store()?;
        let cursor = [0xA5; 64];
        for schema in zinder_derive::ProjectionPreset::Wallet.consumer_schemas() {
            derive_store.put_chain_event_cursor(schema.name, &cursor)?;
        }

        assert_eq!(
            super::unanimous_existing_block_consumer_cursor(&derive_store)?,
            Some(cursor.to_vec())
        );
        super::seed_backfill_owned_consumer_cursors(&chain_store, &derive_store)?;
        Ok(())
    }

    #[test]
    fn wallet_preset_reports_live_from_the_shared_spend_projection_head() -> Result<(), IngestError>
    {
        let canonical_tempdir = tempfile::tempdir().map_err(|error| {
            IngestError::DeriveDispatch(format!("create canonical test directory: {error}"))
        })?;
        let chain_store = PrimaryChainStore::open(
            canonical_tempdir.path(),
            zinder_store::ChainStoreOptions::for_local_tests(),
        )?;
        let tip_height = BlockHeight::new(10);
        let tip_hash = BlockHash::from_bytes([0x42; 32]);
        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            visible_tip_height: tip_height,
            visible_tip_hash: tip_hash,
            settled_tip_height: tip_height,
            settled_tip_hash: tip_hash,
            artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1),
        };
        chain_store.commit_artifactless_checkpoint(chain_epoch)?;
        let (_derive_tempdir, derive_store) = wallet_derive_store()?;
        derive_store.put_consumer(
            TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            &zinder_core::wire::encode_height_key_ascending(tip_height),
            &[],
        )?;

        persist_derive_status(&chain_store, &derive_store, DeriveReplayBudgetState::Normal);

        let encoded = derive_store.get_derive_status()?.ok_or_else(|| {
            IngestError::DeriveDispatch("derive status was not persisted".to_owned())
        })?;
        let status = DeriveStatus::decode(encoded.as_slice()).map_err(|error| {
            IngestError::DeriveDispatch(format!("decode derive status: {error}"))
        })?;
        assert_eq!(status.indexed_height, tip_height.value());
        assert_eq!(status.lag_blocks, 0);
        assert_eq!(status.health, DeriveHealth::Live as i32);
        Ok(())
    }

    #[test]
    fn safe_tip_only_event_progress_uses_the_visible_tip() {
        let visible_tip_height = BlockHeight::new(2_588);
        let settled_tip_height = BlockHeight::new(2_488);
        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(2),
            network: Network::ZcashRegtest,
            visible_tip_height,
            visible_tip_hash: BlockHash::from_bytes([0x42; 32]),
            settled_tip_height,
            settled_tip_hash: BlockHash::from_bytes([0x24; 32]),
            artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(2),
        };
        let empty_range = BlockHeightRange::empty_at(settled_tip_height);
        let envelope = ChainEventEnvelope {
            cursor: StreamCursorTokenV1::from_bytes(vec![0xA5; 64]),
            event_sequence: 2,
            chain_epoch,
            safe_tip_height: settled_tip_height,
            event: ChainEvent::ChainCommitted {
                committed: zinder_store::ChainEpochCommitted {
                    chain_epoch,
                    block_range: empty_range,
                },
            },
        };

        assert_eq!(empty_range.end, settled_tip_height);
        assert_eq!(
            chain_event_replay_progress_height(&envelope),
            visible_tip_height
        );
    }

    fn seed_existing_block_consumer_cursors(
        store: &DeriveStore,
        cursor: &[u8],
    ) -> Result<(), IngestError> {
        for consumer_name in DeriveStore::bundled_chain_event_consumer_names()
            .iter()
            .copied()
            .filter(|name| !BACKFILL_OWNED_BLOCK_CONSUMERS.contains(name))
        {
            store.put_chain_event_cursor(consumer_name, cursor)?;
        }
        Ok(())
    }

    fn seed_backfill_owned_consumer_cursors(store: &DeriveStore) -> Result<(), IngestError> {
        let Some(cursor) = unanimous_existing_block_consumer_cursor(store)? else {
            return Ok(());
        };
        let missing_consumers = missing_backfill_consumer_cursors(store, &cursor)?;
        let Some(authoritative_height) =
            store.last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
        else {
            return Ok(());
        };
        let component_cursor_is_missing =
            missing_consumers.contains(&TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME);
        let conventional_fee_cursor_is_missing =
            missing_consumers.contains(&CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME);
        if conventional_fee_cursor_is_missing
            || ConventionalFeeDistributionConsumer::tail_coverage(store)?.is_none()
        {
            let boundary = authoritative_height.next().ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "conventional-fee distribution live-tail boundary height overflow".to_owned(),
                )
            })?;
            ConventionalFeeDistributionConsumer::initialize_tail_boundary(store, boundary)
                .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
        }
        if component_cursor_is_missing
            || TransactionComponentSummaryConsumer::tail_coverage(store)?.is_none()
        {
            let boundary = authoritative_height.next().ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "transaction-component live-tail boundary height overflow".to_owned(),
                )
            })?;
            TransactionComponentSummaryConsumer::initialize_tail_boundary(store, boundary)
                .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
        }
        for consumer_name in missing_consumers {
            store.put_chain_event_cursor(consumer_name, &cursor)?;
        }
        Ok(())
    }

    fn seed_authoritative_projection_height(
        store: &DeriveStore,
        height: BlockHeight,
    ) -> Result<(), IngestError> {
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &zinder_core::wire::encode_height_key_ascending(height),
            b"test-block-summary",
        )?;
        Ok(())
    }

    fn assert_tail_boundary(
        store: &DeriveStore,
        expected_boundary: BlockHeight,
    ) -> Result<(), IngestError> {
        let coverage = TransactionComponentSummaryConsumer::tail_coverage(store)?
            .ok_or_else(|| IngestError::DeriveDispatch("tail coverage missing".to_owned()))?;
        assert_eq!(coverage.boundary_height, expected_boundary);
        assert_eq!(coverage.complete_through_height, None);
        assert_eq!(coverage.complete_through_time_unix_seconds, None);
        Ok(())
    }

    fn assert_conventional_fee_tail_boundary(
        store: &DeriveStore,
        expected_boundary: BlockHeight,
    ) -> Result<(), IngestError> {
        let coverage =
            ConventionalFeeDistributionConsumer::tail_coverage(store)?.ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "conventional-fee distribution tail coverage missing".to_owned(),
                )
            })?;
        assert_eq!(coverage.boundary_height, expected_boundary);
        assert_eq!(coverage.complete_through_height, None);
        assert_eq!(coverage.complete_through_time_unix_seconds, None);
        Ok(())
    }

    #[test]
    fn three_fresh_backfill_consumers_join_unanimous_existing_boundary() -> Result<(), IngestError>
    {
        let (_tempdir, store) = derive_store()?;
        let cursor = [0xA5; 64];
        seed_existing_block_consumer_cursors(&store, &cursor)?;
        seed_authoritative_projection_height(&store, BlockHeight::new(100))?;

        seed_backfill_owned_consumer_cursors(&store)?;

        for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS {
            assert_eq!(
                store.get_chain_event_cursor(consumer_name)?,
                Some(cursor.to_vec())
            );
        }
        assert_conventional_fee_tail_boundary(&store, BlockHeight::new(101))?;
        assert_tail_boundary(&store, BlockHeight::new(101))?;
        Ok(())
    }

    #[test]
    fn startup_tail_begins_after_the_shared_settled_and_authoritative_prefix()
    -> Result<(), IngestError> {
        assert_eq!(
            backfill_consumer_tail_boundary(BlockHeight::new(90), BlockHeight::new(100), "test",)?,
            BlockHeight::new(91)
        );
        assert_eq!(
            backfill_consumer_tail_boundary(BlockHeight::new(110), BlockHeight::new(100), "test",)?,
            BlockHeight::new(101)
        );
        Ok(())
    }

    #[test]
    fn already_seeded_component_repairs_tail_while_fresh_peer_joins() -> Result<(), IngestError> {
        let (_tempdir, store) = derive_store()?;
        let cursor = [0xA5; 64];
        seed_existing_block_consumer_cursors(&store, &cursor)?;
        seed_authoritative_projection_height(&store, BlockHeight::new(100))?;
        store.put_chain_event_cursor(TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME, &cursor)?;

        seed_backfill_owned_consumer_cursors(&store)?;

        assert_eq!(
            store.get_chain_event_cursor(COMMITMENT_ROOT_SEARCH_CONSUMER_NAME)?,
            Some(cursor.to_vec())
        );
        assert_eq!(
            store.get_chain_event_cursor(CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME)?,
            Some(cursor.to_vec())
        );
        assert_eq!(
            store.get_chain_event_cursor(TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME)?,
            Some(cursor.to_vec())
        );
        assert_conventional_fee_tail_boundary(&store, BlockHeight::new(101))?;
        assert_tail_boundary(&store, BlockHeight::new(101))?;
        seed_backfill_owned_consumer_cursors(&store)?;
        assert_conventional_fee_tail_boundary(&store, BlockHeight::new(101))?;
        assert_tail_boundary(&store, BlockHeight::new(101))?;
        Ok(())
    }

    #[test]
    fn backfill_consumers_stay_fresh_when_any_existing_cursor_is_missing() -> Result<(), IngestError>
    {
        let (_tempdir, store) = derive_store()?;
        seed_authoritative_projection_height(&store, BlockHeight::new(100))?;
        let first_existing = DeriveStore::bundled_chain_event_consumer_names()
            .iter()
            .copied()
            .find(|name| !BACKFILL_OWNED_BLOCK_CONSUMERS.contains(name))
            .ok_or_else(|| IngestError::DeriveDispatch("test consumer missing".to_owned()))?;
        store.put_chain_event_cursor(first_existing, &[0xA5; 64])?;

        seed_backfill_owned_consumer_cursors(&store)?;

        for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS {
            assert!(store.get_chain_event_cursor(consumer_name)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn backfill_consumers_stay_fresh_without_authoritative_projection_height()
    -> Result<(), IngestError> {
        let (_tempdir, store) = derive_store()?;
        seed_existing_block_consumer_cursors(&store, &[0xA5; 64])?;

        seed_backfill_owned_consumer_cursors(&store)?;

        for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS {
            assert!(store.get_chain_event_cursor(consumer_name)?.is_none());
        }
        assert!(TransactionComponentSummaryConsumer::tail_coverage(&store)?.is_none());
        assert!(ConventionalFeeDistributionConsumer::tail_coverage(&store)?.is_none());
        Ok(())
    }

    #[test]
    fn backfill_consumer_seeding_rejects_authoritative_height_overflow() -> Result<(), IngestError>
    {
        let (_tempdir, store) = derive_store()?;
        seed_existing_block_consumer_cursors(&store, &[0xA5; 64])?;
        seed_authoritative_projection_height(&store, BlockHeight::new(u32::MAX))?;

        let result = seed_backfill_owned_consumer_cursors(&store);

        assert!(matches!(result, Err(IngestError::DeriveDispatch(_))));
        for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS {
            assert!(store.get_chain_event_cursor(consumer_name)?.is_none());
        }
        assert!(TransactionComponentSummaryConsumer::tail_coverage(&store)?.is_none());
        Ok(())
    }

    #[test]
    fn backfill_consumer_seeding_rejects_disagreeing_existing_boundaries() -> Result<(), IngestError>
    {
        let (_tempdir, store) = derive_store()?;
        seed_authoritative_projection_height(&store, BlockHeight::new(100))?;
        seed_existing_block_consumer_cursors(&store, &[0xA5; 64])?;
        let first_existing = DeriveStore::bundled_chain_event_consumer_names()
            .iter()
            .copied()
            .find(|name| !BACKFILL_OWNED_BLOCK_CONSUMERS.contains(name))
            .ok_or_else(|| IngestError::DeriveDispatch("test consumer missing".to_owned()))?;
        store.put_chain_event_cursor(first_existing, &[0x5A; 64])?;

        let result = seed_backfill_owned_consumer_cursors(&store);

        assert!(matches!(result, Err(IngestError::DeriveDispatch(_))));
        for consumer_name in BACKFILL_OWNED_BLOCK_CONSUMERS {
            assert!(store.get_chain_event_cursor(consumer_name)?.is_none());
        }
        Ok(())
    }

    fn replay_config() -> IngestDeriveConfig {
        IngestDeriveConfig {
            replay_batch_blocks: NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN),
            replay_policy: DeriveReplayPolicy::CanonicalFirst,
            memory_budget_bytes: NonZeroU64::new(1_000),
            memory_degrade_ratio: 0.85,
            memory_pause_ratio: 0.95,
            memory_resume_ratio: 0.75,
            min_replay_batch_blocks: NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN),
            startup_handoff_lag_blocks: 1_000,
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

        let normal = budget.evaluate(memory_snapshot(800), Some(IngestPhase::FollowingTip));
        assert_eq!(normal.state, DeriveReplayBudgetState::Normal);
        assert_eq!(normal.batch_blocks, 100);

        let degraded = budget.evaluate(memory_snapshot(875), Some(IngestPhase::FollowingTip));
        assert_eq!(degraded.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(degraded.batch_blocks, 50);

        let minimum = budget.evaluate(memory_snapshot(925), Some(IngestPhase::FollowingTip));
        assert_eq!(minimum.state, DeriveReplayBudgetState::Degraded);
        assert_eq!(minimum.batch_blocks, 10);

        let paused = budget.evaluate(memory_snapshot(950), Some(IngestPhase::FollowingTip));
        assert_eq!(paused.state, DeriveReplayBudgetState::Paused);
        assert_eq!(paused.batch_blocks, 0);
    }

    #[test]
    fn replay_budget_resumes_paused_replay_as_degraded_work() {
        let mut budget = DeriveReplayBudget::new(replay_config());

        assert_eq!(
            budget
                .evaluate(memory_snapshot(960), Some(IngestPhase::FollowingTip))
                .state,
            DeriveReplayBudgetState::Paused
        );
        assert_eq!(
            budget
                .evaluate(memory_snapshot(900), Some(IngestPhase::FollowingTip))
                .state,
            DeriveReplayBudgetState::Degraded
        );
        assert_eq!(
            budget
                .evaluate(memory_snapshot(800), Some(IngestPhase::FollowingTip))
                .state,
            DeriveReplayBudgetState::Degraded
        );
        assert_eq!(
            budget
                .evaluate(memory_snapshot(700), Some(IngestPhase::FollowingTip))
                .state,
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

        let limits = budget.evaluate(snapshot, Some(IngestPhase::FollowingTip));

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

        let limits = budget.evaluate(snapshot, Some(IngestPhase::FollowingTip));

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
    fn drain_bound_never_hands_off() {
        assert!(!DeriveCatchUpBound::Drain.handoff_reached_at(BlockHeight::new(1)));
    }

    #[test]
    fn handoff_bound_stops_within_lag_threshold() {
        let bound = DeriveCatchUpBound::Handoff {
            canonical_tip_height: Some(BlockHeight::new(1_000)),
            max_lag_blocks: 100,
            deadline: Instant::now() + Duration::from_hours(1),
        };
        // 1000 - 850 = 150 blocks of lag stays above the threshold.
        assert!(!bound.handoff_reached_at(BlockHeight::new(850)));
        // 1000 - 900 = 100 blocks of lag reaches the threshold.
        assert!(bound.handoff_reached_at(BlockHeight::new(900)));
        assert!(bound.handoff_reached_at(BlockHeight::new(950)));
    }

    #[test]
    fn handoff_bound_stops_on_expired_deadline() {
        let bound = DeriveCatchUpBound::Handoff {
            canonical_tip_height: Some(BlockHeight::new(1_000)),
            max_lag_blocks: 0,
            deadline: Instant::now(),
        };
        // Lag is far above the zero threshold, but the deadline has passed.
        assert!(bound.handoff_reached_at(BlockHeight::new(0)));
    }

    #[test]
    fn handoff_bound_stops_when_canonical_tip_unknown() {
        let bound = DeriveCatchUpBound::Handoff {
            canonical_tip_height: None,
            max_lag_blocks: 0,
            deadline: Instant::now() + Duration::from_hours(1),
        };
        assert!(bound.handoff_reached_at(BlockHeight::new(0)));
    }

    #[test]
    fn continuous_bulk_catchup_pauses_replay() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate(memory_snapshot(800), Some(IngestPhase::BulkCatchup));

        assert!(limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Paused);
        assert_eq!(limits.batch_blocks, 0);
    }

    #[test]
    fn unclassified_startup_phase_pauses_replay() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate(memory_snapshot(800), None);

        assert!(limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Paused);
        assert_eq!(limits.batch_blocks, 0);
    }

    #[test]
    fn replay_without_a_phase_gate_is_allowed() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate_current();

        assert!(!limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Normal);
        assert_eq!(limits.batch_blocks, 100);
    }

    #[test]
    fn installed_but_unclassified_phase_gate_pauses_replay() {
        let readiness = Readiness::default();
        let mut budget = DeriveReplayBudget::with_phase_gate(continuous_config(), readiness);

        let limits = budget.evaluate_current();

        assert!(limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Paused);
        assert_eq!(limits.batch_blocks, 0);
    }

    #[test]
    fn awaiting_upstream_phase_pauses_replay() {
        let mut budget = DeriveReplayBudget::new(continuous_config());

        let limits = budget.evaluate(memory_snapshot(800), Some(IngestPhase::AwaitingUpstream));

        assert!(limits.phase_gate_engaged);
        assert_eq!(limits.state, DeriveReplayBudgetState::Paused);
        assert_eq!(limits.batch_blocks, 0);
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

        let phase_paused = budget.evaluate(memory_snapshot(800), Some(IngestPhase::BulkCatchup));
        assert!(phase_paused.phase_gate_engaged);
        assert_eq!(phase_paused.state, DeriveReplayBudgetState::Paused);
        assert_eq!(phase_paused.batch_blocks, 0);

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
        assert_eq!(engaged.state, DeriveReplayBudgetState::Paused);
        assert_eq!(engaged.batch_blocks, 0);

        let disengaged = budget.evaluate(memory_snapshot(800), Some(IngestPhase::FollowingTip));
        assert!(!disengaged.phase_gate_engaged);
        assert_eq!(disengaged.state, DeriveReplayBudgetState::Normal);
        assert_eq!(disengaged.batch_blocks, 100);
    }

    #[test]
    fn phase_gate_survives_readiness_cause_replacement() {
        let readiness = Readiness::default();
        readiness.set_phase(IngestPhase::BulkCatchup);
        let mut budget =
            DeriveReplayBudget::with_phase_gate(continuous_config(), readiness.clone());

        // Bulk catch-up replaces the readiness cause on every committed batch
        // and every upstream-outage backoff; the gate must keep reading
        // BulkCatchup rather than fail open to unthrottled replay.
        readiness.set(zinder_runtime::ReadinessState::syncing(
            Some(1_500_000),
            Some(100),
            Some(1_500_100),
        ));

        let limits = budget.evaluate_current();
        assert!(limits.phase_gate_engaged);
        assert_ne!(limits.state, DeriveReplayBudgetState::Normal);
    }

    #[test]
    fn replay_budget_observes_process_cancellation() {
        let cancel = CancellationToken::new();
        let budget = DeriveReplayBudget::with_phase_gate_and_cancel(
            replay_config(),
            Readiness::default(),
            cancel.clone(),
        );

        assert!(!budget.is_cancelled());
        cancel.cancel();
        assert!(budget.is_cancelled());
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
            transaction_rows: DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK
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
            transaction_rows: DERIVE_REPLAY_MAX_VARIABLE_PROJECTION_ROWS_PER_CHUNK
                .saturating_sub(1),
            transparent_address_transaction_history: 0,
        };
        let next_block_rows = DeriveReplayProjectionRows {
            transaction_rows: 2,
            transparent_address_transaction_history: 0,
        };

        assert!(should_start_new_projection_chunk(
            current_rows,
            next_block_rows,
        ));
    }

    #[test]
    fn facts_reads_never_hydrate_more_than_the_prefetch_bound() {
        let staged_blocks = (0..DERIVE_REPLAY_FACTS_READ_MAX_BLOCKS
            .saturating_mul(2)
            .saturating_add(1))
            .collect::<Vec<_>>();
        let group_lengths = bounded_facts_read_groups(&staged_blocks)
            .map(<[_]>::len)
            .collect::<Vec<_>>();

        assert_eq!(
            group_lengths,
            vec![
                DERIVE_REPLAY_FACTS_READ_MAX_BLOCKS,
                DERIVE_REPLAY_FACTS_READ_MAX_BLOCKS,
                1,
            ]
        );
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
            transaction_rows: DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS,
            transparent_address_transaction_history: 0,
        };
        let dense_rows = DeriveReplayProjectionRows {
            transaction_rows: DERIVE_REPLAY_READ_AHEAD_VARIABLE_PROJECTION_ROWS.saturating_add(1),
            transparent_address_transaction_history: 0,
        };

        assert!(should_read_ahead_derive_replay(normal_limits, small_rows));
        assert!(!should_read_ahead_derive_replay(normal_limits, dense_rows));
        assert!(!should_read_ahead_derive_replay(
            degraded_limits,
            small_rows
        ));
    }

    #[test]
    fn finalized_spend_replay_accepts_explicit_unresolved_checkpoint_parent()
    -> Result<(), IngestError> {
        let height = BlockHeight::new(100);
        let block_hash = BlockHash::from_bytes([10; 32]);
        let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([11; 32]), 1);
        let requested = HashSet::from([outpoint]);
        let replay = TransparentSpendReplayBlock {
            block_hash,
            input_outpoints: vec![outpoint],
            spend_facts: Vec::new(),
        };

        let facts =
            validate_transparent_spend_replay_block(height, block_hash, &requested, Some(replay))?;

        assert!(facts.is_empty());
        Ok(())
    }

    #[test]
    fn finalized_spend_replay_rejects_a_missing_canonical_input() -> Result<(), IngestError> {
        let height = BlockHeight::new(100);
        let block_hash = BlockHash::from_bytes([10; 32]);
        let first = TransparentOutPoint::new(TransactionId::from_bytes([11; 32]), 1);
        let second = TransparentOutPoint::new(TransactionId::from_bytes([12; 32]), 2);
        let requested = HashSet::from([first, second]);
        let replay = TransparentSpendReplayBlock {
            block_hash,
            input_outpoints: vec![first],
            spend_facts: Vec::new(),
        };

        let Err(error) =
            validate_transparent_spend_replay_block(height, block_hash, &requested, Some(replay))
        else {
            return Err(IngestError::DeriveDispatch(
                "truncated input set unexpectedly passed validation".to_owned(),
            ));
        };

        assert!(error.to_string().contains("inputs disagree"));
        Ok(())
    }
}
