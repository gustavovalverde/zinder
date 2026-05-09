//! Best-chain block-hash to height resolver.
//!
//! Maintains a `(network, block_hash) -> (height, source_chain_epoch_id)`
//! mapping written by every successful chain-epoch commit. Because the key is
//! overwritten when a hash is reintroduced, read paths treat the row as a
//! height hint and verify the block artifact visible at the requested epoch has
//! the requested hash. Reorged-out hashes return
//! [`BlockHashLookup::NotInBestChain`] without an eager delete.

use std::mem::size_of;

use zinder_core::{BlockHash, BlockHeight, BlockId, ChainEpoch, ChainEpochId, Network};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_artifact,
    format::StoreKey,
    kv::{RocksChainStoreRead, StoragePut, StorageTable},
};

/// Outcome of a hash-to-height lookup against the canonical best chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockHashLookup {
    /// The hash addresses a block currently visible in the best chain.
    Resolved(BlockId),
    /// The hash was indexed but its block is no longer visible at the
    /// request epoch (reorged out).
    NotInBestChain,
    /// No entry was written for this hash by any committed chain epoch.
    NotIndexed,
}

const VALUE_LEN: usize = size_of::<u32>() + size_of::<u64>();

pub(crate) fn block_hash_index_put(
    network: Network,
    chain_epoch_id: ChainEpochId,
    height: BlockHeight,
    block_hash: BlockHash,
) -> StoragePut {
    StoragePut {
        table: StorageTable::BlockHashIndex,
        key: StoreKey::block_hash_index(network, block_hash),
        value: encode_block_hash_index_value(height, chain_epoch_id),
    }
}

pub(crate) fn read_block_hash_lookup(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_hash: BlockHash,
) -> Result<BlockHashLookup, StoreError> {
    let key = StoreKey::block_hash_index(chain_epoch.network, block_hash);
    let Some(value_bytes) = inner.get(StorageTable::BlockHashIndex, &key)? else {
        return Ok(BlockHashLookup::NotIndexed);
    };
    let recorded_height = decode_block_hash_index_value(&key, &value_bytes)?;

    if recorded_height > chain_epoch.tip_height {
        return Ok(BlockHashLookup::NotInBestChain);
    }

    match read_block_artifact(inner, chain_epoch, recorded_height) {
        Ok(Some(block)) if block.block_hash == block_hash => Ok(BlockHashLookup::Resolved(
            BlockId::new(recorded_height, block_hash),
        )),
        Ok(Some(_) | None) | Err(StoreError::ArtifactMissing { .. }) => {
            Ok(BlockHashLookup::NotInBestChain)
        }
        Err(error) => Err(error),
    }
}

fn encode_block_hash_index_value(height: BlockHeight, chain_epoch_id: ChainEpochId) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VALUE_LEN);
    bytes.extend_from_slice(&height.value().to_be_bytes());
    bytes.extend_from_slice(&chain_epoch_id.value().to_be_bytes());
    bytes
}

fn decode_block_hash_index_value(
    key: &StoreKey,
    value_bytes: &[u8],
) -> Result<BlockHeight, StoreError> {
    if value_bytes.len() != VALUE_LEN {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockHashIndex,
            key: key.clone().into(),
            reason: "block-hash-index value has unexpected length",
        });
    }

    let mut height_bytes = [0_u8; size_of::<u32>()];
    height_bytes.copy_from_slice(&value_bytes[..size_of::<u32>()]);

    Ok(BlockHeight::new(u32::from_be_bytes(height_bytes)))
}
