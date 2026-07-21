//! Block fact and compact-block read traits.

use zinder_core::{
    BlockBlobArtifact, BlockHeaderArtifact, BlockHeight, BlockHeightRange,
    BlockTransactionIndexArtifact, ChainEpoch, CompactBlockArtifact, TransactionId,
};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::{HeightVisibilityIndex, visible_height_source_epoch},
    format::{
        StoreKey, decode_block_blob_artifact, decode_block_header_artifact,
        decode_block_transaction_index_artifact, decode_compact_block_artifact,
    },
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Read boundary for canonical block-header facts.
pub trait BlockHeaderStore {
    /// Reads the block-header facts at `height` for the reader's chain epoch.
    fn block_header_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHeaderArtifact>, StoreError>;

    /// Reads block-header facts in one batched store read.
    fn block_headers_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockHeaderArtifact>>, StoreError>;
}

/// Read boundary for optional raw block blobs.
pub trait BlockBlobStore {
    /// Reads the raw block blob at `height` for the reader's chain epoch.
    fn block_blob_at(&self, height: BlockHeight) -> Result<Option<BlockBlobArtifact>, StoreError>;
}

/// Read boundary for transaction ids at a block-local index.
pub trait BlockTransactionIndexStore {
    /// Reads the transaction id at `tx_index_in_block` for `height`.
    fn transaction_id_at_block_index(
        &self,
        height: BlockHeight,
        tx_index_in_block: u32,
    ) -> Result<Option<TransactionId>, StoreError>;

    /// Reads the ordered transaction ids for every transaction in `height`.
    fn transaction_ids_at_height(
        &self,
        height: BlockHeight,
    ) -> Result<Vec<TransactionId>, StoreError>;
}

/// Read boundary for compact block artifacts.
pub trait CompactBlockStore {
    /// Reads the compact block artifact at `height` for the reader's chain epoch.
    fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CompactBlockArtifact>, StoreError>;
}

pub(crate) fn read_block_header_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<BlockHeaderArtifact>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::BlockHeader,
        HeightVisibilityIndex::BlockHeader,
    )?;
    let key = StoreKey::block_header(chain_epoch.network, source_epoch, height);
    if let Some(envelope_bytes) = inner.get(StorageTable::BlockHeader, &key)? {
        return decode_block_header_artifact(&key, &envelope_bytes).map(Some);
    }

    Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::BlockHeader,
        key: key.into(),
    })
}

pub(crate) fn read_block_header_artifacts(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<Option<BlockHeaderArtifact>>, StoreError> {
    let mut keys = Vec::new();
    let mut heights = Vec::new();

    for height in block_range {
        if height > chain_epoch.visible_tip_height {
            heights.push(height);
            keys.push(None);
            continue;
        }

        let source_epoch = match visible_height_source_epoch(
            inner,
            chain_epoch,
            height,
            ArtifactFamily::BlockHeader,
            HeightVisibilityIndex::BlockHeader,
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
        keys.push(Some(StoreKey::block_header(
            chain_epoch.network,
            source_epoch,
            height,
        )));
    }

    let block_keys = keys.iter().flatten().cloned().collect::<Vec<_>>();
    let mut block_values = inner
        .multi_get(StorageTable::BlockHeader, &block_keys)?
        .into_iter();
    let mut blocks = Vec::with_capacity(keys.len());

    for (height, key) in heights.into_iter().zip(keys) {
        let Some(key) = key else {
            blocks.push(None);
            continue;
        };

        let envelope_value = block_values.next().ok_or(StoreError::ArtifactMissing {
            family: ArtifactFamily::BlockHeader,
            key: key.clone().into(),
        })?;
        let Some(envelope_bytes) = envelope_value else {
            return Err(StoreError::ArtifactMissing {
                family: ArtifactFamily::BlockHeader,
                key: key.into(),
            });
        };
        let block = decode_block_header_artifact(&key, &envelope_bytes)?;
        if block.height != height {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::BlockHeader,
                key: key.into(),
                reason: "block-header artifact height does not match requested height",
            });
        }

        blocks.push(Some(block));
    }

    Ok(blocks)
}

