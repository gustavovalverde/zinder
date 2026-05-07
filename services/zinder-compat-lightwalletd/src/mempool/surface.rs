//! Mempool read surface consumed by the lightwalletd compatibility adapter.
//!
//! The compatibility shim does not own mempool state. In production, the
//! ingest writer holds the live `MempoolIndex` and `MempoolEventLog` and
//! exposes them through the private `IngestControl` gRPC. The compat shim
//! reaches mempool data through the same public `WalletQuery` surface that
//! native consumers use, encapsulated by this trait so the shim can be
//! tested without standing up a full ingest deployment.
//!
//! The companion [`TipChangeWatcher`] trait carries best-chain tip
//! transitions into the compat layer so [`GetMempoolStream`][gms] can close
//! on tip change while the native `WalletQuery.MempoolEvents` keeps
//! serving live events; that split lets Zinder honor the lightwalletd Go
//! contract without bleeding tip semantics into the native event log.
//!
//! [gms]: zinder_proto::compat::lightwalletd::compact_tx_streamer_server::CompactTxStreamer::get_mempool_stream

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio_stream::Stream;
use zinder_core::MempoolEntry;
use zinder_store::{MempoolEventEnvelope, StreamCursorTokenV1};

/// Stream of mempool event envelopes returned by [`MempoolSurface::mempool_events`].
pub type MempoolEventEnvelopeStream =
    Pin<Box<dyn Stream<Item = Result<MempoolEventEnvelope, MempoolSurfaceError>> + Send + 'static>>;

/// Page of mempool snapshot entries returned by [`MempoolSurface`].
#[derive(Clone, Debug)]
pub struct MempoolSnapshotPage {
    /// Monotonic event sequence observed when the snapshot page was built.
    pub snapshot_sequence: u64,
    /// Hydrated mempool entries in this page.
    pub entries: Vec<MempoolEntry>,
    /// Opaque cursor for the next page, when more entries remain.
    pub next_cursor: Option<Vec<u8>>,
}

/// Error vocabulary for [`MempoolSurface`] consumers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MempoolSurfaceError {
    /// Underlying mempool source is unavailable.
    #[error("mempool surface unavailable: {reason}")]
    Unavailable {
        /// Stable diagnostic reason.
        reason: String,
    },
    /// Cursor is malformed or carries a wrong stream family.
    #[error("mempool cursor is invalid")]
    CursorInvalid,
    /// Cursor sequence is below the oldest retained event.
    #[error("mempool cursor expired")]
    CursorExpired,
}

/// Read-only mempool surface backing the lightwalletd compatibility adapter.
///
/// Implementations are typically thin wrappers over a typed
/// `zinder_client::ChainIndex` (production) or an in-process
/// `MempoolIndex` + `MempoolEventLog` pair (tests).
#[async_trait]
pub trait MempoolSurface: Send + Sync + 'static {
    /// Returns a bounded page of the live mempool snapshot.
    async fn mempool_snapshot_page(
        &self,
        max_entries: u32,
        from_cursor: Option<Vec<u8>>,
    ) -> Result<MempoolSnapshotPage, MempoolSurfaceError>;

    /// Opens a streaming subscription over mempool events strictly after
    /// `from_cursor` (or from the earliest retained event when None).
    async fn mempool_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
    ) -> Result<MempoolEventEnvelopeStream, MempoolSurfaceError>;
}

/// Convenience wrapper for a shared [`MempoolSurface`] handle.
pub type SharedMempoolSurface = Arc<dyn MempoolSurface>;

/// Awaits the next best-chain tip change observed by the writer.
///
/// `GetMempoolStream` (lightwalletd) closes cleanly when the upstream tip
/// advances; native consumers use cursors through `WalletQuery.MempoolEvents`
/// instead and never see this signal.
#[async_trait]
pub trait TipChangeWatcher: Send + Sync + 'static {
    /// Resolves on the next tip change observed after this watcher was
    /// constructed.
    ///
    /// Implementations are typically wrappers over a
    /// [`tokio::sync::watch::Receiver`] tracking the writer's current
    /// chain-event sequence. Returns `Ok(())` on a successful tip change,
    /// or [`TipChangeWatcherError`] if the underlying signal source
    /// disappears (e.g. the writer shut down).
    async fn await_tip_change(&self) -> Result<(), TipChangeWatcherError>;
}

/// Errors surfaced while awaiting a tip change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TipChangeWatcherError {
    /// The underlying tip-change signal source closed before a tip change
    /// was observed.
    #[error("tip-change signal source closed")]
    SignalClosed,
}

/// Convenience wrapper for a shared [`TipChangeWatcher`] handle.
pub type SharedTipChangeWatcher = Arc<dyn TipChangeWatcher>;
