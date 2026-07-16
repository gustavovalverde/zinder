//! Bounded complete-history sorting and output/spend merge construction.

use std::{
    collections::{BTreeSet, VecDeque},
    mem::size_of,
    time::{Duration, Instant},
};

use thiserror::Error;
use zinder_core::wire::UtxoSetCommitmentElement;
use zinder_core::{
    BlockHeight, BlockId, CanonicalBlockFacts, CanonicalBlockFactsDigest,
    CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigest,
    CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    CanonicalBlockFactsSequenceLengthOverflow, Network, TransparentAddressScriptHash,
    TransparentUtxoSetCommitment, ValidatedCanonicalBlockReplay,
};
use zinder_store::CanonicalStoreError;
use zinder_wallet_projection::{
    WalletAddressBalance, WalletAddressTransaction, WalletAddressTransactionKey,
    WalletAddressUnspentOutputKey, WalletOutpointKey, WalletProjectionContractError,
    WalletProjectionDigest, WalletProjectionDigestBuilder, WalletProjectionFamilyRowCounts,
    WalletProjectionRowFamily, WalletReorgUndo, WalletSpentOutput, WalletTransactionPosition,
    WalletUnspentOutput, WalletUtxoSetSummary,
};

/// One canonical-facts value accepted by the complete-history scan.
pub(crate) trait CanonicalFactsInput {
    fn into_authenticated_facts(self) -> Result<AuthenticatedCanonicalFacts, WalletSortMergeError>;
}

impl CanonicalFactsInput for CanonicalBlockFacts {
    fn into_authenticated_facts(self) -> Result<AuthenticatedCanonicalFacts, WalletSortMergeError> {
        let reference_digest = self.digest(CanonicalBlockFactsDigestVersion::V1);
        Ok(AuthenticatedCanonicalFacts {
            facts: self,
            reference_digest,
        })
    }
}

impl CanonicalFactsInput for ValidatedCanonicalBlockReplay {
    fn into_authenticated_facts(self) -> Result<AuthenticatedCanonicalFacts, WalletSortMergeError> {
        let reference_digest = self.reference_digest();
        Ok(AuthenticatedCanonicalFacts {
            facts: self.into_facts(),
            reference_digest,
        })
    }
}

impl<Input> CanonicalFactsInput for &Input
where
    Input: CanonicalFactsInput + Clone,
{
    fn into_authenticated_facts(self) -> Result<AuthenticatedCanonicalFacts, WalletSortMergeError> {
        (*self).clone().into_authenticated_facts()
    }
}

impl CanonicalFactsInput for Result<ValidatedCanonicalBlockReplay, CanonicalStoreError> {
    fn into_authenticated_facts(self) -> Result<AuthenticatedCanonicalFacts, WalletSortMergeError> {
        self.map_err(WalletSortMergeError::CanonicalScan)?
            .into_authenticated_facts()
    }
}

pub(crate) struct AuthenticatedCanonicalFacts {
    facts: CanonicalBlockFacts,
    reference_digest: CanonicalBlockFactsDigest,
}

/// Deterministic failure from bounded complete-history preparation.
#[derive(Debug, Error)]
pub(crate) enum WalletSortMergeError {
    /// A wallet projection contract was violated by canonical facts or derived rows.
    #[error(transparent)]
    Contract(#[from] WalletProjectionContractError),
    /// The canonical scan contained no block and therefore has no publishable tip.
    #[error("complete-history wallet construction requires at least block height 1")]
    EmptyCanonicalHistory,
    /// The canonical source sequence exceeded the version-1 count domain.
    #[error(transparent)]
    SourceSequenceLength(#[from] CanonicalBlockFactsSequenceLengthOverflow),
    /// The authenticated canonical replay iterator refused one record or its final fence.
    #[error("canonical replay scan failed")]
    CanonicalScan(#[source] CanonicalStoreError),
    /// Accounted preparation memory would exceed the caller's explicit limit.
    #[error(
        "wallet sort/merge requires at least {required_bytes} accounted bytes, limit is {limit_bytes}"
    )]
    AccountedMemoryLimit {
        /// Caller-supplied hard limit for accounted row and staging payloads.
        limit_bytes: u64,
        /// Minimum accounted bytes needed by the refused operation.
        required_bytes: u64,
    },
    /// A scan or row counter exceeded the report contract.
    #[error("wallet sort/merge counter exceeds u64::MAX")]
    CounterOverflow,
}

/// Counts that prove the fixed-tip build used a single scan and no prevout reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalletSortMergeCounters {
    pub(crate) scanned_block_count: u64,
    pub(crate) scanned_transaction_count: u64,
    pub(crate) staged_output_count: u64,
    pub(crate) staged_spend_count: u64,
    pub(crate) historical_prevout_read_count: u64,
    pub(crate) peak_accounted_bytes: u64,
    pub(crate) max_accounted_bytes: u64,
}

/// Preparation timings kept separate so deployment evidence can rank costs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalletSortMergePhaseDurations {
    pub(crate) canonical_scan: Duration,
    pub(crate) outpoint_sort: Duration,
    pub(crate) outpoint_merge: Duration,
    pub(crate) secondary_row_derivation: Duration,
    pub(crate) logical_evidence: Duration,
}

