//! Transaction identity, submission, and durable artifact values.

use crate::{BlockHash, BlockHeight};

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
