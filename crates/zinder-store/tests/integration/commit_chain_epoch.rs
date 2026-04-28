#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{sync::Arc, thread};

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, TransactionArtifact, TransactionId,
    TreeStateArtifact, UnixTimestampMillis,
};
use zinder_store::{
    ChainEpochArtifacts, ChainEvent, ChainEventHistoryRequest, ChainStoreOptions,
    PrimaryChainStore, ReorgWindowChange, StoreError,
};

#[test]
fn commit_chain_epoch_writes_artifacts_and_visible_epoch_atomically() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);

    let committed = store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block.clone()],
        vec![compact_block.clone()],
    ))?;

    assert_eq!(committed.chain_epoch, chain_epoch);
    assert!(matches!(
        committed.event,
        ChainEvent::ChainCommitted { committed }
            if committed.chain_epoch == chain_epoch
    ));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    let reader = store.current_chain_epoch_reader()?;

    assert_eq!(reader.chain_epoch(), chain_epoch);
    assert_eq!(reader.block_at(BlockHeight::new(1))?, Some(block));
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(1))?,
        Some(compact_block)
    );

    Ok(())
}

#[test]
fn empty_store_has_no_current_chain_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    assert_eq!(store.current_chain_epoch()?, None);
    assert!(matches!(
        store.current_chain_epoch_reader(),
        Err(StoreError::NoVisibleChainEpoch)
    ));

    Ok(())
}

#[test]
fn store_network_metadata_rejects_mismatched_reopen() -> eyre::Result<()> {
    let tempdir = tempdir()?;

    {
        let _store = PrimaryChainStore::open(
            tempdir.path(),
            ChainStoreOptions {
                network: Some(Network::ZcashRegtest),
                ..ChainStoreOptions::for_local_tests()
            },
        )?;
    }

    let Err(error) = PrimaryChainStore::open(
        tempdir.path(),
        ChainStoreOptions {
            network: Some(Network::ZcashTestnet),
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!("expected network mismatch"));
    };

    assert!(matches!(
        error,
        StoreError::ChainEpochNetworkMismatch { .. }
    ));

    Ok(())
}

#[test]
fn zero_reorg_window_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let Err(error) = PrimaryChainStore::open(
        tempdir.path(),
        ChainStoreOptions {
            reorg_window_blocks: 0,
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!("expected invalid options"));
    };

    assert!(matches!(error, StoreError::InvalidChainStoreOptions { .. }));

    Ok(())
}

#[test]
fn concurrent_same_epoch_commits_do_not_both_publish() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let store = Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);
            store.commit_chain_epoch(ChainEpochArtifacts::new(
                second_epoch,
                vec![second_block],
                vec![second_compact_block],
            ))
        }));
    }

    let mut successes = 0;
    let mut conflicts = 0;
    for handle in handles {
        match handle.join().map_err(|_| eyre!("commit thread panicked"))? {
            Ok(_) => successes += 1,
            Err(StoreError::ChainEpochConflict { .. }) => conflicts += 1,
            Err(error) => return Err(eyre!("unexpected commit error: {error}")),
        }
    }

    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
    assert_eq!(
        store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?
            .len(),
        2
    );

    Ok(())
}

#[test]
fn commit_rejects_compact_block_without_matching_block() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, mut compact_block) = synthetic_epoch(1, 1);
    compact_block.block_hash = BlockHash::from_bytes([99; 32]);

    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    )) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));

    Ok(())
}

#[test]
fn commit_rejects_epoch_zero() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(0, 1);

    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    )) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));

    Ok(())
}

#[test]
fn append_commit_must_include_the_new_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let (attempted_epoch, _tip_block, _tip_compact_block) = synthetic_epoch(2, 3);
    let (_, height_2_block, height_2_compact_block) = synthetic_epoch(2, 2);
    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        attempted_epoch,
        vec![height_2_block],
        vec![height_2_compact_block],
    )) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(first_epoch));

    Ok(())
}

#[test]
fn commit_rejects_transaction_above_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let transaction = TransactionArtifact::new(
        TransactionId::from_bytes([1; 32]),
        BlockHeight::new(2),
        block.block_hash,
        b"tx".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transactions(vec![transaction]),
    ) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));

    Ok(())
}

#[test]
fn commit_rejects_tree_state_above_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(2),
        block.block_hash,
        b"tree-state".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_tree_states(vec![tree_state]),
    ) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));

    Ok(())
}