/// Complete version-1 logical rows ready for one concrete storage writer.
#[derive(Debug)]
pub(crate) struct PreparedWalletProjection {
    pub(crate) network: Network,
    pub(crate) supported_reorg_depth: u32,
    pub(crate) first_block: BlockId,
    pub(crate) tip: BlockId,
    pub(crate) source_sequence_digest: CanonicalBlockFactsSequenceDigest,
    pub(crate) unspent_outputs: Vec<(WalletOutpointKey, WalletUnspentOutput)>,
    pub(crate) unspent_output_by_address: Vec<WalletAddressUnspentOutputKey>,
    pub(crate) spent_outputs: Vec<(WalletOutpointKey, WalletSpentOutput)>,
    pub(crate) address_transactions: Vec<WalletAddressTransaction>,
    pub(crate) address_balances: Vec<WalletAddressBalance>,
    pub(crate) reorg_undo: Vec<WalletReorgUndo>,
    pub(crate) row_counts: WalletProjectionFamilyRowCounts,
    pub(crate) utxo_summary: WalletUtxoSetSummary,
    pub(crate) projection_digest: WalletProjectionDigest,
    pub(crate) counters: WalletSortMergeCounters,
    pub(crate) phase_durations: WalletSortMergePhaseDurations,
}

#[derive(Debug)]
struct StagedOutput {
    key: WalletOutpointKey,
    output: WalletUnspentOutput,
}

#[derive(Clone, Copy, Debug)]
struct StagedSpend {
    key: WalletOutpointKey,
    spent_at: WalletTransactionPosition,
    input_index: u32,
}

type UnspentOutputRows = Vec<(WalletOutpointKey, WalletUnspentOutput)>;
type SpentOutputRows = Vec<(WalletOutpointKey, WalletSpentOutput)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AddressTouch {
    address_script_hash: TransparentAddressScriptHash,
    transaction: WalletTransactionPosition,
}

#[derive(Debug)]
struct AccountedMemory {
    limit: u64,
    current: u64,
    peak: u64,
}

impl AccountedMemory {
    const fn new(limit_bytes: u64) -> Self {
        Self {
            limit: limit_bytes,
            current: 0,
            peak: 0,
        }
    }

    fn reserve(&mut self, additional_bytes: usize) -> Result<(), WalletSortMergeError> {
        let additional_bytes =
            u64::try_from(additional_bytes).map_err(|_| WalletSortMergeError::CounterOverflow)?;
        let required_bytes = self
            .current
            .checked_add(additional_bytes)
            .ok_or(WalletSortMergeError::CounterOverflow)?;
        if required_bytes > self.limit {
            return Err(WalletSortMergeError::AccountedMemoryLimit {
                limit_bytes: self.limit,
                required_bytes,
            });
        }
        self.current = required_bytes;
        self.peak = self.peak.max(required_bytes);
        Ok(())
    }

    fn release(&mut self, released_bytes: usize) {
        let released_bytes = u64::try_from(released_bytes).unwrap_or(u64::MAX);
        self.current = self.current.saturating_sub(released_bytes);
    }
}

