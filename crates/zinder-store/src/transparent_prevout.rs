//! Transparent prevout read helpers.

use std::collections::{HashMap, HashSet};

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, TransparentOutPoint, TransparentPrevoutArtifact,
};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_artifact,
    format::{StoreKey, decode_transparent_prevout_artifact},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

pub(crate) fn read_current_transparent_prevouts_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>, StoreError> {
    let unique_outpoints = outpoints.iter().copied().collect::<HashSet<_>>();
    let mut outpoints = unique_outpoints.into_iter().collect::<Vec<_>>();
    outpoints.sort_by(|left, right| {
        left.transaction_id
            .as_bytes()
            .cmp(&right.transaction_id.as_bytes())
            .then(left.output_index.cmp(&right.output_index))
    });
    let keys = outpoints
        .iter()
        .copied()
        .map(|outpoint| StoreKey::transparent_prevout(chain_epoch.network, outpoint))
        .collect::<Vec<_>>();
    let rows = inner.multi_get(StorageTable::TransparentPrevout, &keys)?;
    let mut resolved = HashMap::with_capacity(outpoints.len());

    for ((outpoint, key), row) in outpoints.into_iter().zip(keys).zip(rows) {
        let Some(envelope_bytes) = row else {
            continue;
        };
        let prevout = decode_transparent_prevout_artifact(&key, &envelope_bytes, outpoint)?;
        resolved.insert(outpoint, prevout);
    }

    Ok(resolved)
}

pub(crate) fn read_transparent_prevout_from_history(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoint: TransparentOutPoint,
) -> Result<Option<TransparentPrevoutArtifact>, StoreError> {
    read_transparent_prevout_from_history_with_block_visibility(
        inner,
        chain_epoch,
        outpoint,
        &mut |height, expected_hash| block_is_visible(inner, chain_epoch, height, expected_hash),
    )
}

pub(crate) fn read_transparent_prevout_from_history_with_block_visibility(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoint: TransparentOutPoint,
    is_block_visible: &mut impl FnMut(BlockHeight, BlockHash) -> Result<bool, StoreError>,
) -> Result<Option<TransparentPrevoutArtifact>, StoreError> {
    read_transparent_prevout_from_history_with_visibility(
        inner,
        chain_epoch,
        outpoint,
        is_block_visible,
    )
}

pub(crate) fn read_historical_transparent_prevouts_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>, StoreError> {
    let unique_outpoints = outpoints.iter().copied().collect::<HashSet<_>>();
    let mut resolved = HashMap::with_capacity(unique_outpoints.len());
    for outpoint in unique_outpoints {
        let Some(prevout) = read_transparent_prevout_from_history(inner, chain_epoch, outpoint)?
        else {
            continue;
        };
        resolved.insert(outpoint, prevout);
    }
    Ok(resolved)
}

fn read_transparent_prevout_from_history_with_visibility(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoint: TransparentOutPoint,
    is_block_visible: &mut impl FnMut(BlockHeight, BlockHash) -> Result<bool, StoreError>,
) -> Result<Option<TransparentPrevoutArtifact>, StoreError> {
    let prefix = StoreKey::transparent_prevout_history_prefix(chain_epoch.network, outpoint);
    let mut resolved = None;

    inner.scan_prefix_reverse(
        StorageTable::TransparentPrevoutHistory,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentPrevout,
                    key: prefix.clone().into(),
                    reason: "transparent prevout key is malformed",
                });
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let key = StoreKey::from_raw_bytes(key_bytes);
            let prevout = decode_transparent_prevout_artifact(&key, envelope_bytes, outpoint)?;
            if is_block_visible(prevout.block_height, prevout.block_hash)? {
                resolved = Some(prevout);
                return Ok(PrefixScanControl::Stop);
            }

            Ok(PrefixScanControl::Continue)
        },
    )?;

    Ok(resolved)
}

fn block_is_visible(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
    expected_hash: BlockHash,
) -> Result<bool, StoreError> {
    let Some(block) = read_block_artifact(inner, chain_epoch, height)? else {
        return Ok(false);
    };

    Ok(block.block_hash == expected_hash)
}
