//! Transparent output read traits.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use zinder_core::{
    BlockHash, BlockHeight, ChainEpoch, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentUnspentOutput,
    TransparentUtxoSetCommitment, wire::UtxoSetCommitmentElement,
};

use crate::{
    StoreError,
    block_artifact::read_block_header_artifact,
    format::{AddressOutputCursorPayload, StoreKey, decode_address_output_index_artifact},
    kv::{PrefixScanControl, RocksChainStoreRead, StorageTable},
    transparent_spend_fact::{
        read_current_transparent_spend_facts_by_outpoints,
        read_visible_transparent_spend_facts_by_outpoints,
    },
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

    /// Reads current positive transparent balances at the reader's visible epoch.
    fn transparent_address_balance_snapshot(
        &self,
    ) -> Result<TransparentAddressBalanceSnapshot, StoreError>;

    /// Reads positive transparent balances at the current settled tip.
    fn settled_transparent_address_balance_snapshot(
        &self,
    ) -> Result<TransparentAddressBalanceSnapshot, StoreError>;
}

/// Current balance for every visible unspent output sharing one script hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressBalanceSummary {
    /// Canonical raw `scriptPubKey`; script classification remains edge-owned.
    pub script_pub_key: Vec<u8>,
    /// Checked sum of visible unspent output values, in zatoshi.
    pub balance_zat: u64,
    /// Number of visible unspent outputs included in `balance_zat`.
    pub utxo_count: u64,
}

/// Exact current transparent balance snapshot bound to one visible chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressBalanceSnapshot {
    /// One positive balance per canonical raw-script hash.
    pub balances_by_script_hash:
        HashMap<TransparentAddressScriptHash, TransparentAddressBalanceSummary>,
    /// Number of address-output projection rows inspected.
    pub indexed_output_count: u64,
    /// Number of visible unspent outputs, including zero-valued outputs.
    pub utxo_count: u64,
    /// Number of hashes with a positive balance.
    pub positive_script_hash_count: u64,
    /// Checked sum of all positive balances, in zatoshi.
    pub total_positive_balance_zat: u64,
    /// Visible tip height summarized by this snapshot.
    pub summarized_height: BlockHeight,
    /// Chain epoch that bounds every field in this snapshot.
    pub chain_epoch: ChainEpoch,
}

#[derive(Default)]
struct TransparentAddressBalanceAccumulator {
    balances_by_script_hash:
        HashMap<TransparentAddressScriptHash, TransparentAddressBalanceSummary>,
    visible_block_hash_by_height: HashMap<BlockHeight, Option<BlockHash>>,
    indexed_output_count: u64,
    utxo_count: u64,
    total_positive_balance_zat: u64,
}

#[derive(Clone, Copy)]
struct TransparentAddressBalanceBoundary {
    chain_epoch: ChainEpoch,
    summarized_height: BlockHeight,
}

/// Streams the network-wide current-UTXO projection into one balance per raw
/// script hash at `chain_epoch.visible_tip_height`.
pub(crate) fn read_transparent_address_balance_snapshot(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    summarized_height: BlockHeight,
) -> Result<TransparentAddressBalanceSnapshot, StoreError> {
    let prefix = StoreKey::address_output_index_network_prefix(chain_epoch.network);
    let boundary = TransparentAddressBalanceBoundary {
        chain_epoch,
        summarized_height,
    };
    let mut accumulator = TransparentAddressBalanceAccumulator::default();
    let mut candidates = Vec::with_capacity(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);

    inner.scan_prefix(
        StorageTable::AddressOutputIndex,
        &prefix,
        &mut |_key_bytes, envelope_bytes| {
            let output = decode_address_output_index_artifact(&prefix, envelope_bytes)?;
            accumulator.indexed_output_count = checked_add(
                &prefix,
                accumulator.indexed_output_count,
                1,
                "transparent balance snapshot indexed-output count overflow",
            )?;
            candidates.push(output);
            if candidates.len() == MAX_TRANSPARENT_OUTPUTS_PER_REQUEST {
                accumulate_visible_balances(
                    inner,
                    boundary,
                    &prefix,
                    &candidates,
                    &mut accumulator,
                )?;
                candidates.clear();
            }
            Ok(PrefixScanControl::Continue)
        },
    )?;
    accumulate_visible_balances(inner, boundary, &prefix, &candidates, &mut accumulator)?;

    accumulator
        .balances_by_script_hash
        .retain(|_, summary| summary.balance_zat > 0);
    let positive_script_hash_count = u64::try_from(accumulator.balances_by_script_hash.len())
        .map_err(|_| {
            corrupt_snapshot(&prefix, "transparent balance snapshot hash count overflow")
        })?;

    Ok(TransparentAddressBalanceSnapshot {
        balances_by_script_hash: accumulator.balances_by_script_hash,
        indexed_output_count: accumulator.indexed_output_count,
        utxo_count: accumulator.utxo_count,
        positive_script_hash_count,
        total_positive_balance_zat: accumulator.total_positive_balance_zat,
        summarized_height,
        chain_epoch,
    })
}

