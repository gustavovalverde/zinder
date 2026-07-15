//! Chain epoch commit values.

use zinder_core::{
    BlockBlobArtifact, BlockFinalNoteCommitmentRoots, BlockHeaderArtifact, BlockHeight,
    BlockHeightRange, BlockTransactionIndexArtifact, BlockValuePoolBalances,
    CanonicalBlockReplayEnvelope, ChainEpoch, CompactBlockArtifact, SubtreeRootArtifact,
    TransactionBlobArtifact, TransactionFactsArtifact, TransactionIntrinsicValueBalancesArtifact,
    TransactionLocation, TransparentOutputArtifact, TransparentSpendFact, TreeStateArtifact,
};

/// Complete artifact set committed as one visible chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEpochArtifacts {
    /// Chain epoch made visible by this commit.
    pub chain_epoch: ChainEpoch,
    /// Block-header facts included in this commit.
    pub block_headers: Vec<BlockHeaderArtifact>,
    /// Canonical replay envelopes for every committed block header, in
    /// the same order as `block_headers`.
    pub block_replay_envelopes: Vec<CanonicalBlockReplayEnvelope>,
    /// Optional raw block blobs included in this commit.
    pub block_blobs: Vec<BlockBlobArtifact>,
    /// Compact block artifacts included in this commit.
    pub compact_blocks: Vec<CompactBlockArtifact>,
    /// Block-local transaction id index rows included in this commit.
    pub block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    /// Transaction-location rows included in this commit.
    pub transaction_locations: Vec<TransactionLocation>,
    /// Transaction fact rows included in this commit.
    pub transaction_facts: Vec<TransactionFactsArtifact>,
    /// Transaction-intrinsic value-balance artifacts included in this commit.
    pub transaction_intrinsic_value_balances: Vec<TransactionIntrinsicValueBalancesArtifact>,
    /// Optional raw transaction blobs included in this commit.
    pub transaction_blobs: Vec<TransactionBlobArtifact>,
    /// Tree-state artifacts included in this commit.
    pub tree_states: Vec<TreeStateArtifact>,
    /// Final note-commitment roots included in this commit.
    pub final_note_commitment_roots: Vec<BlockFinalNoteCommitmentRoots>,
    /// Optional cumulative value-pool balances included in this commit.
    pub block_value_pool_balances: Vec<BlockValuePoolBalances>,
    /// Subtree-root artifacts included in this commit.
    pub subtree_roots: Vec<SubtreeRootArtifact>,
    /// Transparent output artifacts included in this commit. The store
    /// derives the address-output projection rows from these at commit.
    pub transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
    /// Resolved transparent spend facts included in this commit.
    pub transparent_spend_facts: Vec<TransparentSpendFact>,
    /// Reorg-window transition included in this commit.
    pub reorg_window_change: ReorgWindowChange,
}

impl ChainEpochArtifacts {
    /// Creates a commit value with required block replay and no other
    /// transaction or tree-state rows.
    #[must_use]
    pub fn new(
        chain_epoch: ChainEpoch,
        block_headers: Vec<BlockHeaderArtifact>,
        block_replay_envelopes: Vec<CanonicalBlockReplayEnvelope>,
        compact_blocks: Vec<CompactBlockArtifact>,
    ) -> Self {
        Self {
            chain_epoch,
            block_headers,
            block_replay_envelopes,
            block_blobs: Vec::new(),
            compact_blocks,
            block_transaction_index: Vec::new(),
            transaction_locations: Vec::new(),
            transaction_facts: Vec::new(),
            transaction_intrinsic_value_balances: Vec::new(),
            transaction_blobs: Vec::new(),
            tree_states: Vec::new(),
            final_note_commitment_roots: Vec::new(),
            block_value_pool_balances: Vec::new(),
            subtree_roots: Vec::new(),
            transparent_outputs_by_outpoint: Vec::new(),
            transparent_spend_facts: Vec::new(),
            reorg_window_change: ReorgWindowChange::Unchanged,
        }
    }

    /// Adds optional raw block blobs to this commit value.
    #[must_use]
    pub fn with_block_blobs(mut self, block_blobs: Vec<BlockBlobArtifact>) -> Self {
        self.block_blobs = block_blobs;
        self
    }

    /// Adds block-local transaction id index rows to this commit value.
    #[must_use]
    pub fn with_block_transaction_index(
        mut self,
        block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    ) -> Self {
        self.block_transaction_index = block_transaction_index;
        self
    }

    /// Adds transaction locations to this commit value.
    #[must_use]
    pub fn with_transaction_locations(
        mut self,
        transaction_locations: Vec<TransactionLocation>,
    ) -> Self {
        self.transaction_locations = transaction_locations;
        self
    }

    /// Adds transaction facts to this commit value.
    #[must_use]
    pub fn with_transaction_facts(
        mut self,
        transaction_facts: Vec<TransactionFactsArtifact>,
    ) -> Self {
        self.transaction_facts = transaction_facts;
        self
    }

    /// Adds transaction-intrinsic value balances to this commit value.
    #[must_use]
    pub fn with_transaction_intrinsic_value_balances(
        mut self,
        transaction_intrinsic_value_balances: Vec<TransactionIntrinsicValueBalancesArtifact>,
    ) -> Self {
        self.transaction_intrinsic_value_balances = transaction_intrinsic_value_balances;
        self
    }

    /// Adds optional raw transaction blobs to this commit value.
    #[must_use]
    pub fn with_transaction_blobs(
        mut self,
        transaction_blobs: Vec<TransactionBlobArtifact>,
    ) -> Self {
        self.transaction_blobs = transaction_blobs;
        self
    }

    /// Adds tree-state artifacts to this commit value.
    #[must_use]
    pub fn with_tree_states(mut self, tree_states: Vec<TreeStateArtifact>) -> Self {
        self.tree_states = tree_states;
        self
    }

    /// Adds final note-commitment roots to this commit value.
    #[must_use]
    pub fn with_final_note_commitment_roots(
        mut self,
        final_note_commitment_roots: Vec<BlockFinalNoteCommitmentRoots>,
    ) -> Self {
        self.final_note_commitment_roots = final_note_commitment_roots;
        self
    }

    /// Adds cumulative block value-pool balances to this commit value.
    #[must_use]
    pub fn with_block_value_pool_balances(
        mut self,
        block_value_pool_balances: Vec<BlockValuePoolBalances>,
    ) -> Self {
        self.block_value_pool_balances = block_value_pool_balances;
        self
    }

    /// Adds subtree-root artifacts to this commit value.
    #[must_use]
    pub fn with_subtree_roots(mut self, subtree_roots: Vec<SubtreeRootArtifact>) -> Self {
        self.subtree_roots = subtree_roots;
        self
    }

    /// Adds transparent output artifacts to this commit value.
    #[must_use]
    pub fn with_transparent_outputs_by_outpoint(
        mut self,
        transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
    ) -> Self {
        self.transparent_outputs_by_outpoint = transparent_outputs_by_outpoint;
        self
    }

    /// Adds resolved transparent spend facts to this commit value.
    #[must_use]
    pub fn with_transparent_spend_facts(
        mut self,
        transparent_spend_facts: Vec<TransparentSpendFact>,
    ) -> Self {
        self.transparent_spend_facts = transparent_spend_facts;
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
    /// Advance the safe-tip boundary through this height; the prefix below
    /// is past the reorg window and no longer subject to local rollback.
    AdvanceSafeTipTo {
        /// Safe tip height after this commit.
        height: BlockHeight,
    },
    /// This commit does not mutate the reorg window.
    Unchanged,
}
