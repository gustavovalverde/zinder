//! Transparent output read traits.

use std::{collections::HashSet, num::NonZeroU32};

use zinder_core::{
    AddressOutputIndexArtifact, BlockHash, BlockHeight, ChainEpoch, TransparentAddressScriptHash,
    TransparentOutPoint,
};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_header_artifact,
    format::{AddressOutputCursorPayload, StoreKey, decode_address_output_index_artifact},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
    transparent_spend_fact::read_visible_transparent_spend_facts_by_outpoints,
};

/// Read boundary for transparent address output artifacts.
pub trait AddressOutputIndexStore {
    /// Reads unspent transparent outputs for `address_script_hash` in the reader's chain epoch.
    fn address_output_index(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<AddressOutputIndexArtifact>, StoreError>;
}

/// Scan parameters for [`read_address_output_index_rows_paged`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct AddressOutputIndexRowsScan {
    pub(crate) chain_epoch: ChainEpoch,
    pub(crate) address_script_hash: TransparentAddressScriptHash,
    pub(crate) start_height: BlockHeight,
    pub(crate) max_entries: NonZeroU32,
    pub(crate) resume_after: Option<AddressOutputCursorPayload>,
}

pub(crate) fn read_address_output_index(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    address_script_hash: TransparentAddressScriptHash,
    start_height: BlockHeight,
    max_entries: NonZeroU32,
) -> Result<Vec<AddressOutputIndexArtifact>, StoreError> {
    read_address_output_index_rows_paged(
        inner,
        AddressOutputIndexRowsScan {
            chain_epoch,
            address_script_hash,
            start_height,
            max_entries,
            resume_after: None,
        },
    )
}

pub(crate) fn read_address_output_index_rows_paged(
    inner: &impl RocksChainStoreRead,
    scan: AddressOutputIndexRowsScan,
) -> Result<Vec<AddressOutputIndexArtifact>, StoreError> {
    let prefix =
        StoreKey::address_output_index_prefix(scan.chain_epoch.network, scan.address_script_hash);
    let mut outputs = Vec::new();
    let mut seen_outpoints = HashSet::new();
    let max_entries = u32_to_usize(scan.max_entries.get());

    inner.scan_prefix(
        StorageTable::AddressOutputIndex,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                return Err(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::AddressOutputIndex,
                    key: prefix.clone().into(),
                    reason: "transparent address output key is malformed",
                });
            };
            if source_epoch > scan.chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            let output = decode_address_output_index_artifact(&prefix, envelope_bytes)?;
            if output.address_script_hash != scan.address_script_hash {
                return Ok(PrefixScanControl::Continue);
            }
            if let Some(resume) = scan.resume_after {
                if !is_strictly_after_cursor(&output, resume) {
                    return Ok(PrefixScanControl::Continue);
                }
            } else if output.block_height < scan.start_height {
                return Ok(PrefixScanControl::Continue);
            }
            if !block_is_visible(
                inner,
                scan.chain_epoch,
                output.block_height,
                output.block_hash,
            )? || transparent_outpoint_is_spent(inner, scan.chain_epoch, output.outpoint)?
                || !seen_outpoints.insert(output.outpoint)
            {
                return Ok(PrefixScanControl::Continue);
            }

            outputs.push(output);
            if outputs.len() >= max_entries {
                return Ok(PrefixScanControl::Stop);
            }

            Ok(PrefixScanControl::Continue)
        },
    )?;

    Ok(outputs)
}

const fn is_strictly_after_cursor(
    output: &AddressOutputIndexArtifact,
    cursor: AddressOutputCursorPayload,
) -> bool {
    if output.block_height.value() != cursor.last_block_height.value() {
        return output.block_height.value() > cursor.last_block_height.value();
    }
    let utxo_txid = output.outpoint.transaction_id.as_bytes();
    let cursor_txid = cursor.last_outpoint.transaction_id.as_bytes();
    let mut byte_index = 0;
    while byte_index < utxo_txid.len() {
        if utxo_txid[byte_index] != cursor_txid[byte_index] {
            return utxo_txid[byte_index] > cursor_txid[byte_index];
        }
        byte_index += 1;
    }
    output.outpoint.output_index > cursor.last_outpoint.output_index
}

fn transparent_outpoint_is_spent(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoint: TransparentOutPoint,
) -> Result<bool, StoreError> {
    read_visible_transparent_spend_facts_by_outpoints(inner, chain_epoch, &[outpoint])
        .map(|spends| spends.contains_key(&outpoint))
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

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
