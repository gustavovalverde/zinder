use std::collections::{HashMap, HashSet};

use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, BlockTransactionIndexArtifact,
    CanonicalBlockFacts, CanonicalBlockFactsDigestVersion, CanonicalBlockReplayEnvelope,
    CanonicalBlockReplayFormatVersion, CanonicalTransactionFacts, ChainEpoch, CompactBlockArtifact,
    LockTime, PrivacyShape, SerializedBytesDigest, TransactionBlobArtifact,
    TransactionComponentCounts, TransactionFactsArtifact, TransactionId,
    TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
    TransactionVersion, TransparentInputFact, TransparentOutputFact, UnsupportedSection,
    encode_canonical_block_replay,
};
use zinder_store::{ChainEpochArtifacts, ChainEpochCommitOutcome, PrimaryChainStore, StoreError};

mod address_output_projection;
mod block_replay;
mod block_value_pool_balances;
mod canonical_history;
mod chain_epoch_reader;
mod chain_event;
mod commit_chain_epoch;
mod displaced_block;
mod final_note_commitment_roots;
mod mempool_event;
mod primary_secondary;
mod reorg_window;
mod subtree_root;
mod transaction_intrinsic_value_balances;

pub(crate) fn synthetic_chain_epoch_artifacts(
    chain_epoch: ChainEpoch,
    block_headers: Vec<BlockHeaderArtifact>,
    compact_blocks: Vec<CompactBlockArtifact>,
) -> ChainEpochArtifacts {
    let block_replay_envelopes = block_headers
        .iter()
        .map(|block_header| {
            encode_canonical_block_replay(
                &CanonicalBlockFacts {
                    block_header: block_header.clone(),
                    serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[]),
                    transactions: Vec::new(),
                },
                CanonicalBlockReplayFormatVersion::CURRENT,
                CanonicalBlockFactsDigestVersion::CURRENT,
            )
        })
        .collect();
    ChainEpochArtifacts::new(
        chain_epoch,
        block_headers,
        block_replay_envelopes,
        compact_blocks,
    )
}

pub(crate) fn with_synthetic_block_replay_envelopes(
    mut artifacts: ChainEpochArtifacts,
) -> ChainEpochArtifacts {
    add_missing_transparent_fact_transactions(&mut artifacts);
    synchronize_transparent_facts(&mut artifacts);
    artifacts.block_replay_envelopes = synthetic_block_replay_envelopes(&artifacts);
    artifacts
}

fn synchronize_transparent_facts(artifacts: &mut ChainEpochArtifacts) {
    for transaction in &mut artifacts.transaction_facts {
        let transaction_id = transaction.location.transaction_id;
        let mut transparent_outputs = artifacts
            .transparent_outputs_by_outpoint
            .iter()
            .filter(|output| output.outpoint.transaction_id == transaction_id)
            .map(|output| {
                TransparentOutputFact::new(
                    output.outpoint.output_index,
                    output.value_zat,
                    output.script_pub_key.clone(),
                    output.address_script_hash,
                )
            })
            .collect::<Vec<_>>();
        if !transparent_outputs.is_empty() {
            transparent_outputs.sort_by_key(|output| output.output_index);
            transaction.transparent_outputs = transparent_outputs;
        }

        let mut transparent_inputs = artifacts
            .transparent_spend_facts
            .iter()
            .filter(|spend| {
                spend.spending_transaction_id == transaction_id
                    && spend.block_height == transaction.location.block_height
                    && spend.block_hash == transaction.location.block_hash
                    && spend.tx_index_in_block == transaction.location.tx_index_in_block
            })
            .map(|spend| TransparentInputFact::new(spend.input_index, spend.spent_outpoint))
            .collect::<Vec<_>>();
        if !transparent_inputs.is_empty() {
            transparent_inputs.sort_by_key(|input| input.input_index);
            transaction.transparent_inputs = transparent_inputs;
        }
    }
}

