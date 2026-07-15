//! Durable commitment tree-state artifact values.

use std::io::Cursor;

use incrementalmerkletree::{
    Hashable, Position,
    frontier::{CommitmentTree, Frontier},
};
use orchard::tree::MerkleHashOrchard;
use sapling::Node as SaplingNode;
use thiserror::Error;
use zcash_primitives::merkle_tree::{HashSer, read_commitment_tree, write_commitment_tree};

use crate::{BlockHash, BlockHeight, BlockId, ChainTipMetadata, ShieldedProtocol};

/// Maximum canonical Zebra `finalState` size admitted for one commitment tree.
pub const MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES: usize = 1_090;
const NOTE_COMMITMENT_TREE_DEPTH: u8 = 32;

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
    /// Constructs the canonical empty frontier for an active shielded pool.
    #[must_use]
    pub fn empty(protocol: ShieldedProtocol) -> Self {
        match protocol {
            ShieldedProtocol::Sapling => empty_frontier::<SaplingNode>(|root| {
                let mut root_bytes = root.to_bytes();
                root_bytes.reverse();
                root_bytes
            }),
            ShieldedProtocol::Orchard | ShieldedProtocol::Ironwood => {
                empty_frontier::<MerkleHashOrchard>(MerkleHashOrchard::to_bytes)
            }
        }
    }

    /// Validates and constructs one canonical Zebra commitment-tree frontier.
    ///
    /// The official Zcash codecs must accept the legacy `finalState` bytes,
    /// their canonical re-encoding must be byte-identical, and the derived
    /// root must equal `final_root` in Zebra RPC display order.
    pub fn from_canonical_final_state(
        protocol: ShieldedProtocol,
        final_root: FinalNoteCommitmentRoot,
        final_state_bytes: impl Into<Box<[u8]>>,
    ) -> Result<Self, CommitmentTreeFrontierValidationError> {
        let final_state_bytes = final_state_bytes.into();
        match protocol {
            ShieldedProtocol::Sapling => {
                validate_frontier::<SaplingNode>(final_root, final_state_bytes, |root| {
                    let mut root_bytes = root.to_bytes();
                    root_bytes.reverse();
                    root_bytes
                })
            }
            ShieldedProtocol::Orchard | ShieldedProtocol::Ironwood => {
                validate_frontier::<MerkleHashOrchard>(
                    final_root,
                    final_state_bytes,
                    MerkleHashOrchard::to_bytes,
                )
            }
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

fn empty_frontier<Node>(
    root_bytes_in_rpc_order: impl Fn(&Node) -> [u8; 32],
) -> CommitmentTreeFrontier
where
    Node: Clone + Hashable,
{
    let root = Frontier::<Node, NOTE_COMMITMENT_TREE_DEPTH>::empty().root();
    CommitmentTreeFrontier {
        tree_size: 0,
        final_root: FinalNoteCommitmentRoot::from_bytes(root_bytes_in_rpc_order(&root)),
        final_state_bytes: Box::from([0_u8, 0, 0]),
    }
}

/// Invalid canonical Zebra commitment-tree frontier.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommitmentTreeFrontierValidationError {
    /// The encoded frontier exceeds the version-1 admission bound.
    #[error("frontier is {byte_count} bytes; maximum is {max_byte_count}")]
    TooLarge {
        /// Observed `finalState` length.
        byte_count: usize,
        /// Maximum accepted `finalState` length.
        max_byte_count: usize,
    },
    /// The bytes are not one exact canonical legacy `CommitmentTree` encoding.
    #[error("invalid commitment-tree frontier encoding: {reason}")]
    InvalidEncoding {
        /// Stable validation reason.
        reason: &'static str,
    },
    /// The official frontier position cannot be represented by the v1 domain size.
    #[error("commitment-tree size {tree_size} does not fit u32")]
    TreeSizeOutOfRange {
        /// Size derived from the canonical frontier.
        tree_size: u64,
    },
    /// The canonical frontier derives a different Zebra-order root.
    #[error("commitment-tree frontier root does not match finalRoot")]
    RootMismatch,
}

fn validate_frontier<Node>(
    final_root: FinalNoteCommitmentRoot,
    final_state_bytes: Box<[u8]>,
    root_bytes_in_rpc_order: impl Fn(&Node) -> [u8; 32],
) -> Result<CommitmentTreeFrontier, CommitmentTreeFrontierValidationError>
where
    Node: Clone + Hashable + HashSer,
{
    if final_state_bytes.len() > MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES {
        return Err(CommitmentTreeFrontierValidationError::TooLarge {
            byte_count: final_state_bytes.len(),
            max_byte_count: MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES,
        });
    }
    let mut reader = Cursor::new(final_state_bytes.as_ref());
    let legacy_tree = read_commitment_tree::<Node, _, NOTE_COMMITMENT_TREE_DEPTH>(&mut reader)
        .map_err(|_| CommitmentTreeFrontierValidationError::InvalidEncoding {
            reason: "finalState is not a valid legacy CommitmentTree encoding",
        })?;
    if reader.position() != u64::try_from(final_state_bytes.len()).unwrap_or(u64::MAX) {
        return Err(CommitmentTreeFrontierValidationError::InvalidEncoding {
            reason: "finalState contains trailing bytes",
        });
    }

    let frontier = frontier_from_legacy_tree(&legacy_tree)?;
    let tree_size = frontier.tree_size();
    let tree_size = u32::try_from(tree_size)
        .map_err(|_| CommitmentTreeFrontierValidationError::TreeSizeOutOfRange { tree_size })?;
    let canonical_tree = CommitmentTree::from_frontier(&frontier);
    let mut canonical_bytes = Vec::new();
    write_commitment_tree(&canonical_tree, &mut canonical_bytes).map_err(|_| {
        CommitmentTreeFrontierValidationError::InvalidEncoding {
            reason: "validated frontier could not be canonically encoded",
        }
    })?;
    if canonical_bytes.as_slice() != final_state_bytes.as_ref() {
        return Err(CommitmentTreeFrontierValidationError::InvalidEncoding {
            reason: "finalState is not the canonical Zebra RPC encoding",
        });
    }
    if root_bytes_in_rpc_order(&frontier.root()) != final_root.as_bytes() {
        return Err(CommitmentTreeFrontierValidationError::RootMismatch);
    }
    Ok(CommitmentTreeFrontier {
        tree_size,
        final_root,
        final_state_bytes,
    })
}

fn frontier_from_legacy_tree<Node>(
    legacy_tree: &CommitmentTree<Node, NOTE_COMMITMENT_TREE_DEPTH>,
) -> Result<Frontier<Node, NOTE_COMMITMENT_TREE_DEPTH>, CommitmentTreeFrontierValidationError>
where
    Node: Clone,
{
    let (leaf, mut ommers, mut tree_size) = match (legacy_tree.left(), legacy_tree.right()) {
        (None, None) => {
            if legacy_tree.parents().iter().any(Option::is_some) {
                return Err(CommitmentTreeFrontierValidationError::InvalidEncoding {
                    reason: "empty tree contains parent nodes",
                });
            }
            return Ok(Frontier::empty());
        }
        (None, Some(_)) => {
            return Err(CommitmentTreeFrontierValidationError::InvalidEncoding {
                reason: "right leaf is present without a left leaf",
            });
        }
        (Some(left), None) => (left.clone(), Vec::new(), 1_u64),
        (Some(left), Some(right)) => (right.clone(), vec![left.clone()], 2_u64),
    };

    for (parent_index, parent) in legacy_tree.parents().iter().enumerate() {
        if let Some(parent) = parent {
            ommers.push(parent.clone());
            tree_size = tree_size.checked_add(1_u64 << (parent_index + 1)).ok_or(
                CommitmentTreeFrontierValidationError::InvalidEncoding {
                    reason: "legacy tree size overflowed u64",
                },
            )?;
        }
    }
    Frontier::from_parts(Position::from(tree_size - 1), leaf, ommers).map_err(|_| {
        CommitmentTreeFrontierValidationError::InvalidEncoding {
            reason: "legacy tree occupancy does not form a valid frontier",
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_empty_frontiers_round_trip_through_official_codecs() {
        for protocol in [
            ShieldedProtocol::Sapling,
            ShieldedProtocol::Orchard,
            ShieldedProtocol::Ironwood,
        ] {
            let frontier = CommitmentTreeFrontier::empty(protocol);
            let decoded = CommitmentTreeFrontier::from_canonical_final_state(
                protocol,
                frontier.final_root(),
                frontier.final_state_bytes(),
            );
            assert_eq!(decoded, Ok(frontier));
        }
    }

    #[test]
    fn frontier_validation_rejects_wrong_root_trailing_bytes_and_oversize_state() {
        let frontier = CommitmentTreeFrontier::empty(ShieldedProtocol::Orchard);
        assert_eq!(
            CommitmentTreeFrontier::from_canonical_final_state(
                ShieldedProtocol::Orchard,
                FinalNoteCommitmentRoot::from_bytes([0xff; 32]),
                frontier.final_state_bytes(),
            ),
            Err(CommitmentTreeFrontierValidationError::RootMismatch)
        );

        let mut trailing = frontier.final_state_bytes().to_vec();
        trailing.push(0);
        assert!(matches!(
            CommitmentTreeFrontier::from_canonical_final_state(
                ShieldedProtocol::Orchard,
                frontier.final_root(),
                trailing,
            ),
            Err(CommitmentTreeFrontierValidationError::InvalidEncoding { .. })
        ));

        let oversized = vec![0; MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES + 1];
        assert_eq!(
            CommitmentTreeFrontier::from_canonical_final_state(
                ShieldedProtocol::Orchard,
                frontier.final_root(),
                oversized,
            ),
            Err(CommitmentTreeFrontierValidationError::TooLarge {
                byte_count: MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES + 1,
                max_byte_count: MAX_COMMITMENT_TREE_FRONTIER_FINAL_STATE_BYTES,
            })
        );
    }
}
