//! Derive-plane runtime that hosts explorer and analytics consumers.
//!
//! `zinder-derive` is the fourth Zinder deployable. It runs in a separate
//! process from `zinder-ingest` and `zinder-query`, owns its own `RocksDB`
//! instance, and reads the canonical chain and mempool surfaces from
//! `zinder-query` over gRPC. The derive plane is the home for materialized
//! views, precomputed aggregates, and explorer-shaped reads that should not
//! live in the canonical wallet data plane.
//!
//! See `docs/architecture/derive-plane.md` for the architectural backdrop and
//! `docs/adrs/0013-derive-plane-instantiation-and-transparent-address-balance.md`
//! for the milestone that introduces this crate.

mod consumer;
mod error;
mod grpc;
mod store;

pub use consumer::backfill::{
    BackfillPrepareError, BackfillThenAttachConfig, BackfillThenAttachOutcome, backfill_then_attach,
};
pub use consumer::chain_events::{ChainEventsRunOutcome, run as run_chain_events_subscriber};
pub use consumer::mempool_events::{MempoolEventsRunOutcome, run as run_mempool_events_subscriber};
pub use consumer::{
    ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName, DeriveMempoolConsumer, MempoolConsumerEvent, MempoolConsumerEventVariant,
    RevertedRange, TipAdvancedEvent,
};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use grpc::{
    DERIVE_EXPLORER_READY_CAPABILITY, DERIVE_EXPLORER_TRANSPARENT_BALANCE_CAPABILITY,
    ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
pub use store::{
    DERIVE_SCHEMA_VERSION, DeriveCursorEntry, DeriveStore, DeriveStoreOptions, DeriveStoreTable,
};
