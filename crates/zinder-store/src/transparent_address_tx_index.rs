//! Transparent-address tx-history index read traits.
//!
//! Storage uses dynamic-filter visibility: rows are written and never
//! physically deleted on reorg. Visibility is enforced at read time through
//! the trailing `chain_epoch_id` source-epoch filter and `block_is_visible`
//! against the row's stored `block_hash`.

use std::num::NonZeroU32;

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact,
};

use crate::{
    ArtifactFamily, StoreError,
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_transparent_address_tx_index_artifact},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
};

/// Read boundary for transparent-address tx-history artifacts.
pub trait TransparentAddressTxIndexStore {
    /// Reads the indexed transactions associated with `address_script_hash`
    /// inside `[start_height, end_height]`, in ascending mined order.
    fn transparent_address_tx_ids_in_range(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        start_height: BlockHeight,
        end_height: BlockHeight,
        max_entries: NonZeroU32,
    ) -> Result<Vec<TransparentAddressTxIndexArtifact>, StoreError>;
}

/// Scan parameters for [`read_transparent_address_tx_index_paged`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransparentAddressTxIndexScan {
    pub(crate) chain_epoch: ChainEpoch,
    pub(crate) address_script_hash: TransparentAddressScriptHash,
    pub(crate) start_height: BlockHeight,
    pub(crate) end_height: BlockHeight,
    pub(crate) max_entries: NonZeroU32,
    pub(crate) descending: bool,
    pub(crate) resume_after: Option<TransparentHistoryResumePosition>,
}

/// Position recorded by a transparent-history cursor. The store decodes its
/// public cursor token into this and seeds the iterator past it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransparentHistoryResumePosition {
    pub(crate) last_block_height: BlockHeight,
    pub(crate) last_tx_index_in_block: u32,
}

pub(crate) fn read_transparent_address_tx_index_paged(
    inner: &impl RocksChainStoreRead,
    scan: TransparentAddressTxIndexScan,
) -> Result<Vec<TransparentAddressTxIndexArtifact>, StoreError> {
    let prefix = StoreKey::transparent_address_tx_index_address_prefix(
        scan.chain_epoch.network,
        scan.address_script_hash,
    );
    let max_entries = u32_to_usize(scan.max_entries.get());
    let mut artifacts = Vec::new();
    let mut seen_positions = std::collections::HashSet::new();

    let mut visit_row = |key_bytes: &[u8], envelope_bytes: &[u8]| {
        let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes) else {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::TransparentAddressTxIndex,
                key: prefix.clone().into(),
                reason: "transparent address tx index key is malformed",
            });
        };
        if source_epoch > scan.chain_epoch.id {
            return Ok(PrefixScanControl::Continue);
        }

        let Some((row_height, row_tx_index)) = parse_height_and_tx_index(key_bytes) else {
            return Err(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::TransparentAddressTxIndex,
                key: StoreKey::from_raw_bytes(key_bytes).into(),
                reason: "transparent address tx index key is missing the height/tx_index fields",
            });
        };

        if row_height < scan.start_height {
            if scan.descending {
                return Ok(PrefixScanControl::Stop);
            }
            return Ok(PrefixScanControl::Continue);
        }
        if row_height > scan.end_height {
            if scan.descending {
                return Ok(PrefixScanControl::Continue);
            }
            return Ok(PrefixScanControl::Stop);
        }
        if !position_passes_cursor(row_height, row_tx_index, scan.descending, scan.resume_after) {
            return Ok(PrefixScanControl::Continue);
        }

        let artifact = decode_transparent_address_tx_index_artifact(
            &prefix,
            envelope_bytes,
            scan.address_script_hash,
            row_height,
            row_tx_index,
        )?;
        if !block_is_visible(
            inner,
            scan.chain_epoch,
            artifact.block_height,
            artifact.block_hash,
        )? {
            return Ok(PrefixScanControl::Continue);
        }
        if !seen_positions.insert((row_height, row_tx_index)) {
            return Ok(PrefixScanControl::Continue);
        }

        artifacts.push(artifact);
        if artifacts.len() >= max_entries {
            return Ok(PrefixScanControl::Stop);
        }

        Ok(PrefixScanControl::Continue)
    };

    if scan.descending {
        inner.scan_prefix_reverse(
            StorageTable::TransparentAddressTxIndex,
            &prefix,
            &mut visit_row,
        )?;
    } else {
        inner.scan_prefix(
            StorageTable::TransparentAddressTxIndex,
            &prefix,
            &mut visit_row,
        )?;
    }

    Ok(artifacts)
}

const fn position_passes_cursor(
    row_height: BlockHeight,
    row_tx_index: u32,
    descending: bool,
    resume_after: Option<TransparentHistoryResumePosition>,
) -> bool {
    match resume_after {
        None => true,
        Some(resume) => {
            if descending {
                if row_height.value() != resume.last_block_height.value() {
                    return row_height.value() < resume.last_block_height.value();
                }
                row_tx_index < resume.last_tx_index_in_block
            } else {
                if row_height.value() != resume.last_block_height.value() {
                    return row_height.value() > resume.last_block_height.value();
                }
                row_tx_index > resume.last_tx_index_in_block
            }
        }
    }
}

fn parse_height_and_tx_index(key_bytes: &[u8]) -> Option<(BlockHeight, u32)> {
    // Layout: [version, kind, network(4), address_hash(32), height(4), tx_index(4), epoch(8)]
    const HEIGHT_OFFSET: usize = 2 + 4 + 32;
    const HEIGHT_END: usize = HEIGHT_OFFSET + 4;
    const TX_INDEX_END: usize = HEIGHT_END + 4;
    if key_bytes.len() < TX_INDEX_END + 8 {
        return None;
    }
    let height_bytes = <[u8; 4]>::try_from(&key_bytes[HEIGHT_OFFSET..HEIGHT_END]).ok()?;
    let tx_index_bytes = <[u8; 4]>::try_from(&key_bytes[HEIGHT_END..TX_INDEX_END]).ok()?;
    Some((
        BlockHeight::new(u32::from_be_bytes(height_bytes)),
        u32::from_be_bytes(tx_index_bytes),
    ))
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
