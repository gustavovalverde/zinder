//! Selection-aware startup orchestration for the derive projection workload.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::NetworkUpgradeActivations;
use zinder_derive::{
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME, COMMITMENT_ROOT_SEARCH_CONSUMER_NAME,
    CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME, DeriveConsumerName, DeriveStore,
    PAID_FEE_DISTRIBUTION_CONSUMER_NAME, ProjectionPreset,
    TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME, TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME, VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME,
    VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
};
use zinder_source::NodeSource;
use zinder_store::PrimaryChainStore;

use crate::{
    CommitmentRootBackfillConfig, CommitmentRootBackfillContext,
    ConventionalFeeDistributionBackfillConfig, ConventionalFeeDistributionBackfillContext,
    HistoricalWorkGate, IngestDeriveConfig, IngestError, PaidFeeDistributionBackfillConfig,
    PaidFeeDistributionBackfillContext, TransactionComponentBackfillConfig,
    TransactionComponentBackfillContext, TransactionHistoryVerifierConfig,
    TransactionHistoryVerifierContext, ValuePoolBalanceBackfillConfig,
    ValuePoolBalanceBackfillContext, ValuePoolFlowBackfillConfig, ValuePoolFlowBackfillContext,
    bootstrap_transparent_address_ranking, seed_backfill_owned_consumer_cursors,
    seed_paid_fee_distribution_cursor_and_tail, seed_value_pool_flow_cursor_and_tail,
    spawn_block_production_time_backfill_task, spawn_commitment_root_backfill_task,
    spawn_conventional_fee_distribution_backfill_task, spawn_derive_tailer_task,
    spawn_paid_fee_distribution_backfill_task, spawn_transaction_component_backfill_task,
    spawn_transaction_history_verifier_task, spawn_value_pool_balance_backfill_task,
    spawn_value_pool_flow_backfill_task,
};

/// One ordered unit of projection-owned startup work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionStartupWork {
    /// Seed the paid-fee live tail before normal replay starts.
    PaidFeeTailSeed,
    /// Seed the value-pool-flow live tail before normal replay starts.
    ValuePoolFlowTailSeed,
    /// Join backfill-owned consumers to the selected chain-event boundary.
    BackfillOwnedCursorSeed,
    /// Replay selected projection consumers to the canonical position.
    Replay,
    /// Build and activate the transparent-address ranking snapshot.
    RankingBootstrap,
    /// Follow new canonical chain events for selected consumers.
    DeriveTailer,
    /// Reconstruct historical block-production-time rows.
    BlockProductionTimeBackfill,
    /// Reconstruct historical commitment-root search rows.
    CommitmentRootBackfill,
    /// Reconstruct historical conventional-fee distribution rows.
    ConventionalFeeDistributionBackfill,
    /// Reconstruct historical paid-fee distribution rows.
    PaidFeeDistributionBackfill,
    /// Reconstruct historical transaction-component summary rows.
    TransactionComponentBackfill,
    /// Verify transaction-history rows against canonical facts.
    TransactionHistoryVerifier,
    /// Reconstruct historical value-pool-flow rows.
    ValuePoolFlowBackfill,
    /// Reconstruct historical value-pool-balance rows.
    ValuePoolBalanceBackfill,
}

const ALL_PROJECTION_STARTUP_WORK: &[ProjectionStartupWork] = &[
    ProjectionStartupWork::PaidFeeTailSeed,
    ProjectionStartupWork::ValuePoolFlowTailSeed,
    ProjectionStartupWork::BackfillOwnedCursorSeed,
    ProjectionStartupWork::Replay,
    ProjectionStartupWork::RankingBootstrap,
    ProjectionStartupWork::DeriveTailer,
    ProjectionStartupWork::BlockProductionTimeBackfill,
    ProjectionStartupWork::CommitmentRootBackfill,
    ProjectionStartupWork::TransactionComponentBackfill,
    ProjectionStartupWork::TransactionHistoryVerifier,
    ProjectionStartupWork::ConventionalFeeDistributionBackfill,
    ProjectionStartupWork::PaidFeeDistributionBackfill,
    ProjectionStartupWork::ValuePoolFlowBackfill,
    ProjectionStartupWork::ValuePoolBalanceBackfill,
];

const BACKFILL_OWNED_CONSUMERS: &[DeriveConsumerName] = &[
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
    COMMITMENT_ROOT_SEARCH_CONSUMER_NAME,
    CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
    PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
    TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME,
    VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
];

