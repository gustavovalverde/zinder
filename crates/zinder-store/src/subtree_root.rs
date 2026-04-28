//! Subtree-root artifact read traits.

use zinder_core::{ChainEpoch, SubtreeRootArtifact, SubtreeRootRange};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::visible_subtree_root_source_epoch,
    block_artifact::read_block_artifact,
    format::{StoreKey, decode_subtree_root_artifact},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for subtree-root artifacts.
pub trait SubtreeRootStore {
    /// Reads subtree-root artifacts in ascending subtree-index order.
    fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
    ) -> Result<Vec<Option<SubtreeRootArtifact>>, StoreError>;
}

pub(crate) fn read_subtree_root_artifacts(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    subtree_root_range: SubtreeRootRange,
) -> Result<Vec<Option<SubtreeRootArtifact>>, StoreError> {
    let mut subtree_roots = Vec::with_capacity(subtree_root_range.into_iter().len());

    for subtree_index in subtree_root_range {
        let source_epoch = match visible_subtree_root_source_epoch(
            inner,
            chain_epoch,
            subtree_root_range.protocol,
            subtree_index,
        ) {
            Ok(source_epoch) => source_epoch,
            Err(StoreError::ArtifactMissing { .. }) => {
                subtree_roots.push(None);
                continue;
            }
            Err(error) => return Err(error),
        };
        let key = StoreKey::subtree_root(
            chain_epoch.network,
            source_epoch,
            subtree_root_range.protocol,
            subtree_index,
        );
        let Some(envelope_bytes) = inner.get(StorageTable::SubtreeRoot, &key)? else {
            subtree_roots.push(None);
            continue;
        };
        let subtree_root = decode_subtree_root_artifact(&key, &envelope_bytes)?;
        if subtree_root.protocol != subtree_root_range.protocol
            || subtree_root.subtree_index != subtree_index
        {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::SubtreeRoot,
                key: key.into(),
                reason: "subtree-root artifact does not match requested protocol or index",
            });
        }

        let Some(block) =
            read_block_artifact(inner, chain_epoch, subtree_root.completing_block_height)?
        else {
            subtree_roots.push(None);
            continue;
        };

        if block.block_hash == subtree_root.completing_block_hash {
            subtree_roots.push(Some(subtree_root));
        } else {
            subtree_roots.push(None);
        }
    }

    Ok(subtree_roots)
}
