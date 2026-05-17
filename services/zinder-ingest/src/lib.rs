//! Ingestion artifact builders and backfill operations for Zinder.
//!
//! This crate owns deterministic conversion from upstream node source values into
//! canonical artifacts. Node I/O belongs to `zinder-source`; durable writes
//! belong to `zinder-store`.

mod artifact_builder;
mod backfill;
mod chain_ingest;
mod ingest_control;
mod ingest_loop;
mod mempool;
mod phase;
mod retention;
mod source_recovery;
mod tip_follow;
mod upstream_health_probe;

pub use artifact_builder::{
    ArtifactDeriveError, BlockMismatchField, derive_block_artifact, derive_compact_block_artifact,
    derive_transaction_artifacts,
};
pub use backfill::{BackfillConfig, backfill, backfill_until_complete, backfill_with_store};
pub use chain_ingest::{IngestError, NodeSourceKind};
pub use ingest_control::{IngestControlGrpcAdapter, MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE};
pub use ingest_loop::{
    BulkCatchupConfig, IngestLoopConfig, IngestModifiers, PhasesConfig, TipFollowPhaseConfig,
    TipFollowSubsystems, TipFollowSubsystemsLauncher, run_ingest_loop,
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
