//! Ingestion artifact builders and bulk catchup operations for Zinder.
//!
//! This crate owns deterministic conversion from upstream node source values into
//! canonical artifacts. Node I/O belongs to `zinder-source`; durable writes
//! belong to `zinder-store`.

mod artifact_builder;
pub mod bench_support;
mod block_production_time_backfill;
mod bulk_catchup;
mod canonical_construction;
mod chain_ingest;
mod commitment_root_backfill;
mod conventional_fee_distribution_backfill;
mod derive_consumers;
mod derive_status_reader;
mod ingest_control;
mod ingest_loop;
mod memory_pressure;
mod mempool;
mod paid_fee_distribution_backfill;
mod phase;
mod projection_startup;
mod retention;
mod source_recovery;
mod tip_follow;
mod transaction_component_backfill;
mod transaction_history_verifier;
mod transparent_address_ranking_snapshot;
mod upstream_health_probe;
mod value_pool_balance_backfill;
mod value_pool_flow_backfill;

pub use artifact_builder::{
    BlockMismatchField, CanonicalBlockConstructionError, CommitmentTreeSizes,
    PositionedCanonicalBlock, PreparedCanonicalBlock, RawBlobPolicy, RetainedRawBlobs,
    position_canonical_block, prepare_canonical_block,
};
pub use block_production_time_backfill::spawn_block_production_time_backfill_task;
pub use bulk_catchup::{
    BulkCatchupRunConfig, run_bulk_catchup, run_bulk_catchup_until_complete,
    run_bulk_catchup_with_store,
};
pub use canonical_construction::{
    CanonicalConstructionConfig, CanonicalConstructionError, load_fresh_canonical_block_replay,
};
pub use chain_ingest::{
    DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
    DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE, IngestError, NodeSourceKind,
};
pub use commitment_root_backfill::{
    CommitmentRootBackfillConfig, CommitmentRootBackfillContext,
    spawn_commitment_root_backfill_task,
};
pub use conventional_fee_distribution_backfill::{
    ConventionalFeeDistributionBackfillConfig, ConventionalFeeDistributionBackfillContext,
    spawn_conventional_fee_distribution_backfill_task,
};
pub use derive_consumers::{
    DEFAULT_DERIVE_TAILER_POLL_INTERVAL, catch_up_derive_store_to_canonical,
    catch_up_derive_store_to_canonical_until_handoff, open_primary_derive_store_for_canonical,
    open_primary_derive_store_for_canonical_with_projection_preset,
    seed_backfill_owned_consumer_cursors, seed_commitment_root_search_cursor_for_backfill,
    spawn_derive_replay_budget_metrics_task, spawn_derive_tailer_task,
};
pub use derive_status_reader::{
    DeriveStatusReadError, DeriveStatusReader, RocksDbDeriveStatusReader,
};
pub use ingest_control::{IngestControlGrpcAdapter, MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE};
pub use ingest_loop::{
    BulkCatchupConfig, DeriveReplayPolicy, HistoricalWorkGate, IngestDeriveConfig,
    IngestLoopConfig, IngestModifiers, PhasesConfig, TipFollowPhaseConfig, TipFollowSubsystems,
    TipFollowSubsystemsLauncher, run_ingest_loop,
};
pub use memory_pressure::{
    DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL, spawn_runtime_memory_metrics_task,
};
pub use mempool::{
    MempoolApplyOutcome, MempoolEntryBuildError, MempoolIndex, MempoolOrchestratorError,
    MempoolOrchestratorEventOutcome, MempoolReadyGate, MempoolReadySignal, MempoolSnapshotPage,
    build_mempool_entry, mempool_ready_channel, run_mempool_orchestrator,
};
pub use paid_fee_distribution_backfill::{
    PaidFeeDistributionBackfillConfig, PaidFeeDistributionBackfillContext,
    seed_paid_fee_distribution_cursor_and_tail, spawn_paid_fee_distribution_backfill_task,
};
pub use phase::{classify_phase, current_chain_height};
pub use projection_startup::{
    ProjectionRuntime, ProjectionStartupInputs, ProjectionStartupPlan, ProjectionStartupSettings,
    ProjectionStartupWork,
};
pub use retention::{
    ChainEventRetentionConfig, MempoolEventRetentionWorkerConfig, spawn_chain_event_retention_task,
    spawn_mempool_event_retention_task, spawn_transparent_retention_task,
};
pub use tip_follow::{
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, TipFollowConfig, open_tip_follow_store, tip_follow,
    tip_follow_with_primary_store,
};
pub use transaction_component_backfill::{
    TransactionComponentBackfillConfig, TransactionComponentBackfillContext,
    spawn_transaction_component_backfill_task,
};
pub use transaction_history_verifier::{
    TransactionHistoryVerifierConfig, TransactionHistoryVerifierContext,
    spawn_transaction_history_verifier_task,
};
pub use transparent_address_ranking_snapshot::{
    TransparentAddressRankingBootstrapOutcome, bootstrap_transparent_address_ranking,
};
pub use upstream_health_probe::spawn_upstream_health_probe_task;
pub use value_pool_balance_backfill::{
    ValuePoolBalanceBackfillConfig, ValuePoolBalanceBackfillContext,
    spawn_value_pool_balance_backfill_task,
};
pub use value_pool_flow_backfill::{
    ValuePoolFlowBackfillConfig, ValuePoolFlowBackfillContext,
    seed_value_pool_flow_cursor_and_tail, spawn_value_pool_flow_backfill_task,
};
pub use zinder_runtime::container_memory_budget_bytes;
