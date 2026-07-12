//! Canonical block value-pool balance artifact reads.

use zinder_core::{BlockHeight, BlockHeightRange, BlockValuePoolBalances, ChainEpoch};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::{HeightVisibilityIndex, visible_height_source_epoch},
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_block_value_pool_balances},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for cumulative value-pool balances after canonical blocks.
pub trait BlockValuePoolBalancesStore {
    /// Reads cumulative value-pool balances after one canonical block.
    fn block_value_pool_balances_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockValuePoolBalances>, StoreError>;

    /// Reads cumulative value-pool balances for a bounded ascending range.
    fn block_value_pool_balances_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockValuePoolBalances>>, StoreError>;
}

pub(crate) fn read_block_value_pool_balances(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<BlockValuePoolBalances>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    let source_epoch = match visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::BlockValuePoolBalances,
        HeightVisibilityIndex::BlockValuePoolBalances,
    ) {
        Ok(source_epoch) => source_epoch,
        Err(StoreError::ArtifactMissing { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let key = StoreKey::block_value_pool_balances(chain_epoch.network, source_epoch, height);
    let Some(envelope_bytes) = inner.get(StorageTable::BlockValuePoolBalances, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::BlockValuePoolBalances,
            key: key.into(),
        });
    };
    let balances = decode_block_value_pool_balances(&key, &envelope_bytes)?;
    if balances.block_id.height != height {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockValuePoolBalances,
            key: key.into(),
            reason: "block value-pool balances height does not match requested height",
        });
    }

    let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? else {
        return Ok(None);
    };
    if block.block_hash != balances.block_id.hash {
        return Ok(None);
    }

    Ok(Some(balances))
}

pub(crate) fn read_block_value_pool_balances_in_range(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<Option<BlockValuePoolBalances>>, StoreError> {
    block_range
        .into_iter()
        .map(|height| read_block_value_pool_balances(inner, chain_epoch, height))
        .collect()
}
