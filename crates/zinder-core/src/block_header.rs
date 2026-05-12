//! Typed block-header read model.
//!
//! Returned by [`crate::BlockSelector`]-keyed block-header read paths. The
//! shape is Zinder-native: it does not re-export Zebra's JSON-RPC
//! `getblockheader` object, the lightwalletd compact block header, or any
//! zaino-internal response type.

use crate::{BlockHash, BlockId};

/// Typed block-header read-model value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockHeaderInfo {
    /// Resolved block identity.
    pub block_id: BlockId,
    /// Hash of the previous block.
    pub previous_block_hash: BlockHash,
    /// Merkle root of the transactions in this block.
    pub merkle_root_hash: [u8; 32],
    /// Block-commitment bytes. Interpretation depends on the block height
    /// and network upgrade: `hashFinalSaplingRoot` (Sapling through
    /// pre-Heartwood), `hashChainHistoryRoot` (Heartwood through pre-NU5,
    /// ZIP-221), or `hashBlockCommitments` (NU5 onward, ZIP-244 §3.2).
    /// Callers that need the typed commitment must derive it from the
    /// network and height; this read model surfaces the raw 32-byte field
    /// as written in the block header.
    pub commitment_bytes: [u8; 32],
    /// Block-time as Unix seconds.
    pub block_time: i64,
    /// Compact representation of the difficulty target.
    pub bits: u32,
    /// Block nonce bytes.
    pub nonce: [u8; 32],
    /// Block version field from the header.
    pub version: u32,
}

impl BlockHeaderInfo {
    /// Constructs a typed block-header read-model value from its fields.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "block-header fields are independent values that must all be supplied to construct a complete read-model record"
    )]
    pub const fn new(
        block_id: BlockId,
        previous_block_hash: BlockHash,
        merkle_root_hash: [u8; 32],
        commitment_bytes: [u8; 32],
        block_time: i64,
        bits: u32,
        nonce: [u8; 32],
        version: u32,
    ) -> Self {
        Self {
            block_id,
            previous_block_hash,
            merkle_root_hash,
            commitment_bytes,
            block_time,
            bits,
            nonce,
            version,
        }
    }
}
