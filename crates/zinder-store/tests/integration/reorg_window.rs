#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch,
    ChainEpochId, ChainTipMetadata, CompactBlockArtifact, Network, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange, TransactionArtifact,
    TransactionId, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_store::{
    ArtifactFamily, ChainEpochArtifacts, ChainEvent, ChainStoreOptions, PrimaryChainStore,
    ReorgWindowChange, StoreError,
};

#[test]
fn replacing_non_finalized_state_emits_chain_reorged() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch(2, 2, 1, replacement_hash, block_hash(1));
    let reorged = store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block.clone()],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;

    let ChainEvent::ChainReorged {
        reverted,
        committed,
    } = &reorged.event
    else {
        return Err(eyre!("expected ChainReorged, got {:?}", reorged.event));
    };

    assert_eq!(reorged.chain_epoch, replacement_epoch);
    assert_eq!(reverted.chain_epoch, initial_epoch);
    assert_eq!(reverted.block_range.start, BlockHeight::new(2));
    assert_eq!(reverted.block_range.end, BlockHeight::new(2));
    assert_eq!(committed.chain_epoch, replacement_epoch);

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.block_at(BlockHeight::new(2))?,
        Some(replacement_block)
    );

    Ok(())
}

#[test]
fn replacement_cannot_cross_finalized_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 2, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 2, 1, block_hash(20), block_hash(1));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected reorg-window error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ReorgWindowExceeded { .. }));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn replacement_beyond_reorg_window_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 102, 0, block_hash(102), block_hash(101));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 102, 0, block_hash(202), block_hash(101));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected reorg-window error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ReorgWindowExceeded { .. }));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn replacement_start_after_current_tip_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(2));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(3),
        }),
    ) {
        Ok(event) => {
            return Err(eyre!("expected invalid replacement error, got {event:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn finalized_height_can_advance_without_new_block_artifacts() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;
    let initial_reader = store.current_chain_epoch_reader()?;
    let visible_tip_block = initial_reader
        .block_at(BlockHeight::new(2))?
        .ok_or_else(|| eyre!("expected visible tip block"))?;

    let finalized_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        finalized_height: BlockHeight::new(2),
        finalized_hash: block_hash(2),
        created_at: UnixTimestampMillis::new(1_774_668_101_000),
        ..initial_epoch
    };
    let committed = store.commit_chain_epoch(
        ChainEpochArtifacts::new(finalized_epoch, Vec::new(), Vec::new()).with_reorg_window_change(
            ReorgWindowChange::FinalizeThrough {
                height: BlockHeight::new(2),
            },
        ),
    )?;

    assert_eq!(committed.chain_epoch, finalized_epoch);
    assert_eq!(
        committed.event_envelope.finalized_height,
        BlockHeight::new(2)
    );
    assert!(matches!(
        committed.event,
        ChainEvent::ChainCommitted { committed }
            if committed.chain_epoch == finalized_epoch
                && committed.block_range.start == BlockHeight::new(2)
                && committed.block_range.end == BlockHeight::new(2)
    ));
    assert_eq!(store.current_chain_epoch()?, Some(finalized_epoch));
    let finalized_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        finalized_reader.block_at(BlockHeight::new(2))?,
        Some(visible_tip_block)
    );

    Ok(())
}

#[test]
fn unchanged_commit_without_visible_transition_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let attempted_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        created_at: UnixTimestampMillis::new(1_774_668_101_000),
        ..initial_epoch
    };
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(attempted_epoch, Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::Unchanged),
    ) {
        Ok(event) => return Err(eyre!("expected no-op commit error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { reason }
            if reason == "at least one finalized block artifact is required"
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn replacement_start_after_attempted_tip_reports_boundary_error() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 5, 1, block_hash(5), block_hash(4));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 4, 1, block_hash(40), block_hash(3));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(5),
        }),
    ) {
        Ok(event) => {
            return Err(eyre!("expected invalid replacement error, got {event:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { reason }
            if reason == "replacement start height cannot exceed tip height"
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn finalize_through_height_can_be_below_epoch_finalized_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 3, 1, block_hash(3), block_hash(2));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let finalized_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        finalized_height: BlockHeight::new(3),
        finalized_hash: block_hash(3),
        created_at: UnixTimestampMillis::new(1_774_668_101_000),
        ..initial_epoch
    };
    let committed = store.commit_chain_epoch(
        ChainEpochArtifacts::new(finalized_epoch, Vec::new(), Vec::new()).with_reorg_window_change(
            ReorgWindowChange::FinalizeThrough {
                height: BlockHeight::new(2),
            },
        ),
    )?;

    assert_eq!(committed.chain_epoch, finalized_epoch);
    assert_eq!(store.current_chain_epoch()?, Some(finalized_epoch));

    Ok(())
}

#[test]
fn append_extend_window_ending_at_tip_is_valid() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, appended_block, appended_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(2));
    let committed = store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![appended_block],
            vec![appended_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(BlockHeight::new(3), BlockHeight::new(3)),
        }),
    )?;

    assert_eq!(committed.chain_epoch, attempted_epoch);
    assert_eq!(store.current_chain_epoch()?, Some(attempted_epoch));

    Ok(())
}

