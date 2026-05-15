//! Mempool source-event payload values.
//!
//! Mempool events mirror the source-observed transitions: an `Added`
//! variant carries a hydrated [`MempoolEntry`]; `Invalidated` and `Mined`
//! variants carry the affected transaction id with a reason or mined
//! height. `Suppressed` carries the txid when the upstream node refuses
//! admission (ZIP-401 `RecentlyEvicted`; source-side emission is reserved
//! until the node exposes pre-admission visibility). The envelope binds the
//! event to its cursor token, monotonic
//! sequence, and source-observation timestamp.

use zinder_core::{BlockHash, BlockHeight, MempoolEntry, MempoolEvictionReason, TransactionId};

use crate::StreamCursorTokenV1;

/// Cursor-bound mempool source-event delivered to resumable consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MempoolEventEnvelope {
    /// Opaque cursor for resuming strictly after this event.
    pub cursor: StreamCursorTokenV1,
    /// Monotonic sequence in the mempool-event stream. Independent from
    /// the chain-event sequence space.
    pub event_sequence: u64,
    /// Wall-clock time when the indexer observed the source change.
    pub source_observed_unix_millis: u64,
    /// Mempool source transition observed by the indexer.
    pub event: MempoolEvent,
}

impl MempoolEventEnvelope {
    /// Returns the transaction identifier this event applies to.
    #[must_use]
    pub fn transaction_id(&self) -> TransactionId {
        self.event.transaction_id()
    }
}

/// Mempool source transition emitted into the event log.
///
/// `Added` carries the full hydrated [`MempoolEntry`] so consumers replay
/// state without follow-up snapshot calls. `Invalidated` and `Mined` carry
/// the txid plus the discriminating field; consumers cross-reference the
/// last-known entry from their local cache or from a prior `Added` event.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "Added carries the full hydrated MempoolEntry by design; consumers replay state from the event log without a follow-up snapshot call. Boxing the entry would push allocation cost into every consumer's pattern match and is not justified by the in-memory ring buffer footprint."
)]
pub enum MempoolEvent {
    /// Mempool transaction observed.
    Added {
        /// Hydrated entry observed by the indexer.
        entry: MempoolEntry,
    },
    /// Mempool transaction removed without being mined.
    Invalidated {
        /// Identifier of the invalidated transaction.
        transaction_id: TransactionId,
        /// Source-classified eviction reason.
        reason: MempoolEvictionReason,
    },
    /// Mempool transaction observed mined into a block.
    Mined {
        /// Identifier of the mined transaction.
        transaction_id: TransactionId,
        /// Height at which the source observed the mining.
        mined_height: BlockHeight,
        /// Hash of the block that mined the transaction, as observed by the
        /// source. Persisted alongside the height so cursor consumers can
        /// track lifecycle without a follow-up tip read.
        block_hash: BlockHash,
    },
    /// Upstream node refused admission of the transaction. Reserved for
    /// ZIP-401 `RecentlyEvicted` (the node drops re-broadcasts of a txid it
    /// recently evicted). The variant is wired through the wire and event
    /// log so external integrators can subscribe; source-side emission is
    /// pending node-side visibility as documented by the mempool topology.
    Suppressed {
        /// Identifier of the suppressed transaction.
        transaction_id: TransactionId,
    },
}

impl MempoolEvent {
    /// Returns the transaction identifier this event applies to.
    #[must_use]
    pub fn transaction_id(&self) -> TransactionId {
        match self {
            Self::Added { entry } => entry.transaction_id,
            Self::Invalidated { transaction_id, .. }
            | Self::Mined { transaction_id, .. }
            | Self::Suppressed { transaction_id } => *transaction_id,
        }
    }
}