/// Prepares complete version-1 wallet rows from one contiguous canonical scan.
///
/// `max_accounted_bytes` limits deterministic row metadata and owned variable
/// payload bytes. It deliberately does not claim to constrain allocator or
/// sorting-library overhead. Production histories that exceed this tracer's
/// cap require the external-run implementation, not a larger implicit `Vec`.
#[allow(
    clippy::too_many_lines,
    reason = "the single-scan boundary keeps validation, staging, and evidence visibly connected"
)]
pub(crate) fn prepare_wallet_projection<Blocks, Input>(
    network: Network,
    supported_reorg_depth: u32,
    max_accounted_bytes: u64,
    blocks: Blocks,
) -> Result<PreparedWalletProjection, WalletSortMergeError>
where
    Blocks: IntoIterator<Item = Input>,
    Input: CanonicalFactsInput,
{
    let phase_started = Instant::now();
    let mut memory = AccountedMemory::new(max_accounted_bytes);
    let mut outputs = Vec::new();
    let mut spends = Vec::new();
    let mut retained_undo = VecDeque::new();
    let mut sequence_digest =
        CanonicalBlockFactsSequenceDigestBuilder::new(CanonicalBlockFactsSequenceDigestVersion::V1);
    let mut first_block = None;
    let mut previous_block = None;
    let mut scanned_block_count = 0_u64;
    let mut scanned_transaction_count = 0_u64;

    for input in blocks {
        let authenticated_facts = input.into_authenticated_facts()?;
        let facts = &authenticated_facts.facts;
        let block = BlockId::new(facts.block_header.height, facts.block_header.block_hash);
        validate_next_block(previous_block, block, facts.block_header.parent_hash)?;
        first_block.get_or_insert(block);
        sequence_digest.try_append(authenticated_facts.reference_digest)?;
        scanned_block_count = increment(scanned_block_count)?;

        let mut block_undo = if supported_reorg_depth == 0 {
            None
        } else {
            memory.reserve(size_of::<WalletReorgUndo>())?;
            Some(WalletReorgUndo {
                block,
                created_outpoints: Vec::new(),
                spent_outpoints: Vec::new(),
                address_transaction_keys: Vec::new(),
            })
        };
        let mut block_created_outpoints = BTreeSet::new();
        for (transaction_index, transaction) in facts.transactions.iter().enumerate() {
            let tx_index_in_block = u32::try_from(transaction_index)
                .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
            scanned_transaction_count = increment(scanned_transaction_count)?;
            let transaction_position = WalletTransactionPosition::new(
                transaction.public_facts.transaction_id,
                tx_index_in_block,
                block,
            );

            for (input_position, input) in transaction.transparent_inputs.iter().enumerate() {
                let expected_input_index = u32::try_from(input_position)
                    .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
                if input.input_index != expected_input_index {
                    return Err(WalletProjectionContractError::FactIndexMismatch.into());
                }
                if input.spent_outpoint.is_coinbase_sentinel() {
                    continue;
                }
                let key = WalletOutpointKey::new(input.spent_outpoint);
                memory.reserve(size_of::<StagedSpend>())?;
                spends.push(StagedSpend {
                    key,
                    spent_at: transaction_position,
                    input_index: input.input_index,
                });
                if let Some(undo) = &mut block_undo
                    && !block_created_outpoints.contains(&key)
                {
                    memory.reserve(size_of::<WalletOutpointKey>())?;
                    undo.spent_outpoints.push(key);
                }
            }

            for (output_position, output) in transaction.transparent_outputs.iter().enumerate() {
                let expected_output_index = u32::try_from(output_position)
                    .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
                if output.output_index != expected_output_index {
                    return Err(WalletProjectionContractError::FactIndexMismatch.into());
                }
                let outpoint = zinder_core::TransparentOutPoint::new(
                    transaction.public_facts.transaction_id,
                    output.output_index,
                );
                let key = WalletOutpointKey::new(outpoint);
                memory.reserve(
                    size_of::<StagedOutput>()
                        .checked_add(output.script_pub_key.len())
                        .ok_or(WalletSortMergeError::CounterOverflow)?,
                )?;
                let wallet_output = WalletUnspentOutput::new(
                    outpoint,
                    output.address_script_hash,
                    output.value_zat,
                    output.script_pub_key.clone(),
                    transaction_position,
                )?;
                outputs.push(StagedOutput {
                    key,
                    output: wallet_output,
                });
                if let Some(undo) = &mut block_undo {
                    memory.reserve(size_of::<WalletOutpointKey>())?;
                    block_created_outpoints.insert(key);
                    memory.reserve(size_of::<WalletOutpointKey>())?;
                    undo.created_outpoints.push(key);
                }
            }
        }

        if let Some(block_undo) = block_undo {
            memory.release(
                block_created_outpoints
                    .len()
                    .saturating_mul(size_of::<WalletOutpointKey>()),
            );
            retain_undo(
                &mut retained_undo,
                block_undo,
                supported_reorg_depth,
                &mut memory,
            )?;
        }
        previous_block = Some(block);
    }

    let first_block = first_block.ok_or(WalletSortMergeError::EmptyCanonicalHistory)?;
    let tip = previous_block.ok_or(WalletSortMergeError::EmptyCanonicalHistory)?;
    let canonical_scan = phase_started.elapsed();

    let phase_started = Instant::now();
    reject_duplicate_outputs(&mut outputs)?;
    reject_duplicate_spends(&mut spends)?;
    let staged_output_count = count(outputs.len())?;
    let staged_spend_count = count(spends.len())?;
    reserve_final_output_metadata(outputs.len(), &mut memory)?;
    let outpoint_sort = phase_started.elapsed();

    let phase_started = Instant::now();
    let (unspent_outputs, spent_outputs) = merge_output_spends(outputs, spends)?;
    let outpoint_merge = phase_started.elapsed();

    let phase_started = Instant::now();
    let mut address_touches =
        collect_address_touches(&unspent_outputs, &spent_outputs, &mut memory)?;
    address_touches.sort_unstable_by_key(address_touch_order);
    address_touches.dedup_by_key(|touch| address_touch_order(touch));
    append_undo_address_transactions(&address_touches, &mut retained_undo, &mut memory)?;
    memory.reserve(
        address_touches
            .len()
            .checked_mul(size_of::<WalletAddressTransaction>())
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )?;
    let mut address_transactions = address_touches
        .into_iter()
        .map(|touch| {
            let key = WalletAddressTransactionKey::new(
                touch.address_script_hash,
                touch.transaction.block.height,
                touch.transaction.tx_index_in_block,
            );
            WalletAddressTransaction::new(
                key,
                touch.transaction.transaction_id,
                touch.transaction.block.hash,
            )
        })
        .collect::<Vec<_>>();
    address_transactions.sort_unstable_by_key(|entry| entry.key);

    let (unspent_output_by_address, address_balances) =
        build_unspent_address_rows(&unspent_outputs, &mut memory)?;
    let reorg_undo = retained_undo.into_iter().collect::<Vec<_>>();
    let secondary_row_derivation = phase_started.elapsed();

    let phase_started = Instant::now();
    let utxo_summary = build_utxo_summary(network, &unspent_outputs)?;
    let rows = ProjectionRows {
        unspent_outputs: &unspent_outputs,
        unspent_output_by_address: &unspent_output_by_address,
        spent_outputs: &spent_outputs,
        address_transactions: &address_transactions,
        address_balances: &address_balances,
        reorg_undo: &reorg_undo,
    };
    let (row_counts, projection_digest) = projection_row_evidence(rows)?;
    let logical_evidence = phase_started.elapsed();
    let counters = WalletSortMergeCounters {
        scanned_block_count,
        scanned_transaction_count,
        staged_output_count,
        staged_spend_count,
        historical_prevout_read_count: 0,
        peak_accounted_bytes: memory.peak,
        max_accounted_bytes,
    };

    Ok(PreparedWalletProjection {
        network,
        supported_reorg_depth,
        first_block,
        tip,
        source_sequence_digest: sequence_digest.finish(),
        unspent_outputs,
        unspent_output_by_address,
        spent_outputs,
        address_transactions,
        address_balances,
        reorg_undo,
        row_counts,
        utxo_summary,
        projection_digest,
        counters,
        phase_durations: WalletSortMergePhaseDurations {
            canonical_scan,
            outpoint_sort,
            outpoint_merge,
            secondary_row_derivation,
            logical_evidence,
        },
    })
}

