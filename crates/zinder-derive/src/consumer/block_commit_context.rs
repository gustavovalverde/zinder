//! Per-block parsed view shared across derive consumers for one commit.
//!
//! [`BlockCommitContext`] carries the block bytes ingest just committed, the
//! shared parsed `zebra-chain` block, and the prevout map ingest resolved from the
//! canonical store. The same value is shared across every chain-events
//! consumer in the writer fan-out, so the block parses once and prevouts
//! resolve once even though four independent `apply_block` calls observe
//! the same height.
//!
//! Hosting the parsed block (rather than the raw bytes) inside the cache
//! matters: re-parsing a 2 MB mainnet block four times per commit would
//! dominate the per-block CPU budget.

use std::collections::HashMap;
use std::sync::Arc;

use zebra_chain::block::Block as ZebraBlock;
use zinder_core::{BlockHeight, TransparentOutPoint, TransparentPrevout};

/// Errors surfaced while hydrating a [`BlockCommitContext`] or resolving
/// its prevouts.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlockCommitContextError {
    /// Reserved for future in-process resolver failures.
    #[error("prevout resolver failed: {reason}")]
    Resolver {
        /// Human-readable failure reason.
        reason: String,
    },
}

/// How [`BlockCommitContext::prevouts`] resolves missing values.
///
/// `Offline` short-circuits to `None` so consumers that branch on prevout
/// availability never block on a resolution attempt the binary has
/// explicitly disabled. `Static` is the writer-owned path: ingest resolves
/// prevouts from the canonical store and supplies the map directly.
#[derive(Clone)]
pub enum PrevoutResolver {
    /// Prevout resolution is not available; `prevouts()` returns `None`.
    Offline,
    /// In-process resolution: the caller provides the precomputed prevout
    /// map. Used when the consumer runs colocated with the writer that
    /// holds the canonical store and the prevout lookup is a direct read
    /// against canonical UTXO artifacts, not a gRPC round-trip.
    Static(Arc<HashMap<TransparentOutPoint, TransparentPrevout>>),
}

impl PrevoutResolver {
    /// Wraps a precomputed prevout map into the `Static` variant.
    #[must_use]
    pub fn from_map(map: Arc<HashMap<TransparentOutPoint, TransparentPrevout>>) -> Self {
        Self::Static(map)
    }
}

impl std::fmt::Debug for PrevoutResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => formatter.write_str("PrevoutResolver::Offline"),
            Self::Static(map) => write!(formatter, "PrevoutResolver::Static({})", map.len()),
        }
    }
}

/// Per-block parsed view threaded through one batch of consumer `apply_block`
/// calls.
///
/// Held inside `Arc<BlockCommitContext>` and shared across consumers
/// observing the same height.
pub struct BlockCommitContext {
    /// Height the context describes.
    pub height: BlockHeight,
    /// Block hash bytes (32 bytes, internal byte order).
    pub block_hash: Vec<u8>,
    /// Previous-block hash bytes (32 bytes, internal byte order).
    pub previous_block_hash: Vec<u8>,
    /// Raw block byte count as the wallet plane delivered it. This is the
    /// authoritative on-disk block size without retaining another copy of the
    /// raw block payload.
    pub raw_block_size_bytes: usize,
    /// Block parsed once with `zebra-chain` and shared by derive consumers.
    pub block: Arc<ZebraBlock>,
    resolver: PrevoutResolver,
}

/// Parsed-block payload [`BlockCommitContext::new`] takes by value.
pub struct BlockCommitPayload {
    /// Height of the block.
    pub height: BlockHeight,
    /// Block hash bytes (32 bytes, internal byte order).
    pub block_hash: Vec<u8>,
    /// Previous-block hash bytes (32 bytes, internal byte order).
    pub previous_block_hash: Vec<u8>,
    /// Raw block byte count.
    pub raw_block_size_bytes: usize,
    /// Block parsed once with `zebra-chain`.
    pub block: Arc<ZebraBlock>,
}

impl BlockCommitContext {
    /// Builds a context from an already-parsed block plus its raw bytes.
    #[must_use]
    pub fn new(payload: BlockCommitPayload, resolver: PrevoutResolver) -> Self {
        Self {
            height: payload.height,
            block_hash: payload.block_hash,
            previous_block_hash: payload.previous_block_hash,
            raw_block_size_bytes: payload.raw_block_size_bytes,
            block: payload.block,
            resolver,
        }
    }

    /// Returns the prevout map for the block's non-coinbase inputs.
    ///
    /// `Ok(None)` means the binary configured an [`PrevoutResolver::Offline`]
    /// resolver, so the consumer should treat every prevout as unresolved.
    /// `Ok(Some(map))` resolves every transparent input the block contains;
    /// the map may be missing individual outpoints when the upstream cannot
    /// produce them.
    pub fn prevouts(
        &self,
    ) -> Result<
        Option<Arc<HashMap<TransparentOutPoint, TransparentPrevout>>>,
        BlockCommitContextError,
    > {
        Ok(match &self.resolver {
            PrevoutResolver::Offline => None,
            PrevoutResolver::Static(map) => Some(Arc::clone(map)),
        })
    }
}