fn synthetic_block_replay_envelopes(
    artifacts: &ChainEpochArtifacts,
) -> Vec<CanonicalBlockReplayEnvelope> {
    artifacts
        .block_headers
        .iter()
        .map(|block_header| {
            let mut transaction_facts = artifacts
                .transaction_facts
                .iter()
                .filter(|transaction| {
                    transaction.location.block_height == block_header.height
                        && transaction.location.block_hash == block_header.block_hash
                })
                .collect::<Vec<_>>();
            transaction_facts.sort_by_key(|transaction| transaction.location.tx_index_in_block);
            let transactions = transaction_facts
                .into_iter()
                .map(|transaction| CanonicalTransactionFacts {
                    public_facts: transaction.public_facts.clone(),
                    serialized_bytes_digest: artifacts
                        .transaction_blobs
                        .iter()
                        .find(|blob| blob.location == transaction.location)
                        .map_or_else(
                            || SerializedBytesDigest::from_serialized_bytes(&[]),
                            |blob| {
                                SerializedBytesDigest::from_serialized_bytes(
                                    &blob.raw_transaction_bytes,
                                )
                            },
                        ),
                    intrinsic_value_balances: artifacts
                        .transaction_intrinsic_value_balances
                        .iter()
                        .find(|balances| {
                            balances.location.transaction_id == transaction.location.transaction_id
                        })
                        .map_or_else(TransactionIntrinsicValueBalances::default, |balances| {
                            balances.value_balances
                        }),
                    transparent_inputs: transaction.transparent_inputs.clone(),
                    transparent_outputs: transaction.transparent_outputs.clone(),
                })
                .collect();
            encode_canonical_block_replay(
                &CanonicalBlockFacts {
                    block_header: block_header.clone(),
                    serialized_bytes_digest: artifacts
                        .block_blobs
                        .iter()
                        .find(|blob| {
                            blob.height == block_header.height
                                && blob.block_hash == block_header.block_hash
                        })
                        .map_or_else(
                            || SerializedBytesDigest::from_serialized_bytes(&[]),
                            |blob| {
                                SerializedBytesDigest::from_serialized_bytes(&blob.raw_block_bytes)
                            },
                        ),
                    transactions,
                },
                CanonicalBlockReplayFormatVersion::CURRENT,
                CanonicalBlockFactsDigestVersion::CURRENT,
            )
        })
        .collect()
}

pub(crate) fn commit_synthetic_chain_epoch(
    store: &PrimaryChainStore,
    artifacts: ChainEpochArtifacts,
) -> Result<ChainEpochCommitOutcome, StoreError> {
    store.commit_chain_epoch(with_synthetic_block_replay_envelopes(artifacts))
}

fn add_missing_transparent_fact_transactions(artifacts: &mut ChainEpochArtifacts) {
    let mut known_transaction_ids = artifacts
        .transaction_facts
        .iter()
        .map(|transaction| transaction.location.transaction_id)
        .collect::<HashSet<_>>();
    let mut used_indexes_by_block = artifacts.block_transaction_index.iter().fold(
        HashMap::<BlockId, HashSet<u32>>::new(),
        |mut indexes, row| {
            indexes
                .entry(BlockId::new(row.block_height, row.block_hash))
                .or_default()
                .insert(row.tx_index_in_block);
            indexes
        },
    );

    add_missing_spending_transactions(
        artifacts,
        &mut known_transaction_ids,
        &mut used_indexes_by_block,
    );
    add_missing_output_transactions(
        artifacts,
        &mut known_transaction_ids,
        &mut used_indexes_by_block,
    );
}

fn add_missing_spending_transactions(
    artifacts: &mut ChainEpochArtifacts,
    known_transaction_ids: &mut HashSet<TransactionId>,
    used_indexes_by_block: &mut HashMap<BlockId, HashSet<u32>>,
) {
    let spending_transactions = artifacts
        .transparent_spend_facts
        .iter()
        .map(|spend| {
            (
                spend.spending_transaction_id,
                spend.block_height,
                spend.block_hash,
            )
        })
        .collect::<Vec<_>>();
    for (transaction_id, block_height, block_hash) in spending_transactions {
        let tx_index_in_block = artifacts
            .transaction_facts
            .iter()
            .find(|transaction| transaction.location.transaction_id == transaction_id)
            .map_or_else(
                || {
                    reserve_next_transaction_index(
                        used_indexes_by_block,
                        BlockId::new(block_height, block_hash),
                    )
                },
                |transaction| transaction.location.tx_index_in_block,
            );
        for spend in &mut artifacts.transparent_spend_facts {
            if spend.spending_transaction_id == transaction_id
                && spend.block_height == block_height
                && spend.block_hash == block_hash
            {
                spend.tx_index_in_block = tx_index_in_block;
            }
        }
        add_synthetic_transaction_if_missing(
            artifacts,
            known_transaction_ids,
            used_indexes_by_block,
            transaction_id,
            block_height,
            block_hash,
            tx_index_in_block,
        );
    }
}

