//! Stable identity of a block on the canonical chain.
//!
//! [`BlockId`] pairs the height and hash that together identify a block
//! independently of any storage epoch or node transport. Domain crates
//! that need to compare or persist block identity use this type instead of
//! tracking the height and hash as separate fields.

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
