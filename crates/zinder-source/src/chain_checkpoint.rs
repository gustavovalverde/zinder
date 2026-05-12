//! Node-observed chain checkpoint values.
//!
//! A [`SourceChainCheckpoint`] records the minimum data needed to seed a
//! Zinder canonical chain epoch at a non-genesis height: the block hash and
//! commitment-tree sizes that an [`ingest`-style consumer] needs to produce a
//! valid [`ChainTipMetadata`] without re-deriving every commitment from
//! genesis.
//!
//! Zebra's `getblock` (verbosity 1) is the source of truth for these values
//! today. Future node adapters can populate this struct from any
//! equivalent observation.
//!
//! [`ChainTipMetadata`]: zinder_core::ChainTipMetadata
//! [`ingest`-style consumer]: https://github.com/gustavovalverde/zinder

use zinder_core::{BlockHash, BlockHeight, ChainTipMetadata};

/// One node-observed chain checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceChainCheckpoint {
    /// Block height at which this checkpoint was observed.
    pub height: BlockHeight,
    /// Canonical block hash at `height`, in internal little-endian byte order.
    pub hash: BlockHash,
    /// Commitment-tree sizes after applying the block at `height`.
    pub tip_metadata: ChainTipMetadata,
}

impl SourceChainCheckpoint {
    /// Creates a checkpoint observation.
    #[must_use]
    pub const fn new(height: BlockHeight, hash: BlockHash, tip_metadata: ChainTipMetadata) -> Self {
        Self {
            height,
            hash,
            tip_metadata,
        }
    }
}
