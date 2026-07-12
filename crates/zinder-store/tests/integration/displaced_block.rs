use std::num::NonZeroU32;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockBlobArtifact, BlockFinalNoteCommitmentRoots, BlockHash,
    BlockHeaderArtifact, BlockHeight, BlockId, BlockTransactionIndexArtifact, ChainEpoch,
    ChainEpochId, ChainTipMetadata, CompactBlockArtifact, FinalNoteCommitmentRoot, Network,
    ShieldedProtocol, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact, UnixTimestampMillis,
};
use zinder_store::{
    ChainEpochArtifacts, ChainEpochReader, ChainStoreOptions, DisplacedBlockStore,
    PrimaryChainStore, ReorgWindowChange, SecondaryChainStore, StoreError,
};

#[test]
fn replacement_archives_displaced_blocks_without_affecting_canonical_reads() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let secondary_path = tempdir.path().join("secondary");
    let old_hash_2 = block_hash(2);
    let old_hash_3 = block_hash(3);
    let coinbase_id_2 = transaction_id(2);
    let coinbase_id_3 = transaction_id(3);
    let replacement_hash_2 = block_hash(20);
    let replacement_hash_3 = block_hash(30);

    {
        let store = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
        store.commit_chain_epoch(initial_artifacts(
            old_hash_2,
            old_hash_3,
            coinbase_id_2,
            coinbase_id_3,
        ))?;
        let outcome = store.commit_chain_epoch(replacement_artifacts(
            replacement_hash_2,
            replacement_hash_3,
        ))?;

        assert_multi_block_archive(
            &store,
            outcome.event_envelope.event_sequence,
            [old_hash_2, old_hash_3],
            [coinbase_id_2, coinbase_id_3],
        )?;
        let canonical = store.current_chain_epoch_reader()?;
        assert_displaced_root_capture(
            &canonical,
            outcome.event_envelope.event_sequence,
            old_hash_2,
        )?;
        assert_eq!(
            canonical
                .block_header_at(BlockHeight::new(2))?
                .map(|block| block.block_hash),
            Some(replacement_hash_2)
        );
        assert_eq!(
            canonical
                .block_header_at(BlockHeight::new(3))?
                .map(|block| block.block_hash),
            Some(replacement_hash_3)
        );
        let historical = store.chain_epoch_reader_at(ChainEpochId::new(1))?;
        assert_historical_reader_rejects_displaced_roots(&historical)?;
    }

    let reopened = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
    assert_eq!(reopened.displaced_block_count()?, 2);
    assert_eq!(
        reopened
            .displaced_block_by_hash(old_hash_2)?
            .map(|block| block.block_hash),
        Some(old_hash_2)
    );
    drop(reopened);

    let secondary = SecondaryChainStore::open(
        &primary_path,
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    assert_eq!(secondary.displaced_block_count()?, 2);
    assert_eq!(secondary.newest_displaced_blocks(nonzero(8)?)?.len(), 2);
    let secondary_reader = secondary.current_chain_epoch_reader()?;
    assert_eq!(
        secondary_reader
            .displaced_root_candidates(ShieldedProtocol::Sapling, final_root(0x42), nonzero(8)?,)?
            .len(),
        1
    );

    Ok(())
}

