//! Durable facts for blocks displaced from the canonical branch.

use crate::{
    BlockFinalNoteCommitmentRoots, BlockHash, BlockHeaderArtifact, BlockId, ChainEpochId,
    FinalNoteCommitmentRoot, ShieldedProtocol, TransactionId, UnixTimestampMillis,
};

/// Transparent coinbase payout retained with a displaced block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplacedBlockCoinbaseOutput {
    /// Output index within the coinbase transaction.
    pub output_index: u32,
    /// Output value in zatoshis.
    pub value_zat: u64,
    /// Raw transparent `scriptPubKey` bytes used to derive the payout address.
    pub script_pub_key: Vec<u8>,
}

impl DisplacedBlockCoinbaseOutput {
    /// Creates a retained transparent coinbase payout.
    #[must_use]
    pub fn new(output_index: u32, value_zat: u64, script_pub_key: impl Into<Vec<u8>>) -> Self {
        Self {
            output_index,
            value_zat,
            script_pub_key: script_pub_key.into(),
        }
    }
}

/// Product-neutral facts retained when a canonical block is displaced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplacedBlock {
    /// Stable identity of the displaced block.
    pub block_hash: BlockHash,
    /// Complete header facts visible immediately before displacement.
    pub header: BlockHeaderArtifact,
    /// Transaction identifiers in block order.
    pub transaction_ids: Vec<TransactionId>,
    /// Transparent payout outputs from the coinbase transaction, in output order.
    pub coinbase_outputs: Vec<DisplacedBlockCoinbaseOutput>,
    /// Serialized consensus block bytes when raw block retention was enabled.
    pub raw_block_bytes: Option<Vec<u8>>,
    /// Final post-block roots captured from the canonical artifact before displacement.
    ///
    /// `None` means the enclosing artifact was unavailable. When present,
    /// individual protocol roots can still be absent before activation.
    pub final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
    /// Chain event that displaced this block.
    pub displacement_event_sequence: u64,
    /// Epoch made visible by the displacement event.
    pub displacement_epoch: ChainEpochId,
    /// Wall-clock time of the displacement epoch.
    pub displaced_at: UnixTimestampMillis,
}

/// Coverage boundary for the additive displaced-block archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplacedBlockArchiveCoverage {
    /// First chain event for which replacement capture is guaranteed.
    pub activation_event_sequence: u64,
    /// Epoch made visible by the activation event.
    pub activation_epoch: ChainEpochId,
    /// Wall-clock time when archive coverage became active.
    pub activated_at: UnixTimestampMillis,
}

/// One writer-owned reverse-index candidate for a displaced final root.
///
/// Callers must compare [`Self::block_id`] with a pinned canonical block at
/// the same height before presenting the candidate as non-canonical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplacedRootCandidate {
    /// Stable identity of the block that carried the root.
    pub block_id: BlockId,
    /// Shielded protocol whose post-block root matched.
    pub protocol: ShieldedProtocol,
    /// Final post-block root captured before displacement.
    pub root: FinalNoteCommitmentRoot,
    /// Chain event that displaced this occurrence of the block.
    pub displacement_event_sequence: u64,
    /// Epoch made visible by the displacement event.
    pub displacement_epoch: ChainEpochId,
    /// Original block timestamp as Unix seconds.
    pub block_time_unix_seconds: i64,
}

/// Coverage boundary for displaced final-root capture and reverse indexing.
///
/// Coverage includes the activation event itself. Archive rows written before
/// activation remain unknown and are excluded from both counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplacedRootArchiveCoverage {
    /// First displacement event covered by root capture and reverse indexing.
    pub activation_event_sequence: u64,
    /// Epoch made visible by the activation event.
    pub activation_epoch: ChainEpochId,
    /// Wall-clock time when displaced-root coverage became active.
    pub activated_at: UnixTimestampMillis,
    /// Number of displaced blocks examined at or after activation.
    pub captured_block_count: u64,
    /// Covered blocks whose canonical final-root artifact was unavailable.
    pub root_artifact_unavailable_count: u64,
}
