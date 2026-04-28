//! Source boundary error vocabulary.

use thiserror::Error;
use zinder_core::BlockHeight;

use crate::NodeCapability;

/// Error returned while normalizing upstream node source values.
#[derive(Debug, Error)]
pub enum SourceError {
    /// Display-order block hash hex could not be decoded.
    #[error("block hash is not valid hex")]
    InvalidBlockHashHex {
        /// Hex decoding failure.
        #[source]
        source: hex::FromHexError,
    },

    /// Raw block hex returned by the node could not be decoded.
    #[error("raw block is not valid hex")]
    InvalidRawBlockHex {
        /// Hex decoding failure.
        #[source]
        source: hex::FromHexError,
    },

    /// Display-order block hash decoded to the wrong byte length.
    #[error("block hash must be 32 bytes, got {byte_count}")]
    InvalidBlockHashLength {
        /// Decoded byte length.
        byte_count: usize,
    },

    /// Display-order transaction id hex could not be decoded.
    #[error("transaction id is not valid hex")]
    InvalidTransactionIdHex {
        /// Hex decoding failure.
        #[source]
        source: hex::FromHexError,
    },

    /// Display-order transaction id decoded to the wrong byte length.
    #[error("transaction id must be 32 bytes, got {byte_count}")]
    InvalidTransactionIdLength {
        /// Decoded byte length.
        byte_count: usize,
    },

    /// Subtree root hex could not be decoded.
    #[error("subtree root is not valid hex")]
    InvalidSubtreeRootHex {
        /// Hex decoding failure.
        #[source]
        source: hex::FromHexError,
    },

    /// Subtree root decoded to the wrong byte length.
    #[error("subtree root must be 32 bytes, got {byte_count}")]
    InvalidSubtreeRootLength {
        /// Decoded byte length.
        byte_count: usize,
    },

    /// Raw block bytes could not be parsed as a Zcash block.
    #[error("raw block parse failed: {reason}")]
    RawBlockParseFailed {
        /// Parser failure reason.
        reason: String,
    },

    /// Parsed raw block did not contain a coinbase height.
    #[error("raw block is missing its coinbase height")]
    RawBlockCoinbaseHeightMissing,

    /// Parsed raw block height did not match the node-reported height.
    #[error("raw block height {parsed_height} does not match source height {source_height:?}")]
    RawBlockHeightMismatch {
        /// Height parsed from the raw block coinbase transaction.
        parsed_height: u32,
        /// Height reported by the node request path.
        source_height: BlockHeight,
    },

    /// Parsed raw block timestamp could not be represented as Unix seconds.
    #[error("raw block time is outside the supported Unix-seconds range")]
    RawBlockTimeOutOfRange,

    /// Configured upstream node could not answer a request.
    #[error("upstream node is unavailable: {reason}")]
    NodeUnavailable {
        /// Node or transport failure reason.
        reason: String,
        /// Whether retrying the same request can reasonably succeed later.
        is_retryable: bool,
    },

    /// The configured upstream node does not support a required capability.
    #[error("node capability is missing: {capability}")]
    NodeCapabilityMissing {
        /// Missing node capability.
        capability: NodeCapability,
    },

    /// The wired transaction broadcaster is a no-op.
    ///
    /// Returned by the unit `TransactionBroadcaster` impl to let query layers
    /// distinguish a deliberate read-only configuration from a real upstream
    /// node failure.
    #[error("transaction broadcast is disabled")]
    TransactionBroadcastDisabled,

    /// The selected node authentication mode is unsupported by this source.
    #[error("node source {source_name} does not support {auth_scheme} authentication")]
    UnsupportedNodeAuth {
        /// Node source name.
        source_name: &'static str,
        /// Unsupported authentication scheme.
        auth_scheme: &'static str,
    },

    /// The node returned an error for a required block.
    #[error("block at height {height:?} is unavailable: {reason}")]
    BlockUnavailable {
        /// Requested block height.
        height: BlockHeight,
        /// Node error message.
        reason: String,
        /// Whether retrying the same request can reasonably succeed later.
        is_retryable: bool,
    },

    /// The node returned an error for required subtree roots.
    #[error("{protocol:?} subtree roots from {start_index:?} are unavailable: {reason}")]
    SubtreeRootsUnavailable {
        /// Shielded protocol requested.
        protocol: zinder_core::ShieldedProtocol,
        /// First requested subtree-root index.
        start_index: zinder_core::SubtreeRootIndex,
        /// Node error message.
        reason: String,
        /// Whether retrying the same request can reasonably succeed later.
        is_retryable: bool,
    },

    /// The node response did not match the expected JSON-RPC contract.
    #[error("source protocol mismatch: {reason}")]
    SourceProtocolMismatch {
        /// Protocol mismatch reason.
        reason: &'static str,
    },

    /// A source payload could not be encoded for downstream storage.
    #[error("source payload could not be encoded as JSON")]
    SourcePayloadEncodingFailed {
        /// JSON encoding failure.
        #[source]
        source: serde_json::Error,
    },
}

impl SourceError {
    /// Returns whether retrying the same source request can reasonably succeed later.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::NodeUnavailable { is_retryable, .. }
            | Self::BlockUnavailable { is_retryable, .. }
            | Self::SubtreeRootsUnavailable { is_retryable, .. } => *is_retryable,
            Self::InvalidBlockHashHex { .. }
            | Self::InvalidRawBlockHex { .. }
            | Self::InvalidBlockHashLength { .. }
            | Self::InvalidTransactionIdHex { .. }
            | Self::InvalidTransactionIdLength { .. }
            | Self::InvalidSubtreeRootHex { .. }
            | Self::InvalidSubtreeRootLength { .. }
            | Self::RawBlockParseFailed { .. }
            | Self::RawBlockCoinbaseHeightMissing
            | Self::RawBlockHeightMismatch { .. }
            | Self::RawBlockTimeOutOfRange
            | Self::NodeCapabilityMissing { .. }
            | Self::TransactionBroadcastDisabled
            | Self::UnsupportedNodeAuth { .. }
            | Self::SourceProtocolMismatch { .. }
            | Self::SourcePayloadEncodingFailed { .. } => false,
        }
    }
}