pub(crate) fn read_block_blob_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<BlockBlobArtifact>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::BlockBlob,
        HeightVisibilityIndex::BlockHeader,
    )?;
    let key = StoreKey::block_blob(chain_epoch.network, source_epoch, height);
    let Some(envelope_bytes) = inner.get(StorageTable::BlockBlob, &key)? else {
        return Ok(None);
    };
    decode_block_blob_artifact(&key, &envelope_bytes).map(Some)
}

pub(crate) fn read_block_blob_artifacts(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<Option<BlockBlobArtifact>>, StoreError> {
    let mut keys = Vec::new();
    let mut heights = Vec::new();

    for height in block_range {
        if height > chain_epoch.visible_tip_height {
            heights.push(height);
            keys.push(None);
            continue;
        }

        let source_epoch = match visible_height_source_epoch(
            inner,
            chain_epoch,
            height,
            ArtifactFamily::BlockBlob,
            HeightVisibilityIndex::BlockHeader,
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
        keys.push(Some(StoreKey::block_blob(
            chain_epoch.network,
            source_epoch,
            height,
        )));
    }

    let block_keys = keys.iter().flatten().cloned().collect::<Vec<_>>();
    let mut block_values = inner
        .multi_get(StorageTable::BlockBlob, &block_keys)?
        .into_iter();
    let mut block_blobs = Vec::with_capacity(keys.len());

    for (height, key) in heights.into_iter().zip(keys) {
        let Some(key) = key else {
            block_blobs.push(None);
            continue;
        };

        let envelope_value = block_values.next().ok_or(StoreError::ArtifactMissing {
            family: ArtifactFamily::BlockBlob,
            key: key.clone().into(),
        })?;
        let Some(envelope_bytes) = envelope_value else {
            block_blobs.push(None);
            continue;
        };
        let block_blob = decode_block_blob_artifact(&key, &envelope_bytes)?;
        if block_blob.height != height {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::BlockBlob,
                key: key.into(),
                reason: "block blob artifact height does not match requested height",
            });
        }

        block_blobs.push(Some(block_blob));
    }

    Ok(block_blobs)
}

pub(crate) fn read_block_transaction_index_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
    tx_index_in_block: u32,
) -> Result<Option<BlockTransactionIndexArtifact>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::BlockTransactionIndex,
        HeightVisibilityIndex::BlockHeader,
    )?;
    let key = StoreKey::block_transaction_index(
        chain_epoch.network,
        source_epoch,
        height,
        tx_index_in_block,
    );
    let Some(envelope_bytes) = inner.get(StorageTable::BlockTransactionIndex, &key)? else {
        return Ok(None);
    };
    let artifact = decode_block_transaction_index_artifact(&key, &envelope_bytes)?;
    if artifact.block_height == height && artifact.tx_index_in_block == tx_index_in_block {
        return Ok(Some(artifact));
    }

    Err(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::BlockTransactionIndex,
        key: key.into(),
        reason: "block transaction-index row does not match requested location",
    })
}

pub(crate) fn read_block_transaction_index_artifacts_at_height(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Vec<BlockTransactionIndexArtifact>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(Vec::new());
    }

    let source_epoch = visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::BlockTransactionIndex,
        HeightVisibilityIndex::BlockHeader,
    )?;
    let prefix =
        StoreKey::block_transaction_index_prefix(chain_epoch.network, source_epoch, height);
    let mut artifacts = Vec::new();
    inner.scan_prefix(
        StorageTable::BlockTransactionIndex,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let key = StoreKey::from_raw_bytes(key_bytes);
            let artifact = decode_block_transaction_index_artifact(&key, envelope_bytes)?;
            if artifact.block_height != height {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::BlockTransactionIndex,
                    key: key.into(),
                    reason: "block transaction-index row height does not match scan prefix",
                });
            }
            artifacts.push(artifact);
            Ok(PrefixScanControl::Continue)
        },
    )?;
    artifacts.sort_by_key(|artifact| artifact.tx_index_in_block);
    Ok(artifacts)
}

pub(crate) fn read_compact_block_artifact(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<CompactBlockArtifact>, StoreError> {
    if height > chain_epoch.visible_tip_height {
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
        if height > chain_epoch.visible_tip_height {
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
        if compact_block.height() != height {
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
