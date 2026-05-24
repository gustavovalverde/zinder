//! Transparent-output read helpers.

use std::collections::{HashMap, HashSet};

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, TransparentOutPoint, TransparentOutputArtifact,
};

use crate::{
    StoreError,
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_transparent_output_artifact},
    kv::{RocksChainStoreRead, StorageTable},
};

pub(crate) fn read_current_transparent_outputs_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, StoreError> {
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
        .map(|outpoint| StoreKey::transparent_output(chain_epoch.network, outpoint))
        .collect::<Vec<_>>();
    let rows = inner.multi_get(StorageTable::TransparentOutput, &keys)?;
    let mut resolved = HashMap::with_capacity(outpoints.len());

    for ((outpoint, key), row) in outpoints.into_iter().zip(keys).zip(rows) {
        let Some(envelope_bytes) = row else {
            continue;
        };
        let output = decode_transparent_output_artifact(&key, &envelope_bytes, outpoint)?;
        resolved.insert(outpoint, output);
    }

    Ok(resolved)
}

pub(crate) fn read_visible_transparent_outputs_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, StoreError> {
    let current_outputs =
        read_current_transparent_outputs_by_outpoints(inner, chain_epoch, outpoints)?;
    let mut visible_outputs = HashMap::with_capacity(current_outputs.len());
    for (outpoint, output) in current_outputs {
        if !block_is_visible(inner, chain_epoch, output.block_height, output.block_hash)? {
            continue;
        }
        visible_outputs.insert(outpoint, output);
    }
    Ok(visible_outputs)
}

fn block_is_visible(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
    expected_hash: BlockHash,
) -> Result<bool, StoreError> {
    let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? else {
        return Ok(false);
    };

    Ok(block.block_hash == expected_hash)
}
