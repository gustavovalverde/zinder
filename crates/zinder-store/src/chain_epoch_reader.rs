//! Epoch-bound chain artifact reader.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use zinder_core::{
    BlockBlobArtifact, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, ChainEpoch,
    CompactBlockArtifact, SubtreeRootArtifact, SubtreeRootRange, TransactionBlobArtifact,
    TransactionFactsArtifact, TransactionId, TransactionLocation, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentOutputArtifact, TransparentOutputEntry, TransparentSpendFact,
    TransparentUnspentOutput, TransparentUtxoSetSummary, TreeStateArtifact,
};

use crate::{
    StoreError,
    address_output_index::{
        AddressOutputIndexStore, read_address_output_index, read_transparent_utxo_set_aggregate,
    },
    block_artifact::{
        BlockBlobStore, BlockHeaderStore, BlockTransactionIndexStore, CompactBlockStore,
        read_block_blob_artifact, read_block_blob_artifacts, read_block_header_artifact,
        read_block_header_artifacts, read_block_transaction_index_artifact,
        read_block_transaction_index_artifacts_at_height, read_compact_block_artifact,
        read_compact_block_artifacts,
    },
    block_hash_index::{BlockHashLookup, read_block_hash_lookup},
    kv::RocksChainStoreReadView,
    subtree_root::{SubtreeRootStore, read_subtree_root_artifacts},
    transaction_artifact::{
        TransactionBlobStore, TransactionFactsStore, TransactionLocationStore,
        read_transaction_blob_artifact, read_transaction_facts_artifact,
        read_transaction_facts_artifacts_batch, read_transaction_location,
    },
    transparent_output::{
        read_current_transparent_outputs_by_outpoints,
        read_visible_transparent_outputs_by_outpoints,
    },
    transparent_spend_fact::{
        read_current_transparent_spend_facts_by_outpoints,
        read_visible_transparent_spend_facts_by_outpoints,
    },
    tree_state::{TreeStateStore, read_tree_state_checkpoint_at_or_before},
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

    /// Reads block-header facts by height.
    pub fn block_header_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHeaderArtifact>, StoreError> {
        read_block_header_artifact(&self.read_view, self.chain_epoch, height)
    }

    /// Reads block-header facts in one batched store read.
    pub fn block_headers_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockHeaderArtifact>>, StoreError> {
        read_block_header_artifacts(&self.read_view, self.chain_epoch, block_range)
    }

    /// Reads an optional raw block blob by height.
    pub fn block_blob_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockBlobArtifact>, StoreError> {
        read_block_blob_artifact(&self.read_view, self.chain_epoch, height)
    }

    /// Reads optional raw block blobs in one batched store read.
    pub fn block_blobs_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockBlobArtifact>>, StoreError> {
        read_block_blob_artifacts(&self.read_view, self.chain_epoch, block_range)
    }

    /// Reads the transaction id at a block-local index.
    pub fn transaction_id_at_block_index(
        &self,
        height: BlockHeight,
        tx_index_in_block: u32,
    ) -> Result<Option<TransactionId>, StoreError> {
        Ok(read_block_transaction_index_artifact(
            &self.read_view,
            self.chain_epoch,
            height,
            tx_index_in_block,
        )?
        .map(|artifact| artifact.transaction_id))
    }

    /// Reads the ordered transaction ids for every transaction in a block.
    pub fn transaction_ids_at_height(
        &self,
        height: BlockHeight,
    ) -> Result<Vec<TransactionId>, StoreError> {
        read_block_transaction_index_artifacts_at_height(&self.read_view, self.chain_epoch, height)
            .map(|artifacts| {
                artifacts
                    .into_iter()
                    .map(|artifact| artifact.transaction_id)
                    .collect()
            })
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

    /// Reads transaction facts by transaction id.
    pub fn transaction_facts_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionFactsArtifact>, StoreError> {
        read_transaction_facts_artifact(&self.read_view, self.chain_epoch, transaction_id)
    }

    /// Reads transaction facts for many ids in one batched store read.
    pub fn transaction_facts_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, StoreError> {
        read_transaction_facts_artifacts_batch(&self.read_view, self.chain_epoch, transaction_ids)
    }

    /// Reads a transaction location by transaction id.
    pub fn transaction_location_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionLocation>, StoreError> {
        read_transaction_location(&self.read_view, self.chain_epoch, transaction_id)
    }

    /// Reads an optional raw transaction blob by transaction id.
    pub fn transaction_blob_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionBlobArtifact>, StoreError> {
        read_transaction_blob_artifact(&self.read_view, self.chain_epoch, transaction_id)
    }

    /// Reads a checkpoint tree-state artifact at or before `max_height`.
    pub fn tree_state_checkpoint_at_or_before(
        &self,
        max_height: BlockHeight,
    ) -> Result<Option<TreeStateArtifact>, StoreError> {
        read_tree_state_checkpoint_at_or_before(&self.read_view, self.chain_epoch, max_height)
    }

    /// Reads the latest checkpoint tree-state artifact visible to this reader.
    pub fn latest_tree_state_checkpoint(&self) -> Result<Option<TreeStateArtifact>, StoreError> {
        self.tree_state_checkpoint_at_or_before(self.chain_epoch.visible_tip_height)
    }

    /// Reads subtree-root artifacts in ascending subtree-index order.
    pub fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
    ) -> Result<Vec<Option<SubtreeRootArtifact>>, StoreError> {
        read_subtree_root_artifacts(&self.read_view, self.chain_epoch, subtree_root_range)
    }

    /// Reads unspent transparent outputs for an address script hash.
    pub fn address_output_index(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentUnspentOutput>, StoreError> {
        read_address_output_index(
            &self.read_view,
            self.chain_epoch,
            address_script_hash,
            start_height,
            max_entries,
        )
    }

    /// Aggregates the chain-wide transparent UTXO set at this epoch's settled tip.
    ///
    /// Streams the whole current-UTXO projection and folds it into an unspent
    /// count and total value over the outputs created at or below
    /// `settled_tip_height`, where the projection is the irreversible unspent
    /// set. Request-time, full-set scan with constant memory.
    pub fn transparent_utxo_set_summary(&self) -> Result<TransparentUtxoSetSummary, StoreError> {
        let aggregate = read_transparent_utxo_set_aggregate(&self.read_view, self.chain_epoch)?;
        Ok(TransparentUtxoSetSummary {
            utxo_count: aggregate.utxo_count,
            total_value_zat: aggregate.total_value_zat,
            summarized_height: self.chain_epoch.settled_tip_height,
            chain_epoch: self.chain_epoch,
        })
    }

    /// Resolves a block hash through the canonical best-chain index.
    pub fn block_hash_lookup(&self, block_hash: BlockHash) -> Result<BlockHashLookup, StoreError> {
        read_block_hash_lookup(&self.read_view, self.chain_epoch, block_hash)
    }

    /// Resolves transparent output artifacts by outpoint.
    ///
    /// This does not filter out spent rows: prevout resolution needs the
    /// original value and script after the output has been spent. Current
    /// readers use the exact current projection; pinned historical readers
    /// use the same canonical rows and verify producing-block visibility.
    pub fn transparent_outputs_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, StoreError> {
        if self.is_current {
            return read_current_transparent_outputs_by_outpoints(
                &self.read_view,
                self.chain_epoch,
                outpoints,
            );
        }
        read_visible_transparent_outputs_by_outpoints(&self.read_view, self.chain_epoch, outpoints)
    }

    /// Resolves transparent output artifacts by outpoint on the primary writer's
    /// commit path.
    ///
    /// This intentionally skips external-reader visibility and spend-state
    /// filtering. Use it only while the writer is deriving a node-validated
    /// batch against its own current epoch.
    pub fn transparent_outputs_by_outpoints_for_writer_commit(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, StoreError> {
        read_current_transparent_outputs_by_outpoints(&self.read_view, self.chain_epoch, outpoints)
    }

    /// Resolves transparent spend facts by spent outpoint.
    pub fn transparent_spend_facts_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, StoreError> {
        if self.is_current {
            return read_current_transparent_spend_facts_by_outpoints(
                &self.read_view,
                self.chain_epoch,
                outpoints,
            );
        }
        read_visible_transparent_spend_facts_by_outpoints(
            &self.read_view,
            self.chain_epoch,
            outpoints,
        )
    }

    /// Resolves the unspent transparent outputs among `outpoints` at this
    /// epoch.
    ///
    /// An outpoint is kept only when its output is present and carries no
    /// canonical spend fact at the pinned epoch. Duplicate input outpoints
    /// yield at most one entry, preserving first-seen order. Each kept entry's
    /// `output` is always populated.
    pub fn transparent_unspent_outputs_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<Vec<TransparentOutputEntry>, StoreError> {
        let outputs_by_outpoint = self.transparent_outputs_by_outpoints(outpoints)?;
        let spends_by_outpoint = self.transparent_spend_facts_by_outpoints(outpoints)?;

        let mut entries = Vec::with_capacity(outputs_by_outpoint.len());
        let mut seen = HashSet::with_capacity(outputs_by_outpoint.len());
        for outpoint in outpoints {
            if spends_by_outpoint.contains_key(outpoint) {
                continue;
            }
            if let Some(output) = outputs_by_outpoint.get(outpoint)
                && seen.insert(*outpoint)
            {
                entries.push(TransparentOutputEntry {
                    outpoint: *outpoint,
                    output: Some(output.clone().into_output()),
                });
            }
        }
        Ok(entries)
    }

    /// Resolves transparent spend facts from the current projection, skipping
    /// the per-outpoint reorg-visibility header reads.
    ///
    /// Correct only when every referenced block is finalized (at or below
    /// `settled_tip_height`): such blocks are immutable, so the visibility filter
    /// that [`Self::transparent_spend_facts_by_outpoints`] applies on a
    /// non-current reader can never drop a fact. Skipping it turns two reads per
    /// outpoint into a single `multi_get`, which is the dominant cost of
    /// from-genesis derive replay.
    pub fn current_transparent_spend_facts_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, StoreError> {
        read_current_transparent_spend_facts_by_outpoints(
            &self.read_view,
            self.chain_epoch,
            outpoints,
        )
    }
}

impl BlockHeaderStore for ChainEpochReader<'_> {
    fn block_header_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHeaderArtifact>, StoreError> {
        self.block_header_at(height)
    }

    fn block_headers_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockHeaderArtifact>>, StoreError> {
        self.block_headers_in_range(block_range)
    }
}

impl BlockBlobStore for ChainEpochReader<'_> {
    fn block_blob_at(&self, height: BlockHeight) -> Result<Option<BlockBlobArtifact>, StoreError> {
        self.block_blob_at(height)
    }
}