#[test]
fn secondary_reader_pins_roots_coverage_and_canonical_validation_to_one_snapshot()
-> eyre::Result<()> {
    let tempdir = tempdir()?;
    let primary_path = tempdir.path().join("primary");
    let secondary_path = tempdir.path().join("secondary");
    let old_hash_2 = block_hash(2);
    let old_hash_3 = block_hash(3);
    let replacement_hash_2 = block_hash(20);
    let replacement_hash_3 = block_hash(30);
    let primary = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
    primary.commit_chain_epoch(initial_artifacts(
        old_hash_2,
        old_hash_3,
        transaction_id(2),
        transaction_id(3),
    ))?;
    let secondary = SecondaryChainStore::open(
        &primary_path,
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    let before_catchup = secondary.current_chain_epoch_reader()?;
    assert_eq!(before_catchup.displaced_root_archive_coverage()?, None);
    assert_eq!(
        before_catchup
            .block_header_at(BlockHeight::new(2))?
            .map(|block| block.block_hash),
        Some(old_hash_2)
    );

    primary.commit_chain_epoch(replacement_artifacts(
        replacement_hash_2,
        replacement_hash_3,
    ))?;
    secondary.try_catch_up()?;

    assert!(matches!(
        before_catchup.displaced_root_archive_coverage(),
        Err(StoreError::ChainEpochConflict { .. })
    ));
    assert!(matches!(
        before_catchup.displaced_root_candidates(
            ShieldedProtocol::Sapling,
            final_root(0x42),
            nonzero(8)?,
        ),
        Err(StoreError::ChainEpochConflict { .. })
    ));
    assert!(matches!(
        before_catchup.block_header_at(BlockHeight::new(2)),
        Err(StoreError::ChainEpochConflict { .. })
    ));

    let after_catchup = secondary.current_chain_epoch_reader()?;
    assert!(after_catchup.displaced_root_archive_coverage()?.is_some());
    assert_eq!(
        after_catchup
            .displaced_root_candidates(ShieldedProtocol::Sapling, final_root(0x42), nonzero(8)?,)?
            .len(),
        1
    );
    assert_eq!(
        after_catchup
            .block_header_at(BlockHeight::new(2))?
            .map(|block| block.block_hash),
        Some(replacement_hash_2)
    );
    Ok(())
}

#[test]
fn failed_replacement_writes_no_archive_rows_or_metadata() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let old_hash_2 = block_hash(2);
    let old_hash_3 = block_hash(3);
    store.commit_chain_epoch(initial_artifacts(
        old_hash_2,
        old_hash_3,
        transaction_id(2),
        transaction_id(3),
    ))?;

    let mut invalid = replacement_artifacts(block_hash(20), block_hash(30));
    invalid.block_headers.pop();
    invalid.compact_blocks.pop();
    let Err(error) = store.commit_chain_epoch(invalid) else {
        return Err(eyre!("incomplete replacement unexpectedly succeeded"));
    };
    assert!(matches!(
        error,
        StoreError::InvalidChainEpochArtifacts { .. }
    ));
    assert_eq!(store.displaced_block_count()?, 0);
    assert_eq!(store.displaced_block_archive_coverage()?, None);
    let failed_reader = store.current_chain_epoch_reader()?;
    assert_eq!(failed_reader.displaced_root_archive_coverage()?, None);
    assert_eq!(store.displaced_block_by_hash(old_hash_2)?, None);
    assert!(store.newest_displaced_blocks(nonzero(8)?)?.is_empty());
    assert!(
        failed_reader
            .displaced_root_candidates(ShieldedProtocol::Sapling, final_root(0x42), nonzero(8)?,)?
            .is_empty()
    );

    assert_eq!(
        failed_reader
            .block_header_at(BlockHeight::new(3))?
            .map(|block| block.block_hash),
        Some(old_hash_3)
    );

    let replacement_hash = block_hash(30);
    let replacement_epoch = chain_epoch(2, replacement_hash, 2_000);
    let replacement_header = block_header(3, replacement_hash, old_hash_2);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_header.clone()],
            vec![CompactBlockArtifact::new(
                replacement_header.height,
                replacement_header.block_hash,
                [0x03],
            )],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(3),
        }),
    )?;
    assert_eq!(store.displaced_block_count()?, 1);
    let current_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        current_reader.displaced_root_archive_coverage()?,
        Some(zinder_core::DisplacedRootArchiveCoverage {
            activation_event_sequence: 2,
            activation_epoch: ChainEpochId::new(2),
            activated_at: UnixTimestampMillis::new(2_000),
            captured_block_count: 1,
            root_artifact_unavailable_count: 1,
        })
    );
    assert_eq!(
        store
            .displaced_block_by_hash(old_hash_3)?
            .map(|block| block.header.height),
        Some(BlockHeight::new(3))
    );
    Ok(())
}

