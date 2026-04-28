//! Epoch-bound chain artifact reader.

use std::num::NonZeroU32;
use zinder_core::{
    BlockArtifact, BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact,
    SubtreeRootArtifact, SubtreeRootRange, TransactionArtifact, TransactionId,
    TransparentAddressScriptHash, TransparentAddressUtxoArtifact, TreeStateArtifact,
};

use crate::{
    StoreError,
    block_artifact::{
        CompactBlockStore, FinalizedBlockStore, read_block_artifact, read_compact_block_artifact,
        read_compact_block_artifacts,
    },
    kv::RocksChainStoreReadView,
    subtree_root::{SubtreeRootStore, read_subtree_root_artifacts},
    transaction_artifact::{TransactionArtifactStore, read_transaction_artifact},
    transparent_utxo::{TransparentUtxoStore, read_transparent_address_utxos},
    tree_state::{TreeStateStore, read_tree_state_artifact},
};

/// In-process read view pinned to one [`ChainEpoch`].
pub struct ChainEpochReader<'store> {
    chain_epoch: ChainEpoch,
    read_view: RocksChainStoreReadView<'store>,
}

impl<'store> ChainEpochReader<'store> {
    pub(crate) const fn new(
        chain_epoch: ChainEpoch,
        read_view: RocksChainStoreReadView<'store>,
    ) -> Self {
        Self {
            chain_epoch,
            read_view,
        }
    }

    /// Returns the chain epoch this reader is pinned to.
    #[must_use]
    pub const fn chain_epoch(&self) -> ChainEpoch {
        self.chain_epoch
    }

    /// Reads a finalized block artifact by height.
    pub fn block_at(&self, height: BlockHeight) -> Result<Option<BlockArtifact>, StoreError> {
        read_block_artifact(&self.read_view, self.chain_epoch, height)
    }

    /// Reads a compact block artifact by height.
    pub fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CompactBlockArtifact>, StoreError> {
        read_compact_block_artifact(&self.read_view, self.chain_epoch, height)
    }

    /// Reads compact block artifacts in one batched store read.
    pub fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<CompactBlockArtifact>>, StoreError> {
        read_compact_block_artifacts(&self.read_view, self.chain_epoch, block_range)
    }

    /// Reads a transaction artifact by transaction id.
    pub fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionArtifact>, StoreError> {
        read_transaction_artifact(&self.read_view, self.chain_epoch, transaction_id)
    }

    /// Reads a tree-state artifact by height.
    pub fn tree_state_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<TreeStateArtifact>, StoreError> {
        read_tree_state_artifact(&self.read_view, self.chain_epoch, height)
    }

    /// Reads the tree-state artifact at this reader's tip height.
    pub fn latest_tree_state(&self) -> Result<Option<TreeStateArtifact>, StoreError> {
        self.tree_state_at(self.chain_epoch.tip_height)
    }

    /// Reads subtree-root artifacts in ascending subtree-index order.
    pub fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
    ) -> Result<Vec<Option<SubtreeRootArtifact>>, StoreError> {
        read_subtree_root_artifacts(&self.read_view, self.chain_epoch, subtree_root_range)
    }

    /// Reads unspent transparent outputs for an address script hash.
    pub fn transparent_address_utxos(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentAddressUtxoArtifact>, StoreError> {
        read_transparent_address_utxos(
            &self.read_view,
            self.chain_epoch,
            address_script_hash,
            start_height,
            max_entries,
        )
    }
}

impl FinalizedBlockStore for ChainEpochReader<'_> {
    fn block_at(&self, height: BlockHeight) -> Result<Option<BlockArtifact>, StoreError> {
        self.block_at(height)
    }
}

impl CompactBlockStore for ChainEpochReader<'_> {
    fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CompactBlockArtifact>, StoreError> {
        self.compact_block_at(height)
    }
}

impl TransactionArtifactStore for ChainEpochReader<'_> {
    fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionArtifact>, StoreError> {
        self.transaction_by_id(transaction_id)
    }
}

impl TreeStateStore for ChainEpochReader<'_> {
    fn tree_state_at(&self, height: BlockHeight) -> Result<Option<TreeStateArtifact>, StoreError> {
        self.tree_state_at(height)
    }
}

impl SubtreeRootStore for ChainEpochReader<'_> {
    fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
    ) -> Result<Vec<Option<SubtreeRootArtifact>>, StoreError> {
        self.subtree_roots(subtree_root_range)
    }
}

impl TransparentUtxoStore for ChainEpochReader<'_> {
    fn transparent_address_utxos(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentAddressUtxoArtifact>, StoreError> {
        self.transparent_address_utxos(address_script_hash, start_height, max_entries)
    }
}
