//! Mempool source boundary values.
//!
//! The source layer normalizes upstream node mempool observations into typed
//! [`MempoolSourceEvent`] envelopes. Adapters hydrate `ADDED` observations
//! into raw transaction bytes before yielding them; the spec forbids the
//! lightwalletd compatibility shim from compensating for missing hydration.
//!
//! `MempoolSourceEntry` is the partial record the source layer can produce.
//! Ingest finalizes it into the public [`zinder_core::MempoolEntry`] when it
//! stamps the chain epoch visible at observation time and computes the
//! compact-transaction bytes from the raw payload.

use std::pin::Pin;

use async_trait::async_trait;
use tokio_stream::Stream;
use zinder_core::{
    AuthDigest, BlockHash, BlockHeight, MempoolEvictionReason, RawTransactionBytes, TransactionId,
    UnixTimestampMillis,
};

use crate::SourceError;

/// Source-observed mempool transition.
///
/// Variants correspond directly to Zebra's `MempoolChange::ChangeType`. A
/// polling backend that observes a txid disappear without a chain commit
/// emits [`MempoolSourceEvent::Invalidated`] with
/// [`MempoolEvictionReason::Unknown`]; the streaming backend always emits
/// `Unknown` because Zebra's `MempoolChange` does not carry a reason on the
/// wire.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolSourceEvent {
    /// Control-plane marker emitted once the source has completed its
    /// initial snapshot for this stream generation.
    ///
    /// This is not a mempool lifecycle transition and must never be
    /// persisted in the canonical mempool-event log. Consumers use it as the
    /// only proof that an empty or partially populated in-memory index may be
    /// exposed to callers. Streaming sources open the change stream before
    /// taking the snapshot and emit this marker only after replaying the
    /// snapshot and the buffered change-stream prefix. Polling sources emit
    /// it after their first fully successful poll.
    InitialSnapshotComplete,
    /// New transaction admitted to the upstream mempool, with hydrated raw
    /// bytes.
    Added(MempoolSourceEntry),
    /// Mempool transaction was removed without being mined.
    Invalidated {
        /// Identifier of the invalidated transaction.
        transaction_id: TransactionId,
        /// Source-classified eviction reason.
        reason: MempoolEvictionReason,
    },
    /// Mempool transaction was mined into the best chain at the upstream
    /// node.
    Mined {
        /// Identifier of the mined transaction.
        transaction_id: TransactionId,
        /// Height at which the source observed the mining.
        mined_height: BlockHeight,
        /// Hash of the block that mined the transaction, as observed by the
        /// source. Authoritative observation: avoids the canonical-chain
        /// catch-up race when consumers want to track lifecycle without a
        /// follow-up tip read.
        block_hash: BlockHash,
    },
}

/// Partial mempool entry produced by the source layer.
///
/// The source captures everything it can produce locally: the txid, the
/// authorization digest (when the source provides one), the hydrated raw
/// transaction bytes, and the wall-clock observation timestamp. Ingest
/// completes the record into a [`zinder_core::MempoolEntry`] by stamping
/// the visible chain epoch, parsing transparent overlays from the raw
/// bytes, and building compact-transaction bytes for the lightwalletd
/// compatibility adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MempoolSourceEntry {
    /// Transaction identifier reported by the source.
    pub transaction_id: TransactionId,
    /// ZIP-244 authorization digest. Set for v5+ transactions; `None` for
    /// v1-v4.
    pub auth_digest: Option<AuthDigest>,
    /// Raw serialized transaction bytes hydrated from the source.
    pub raw_transaction_bytes: RawTransactionBytes,
    /// Wall-clock time when the adapter observed the source change.
    pub observed_at_unix_millis: UnixTimestampMillis,
}

/// Backend powering a [`MempoolSource`] adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolSourceBackend {
    /// Source consumes the upstream node's gRPC mempool change stream.
    ///
    /// Emits typed change events with low latency. Hydration of `Added`
    /// observations still requires a JSON-RPC follow-up because Zebra's
    /// `MempoolChange` carries only the txid and authorization digest.
    Streaming,
    /// Source diffs the upstream node's mempool state against its previous
    /// snapshot.
    ///
    /// Yields the same change variants as the streaming backend, except
    /// that eviction reasons collapse into
    /// [`MempoolEvictionReason::Unknown`] when the source cannot prove a
    /// more specific cause.
    Polling,
}