fn assert_historical_reader_rejects_displaced_roots(
    reader: &ChainEpochReader<'_>,
) -> eyre::Result<()> {
    assert!(matches!(
        reader.displaced_root_candidates(ShieldedProtocol::Sapling, final_root(0x42), nonzero(8)?,),
        Err(StoreError::Unsupported { .. })
    ));
    assert!(matches!(
        reader.displaced_root_archive_coverage(),
        Err(StoreError::Unsupported { .. })
    ));
    Ok(())
}

#[test]
fn repeated_displacement_counts_occurrences_and_pages_strictly_older() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let hash_a = block_hash(3);
    let hash_b = block_hash(30);
    let mut initial =
        initial_artifacts(block_hash(2), hash_a, transaction_id(2), transaction_id(3));
    initial
        .final_note_commitment_roots
        .push(final_roots(BlockHeight::new(3), hash_a, 0x71));
    store.commit_chain_epoch(initial)?;
    store.commit_chain_epoch(single_block_replacement(2, hash_b, 0x72))?;
    store.commit_chain_epoch(single_block_replacement(3, hash_a, 0x71))?;

    let canonical_reader = store.current_chain_epoch_reader()?;
    let currently_canonical = canonical_reader
        .block_header_at(BlockHeight::new(3))?
        .ok_or_else(|| eyre!("missing current canonical block"))?;
    let root_candidates = canonical_reader.displaced_root_candidates(
        ShieldedProtocol::Sapling,
        final_root(0x71),
        nonzero(8)?,
    )?;
    assert_eq!(root_candidates.len(), 1);
    assert_eq!(
        root_candidates[0].block_id,
        BlockId::new(currently_canonical.height, currently_canonical.block_hash)
    );
    drop(canonical_reader);

    store.commit_chain_epoch(single_block_replacement(4, hash_b, 0x72))?;

    assert_eq!(store.displaced_block_count()?, 3);
    let latest_a = store
        .displaced_block_by_hash(hash_a)?
        .ok_or_else(|| eyre!("missing repeatedly displaced block"))?;
    assert_eq!(latest_a.displacement_event_sequence, 4);

    let final_reader = store.current_chain_epoch_reader()?;
    let root_candidates = final_reader.displaced_root_candidates(
        ShieldedProtocol::Sapling,
        final_root(0x71),
        nonzero(8)?,
    )?;
    assert_eq!(
        root_candidates
            .iter()
            .map(|candidate| candidate.displacement_event_sequence)
            .collect::<Vec<_>>(),
        vec![4, 2]
    );
    assert!(
        root_candidates
            .iter()
            .all(|candidate| candidate.block_id == BlockId::new(BlockHeight::new(3), hash_a))
    );
    assert_eq!(
        final_reader.displaced_root_archive_coverage()?,
        Some(zinder_core::DisplacedRootArchiveCoverage {
            activation_event_sequence: 2,
            activation_epoch: ChainEpochId::new(2),
            activated_at: UnixTimestampMillis::new(2_000),
            captured_block_count: 3,
            root_artifact_unavailable_count: 0,
        })
    );

    let first_page = store.displaced_block_page(None, nonzero(2)?)?;
    assert_eq!(first_page.blocks.len(), 2);
    assert!(first_page.has_more);
    let cursor = first_page
        .next_cursor
        .ok_or_else(|| eyre!("missing next archive cursor"))?;
    assert_eq!(cursor.event_sequence(), 3);
    let second_page = store.displaced_block_page(Some(&cursor), nonzero(2)?)?;
    assert_eq!(second_page.blocks.len(), 1);
    assert_eq!(second_page.blocks[0].displacement_event_sequence, 2);
    assert!(!second_page.has_more);
    assert_eq!(second_page.next_cursor, None);
    Ok(())
}

