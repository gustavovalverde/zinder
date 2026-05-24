//! Node-sourced commitment tree-state values.

use zinder_core::BlockId;

/// Commitment tree state observed directly from an upstream node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceTreeState {
    /// Block this tree-state payload belongs to.
    pub block_id: BlockId,
    /// JSON-encoded upstream tree-state payload.
    pub payload_bytes: Vec<u8>,
}

impl SourceTreeState {
    /// Creates a validated source tree-state value.
    #[must_use]
    pub fn new(block_id: BlockId, payload_bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            block_id,
            payload_bytes: payload_bytes.into(),
        }
    }
}