/// Capabilities the wired [`MempoolSource`] supports at runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MempoolSourceCapabilities {
    /// Mempool source backend in use.
    pub backend: MempoolSourceBackend,
    /// Whether the backend can report eviction reasons more precise than
    /// [`MempoolEvictionReason::Unknown`].
    pub reports_eviction_reasons: bool,
    /// Whether the backend reports `Mined` events without correlating with
    /// chain commits separately.
    ///
    /// Streaming backends that mirror Zebra's `MempoolChange::MINED`
    /// directly report `true`; polling backends that derive `Mined` from
    /// chain-commit correlation report `false`.
    pub reports_mined_directly: bool,
}

impl MempoolSourceCapabilities {
    /// Returns the capability set for a Zebra streaming backend.
    #[must_use]
    pub const fn streaming() -> Self {
        Self {
            backend: MempoolSourceBackend::Streaming,
            reports_eviction_reasons: false,
            reports_mined_directly: true,
        }
    }

    /// Returns the capability set for a JSON-RPC polling backend.
    #[must_use]
    pub const fn polling() -> Self {
        Self {
            backend: MempoolSourceBackend::Polling,
            reports_eviction_reasons: false,
            reports_mined_directly: false,
        }
    }
}

/// Stream of source-observed mempool events.
pub type MempoolSourceEventStream =
    Pin<Box<dyn Stream<Item = Result<MempoolSourceEvent, SourceError>> + Send + 'static>>;

/// Why hydrating a mempool source observation failed.
///
/// Each variant maps to a canonical `reason` label of the
/// `zinder_mempool_hydration_failures_total` counter so dashboards stay in
/// sync with emitter sites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolHydrationFailureReason {
    /// The upstream node did not return raw bytes for the txid.
    NotFound,
    /// The hydration RPC failed for a non-not-found reason.
    RpcError,
    /// No visible chain epoch was available to stamp the entry with.
    NoVisibleChainEpoch,
    /// Parsing the raw transaction bytes into a typed transaction failed.
    TransactionParseFailed,
    /// Building the compact-transaction projection from the parsed
    /// transaction failed.
    CompactTransactionBuildFailed,
    /// A transparent output index did not fit in `u32`.
    TransparentOutputIndexOverflow,
    /// The live mempool owner received a [`MempoolSourceEvent`] variant it does
    /// not yet know how to handle.
    UnknownSourceEventVariant,
}

impl MempoolHydrationFailureReason {
    /// Returns the canonical metric `reason` label.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::RpcError => "rpc_error",
            Self::NoVisibleChainEpoch => "no_visible_chain_epoch",
            Self::TransactionParseFailed => "transaction_parse_failed",
            Self::CompactTransactionBuildFailed => "compact_transaction_build_failed",
            Self::TransparentOutputIndexOverflow => "transparent_output_index_overflow",
            Self::UnknownSourceEventVariant => "unknown_source_event_variant",
        }
    }
}

/// Configured upstream mempool source for ingestion.
///
/// Implementations encapsulate streaming or polling, raw-transaction
/// hydration, initial-snapshot completion, and reconnect handling. The trait
/// yields one event stream per call to [`MempoolSource::events`]; callers
/// expecting durable reconnect must wrap the trait themselves.
#[async_trait]
pub trait MempoolSource: Send + Sync + 'static {
    /// Returns the capabilities of this mempool source backend.
    fn capabilities(&self) -> MempoolSourceCapabilities;

    /// Opens a typed mempool source event stream.
    ///
    /// Returns a [`MempoolSourceEventStream`] that yields hydrated
    /// [`MempoolSourceEvent`] values until the underlying source closes or
    /// fails permanently. Each healthy stream generation emits exactly one
    /// [`MempoolSourceEvent::InitialSnapshotComplete`] marker after its
    /// initial snapshot is complete. Transient transport failures are
    /// signalled by a [`SourceError`] item in the stream; callers should
    /// reconnect and keep the previous snapshot unavailable until the next
    /// generation's completion marker because the underlying broadcast
    /// channel may have lagged events.
    async fn events(&self) -> Result<MempoolSourceEventStream, SourceError>;
}

#[cfg(test)]
mod tests {
    use super::{MempoolSourceBackend, MempoolSourceCapabilities};

    #[test]
    fn streaming_capabilities_report_streaming_backend() {
        let capabilities = MempoolSourceCapabilities::streaming();
        assert_eq!(capabilities.backend, MempoolSourceBackend::Streaming);
        assert!(capabilities.reports_mined_directly);
    }

    #[test]
    fn polling_capabilities_report_polling_backend() {
        let capabilities = MempoolSourceCapabilities::polling();
        assert_eq!(capabilities.backend, MempoolSourceBackend::Polling);
        assert!(!capabilities.reports_mined_directly);
    }
}