fn initial_artifacts(
    old_hash_2: BlockHash,
    old_hash_3: BlockHash,
    coinbase_id_2: TransactionId,
    coinbase_id_3: TransactionId,
) -> ChainEpochArtifacts {
    let epoch = chain_epoch(1, old_hash_3, 1_000);
    let headers = vec![
        block_header(1, block_hash(1), block_hash(0)),
        block_header(2, old_hash_2, block_hash(1)),
        block_header(3, old_hash_3, old_hash_2),
    ];
    let compacts = headers
        .iter()
        .map(|header| CompactBlockArtifact::new(header.height, header.block_hash, [0x01]))
        .collect();
    let transaction_index = vec![
        BlockTransactionIndexArtifact::new(BlockHeight::new(2), 0, coinbase_id_2, old_hash_2),
        BlockTransactionIndexArtifact::new(BlockHeight::new(3), 0, coinbase_id_3, old_hash_3),
    ];
    let outpoint = TransparentOutPoint::new(coinbase_id_2, 0);
    let output = TransparentOutputArtifact::new(
        outpoint,
        625_000_000,
        [0x51],
        TransparentAddressScriptHash::of_script_pub_key(&[0x51]),
        BlockHeight::new(2),
        old_hash_2,
    );
    ChainEpochArtifacts::new(epoch, headers, compacts)
        .with_block_blobs(vec![BlockBlobArtifact::new(
            BlockHeight::new(2),
            old_hash_2,
            block_hash(1),
            b"raw-old-block-2".to_vec(),
        )])
        .with_block_transaction_index(transaction_index)
        .with_final_note_commitment_roots(vec![final_roots(BlockHeight::new(2), old_hash_2, 0x42)])
        .with_transparent_outputs_by_outpoint(vec![output])
}

fn assert_multi_block_archive(
    store: &PrimaryChainStore,
    event_sequence: u64,
    old_hashes: [BlockHash; 2],
    coinbase_ids: [TransactionId; 2],
) -> eyre::Result<()> {
    let [old_hash_2, old_hash_3] = old_hashes;
    let [coinbase_id_2, coinbase_id_3] = coinbase_ids;
    assert_eq!(store.displaced_block_count()?, 2);
    let newest = store.newest_displaced_blocks(nonzero(1)?)?;
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].block_hash, old_hash_3);
    assert_eq!(newest[0].raw_block_bytes, None);
    assert_eq!(newest[0].transaction_ids, vec![coinbase_id_3]);

    let displaced_2 = store
        .displaced_block_by_hash(old_hash_2)?
        .ok_or_else(|| eyre!("missing displaced block 2"))?;
    assert_eq!(displaced_2.header.height, BlockHeight::new(2));
    assert_eq!(
        displaced_2.raw_block_bytes,
        Some(b"raw-old-block-2".to_vec())
    );
    assert_eq!(displaced_2.transaction_ids, vec![coinbase_id_2]);
    assert_eq!(displaced_2.coinbase_outputs.len(), 1);
    assert_eq!(displaced_2.coinbase_outputs[0].output_index, 0);
    assert_eq!(displaced_2.coinbase_outputs[0].value_zat, 625_000_000);
    assert_eq!(displaced_2.coinbase_outputs[0].script_pub_key, [0x51]);
    assert_eq!(displaced_2.displacement_event_sequence, event_sequence);
    assert_eq!(
        displaced_2.final_note_commitment_roots,
        Some(final_roots(BlockHeight::new(2), old_hash_2, 0x42))
    );
    assert_eq!(
        store
            .displaced_block_by_hash(old_hash_3)?
            .and_then(|block| block.final_note_commitment_roots),
        None
    );

    let linked = store.displaced_blocks_for_event(event_sequence, nonzero(8)?)?;
    assert_eq!(
        linked
            .iter()
            .map(|block| block.header.height)
            .collect::<Vec<_>>(),
        vec![BlockHeight::new(3), BlockHeight::new(2)]
    );
    let coverage = store
        .displaced_block_archive_coverage()?
        .ok_or_else(|| eyre!("missing archive coverage"))?;
    assert_eq!(coverage.activation_event_sequence, event_sequence);
    assert_eq!(coverage.activation_epoch, ChainEpochId::new(2));
    Ok(())
}