impl ProjectionStartupWork {
    /// Returns every projection-owned startup operation in execution order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        ALL_PROJECTION_STARTUP_WORK
    }

    const fn owner(self) -> Option<DeriveConsumerName> {
        match self {
            Self::PaidFeeTailSeed | Self::PaidFeeDistributionBackfill => {
                Some(PAID_FEE_DISTRIBUTION_CONSUMER_NAME)
            }
            Self::ValuePoolFlowTailSeed | Self::ValuePoolFlowBackfill => {
                Some(VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME)
            }
            Self::RankingBootstrap => Some(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME),
            Self::BlockProductionTimeBackfill => Some(BLOCK_PRODUCTION_TIME_CONSUMER_NAME),
            Self::CommitmentRootBackfill => Some(COMMITMENT_ROOT_SEARCH_CONSUMER_NAME),
            Self::ConventionalFeeDistributionBackfill => {
                Some(CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME)
            }
            Self::TransactionComponentBackfill => Some(TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME),
            Self::TransactionHistoryVerifier => Some(TRANSACTION_HISTORY_CONSUMER_NAME),
            Self::ValuePoolBalanceBackfill => Some(VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME),
            Self::BackfillOwnedCursorSeed | Self::Replay | Self::DeriveTailer => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::PaidFeeTailSeed => "paid_fee_tail_seed",
            Self::ValuePoolFlowTailSeed => "value_pool_flow_tail_seed",
            Self::BackfillOwnedCursorSeed => "backfill_owned_cursor_seed",
            Self::Replay => "replay",
            Self::RankingBootstrap => "ranking_bootstrap",
            Self::DeriveTailer => "derive_tailer",
            Self::BlockProductionTimeBackfill => "block_production_time_backfill",
            Self::CommitmentRootBackfill => "commitment_root_backfill",
            Self::ConventionalFeeDistributionBackfill => "conventional_fee_distribution_backfill",
            Self::PaidFeeDistributionBackfill => "paid_fee_distribution_backfill",
            Self::TransactionComponentBackfill => "transaction_component_backfill",
            Self::TransactionHistoryVerifier => "transaction_history_verifier",
            Self::ValuePoolFlowBackfill => "value_pool_flow_backfill",
            Self::ValuePoolBalanceBackfill => "value_pool_balance_backfill",
        }
    }

    const fn join_failure_event(self) -> &'static str {
        match self {
            Self::BlockProductionTimeBackfill => "block_production_time_backfill_join_failed",
            Self::CommitmentRootBackfill => "commitment_root_backfill_join_failed",
            Self::ConventionalFeeDistributionBackfill => {
                "conventional_fee_distribution_backfill_join_failed"
            }
            Self::PaidFeeDistributionBackfill => "paid_fee_distribution_backfill_join_failed",
            Self::TransactionComponentBackfill => "transaction_component_backfill_join_failed",
            Self::TransactionHistoryVerifier => "transaction_history_verifier_join_failed",
            Self::ValuePoolFlowBackfill => "value_pool_flow_backfill_join_failed",
            Self::ValuePoolBalanceBackfill => "value_pool_balance_backfill_join_failed",
            Self::PaidFeeTailSeed
            | Self::ValuePoolFlowTailSeed
            | Self::BackfillOwnedCursorSeed
            | Self::Replay
            | Self::RankingBootstrap
            | Self::DeriveTailer => "projection_task_join_failed",
        }
    }
}

/// Closed projection selection applied to ingest-owned startup work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionStartupPlan {
    preset: ProjectionPreset,
}

impl ProjectionStartupPlan {
    /// Builds the startup plan for one closed projection preset.
    #[must_use]
    pub const fn for_preset(preset: ProjectionPreset) -> Self {
        Self { preset }
    }

    /// Returns the selected preset.
    #[must_use]
    pub const fn preset(self) -> ProjectionPreset {
        self.preset
    }

    /// Returns whether this plan includes one durable projection identity.
    #[must_use]
    pub fn includes(self, identity: DeriveConsumerName) -> bool {
        self.preset
            .consumer_schemas()
            .iter()
            .any(|schema| schema.name == identity)
    }

    /// Iterates selected startup work in execution order.
    pub fn selected_work(self) -> impl Iterator<Item = ProjectionStartupWork> {
        ProjectionStartupWork::all()
            .iter()
            .copied()
            .filter(move |work| self.includes_work(*work))
    }

