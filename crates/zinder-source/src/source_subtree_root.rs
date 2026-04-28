//! Node-sourced subtree-root values.

use zinder_core::{BlockHeight, ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex};

/// Source subtree-root data observed before canonical artifact construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSubtreeRoot {
    /// Subtree index within the shielded protocol.
    pub subtree_index: SubtreeRootIndex,
    /// Merkle root of the complete subtree.
    pub root_hash: SubtreeRootHash,
    /// Height of the block that completed this subtree.
    pub completing_block_height: BlockHeight,
}

impl SourceSubtreeRoot {
    /// Creates a source subtree-root value.
    #[must_use]
    pub const fn new(
        subtree_index: SubtreeRootIndex,
        root_hash: SubtreeRootHash,
        completing_block_height: BlockHeight,
    ) -> Self {
        Self {
            subtree_index,
            root_hash,
            completing_block_height,
        }
    }
}

/// Bounded source subtree-root response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSubtreeRoots {
    /// Shielded protocol requested.
    pub protocol: ShieldedProtocol,
    /// First requested subtree-root index.
    pub start_index: SubtreeRootIndex,
    /// Returned subtree roots in ascending index order.
    pub subtree_roots: Vec<SourceSubtreeRoot>,
}

impl SourceSubtreeRoots {
    /// Creates a bounded source subtree-root response.
    #[must_use]
    pub fn new(
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        subtree_roots: impl Into<Vec<SourceSubtreeRoot>>,
    ) -> Self {
        Self {
            protocol,
            start_index,
            subtree_roots: subtree_roots.into(),
        }
    }
}
