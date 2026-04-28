//! Block artifact read traits.

use zinder_core::{BlockArtifact, BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::{HeightVisibilityIndex, visible_height_source_epoch},
    format::{StoreKey, decode_block_artifact, decode_compact_block_artifact},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for finalized block artifacts.
pub trait FinalizedBlockStore {
    /// Reads the finalized block artifact at `height` for the reader's chain epoch.
    fn block_at(&self, height: BlockHeight) -> Result<Option<BlockArtifact>, StoreError>;
}

/// Read boundary for compact block artifacts.
pub trait CompactBlockStore {
    /// Reads the compact block artifact at `height` for the reader's chain epoch.
    fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CompactBlockArtifact>, StoreError>;
}

pub(crate) fn read_block_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<BlockArtifact>, StoreError> {
    if height > chain_epoch.tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::FinalizedBlock,
        HeightVisibilityIndex::FinalizedBlock,
    )?;
    let key = StoreKey::finalized_block(chain_epoch.network, source_epoch, height);
    if let Some(envelope_bytes) = inner.get(StorageTable::FinalizedBlock, &key)? {
        return decode_block_artifact(&key, &envelope_bytes).map(Some);
    }

    Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::FinalizedBlock,
        key: key.into(),
    })
}

pub(crate) fn read_compact_block_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<CompactBlockArtifact>, StoreError> {
    if height > chain_epoch.tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::CompactBlock,
        HeightVisibilityIndex::CompactBlock,
    )?;
    let key = StoreKey::compact_block(chain_epoch.network, source_epoch, height);
    if let Some(envelope_bytes) = inner.get(StorageTable::CompactBlock, &key)? {
        return decode_compact_block_artifact(&key, &envelope_bytes).map(Some);
    }

    Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::CompactBlock,
        key: key.into(),
    })
}

pub(crate) fn read_compact_block_artifacts(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<Option<CompactBlockArtifact>>, StoreError> {
    let mut keys = Vec::new();
    let mut heights = Vec::new();

    for height in block_range {
        if height > chain_epoch.tip_height {
            heights.push(height);
            keys.push(None);
            continue;
        }

        let source_epoch = match visible_height_source_epoch(
            inner,
            chain_epoch,
            height,
            ArtifactFamily::CompactBlock,
            HeightVisibilityIndex::CompactBlock,
        ) {
            Ok(source_epoch) => source_epoch,
            Err(StoreError::ArtifactMissing { .. }) => {
                heights.push(height);
                keys.push(None);
                continue;
            }
            Err(error) => return Err(error),
        };
        heights.push(height);
        keys.push(Some(StoreKey::compact_block(
            chain_epoch.network,
            source_epoch,
            height,
        )));
    }

    let compact_block_keys = keys.iter().flatten().cloned().collect::<Vec<_>>();
    let mut compact_block_values = inner
        .multi_get(StorageTable::CompactBlock, &compact_block_keys)?
        .into_iter();
    let mut compact_blocks = Vec::with_capacity(keys.len());

    for (height, key) in heights.into_iter().zip(keys) {
        let Some(key) = key else {
            compact_blocks.push(None);
            continue;
        };

        let envelope_value = compact_block_values
            .next()
            .ok_or(StoreError::ArtifactMissing {
                family: ArtifactFamily::CompactBlock,
                key: key.clone().into(),
            })?;
        let Some(envelope_bytes) = envelope_value else {
            compact_blocks.push(None);
            continue;
        };
        let compact_block = decode_compact_block_artifact(&key, &envelope_bytes)?;
        if compact_block.height != height {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::CompactBlock,
                key: key.into(),
                reason: "compact block artifact height does not match requested height",
            });
        }

        compact_blocks.push(Some(compact_block));
    }

    Ok(compact_blocks)
}
