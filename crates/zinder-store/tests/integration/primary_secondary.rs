#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainStoreOptions, PrimaryChainStore, RawBlobRetention,
    SecondaryChainStore, StoreError,
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
    primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
    primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
    assert_eq!(
        reader.block_header_at(BlockHeight::new(2))?,
        Some(second_block)
    );

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
fn secondary_continues_serving_after_primary_drops() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let secondary_path = tempdir.path().join("secondary-query");

    // Phase 1: open the primary, commit two epochs, drop the primary
    // handle. This stands in for an unclean writer shutdown (process
    // crash, SIGKILL, host failure): readers that come up afterward must
    // still see the last durable state.
    let final_epoch = {
        let primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
        let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
        primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            first_epoch,
            vec![first_block],
            vec![first_compact_block],
        ))?;
        let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);
        primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        ))?;
        second_epoch
    };

    // Phase 2: a fresh secondary opened against the now-closed primary
    // serves the last committed epoch without needing the primary to
    // come back up.
    let secondary = SecondaryChainStore::open(
        &primary_path,
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    assert_eq!(secondary.current_chain_epoch()?, Some(final_epoch));
    let reader = secondary.current_chain_epoch_reader()?;
    assert_eq!(reader.chain_epoch(), final_epoch);
    assert!(reader.block_header_at(BlockHeight::new(2))?.is_some());

    // Phase 3: a new primary opened against the same path resumes from
    // the durable state. Validates that an operator restart after a
    // crash does not regress to genesis.
    let restarted_primary =
        PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
    assert_eq!(restarted_primary.current_chain_epoch()?, Some(final_epoch));
    Ok(())
}

#[test]
fn checkpoint_round_trip_preserves_visible_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let checkpoint_path = tempdir.path().join("checkpoint");
    let primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block.clone()],
        vec![compact_block],
    ))?;

    primary.create_checkpoint(&checkpoint_path)?;

    let checkpoint =
        PrimaryChainStore::open(&checkpoint_path, ChainStoreOptions::for_local_tests())?;
    assert_eq!(checkpoint.current_chain_epoch()?, Some(chain_epoch));
    let reader = checkpoint.current_chain_epoch_reader()?;
    assert_eq!(reader.block_header_at(BlockHeight::new(1))?, Some(block));

    Ok(())
}

#[test]
fn secondary_reads_persisted_raw_blob_retention_after_catch_up() -> eyre::Result<()> {
    for retention in [
        RawBlobRetention::None,
        RawBlobRetention::Transactions,
        RawBlobRetention::All,
    ] {
        let tempdir = tempdir()?;
        let primary_path = tempdir.path().join("primary");
        let secondary_path = tempdir.path().join("secondary-query");
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions {
                raw_blob_retention: retention,
                ..ChainStoreOptions::for_local_tests()
            },
        )?;
        assert_eq!(primary.raw_blob_retention()?, retention);

        let secondary = SecondaryChainStore::open(
            &primary_path,
            &secondary_path,
            ChainStoreOptions::for_local_tests(),
        )?;
        secondary.try_catch_up()?;
        assert_eq!(secondary.raw_blob_retention()?, retention);
    }

    Ok(())
}

#[test]
fn empty_primary_reopen_can_change_raw_blob_retention() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    {
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions {
                raw_blob_retention: RawBlobRetention::None,
                ..ChainStoreOptions::for_local_tests()
            },
        )?;
        assert_eq!(primary.raw_blob_retention()?, RawBlobRetention::None);
    }

    let primary = PrimaryChainStore::open(
        &primary_path,
        ChainStoreOptions {
            raw_blob_retention: RawBlobRetention::All,
            ..ChainStoreOptions::for_local_tests()
        },
    )?;
    assert_eq!(primary.raw_blob_retention()?, RawBlobRetention::All);

    Ok(())
}

#[test]
fn committed_primary_reopen_rejects_raw_blob_retention_change() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let committed_epoch;
    {
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions {
                raw_blob_retention: RawBlobRetention::None,
                ..ChainStoreOptions::for_local_tests()
            },
        )?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        committed_epoch = chain_epoch;
        primary.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
    }

    let Err(error) = PrimaryChainStore::open(
        &primary_path,
        ChainStoreOptions {
            raw_blob_retention: RawBlobRetention::All,
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!("expected retention mismatch on committed store"));
    };
    assert!(matches!(
        error,
        StoreError::RawBlobRetentionMismatch {
            persisted: RawBlobRetention::None,
            configured: RawBlobRetention::All,
        }
    ));

    let reopened = PrimaryChainStore::open(
        &primary_path,
        ChainStoreOptions {
            raw_blob_retention: RawBlobRetention::None,
            ..ChainStoreOptions::for_local_tests()
        },
    )?;
    assert_eq!(reopened.raw_blob_retention()?, RawBlobRetention::None);
    assert_eq!(reopened.current_chain_epoch()?, Some(committed_epoch));

    Ok(())
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let source_hash = block_hash(height);
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: block_height,
            visible_tip_hash: source_hash,
            settled_tip_height: block_height,
            settled_tip_hash: source_hash,
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_000_000 + u64::from(height)),
        },
        super::synthetic_block_header(
            block_height,
            source_hash,
            parent_hash,
            format!("raw-block-{height}").as_bytes(),
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
