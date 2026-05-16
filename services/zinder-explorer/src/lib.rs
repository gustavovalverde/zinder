//! Explorer-plane runtime serving block-explorer, dashboard, and analytics reads.
//!
//! `zinder-explorer` is the fourth Zinder deployable. It runs in a separate
//! process from `zinder-ingest` and `zinder-query`, owns its own `RocksDB`
//! instance (per the `DeriveStore` pattern reused below), and reads the
//! canonical chain and mempool surfaces from `zinder-query` over gRPC. The
//! explorer plane is the home for materialized views, precomputed aggregates,
//! and explorer-shaped reads that should not live in the canonical wallet
//! data plane.
//!
//! See `docs/architecture/explorer-plane.md` for the product surface and
//! `docs/architecture/derive-plane.md` for the reusable SDK pattern this
//! crate exercises. The SDK abstractions (`DeriveStore`, `DeriveConsumer`,
//! `DeriveProxy`, `run_chain_events_subscriber`, etc.) keep their pattern
//! names so a future second consumer can link the same crate without renames.

mod consumer;
mod error;
mod grpc;
mod store;

pub use consumer::block_summary::{
    BLOCK_SUMMARY_CAPABILITIES, BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME,
    BlockSummaryConsumer, BlockSummaryConsumerError, decode_stored_record,
};
pub use consumer::chain_events::{ChainEventsRunOutcome, run as run_chain_events_subscriber};
pub use consumer::mempool_events::{MempoolEventsRunOutcome, run as run_mempool_events_subscriber};
pub use consumer::{
    ChainCommittedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveMempoolConsumer, MempoolConsumerEvent,
    MempoolConsumerEventVariant, RevertedRange,
};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use grpc::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
pub use store::{
    ConsumerEntry, DERIVE_SCHEMA_VERSION, DeriveCursorEntry, DeriveStore, DeriveStoreOptions,
    DeriveStoreTable,
};
