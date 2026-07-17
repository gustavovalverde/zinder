//! Intrinsically valid commitment-tree frontier fixtures.

use eyre::{Result, eyre};
use incrementalmerkletree::{
    Position,
    frontier::{CommitmentTree, Frontier},
};
use sapling::Node as SaplingNode;
use zcash_primitives::merkle_tree::write_commitment_tree;
use zinder_core::{
    CommitmentTreeFrontier, FinalNoteCommitmentRoot, SUBTREE_LEAF_COUNT, ShieldedProtocol,
};

/// Returns a canonical Sapling frontier containing one note commitment.
pub fn one_leaf_sapling_frontier() -> Result<CommitmentTreeFrontier> {
    sapling_frontier(Position::from(0), Vec::new())
}

/// Returns a canonical Sapling frontier after exactly one completed subtree.
pub fn completed_sapling_subtree_frontier() -> Result<CommitmentTreeFrontier> {
    let tree_size = u64::from(SUBTREE_LEAF_COUNT);
    let position = Position::from(tree_size.saturating_sub(1));
    let ommers = vec![sapling_leaf()?; 16];
    sapling_frontier(position, ommers)
}

fn sapling_frontier(
    position: Position,
    ommers: Vec<SaplingNode>,
) -> Result<CommitmentTreeFrontier> {
    let leaf = sapling_leaf()?;
    let frontier: Frontier<SaplingNode, 32> = Frontier::from_parts(position, leaf, ommers)
        .map_err(|error| eyre!("valid Sapling frontier fixture rejected: {error:?}"))?;
    let tree = CommitmentTree::from_frontier(&frontier);
    let mut final_state_bytes = Vec::new();
    write_commitment_tree(&tree, &mut final_state_bytes)?;
    let mut final_root_bytes = frontier.root().to_bytes();
    final_root_bytes.reverse();
    Ok(CommitmentTreeFrontier::from_canonical_final_state(
        ShieldedProtocol::Sapling,
        FinalNoteCommitmentRoot::from_bytes(final_root_bytes),
        final_state_bytes,
    )?)
}

fn sapling_leaf() -> Result<SaplingNode> {
    let mut leaf_bytes = [0; 32];
    leaf_bytes[0] = 1;
    Option::<SaplingNode>::from(SaplingNode::from_bytes(leaf_bytes))
        .ok_or_else(|| eyre!("one must be a canonical Sapling field element"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_have_exact_nonzero_sizes() -> Result<()> {
        assert_eq!(one_leaf_sapling_frontier()?.tree_size(), 1);
        assert_eq!(
            completed_sapling_subtree_frontier()?.tree_size(),
            SUBTREE_LEAF_COUNT
        );
        Ok(())
    }
}