fn add_missing_output_transactions(
    artifacts: &mut ChainEpochArtifacts,
    known_transaction_ids: &mut HashSet<TransactionId>,
    used_indexes_by_block: &mut HashMap<BlockId, HashSet<u32>>,
) {
    let output_transactions = artifacts
        .transparent_outputs_by_outpoint
        .iter()
        .map(|output| {
            (
                output.outpoint.transaction_id,
                output.block_height,
                output.block_hash,
            )
        })
        .collect::<Vec<_>>();
    for (transaction_id, block_height, block_hash) in output_transactions {
        if known_transaction_ids.contains(&transaction_id) {
            continue;
        }
        let block_id = BlockId::new(block_height, block_hash);
        let tx_index_in_block = reserve_next_transaction_index(used_indexes_by_block, block_id);
        add_synthetic_transaction_if_missing(
            artifacts,
            known_transaction_ids,
            used_indexes_by_block,
            transaction_id,
            block_height,
            block_hash,
            tx_index_in_block,
        );
    }
}

fn reserve_next_transaction_index(
    used_indexes_by_block: &mut HashMap<BlockId, HashSet<u32>>,
    block_id: BlockId,
) -> u32 {
    let used_indexes = used_indexes_by_block.entry(block_id).or_default();
    let tx_index_in_block = (0..=u32::MAX)
        .find(|candidate| !used_indexes.contains(candidate))
        .unwrap_or(u32::MAX);
    used_indexes.insert(tx_index_in_block);
    tx_index_in_block
}

#[allow(
    clippy::too_many_arguments,
    reason = "the test migration preserves the complete mined transaction identity"
)]
fn add_synthetic_transaction_if_missing(
    artifacts: &mut ChainEpochArtifacts,
    known_transaction_ids: &mut HashSet<TransactionId>,
    used_indexes_by_block: &mut HashMap<BlockId, HashSet<u32>>,
    transaction_id: TransactionId,
    block_height: BlockHeight,
    block_hash: BlockHash,
    tx_index_in_block: u32,
) {
    if !known_transaction_ids.insert(transaction_id) {
        return;
    }

    let (index, location, facts, _) = synthetic_transaction_rows(
        transaction_id,
        block_height,
        block_hash,
        tx_index_in_block,
        b"synthetic-transparent-facts",
    );
    used_indexes_by_block
        .entry(BlockId::new(block_height, block_hash))
        .or_default()
        .insert(tx_index_in_block);
    artifacts.block_transaction_index.push(index);
    artifacts.transaction_locations.push(location);
    artifacts.transaction_facts.push(facts);
    artifacts.transaction_intrinsic_value_balances.push(
        zinder_core::TransactionIntrinsicValueBalancesArtifact::new(
            location,
            TransactionIntrinsicValueBalances::default(),
        ),
    );
}

pub(crate) fn synthetic_block_header(
    height: BlockHeight,
    block_hash: BlockHash,
    parent_hash: BlockHash,
    raw_block_bytes: &[u8],
) -> BlockHeaderArtifact {
    BlockHeaderArtifact::new(
        height,
        block_hash,
        parent_hash,
        [0; 32],
        [0; 32],
        0,
        0,
        [0; 32],
        0,
        u64::try_from(raw_block_bytes.len()).unwrap_or(u64::MAX),
    )
}

pub(crate) fn synthetic_transaction_rows(
    transaction_id: TransactionId,
    block_height: BlockHeight,
    block_hash: BlockHash,
    tx_index_in_block: u32,
    raw_transaction_bytes: &[u8],
) -> (
    BlockTransactionIndexArtifact,
    TransactionLocation,
    TransactionFactsArtifact,
    TransactionBlobArtifact,
) {
    let location =
        TransactionLocation::new(transaction_id, block_height, block_hash, tx_index_in_block);
    (
        BlockTransactionIndexArtifact::new(
            block_height,
            tx_index_in_block,
            transaction_id,
            block_hash,
        ),
        location,
        TransactionFactsArtifact::new(
            location,
            synthetic_transaction_public_facts(transaction_id, raw_transaction_bytes.len()),
        ),
        TransactionBlobArtifact::new(location, raw_transaction_bytes.to_vec()),
    )
}

pub(crate) fn synthetic_transaction_public_facts(
    transaction_id: TransactionId,
    raw_transaction_size_bytes: usize,
) -> TransactionPublicFacts {
    TransactionPublicFacts {
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
        size_bytes: u32::try_from(raw_transaction_size_bytes).unwrap_or(u32::MAX),
        counts: TransactionComponentCounts::EMPTY,
        orchard_value_balance_zat: None,
        orchard_anchor: None,
        ironwood_value_balance_zat: None,
        privacy_shape: PrivacyShape::Unclassified,
        is_coinbase: false,
        unsupported_sections: vec![UnsupportedSection::FutureVersionHeader],
    }
}