impl BlockTransactionIndexStore for ChainEpochReader<'_> {
    fn transaction_id_at_block_index(
        &self,
        height: BlockHeight,
        tx_index_in_block: u32,
    ) -> Result<Option<TransactionId>, StoreError> {
        self.transaction_id_at_block_index(height, tx_index_in_block)
    }

    fn transaction_ids_at_height(
        &self,
        height: BlockHeight,
    ) -> Result<Vec<TransactionId>, StoreError> {
        self.transaction_ids_at_height(height)
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

impl TransactionFactsStore for ChainEpochReader<'_> {
    fn transaction_facts_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionFactsArtifact>, StoreError> {
        self.transaction_facts_by_id(transaction_id)
    }

    fn transaction_facts_by_ids(
        &self,
        transaction_ids: &[TransactionId],
    ) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, StoreError> {
        self.transaction_facts_by_ids(transaction_ids)
    }
}

impl TransactionLocationStore for ChainEpochReader<'_> {
    fn transaction_location_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionLocation>, StoreError> {
        self.transaction_location_by_id(transaction_id)
    }
}

impl TransactionBlobStore for ChainEpochReader<'_> {
    fn transaction_blob_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionBlobArtifact>, StoreError> {
        self.transaction_blob_by_id(transaction_id)
    }
}

impl TreeStateStore for ChainEpochReader<'_> {
    fn tree_state_checkpoint_at_or_before(
        &self,
        max_height: BlockHeight,
    ) -> Result<Option<TreeStateArtifact>, StoreError> {
        self.tree_state_checkpoint_at_or_before(max_height)
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

impl AddressOutputIndexStore for ChainEpochReader<'_> {
    fn address_output_index(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentUnspentOutput>, StoreError> {
        self.address_output_index(address_script_hash, start_height, max_entries)
    }
}
