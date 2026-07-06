//! Transparent spend fact read helpers.

use std::collections::{HashMap, HashSet};

use zinder_core::{BlockHash, BlockHeight, ChainEpoch, TransparentOutPoint, TransparentSpendFact};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_transparent_spend_fact, decode_transparent_spend_fact_block_index},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

pub(crate) fn read_current_transparent_spend_facts_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, StoreError> {
    let outpoints = unique_sorted_outpoints(outpoints);
    let keys = outpoints
        .iter()
        .copied()
        .map(|outpoint| StoreKey::transparent_spend_fact(chain_epoch.network, outpoint))
        .collect::<Vec<_>>();
    let rows = inner.multi_get(StorageTable::TransparentSpendFact, &keys)?;
    let mut resolved = HashMap::with_capacity(outpoints.len());

    for ((outpoint, key), row) in outpoints.into_iter().zip(keys).zip(rows) {
        let Some(envelope_bytes) = row else {
            continue;
        };
        let spend = decode_transparent_spend_fact(&key, &envelope_bytes, outpoint)?;
        resolved.insert(outpoint, spend);
    }

    Ok(resolved)
}

pub(crate) fn read_visible_transparent_spend_facts_by_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, StoreError> {
    let current_spends =
        read_current_transparent_spend_facts_by_outpoints(inner, chain_epoch, outpoints)?;
    let mut visible_spends = HashMap::with_capacity(current_spends.len());
    for (outpoint, spend) in current_spends {
        let spending_block_is_visible =
            block_is_visible(inner, chain_epoch, spend.block_height, spend.block_hash)?;
        let spent_block_is_visible = block_is_visible(
            inner,
            chain_epoch,
            spend.spent_block_height,
            spend.spent_block_hash,
        )?;
        if !spending_block_is_visible || !spent_block_is_visible {
            continue;
        }
        visible_spends.insert(outpoint, spend);
    }
    Ok(visible_spends)
}

pub(crate) fn read_visible_transparent_spend_fact_block_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Vec<TransparentOutPoint>, StoreError> {
    let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? else {
        return Ok(Vec::new());
    };

    let prefix = StoreKey::transparent_spend_fact_block_index_prefix(chain_epoch.network, height);
    let mut outpoints = None;
    let mut scan_error = None;
    inner.scan_prefix_reverse(
        StorageTable::TransparentSpendFactBlockIndex,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                scan_error = Some(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentSpendFact,
                    key: prefix.clone().into(),
                    reason: "transparent spend fact block index key is malformed",
                });
                return Ok(PrefixScanControl::Stop);
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let key = StoreKey::from_raw_bytes(key_bytes);
            match decode_transparent_spend_fact_block_index(&key, envelope_bytes) {
                Ok((block_hash, block_outpoints)) if block_hash == block.block_hash => {
                    outpoints = Some(block_outpoints);
                    Ok(PrefixScanControl::Stop)
                }
                Ok(_) => Ok(PrefixScanControl::Continue),
                Err(error) => {
                    scan_error = Some(error);
                    Ok(PrefixScanControl::Stop)
                }
            }
        },
    )?;

    if let Some(error) = scan_error {
        return Err(error);
    }

    Ok(outpoints.unwrap_or_default())
}

/// Reads the spent outpoints recorded for `height` from the current projection,
/// skipping the visible-header seek [`read_visible_transparent_spend_fact_block_outpoints`]
/// performs.
///
/// Correct only for finalized heights (at or below `settled_tip_height`): such
/// blocks are immutable, so the highest-epoch block-index entry at or below the
/// pinned epoch is the canonical one and no orphaned entry can outrank it. The
/// reverse scan returns that entry directly, turning a block-header read plus a
/// hash match into a single index seek.
pub(crate) fn read_current_transparent_spend_fact_block_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Vec<TransparentOutPoint>, StoreError> {
    let prefix = StoreKey::transparent_spend_fact_block_index_prefix(chain_epoch.network, height);
    let mut outpoints = None;
    let mut scan_error = None;
    inner.scan_prefix_reverse(
        StorageTable::TransparentSpendFactBlockIndex,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                scan_error = Some(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentSpendFact,
                    key: prefix.clone().into(),
                    reason: "transparent spend fact block index key is malformed",
                });
                return Ok(PrefixScanControl::Stop);
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let key = StoreKey::from_raw_bytes(key_bytes);
            match decode_transparent_spend_fact_block_index(&key, envelope_bytes) {
                Ok((_, block_outpoints)) => {
                    outpoints = Some(block_outpoints);
                    Ok(PrefixScanControl::Stop)
                }
                Err(error) => {
                    scan_error = Some(error);
                    Ok(PrefixScanControl::Stop)
                }
            }
        },
    )?;

    if let Some(error) = scan_error {
        return Err(error);
    }

    Ok(outpoints.unwrap_or_default())
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

fn unique_sorted_outpoints(outpoints: &[TransparentOutPoint]) -> Vec<TransparentOutPoint> {
    let unique_outpoints = outpoints.iter().copied().collect::<HashSet<_>>();
    let mut outpoints = unique_outpoints.into_iter().collect::<Vec<_>>();
    outpoints.sort_by(|left, right| {
        left.transaction_id
            .as_bytes()
            .cmp(&right.transaction_id.as_bytes())
            .then(left.output_index.cmp(&right.output_index))
    });
    outpoints
}
