//! Ingestion artifact builders and bulk catchup operations for Zinder.
//!
//! This crate owns deterministic conversion from upstream node source values into
//! canonical artifacts. Node I/O belongs to `zinder-source`; durable writes
//! belong to `zinder-store`.

mod artifact_builder;
mod bulk_catchup;
mod chain_ingest;
mod derive_consumers;
mod ingest_control;
mod ingest_loop;
mod memory_pressure;
mod mempool;
mod phase;
mod retention;
mod source_recovery;
mod tip_follow;
mod upstream_health_probe;

pub use artifact_builder::{
    ArtifactDeriveError, BlockMismatchField, CommitmentTreeSizes, DerivedBlockArtifacts,
    RawBlobPolicy, derive_block, derive_block_with_raw_blob_policy, finalize_derived_block,
};
pub use bulk_catchup::{
    BulkCatchupRunConfig, run_bulk_catchup, run_bulk_catchup_until_complete,
    run_bulk_catchup_with_store,
};
pub use chain_ingest::{BuiltArtifacts, IngestError, NodeSourceKind};
pub use derive_consumers::{
    DEFAULT_DERIVE_TAILER_POLL_INTERVAL, catch_up_derive_store_to_canonical,
    open_primary_derive_store_for_canonical, spawn_derive_replay_budget_metrics_task,
    spawn_derive_tailer_task,
};
pub use ingest_control::{IngestControlGrpcAdapter, MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE};
pub use ingest_loop::{
    BulkCatchupConfig, DeriveReplayPolicy, IngestDeriveConfig, IngestLoopConfig, IngestModifiers,
    PhasesConfig, TipFollowPhaseConfig, TipFollowSubsystems, TipFollowSubsystemsLauncher,
    run_ingest_loop,
};
pub use memory_pressure::{
    DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL, spawn_runtime_memory_metrics_task,
};
pub use mempool::{
    MempoolApplyOutcome, MempoolEntryBuildError, MempoolIndex, MempoolOrchestratorError,
    MempoolOrchestratorEventOutcome, MempoolReadyGate, MempoolReadySignal, MempoolSnapshotPage,
    build_mempool_entry, mempool_ready_channel, run_mempool_orchestrator,
};
pub use phase::{classify_phase, current_chain_height};
pub use retention::{
    ChainEventRetentionConfig, MempoolEventRetentionWorkerConfig, spawn_chain_event_retention_task,
    spawn_mempool_event_retention_task,
};
pub use tip_follow::{
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, TipFollowConfig, open_tip_follow_store, tip_follow,
    tip_follow_with_primary_store,
};
pub use upstream_health_probe::spawn_upstream_health_probe_task;
