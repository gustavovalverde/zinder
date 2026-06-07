//! Tree-state artifact read traits.

use zinder_core::{BlockHeight, ChainEpoch, TreeStateArtifact};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_tree_state_artifact},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Read boundary for commitment tree-state artifacts.
pub trait TreeStateStore {
    /// Reads the latest checkpoint tree state not above `max_height`.
    fn tree_state_checkpoint_at_or_before(
        &self,
        max_height: BlockHeight,
    ) -> Result<Option<TreeStateArtifact>, StoreError>;
}

pub(crate) fn read_tree_state_checkpoint_at_or_before(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    max_height: BlockHeight,
) -> Result<Option<TreeStateArtifact>, StoreError> {
    if max_height > chain_epoch.tip_height {
        return read_tree_state_checkpoint_at_or_before(inner, chain_epoch, chain_epoch.tip_height);
    }

    let prefix = StoreKey::tree_state_network_prefix(chain_epoch.network);
    let mut checkpoint = None::<TreeStateArtifact>;
    inner.scan_prefix_reverse(
        StorageTable::TreeState,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let key = StoreKey::from_raw_bytes(key_bytes);
            let Some((source_epoch, height)) = StoreKey::tree_state_key_parts(key_bytes) else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TreeState,
                    key: key.into(),
                    reason: "tree-state key does not match checkpoint scan prefix",
                });
            };
            if source_epoch > chain_epoch.id || height > max_height {
                return Ok(PrefixScanControl::Continue);
            }

            let tree_state = decode_tree_state_artifact(&key, envelope_bytes)?;
            let Some(block) = read_block_header_artifact(inner, chain_epoch, tree_state.height)?
            else {
                return Ok(PrefixScanControl::Continue);
            };
            if block.block_hash != tree_state.block_hash {
                return Ok(PrefixScanControl::Continue);
            }

            // The reverse scan visits descending (epoch, height), so the first
            // entry that clears the epoch/height filters and matches the
            // canonical block hash is the highest stored checkpoint at-or-below
            // max_height. Stop there instead of draining the whole column family
            // (one checkpoint per 100 blocks, plus a block-header read each).
            checkpoint = Some(tree_state);
            Ok(PrefixScanControl::Stop)
        },
    )?;
    Ok(checkpoint)
}
