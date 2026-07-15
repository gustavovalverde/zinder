//! Durable commitment tree-state artifact values.

use crate::{BlockHash, BlockHeight, BlockId, ChainTipMetadata, ShieldedProtocol};

/// One final note-commitment-tree root in the byte order emitted by Zebra.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FinalNoteCommitmentRoot([u8; 32]);

impl FinalNoteCommitmentRoot {
    /// Creates a final note-commitment root from exactly 32 bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the root bytes in their original Zebra RPC byte order.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// A validated note-commitment-tree frontier returned by Zebra.
///
/// `final_state_bytes` contains the canonical legacy `CommitmentTree`
/// encoding used by `z_gettreestate`. `final_root` preserves Zebra's RPC
/// display byte order, which is reversed for Sapling and direct for Orchard
/// and Ironwood.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommitmentTreeFrontier {
    /// Number of note commitments represented by this frontier.
    tree_size: u32,
    /// Root computed from `final_state_bytes`, in Zebra RPC display order.
    final_root: FinalNoteCommitmentRoot,
    /// Canonical `z_gettreestate` `finalState` bytes.
    final_state_bytes: Box<[u8]>,
}

impl CommitmentTreeFrontier {
    /// Creates a validated frontier value.
    ///
    /// Source adapters must validate the encoding, tree size, and root before
    /// constructing this domain value.
    #[must_use]
    pub fn from_validated_parts(
        tree_size: u32,
        final_root: FinalNoteCommitmentRoot,
        final_state_bytes: impl Into<Box<[u8]>>,
    ) -> Self {
        Self {
            tree_size,
            final_root,
            final_state_bytes: final_state_bytes.into(),
        }
    }

    /// Returns the number of note commitments represented by this frontier.
    #[must_use]
    pub const fn tree_size(&self) -> u32 {
        self.tree_size
    }

    /// Returns the validated root in Zebra RPC display order.
    #[must_use]
    pub const fn final_root(&self) -> FinalNoteCommitmentRoot {
        self.final_root
    }

    /// Returns the canonical `z_gettreestate` `finalState` bytes.
    #[must_use]
    pub fn final_state_bytes(&self) -> &[u8] {
        &self.final_state_bytes
    }
}

/// Validated note-commitment-tree frontiers after one block.
///
/// A pool is absent only before its node-advertised activation height. Once a
/// pool is active, its frontier is present even when the tree is empty.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct CommitmentTreeFrontiers {
    /// Sapling frontier, or `None` before Sapling activation.
    sapling: Option<CommitmentTreeFrontier>,
    /// Orchard frontier, or `None` before NU5 activation.
    orchard: Option<CommitmentTreeFrontier>,
    /// Ironwood frontier, or `None` before NU6.3 activation.
    ironwood: Option<CommitmentTreeFrontier>,
}

impl CommitmentTreeFrontiers {
    /// Creates the frontiers associated with one block.
    #[must_use]
    pub const fn from_validated_parts(
        sapling: Option<CommitmentTreeFrontier>,
        orchard: Option<CommitmentTreeFrontier>,
        ironwood: Option<CommitmentTreeFrontier>,
    ) -> Self {
        Self {
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Returns the frontier for `protocol`, if that pool is active.
    #[must_use]
    pub const fn get(&self, protocol: ShieldedProtocol) -> Option<&CommitmentTreeFrontier> {
        match protocol {
            ShieldedProtocol::Sapling => self.sapling.as_ref(),
            ShieldedProtocol::Orchard => self.orchard.as_ref(),
            ShieldedProtocol::Ironwood => self.ironwood.as_ref(),
        }
    }

    /// Returns the Sapling frontier, if Sapling is active.
    #[must_use]
    pub const fn sapling(&self) -> Option<&CommitmentTreeFrontier> {
        self.sapling.as_ref()
    }

    /// Returns the Orchard frontier, if Orchard is active.
    #[must_use]
    pub const fn orchard(&self) -> Option<&CommitmentTreeFrontier> {
        self.orchard.as_ref()
    }

    /// Returns the Ironwood frontier, if Ironwood is active.
    #[must_use]
    pub const fn ironwood(&self) -> Option<&CommitmentTreeFrontier> {
        self.ironwood.as_ref()
    }

    /// Derives chain-tip metadata from the admitted frontier sizes.
    #[must_use]
    pub fn tip_metadata(&self) -> ChainTipMetadata {
        ChainTipMetadata::new(
            self.sapling
                .as_ref()
                .map_or(0, CommitmentTreeFrontier::tree_size),
            self.orchard
                .as_ref()
                .map_or(0, CommitmentTreeFrontier::tree_size),
            self.ironwood
                .as_ref()
                .map_or(0, CommitmentTreeFrontier::tree_size),
        )
    }

    /// Derives the final roots associated with `block_id`.
    #[must_use]
    pub fn final_note_commitment_roots(&self, block_id: BlockId) -> BlockFinalNoteCommitmentRoots {
        BlockFinalNoteCommitmentRoots::new(
            block_id.height,
            block_id.hash,
            self.sapling
                .as_ref()
                .map(CommitmentTreeFrontier::final_root),
            self.orchard
                .as_ref()
                .map(CommitmentTreeFrontier::final_root),
            self.ironwood
                .as_ref()
                .map(CommitmentTreeFrontier::final_root),
        )
    }
}

/// Final note-commitment-tree roots after one canonical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockFinalNoteCommitmentRoots {
    /// Height of the block that produced these roots.
    pub height: BlockHeight,
    /// Hash of the block that produced these roots.
    pub block_hash: BlockHash,
    /// Final Sapling root, or `None` when unavailable or before activation.
    pub sapling: Option<FinalNoteCommitmentRoot>,
    /// Final Orchard root, or `None` when unavailable or before activation.
    pub orchard: Option<FinalNoteCommitmentRoot>,
    /// Final Ironwood root, or `None` when unavailable or before activation.
    pub ironwood: Option<FinalNoteCommitmentRoot>,
}

impl BlockFinalNoteCommitmentRoots {
    /// Creates the final roots associated with one block.
    #[must_use]
    pub const fn new(
        height: BlockHeight,
        block_hash: BlockHash,
        sapling: Option<FinalNoteCommitmentRoot>,
        orchard: Option<FinalNoteCommitmentRoot>,
        ironwood: Option<FinalNoteCommitmentRoot>,
    ) -> Self {
        Self {
            height,
            block_hash,
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Creates a block value whose roots are all unavailable.
    #[must_use]
    pub const fn unavailable(height: BlockHeight, block_hash: BlockHash) -> Self {
        Self::new(height, block_hash, None, None, None)
    }
}

/// Commitment tree-state artifact for wallet-compatible reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeStateArtifact {
    /// Height the tree state belongs to.
    pub height: BlockHeight,
    /// Hash of the block this tree state belongs to.
    pub block_hash: BlockHash,
    /// Encoded tree-state payload bytes.
    pub payload_bytes: Vec<u8>,
}

impl TreeStateArtifact {
    /// Creates a tree-state artifact.
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
