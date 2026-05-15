//! Source boundary error vocabulary.

use thiserror::Error;
use zinder_core::BlockHeight;

use crate::NodeCapability;

/// Error returned while normalizing upstream node source values.
#[derive(Debug, Error)]
pub enum SourceError {
    /// Display-order block hash hex could not be decoded.
    #[error("block hash is not valid hex: {reason}")]
    InvalidBlockHashHex {
        /// Hex decoding failure.
        reason: String,
    },

    /// Raw block hex returned by the node could not be decoded.
    ///
    /// Prefer this variant when the `getblock` payload is malformed at the hex
    /// layer (odd-length, non-hex digit). Once the bytes decode successfully
    /// but fail a Zcash invariant (coinbase height, parse, timestamp range),
    /// reach for the matching `RawBlock*` variant instead. Pure JSON-RPC
    /// envelope violations (missing fields, wrong types) belong in
    /// [`Self::SourceProtocolMismatch`].
    #[error("raw block is not valid hex")]
    InvalidRawBlockHex {
        /// Hex decoding failure.
        #[source]
        source: hex::FromHexError,
    },

    /// Raw transaction hex returned by the node could not be decoded.
    ///
    /// Distinct from [`Self::InvalidRawBlockHex`] because the metric class
    /// label aggregates by error variant; mempool hydration failures must
    /// not be charged to the block-decode counter.
    #[error("raw transaction is not valid hex")]
    InvalidRawTransactionHex {
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
    #[error("transaction id is not valid hex: {reason}")]
    InvalidTransactionIdHex {
        /// Hex decoding failure.
        reason: String,
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
    ///
    /// Prefer this variant when bytes decoded from hex successfully but the
    /// resulting buffer is not a valid Zcash block (truncated header,
    /// unexpected transaction encoding). For hex-layer failures use
    /// [`Self::InvalidRawBlockHex`]; for JSON-RPC envelope violations use
    /// [`Self::SourceProtocolMismatch`].
    #[error("raw block parse failed: {reason}")]
    RawBlockParseFailed {
        /// Parser failure reason.
        reason: String,
    },

    /// Raw transaction bytes could not be parsed as a Zcash transaction.
    #[error("raw transaction parse failed: {reason}")]
    RawTransactionParseFailed {
        /// Parser failure reason.
        reason: String,
    },

    /// Parsed raw block did not contain a coinbase height.
    #[error("raw block is missing its coinbase height")]
    RawBlockCoinbaseHeightMissing,

    /// Parsed raw block height did not match the node-reported height.
    ///
    /// Prefer this variant when the bytes parsed cleanly but the coinbase
    /// height disagrees with what the node-side height lookup returned (a
    /// Zcash consensus-level mismatch). For JSON-RPC envelope mismatches
    /// (header object reports a different height than requested) use
    /// [`Self::SourceProtocolMismatch`] instead.
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
    ///
    /// Prefer this variant when the JSON-RPC envelope or response shape is
    /// wrong: missing fields, wrong types, header height disagreeing with the
    /// requested height, tree-state hash disagreeing with the anchor hash. For
    /// hex-layer failures use [`Self::InvalidRawBlockHex`]; for byte-level
    /// parse failures use [`Self::RawBlockParseFailed`]; for coinbase-height
    /// disagreements after successful parse use
    /// [`Self::RawBlockHeightMismatch`].
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

    /// The upstream mempool source stream closed or failed.
    ///
    /// Returned when the gRPC `MempoolChange` stream terminates with a
    /// transport error or with `RecvError::Lagged`. Adapters that wrap a
    /// [`crate::MempoolSource`] for durable consumption must reconnect and
    /// snapshot the mempool state because lagged events are not replayed.
    #[error("upstream mempool source stream is unavailable: {reason}")]
    MempoolStreamUnavailable {
        /// Stream or transport failure reason.
        reason: String,
        /// Whether reconnecting can reasonably succeed later.
        is_retryable: bool,
    },

    /// Hydrating an `Added` mempool observation failed.
    ///
    /// Returned when fetching the raw transaction bytes for an
    /// `ADDED` source change fails (e.g. JSON-RPC `getrawtransaction`
    /// returns an error). The adapter increments
    /// `zinder_mempool_hydration_failures_total` before yielding this
    /// error.
    #[error("mempool transaction hydration failed: {reason}")]
    MempoolHydrationFailed {
        /// Identifier of the unhydrated transaction.
        transaction_id: zinder_core::TransactionId,
        /// Hydration failure reason.
        reason: String,
        /// Whether retrying the same hydration request can reasonably
        /// succeed later.
        is_retryable: bool,
    },

    /// Upstream chain-tip notification stream is unavailable.
    ///
    /// Returned when the gRPC `ChainTipChange` subscription cannot be
    /// established or terminates with a transport error. The ingest
    /// tip-follow loop treats this as a signal to keep polling and
    /// re-subscribe in the background.
    #[error("upstream chain-tip notification stream is unavailable: {reason}")]
    ChainTipStreamUnavailable {
        /// Stream or transport failure reason.
        reason: String,
        /// Whether reconnecting can reasonably succeed later.
        is_retryable: bool,
    },
}

impl SourceError {
    /// Returns whether retrying the same source request can reasonably succeed later.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::NodeUnavailable { is_retryable, .. }
            | Self::BlockUnavailable { is_retryable, .. }
            | Self::SubtreeRootsUnavailable { is_retryable, .. }
            | Self::MempoolStreamUnavailable { is_retryable, .. }
            | Self::MempoolHydrationFailed { is_retryable, .. }
            | Self::ChainTipStreamUnavailable { is_retryable, .. } => *is_retryable,
            Self::InvalidBlockHashHex { .. }
            | Self::InvalidRawBlockHex { .. }
            | Self::InvalidRawTransactionHex { .. }
            | Self::InvalidBlockHashLength { .. }
            | Self::InvalidTransactionIdHex { .. }
            | Self::InvalidTransactionIdLength { .. }
            | Self::InvalidSubtreeRootHex { .. }
            | Self::InvalidSubtreeRootLength { .. }
            | Self::RawBlockParseFailed { .. }
            | Self::RawTransactionParseFailed { .. }
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