fn validate_next_block(
    previous: Option<BlockId>,
    block: BlockId,
    parent_hash: zinder_core::BlockHash,
) -> Result<(), WalletProjectionContractError> {
    match previous {
        None if block.height == BlockHeight::new(1) => Ok(()),
        Some(previous)
            if previous.height.next() == Some(block.height) && previous.hash == parent_hash =>
        {
            Ok(())
        }
        None | Some(_) => Err(WalletProjectionContractError::NonContiguousBlock),
    }
}

fn retain_undo(
    retained: &mut VecDeque<WalletReorgUndo>,
    mut undo: WalletReorgUndo,
    supported_reorg_depth: u32,
    memory: &mut AccountedMemory,
) -> Result<(), WalletSortMergeError> {
    undo.created_outpoints.sort_unstable();
    undo.spent_outpoints.sort_unstable();
    retained.push_back(undo);
    let retained_limit = usize::try_from(supported_reorg_depth).unwrap_or(usize::MAX);
    if retained.len() > retained_limit
        && let Some(expired) = retained.pop_front()
    {
        memory.release(accounted_undo_bytes(&expired)?);
    }
    Ok(())
}

fn reserve_final_output_metadata(
    output_count: usize,
    memory: &mut AccountedMemory,
) -> Result<(), WalletSortMergeError> {
    let largest_row = size_of::<(WalletOutpointKey, WalletUnspentOutput)>()
        .max(size_of::<(WalletOutpointKey, WalletSpentOutput)>());
    memory.reserve(
        output_count
            .checked_mul(largest_row)
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )
}

