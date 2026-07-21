//! Node-sourced commitment tree-state values.

use zinder_core::{BlockFinalNoteCommitmentRoots, BlockId};

/// Commitment tree state observed directly from an upstream node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceTreeState {
    /// Block this tree-state payload belongs to.
    pub block_id: BlockId,
    /// Block timestamp in Unix seconds.
    pub block_time_seconds: u32,
    /// Typed final note-commitment roots promoted from the upstream payload.
    pub final_note_commitment_roots: BlockFinalNoteCommitmentRoots,
    /// JSON-encoded upstream tree-state payload.
    pub payload_bytes: Vec<u8>,
}

impl SourceTreeState {
    /// Creates a validated source tree-state value.
    #[must_use]
    pub fn new(
        block_id: BlockId,
        block_time_seconds: u32,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            block_id,
            block_time_seconds,
            final_note_commitment_roots: BlockFinalNoteCommitmentRoots::unavailable(
                block_id.height,
                block_id.hash,
            ),
            payload_bytes: payload_bytes.into(),
        }
    }

    /// Creates a source tree-state value with promoted final roots.
    #[must_use]
    pub fn with_final_note_commitment_roots(
        final_note_commitment_roots: BlockFinalNoteCommitmentRoots,
        block_time_seconds: u32,
        payload_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            block_id: BlockId::new(
                final_note_commitment_roots.height,
                final_note_commitment_roots.block_hash,
            ),
            block_time_seconds,
            final_note_commitment_roots,
            payload_bytes: payload_bytes.into(),
        }
    }
}
