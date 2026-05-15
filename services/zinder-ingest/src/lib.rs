//! Ingestion artifact builders and backfill operations for Zinder.
//!
//! This crate owns deterministic conversion from upstream node source values into
//! canonical artifacts. Node I/O belongs to `zinder-source`; durable writes
//! belong to `zinder-store`.

mod artifact_builder;
mod backfill;
mod chain_ingest;
mod ingest_control;
mod mempool;
mod retention;
mod tip_follow;

pub use artifact_builder::{
    ArtifactDeriveError, BlockMismatchField, derive_block_artifact, derive_compact_block_artifact,
    derive_transaction_artifacts,
};
pub use backfill::{
    BackfillConfig, BackfillOutcome, backfill, backfill_until_complete, backfill_with_store,
};
pub use chain_ingest::{IngestError, NodeSourceKind};
pub use ingest_control::{IngestControlGrpcAdapter, MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE};
pub use mempool::{
    MempoolApplyOutcome, MempoolEntryBuildError, MempoolIndex, MempoolOrchestratorError,
    MempoolOrchestratorEventOutcome, MempoolReadyGate, MempoolReadySignal, MempoolSnapshotPage,
    build_mempool_entry, mempool_ready_channel, run_mempool_orchestrator,
};
pub use retention::{
    ChainEventRetentionConfig, MempoolEventRetentionWorkerConfig, spawn_chain_event_retention_task,
    spawn_mempool_event_retention_task,
};
pub use tip_follow::{
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, TipFollowConfig, open_tip_follow_store, tip_follow,
    tip_follow_with_primary_store,
};