fn accounted_undo_bytes(undo: &WalletReorgUndo) -> Result<usize, WalletSortMergeError> {
    size_of::<WalletReorgUndo>()
        .checked_add(
            undo.created_outpoints
                .len()
                .checked_mul(size_of::<WalletOutpointKey>())
                .ok_or(WalletSortMergeError::CounterOverflow)?,
        )
        .and_then(|bytes| {
            bytes.checked_add(
                undo.spent_outpoints
                    .len()
                    .checked_mul(size_of::<WalletOutpointKey>())?,
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(
                undo.address_transaction_keys
                    .len()
                    .checked_mul(size_of::<WalletAddressTransactionKey>())?,
            )
        })
        .ok_or(WalletSortMergeError::CounterOverflow)
}

fn reject_duplicate_outputs(outputs: &mut [StagedOutput]) -> Result<(), WalletSortMergeError> {
    outputs.sort_unstable_by_key(|output| output.key);
    if outputs.windows(2).any(|pair| pair[0].key == pair[1].key) {
        return Err(WalletProjectionContractError::DuplicateOutput.into());
    }
    Ok(())
}

fn reject_duplicate_spends(spends: &mut [StagedSpend]) -> Result<(), WalletSortMergeError> {
    spends.sort_unstable_by_key(|spend| spend.key);
    if spends.windows(2).any(|pair| pair[0].key == pair[1].key) {
        return Err(WalletProjectionContractError::DuplicateSpend.into());
    }
    Ok(())
}

fn merge_output_spends(
    outputs: Vec<StagedOutput>,
    spends: Vec<StagedSpend>,
) -> Result<(UnspentOutputRows, SpentOutputRows), WalletSortMergeError> {
    let mut unspent_outputs = Vec::new();
    let mut spent_outputs = Vec::new();
    let mut output_iter = outputs.into_iter().peekable();
    let mut spend_iter = spends.into_iter().peekable();
    loop {
        match (output_iter.peek(), spend_iter.peek()) {
            (Some(output), Some(spend)) if output.key < spend.key => {
                if let Some(output) = output_iter.next() {
                    unspent_outputs.push((output.key, output.output));
                }
            }
            (Some(output), Some(spend)) if output.key == spend.key => {
                let Some(output) = output_iter.next() else {
                    return Err(WalletProjectionContractError::MissingTransparentPredecessor.into());
                };
                let Some(spend) = spend_iter.next() else {
                    return Err(WalletProjectionContractError::MissingTransparentPredecessor.into());
                };
                if !precedes(output.output.created_at, spend.spent_at) {
                    return Err(WalletProjectionContractError::MissingTransparentPredecessor.into());
                }
                spent_outputs.push((
                    output.key,
                    WalletSpentOutput::new(output.output, spend.spent_at, spend.input_index),
                ));
            }
            (Some(_) | None, Some(_)) => {
                return Err(WalletProjectionContractError::MissingTransparentPredecessor.into());
            }
            (Some(_), None) => {
                unspent_outputs.extend(output_iter.map(|output| (output.key, output.output)));
                break;
            }
            (None, None) => break,
        }
    }
    Ok((unspent_outputs, spent_outputs))
}

fn precedes(created_at: WalletTransactionPosition, spent_at: WalletTransactionPosition) -> bool {
    (
        created_at.block.height.value(),
        created_at.tx_index_in_block,
    ) < (spent_at.block.height.value(), spent_at.tx_index_in_block)
}

fn collect_address_touches(
    unspent_outputs: &[(WalletOutpointKey, WalletUnspentOutput)],
    spent_outputs: &[(WalletOutpointKey, WalletSpentOutput)],
    memory: &mut AccountedMemory,
) -> Result<Vec<AddressTouch>, WalletSortMergeError> {
    let touch_count = unspent_outputs
        .len()
        .checked_add(
            spent_outputs
                .len()
                .checked_mul(2)
                .ok_or(WalletSortMergeError::CounterOverflow)?,
        )
        .ok_or(WalletSortMergeError::CounterOverflow)?;
    memory.reserve(
        touch_count
            .checked_mul(size_of::<AddressTouch>())
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )?;
    let mut touches = Vec::with_capacity(touch_count);
    for (_, output) in unspent_outputs {
        touches.push(AddressTouch {
            address_script_hash: output.address_script_hash,
            transaction: output.created_at,
        });
    }
    for (_, spent) in spent_outputs {
        touches.push(AddressTouch {
            address_script_hash: spent.output.address_script_hash,
            transaction: spent.output.created_at,
        });
        touches.push(AddressTouch {
            address_script_hash: spent.output.address_script_hash,
            transaction: spent.spent_at,
        });
    }
    Ok(touches)
}

fn address_touch_order(touch: &AddressTouch) -> (u32, u32, [u8; 32]) {
    (
        touch.transaction.block.height.value(),
        touch.transaction.tx_index_in_block,
        touch.address_script_hash.as_bytes(),
    )
}

fn append_undo_address_transactions(
    touches: &[AddressTouch],
    retained_undo: &mut VecDeque<WalletReorgUndo>,
    memory: &mut AccountedMemory,
) -> Result<(), WalletSortMergeError> {
    let Some(first_height) = retained_undo.front().map(|undo| undo.block.height.value()) else {
        return Ok(());
    };
    for touch in touches
        .iter()
        .filter(|touch| touch.transaction.block.height.value() >= first_height)
    {
        let key = WalletAddressTransactionKey::new(
            touch.address_script_hash,
            touch.transaction.block.height,
            touch.transaction.tx_index_in_block,
        );
        if let Some(undo) = retained_undo
            .iter_mut()
            .find(|undo| undo.block.height == touch.transaction.block.height)
        {
            memory.reserve(size_of::<WalletAddressTransactionKey>())?;
            undo.address_transaction_keys.push(key);
        }
    }
    for undo in retained_undo {
        undo.address_transaction_keys.sort_unstable();
    }
    Ok(())
}

fn build_unspent_address_rows(
    unspent_outputs: &[(WalletOutpointKey, WalletUnspentOutput)],
    memory: &mut AccountedMemory,
) -> Result<
    (
        Vec<WalletAddressUnspentOutputKey>,
        Vec<WalletAddressBalance>,
    ),
    WalletSortMergeError,
> {
    memory.reserve(
        unspent_outputs
            .len()
            .checked_mul(size_of::<(WalletAddressUnspentOutputKey, u64)>())
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )?;
    memory.reserve(
        unspent_outputs
            .len()
            .checked_mul(size_of::<WalletAddressUnspentOutputKey>())
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )?;
    memory.reserve(
        unspent_outputs
            .len()
            .checked_mul(size_of::<WalletAddressBalance>())
            .ok_or(WalletSortMergeError::CounterOverflow)?,
    )?;
    let mut address_rows = unspent_outputs
        .iter()
        .map(|(_, output)| (WalletAddressUnspentOutputKey::new(output), output.value_zat))
        .collect::<Vec<_>>();
    address_rows.sort_unstable_by_key(|(key, _)| *key);

    let mut address_balances = Vec::new();
    let mut current_address = None;
    let mut current_balance = 0_u64;
    for (key, value_zat) in &address_rows {
        let address = key.address_script_hash();
        if current_address.is_some_and(|current| current != address) {
            let completed_address =
                current_address.ok_or(WalletProjectionContractError::AddressBalanceUnderflow)?;
            if current_balance > 0 {
                address_balances.push(WalletAddressBalance {
                    address_script_hash: completed_address,
                    balance_zat: current_balance,
                });
            }
            current_balance = 0;
        }
        current_address = Some(address);
        current_balance = current_balance
            .checked_add(*value_zat)
            .ok_or(WalletProjectionContractError::AddressBalanceOverflow)?;
    }
    if let Some(address_script_hash) = current_address.filter(|_| current_balance > 0) {
        address_balances.push(WalletAddressBalance {
            address_script_hash,
            balance_zat: current_balance,
        });
    }
    let keys = address_rows.into_iter().map(|(key, _)| key).collect();
    Ok((keys, address_balances))
}

fn build_utxo_summary(
    network: Network,
    unspent_outputs: &[(WalletOutpointKey, WalletUnspentOutput)],
) -> Result<WalletUtxoSetSummary, WalletSortMergeError> {
    let mut total_value_zat = 0_u64;
    let mut commitment = TransparentUtxoSetCommitment::empty();
    for (_, output) in unspent_outputs {
        total_value_zat = total_value_zat
            .checked_add(output.value_zat)
            .ok_or(WalletProjectionContractError::UtxoValueOverflow)?;
        commitment.insert(&UtxoSetCommitmentElement {
            network_id: network.id(),
            outpoint: output.outpoint,
            value_zat: output.value_zat,
            script_pub_key: &output.script_pub_key,
            block_height: output.created_at.block.height,
        });
    }
    Ok(WalletUtxoSetSummary {
        utxo_count: count(unspent_outputs.len())?,
        total_value_zat,
        commitment,
    })
}

#[derive(Clone, Copy)]
struct ProjectionRows<'rows> {
    unspent_outputs: &'rows [(WalletOutpointKey, WalletUnspentOutput)],
    unspent_output_by_address: &'rows [WalletAddressUnspentOutputKey],
    spent_outputs: &'rows [(WalletOutpointKey, WalletSpentOutput)],
    address_transactions: &'rows [WalletAddressTransaction],
    address_balances: &'rows [WalletAddressBalance],
    reorg_undo: &'rows [WalletReorgUndo],
}

