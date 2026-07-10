#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    env, io,
    num::NonZeroU32,
    path::Path,
    process::{Command, Stdio},
};

use eyre::eyre;
use tempfile::{TempDir, tempdir};
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId,
    ChainEpoch, ChainEpochId, ChainTipMetadata, CompactBlockArtifact, Network, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentOutputArtifact,
    TransparentSpendFact, TransparentUnspentOutput, UnixTimestampMillis,
};
use zinder_store::{
    BlockHashLookup, ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

const CRASH_CHILD_ENV: &str = "ZINDER_STORE_CRASH_CHILD";
const CRASH_STORE_PATH_ENV: &str = "ZINDER_STORE_CRASH_PATH";
const CRASH_MODE_ENV: &str = "ZINDER_STORE_CRASH_MODE";

#[test]
fn chain_epoch_reader_stays_pinned_after_a_new_epoch_is_committed() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_block_1) = synthetic_epoch(1, 1);
    let (epoch_2, block_2, compact_block_2) = synthetic_epoch(2, 2);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_1,
        vec![block_1.clone()],
        vec![compact_block_1],
    ))?;
    let reader_1 = store.current_chain_epoch_reader()?;

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_2,
        vec![block_2.clone()],
        vec![compact_block_2],
    ))?;

    let reader_2 = store.current_chain_epoch_reader()?;

    assert_eq!(reader_1.chain_epoch(), epoch_1);
    assert_eq!(
        reader_1.block_header_at(BlockHeight::new(1))?,
        Some(block_1.clone())
    );
    assert_eq!(reader_1.compact_block_at(BlockHeight::new(2))?, None);

    assert_eq!(reader_2.chain_epoch(), epoch_2);
    assert_eq!(
        reader_2.block_header_at(BlockHeight::new(1))?,
        Some(block_1)
    );
    assert_eq!(
        reader_2.block_header_at(BlockHeight::new(2))?,
        Some(block_2)
    );
    assert_eq!(
        reader_2.block_headers_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2)
        ))?,
        vec![
            reader_2.block_header_at(BlockHeight::new(1))?,
            reader_2.block_header_at(BlockHeight::new(2))?
        ]
    );

    Ok(())
}

#[test]
fn chain_epoch_reader_stays_pinned_after_replacement_deletes_visibility() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (safe_tip_epoch, safe_tip_block, safe_tip_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.settled_tip_height = safe_tip_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = safe_tip_epoch.visible_tip_hash;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        initial_epoch,
        vec![safe_tip_block.clone(), initial_block],
        vec![safe_tip_compact_block, initial_compact_block.clone()],
    ))?;
    let pre_reorg_reader = store.current_chain_epoch_reader()?;

    let replacement_hash = BlockHash::from_bytes([42; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        safe_tip_block.block_hash,
        b"replacement-block-2",
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block.clone()],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;
    let post_reorg_reader = store.current_chain_epoch_reader()?;

    assert_eq!(
        pre_reorg_reader.compact_block_at(replacement_height)?,
        Some(initial_compact_block)
    );
    assert_eq!(
        post_reorg_reader.compact_block_at(replacement_height)?,
        Some(replacement_compact_block)
    );

    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario builds three sequential chain epochs to exercise hash reintroduction; splitting it into helpers obscures the reorg sequence"
)]
fn block_hash_lookup_for_historical_epoch_survives_hash_reintroduction() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (safe_tip_epoch, safe_tip_block, safe_tip_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.settled_tip_height = safe_tip_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = safe_tip_epoch.visible_tip_hash;
    let reintroduced_hash = initial_block.block_hash;
    let replacement_height = BlockHeight::new(2);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        initial_epoch,
        vec![safe_tip_block.clone(), initial_block],
        vec![safe_tip_compact_block, initial_compact_block],
    ))?;

    let replacement_hash = BlockHash::from_bytes([42; 32]);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        safe_tip_block.block_hash,
        b"replacement-block-2",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    let reintroduced_epoch = ChainEpoch {
        id: ChainEpochId::new(3),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: reintroduced_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_030),
    };
    let reintroduced_block = super::synthetic_block_header(
        replacement_height,
        reintroduced_hash,
        safe_tip_block.block_hash,
        b"reintroduced-block-2",
    );
    let reintroduced_compact_block = CompactBlockArtifact::new(
        replacement_height,
        reintroduced_hash,
        b"reintroduced-compact-block-2".to_vec(),
    );
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            reintroduced_epoch,
            vec![reintroduced_block],
            vec![reintroduced_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    let historical_reader = store.chain_epoch_reader_at(initial_epoch.id)?;
    assert_eq!(
        historical_reader.block_hash_lookup(reintroduced_hash)?,
        BlockHashLookup::Resolved(BlockId::new(replacement_height, reintroduced_hash))
    );

    let current_reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        current_reader.block_hash_lookup(replacement_hash)?,
        BlockHashLookup::NotInBestChain
    );

    Ok(())
}

