//! Ingestion artifact builders and backfill operations for Zinder.
//!
//! This crate owns deterministic conversion from upstream node source values into
//! canonical artifacts. Node I/O belongs to `zinder-source`; durable writes
//! belong to `zinder-store`.

mod artifact_builder;
mod backfill;
mod chain_ingest;
mod ingest_control;
mod retention;
mod tip_follow;

pub use artifact_builder::{
    ArtifactDeriveError, BlockMismatchField, derive_block_artifact, derive_compact_block_artifact,
    derive_transaction_artifacts,
};
pub use backfill::{BackfillConfig, BackfillOutcome, backfill};
pub use chain_ingest::{IngestError, NodeSourceKind};
pub use ingest_control::IngestControlGrpcAdapter;
pub use retention::{ChainEventRetentionConfig, spawn_chain_event_retention_task};
pub use tip_follow::{
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, TipFollowConfig, open_tip_follow_store, tip_follow,
    tip_follow_with_primary_store,
};
