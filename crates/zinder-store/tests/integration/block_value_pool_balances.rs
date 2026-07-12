#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId,
    BlockValuePoolBalances, ChainEpoch, ChainEpochId, ChainTipMetadata, CompactBlockArtifact,
    Network, UnixTimestampMillis, ValuePoolBalance,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEventHistoryRequest,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange, StoreError,
};

use super::synthetic_block_header;

#[test]
fn balances_round_trip_optional_future_pools_and_bounded_ranges() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 15);
    let balances_1 = balances(block_1.height, block_1.block_hash, block_1.block_time, 11);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_1])
            .with_block_value_pool_balances(vec![balances_1.clone()]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.block_value_pool_balances_at(BlockHeight::new(1))?,
        Some(balances_1.clone())
    );
    assert_eq!(
        reader.block_value_pool_balances_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))?,
        vec![Some(balances_1), None]
    );
    Ok(())
}

#[test]
fn historical_enrichment_is_idempotent_hash_and_time_bound_and_event_neutral() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 14);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1.clone()],
        vec![compact_1],
    ))?;
    let snapshot = balances(block_1.height, block_1.block_hash, block_1.block_time, 21);
    let events_before =
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;

    let first = store.enrich_block_value_pool_balances(std::slice::from_ref(&snapshot))?;
    let second = store.enrich_block_value_pool_balances(std::slice::from_ref(&snapshot))?;
    assert_eq!(first, second);
    assert_eq!(first.chain_epoch, epoch_1);
    assert_eq!(store.current_chain_epoch()?, Some(epoch_1));
    assert_eq!(
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?,
        events_before
    );

    let stale_hash = balances(
        block_1.height,
        BlockHash::from_bytes([99; 32]),
        block_1.block_time,
        31,
    );
    let stale_time = balances(
        block_1.height,
        block_1.block_hash,
        block_1.block_time + 1,
        31,
    );
    for invalid in [stale_hash, stale_time] {
        assert!(matches!(
            store.enrich_block_value_pool_balances(&[invalid]),
            Err(StoreError::InvalidChainEpochArtifacts { .. })
        ));
    }
    assert!(matches!(
        store.enrich_block_value_pool_balances(&[snapshot.clone(), snapshot.clone()]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));

    let conflicting = balances(block_1.height, block_1.block_hash, block_1.block_time, 41);
    assert!(matches!(
        store.enrich_block_value_pool_balances(&[conflicting]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));

    let (epoch_2, block_2, compact_2) =
        epoch_artifacts(2, 2, 2, 1, 1, 1, 2, CURRENT_ARTIFACT_SCHEMA_VERSION.value());
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_2,
        vec![block_2],
        vec![compact_2],
    ))?;
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .block_value_pool_balances_at(BlockHeight::new(1))?,
        Some(snapshot)
    );
    assert_eq!(
        epoch_2.artifact_schema_version,
        CURRENT_ARTIFACT_SCHEMA_VERSION
    );
    Ok(())
}

#[test]
fn reorged_balances_remain_epoch_readable_but_leave_the_canonical_view() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 15);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1],
        vec![compact_1],
    ))?;

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 2, 15);
    let old_balances = balances(block_2.height, block_2.block_hash, block_2.block_time, 51);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_2])
            .with_block_value_pool_balances(vec![old_balances.clone()]),
    )?;

    let (epoch_3, replacement, replacement_compact) = epoch_artifacts(3, 2, 92, 1, 1, 1, 3, 15);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_3, vec![replacement], vec![replacement_compact])
            .with_reorg_window_change(ReorgWindowChange::Replace {
                from_height: BlockHeight::new(2),
            }),
    )?;

    assert_eq!(
        store
            .chain_epoch_reader_at(ChainEpochId::new(2))?
            .block_value_pool_balances_at(BlockHeight::new(2))?,
        Some(old_balances)
    );
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .block_value_pool_balances_at(BlockHeight::new(2))?,
        None
    );
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
    created_at_offset: u64,
    schema_version: u16,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let height = BlockHeight::new(tip_height);
    let block_hash = BlockHash::from_bytes([hash_seed; 32]);
    let parent_hash = BlockHash::from_bytes([parent_hash_seed; 32]);
    let block = synthetic_block_header(height, block_hash, parent_hash, b"value-pool-block");
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
            created_at: UnixTimestampMillis::new(1_774_668_500_000 + created_at_offset),
        },
        block,
        CompactBlockArtifact::new(height, block_hash, b"value-pool-compact".to_vec()),
    )
}

fn balances(
    height: BlockHeight,
    block_hash: BlockHash,
    block_time_seconds: i64,
    value_seed: u64,
) -> BlockValuePoolBalances {
    BlockValuePoolBalances::new(
        BlockId::new(height, block_hash),
        block_time_seconds,
        vec![
            ValuePoolBalance::new("transparent", true, Some(value_seed)),
            ValuePoolBalance::new("future-pool", false, None),
            ValuePoolBalance::new("sapling", true, Some(value_seed + 1)),
        ],
    )
}