#[test]
fn address_output_index_return_visible_remined_outpoint_after_reorg() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (safe_tip_epoch, safe_tip_block, safe_tip_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.settled_tip_height = safe_tip_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = safe_tip_epoch.visible_tip_hash;

    let replacement_hash = BlockHash::from_bytes([42; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        safe_tip_block.block_hash,
        b"replacement-block-2",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );

    let address_script_hash = TransparentAddressScriptHash::from_bytes([17; 32]);
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([23; 32]), 0);
    let stale_utxo = TransparentUnspentOutput::new(
        address_script_hash,
        b"stale-script".to_vec(),
        outpoint,
        50_000,
        replacement_height,
        initial_block.block_hash,
    );
    let visible_utxo = TransparentUnspentOutput::new(
        address_script_hash,
        b"visible-script".to_vec(),
        outpoint,
        75_000,
        replacement_height,
        replacement_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            vec![safe_tip_block, initial_block],
            vec![safe_tip_compact_block, initial_compact_block],
        )
        .with_transparent_outputs_by_outpoint(vec![transparent_output_from_utxo(&stale_utxo)]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_transparent_outputs_by_outpoint(vec![transparent_output_from_utxo(&visible_utxo)])
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    let utxos = reader.address_output_index(
        address_script_hash,
        BlockHeight::new(1),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
    )?;

    assert_eq!(utxos, vec![visible_utxo]);

    Ok(())
}

#[test]
fn transparent_outputs_by_outpoint_return_latest_visible_row_after_reorg() -> eyre::Result<()> {
    let fixture = reorged_outpoint_fixture()?;
    let reader = fixture.store.current_chain_epoch_reader()?;
    let stale_epoch_reader = fixture.store.chain_epoch_reader_at(ChainEpochId::new(2))?;
    let rows = fixture.rows;

    assert_eq!(
        reader
            .transparent_outputs_by_outpoints(&[rows.outpoint])?
            .get(&rows.outpoint),
        Some(&rows.visible_prevout)
    );
    assert_eq!(
        reader
            .transparent_outputs_by_outpoints_for_writer_commit(&[rows.outpoint])?
            .get(&rows.outpoint),
        Some(&rows.visible_prevout)
    );
    assert_eq!(
        stale_epoch_reader
            .transparent_outputs_by_outpoints(&[rows.outpoint])?
            .get(&rows.outpoint),
        Some(&rows.visible_prevout)
    );
    assert!(
        stale_epoch_reader
            .transparent_outputs_by_outpoints(&[rows.stale_prevout.outpoint])?
            .is_empty()
    );

    Ok(())
}

#[test]
fn transparent_outputs_by_outpoint_remove_reorged_row_without_visible_history() -> eyre::Result<()>
{
    let fixture = reorged_only_outpoint_fixture()?;
    let current_reader = fixture.store.current_chain_epoch_reader()?;
    let stale_epoch_reader = fixture.store.chain_epoch_reader_at(ChainEpochId::new(2))?;

    assert_eq!(
        current_reader
            .transparent_outputs_by_outpoints(&[fixture.outpoint])?
            .get(&fixture.outpoint),
        None
    );
    assert_eq!(
        current_reader
            .transparent_outputs_by_outpoints_for_writer_commit(&[fixture.outpoint])?
            .get(&fixture.outpoint),
        None
    );
    assert_eq!(
        stale_epoch_reader
            .transparent_outputs_by_outpoints(&[fixture.outpoint])?
            .get(&fixture.outpoint),
        None
    );

    Ok(())
}

#[test]
fn transparent_spend_facts_by_outpoint_read_current_rows() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_block_1) = synthetic_epoch(1, 1);
    let (epoch_2, block_2, compact_block_2) = synthetic_epoch(2, 2);
    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([31; 32]), 0);
    let output = TransparentUnspentOutput::new(
        TransparentAddressScriptHash::from_bytes([32; 32]),
        b"spendable-script".to_vec(),
        spent_outpoint,
        77,
        block_1.height,
        block_1.block_hash,
    );
    let spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        TransactionId::from_bytes([33; 32]),
        0,
        block_2.height,
        block_2.block_hash,
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_block_1])
            .with_transparent_outputs_by_outpoint(vec![transparent_output_from_utxo(&output)]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_block_2])
            .with_transparent_spend_facts(vec![spend.clone()]),
    )?;

    let current_reader = store.current_chain_epoch_reader()?;
    let epoch_1_reader = store.chain_epoch_reader_at(ChainEpochId::new(1))?;
    assert_eq!(
        current_reader
            .transparent_spend_facts_by_outpoints(&[spent_outpoint])?
            .get(&spent_outpoint),
        Some(&spend)
    );
    assert!(
        epoch_1_reader
            .transparent_spend_facts_by_outpoints(&[spent_outpoint])?
            .is_empty()
    );
    assert!(
        current_reader
            .address_output_index(
                output.address_script_hash,
                BlockHeight::new(1),
                NonZeroU32::MIN
            )?
            .is_empty()
    );

    Ok(())
}

