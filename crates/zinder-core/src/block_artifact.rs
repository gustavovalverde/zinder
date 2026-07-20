//! Durable block fact and blob values.

use crate::{BlockHash, BlockHeader, BlockHeight};

/// Canonical block-header facts indexed by block height.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockHeaderArtifact {
    /// Height of the source block.
    pub height: BlockHeight,
    /// Hash of the source block.
    pub block_hash: BlockHash,
    /// Parent hash of the source block.
    pub parent_hash: BlockHash,
    /// Merkle root of the transactions in this block.
    pub merkle_root_hash: [u8; 32],
    /// Block commitment bytes.
    pub commitment_bytes: [u8; 32],
    /// Block-time as Unix seconds.
    pub block_time: i64,
    /// Compact difficulty target.
    pub bits: u32,
    /// Block nonce bytes.
    pub nonce: [u8; 32],
    /// Block version field.
    pub version: u32,
    /// Serialized consensus block size in bytes.
    pub block_size_bytes: u64,
}

impl BlockHeaderArtifact {
    /// Creates canonical block-header facts.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "block-header facts mirror the durable read-model fields"
    )]
    pub fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
        merkle_root_hash: [u8; 32],
        commitment_bytes: [u8; 32],
        block_time: i64,
        bits: u32,
        nonce: [u8; 32],
        version: u32,
        block_size_bytes: u64,
    ) -> Self {
        Self {
            height,
            block_hash,
            parent_hash,
            merkle_root_hash,
            commitment_bytes,
            block_time,
            bits,
            nonce,
            version,
            block_size_bytes,
        }
    }

    /// Creates canonical block-header facts from the public read model.
    #[must_use]
    pub const fn from_header(header: BlockHeader) -> Self {
        Self::from_header_with_block_size(header, 0)
    }

    /// Creates canonical block-header facts from the public read model and
    /// the serialized source block size.
    #[must_use]
    pub const fn from_header_with_block_size(header: BlockHeader, block_size_bytes: u64) -> Self {
        Self {
            height: header.block_id.height,
            block_hash: header.block_id.hash,
            parent_hash: header.previous_block_hash,
            merkle_root_hash: header.merkle_root_hash,
            commitment_bytes: header.commitment_bytes,
            block_time: header.block_time,
            bits: header.bits,
            nonce: header.nonce,
            version: header.version,
            block_size_bytes,
        }
    }

    /// Converts the persisted facts into the public header read model.
    #[must_use]
    pub const fn into_header(self) -> BlockHeader {
        BlockHeader::new(
            crate::BlockId::new(self.height, self.block_hash),
            self.parent_hash,
            self.merkle_root_hash,
            self.commitment_bytes,
            self.block_time,
            self.bits,
            self.nonce,
            self.version,
        )
    }
}

/// Optional cold-path raw block blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockBlobArtifact {
    /// Height of the source block.
    pub height: BlockHeight,
    /// Hash of the source block.
    pub block_hash: BlockHash,
    /// Parent hash of the source block.
    pub parent_hash: BlockHash,
    /// Serialized consensus block bytes.
    pub raw_block_bytes: Vec<u8>,
}

impl BlockBlobArtifact {
    /// Creates a raw block blob.
    #[must_use]
    pub fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
        raw_block_bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            height,
            block_hash,
            parent_hash,
            raw_block_bytes: raw_block_bytes.into(),
        }
    }
}

/// Transaction identifier at a block-local index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockTransactionIndexArtifact {
    /// Height of the containing block.
    pub block_height: BlockHeight,
    /// Block-local transaction index.
    pub tx_index_in_block: u32,
    /// Transaction identifier at this location.
    pub transaction_id: crate::TransactionId,
    /// Hash of the containing block.
    pub block_hash: BlockHash,
}

impl BlockTransactionIndexArtifact {
    /// Creates a block transaction-index row.
    #[must_use]
    pub const fn new(
        block_height: BlockHeight,
        tx_index_in_block: u32,
        transaction_id: crate::TransactionId,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            block_height,
            tx_index_in_block,
            transaction_id,
            block_hash,
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