#[test]
fn commit_rejects_tree_state_for_wrong_block_hash() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(1),
        BlockHash::from_bytes([99; 32]),
        b"tree-state".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_tree_states(vec![tree_state]),
    ) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));

    Ok(())
}

#[test]
fn empty_store_accepts_bootstrap_commit_with_finalize_through_and_no_artifacts() -> eyre::Result<()>
{
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let bootstrap_height = BlockHeight::new(1_000);
    let bootstrap_hash = block_hash(1_000);
    let bootstrap_tip_metadata = ChainTipMetadata::new(130_002, 39_758);
    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: bootstrap_height,
        tip_hash: bootstrap_hash,
        finalized_height: bootstrap_height,
        finalized_hash: bootstrap_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: bootstrap_tip_metadata,
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    let committed = store.commit_chain_epoch(
        ChainEpochArtifacts::new(bootstrap_chain_epoch, Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
                height: bootstrap_height,
            }),
    )?;
    assert_eq!(committed.chain_epoch, bootstrap_chain_epoch);
    assert_eq!(store.current_chain_epoch()?, Some(bootstrap_chain_epoch));
    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(reader.chain_epoch(), bootstrap_chain_epoch);
    // Heights at or above the bootstrap tip return Ok(None) because the
    // chain has no artifacts beyond the checkpoint; heights below surface a
    // typed `ArtifactMissing` error which the wallet layer already maps to
    // `QueryError::ArtifactUnavailable`.
    assert_eq!(reader.block_at(BlockHeight::new(2_000))?, None);
    assert!(matches!(
        reader.block_at(BlockHeight::new(1)),
        Err(StoreError::ArtifactMissing { .. })
    ));
    assert!(matches!(
        reader.compact_block_at(BlockHeight::new(1)),
        Err(StoreError::ArtifactMissing { .. })
    ));
    Ok(())
}

#[test]
fn bootstrap_epoch_rejects_replace_below_checkpoint_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    // Bootstrap a stub chain epoch at the checkpoint height. `finalized_height`
    // is pinned to the checkpoint, which is the load-bearing invariant for the
    // reorg-below-checkpoint defense: any subsequent Replace whose `from_height`
    // would rewind through the checkpoint runs into `minimum_reorg_height =
    // finalized_height + 1` and surfaces `StoreError::ReorgWindowExceeded`.
    let checkpoint_height = BlockHeight::new(1_000);
    let checkpoint_hash = block_hash(1_000);
    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: checkpoint_height,
        tip_hash: checkpoint_hash,
        finalized_height: checkpoint_height,
        finalized_hash: checkpoint_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::new(130_002, 39_758),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(bootstrap_chain_epoch, Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
                height: checkpoint_height,
            }),
    )?;

    // Attempt a reorg whose `from_height` rewinds onto the checkpoint height
    // itself. `minimum_reorg_height = finalized_height + 1 = 1001`, so 1000 is
    // already below the floor and must be rejected. Artifacts at 1000 are
    // supplied only to clear `validate_artifact_presence`; the reorg-window
    // check fires before any coverage validation.
    let attempted_from_height = checkpoint_height;
    let replaced_tip_hash = BlockHash::from_bytes([0xa5; 32]);
    let replacement_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        tip_height: checkpoint_height,
        tip_hash: replaced_tip_hash,
        finalized_height: checkpoint_height,
        finalized_hash: replaced_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::new(130_002, 39_758),
        created_at: UnixTimestampMillis::new(1_774_668_000_001),
    };
    let replaced_block = BlockArtifact::new(
        checkpoint_height,
        replaced_tip_hash,
        block_hash(checkpoint_height.value().saturating_sub(1)),
        b"raw-replaced-block".to_vec(),
    );
    let replaced_compact_block = CompactBlockArtifact::new(
        checkpoint_height,
        replaced_tip_hash,
        b"compact-replaced-block".to_vec(),
    );
    let outcome = store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_chain_epoch,
            vec![replaced_block],
            vec![replaced_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: attempted_from_height,
        }),
    );

    let error = match outcome {
        Ok(committed) => {
            return Err(eyre!(
                "expected ReorgWindowExceeded; got committed epoch {committed:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            StoreError::ReorgWindowExceeded {
                attempted_from_height: attempted,
                finalized_height: finalized,
                ..
            } if attempted == attempted_from_height && finalized == checkpoint_height
        ),
        "expected ReorgWindowExceeded with attempted={attempted_from_height:?} \
         and finalized={checkpoint_height:?}; got {error:?}"
    );

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
