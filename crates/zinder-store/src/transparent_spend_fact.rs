//! Transparent spend fact read helpers.

use std::collections::{HashMap, HashSet};

use zinder_core::{BlockHash, BlockHeight, ChainEpoch, TransparentOutPoint, TransparentSpendFact};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_transparent_spend_fact, decode_transparent_spend_fact_block_index},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Durable transparent spend replay data for one finalized block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentSpendReplayBlock {
    /// Canonical hash of the block that produced these spends.
    pub block_hash: BlockHash,
    /// Every non-coinbase transparent input observed in the block.
    pub input_outpoints: Vec<TransparentOutPoint>,
    /// Inputs whose parent outputs were available to canonical ingest.
    pub spend_facts: Vec<TransparentSpendFact>,
}

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
    let rows = inner.sorted_multi_get(StorageTable::TransparentSpendFact, &keys)?;
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
                Ok((block_hash, _, block_spend_facts)) if block_hash == block.block_hash => {
                    outpoints = Some(
                        block_spend_facts
                            .into_iter()
                            .map(|spend| spend.spent_outpoint)
                            .collect(),
                    );
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

/// Reads complete block-local spend replay facts from the current projection.
///
/// Correct only for finalized heights. Unlike the point rows used by serving
/// queries, this replay record is durable across transparent spend retention.
pub(crate) fn read_current_transparent_spend_fact_block_facts(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Vec<TransparentSpendFact>, StoreError> {
    Ok(
        read_current_transparent_spend_replay_block(inner, chain_epoch, height)?
            .map_or_else(Vec::new, |replay| replay.spend_facts),
    )
}

/// Reads the complete block-local spend replay record from the current projection.
pub(crate) fn read_current_transparent_spend_replay_block(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<TransparentSpendReplayBlock>, StoreError> {
    let prefix = StoreKey::transparent_spend_fact_block_index_prefix(chain_epoch.network, height);
    let mut replay = None;
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
                Ok((block_hash, input_outpoints, spend_facts)) => {
                    replay = Some(TransparentSpendReplayBlock {
                        block_hash,
                        input_outpoints,
                        spend_facts,
                    });
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

    Ok(replay)
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
