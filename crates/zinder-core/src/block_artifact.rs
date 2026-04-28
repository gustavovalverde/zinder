//! Durable block artifact values.

use crate::{BlockHash, BlockHeight};

/// Durable artifact derived from a full block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockArtifact {
    /// Height of the source block.
    pub height: BlockHeight,
    /// Hash of the source block.
    pub block_hash: BlockHash,
    /// Parent hash of the source block.
    pub parent_hash: BlockHash,
    /// Serialized block payload or fixture bytes.
    pub payload_bytes: Vec<u8>,
}

impl BlockArtifact {
    /// Creates a block artifact from block metadata and payload bytes.
    #[must_use]
    pub fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            height,
            block_hash,
            parent_hash,
            payload_bytes: payload_bytes.into(),
        }
    }
}

/// Wallet-oriented compact block artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactBlockArtifact {
    /// Height of the source block.
    pub height: BlockHeight,
    /// Hash of the full block this compact artifact was built from.
    pub block_hash: BlockHash,
    /// Compact block payload bytes.
    pub payload_bytes: Vec<u8>,
}

impl CompactBlockArtifact {
    /// Creates a compact block artifact.
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
