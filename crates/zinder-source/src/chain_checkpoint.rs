//! Node-observed chain checkpoint values.
//!
//! A [`SourceChainCheckpoint`] records the minimum data needed to seed note
//! commitment trees at a non-genesis height: the exact block identity and
//! time, plus the validated frontier of every active shielded pool.
//!
//! Zebra's `z_gettreestate` is the source of truth for these values. Future
//! node adapters can populate this struct from any equivalent observation.

use zinder_core::{BlockId, ChainTipMetadata, CommitmentTreeFrontiers};

/// One node-observed chain checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceChainCheckpoint {
    /// Exact canonical block at which this checkpoint was observed.
    pub block_id: BlockId,
    /// Checkpoint block timestamp in Unix seconds.
    pub block_time_seconds: u32,
    /// Validated frontiers after applying `block_id`.
    pub frontiers: CommitmentTreeFrontiers,
}

impl SourceChainCheckpoint {
    /// Creates a checkpoint observation.
    #[must_use]
    pub const fn new(
        block_id: BlockId,
        block_time_seconds: u32,
        frontiers: CommitmentTreeFrontiers,
    ) -> Self {
        Self {
            block_id,
            block_time_seconds,
            frontiers,
        }
    }

    /// Derives the commitment-tree sizes represented by this checkpoint.
    #[must_use]
    pub fn tip_metadata(&self) -> ChainTipMetadata {
        self.frontiers.tip_metadata()
    }
}
