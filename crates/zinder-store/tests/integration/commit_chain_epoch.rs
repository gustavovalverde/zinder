#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{sync::Arc, thread};

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId, ChainEpoch,
    ChainEpochId, ChainTipMetadata, CompactBlockArtifact, CompactTransaction,
    CompactTransactionData, Network, TransactionId, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_store::{
    ChainEvent, ChainEventHistoryRequest, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
    StoreError,
};

#[test]
fn commit_chain_epoch_writes_artifacts_and_visible_epoch_atomically() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);

    let committed = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
    assert_eq!(reader.block_header_at(BlockHeight::new(1))?, Some(block));
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(1))?,
        Some(compact_block)
    );

    Ok(())
}

#[test]
fn commit_chain_epoch_can_publish_genesis_artifacts() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 0);

    let committed = store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block.clone()],
            vec![compact_block.clone()],
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(BlockHeight::new(0), BlockHeight::new(0)),
        }),
    )?;

    assert_eq!(committed.chain_epoch, chain_epoch);
    assert_eq!(
        committed.block_range,
        BlockHeightRange::inclusive(BlockHeight::new(0), BlockHeight::new(0))
    );
    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(reader.block_header_at(BlockHeight::new(0))?, Some(block));
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(0))?,
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
fn zero_max_wal_bytes_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let mut rocksdb_resource_budget = zinder_store::RocksDbResourceBudget::for_local_tests();
    rocksdb_resource_budget.max_wal_bytes = 0;
    let Err(error) = PrimaryChainStore::open(
        tempdir.path(),
        ChainStoreOptions {
            rocksdb_resource_budget,
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!(
            "expected invalid options: max_wal_bytes = 0 reopens the OOM trap"
        ));
    };

    assert!(matches!(error, StoreError::InvalidChainStoreOptions { .. }));

    Ok(())
}

#[test]
fn negative_max_open_files_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let mut rocksdb_resource_budget = zinder_store::RocksDbResourceBudget::for_local_tests();
    rocksdb_resource_budget.max_open_files = -1;
    let Err(error) = PrimaryChainStore::open(
        tempdir.path(),
        ChainStoreOptions {
            rocksdb_resource_budget,
            ..ChainStoreOptions::for_local_tests()
        },
    ) else {
        return Err(eyre!(
            "expected invalid options: max_open_files = -1 pins every SST's metadata"
        ));
    };

    assert!(matches!(error, StoreError::InvalidChainStoreOptions { .. }));

    Ok(())
}

#[test]
fn concurrent_same_epoch_commits_do_not_both_publish() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
            store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
fn first_commit_requires_chain_epoch_one() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (mut epoch, block, compact_block) = synthetic_epoch(1, 1);
    epoch.id = ChainEpochId::new(2);

    let error = store
        .commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            epoch,
            vec![block],
            vec![compact_block],
        ))
        .err()
        .ok_or_else(|| eyre!("first commit with epoch id 2 was accepted"))?;

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts {
            reason: "first chain epoch id must be 1",
        }
    ));
    Ok(())
}

#[test]
fn commit_requires_the_next_chain_epoch_id() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;
    let (mut skipped_epoch, skipped_block, skipped_compact_block) = synthetic_epoch(2, 2);
    skipped_epoch.id = ChainEpochId::new(3);

    let error = store
        .commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            skipped_epoch,
            vec![skipped_block],
            vec![skipped_compact_block],
        ))
        .err()
        .ok_or_else(|| eyre!("commit that skipped epoch id 2 was accepted"))?;

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts {
            reason: "chain epoch id must increase by exactly one",
        }
    ));
    Ok(())
}

#[test]
fn commit_rejects_compact_block_without_matching_block() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let compact_block = CompactBlockArtifact::empty(
        BlockId::new(compact_block.height(), BlockHash::from_bytes([99; 32])),
        compact_block.previous_block_hash(),
        compact_block.time(),
        compact_block.chain_metadata(),
    );

    let error = match store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