    /// Validates the canonical-plus-projection storage pair before the derive
    /// store is opened for writes or schema reconciliation.
    pub fn preflight_storage_pair(
        self,
        chain_store: &PrimaryChainStore,
        canonical_path: &Path,
    ) -> Result<(), IngestError> {
        let derive_path = DeriveStore::path_for_canonical(canonical_path);
        let inspection = DeriveStore::inspect_projection_store_at_path(&derive_path, self.preset)?;
        let canonical_epoch = chain_store.current_chain_epoch()?;

        let Some(inspection) = inspection else {
            if canonical_epoch.is_some() {
                return Err(IngestError::ProjectionStoreMissingForCanonical { path: derive_path });
            }
            return Ok(());
        };
        if canonical_epoch.is_none() && inspection.has_projection_data() {
            return Err(IngestError::ProjectionStoreWithoutCanonicalHistory { path: derive_path });
        }
        Ok(())
    }

    fn includes_work(self, work: ProjectionStartupWork) -> bool {
        match work {
            ProjectionStartupWork::BackfillOwnedCursorSeed => BACKFILL_OWNED_CONSUMERS
                .iter()
                .copied()
                .any(|identity| self.includes(identity)),
            ProjectionStartupWork::Replay | ProjectionStartupWork::DeriveTailer => true,
            ProjectionStartupWork::PaidFeeTailSeed
            | ProjectionStartupWork::ValuePoolFlowTailSeed
            | ProjectionStartupWork::RankingBootstrap
            | ProjectionStartupWork::BlockProductionTimeBackfill
            | ProjectionStartupWork::CommitmentRootBackfill
            | ProjectionStartupWork::ConventionalFeeDistributionBackfill
            | ProjectionStartupWork::PaidFeeDistributionBackfill
            | ProjectionStartupWork::TransactionComponentBackfill
            | ProjectionStartupWork::TransactionHistoryVerifier
            | ProjectionStartupWork::ValuePoolFlowBackfill
            | ProjectionStartupWork::ValuePoolBalanceBackfill => {
                work.owner().is_some_and(|identity| self.includes(identity))
            }
        }
    }

    /// Runs selected startup work and returns the projection task runtime.
    pub async fn start(
        self,
        inputs: ProjectionStartupInputs<'_>,
    ) -> Result<ProjectionRuntime, IngestError> {
        self.validate_store_selection(inputs.derive_store)?;
        let started_at = Instant::now();
        let contexts_outcome = self.prepare(&inputs).await;
        self.record_startup_recovery(started_at, contexts_outcome.is_ok());
        let contexts = contexts_outcome?;
        Ok(self.spawn_runtime(&inputs, &contexts))
    }

    fn record_startup_recovery(self, started_at: Instant, succeeded: bool) {
        let status = if succeeded { "ok" } else { "error" };
        let elapsed = started_at.elapsed();
        for schema in self.preset.consumer_schemas() {
            metrics::histogram!(
                "zinder_ingest_projection_startup_recovery_duration_seconds",
                "projection" => schema.name.as_str(),
                "status" => status
            )
            .record(elapsed);
            metrics::counter!(
                "zinder_ingest_projection_startup_recovery_total",
                "projection" => schema.name.as_str(),
                "status" => status
            )
            .increment(1);
        }
    }

    fn validate_store_selection(self, derive_store: &DeriveStore) -> Result<(), IngestError> {
        for schema in DeriveStore::bundled_consumers() {
            if self.includes(schema.name) != derive_store.has_consumer(schema.name) {
                return Err(IngestError::DeriveDispatch(format!(
                    "projection startup plan {} does not match the opened derive store at {}",
                    self.preset.as_str(),
                    schema.name.as_str()
                )));
            }
        }
        Ok(())
    }