#[test]
fn extend_window_end_after_tip_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, appended_block, appended_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(2));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![appended_block],
            vec![appended_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(BlockHeight::new(3), BlockHeight::new(4)),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected extend-window error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { reason }
            if reason == "reorg-window extension cannot exceed tip height"
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn replacement_cannot_lower_finalized_anchor() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 6, 5, block_hash(6), block_hash(5));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 6, 4, block_hash(60), block_hash(5));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(6),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected finalized-anchor error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn replacement_cannot_change_finalized_anchor_hash() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 6, 5, block_hash(6), block_hash(5));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (mut attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 6, 5, block_hash(60), block_hash(5));
    attempted_epoch.finalized_hash = block_hash(50);
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(6),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected finalized-anchor error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn non_replacement_commit_cannot_lower_tip_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 1, 1, block_hash(1), block_hash(0));
    let error = match store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            attempted_epoch,
            vec![attempted_block],
            vec![attempted_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(1)),
        }),
    ) {
        Ok(event) => return Err(eyre!("expected invalid epoch error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn non_replacement_commit_cannot_lower_finalized_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (chain_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 2, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(chain_epoch))?;

    let (attempted_epoch, attempted_block, attempted_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(2));
    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        attempted_epoch,
        vec![attempted_block],
        vec![attempted_compact_block],
    )) {
        Ok(event) => return Err(eyre!("expected invalid epoch error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));

    Ok(())
}

#[test]
fn append_commit_cannot_publish_block_for_existing_visible_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, appended_block, appended_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(2));
    let rewritten_block = BlockArtifact::new(
        BlockHeight::new(2),
        block_hash(20),
        block_hash(1),
        b"rewritten-block-2".to_vec(),
    );
    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        attempted_epoch,
        vec![rewritten_block, appended_block],
        vec![appended_compact_block],
    )) {
        Ok(event) => return Err(eyre!("expected invalid epoch error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn append_commit_must_link_first_new_block_to_current_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (attempted_epoch, appended_block, appended_compact_block) =
        synthetic_epoch(2, 3, 1, block_hash(3), block_hash(99));
    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        attempted_epoch,
        vec![appended_block],
        vec![appended_compact_block],
    )) {
        Ok(event) => return Err(eyre!("expected invalid epoch error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn finalized_hash_must_match_existing_visible_block() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let (initial_epoch, _block, _compact_block) =
        synthetic_epoch(1, 2, 1, block_hash(2), block_hash(1));
    store.commit_chain_epoch(artifacts_from_height_one_to_tip(initial_epoch))?;

    let (mut attempted_epoch, appended_block, appended_compact_block) =
        synthetic_epoch(2, 3, 2, block_hash(3), block_hash(2));
    attempted_epoch.finalized_hash = block_hash(99);
    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        attempted_epoch,
        vec![appended_block],
        vec![appended_compact_block],
    )) {
        Ok(event) => return Err(eyre!("expected invalid epoch error, got {event:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.current_chain_epoch()?, Some(initial_epoch));

    Ok(())
}

#[test]
fn transaction_lookup_ignores_reverted_branch_artifacts() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let transaction_id = TransactionId::from_bytes([9; 32]);
    let old_hash = block_hash(2);
    let old_transaction = TransactionArtifact::new(
        transaction_id,
        BlockHeight::new(2),
        old_hash,
        b"old-transaction".to_vec(),
    );
    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, old_hash, block_hash(1));
    store.commit_chain_epoch(
        artifacts_from_height_one_to_tip(initial_epoch)
            .with_transactions(vec![old_transaction.clone()]),
    )?;

    let initial_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        initial_reader.transaction_by_id(transaction_id)?,
        Some(old_transaction.clone())
    );

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch(2, 2, 1, replacement_hash, block_hash(1));
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;

    assert_eq!(
        initial_reader.transaction_by_id(transaction_id)?,
        Some(old_transaction)
    );

    let replacement_reader = store.current_chain_epoch_reader()?;
    assert_eq!(replacement_reader.transaction_by_id(transaction_id)?, None);

    Ok(())
}

#[test]
fn subtree_root_lookup_ignores_reverted_branch_artifacts() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let old_hash = block_hash(2);
    let old_subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([7; 32]),
        BlockHeight::new(2),
        old_hash,
    );
    let subtree_range = SubtreeRootRange::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid subtree root range"))?,
    );
    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, old_hash, block_hash(1));
    store.commit_chain_epoch(
        artifacts_from_height_one_to_tip(initial_epoch)
            .with_subtree_roots(vec![old_subtree_root.clone()]),
    )?;

    let initial_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        initial_reader.subtree_roots(subtree_range)?,
        vec![Some(old_subtree_root.clone())]
    );

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch(2, 2, 1, replacement_hash, block_hash(1));
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;

    assert_eq!(
        initial_reader.subtree_roots(subtree_range)?,
        vec![Some(old_subtree_root)]
    );

    let replacement_reader = store.current_chain_epoch_reader()?;
    assert_eq!(replacement_reader.subtree_roots(subtree_range)?, vec![None]);

    Ok(())
}

