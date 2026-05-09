//! Transaction identity, submission, and durable artifact values.

use crate::{BlockHash, BlockHeight, ChainEpoch, MempoolEntry};

/// Zcash transaction identifier bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransactionId([u8; 32]);

impl TransactionId {
    /// Creates a transaction identifier from canonical 32-byte id material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the transaction identifier bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// ZIP-244 transaction authorization digest bytes.
///
/// Populated for v5+ transactions. The legacy v1-v4 transactions have no
/// distinct authorization digest because their txid already covers their
/// witness data; on those source observations the mempool entry's
/// authorization digest is `None`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AuthDigest([u8; 32]);

impl AuthDigest {
    /// Creates an authorization digest from canonical 32-byte material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the authorization digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Raw serialized Zcash transaction bytes submitted by a wallet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTransactionBytes(Vec<u8>);

impl RawTransactionBytes {
    /// Creates raw transaction bytes from serialized transaction material.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Returns the raw serialized transaction bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

/// Result of submitting a raw transaction to a node or network path.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransactionBroadcastResult {
    /// The node accepted the transaction for mempool admission or relay.
    Accepted(BroadcastAccepted),
    /// The node reported the transaction was already known.
    Duplicate(BroadcastDuplicate),
    /// The node could not decode the submitted transaction bytes.
    InvalidEncoding(BroadcastInvalidEncoding),
    /// The node rejected the transaction with a known rejection message.
    Rejected(BroadcastRejected),
    /// The node returned an unclassified broadcast response.
    Unknown(BroadcastUnknown),
}

/// Accepted transaction broadcast details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BroadcastAccepted {
    /// Transaction identifier reported by the node.
    pub transaction_id: TransactionId,
}

/// Duplicate transaction broadcast details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastDuplicate {
    /// Node error code when one was supplied.
    pub error_code: Option<i64>,
    /// Operator-facing node message.
    pub message: String,
}

/// Invalid transaction encoding details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastInvalidEncoding {
    /// Node error code when one was supplied.
    pub error_code: Option<i64>,
    /// Operator-facing node message.
    pub message: String,
}

/// Rejected transaction broadcast details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastRejected {
    /// Node error code when one was supplied.
    pub error_code: Option<i64>,
    /// Operator-facing node message.
    pub message: String,
}

/// Unclassified transaction broadcast details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastUnknown {
    /// Node error code when one was supplied.
    pub error_code: Option<i64>,
    /// Operator-facing node message.
    pub message: String,
}

/// Durable artifact derived from a transaction in a block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionArtifact {
    /// Transaction identifier.
    pub transaction_id: TransactionId,
    /// Height of the containing block.
    pub block_height: BlockHeight,
    /// Hash of the containing block.
    pub block_hash: BlockHash,
    /// Serialized transaction payload or fixture bytes.
    pub payload_bytes: Vec<u8>,
}

/// Mined-transaction enrichment fields bound to a response's [`ChainEpoch`].
///
/// `MinedDetails` is a *response/read-model* value, not a persisted field on
/// [`TransactionArtifact`]. The only public constructor takes the response
/// epoch and the mined block's identity together, which prevents the racy
/// `tip_height - block_height` confirmations computation by construction:
/// the epoch is in scope when confirmations are computed, so callers cannot
/// accidentally re-read the tip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MinedDetails {
    /// Consensus branch identifier in effect at the mined height.
    pub consensus_branch_id: u32,
    /// Block-time as Unix seconds, taken from the mined block header.
    pub block_time: i64,
    /// Confirmations: `tip_height - mined_height + 1`, bound to the
    /// response's `ChainEpoch`.
    pub confirmations: u32,
}

impl MinedDetails {
    /// Constructs the only canonical mined-details value, binding
    /// confirmations to the response's chain epoch.
    ///
    /// Callers supply pre-computed `consensus_branch_id` and `block_time`;
    /// these are derived from the source-aware (zinder-source) layer that
    /// owns Zcash consensus parameters and block-header parsing. The
    /// constructor itself only computes `confirmations`, which is the
    /// quantity that depends on the response epoch.
    #[must_use]
    pub fn from_response_epoch(
        epoch: &ChainEpoch,
        mined_height: BlockHeight,
        consensus_branch_id: u32,
        block_time: i64,
    ) -> Self {
        let tip = epoch.tip_height.value();
        let mined = mined_height.value();
        let confirmations = tip.saturating_sub(mined).saturating_add(1);
        Self {
            consensus_branch_id,
            block_time,
            confirmations,
        }
    }
}

/// Mined-transaction read-model record carried in [`TxStatus::Mined`]
/// and the wire-side `MinedTransaction` message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinedTransaction {
    /// Durable mined-transaction artifact.
    pub artifact: TransactionArtifact,
    /// Response-bound enrichment fields.
    pub details: MinedDetails,
}

impl MinedTransaction {
    /// Creates a mined-transaction read-model record.
    #[must_use]
    pub const fn new(artifact: TransactionArtifact, details: MinedDetails) -> Self {
        Self { artifact, details }
    }
}

/// Typed transaction-status value returned by transaction lookups.
///
/// Mirrors the wire `TransactionStatusResponse.status` oneof at the
/// in-memory boundary, with one extra `NotFound` arm. `NotFound` is a
/// gRPC `NOT_FOUND` on the wire (typed errors don't waste oneof slots);
/// this Rust enum surfaces it as a value so callers can match
/// exhaustively without conflating it with an error.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "InMempool carries the full hydrated MempoolEntry by design so consumers (Zallet, Zashi/Zodl) can drop the string-matching tx-status workaround they currently use against zaino. Boxing would push allocation cost into every consumer's pattern match."
)]
pub enum TxStatus {
    /// Transaction is mined in the canonical chain.
    ///
    /// Carries the durable [`TransactionArtifact`] together with
    /// epoch-bound [`MinedDetails`] enrichment.
    Mined(MinedTransaction),
    /// Transaction is not indexed in the visible canonical chain.
    NotFound,
    /// Transaction is known to be in the mempool.
    InMempool(MempoolEntry),
    /// Transaction conflicts with the visible canonical chain.
    ConflictingChain,
}

impl TransactionArtifact {
    /// Creates a transaction artifact.
    #[must_use]
    pub fn new(
        transaction_id: TransactionId,
        block_height: BlockHeight,
        block_hash: BlockHash,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            transaction_id,
            block_height,
            block_hash,
            payload_bytes: payload_bytes.into(),
        }
    }
}
