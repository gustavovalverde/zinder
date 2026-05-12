//! Derive-plane runtime that hosts explorer and analytics consumers.
//!
//! `zinder-derive` is the fourth Zinder deployable. It runs in a separate
//! process from `zinder-ingest` and `zinder-query`, owns its own `RocksDB`
//! instance, and reads the canonical chain and mempool surfaces from
//! `zinder-query` over gRPC. The derive plane is the home for materialized
//! views, precomputed aggregates, and explorer-shaped reads that should not
//! live in the canonical wallet data plane.
//!
//! See `docs/architecture/derive-plane.md` for the architectural backdrop.

mod consumer;
mod error;
mod grpc;
mod store;

pub use consumer::chain_events::{ChainEventsRunOutcome, run as run_chain_events_subscriber};
pub use consumer::mempool_events::{MempoolEventsRunOutcome, run as run_mempool_events_subscriber};
pub use consumer::{
    ChainCommittedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveMempoolConsumer, MempoolConsumerEvent,
    MempoolConsumerEventVariant, RevertedRange,
};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use grpc::{
    DERIVE_EXPLORER_READY_CAPABILITY, DERIVE_EXPLORER_TRANSPARENT_BALANCE_CAPABILITY,
    ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
pub use store::{
    DERIVE_SCHEMA_VERSION, DeriveCursorEntry, DeriveStore, DeriveStoreOptions, DeriveStoreTable,
};