#[test]
fn current_transparent_spend_facts_match_visible_for_finalized_outpoints() -> eyre::Result<()> {
    // A finalized spend (block_2 == safe_tip): the visibility-skipping
    // current path the derive replay uses must equal the visible path, since
    // an immutable block can never be filtered out. Guards the fast-path
    // optimization in `read_transparent_spend_facts_for_committed_blocks`.
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_block_1) = synthetic_epoch(1, 1);
    let (epoch_2, block_2, compact_block_2) = synthetic_epoch(2, 2);
    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([51; 32]), 0);
    let output = TransparentUnspentOutput::new(
        TransparentAddressScriptHash::from_bytes([52; 32]),
        b"spendable-script".to_vec(),
        spent_outpoint,
        77,
        block_1.height,
        block_1.block_hash,
    );
    let spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        TransactionId::from_bytes([53; 32]),
        0,
        block_2.height,
        block_2.block_hash,
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_block_1])
            .with_transparent_outputs_by_outpoint(vec![transparent_output_from_utxo(&output)]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_2, vec![block_2], vec![compact_block_2])
            .with_transparent_spend_facts(vec![spend.clone()]),
    )?;

    let epoch_2_reader = store.chain_epoch_reader_at(ChainEpochId::new(2))?;
    let visible = epoch_2_reader.transparent_spend_facts_by_outpoints(&[spent_outpoint])?;
    let current = epoch_2_reader.current_transparent_spend_facts_by_outpoints(&[spent_outpoint])?;

    assert_eq!(visible.get(&spent_outpoint), Some(&spend));
    assert_eq!(current, visible);

    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the reorg fixture keeps the committed epochs visible inside the assertion"
)]
fn transparent_spend_facts_by_outpoint_remove_reorged_spend() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (epoch_1, block_1, compact_block_1) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(2, 2);
    initial_epoch.settled_tip_height = epoch_1.visible_tip_height;
    initial_epoch.settled_tip_hash = epoch_1.visible_tip_hash;
    let replacement_height = BlockHeight::new(2);
    let replacement_hash = BlockHash::from_bytes([45; 32]);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(3),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: epoch_1.visible_tip_height,
        settled_tip_hash: epoch_1.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_030),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        block_1.block_hash,
        b"replacement-spend-block",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement".to_vec(),
    );
    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([41; 32]), 0);
    let output = TransparentUnspentOutput::new(
        TransparentAddressScriptHash::from_bytes([42; 32]),
        b"reorg-script".to_vec(),
        spent_outpoint,
        99,
        block_1.height,
        block_1.block_hash,
    );
    let reorged_spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        TransactionId::from_bytes([43; 32]),
        0,
        initial_block.height,
        initial_block.block_hash,
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_1, vec![block_1], vec![compact_block_1])
            .with_transparent_outputs_by_outpoint(vec![transparent_output_from_utxo(&output)]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            vec![initial_block],
            vec![initial_compact_block],
        )
        .with_transparent_spend_facts(vec![reorged_spend]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    let current_reader = store.current_chain_epoch_reader()?;
    let stale_reader = store.chain_epoch_reader_at(ChainEpochId::new(2))?;
    assert!(
        current_reader
            .transparent_spend_facts_by_outpoints(&[spent_outpoint])?
            .is_empty()
    );
    assert_eq!(
        stale_reader
            .transparent_spend_facts_by_outpoints(&[spent_outpoint])?
            .get(&spent_outpoint),
        None
    );
    assert_eq!(
        current_reader.address_output_index(
            output.address_script_hash,
            BlockHeight::new(1),
            NonZeroU32::MIN,
        )?,
        vec![output]
    );

    Ok(())
}

