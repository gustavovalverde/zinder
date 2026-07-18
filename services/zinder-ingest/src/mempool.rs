//! Mempool concerns owned by `zinder-ingest`.
//!
//! Groups the in-memory live index, hydration of source observations into
//! canonical [`zinder_core::MempoolEntry`] records, and the orchestrator
//! that drives [`zinder_source::MempoolSource`] events into both the live
//! index and the canonical mempool-event store.

mod entry;
mod index;
mod live_owner;
mod orchestrator;
mod ready_gate;

pub use entry::{MempoolEntryBuildError, build_mempool_entry};
pub(crate) use index::MempoolIndexPreflight;
pub use index::{MempoolApplyOutcome, MempoolIndex, MempoolSnapshotPage};
pub use live_owner::{
    FactFirstMempoolOwner, run_fact_first_mempool_owner, run_fact_first_mempool_retention,
};
pub use orchestrator::{
    MempoolOrchestratorError, MempoolOrchestratorEventOutcome, run_mempool_orchestrator,
};
pub use ready_gate::{MempoolReadyGate, MempoolReadySignal, mempool_ready_channel};
