use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockTransactionIndexArtifact, LockTime,
    PrivacyShape, TransactionBlobArtifact, TransactionComponentCounts, TransactionFactsArtifact,
    TransactionId, TransactionLocation, TransactionPublicFacts, TransactionVersion,
    UnsupportedSection,
};

mod address_output_projection;
mod chain_epoch_reader;
mod chain_event;
mod commit_chain_epoch;
mod mempool_event;
mod primary_secondary;
mod reorg_window;
mod subtree_root;

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