struct ReorgedOnlyOutpointFixture {
    store: PrimaryChainStore,
    _tempdir: TempDir,
    outpoint: TransparentOutPoint,
}

fn reorged_only_outpoint_fixture() -> eyre::Result<ReorgedOnlyOutpointFixture> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (safe_tip_epoch, safe_tip_block, safe_tip_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(2, 2);
    initial_epoch.settled_tip_height = safe_tip_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = safe_tip_epoch.visible_tip_hash;

    let replacement_height = BlockHeight::new(2);
    let replacement_hash = BlockHash::from_bytes([44; 32]);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(3),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        safe_tip_block.block_hash,
        b"replacement-block-2",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([25; 32]), 0);
    let stale_utxo = TransparentUnspentOutput::new(
        TransparentAddressScriptHash::from_bytes([19; 32]),
        b"stale-script".to_vec(),
        outpoint,
        50_000,
        replacement_height,
        initial_block.block_hash,
    );
    let stale_prevout = transparent_output_from_utxo(&stale_utxo);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        safe_tip_epoch,
        vec![safe_tip_block],
        vec![safe_tip_compact_block],
    ))?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            vec![initial_block],
            vec![initial_compact_block],
        )
        .with_transparent_outputs_by_outpoint(vec![stale_prevout]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    Ok(ReorgedOnlyOutpointFixture {
        store,
        _tempdir: tempdir,
        outpoint,
    })
}

struct ReorgedOutpointFixture {
    store: PrimaryChainStore,
    _tempdir: TempDir,
    rows: ReorgedOutpointRows,
}

struct ReorgedOutpointRows {
    outpoint: TransparentOutPoint,
    visible_prevout: TransparentOutputArtifact,
    stale_prevout: TransparentOutputArtifact,
}

