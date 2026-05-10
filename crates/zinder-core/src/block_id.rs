//! Stable identity of a block on the canonical chain, plus the typed
//! selector used by hash- or height-keyed read paths.

use crate::{BlockHash, BlockHeight};

/// Stable block identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct BlockId {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
}

impl BlockId {
    /// Constructs a [`BlockId`] from its component parts.
    #[must_use]
    pub const fn new(height: BlockHeight, hash: BlockHash) -> Self {
        Self { height, hash }
    }
}

/// Typed selector for a block in the canonical best chain.
///
/// Read paths that may receive either a height or a hash accept this
/// enum and resolve `Hash` selectors through the canonical hash-to-height
/// resolver before reaching height-keyed storage. Replaces the
/// lightwalletd `BlockId { height, hash }` request shape (where
/// `height = 0` is a sentinel for "ignore height, use hash"); the typed
/// enum has no sentinel and no genesis ambiguity.
///
/// Non-best-chain `(txid, block_hash)` lookup is a separate, deferred
/// operation; future support lands as a different method, not a third
/// selector arm. The enum is `#[non_exhaustive]` to keep that boundary
/// explicit.
///
/// refuses: A4
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BlockSelector {
    /// Address the block at this height in the canonical best chain.
    Height(BlockHeight),
    /// Address the block with this hash in the canonical best chain.
    Hash(BlockHash),
}

impl BlockSelector {
    /// Constructs a height-keyed selector.
    #[must_use]
    pub const fn from_height(height: BlockHeight) -> Self {
        Self::Height(height)
    }

    /// Constructs a hash-keyed selector.
    #[must_use]
    pub const fn from_hash(hash: BlockHash) -> Self {
        Self::Hash(hash)
    }
}