fn projection_row_evidence(
    rows: ProjectionRows<'_>,
) -> Result<(WalletProjectionFamilyRowCounts, WalletProjectionDigest), WalletSortMergeError> {
    let mut digest = WalletProjectionDigestBuilder::new();
    for (key, output) in rows.unspent_outputs {
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutput,
            key.as_bytes(),
            &output.encode_value()?,
        )?;
    }
    for key in rows.unspent_output_by_address {
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            key.as_bytes(),
            &[],
        )?;
    }
    for (key, output) in rows.spent_outputs {
        digest.append_row(
            WalletProjectionRowFamily::TransparentSpentOutput,
            key.as_bytes(),
            &output.encode_value()?,
        )?;
    }
    for transaction in rows.address_transactions {
        digest.append_row(
            WalletProjectionRowFamily::TransparentAddressTransaction,
            transaction.key.as_bytes(),
            &transaction.encode_value(),
        )?;
    }
    for balance in rows.address_balances {
        digest.append_row(
            WalletProjectionRowFamily::TransparentAddressBalance,
            &balance.encode_key(),
            &balance.encode_value(),
        )?;
    }
    for undo in rows.reorg_undo {
        digest.append_row(
            WalletProjectionRowFamily::ReorgUndo,
            &undo.encode_key(),
            &undo.encode_value()?,
        )?;
    }
    let row_counts = digest.row_counts();
    Ok((row_counts, digest.finish()))
}

fn increment(counter: u64) -> Result<u64, WalletSortMergeError> {
    counter
        .checked_add(1)
        .ok_or(WalletSortMergeError::CounterOverflow)
}

