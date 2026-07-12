#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockFinalNoteCommitmentRoots, BlockHash, BlockHeaderArtifact,
    BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, FinalNoteCommitmentRoot, Network, UnixTimestampMillis,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEventHistoryRequest,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange, StoreError,
};

use super::synthetic_block_header;

#[test]
fn final_roots_round_trip_optional_pools_and_bounded_ranges() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 13);
    let roots_1 = roots(block_1.height, block_1.block_hash, 11, 12, 13);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_1])
            .with_final_note_commitment_roots(vec![roots_1]),
    )?;

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 2, 13);
    let unavailable =
        BlockFinalNoteCommitmentRoots::unavailable(block_2.height, block_2.block_hash);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_2])
            .with_final_note_commitment_roots(vec![unavailable]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.final_note_commitment_roots_at(BlockHeight::new(1))?,
        Some(roots_1)
    );
    assert_eq!(
        reader.final_note_commitment_roots_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(3),
        ))?,
        vec![Some(roots_1), Some(unavailable), None]
    );

    Ok(())
}

#[test]
fn reorged_final_roots_remain_physical_but_are_not_canonical() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 13);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1],
        vec![compact_1],
    ))?;

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 2, 13);
    let old_roots = roots(block_2.height, block_2.block_hash, 21, 22, 23);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_2])
            .with_final_note_commitment_roots(vec![old_roots]),
    )?;

    let (epoch_3, replacement_block, replacement_compact) =
        epoch_artifacts(3, 2, 92, 1, 1, 1, 2, 13);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_3, vec![replacement_block], vec![replacement_compact])
            .with_reorg_window_change(ReorgWindowChange::Replace {
                from_height: BlockHeight::new(2),
            }),
    )?;

    assert_eq!(
        store
            .chain_epoch_reader_at(ChainEpochId::new(2))?
            .final_note_commitment_roots_at(BlockHeight::new(2))?,
        Some(old_roots)
    );
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .final_note_commitment_roots_at(BlockHeight::new(2))?,
        None
    );

    Ok(())
}

#[test]
fn historical_enrichment_is_idempotent_rejects_stale_hashes_and_survives_commit() -> eyre::Result<()>
{
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_1) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 12);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1.clone()],
        vec![compact_1],
    ))?;
    let roots_1 = roots(block_1.height, block_1.block_hash, 41, 42, 43);
    let events_before =
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;

    let first = store.enrich_final_note_commitment_roots(&[roots_1])?;
    let second = store.enrich_final_note_commitment_roots(&[roots_1])?;
    assert_eq!(first.chain_epoch, epoch_1);
    assert_eq!(first, second);
    assert_eq!(store.current_chain_epoch()?, Some(epoch_1));
    assert_eq!(
        store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?,
        events_before
    );

    let stale = roots(block_1.height, BlockHash::from_bytes([99; 32]), 51, 52, 53);
    assert!(matches!(
        store.enrich_final_note_commitment_roots(&[stale]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));
    assert!(matches!(
        store.enrich_final_note_commitment_roots(&[roots_1, roots_1]),
        Err(StoreError::InvalidChainEpochArtifacts { .. })
    ));

    let (epoch_2, block_2, compact_2) = epoch_artifacts(2, 2, 2, 1, 1, 1, 2, 13);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_2,
        vec![block_2],
        vec![compact_2],
    ))?;
    assert_eq!(
        store
            .current_chain_epoch_reader()?
            .final_note_commitment_roots_at(BlockHeight::new(1))?,
        Some(roots_1)
    );

    Ok(())
}

#[test]
fn artifact_schemas_12_through_17_are_readable_11_and_18_are_rejected_and_17_commits()
-> eyre::Result<()> {
    assert_schema_reopen(12, true)?;
    assert_schema_reopen(13, true)?;
    assert_schema_reopen(14, true)?;
    assert_schema_reopen(15, true)?;
    assert_schema_reopen(16, true)?;
    assert_schema_reopen(17, true)?;
    assert_schema_reopen(11, false)?;
    assert_schema_reopen(18, false)?;

    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch, block, compact) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, 17);
    store.commit_chain_epoch(ChainEpochArtifacts::new(epoch, vec![block], vec![compact]))?;
    assert_eq!(
        store
            .current_chain_epoch()?
            .map(|value| value.artifact_schema_version),
        Some(CURRENT_ARTIFACT_SCHEMA_VERSION)
    );

    Ok(())
}

fn assert_schema_reopen(version: u16, should_open: bool) -> eyre::Result<()> {
    let tempdir = tempdir()?;
    {
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (epoch, block, compact) = epoch_artifacts(1, 1, 1, 0, 1, 1, 1, version);
        store.commit_chain_epoch(ChainEpochArtifacts::new(epoch, vec![block], vec![compact]))?;
    }

    let reopened = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests());
    assert_eq!(
        reopened.is_ok(),
        should_open,
        "unexpected schema {version} reopen result"
    );
    if let Err(error) = reopened {
        match version {
            11 => assert!(matches!(error, StoreError::SchemaTooOld { .. })),
            18 => assert!(matches!(error, StoreError::SchemaTooNew { .. })),
            _ => return Err(eyre::eyre!("unexpected schema reopen error: {error}")),
        }
    }

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
        synthetic_block_header(height, block_hash, parent_hash, b"final-roots-block"),
        CompactBlockArtifact::new(height, block_hash, b"final-roots-compact".to_vec()),
    )
}

fn roots(
    height: BlockHeight,
    block_hash: BlockHash,
    sapling_seed: u8,
    orchard_seed: u8,
    ironwood_seed: u8,
) -> BlockFinalNoteCommitmentRoots {
    BlockFinalNoteCommitmentRoots::new(
        height,
        block_hash,
        Some(FinalNoteCommitmentRoot::from_bytes([sapling_seed; 32])),
        Some(FinalNoteCommitmentRoot::from_bytes([orchard_seed; 32])),
        Some(FinalNoteCommitmentRoot::from_bytes([ironwood_seed; 32])),
    )
}