fn commit_rejects_compact_block_parent_or_time_mismatch() -> eyre::Result<()> {
    for (previous_block_hash, time) in [
        (BlockHash::from_bytes([99; 32]), 0),
        (BlockHash::from_bytes([0; 32]), 1),
    ] {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        let compact_block = CompactBlockArtifact::empty(
            BlockId::new(compact_block.height(), compact_block.block_hash()),
            previous_block_hash,
            time,
            compact_block.chain_metadata(),
        );

        let error = store
            .commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
                chain_epoch,
                vec![block],
                vec![compact_block],
            ))
            .err()
            .ok_or_else(|| eyre!("compact parent/time mismatch must be rejected"))?;
        assert!(matches!(
            error,
            StoreError::InvalidChainEpochArtifacts { .. }
        ));
    }
    Ok(())
}

#[test]
fn commit_rejects_compact_transaction_index_or_id_mismatch() -> eyre::Result<()> {
    for (compact_index, compact_transaction_id) in [
        (1, TransactionId::from_bytes([1; 32])),
        (0, TransactionId::from_bytes([2; 32])),
    ] {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        let canonical_transaction_id = TransactionId::from_bytes([1; 32]);
        let (transaction_index, transaction_location, transaction_facts, transaction_blob) =
            super::synthetic_transaction_rows(
                canonical_transaction_id,
                block.height,
                block.block_hash,
                0,
                b"tx",
            );
        let compact_block = CompactBlockArtifact::new(
            BlockId::new(compact_block.height(), compact_block.block_hash()),
            compact_block.previous_block_hash(),
            compact_block.time(),
            vec![CompactTransaction {
                index: compact_index,
                transaction_id: compact_transaction_id,
                data: CompactTransactionData::default(),
            }],
            compact_block.chain_metadata(),
        )?;
        let artifacts =
            super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
                .with_block_transaction_index(vec![transaction_index])
                .with_transaction_locations(vec![transaction_location])
                .with_transaction_facts(vec![transaction_facts])
                .with_transaction_blobs(vec![transaction_blob]);

        let error = store
            .commit_chain_epoch(artifacts)
            .err()
            .ok_or_else(|| eyre!("compact transaction mismatch must be rejected"))?;
        assert!(matches!(
            error,
            StoreError::InvalidChainEpochArtifacts { .. }
        ));
    }
    Ok(())
}