#[test]
fn tree_state_lookup_ignores_reverted_branch_artifacts() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let old_hash = block_hash(2);
    let old_tree_state =
        TreeStateArtifact::new(BlockHeight::new(2), old_hash, b"old-tree-state".to_vec());
    let (initial_epoch, _initial_block, _initial_compact_block) =
        synthetic_epoch(1, 2, 1, old_hash, block_hash(1));
    store.commit_chain_epoch(
        artifacts_from_height_one_to_tip(initial_epoch)
            .with_tree_states(vec![old_tree_state.clone()]),
    )?;

    let initial_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        initial_reader.tree_state_at(BlockHeight::new(2))?,
        Some(old_tree_state.clone())
    );

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch(2, 2, 1, replacement_hash, block_hash(1));
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;

    assert_eq!(
        initial_reader.tree_state_at(BlockHeight::new(2))?,
        Some(old_tree_state)
    );

    let replacement_reader = store.current_chain_epoch_reader()?;
    let error = match replacement_reader.tree_state_at(BlockHeight::new(2)) {
        Ok(tree_state) => {
            return Err(eyre!(
                "expected missing tree-state error, got {tree_state:?}"
            ));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::ArtifactMissing {
            family: ArtifactFamily::TreeState,
            ..
        }
    ));

    Ok(())
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
    finalized_height: u32,
    hash: BlockHash,
    parent_hash: BlockHash,
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            tip_height: block_height,
            tip_hash: hash,
            finalized_height: BlockHeight::new(finalized_height),
            finalized_hash: block_hash(finalized_height),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_100_000 + u64::from(height)),
        },
        BlockArtifact::new(
            block_height,
            hash,
            parent_hash,
            format!("raw-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
        CompactBlockArtifact::new(
            block_height,
            hash,
            format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
    )
}

fn artifacts_from_height_one_to_tip(chain_epoch: ChainEpoch) -> ChainEpochArtifacts {
    let mut blocks = Vec::new();
    let mut compact_blocks = Vec::new();

    for height in 1..=chain_epoch.tip_height.value() {
        let block_height = BlockHeight::new(height);
        let block_hash = if block_height == chain_epoch.tip_height {
            chain_epoch.tip_hash
        } else {
            self::block_hash(height)
        };
        let parent_hash = self::block_hash(height.saturating_sub(1));

        blocks.push(BlockArtifact::new(
            block_height,
            block_hash,
            parent_hash,
            format!("raw-block-{}-{height}", chain_epoch.id.value()).into_bytes(),
        ));
        compact_blocks.push(CompactBlockArtifact::new(
            block_height,
            block_hash,
            format!("compact-block-{}-{height}", chain_epoch.id.value()).into_bytes(),
        ));
    }

    ChainEpochArtifacts::new(chain_epoch, blocks, compact_blocks)
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
