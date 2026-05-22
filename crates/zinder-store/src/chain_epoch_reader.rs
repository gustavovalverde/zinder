//! Epoch-bound chain artifact reader.

use std::collections::HashMap;
use std::num::NonZeroU32;
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact,
    SubtreeRootArtifact, SubtreeRootRange, TransactionArtifact, TransactionId,
    TransparentAddressScriptHash, TransparentAddressUtxoArtifact, TransparentOutPoint,
    TransparentPrevoutArtifact, TreeStateArtifact,
};

use crate::{
    StoreError,
    block_artifact::{
        CompactBlockStore, FinalizedBlockStore, read_block_artifact, read_block_artifacts,
        read_compact_block_artifact, read_compact_block_artifacts,
    },
    block_hash_index::{BlockHashLookup, read_block_hash_lookup},
    kv::RocksChainStoreReadView,
    subtree_root::{SubtreeRootStore, read_subtree_root_artifacts},
    transaction_artifact::{
        TransactionArtifactStore, read_transaction_artifact, read_transaction_artifacts_batch,
    },
    transparent_prevout::{
        read_current_transparent_prevouts_by_outpoints,
        read_historical_transparent_prevouts_by_outpoints,
    },
    transparent_utxo::{TransparentUtxoStore, read_transparent_address_utxos},
    tree_state::{TreeStateStore, read_tree_state_artifact},
};

/// In-process read view pinned to one [`ChainEpoch`].
pub struct ChainEpochReader<'store> {
    chain_epoch: ChainEpoch,
    read_view: RocksChainStoreReadView<'store>,
    is_current: bool,
}

impl<'store> ChainEpochReader<'store> {
    pub(crate) const fn current(
        chain_epoch: ChainEpoch,
        read_view: RocksChainStoreReadView<'store>,
    ) -> Self {
        Self {
            chain_epoch,
            read_view,
            is_current: true,
        }
    }

    pub(crate) const fn at_epoch(
        chain_epoch: ChainEpoch,
        read_view: RocksChainStoreReadView<'store>,
    ) -> Self {
        Self {
            chain_epoch,
            read_view,
            is_current: false,
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

    /// Reads finalized block artifacts in one batched store read.
    pub fn blocks_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockArtifact>>, StoreError> {
        read_block_artifacts(&self.read_view, self.chain_epoch, block_range)
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

    /// Reads transaction artifacts for many ids in one batched store read.
    pub fn transactions_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionArtifact>>, StoreError> {
        read_transaction_artifacts_batch(&self.read_view, self.chain_epoch, transaction_ids)
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

    /// Resolves a block hash through the canonical best-chain index.
    pub fn block_hash_lookup(&self, block_hash: BlockHash) -> Result<BlockHashLookup, StoreError> {
        read_block_hash_lookup(&self.read_view, self.chain_epoch, block_hash)
    }

    /// Resolves transparent prevout artifacts by outpoint.
    ///
    /// This does not filter out spent rows: prevout resolution needs the
    /// original value and script after the output has been spent. Current
    /// readers use the exact current projection; pinned historical readers
    /// scan epoch-suffixed history and verify producing-block visibility.
    pub fn transparent_prevouts_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>, StoreError> {
        if self.is_current {
            return read_current_transparent_prevouts_by_outpoints(
                &self.read_view,
                self.chain_epoch,
                outpoints,
            );
        }
        read_historical_transparent_prevouts_by_outpoints(
            &self.read_view,
            self.chain_epoch,
            outpoints,
        )
    }

    /// Resolves transparent prevout artifacts by outpoint on the primary writer's
    /// commit path.
    ///
    /// This intentionally skips external-reader visibility and spend-state
    /// filtering. Use it only while the writer is deriving a node-validated
    /// batch against its own current epoch.
    pub fn transparent_prevouts_by_outpoints_for_writer_commit(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>, StoreError> {
        read_current_transparent_prevouts_by_outpoints(&self.read_view, self.chain_epoch, outpoints)
    }
}

impl FinalizedBlockStore for ChainEpochReader<'_> {
    fn block_at(&self, height: BlockHeight) -> Result<Option<BlockArtifact>, StoreError> {
        self.block_at(height)
    }

    fn blocks_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockArtifact>>, StoreError> {
        self.blocks_in_range(block_range)
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

    fn transactions_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionArtifact>>, StoreError> {
        self.transactions_by_ids(transaction_ids)
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
