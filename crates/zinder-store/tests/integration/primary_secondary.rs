#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_store::{
    ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore, SecondaryChainStore, StoreError,
};

#[test]
fn second_primary_open_returns_primary_already_open() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let _primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;

    let Err(error) = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())
    else {
        return Err(eyre!("expected second primary open to fail"));
    };

    assert!(
        matches!(
            error,
            StoreError::PrimaryAlreadyOpen { ref lock_path }
                if lock_path == &primary_path.join("LOCK")
        ),
        "unexpected error: {error:?}"
    );

    Ok(())
}

#[test]
fn secondary_catches_up_after_primary_commits() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let secondary_path = tempdir.path().join("secondary-query");
    let primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;

    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    primary.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let secondary = SecondaryChainStore::open(
        &primary_path,
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    assert_eq!(secondary.current_chain_epoch()?, Some(first_epoch));

    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);
    primary.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block.clone()],
        vec![second_compact_block],
    ))?;

    assert_eq!(secondary.current_chain_epoch()?, Some(first_epoch));
    let catchup = secondary.try_catch_up()?;
    assert_eq!(catchup.before, Some(first_epoch.id));
    assert_eq!(catchup.after, Some(second_epoch.id));

    let reader = secondary.current_chain_epoch_reader()?;
    assert_eq!(reader.chain_epoch(), second_epoch);
    assert_eq!(reader.block_at(BlockHeight::new(2))?, Some(second_block));

    Ok(())
}

#[test]
fn secondary_open_rejects_network_mismatch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let secondary_path = tempdir.path().join("secondary-query");
    let _primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;

    let Err(error) = SecondaryChainStore::open(
        &primary_path,
        &secondary_path,
        ChainStoreOptions {
            network: Some(Network::ZcashTestnet),
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!("expected secondary network mismatch"));
    };

    assert!(
        matches!(error, StoreError::ChainEpochNetworkMismatch { .. }),
        "unexpected error: {error:?}"
    );

    Ok(())
}

#[test]
fn checkpoint_round_trip_preserves_visible_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let checkpoint_path = tempdir.path().join("checkpoint");
    let primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    primary.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block.clone()],
        vec![compact_block],
    ))?;

    primary.create_checkpoint(&checkpoint_path)?;

    let checkpoint =
        PrimaryChainStore::open(&checkpoint_path, ChainStoreOptions::for_local_tests())?;
    assert_eq!(checkpoint.current_chain_epoch()?, Some(chain_epoch));
    let reader = checkpoint.current_chain_epoch_reader()?;
    assert_eq!(reader.block_at(BlockHeight::new(1))?, Some(block));

    Ok(())
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    let source_hash = block_hash(height);
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            tip_height: block_height,
            tip_hash: source_hash,
            finalized_height: block_height,
            finalized_hash: source_hash,
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_000_000 + u64::from(height)),
        },
        BlockArtifact::new(
            block_height,
            source_hash,
            parent_hash,
            format!("raw-block-{height}").into_bytes(),
        ),
        CompactBlockArtifact::new(
            block_height,
            source_hash,
            format!("compact-block-{height}").into_bytes(),
        ),
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
