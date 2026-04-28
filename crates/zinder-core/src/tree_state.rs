//! Durable commitment tree-state artifact values.

use crate::{BlockHash, BlockHeight};

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
