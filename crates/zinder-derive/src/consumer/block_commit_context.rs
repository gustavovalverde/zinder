//! Per-block canonical fact view shared across derive consumers.
//!
//! [`BlockCommitContext`] is hydrated from the fact-first canonical store:
//! block-header facts, ordered transaction facts, and resolved transparent
//! outputs for the block's transparent inputs. Derive replay no longer depends
//! on raw block blobs or `zebra-chain` parsing, so startup replay and
//! steady-state tailing use the same typed input shape.

use std::collections::HashMap;
use std::sync::Arc;

use zinder_core::{
    BlockHeight, TransactionFactsArtifact, TransparentOutPoint, TransparentSpendFact,
};

/// Errors surfaced while reading a block context's hydrated transparent spends.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockCommitContextError {
    /// Reserved for future in-process hydration failures.
    #[error("transparent spend hydration failed: {reason}")]
    Hydration {
        /// Human-readable failure reason.
        reason: String,
    },
}

/// Hydrated transparent spend facts available to derive consumers.
///
/// `Offline` short-circuits to `None` so consumers can emit explicit
/// `UNAVAILABLE` records when transparent-spend hydration is disabled. `Static`
/// is the writer-owned path: ingest resolves every event-scoped spend from the
/// canonical store and supplies the map directly.
#[derive(Clone)]
pub enum TransparentSpendFacts {
    /// Transparent-spend hydration is not available.
    Offline,
    /// In-process hydration: the caller provides the precomputed spend map.
    Static(Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>),
}

impl TransparentSpendFacts {
    /// Wraps a precomputed spend-fact map into the `Static` variant.
    #[must_use]
    pub fn from_map(map: Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>) -> Self {
        Self::Static(map)
    }
}

impl std::fmt::Debug for TransparentSpendFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => formatter.write_str("TransparentSpendFacts::Offline"),
            Self::Static(map) => write!(formatter, "TransparentSpendFacts::Static({})", map.len()),
        }
    }
}

/// Per-block typed fact view threaded through consumer `apply_block` calls.
pub struct BlockCommitContext {
    /// Height the context describes.
    pub height: BlockHeight,
    /// Block hash bytes (32 bytes, internal byte order).
    pub block_hash: Vec<u8>,
    /// Previous-block hash bytes (32 bytes, internal byte order).
    pub previous_block_hash: Vec<u8>,
    /// Block-time as Unix seconds.
    pub block_time_unix_seconds: i64,
    /// Serialized consensus block size in bytes.
    pub block_size_bytes: u64,
    /// Ordered transaction facts for every transaction in the block.
    pub transactions: Vec<TransactionFactsArtifact>,
    transparent_spends: TransparentSpendFacts,
}

/// Canonical fact payload [`BlockCommitContext::new`] takes by value.
pub struct BlockCommitPayload {
    /// Height of the block.
    pub height: BlockHeight,
    /// Block hash bytes (32 bytes, internal byte order).
    pub block_hash: Vec<u8>,
    /// Previous-block hash bytes (32 bytes, internal byte order).
    pub previous_block_hash: Vec<u8>,
    /// Block-time as Unix seconds.
    pub block_time_unix_seconds: i64,
    /// Serialized consensus block size in bytes.
    pub block_size_bytes: u64,
    /// Ordered transaction facts for every transaction in the block.
    pub transactions: Vec<TransactionFactsArtifact>,
}

impl BlockCommitContext {
    /// Builds a context from canonical block and transaction facts.
    #[must_use]
    pub fn new(payload: BlockCommitPayload, transparent_spends: TransparentSpendFacts) -> Self {
        Self {
            height: payload.height,
            block_hash: payload.block_hash,
            previous_block_hash: payload.previous_block_hash,
            block_time_unix_seconds: payload.block_time_unix_seconds,
            block_size_bytes: payload.block_size_bytes,
            transactions: payload.transactions,
            transparent_spends,
        }
    }

    /// Returns the hydrated transparent spend map for the block's transparent inputs.
    ///
    /// `Ok(None)` means the binary configured [`TransparentSpendFacts::Offline`].
    /// `Ok(Some(map))` contains every transparent input the store can identify;
    /// the map may be missing individual outpoints when the source block
    /// references an output outside retained canonical facts.
    pub fn transparent_spends(
        &self,
    ) -> Result<
        Option<Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>>,
        BlockCommitContextError,
    > {
        Ok(match &self.transparent_spends {
            TransparentSpendFacts::Offline => None,
            TransparentSpendFacts::Static(map) => Some(Arc::clone(map)),
        })
    }
}