fn accumulate_visible_balances(
    inner: &impl RocksChainStoreRead,
    boundary: TransparentAddressBalanceBoundary,
    prefix: &StoreKey,
    candidates: &[TransparentUnspentOutput],
    accumulator: &mut TransparentAddressBalanceAccumulator,
) -> Result<(), StoreError> {
    if candidates.is_empty() {
        return Ok(());
    }

    let outpoints = candidates
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    let spent_outpoints =
        read_current_transparent_spend_facts_by_outpoints(inner, boundary.chain_epoch, &outpoints)?;

    for output in candidates {
        if output.block_height > boundary.summarized_height
            || spent_outpoints
                .get(&output.outpoint)
                .is_some_and(|spend| spend.block_height <= boundary.summarized_height)
            || !creation_is_visible(inner, boundary, output, accumulator)?
        {
            continue;
        }

        accumulator.utxo_count = checked_add(
            prefix,
            accumulator.utxo_count,
            1,
            "transparent balance snapshot UTXO count overflow",
        )?;
        accumulator.total_positive_balance_zat = checked_add(
            prefix,
            accumulator.total_positive_balance_zat,
            output.value_zat,
            "transparent balance snapshot total balance overflow",
        )?;

        match accumulator
            .balances_by_script_hash
            .get_mut(&output.address_script_hash)
        {
            Some(summary) if summary.script_pub_key != output.script_pub_key => {
                return Err(corrupt_snapshot(
                    prefix,
                    "transparent balance snapshot contains conflicting scripts for one hash",
                ));
            }
            Some(summary) => {
                summary.balance_zat = checked_add(
                    prefix,
                    summary.balance_zat,
                    output.value_zat,
                    "transparent address balance overflow",
                )?;
                summary.utxo_count = checked_add(
                    prefix,
                    summary.utxo_count,
                    1,
                    "transparent address UTXO count overflow",
                )?;
            }
            None => {
                accumulator.balances_by_script_hash.insert(
                    output.address_script_hash,
                    TransparentAddressBalanceSummary {
                        script_pub_key: output.script_pub_key.clone(),
                        balance_zat: output.value_zat,
                        utxo_count: 1,
                    },
                );
            }
        }
    }

    Ok(())
}

fn creation_is_visible(
    inner: &impl RocksChainStoreRead,
    boundary: TransparentAddressBalanceBoundary,
    output: &TransparentUnspentOutput,
    accumulator: &mut TransparentAddressBalanceAccumulator,
) -> Result<bool, StoreError> {
    if output.block_height <= boundary.chain_epoch.settled_tip_height {
        return Ok(true);
    }

    if output.block_height > boundary.summarized_height {
        return Ok(false);
    }

    if let Some(block_hash) = accumulator
        .visible_block_hash_by_height
        .get(&output.block_height)
    {
        return Ok(*block_hash == Some(output.block_hash));
    }

    let block_hash = read_block_header_artifact(inner, boundary.chain_epoch, output.block_height)?
        .map(|block| block.block_hash);
    accumulator
        .visible_block_hash_by_height
        .insert(output.block_height, block_hash);
    Ok(block_hash == Some(output.block_hash))
}

