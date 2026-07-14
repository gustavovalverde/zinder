#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, TransactionId,
    TransactionIntrinsicValueBalances, TransactionIntrinsicValueBalancesArtifact,
    UnixTimestampMillis,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEventHistoryRequest,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange, StoreError,
};

use super::{synthetic_block_header, synthetic_transaction_rows};

#[test]
fn signed_intrinsic_balances_round_trip_and_remain_optional() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch, block, compact) = epoch_artifacts(1, 1, 1, 0, 1, 1, 14);
    let transaction_id = TransactionId::from_bytes([7; 32]);
    let (index, location, facts, blob) = synthetic_transaction_rows(
        transaction_id,
        block.height,
        block.block_hash,
        0,
        b"intrinsic-balance-transaction",
    );
    let balances = TransactionIntrinsicValueBalancesArtifact::new(
        location,
        TransactionIntrinsicValueBalances::new(-11, 22, -33, 44),
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch, vec![block], vec![compact])
            .with_block_transaction_index(vec![index])
            .with_transaction_locations(vec![location])
            .with_transaction_facts(vec![facts])
            .with_transaction_intrinsic_value_balances(vec![balances])
            .with_transaction_blobs(vec![blob]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.transaction_intrinsic_value_balances_by_id(transaction_id)?,
        Some(balances)
    );
    assert_eq!(
        reader.transaction_intrinsic_value_balances_by_id(TransactionId::from_bytes([8; 32]))?,
        None
    );

    let unknown_transaction_id = TransactionId::from_bytes([8; 32]);
    let batch = reader.transaction_intrinsic_value_balances_by_ids(&[
        transaction_id,
        unknown_transaction_id,
        transaction_id,
    ])?;
    assert_eq!(batch.len(), 2);
    assert_eq!(batch.get(&transaction_id), Some(&Some(balances)));
    assert_eq!(batch.get(&unknown_transaction_id), Some(&None));

    Ok(())
}

#[test]
fn reorged_intrinsic_balances_are_hidden_from_the_replacement_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 14);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1],
        vec![compact_1],
    ))?;

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 14);
    let transaction_id = TransactionId::from_bytes([9; 32]);
    let (index, location, facts, _) = synthetic_transaction_rows(
        transaction_id,
        block_2.height,
        block_2.block_hash,
        0,
        b"reorged-intrinsic-balance-transaction",
    );
    let balances = TransactionIntrinsicValueBalancesArtifact::new(
        location,
        TransactionIntrinsicValueBalances::new(1, 2, 3, 4),
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_2])
            .with_block_transaction_index(vec![index])
            .with_transaction_locations(vec![location])
            .with_transaction_facts(vec![facts])
            .with_transaction_intrinsic_value_balances(vec![balances]),
    )?;

    let (epoch_3, replacement_block, replacement_compact) = epoch_artifacts(3, 2, 92, 1, 1, 1, 14);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_3, vec![replacement_block], vec![replacement_compact])
            .with_reorg_window_change(ReorgWindowChange::Replace {
                from_height: BlockHeight::new(2),
            }),
    )?;

    assert_eq!(
        store
            .chain_epoch_reader_at(ChainEpochId::new(1))?
            .transaction_intrinsic_value_balances_by_id(transaction_id)?,
        None
    );
    assert_eq!(
        store
            .chain_epoch_reader_at(ChainEpochId::new(2))?
            .transaction_intrinsic_value_balances_by_id(transaction_id)?,
        Some(balances)
    );
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .transaction_intrinsic_value_balances_by_id(transaction_id)?,
        None
    );
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .transaction_intrinsic_value_balances_by_ids(&[transaction_id])?
            .get(&transaction_id),
        Some(&None)
    );

    Ok(())
}

#[test]
fn historical_enrichment_is_settled_idempotent_and_emits_no_chain_event() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 13);
    let transaction_id = TransactionId::from_bytes([10; 32]);
    let (index, location, facts, _) = synthetic_transaction_rows(
        transaction_id,
        block_1.height,
        block_1.block_hash,
        0,
        b"historically-enriched-transaction",
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_1])
            .with_block_transaction_index(vec![index])
            .with_transaction_locations(vec![location])
            .with_transaction_facts(vec![facts]),
    )?;
    let balances = TransactionIntrinsicValueBalancesArtifact::new(
        location,
        TransactionIntrinsicValueBalances::new(-5, -6, 7, 8),
    );
    let events_before =
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;

    let first = store.enrich_transaction_intrinsic_value_balances(&[balances])?;
    let second = store.enrich_transaction_intrinsic_value_balances(&[balances])?;
    assert_eq!(first, second);
    assert_eq!(first.chain_epoch, epoch_1);
    assert_eq!(store.current_chain_epoch()?, Some(epoch_1));
    assert_eq!(
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?,
        events_before
    );

    let stale = TransactionIntrinsicValueBalancesArtifact::new(
        zinder_core::TransactionLocation::new(
            transaction_id,
            location.block_height,
            BlockHash::from_bytes([99; 32]),
            location.tx_index_in_block,
        ),
        balances.value_balances,
    );
    assert!(matches!(
        store.enrich_transaction_intrinsic_value_balances(&[stale]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));
    assert!(matches!(
        store.enrich_transaction_intrinsic_value_balances(&[balances, balances]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 2, 2, 14);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_2,
        vec![block_2],
        vec![compact_2],
    ))?;
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .transaction_intrinsic_value_balances_by_id(transaction_id)?,
        Some(balances)
    );

    Ok(())
}

#[test]
fn enrichment_rejects_transactions_above_the_settled_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 14);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1],
        vec![compact_1],
    ))?;
    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 14);
    let transaction_id = TransactionId::from_bytes([11; 32]);
    let (index, location, facts, _) = synthetic_transaction_rows(
        transaction_id,
        block_2.height,
        block_2.block_hash,
        0,
        b"unsettled-transaction",
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_2])
            .with_block_transaction_index(vec![index])
            .with_transaction_locations(vec![location])
            .with_transaction_facts(vec![facts]),
    )?;
    let balances = TransactionIntrinsicValueBalancesArtifact::new(
        location,
        TransactionIntrinsicValueBalances::default(),
    );

    assert!(matches!(
        store.enrich_transaction_intrinsic_value_balances(&[balances]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));

    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "test fixture spells out both chain boundaries"
)]
fn epoch_artifacts(
    epoch_id: u64,
    tip_height: u32,
    hash_seed: u8,
    parent_hash_seed: u8,
    settled_height: u32,
    settled_hash_seed: u8,
    schema_version: u16,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let height = BlockHeight::new(tip_height);
    let block_hash = BlockHash::from_bytes([hash_seed; 32]);
    let parent_hash = BlockHash::from_bytes([parent_hash_seed; 32]);
    (
        ChainEpoch {
            id: ChainEpochId::new(epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: height,
            visible_tip_hash: block_hash,
            settled_tip_height: BlockHeight::new(settled_height),
            settled_tip_hash: BlockHash::from_bytes([settled_hash_seed; 32]),
            artifact_schema_version: ArtifactSchemaVersion::new(schema_version),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_600_000 + epoch_id),
        },
        synthetic_block_header(height, block_hash, parent_hash, b"intrinsic-balance-block"),
        CompactBlockArtifact::new(height, block_hash, b"intrinsic-balance-compact".to_vec()),
    )
}

#[test]
fn current_schema_is_eighteen() {
    assert_eq!(CURRENT_ARTIFACT_SCHEMA_VERSION.value(), 18);
}