    async fn prepare(
        self,
        inputs: &ProjectionStartupInputs<'_>,
    ) -> Result<ProjectionBackfillContexts, IngestError> {
        let chain_store = inputs.chain_store;
        let derive_store = inputs.derive_store;
        let paid_fee_context = PaidFeeDistributionBackfillContext::new(
            inputs.request_timeout,
            Arc::clone(&inputs.activations),
            Arc::clone(&inputs.source),
            chain_store.clone(),
            derive_store.clone(),
        );
        let value_pool_flow_context = ValuePoolFlowBackfillContext::new(
            chain_store.clone(),
            derive_store.clone(),
            paid_fee_context.clone(),
        );
        if self.includes_work(ProjectionStartupWork::PaidFeeTailSeed) {
            seed_paid_fee_distribution_cursor_and_tail(
                inputs.settings.paid_fee_distribution_backfill,
                &paid_fee_context,
            )
            .await?;
        }
        if self.includes_work(ProjectionStartupWork::ValuePoolFlowTailSeed) {
            seed_value_pool_flow_cursor_and_tail(
                inputs.settings.value_pool_flow_backfill,
                &value_pool_flow_context,
            )
            .await?;
        }
        if self.includes_work(ProjectionStartupWork::BackfillOwnedCursorSeed) {
            seed_backfill_owned_consumer_cursors(chain_store, derive_store)?;
        }
        if self.includes_work(ProjectionStartupWork::RankingBootstrap) {
            let _ = bootstrap_transparent_address_ranking(chain_store, derive_store).await?;
        }
        Ok(ProjectionBackfillContexts {
            paid_fee: paid_fee_context,
            value_pool_flow: value_pool_flow_context,
        })
    }

    fn spawn_runtime(
        self,
        inputs: &ProjectionStartupInputs<'_>,
        contexts: &ProjectionBackfillContexts,
    ) -> ProjectionRuntime {
        let derive_tailer = spawn_derive_tailer_task(
            inputs.chain_store.clone(),
            inputs.derive_store.clone(),
            inputs.settings.derive,
            crate::DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
            inputs.historical_work_gate.clone(),
            inputs.cancel.clone(),
        );
        let mut optional_tasks = Vec::new();
        self.spawn_canonical_backfill_tasks(inputs, &mut optional_tasks);
        self.spawn_source_backfill_tasks(inputs, contexts, &mut optional_tasks);
        ProjectionRuntime {
            derive_tailer,
            optional_tasks,
        }
    }

