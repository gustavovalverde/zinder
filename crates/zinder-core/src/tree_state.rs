//! Durable commitment tree-state artifact values.

use crate::{BlockHash, BlockHeight};

/// One final note-commitment-tree root in the byte order emitted by Zebra.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FinalNoteCommitmentRoot([u8; 32]);

impl FinalNoteCommitmentRoot {
    /// Creates a final note-commitment root from exactly 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the root bytes in their original Zebra RPC byte order.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Final note-commitment-tree roots after one canonical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockFinalNoteCommitmentRoots {
    /// Height of the block that produced these roots.
    pub height: BlockHeight,
    /// Hash of the block that produced these roots.
    pub block_hash: BlockHash,
    /// Final Sapling root, or `None` when unavailable or before activation.
    pub sapling: Option<FinalNoteCommitmentRoot>,
    /// Final Orchard root, or `None` when unavailable or before activation.
    pub orchard: Option<FinalNoteCommitmentRoot>,
    /// Final Ironwood root, or `None` when unavailable or before activation.
    pub ironwood: Option<FinalNoteCommitmentRoot>,
}

impl BlockFinalNoteCommitmentRoots {
    /// Creates the final roots associated with one block.
    #[must_use]
    pub const fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        sapling: Option<FinalNoteCommitmentRoot>,
        orchard: Option<FinalNoteCommitmentRoot>,
        ironwood: Option<FinalNoteCommitmentRoot>,
    ) -> Self {
        Self {
            height,
            block_hash,
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Creates a block value whose roots are all unavailable.
    #[must_use]
    pub const fn unavailable(height: BlockHeight, block_hash: BlockHash) -> Self {
        Self::new(height, block_hash, None, None, None)
    }
}

/// Commitment tree-state artifact for wallet-compatible reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeStateArtifact {
    /// Height the tree state belongs to.
    pub height: BlockHeight,
    /// Hash of the block this tree state belongs to.
    pub block_hash: BlockHash,
    /// Encoded tree-state payload bytes.
    pub payload_bytes: Vec<u8>,
}

impl TreeStateArtifact {
    /// Creates a tree-state artifact.
    #[must_use]
    pub fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            height,
            block_hash,
            payload_bytes: payload_bytes.into(),
        }
    }
}
