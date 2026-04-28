//! Tree-state artifact read traits.

use zinder_core::{BlockHeight, ChainEpoch, TreeStateArtifact};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::{HeightVisibilityIndex, visible_height_source_epoch},
    block_artifact::read_block_artifact,
    format::{StoreKey, decode_tree_state_artifact},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for commitment tree-state artifacts.
pub trait TreeStateStore {
    /// Reads the tree-state artifact at `height` for the reader's chain epoch.
    fn tree_state_at(&self, height: BlockHeight) -> Result<Option<TreeStateArtifact>, StoreError>;
}

pub(crate) fn read_tree_state_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<TreeStateArtifact>, StoreError> {
    if height > chain_epoch.tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::TreeState,
        HeightVisibilityIndex::TreeState,
    )?;
    let key = StoreKey::tree_state(chain_epoch.network, source_epoch, height);
    let Some(envelope_bytes) = inner.get(StorageTable::TreeState, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::TreeState,
            key: key.into(),
        });
    };

    let tree_state = decode_tree_state_artifact(&key, &envelope_bytes)?;
    let Some(block) = read_block_artifact(inner, chain_epoch, tree_state.height)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::TreeState,
            key: key.into(),
        });
    };

    if block.block_hash == tree_state.block_hash {
        return Ok(Some(tree_state));
    }

    // Tree-state reads are height-addressed consistency checks, so a reverted
    // branch at that height is treated as a missing required artifact.
    Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::TreeState,
        key: StoreKey::tree_state(chain_epoch.network, chain_epoch.id, height).into(),
    })
}
