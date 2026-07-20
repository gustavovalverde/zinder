use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, CanonicalHistoryBounds, ChainEpoch,
    ChainEpochId, ChainTipMetadata, CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainStoreOptions, PrimaryChainStore, StoreError,
};

use super::synthetic_block_header;

#[test]
fn first_full_commit_publishes_complete_history_bounds() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let block = block(1, 1, 0);
    let compact_block =
        CompactBlockArtifact::new(BlockHeight::new(1), BlockHash::from_bytes([1; 32]), [1]);

    assert_eq!(store.canonical_history_bounds()?, None);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        epoch(1, 1, 1),
        vec![block],
        vec![compact_block],
    ))?;

    assert_eq!(
        store.canonical_history_bounds()?,
        Some(CanonicalHistoryBounds::complete())
    );

    Ok(())
}

#[test]
fn artifactless_checkpoint_commit_publishes_checkpointed_history_bounds() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let checkpoint_epoch = epoch(1, 20, 7);

    store.commit_artifactless_checkpoint(checkpoint_epoch)?;

    assert_eq!(store.current_chain_epoch()?, Some(checkpoint_epoch));
    assert_eq!(
        store.canonical_history_bounds()?,
        Some(CanonicalHistoryBounds::checkpointed(BlockId::new(
            BlockHeight::new(20),
            BlockHash::from_bytes([7; 32]),
        ))?)
    );
    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.canonical_history_bounds(),
        CanonicalHistoryBounds::checkpointed(BlockId::new(
            BlockHeight::new(20),
            BlockHash::from_bytes([7; 32]),
        ))?
    );
    assert!(matches!(
        reader.block_header_at(BlockHeight::new(20)),
        Err(StoreError::CanonicalHistoryUnavailable {
            requested_height,
            first_available_height,
            ..
        }) if requested_height == BlockHeight::new(20)
            && first_available_height == BlockHeight::new(21)
    ));

    Ok(())
}

#[test]
fn reconciliation_leaves_empty_store_unbounded() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    assert_eq!(
        store.reconcile_canonical_history_bounds(Some(BlockId::new(
            BlockHeight::new(20),
            BlockHash::from_bytes([7; 32]),
        )))?,
        None
    );
    assert_eq!(store.canonical_history_bounds()?, None);

    Ok(())
}

#[test]
fn reconciliation_keeps_durable_bounds_authoritative() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    store.commit_artifactless_checkpoint(epoch(1, 20, 7))?;
    let durable = CanonicalHistoryBounds::checkpointed(BlockId::new(
        BlockHeight::new(20),
        BlockHash::from_bytes([7; 32]),
    ))?;

    assert_eq!(
        store.reconcile_canonical_history_bounds(Some(BlockId::new(
            BlockHeight::new(30),
            BlockHash::from_bytes([9; 32]),
        )))?,
        Some(durable)
    );
    assert_eq!(store.canonical_history_bounds()?, Some(durable));

    Ok(())
}

#[test]
fn invalid_artifactless_checkpoint_publishes_neither_epoch_nor_bounds() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut checkpoint_epoch = epoch(1, 20, 7);
    checkpoint_epoch.settled_tip_hash = BlockHash::from_bytes([8; 32]);

    assert!(matches!(
        store.commit_artifactless_checkpoint(checkpoint_epoch),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));
    assert_eq!(store.current_chain_epoch()?, None);
    assert_eq!(store.canonical_history_bounds()?, None);

    Ok(())
}

#[test]
fn ordinary_commit_cannot_publish_an_artifactless_checkpoint_as_complete_history()
-> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let checkpoint_epoch = epoch(1, 20, 7);

    assert!(matches!(
        store.commit_chain_epoch(
            super::synthetic_chain_epoch_artifacts(checkpoint_epoch, Vec::new(), Vec::new())
                .with_reorg_window_change(zinder_store::ReorgWindowChange::AdvanceSettledTipTo {
                    height: BlockHeight::new(20),
                })
        ),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));
    assert_eq!(store.current_chain_epoch()?, None);
    assert_eq!(store.canonical_history_bounds()?, None);

    Ok(())
}

fn epoch(id: u64, tip_height: u32, tip_hash_byte: u8) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(tip_height),
        visible_tip_hash: BlockHash::from_bytes([tip_hash_byte; 32]),
        settled_tip_height: BlockHeight::new(tip_height),
        settled_tip_hash: BlockHash::from_bytes([tip_hash_byte; 32]),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(id),
    }
}

fn block(height: u32, hash_byte: u8, parent_hash_byte: u8) -> BlockHeaderArtifact {
    synthetic_block_header(
        BlockHeight::new(height),
        BlockHash::from_bytes([hash_byte; 32]),
        BlockHash::from_bytes([parent_hash_byte; 32]),
        &[],
    )
}