#[test]
fn commit_rejects_epoch_zero() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(0, 1);

    let error = match store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let (attempted_epoch, _tip_block, _tip_compact_block) = synthetic_epoch(2, 3);
    let (_, height_2_block, height_2_compact_block) = synthetic_epoch(2, 2);
    let error = match store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
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
    let (transaction_index, transaction_location, transaction_facts, transaction_blob) =
        super::synthetic_transaction_rows(
            TransactionId::from_bytes([1; 32]),
            BlockHeight::new(2),
            block.block_hash,
            0,
            b"tx",
        );

    let error = match store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
            .with_block_transaction_index(vec![transaction_index])
            .with_transaction_locations(vec![transaction_location])
            .with_transaction_facts(vec![transaction_facts])
            .with_transaction_blobs(vec![transaction_blob]),
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
        u32::try_from(block.block_time)?,
        b"tree-state".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
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
        u32::try_from(block.block_time)?,
        b"tree-state".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
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
fn commit_rejects_tree_state_with_wrong_block_time() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        block.height,
        block.block_hash,
        u32::try_from(block.block_time)?.saturating_add(1),
        b"tree-state".to_vec(),
    );

    let error = match store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
            .with_tree_states(vec![tree_state]),
    ) {
        Ok(outcome) => return Err(eyre!("expected invalid artifacts, got {outcome:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts {
            reason: "tree-state artifact block time must match its block artifact"
        }
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
    let bootstrap_tip_metadata = ChainTipMetadata::new(130_002, 39_758, 0);
    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        visible_tip_height: bootstrap_height,
        visible_tip_hash: bootstrap_hash,
        settled_tip_height: bootstrap_height,
        settled_tip_hash: bootstrap_hash,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: bootstrap_tip_metadata,
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    let committed = store.commit_artifactless_checkpoint(bootstrap_chain_epoch)?;
    assert_eq!(committed.chain_epoch, bootstrap_chain_epoch);
    assert_eq!(
        committed.block_range,
        BlockHeightRange::empty_at(bootstrap_height)
    );
    assert_eq!(store.current_chain_epoch()?, Some(bootstrap_chain_epoch));
    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(reader.chain_epoch(), bootstrap_chain_epoch);
    // Heights at or above the bootstrap tip return Ok(None) because the
    // chain has no artifacts beyond the checkpoint; heights below surface a
    // typed canonical-history error.
    assert_eq!(reader.block_header_at(BlockHeight::new(2_000))?, None);
    assert!(matches!(
        reader.block_header_at(BlockHeight::new(1)),
        Err(StoreError::CanonicalHistoryUnavailable { .. })
    ));
    assert!(matches!(
        reader.compact_block_at(BlockHeight::new(1)),
        Err(StoreError::CanonicalHistoryUnavailable { .. })
    ));
    Ok(())
}

#[test]
fn bootstrap_epoch_rejects_replace_below_checkpoint_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    // Bootstrap a stub chain epoch at the checkpoint height. `settled_tip_height`
    // is pinned to the checkpoint, which is the load-bearing invariant for the
    // reorg-below-checkpoint defense: any subsequent Replace whose `from_height`
    // would rewind through the checkpoint runs into `minimum_reorg_height =
    // settled_tip_height + 1` and surfaces `StoreError::ReorgWindowExceeded`.
    let checkpoint_height = BlockHeight::new(1_000);
    let checkpoint_hash = block_hash(1_000);
    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        visible_tip_height: checkpoint_height,
        visible_tip_hash: checkpoint_hash,
        settled_tip_height: checkpoint_height,
        settled_tip_hash: checkpoint_hash,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::new(130_002, 39_758, 0),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };
    store.commit_artifactless_checkpoint(bootstrap_chain_epoch)?;

    // Attempt a reorg whose `from_height` rewinds onto the checkpoint height
    // itself. `minimum_reorg_height = settled_tip_height + 1 = 1001`, so 1000 is
    // already below the floor and must be rejected. Artifacts at 1000 are
    // supplied only to clear `validate_artifact_presence`; the reorg-window
    // check fires before any coverage validation.
    let attempted_from_height = checkpoint_height;
    let replaced_tip_hash = BlockHash::from_bytes([0xa5; 32]);
    let replacement_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        visible_tip_height: checkpoint_height,
        visible_tip_hash: replaced_tip_hash,
        settled_tip_height: checkpoint_height,
        settled_tip_hash: replaced_tip_hash,
        artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::new(130_002, 39_758, 0),
        created_at: UnixTimestampMillis::new(1_774_668_000_001),
    };
    let replaced_block = super::synthetic_block_header(
        checkpoint_height,
        replaced_tip_hash,
        block_hash(checkpoint_height.value().saturating_sub(1)),
        b"raw-replaced-block",
    );
    let replaced_compact_block = super::empty_compact_block_for_header(
        &replaced_block,
        replacement_chain_epoch.tip_metadata,
    );
    let outcome = store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(
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
                settled_tip_height: settled_tip,
                ..
            } if attempted == attempted_from_height && settled_tip == checkpoint_height
        ),
        "expected ReorgWindowExceeded with attempted={attempted_from_height:?} \
         and settled_tip={checkpoint_height:?}; got {error:?}"
    );

    Ok(())
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let source_hash = block_hash(height);
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);
    let block = super::synthetic_block_header(
        block_height,
        source_hash,
        parent_hash,
        format!("raw-block-{height}").as_bytes(),
    );
    let compact = super::empty_compact_block_for_header(&block, ChainTipMetadata::empty());

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: block_height,
            visible_tip_hash: source_hash,
            settled_tip_height: block_height,
            settled_tip_hash: source_hash,
            artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_000_000 + u64::from(height)),
        },
        block,
        compact,
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