fn reorged_outpoint_fixture() -> eyre::Result<ReorgedOutpointFixture> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (safe_tip_epoch, safe_tip_block, safe_tip_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(2, 2);
    initial_epoch.settled_tip_height = safe_tip_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = safe_tip_epoch.visible_tip_hash;

    let replacement_hash = BlockHash::from_bytes([43; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(3),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: safe_tip_epoch.visible_tip_height,
        settled_tip_hash: safe_tip_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        safe_tip_block.block_hash,
        b"replacement-block-2",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    let rows = reorged_outpoint_rows(safe_tip_epoch, &initial_block, replacement_height);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            safe_tip_epoch,
            vec![safe_tip_block],
            vec![safe_tip_compact_block],
        )
        .with_transparent_outputs_by_outpoint(vec![rows.visible_prevout.clone()]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            vec![initial_block],
            vec![initial_compact_block],
        )
        .with_transparent_outputs_by_outpoint(vec![rows.stale_prevout.clone()]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    Ok(ReorgedOutpointFixture {
        store,
        _tempdir: tempdir,
        rows,
    })
}

fn reorged_outpoint_rows(
    safe_tip_epoch: ChainEpoch,
    initial_block: &BlockHeaderArtifact,
    replacement_height: BlockHeight,
) -> ReorgedOutpointRows {
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([24; 32]), 0);
    let stale_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([25; 32]), 0);
    let base_address = TransparentAddressScriptHash::from_bytes([18; 32]);
    let stale_address = TransparentAddressScriptHash::from_bytes([19; 32]);
    let visible_prevout = TransparentOutputArtifact::new(
        outpoint,
        75_000,
        b"visible-script".to_vec(),
        base_address,
        safe_tip_epoch.visible_tip_height,
        safe_tip_epoch.visible_tip_hash,
    );
    let stale_prevout = TransparentOutputArtifact::new(
        stale_outpoint,
        50_000,
        b"stale-script".to_vec(),
        stale_address,
        replacement_height,
        initial_block.block_hash,
    );

    ReorgedOutpointRows {
        outpoint,
        visible_prevout,
        stale_prevout,
    }
}

fn transparent_output_from_utxo(utxo: &TransparentUnspentOutput) -> TransparentOutputArtifact {
    TransparentOutputArtifact::new(
        utxo.outpoint,
        utxo.value_zat,
        utxo.script_pub_key.clone(),
        utxo.address_script_hash,
        utxo.block_height,
        utxo.block_hash,
    )
}

#[test]
fn reopening_store_recovers_the_last_visible_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);

    {
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        store.commit_chain_epoch(ChainEpochArtifacts::new(
            chain_epoch,
            vec![block],
            vec![compact_block.clone()],
        ))?;
    }

    let reopened = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let reader = reopened.current_chain_epoch_reader()?;

    assert_eq!(reader.chain_epoch(), chain_epoch);
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(1))?,
        Some(compact_block)
    );

    Ok(())
}

#[test]
fn process_crash_after_append_commit_recovers_complete_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;

    run_crash_child(tempdir.path(), "append")?;

    let reopened = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let reader = reopened.current_chain_epoch_reader()?;
    let (expected_epoch, expected_block, expected_compact_block) = synthetic_epoch(2, 2);

    assert_eq!(reader.chain_epoch(), expected_epoch);
    assert_eq!(
        reader.block_header_at(BlockHeight::new(2))?,
        Some(expected_block)
    );
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(2))?,
        Some(expected_compact_block)
    );

    Ok(())
}

#[test]
fn process_crash_after_reorg_commit_recovers_complete_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;

    run_crash_child(tempdir.path(), "reorg")?;

    let reopened = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let reader = reopened.current_chain_epoch_reader()?;
    let replacement_hash = BlockHash::from_bytes([20; 32]);

    assert_eq!(reader.chain_epoch().id, ChainEpochId::new(2));
    assert_eq!(
        reader
            .block_header_at(BlockHeight::new(2))?
            .map(|block| block.block_hash),
        Some(replacement_hash)
    );

    Ok(())
}

#[test]
fn process_crash_with_sync_writes_recovers_complete_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;

    run_crash_child(tempdir.path(), "append-sync")?;

    let reopened = PrimaryChainStore::open(
        tempdir.path(),
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let reader = reopened.current_chain_epoch_reader()?;
    let (expected_epoch, expected_block, expected_compact_block) = synthetic_epoch(2, 2);

    assert_eq!(reader.chain_epoch(), expected_epoch);
    assert_eq!(
        reader.block_header_at(BlockHeight::new(2))?,
        Some(expected_block)
    );
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(2))?,
        Some(expected_compact_block)
    );

    Ok(())
}