    fn spawn_canonical_backfill_tasks(
        self,
        inputs: &ProjectionStartupInputs<'_>,
        optional_tasks: &mut Vec<ProjectionTask>,
    ) {
        if self.includes_work(ProjectionStartupWork::BlockProductionTimeBackfill) {
            optional_tasks.push(ProjectionTask {
                work: ProjectionStartupWork::BlockProductionTimeBackfill,
                handle: spawn_block_production_time_backfill_task(
                    inputs.derive_store.clone(),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            });
        }
        if self.includes_work(ProjectionStartupWork::CommitmentRootBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::CommitmentRootBackfill,
                spawn_commitment_root_backfill_task(
                    inputs.settings.commitment_root_backfill,
                    CommitmentRootBackfillContext::new(
                        inputs.request_timeout,
                        Arc::clone(&inputs.activations),
                        Arc::clone(&inputs.source),
                        inputs.chain_store.clone(),
                        inputs.derive_store.clone(),
                    ),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
        if self.includes_work(ProjectionStartupWork::TransactionComponentBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::TransactionComponentBackfill,
                spawn_transaction_component_backfill_task(
                    inputs.settings.transaction_component_backfill,
                    TransactionComponentBackfillContext::new(
                        inputs.chain_store.clone(),
                        inputs.derive_store.clone(),
                    ),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
        if self.includes_work(ProjectionStartupWork::TransactionHistoryVerifier) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::TransactionHistoryVerifier,
                spawn_transaction_history_verifier_task(
                    inputs.settings.transaction_history_verifier,
                    TransactionHistoryVerifierContext::new(
                        inputs.chain_store.clone(),
                        inputs.derive_store.clone(),
                    ),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
        if self.includes_work(ProjectionStartupWork::ConventionalFeeDistributionBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::ConventionalFeeDistributionBackfill,
                spawn_conventional_fee_distribution_backfill_task(
                    inputs.settings.conventional_fee_distribution_backfill,
                    ConventionalFeeDistributionBackfillContext::new(
                        inputs.chain_store.clone(),
                        inputs.derive_store.clone(),
                    ),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
    }

    fn spawn_source_backfill_tasks(
        self,
        inputs: &ProjectionStartupInputs<'_>,
        contexts: &ProjectionBackfillContexts,
        optional_tasks: &mut Vec<ProjectionTask>,
    ) {
        if self.includes_work(ProjectionStartupWork::PaidFeeDistributionBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::PaidFeeDistributionBackfill,
                spawn_paid_fee_distribution_backfill_task(
                    inputs.settings.paid_fee_distribution_backfill,
                    contexts.paid_fee.clone(),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
        if self.includes_work(ProjectionStartupWork::ValuePoolFlowBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::ValuePoolFlowBackfill,
                spawn_value_pool_flow_backfill_task(
                    inputs.settings.value_pool_flow_backfill,
                    contexts.value_pool_flow.clone(),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
        if self.includes_work(ProjectionStartupWork::ValuePoolBalanceBackfill) {
            push_optional_task(
                optional_tasks,
                ProjectionStartupWork::ValuePoolBalanceBackfill,
                spawn_value_pool_balance_backfill_task(
                    inputs.settings.value_pool_balance_backfill,
                    ValuePoolBalanceBackfillContext::new(
                        inputs.request_timeout,
                        Arc::clone(&inputs.source),
                        inputs.chain_store.clone(),
                        inputs.derive_store.clone(),
                    ),
                    inputs.historical_work_gate.clone(),
                    inputs.cancel.clone(),
                ),
            );
        }
    }
}

struct ProjectionBackfillContexts {
    paid_fee: PaidFeeDistributionBackfillContext,
    value_pool_flow: ValuePoolFlowBackfillContext,
}

/// Projection startup controls resolved from ingest configuration.
#[derive(Clone, Copy, Debug)]
pub struct ProjectionStartupSettings {
    /// Chain-event replay controls.
    pub derive: IngestDeriveConfig,
    /// Commitment-root backfill controls.
    pub commitment_root_backfill: CommitmentRootBackfillConfig,
    /// Conventional-fee distribution backfill controls.
    pub conventional_fee_distribution_backfill: ConventionalFeeDistributionBackfillConfig,
    /// Paid-fee distribution seed and backfill controls.
    pub paid_fee_distribution_backfill: PaidFeeDistributionBackfillConfig,
    /// Transaction-component backfill controls.
    pub transaction_component_backfill: TransactionComponentBackfillConfig,
    /// Transaction-history verifier controls.
    pub transaction_history_verifier: TransactionHistoryVerifierConfig,
    /// Value-pool-flow seed and backfill controls.
    pub value_pool_flow_backfill: ValuePoolFlowBackfillConfig,
    /// Value-pool-balance backfill controls.
    pub value_pool_balance_backfill: ValuePoolBalanceBackfillConfig,
}

/// Existing process dependencies consumed during selected projection startup.
pub struct ProjectionStartupInputs<'a> {
    /// Resolved projection startup controls.
    pub settings: ProjectionStartupSettings,
    /// Maximum duration of one upstream source request.
    pub request_timeout: Duration,
    /// Network-upgrade activation table discovered from the source.
    pub activations: Arc<NetworkUpgradeActivations>,
    /// Shared upstream source used by selected historical jobs.
    pub source: Arc<dyn NodeSource>,
    /// Canonical store that owns chain facts and retention.
    pub chain_store: &'a PrimaryChainStore,
    /// Selected derive store opened with the same preset as the plan.
    pub derive_store: &'a DeriveStore,
    /// Shared exclusive-stage gate used by replay, backfills, and retention.
    pub historical_work_gate: &'a HistoricalWorkGate,
    /// Process cancellation token inherited by projection tasks.
    pub cancel: &'a CancellationToken,
}

struct ProjectionTask {
    work: ProjectionStartupWork,
    handle: JoinHandle<()>,
}

/// Running projection tailer plus selected optional background workers.
pub struct ProjectionRuntime {
    derive_tailer: JoinHandle<()>,
    optional_tasks: Vec<ProjectionTask>,
}

impl ProjectionRuntime {
    /// Returns the names of optional tasks that were actually spawned.
    #[must_use]
    pub fn optional_task_names(&self) -> Vec<&'static str> {
        self.optional_tasks
            .iter()
            .map(|task| task.work.as_str())
            .collect()
    }

    /// Joins the projection tasks after their shared token has been cancelled.
    pub async fn join(self) {
        if let Err(join_error) = self.derive_tailer.await {
            tracing::warn!(
                target: "zinder::ingest",
                event = "derive_tailer_join_failed",
                error = %join_error,
                "derive tailer task failed during shutdown"
            );
        }
        for task in self.optional_tasks {
            if let Err(join_error) = task.handle.await {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = task.work.join_failure_event(),
                    projection_work = task.work.as_str(),
                    error = %join_error,
                    "projection background task failed during shutdown"
                );
            }
        }
    }
}

fn push_optional_task(
    tasks: &mut Vec<ProjectionTask>,
    work: ProjectionStartupWork,
    handle: Option<JoinHandle<()>>,
) {
    if let Some(handle) = handle {
        tasks.push(ProjectionTask { work, handle });
    }
}