fn count(len: usize) -> Result<u64, WalletSortMergeError> {
    u64::try_from(len).map_err(|_| WalletSortMergeError::CounterOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, CanonicalTransactionFacts, LockTime, PrivacyShape,
        SerializedBytesDigest, TransactionComponentCounts, TransactionId,
        TransactionIntrinsicValueBalances, TransactionPublicFacts, TransactionVersion,
        TransparentInputFact, TransparentOutPoint, TransparentOutputFact,
    };
    use zinder_wallet_projection::WalletProjectionSerialOracle;

    const TEST_MEMORY_LIMIT: u64 = 1_000_000;

    #[test]
    fn sort_merge_matches_serial_oracle_with_same_block_spend() {
        let address_one = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
        let address_two = TransparentAddressScriptHash::from_bytes([0xa2; 32]);
        let transaction_one = TransactionId::from_bytes([0xb1; 32]);
        let transaction_two = TransactionId::from_bytes([0xb2; 32]);
        let transaction_three = TransactionId::from_bytes([0xb3; 32]);
        let first_outpoint = TransparentOutPoint::new(transaction_one, 0);
        let same_block_outpoint = TransparentOutPoint::new(transaction_two, 0);
        let blocks = vec![
            block_facts(
                1,
                [0; 32],
                [0xc1; 32],
                vec![transaction_facts(
                    transaction_one,
                    Vec::new(),
                    vec![TransparentOutputFact::new(0, 7, [0x51], address_one)],
                )],
            ),
            block_facts(
                2,
                [0xc1; 32],
                [0xc2; 32],
                vec![
                    transaction_facts(
                        transaction_two,
                        vec![TransparentInputFact::new(0, first_outpoint)],
                        vec![TransparentOutputFact::new(0, 5, [0x52], address_two)],
                    ),
                    transaction_facts(
                        transaction_three,
                        vec![TransparentInputFact::new(0, same_block_outpoint)],
                        vec![TransparentOutputFact::new(0, 3, [0x53], address_one)],
                    ),
                ],
            ),
        ];
        let prepared =
            prepare_wallet_projection(Network::ZcashRegtest, 2, TEST_MEMORY_LIMIT, blocks.iter())
                .unwrap_or_else(|error| unreachable!("valid sort/merge fixture: {error}"));
        let mut oracle =
            WalletProjectionSerialOracle::with_supported_reorg_depth(Network::ZcashRegtest, 2);
        for block in &blocks {
            oracle
                .apply_block(block)
                .unwrap_or_else(|error| unreachable!("valid oracle fixture: {error}"));
        }

        assert_eq!(prepared.row_counts, oracle.row_counts());
        assert_eq!(prepared.utxo_summary, oracle.utxo_summary());
        assert_eq!(
            prepared.projection_digest,
            oracle
                .projection_digest()
                .unwrap_or_else(|error| unreachable!("valid oracle digest: {error}"))
        );
        assert_eq!(
            prepared.address_transactions,
            oracle.address_transactions().copied().collect::<Vec<_>>()
        );
        assert_eq!(
            prepared.reorg_undo,
            oracle.reorg_undo().cloned().collect::<Vec<_>>()
        );
        assert_eq!(prepared.counters.historical_prevout_read_count, 0);
        assert_eq!(prepared.counters.scanned_block_count, 2);
        assert_eq!(prepared.counters.staged_output_count, 3);
        assert_eq!(prepared.counters.staged_spend_count, 2);
        assert!(oracle.find_spent_output(first_outpoint).is_some());
        assert!(oracle.find_spent_output(same_block_outpoint).is_some());
    }

    #[test]
    fn sort_merge_retains_zero_value_utxo_without_balance_row() {
        let address = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
        let transaction = TransactionId::from_bytes([0xb1; 32]);
        let outpoint = TransparentOutPoint::new(transaction, 0);
        let blocks = [block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![transaction_facts(
                transaction,
                Vec::new(),
                vec![TransparentOutputFact::new(0, 0, [0x51], address)],
            )],
        )];
        let prepared =
            prepare_wallet_projection(Network::ZcashRegtest, 0, TEST_MEMORY_LIMIT, blocks.iter())
                .unwrap_or_else(|error| unreachable!("valid zero-value fixture: {error}"));
        let mut oracle = WalletProjectionSerialOracle::new(Network::ZcashRegtest);
        oracle
            .apply_block(&blocks[0])
            .unwrap_or_else(|error| unreachable!("valid zero-value oracle fixture: {error}"));

        assert_eq!(prepared.row_counts, oracle.row_counts());
        assert_eq!(prepared.utxo_summary, oracle.utxo_summary());
        assert_eq!(prepared.unspent_outputs.len(), 1);
        assert_eq!(
            prepared.unspent_outputs[0].0,
            WalletOutpointKey::new(outpoint)
        );
        assert_eq!(prepared.unspent_output_by_address.len(), 1);
        assert!(prepared.address_balances.is_empty());
        assert_eq!(prepared.row_counts.transparent_address_balance_count, 0);
        assert_eq!(
            prepared.projection_digest,
            oracle
                .projection_digest()
                .unwrap_or_else(|error| unreachable!("valid zero-value oracle digest: {error}"))
        );
    }

    #[test]
    fn sort_merge_mixes_zero_and_positive_utxos_into_one_positive_balance() {
        let address = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
        let transaction = TransactionId::from_bytes([0xb1; 32]);
        let blocks = [block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![transaction_facts(
                transaction,
                Vec::new(),
                vec![
                    TransparentOutputFact::new(0, 0, [0x51], address),
                    TransparentOutputFact::new(1, 7, [0x52], address),
                ],
            )],
        )];
        let prepared =
            prepare_wallet_projection(Network::ZcashRegtest, 0, TEST_MEMORY_LIMIT, blocks.iter())
                .unwrap_or_else(|error| unreachable!("valid mixed-value fixture: {error}"));
        let mut oracle = WalletProjectionSerialOracle::new(Network::ZcashRegtest);
        oracle
            .apply_block(&blocks[0])
            .unwrap_or_else(|error| unreachable!("valid mixed-value oracle fixture: {error}"));

        assert_eq!(prepared.row_counts, oracle.row_counts());
        assert_eq!(prepared.utxo_summary, oracle.utxo_summary());
        assert_eq!(prepared.unspent_outputs.len(), 2);
        assert_eq!(prepared.unspent_output_by_address.len(), 2);
        assert_eq!(
            prepared.address_balances,
            vec![WalletAddressBalance {
                address_script_hash: address,
                balance_zat: 7,
            }]
        );
        assert_eq!(prepared.row_counts.transparent_address_balance_count, 1);
        assert_eq!(
            prepared.projection_digest,
            oracle
                .projection_digest()
                .unwrap_or_else(|error| unreachable!("valid mixed-value oracle digest: {error}"))
        );
    }

    #[test]
    fn sort_merge_rejects_duplicate_output_and_spend_keys() {
        let address = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
        let transaction = TransactionId::from_bytes([0xb1; 32]);
        let outpoint = TransparentOutPoint::new(transaction, 0);
        let duplicate_outputs = vec![block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![
                transaction_facts(
                    transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(0, 1, [0x51], address)],
                ),
                transaction_facts(
                    transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(0, 1, [0x51], address)],
                ),
            ],
        )];
        assert!(matches!(
            prepare_wallet_projection(
                Network::ZcashRegtest,
                0,
                TEST_MEMORY_LIMIT,
                duplicate_outputs
            ),
            Err(WalletSortMergeError::Contract(
                WalletProjectionContractError::DuplicateOutput
            ))
        ));

        let duplicate_spends = vec![
            block_facts(
                1,
                [0; 32],
                [0xc1; 32],
                vec![transaction_facts(
                    transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(0, 1, [0x51], address)],
                )],
            ),
            block_facts(
                2,
                [0xc1; 32],
                [0xc2; 32],
                vec![transaction_facts(
                    TransactionId::from_bytes([0xb2; 32]),
                    vec![
                        TransparentInputFact::new(0, outpoint),
                        TransparentInputFact::new(1, outpoint),
                    ],
                    Vec::new(),
                )],
            ),
        ];
        assert!(matches!(
            prepare_wallet_projection(
                Network::ZcashRegtest,
                0,
                TEST_MEMORY_LIMIT,
                duplicate_spends
            ),
            Err(WalletSortMergeError::Contract(
                WalletProjectionContractError::DuplicateSpend
            ))
        ));
    }

    #[test]
    fn sort_merge_rejects_missing_or_future_predecessor() {
        let missing = TransparentOutPoint::new(TransactionId::from_bytes([0xee; 32]), 0);
        let missing_blocks = vec![block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![transaction_facts(
                TransactionId::from_bytes([0xb1; 32]),
                vec![TransparentInputFact::new(0, missing)],
                Vec::new(),
            )],
        )];
        assert_missing_predecessor(missing_blocks);

        let later_transaction = TransactionId::from_bytes([0xb3; 32]);
        let future = TransparentOutPoint::new(later_transaction, 0);
        let future_blocks = vec![block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![
                transaction_facts(
                    TransactionId::from_bytes([0xb2; 32]),
                    vec![TransparentInputFact::new(0, future)],
                    Vec::new(),
                ),
                transaction_facts(
                    later_transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(
                        0,
                        1,
                        [0x51],
                        TransparentAddressScriptHash::from_bytes([0xa1; 32]),
                    )],
                ),
            ],
        )];
        assert_missing_predecessor(future_blocks);
    }

    #[test]
    fn sort_merge_refuses_before_crossing_accounted_memory_limit() {
        let block = block_facts(
            1,
            [0; 32],
            [0xc1; 32],
            vec![transaction_facts(
                TransactionId::from_bytes([0xb1; 32]),
                Vec::new(),
                vec![TransparentOutputFact::new(
                    0,
                    1,
                    [0x51; 64],
                    TransparentAddressScriptHash::from_bytes([0xa1; 32]),
                )],
            )],
        );
        assert!(matches!(
            prepare_wallet_projection(Network::ZcashRegtest, 0, 1, [block]),
            Err(WalletSortMergeError::AccountedMemoryLimit {
                limit_bytes: 1,
                required_bytes: _
            })
        ));
    }

    fn assert_missing_predecessor(blocks: Vec<CanonicalBlockFacts>) {
        assert!(matches!(
            prepare_wallet_projection(Network::ZcashRegtest, 0, TEST_MEMORY_LIMIT, blocks),
            Err(WalletSortMergeError::Contract(
                WalletProjectionContractError::MissingTransparentPredecessor
            ))
        ));
    }

    fn block_facts(
        height: u32,
        parent_hash: [u8; 32],
        block_hash: [u8; 32],
        transactions: Vec<CanonicalTransactionFacts>,
    ) -> CanonicalBlockFacts {
        CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                BlockHeight::new(height),
                BlockHash::from_bytes(block_hash),
                BlockHash::from_bytes(parent_hash),
                [0; 32],
                [0; 32],
                0,
                0,
                [0; 32],
                0,
                0,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[0]),
            transactions,
        }
    }

    fn transaction_facts(
        transaction_id: TransactionId,
        transparent_inputs: Vec<TransparentInputFact>,
        transparent_outputs: Vec<TransparentOutputFact>,
    ) -> CanonicalTransactionFacts {
        CanonicalTransactionFacts {
            public_facts: TransactionPublicFacts {
                transaction_id,
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::Unsupported {
                    effective_version: 0,
                    version_group_id: None,
                },
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: 0,
                counts: TransactionComponentCounts::EMPTY,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                privacy_shape: PrivacyShape::Unclassified,
                is_coinbase: false,
                unsupported_sections: Vec::new(),
            },
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[0]),
            intrinsic_value_balances: TransactionIntrinsicValueBalances::default(),
            transparent_inputs,
            transparent_outputs,
        }
    }
}
