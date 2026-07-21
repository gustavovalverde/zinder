//! Durable block fact and blob values.

use crate::{BlockHash, BlockHeader, BlockHeight, BlockId, CompactTransactionData};
use thiserror::Error;

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

/// Commitment-tree sizes after one compact block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactChainMetadata {
    /// Sapling note-commitment tree size.
    pub sapling_commitment_tree_size: u32,
    /// Orchard note-commitment tree size.
    pub orchard_commitment_tree_size: u32,
    /// Ironwood note-commitment tree size.
    pub ironwood_commitment_tree_size: u32,
}

/// Wallet-relevant data from one mined transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactTransaction {
    /// Position within the block.
    pub index: u64,
    /// Consensus transaction identifier.
    pub transaction_id: crate::TransactionId,
    /// Wallet scan fields.
    pub data: CompactTransactionData,
}

/// Wallet-oriented structured compact block artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactBlockArtifact {
    /// Height of the source block.
    height: BlockHeight,
    /// Hash of the full block this compact artifact was built from.
    block_hash: BlockHash,
    /// Parent block hash.
    previous_block_hash: BlockHash,
    /// Block time as Unix seconds.
    time: u32,
    /// Wallet-relevant transactions in block order.
    transactions: Vec<CompactTransaction>,
    /// Commitment-tree sizes after this block.
    chain_metadata: CompactChainMetadata,
}

impl CompactBlockArtifact {
    /// Creates an artifact with no wallet-relevant transactions.
    #[must_use]
    pub const fn empty(
        block_id: BlockId,
        previous_block_hash: BlockHash,
        time: u32,
        chain_metadata: CompactChainMetadata,
    ) -> Self {
        Self {
            height: block_id.height,
            block_hash: block_id.hash,
            previous_block_hash,
            time,
            transactions: Vec::new(),
            chain_metadata,
        }
    }

    /// Creates a compact block artifact.
    pub fn new(
        block_id: BlockId,
        previous_block_hash: BlockHash,
        time: u32,
        transactions: Vec<CompactTransaction>,
        chain_metadata: CompactChainMetadata,
    ) -> Result<Self, CompactBlockArtifactError> {
        if transactions
            .windows(2)
            .any(|pair| pair[0].index >= pair[1].index)
        {
            return Err(CompactBlockArtifactError::InvalidTransactionOrder);
        }
        // Finalized artifacts normalize away spare capacity so consumers can
        // account the private transaction allocation from `transactions().len()`.
        let transactions = transactions.into_boxed_slice().into_vec();
        Ok(Self {
            height: block_id.height,
            block_hash: block_id.hash,
            previous_block_hash,
            time,
            transactions,
            chain_metadata,
        })
    }

    /// Returns the block height.
    #[must_use]
    pub const fn height(&self) -> BlockHeight {
        self.height
    }

    /// Returns the block hash.
    #[must_use]
    pub const fn block_hash(&self) -> BlockHash {
        self.block_hash
    }

    /// Returns the parent block hash.
    #[must_use]
    pub const fn previous_block_hash(&self) -> BlockHash {
        self.previous_block_hash
    }

    /// Returns the block time as Unix seconds.
    #[must_use]
    pub const fn time(&self) -> u32 {
        self.time
    }

    /// Returns wallet-relevant transactions in block order.
    #[must_use]
    pub fn transactions(&self) -> &[CompactTransaction] {
        &self.transactions
    }

    /// Returns commitment-tree sizes after this block.
    #[must_use]
    pub const fn chain_metadata(&self) -> CompactChainMetadata {
        self.chain_metadata
    }

    /// Consumes the artifact into its validated constituent fields.
    #[must_use]
    pub fn into_parts(self) -> CompactBlockArtifactParts {
        CompactBlockArtifactParts {
            block_id: BlockId::new(self.height, self.block_hash),
            previous_block_hash: self.previous_block_hash,
            time: self.time,
            transactions: self.transactions,
            chain_metadata: self.chain_metadata,
        }
    }
}

/// Validated constituent fields of a compact block artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactBlockArtifactParts {
    /// Block identity.
    pub block_id: BlockId,
    /// Parent block hash.
    pub previous_block_hash: BlockHash,
    /// Block time as Unix seconds.
    pub time: u32,
    /// Wallet-relevant transactions in block order.
    pub transactions: Vec<CompactTransaction>,
    /// Commitment-tree sizes after the block.
    pub chain_metadata: CompactChainMetadata,
}

/// Invalid structured compact block artifact.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompactBlockArtifactError {
    /// Transaction indexes are duplicated or decrease.
    #[error("compact transaction indexes are not strictly increasing")]
    InvalidTransactionOrder,
}

#[cfg(test)]
mod tests {
    use super::{CompactBlockArtifact, CompactChainMetadata, CompactTransaction};
    use crate::{BlockHash, BlockHeight, BlockId, CompactTransactionData, TransactionId};

    #[test]
    fn compact_block_constructor_discards_transaction_spare_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut transactions = Vec::with_capacity(16);
        transactions.push(CompactTransaction {
            index: 0,
            transaction_id: TransactionId::from_bytes([0x33; 32]),
            data: CompactTransactionData {
                fee_zat: None,
                sapling_spends: Vec::new(),
                sapling_outputs: Vec::new(),
                orchard_actions: Vec::new(),
                ironwood_actions: Vec::new(),
                transparent_inputs: Vec::new(),
                transparent_outputs: Vec::new(),
            },
        });
        assert!(transactions.capacity() > transactions.len());

        let block = CompactBlockArtifact::new(
            BlockId::new(BlockHeight::new(7), BlockHash::from_bytes([0x11; 32])),
            BlockHash::from_bytes([0x22; 32]),
            1_700_000_000,
            transactions,
            CompactChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
                ironwood_commitment_tree_size: 0,
            },
        )?;

        assert_eq!(block.transactions.capacity(), block.transactions.len());
        Ok(())
    }
}