#[test]
fn initial_commit_uses_artifact_set_as_lower_bound() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 2);

    let outcome = store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block.clone()],
        vec![compact_block.clone()],
    ))?;
    let reader = store.current_chain_epoch_reader()?;
    let below_lower_bound = match reader.block_header_at(BlockHeight::new(1)) {
        Ok(block) => {
            return Err(eyre!(
                "expected missing block below lower bound, got {block:?}"
            ));
        }
        Err(error) => error,
    };

    assert_eq!(outcome.chain_epoch, chain_epoch);
    assert!(matches!(
        below_lower_bound,
        zinder_store::StoreError::ArtifactMissing { .. }
    ));
    assert_eq!(reader.block_header_at(BlockHeight::new(2))?, Some(block));
    assert_eq!(
        reader.compact_block_at(BlockHeight::new(2))?,
        Some(compact_block)
    );

    Ok(())
}

#[test]
fn many_epoch_reads_do_not_depend_on_dense_artifact_rewrites() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut first_block = None;

    for height in 1..=200 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        if height == 1 {
            first_block = Some(block.clone());
        }

        store.commit_chain_epoch(ChainEpochArtifacts::new(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
    }

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(reader.chain_epoch().id, ChainEpochId::new(200));
    assert_eq!(reader.block_header_at(BlockHeight::new(1))?, first_block);

    Ok(())
}

#[test]
fn crash_recovery_child_process() -> eyre::Result<()> {
    if env::var_os(CRASH_CHILD_ENV).is_none() {
        return Ok(());
    }

    let store_path = env::var_os(CRASH_STORE_PATH_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "crash child store path is missing",
        )
    })?;
    let crash_mode = env::var(CRASH_MODE_ENV)?;
    let store = if crash_mode.ends_with("-sync") {
        PrimaryChainStore::open(
            Path::new(&store_path),
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?
    } else {
        PrimaryChainStore::open(Path::new(&store_path), ChainStoreOptions::for_local_tests())?
    };

    match crash_mode.as_str() {
        "append" | "append-sync" => commit_append_crash_fixture(&store)?,
        "reorg" => commit_reorg_crash_fixture(&store)?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown crash test mode: {crash_mode}"),
            )
            .into());
        }
    }

    std::process::abort();
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
            artifact_schema_version: ArtifactSchemaVersion::new(13),
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

fn run_crash_child(store_path: &Path, crash_mode: &str) -> eyre::Result<()> {
    let status = Command::new(env::current_exe()?)
        .arg("--exact")
        .arg("integration::chain_epoch_reader::crash_recovery_child_process")
        .arg("--nocapture")
        .env(CRASH_CHILD_ENV, "1")
        .env(CRASH_STORE_PATH_ENV, store_path.as_os_str())
        .env(CRASH_MODE_ENV, crash_mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    assert!(!status.success());

    Ok(())
}

fn commit_append_crash_fixture(store: &PrimaryChainStore) -> eyre::Result<()> {
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    Ok(())
}

fn commit_reorg_crash_fixture(store: &PrimaryChainStore) -> eyre::Result<()> {
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.settled_tip_height = first_epoch.visible_tip_height;
    initial_epoch.settled_tip_hash = first_epoch.visible_tip_hash;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        initial_epoch,
        vec![first_block.clone(), initial_block],
        vec![first_compact_block, initial_compact_block],
    ))?;

    let replacement_hash = BlockHash::from_bytes([20; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        visible_tip_height: replacement_height,
        visible_tip_hash: replacement_hash,
        settled_tip_height: first_epoch.visible_tip_height,
        settled_tip_hash: first_epoch.visible_tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = super::synthetic_block_header(
        replacement_height,
        replacement_hash,
        first_block.block_hash,
        b"replacement-block-2",
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    Ok(())
}
