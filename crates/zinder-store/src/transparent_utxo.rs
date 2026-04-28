//! Transparent UTXO read traits.

use std::{collections::HashSet, num::NonZeroU32};

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, TransparentAddressScriptHash,
    TransparentAddressUtxoArtifact, TransparentOutPoint,
};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_artifact,
    format::{
        StoreKey, decode_transparent_address_utxo_artifact, decode_transparent_utxo_spend_artifact,
    },
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Read boundary for transparent address UTXO artifacts.
pub trait TransparentUtxoStore {
    /// Reads unspent transparent outputs for `address_script_hash` in the reader's chain epoch.
    fn transparent_address_utxos(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentAddressUtxoArtifact>, StoreError>;
}

pub(crate) fn read_transparent_address_utxos(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    address_script_hash: TransparentAddressScriptHash,
    start_height: BlockHeight,
    max_entries: NonZeroU32,
) -> Result<Vec<TransparentAddressUtxoArtifact>, StoreError> {
    let prefix =
        StoreKey::transparent_address_utxo_prefix(chain_epoch.network, address_script_hash);
    let mut utxos = Vec::new();
    let mut seen_outpoints = HashSet::new();
    let max_entries = u32_to_usize(max_entries.get());

    inner.scan_prefix(
        StorageTable::TransparentAddressUtxo,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentAddressUtxo,
                    key: prefix.clone().into(),
                    reason: "transparent address UTXO key is malformed",
                });
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let utxo = decode_transparent_address_utxo_artifact(&prefix, envelope_bytes)?;
            if utxo.address_script_hash != address_script_hash || utxo.block_height < start_height {
                return Ok(PrefixScanControl::Continue);
            }
            if !block_is_visible(inner, chain_epoch, utxo.block_height, utxo.block_hash)?
                || transparent_outpoint_is_spent(inner, chain_epoch, utxo.outpoint)?
                || !seen_outpoints.insert(utxo.outpoint)
            {
                return Ok(PrefixScanControl::Continue);
            }

            utxos.push(utxo);
            if utxos.len() >= max_entries {
                return Ok(PrefixScanControl::Stop);
            }

            Ok(PrefixScanControl::Continue)
        },
    )?;

    Ok(utxos)
}

fn transparent_outpoint_is_spent(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoint: TransparentOutPoint,
) -> Result<bool, StoreError> {
    let prefix = StoreKey::transparent_utxo_spend_prefix(chain_epoch.network, outpoint);
    let mut is_spent = false;

    inner.scan_prefix(
        StorageTable::TransparentUtxoSpend,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentUtxoSpend,
                    key: prefix.clone().into(),
                    reason: "transparent UTXO spend key is malformed",
                });
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let spend = decode_transparent_utxo_spend_artifact(&prefix, envelope_bytes)?;
            if spend.spent_outpoint == outpoint
                && block_is_visible(inner, chain_epoch, spend.block_height, spend.block_hash)?
            {
                is_spent = true;
                return Ok(PrefixScanControl::Stop);
            }

            Ok(PrefixScanControl::Continue)
        },
    )?;

    Ok(is_spent)
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

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