fn checked_add(
    prefix: &StoreKey,
    left: u64,
    right: u64,
    reason: &'static str,
) -> Result<u64, StoreError> {
    left.checked_add(right)
        .ok_or_else(|| corrupt_snapshot(prefix, reason))
}

fn corrupt_snapshot(prefix: &StoreKey, reason: &'static str) -> StoreError {
    StoreError::ArtifactCorrupt {
        family: crate::ArtifactFamily::AddressOutputIndex,
        key: prefix.clone().into(),
        reason,
    }
}

/// Chain-wide aggregate of the transparent UTXO-set projection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TransparentUtxoSetAggregate {
    /// Number of unspent transparent outputs at or below the settled tip.
    pub(crate) utxo_count: u64,
    /// Sum of the values of those outputs, in zatoshi.
    pub(crate) total_value_zat: u64,
    /// Homomorphic commitment to the same outputs, present only when the fold
    /// was asked to compute it.
    pub(crate) commitment: Option<TransparentUtxoSetCommitment>,
}

/// Streams the whole current-UTXO projection for one network and accumulates a
/// count and value sum over the outputs created at or below the chain epoch's
/// settled tip.
///
/// Rows at heights at or below `settled_tip_height` are the irreversible
/// unspent set: reorgs cannot reach them, and transparent-retention maintenance has
/// already deleted finalized spends and reverted creations. The scan therefore
/// needs neither a producing-block visibility check nor a spend re-check. Rows
/// above the settled tip live inside the reorg window and are excluded so the
/// aggregate cannot count an output a later reorg or spend could remove. The
/// accumulator is two integers, so memory stays constant regardless of set
/// size.
///
/// The projection keys every transparent output by the SHA-256 of its raw
/// `scriptPubKey`, including non-standard and provably-unspendable scripts
/// (`OP_RETURN`, bare data outputs). Such outputs are counted here, so the totals
/// are the full unspent set and not zcashd's `IsUnspendable`-filtered set.
///
/// When `commitment_enabled` is set, the same loop folds every counted output
/// into a [`TransparentUtxoSetCommitment`] (LtHash16): each surviving row's
/// outpoint, value, raw script, and height are the exact element the commitment
/// binds, so no extra read is needed. The fold has real per-output CPU cost, so
/// the commitment is computed only when the operator opts in.
pub(crate) fn read_transparent_utxo_set_aggregate(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    commitment_enabled: bool,
) -> Result<TransparentUtxoSetAggregate, StoreError> {
    let prefix = StoreKey::address_output_index_network_prefix(chain_epoch.network);
    let settled_tip_height = chain_epoch.settled_tip_height;
    let network_id = chain_epoch.network.id();
    let mut aggregate = TransparentUtxoSetAggregate::default();
    let mut commitment = commitment_enabled.then(TransparentUtxoSetCommitment::empty);

    inner.scan_prefix(
        StorageTable::AddressOutputIndex,
        &prefix,
        &mut |_key_bytes, envelope_bytes| {
            let output = decode_address_output_index_artifact(&prefix, envelope_bytes)?;
            if output.block_height > settled_tip_height {
                return Ok(PrefixScanControl::Continue);
            }
            aggregate.utxo_count = aggregate.utxo_count.saturating_add(1);
            aggregate.total_value_zat = aggregate.total_value_zat.saturating_add(output.value_zat);
            if let Some(commitment) = commitment.as_mut() {
                commitment.insert(&UtxoSetCommitmentElement {
                    network_id,
                    outpoint: output.outpoint,
                    value_zat: output.value_zat,
                    script_pub_key: &output.script_pub_key,
                    block_height: output.block_height,
                });
            }
            Ok(PrefixScanControl::Continue)
        },
    )?;

    aggregate.commitment = commitment;
    Ok(aggregate)
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
