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
//! `docs/specs/m5-transparent-address-balance.md` for the milestone that
//! introduces this crate.

mod consumer;
mod error;
mod grpc;
mod store;

pub use consumer::{DeriveConsumer, DeriveConsumerName};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use grpc::{
    DERIVE_EXPLORER_READY_CAPABILITY, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
pub use store::{
    DERIVE_SCHEMA_VERSION, DeriveCursorEntry, DeriveStore, DeriveStoreOptions, DeriveStoreTable,
};
