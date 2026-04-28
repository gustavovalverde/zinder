//! Chain epoch commit values.

use zinder_core::{
    BlockArtifact, BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact,
    SubtreeRootArtifact, TransactionArtifact, TransparentAddressUtxoArtifact,
    TransparentUtxoSpendArtifact, TreeStateArtifact,
};

/// Complete artifact set committed as one visible chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEpochArtifacts {
    /// Chain epoch made visible by this commit.
    pub chain_epoch: ChainEpoch,
    /// Finalized block artifacts included in this commit.
    pub finalized_blocks: Vec<BlockArtifact>,
    /// Compact block artifacts included in this commit.
    pub compact_blocks: Vec<CompactBlockArtifact>,
    /// Transaction artifacts included in this commit.
    pub transactions: Vec<TransactionArtifact>,
    /// Tree-state artifacts included in this commit.
    pub tree_states: Vec<TreeStateArtifact>,
    /// Subtree-root artifacts included in this commit.
    pub subtree_roots: Vec<SubtreeRootArtifact>,
    /// Transparent address UTXO artifacts included in this commit.
    pub transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    /// Transparent spend artifacts included in this commit.
    pub transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    /// Reorg-window transition included in this commit.
    pub reorg_window_change: ReorgWindowChange,
}

impl ChainEpochArtifacts {
    /// Creates a chain epoch commit value with no transaction or tree-state artifacts.
    #[must_use]
    pub fn new(
        chain_epoch: ChainEpoch,
        finalized_blocks: Vec<BlockArtifact>,
        compact_blocks: Vec<CompactBlockArtifact>,
    ) -> Self {
        Self {
            chain_epoch,
            finalized_blocks,
            compact_blocks,
            transactions: Vec::new(),
            tree_states: Vec::new(),
            subtree_roots: Vec::new(),
            transparent_address_utxos: Vec::new(),
            transparent_utxo_spends: Vec::new(),
            reorg_window_change: ReorgWindowChange::Unchanged,
        }
    }

    /// Adds transaction artifacts to this commit value.
    #[must_use]
    pub fn with_transactions(mut self, transactions: Vec<TransactionArtifact>) -> Self {
        self.transactions = transactions;
        self
    }

    /// Adds tree-state artifacts to this commit value.
    #[must_use]
    pub fn with_tree_states(mut self, tree_states: Vec<TreeStateArtifact>) -> Self {
        self.tree_states = tree_states;
        self
    }

    /// Adds subtree-root artifacts to this commit value.
    #[must_use]
    pub fn with_subtree_roots(mut self, subtree_roots: Vec<SubtreeRootArtifact>) -> Self {
        self.subtree_roots = subtree_roots;
        self
    }

    /// Adds transparent address UTXO artifacts to this commit value.
    #[must_use]
    pub fn with_transparent_address_utxos(
        mut self,
        transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    ) -> Self {
        self.transparent_address_utxos = transparent_address_utxos;
        self
    }

    /// Adds transparent spend artifacts to this commit value.
    #[must_use]
    pub fn with_transparent_utxo_spends(
        mut self,
        transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    ) -> Self {
        self.transparent_utxo_spends = transparent_utxo_spends;
        self
    }

    /// Sets the reorg-window transition for this commit value.
    #[must_use]
    pub fn with_reorg_window_change(mut self, reorg_window_change: ReorgWindowChange) -> Self {
        self.reorg_window_change = reorg_window_change;
        self
    }
}

/// Reorg-window transition represented by a chain epoch commit.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReorgWindowChange {
    /// Append artifacts to the current replaceable chain segment.
    Extend {
        /// Inclusive range added to the replaceable chain segment.
        block_range: BlockHeightRange,
    },
    /// Replace the current branch from the first divergent height.
    Replace {
        /// First height where the previous visible branch is invalidated.
        from_height: BlockHeight,
    },
    /// Advance the finalized prefix through this height.
    FinalizeThrough {
        /// Finalized height after this commit.
        height: BlockHeight,
    },
    /// This commit does not mutate the reorg window.
    Unchanged,
}