fn assert_displaced_root_capture(
    reader: &ChainEpochReader<'_>,
    event_sequence: u64,
    old_hash_2: BlockHash,
) -> eyre::Result<()> {
    for (protocol, seed) in [
        (ShieldedProtocol::Sapling, 0x42),
        (ShieldedProtocol::Orchard, 0x43),
        (ShieldedProtocol::Ironwood, 0x44),
    ] {
        let candidates =
            reader.displaced_root_candidates(protocol, final_root(seed), nonzero(8)?)?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].block_id,
            BlockId::new(BlockHeight::new(2), old_hash_2)
        );
        assert_eq!(candidates[0].protocol, protocol);
        assert_eq!(candidates[0].root, final_root(seed));
        assert_eq!(candidates[0].displacement_event_sequence, event_sequence);
    }
    assert_eq!(
        reader.displaced_root_archive_coverage()?,
        Some(zinder_core::DisplacedRootArchiveCoverage {
            activation_event_sequence: event_sequence,
            activation_epoch: ChainEpochId::new(2),
            activated_at: UnixTimestampMillis::new(2_000),
            captured_block_count: 2,
            root_artifact_unavailable_count: 1,
        })
    );
    Ok(())
}

fn replacement_artifacts(hash_2: BlockHash, hash_3: BlockHash) -> ChainEpochArtifacts {
    let epoch = chain_epoch(2, hash_3, 2_000);
    let headers = vec![
        block_header(2, hash_2, block_hash(1)),
        block_header(3, hash_3, hash_2),
    ];
    let compacts = headers
        .iter()
        .map(|header| CompactBlockArtifact::new(header.height, header.block_hash, [0x02]))
        .collect();
    ChainEpochArtifacts::new(epoch, headers, compacts).with_reorg_window_change(
        ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        },
    )
}

fn single_block_replacement(
    epoch_id: u64,
    block_hash: BlockHash,
    root_seed: u8,
) -> ChainEpochArtifacts {
    let epoch = chain_epoch(epoch_id, block_hash, epoch_id.saturating_mul(1_000));
    let header = block_header(3, block_hash, self::block_hash(2));
    ChainEpochArtifacts::new(
        epoch,
        vec![header.clone()],
        vec![CompactBlockArtifact::new(
            header.height,
            header.block_hash,
            [0x04],
        )],
    )
    .with_final_note_commitment_roots(vec![final_roots(
        header.height,
        header.block_hash,
        root_seed,
    )])
    .with_reorg_window_change(ReorgWindowChange::Replace {
        from_height: BlockHeight::new(3),
    })
}

fn final_roots(
    height: BlockHeight,
    block_hash: BlockHash,
    sapling_seed: u8,
) -> BlockFinalNoteCommitmentRoots {
    BlockFinalNoteCommitmentRoots::new(
        height,
        block_hash,
        Some(final_root(sapling_seed)),
        Some(final_root(sapling_seed.saturating_add(1))),
        Some(final_root(sapling_seed.saturating_add(2))),
    )
}

fn final_root(seed: u8) -> FinalNoteCommitmentRoot {
    FinalNoteCommitmentRoot::from_bytes([seed; 32])
}

fn chain_epoch(id: u64, tip_hash: BlockHash, created_at: u64) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(3),
        visible_tip_hash: tip_hash,
        settled_tip_height: BlockHeight::new(1),
        settled_tip_hash: block_hash(1),
        artifact_schema_version: ArtifactSchemaVersion::new(12),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(created_at),
    }
}

fn block_header(height: u32, hash: BlockHash, parent_hash: BlockHash) -> BlockHeaderArtifact {
    super::synthetic_block_header(BlockHeight::new(height), hash, parent_hash, b"header")
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}

fn transaction_id(seed: u8) -> TransactionId {
    TransactionId::from_bytes([seed; 32])
}

fn nonzero(limit: u32) -> eyre::Result<NonZeroU32> {
    NonZeroU32::new(limit).ok_or_else(|| eyre!("test limit must be nonzero"))
}
