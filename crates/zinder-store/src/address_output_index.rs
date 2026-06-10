//! Transparent output read traits.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentUnspentOutput,
};

use crate::{
    StoreError,
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
    ) -> Result<Vec<TransparentUnspentOutput>, StoreError>;
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
) -> Result<Vec<TransparentUnspentOutput>, StoreError> {
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

/// Reads unspent transparent outputs from the address-output projection.
///
/// Two-phase read: the prefix walk collects candidate rows without any
/// per-row store reads, then visibility resolves once per distinct creation
/// height and spend facts resolve in sliced batched reads. The projection
/// holds unspent rows plus at most one reorg window of recent spends, so
/// the candidate set stays bounded.
pub(crate) fn read_address_output_index_rows_paged(
    inner: &impl RocksChainStoreRead,
    scan: AddressOutputIndexRowsScan,
) -> Result<Vec<TransparentUnspentOutput>, StoreError> {
    let prefix =
        StoreKey::address_output_index_prefix(scan.chain_epoch.network, scan.address_script_hash);
    let mut candidates = Vec::new();

    inner.scan_prefix(
        StorageTable::AddressOutputIndex,
        &prefix,
        &mut |_key_bytes, envelope_bytes| {
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

            candidates.push(output);
            Ok(PrefixScanControl::Continue)
        },
    )?;

    let visible_block_hash_by_height = resolve_visible_block_hashes(
        inner,
        scan.chain_epoch,
        candidates.iter().map(|output| output.block_height),
    )?;
    let spent_outpoints = resolve_visible_spent_outpoints(
        inner,
        scan.chain_epoch,
        candidates.iter().map(|output| output.outpoint),
    )?;

    let max_entries = u32_to_usize(scan.max_entries.get());
    let mut outputs = Vec::new();
    for output in candidates {
        if visible_block_hash_by_height.get(&output.block_height) != Some(&Some(output.block_hash))
            || spent_outpoints.contains(&output.outpoint)
        {
            continue;
        }

        outputs.push(output);
        if outputs.len() >= max_entries {
            break;
        }
    }

    Ok(outputs)
}

fn resolve_visible_block_hashes(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    heights: impl Iterator<Item = BlockHeight>,
) -> Result<HashMap<BlockHeight, Option<BlockHash>>, StoreError> {
    let mut visible_block_hash_by_height = HashMap::new();
    for height in heights {
        if visible_block_hash_by_height.contains_key(&height) {
            continue;
        }
        let block_hash =
            read_block_header_artifact(inner, chain_epoch, height)?.map(|block| block.block_hash);
        visible_block_hash_by_height.insert(height, block_hash);
    }

    Ok(visible_block_hash_by_height)
}

fn resolve_visible_spent_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    outpoints: impl Iterator<Item = TransparentOutPoint>,
) -> Result<HashSet<TransparentOutPoint>, StoreError> {
    let outpoints = outpoints.collect::<Vec<_>>();
    let mut spent_outpoints = HashSet::new();
    for outpoint_slice in outpoints.chunks(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST) {
        spent_outpoints.extend(
            read_visible_transparent_spend_facts_by_outpoints(inner, chain_epoch, outpoint_slice)?
                .into_keys(),
        );
    }

    Ok(spent_outpoints)
}

const fn is_strictly_after_cursor(
    output: &TransparentUnspentOutput,
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

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
