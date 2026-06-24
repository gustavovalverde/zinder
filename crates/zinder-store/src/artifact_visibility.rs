//! Visibility index lookups for epoch-bound artifact reads.

use zinder_core::{BlockHeight, ChainEpoch, ChainEpochId, Network};

use crate::{
    ArtifactFamily, StoreError,
    format::StoreKey,
    kv::{RocksChainStoreRead, StorageTable},
};

pub(crate) fn visible_height_source_epoch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
    family: ArtifactFamily,
    index: HeightVisibilityIndex,
) -> Result<ChainEpochId, StoreError> {
    metrics::counter!(
        "zinder_store_visibility_seek_total",
        "artifact_family" => family.wire_label()
    )
    .increment(1);
    let prefix = index.prefix_key(chain_epoch.network, height);
    let seek_key = index.seek_key(chain_epoch.network, height, chain_epoch.id);
    let Some(source_epoch_bytes) =
        inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
    else {
        return Err(StoreError::ArtifactMissing {
            family,
            key: seek_key.into(),
        });
    };

    decode_visible_source_epoch(family, &seek_key, &source_epoch_bytes)
}

pub(crate) fn decode_visible_source_epoch(
    family: ArtifactFamily,
    key: &StoreKey,
    source_epoch_bytes: &[u8],
) -> Result<ChainEpochId, StoreError> {
    if source_epoch_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family,
            key: key.clone().into(),
            reason: "visible artifact epoch pointer must be 8 bytes",
        });
    }

    let source_epoch: [u8; 8] =
        source_epoch_bytes
            .try_into()
            .map_err(|_| StoreError::ArtifactCorrupt {
                family,
                key: key.clone().into(),
                reason: "visible artifact epoch pointer must be 8 bytes",
            })?;
    Ok(ChainEpochId::new(u64::from_be_bytes(source_epoch)))
}

#[derive(Clone, Copy)]
pub(crate) enum HeightVisibilityIndex {
    SafeBlock,
    CompactBlock,
}

impl HeightVisibilityIndex {
    fn prefix_key(self, network: Network, height: BlockHeight) -> StoreKey {
        match self {
            Self::SafeBlock => StoreKey::visible_block_epoch_prefix(network, height),
            Self::CompactBlock => StoreKey::visible_compact_block_epoch_prefix(network, height),
        }
    }

    fn seek_key(
        self,
        network: Network,
        height: BlockHeight,
        chain_epoch: ChainEpochId,
    ) -> StoreKey {
        match self {
            Self::SafeBlock => StoreKey::visible_block_epoch(network, height, chain_epoch),
            Self::CompactBlock => {
                StoreKey::visible_compact_block_epoch(network, height, chain_epoch)
            }
        }
    }
}

pub(crate) fn visible_subtree_root_source_epoch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    protocol: zinder_core::ShieldedProtocol,
    subtree_index: zinder_core::SubtreeRootIndex,
) -> Result<ChainEpochId, StoreError> {
    let prefix =
        StoreKey::visible_subtree_root_epoch_prefix(chain_epoch.network, protocol, subtree_index);
    let seek_key = StoreKey::visible_subtree_root_epoch(
        chain_epoch.network,
        protocol,
        subtree_index,
        chain_epoch.id,
    );
    let Some(source_epoch_bytes) =
        inner.get_previous_by_prefix(StorageTable::ReorgWindow, &prefix, &seek_key)?
    else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::SubtreeRoot,
            key: seek_key.into(),
        });
    };

    decode_visible_source_epoch(ArtifactFamily::SubtreeRoot, &seek_key, &source_epoch_bytes)
}
