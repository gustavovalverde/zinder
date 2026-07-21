#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;
use std::path::Path;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, TransparentUnspentOutput,
    TransparentUtxoSetCommitment, UnixTimestampMillis, UtxoSetCommitmentScheme,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore,
    ReorgWindowChange, SecondaryChainStore, StoreError,
};

const ADDRESS_SCRIPT_HASH: [u8; 32] = [71; 32];

#[test]
fn balance_snapshot_groups_all_visible_scripts_and_excludes_recent_spends() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let standard_script = [0x76, 0xa9, 0x14, 1, 2, 3, 0x88, 0xac];
    let nonstandard_script = [0x6a, 2, 0xca, 0xfe];
    let standard_hash = TransparentAddressScriptHash::of_script_pub_key(&standard_script);
    let nonstandard_hash = TransparentAddressScriptHash::of_script_pub_key(&nonstandard_script);
    let standard_first = output_with_script(
        BlockHeight::new(1),
        [1; 32],
        10,
        standard_script.to_vec(),
        standard_hash,
    );
    let standard_second = output_with_script(
        BlockHeight::new(2),
        [2; 32],
        20,
        standard_script.to_vec(),
        standard_hash,
    );
    let nonstandard = output_with_script(
        BlockHeight::new(3),
        [3; 32],
        7,
        nonstandard_script.to_vec(),
        nonstandard_hash,
    );
    let zero = output_with_script(
        BlockHeight::new(4),
        [4; 32],
        0,
        vec![0x51],
        TransparentAddressScriptHash::of_script_pub_key(&[0x51]),
    );
    let recently_spent = output_with_script(
        BlockHeight::new(1),
        [5; 32],
        100,
        standard_script.to_vec(),
        standard_hash,
    );
    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                standard_first,
                standard_second,
                nonstandard,
                zero,
                recently_spent.clone(),
            ])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(5), &recently_spent)]),
    )?;

    let snapshot = store
        .current_chain_epoch_reader()?
        .transparent_address_balance_snapshot()?;
    assert_eq!(snapshot.indexed_output_count, 5);
    assert_eq!(snapshot.utxo_count, 4);
    assert_eq!(snapshot.positive_script_hash_count, 2);
    assert_eq!(snapshot.total_positive_balance_zat, 37);
    assert_eq!(snapshot.summarized_height, BlockHeight::new(5));
    assert_eq!(snapshot.chain_epoch.visible_tip_height, BlockHeight::new(5));
    assert_eq!(
        snapshot.balances_by_script_hash.get(&standard_hash),
        Some(&zinder_store::TransparentAddressBalanceSummary {
            script_pub_key: standard_script.to_vec(),
            balance_zat: 30,
            utxo_count: 2,
        })
    );
    assert_eq!(
        snapshot.balances_by_script_hash.get(&nonstandard_hash),
        Some(&zinder_store::TransparentAddressBalanceSummary {
            script_pub_key: nonstandard_script.to_vec(),
            balance_zat: 7,
            utxo_count: 1,
        })
    );

    assert_settled_balance_snapshot(&store, standard_hash, &standard_script)?;

    Ok(())
}

fn assert_settled_balance_snapshot(
    store: &PrimaryChainStore,
    standard_hash: TransparentAddressScriptHash,
    standard_script: &[u8],
) -> eyre::Result<()> {
    let snapshot = store
        .current_chain_epoch_reader()?
        .settled_transparent_address_balance_snapshot()?;
    assert_eq!(snapshot.summarized_height, BlockHeight::new(1));
    assert_eq!(snapshot.utxo_count, 2);
    assert_eq!(snapshot.positive_script_hash_count, 1);
    assert_eq!(snapshot.total_positive_balance_zat, 110);
    assert_eq!(
        snapshot.balances_by_script_hash.get(&standard_hash),
        Some(&zinder_store::TransparentAddressBalanceSummary {
            script_pub_key: standard_script.to_vec(),
            balance_zat: 110,
            utxo_count: 2,
        })
    );
    Ok(())
}

#[test]
fn balance_snapshot_rejects_conflicting_scripts_for_one_hash() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let shared_hash = TransparentAddressScriptHash::from_bytes([8; 32]);
    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 2).with_transparent_outputs_by_outpoint(vec![
            output_with_script(
                BlockHeight::new(1),
                [8; 32],
                1,
                b"first-script".to_vec(),
                shared_hash,
            ),
            output_with_script(
                BlockHeight::new(2),
                [9; 32],
                1,
                b"second-script".to_vec(),
                shared_hash,
            ),
        ]),
    )?;

    let Err(error) = store
        .current_chain_epoch_reader()?
        .transparent_address_balance_snapshot()
    else {
        return Err(eyre!("conflicting scripts must fail closed"));
    };
    assert!(matches!(error, StoreError::ArtifactCorrupt { .. }));

    Ok(())
}

#[test]
fn balance_snapshot_rejects_balance_overflow() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let script = b"overflow-script".to_vec();
    let script_hash = TransparentAddressScriptHash::of_script_pub_key(&script);
    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 2).with_transparent_outputs_by_outpoint(vec![
            output_with_script(
                BlockHeight::new(1),
                [10; 32],
                u64::MAX,
                script.clone(),
                script_hash,
            ),
            output_with_script(BlockHeight::new(2), [11; 32], 1, script, script_hash),
        ]),
    )?;

    let Err(error) = store
        .current_chain_epoch_reader()?
        .transparent_address_balance_snapshot()
    else {
        return Err(eyre!("balance overflow must fail closed"));
    };
    assert!(matches!(error, StoreError::ArtifactCorrupt { .. }));

    Ok(())
}

#[test]
fn balance_snapshot_rejects_historical_reader() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    super::commit_synthetic_chain_epoch(&store, epoch_artifacts(1, 1, 1))?;
    let epoch_id = store.current_chain_epoch_reader()?.chain_epoch().id;

    let Err(error) = store
        .chain_epoch_reader_at(epoch_id)?
        .transparent_address_balance_snapshot()
    else {
        return Err(eyre!("historical projection reads must fail closed"));
    };
    assert!(matches!(error, StoreError::Unsupported { .. }));

    Ok(())
}

#[test]
fn spend_reverted_by_replace_resurfaces_address_row_without_new_writes() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let output = output_at(BlockHeight::new(2), [11; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 3).with_transparent_outputs_by_outpoint(vec![output.clone()]),
    )?;
    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(2, 4, 4)
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(4), &output)]),
    )?;

    let spent_reader = store.current_chain_epoch_reader()?;
    assert!(
        spent_reader
            .address_output_index(
                output.address_script_hash,
                BlockHeight::new(1),
                NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
            )?
            .is_empty()
    );
    drop(spent_reader);

    // Replace the spending block; the spend fact repair un-hides the
    // address row that was never deleted.
    super::commit_synthetic_chain_epoch(&store, replacement_artifacts(3, 4, [99; 32]))?;

    let reader = store.current_chain_epoch_reader()?;
    let outputs = reader.address_output_index(
        output.address_script_hash,
        BlockHeight::new(1),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
    )?;
    assert_eq!(outputs, vec![address_row(&output)]);

    Ok(())
}

#[test]
fn settled_tip_sweep_deletes_settled_spends_and_keeps_in_window_spends() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [21; 32]);
    let in_window_output = output_at(BlockHeight::new(1), [22; 32]);
    let unspent_output = output_at(BlockHeight::new(1), [23; 32]);
    let settled_spend = spend_at(BlockHeight::new(2), &settled_output);
    let in_window_spend = spend_at(BlockHeight::new(5), &in_window_output);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                settled_output.clone(),
                in_window_output.clone(),
                unspent_output.clone(),
            ])
            .with_transparent_spend_facts(vec![settled_spend.clone(), in_window_spend]),
    )?;
    // The durable spend projection has consumed through the tip, so the sweep
    // may release retention up to the settled tip.
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;

    // Canonical settled-tip advancement stays independent from historical
    // retention maintenance, so the settled rows remain until the worker
    // explicitly runs a bounded pass.
    let before_sweep = store.current_chain_epoch_reader()?;
    assert!(
        before_sweep
            .transparent_spend_facts_by_outpoints(&[settled_output.outpoint])?
            .contains_key(&settled_output.outpoint)
    );
    drop(before_sweep);
    store.sweep_transparent_retention_once()?;

    let reader = store.current_chain_epoch_reader()?;
    let outpoints = [
        settled_output.outpoint,
        in_window_output.outpoint,
        unspent_output.outpoint,
    ];

    // Spent at height 2 (at or below the new settled tip 3): physically gone
    // from all three projections.
    let outputs = reader.transparent_outputs_by_outpoints(&outpoints)?;
    assert!(!outputs.contains_key(&settled_output.outpoint));
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&settled_output.outpoint));
    assert_eq!(
        reader.current_transparent_spend_facts_at_height(BlockHeight::new(2))?,
        vec![settled_spend]
    );

    // Spent at height 5 (above the settled tip): retained for reorg repair.
    assert!(outputs.contains_key(&in_window_output.outpoint));
    assert!(spends.contains_key(&in_window_output.outpoint));

    // Unspent: retained and visible.
    assert!(outputs.contains_key(&unspent_output.outpoint));
    let visible = reader.address_output_index(
        unspent_output.address_script_hash,
        BlockHeight::new(1),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
    )?;
    assert_eq!(visible, vec![address_row(&unspent_output)]);

    Ok(())
}

#[test]
fn real_sweep_records_the_deleted_through_marker() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [26; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    // A sweep that deletes a fact records the ceiling as the deleted-through
    // height, so the ingest guard can tell it apart from a bare cursor advance.
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(3))
    );

    Ok(())
}

#[test]
fn sweep_without_deletions_leaves_the_deleted_through_marker_unset() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let unspent_output = output_at(BlockHeight::new(1), [27; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![unspent_output]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    // The swept cursor advances, but with nothing deleted the deleted-through
    // marker stays unset so an empty projection reads as no deletion.
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );
    assert_eq!(store.transparent_retention_deleted_through_height()?, None);

    Ok(())
}

#[test]
fn retention_release_floor_below_spend_height_keeps_the_spend_fact() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [81; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    // The durable projection has only consumed through height 1, below the
    // spend at height 2, so the sweep must not delete the spend fact even
    // though it settled below the settled tip.
    store.set_transparent_retention_release_height(BlockHeight::new(1))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[settled_output.outpoint])?;
    assert!(spends.contains_key(&settled_output.outpoint));
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(1))
    );

    Ok(())
}

#[test]
fn retention_release_floor_at_settle_height_sweeps_as_usual() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [82; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(3))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[settled_output.outpoint])?;
    assert!(!spends.contains_key(&settled_output.outpoint));
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );

    Ok(())
}

#[test]
fn retention_release_floor_regression_is_ignored_safely() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let early_spent = output_at(BlockHeight::new(1), [83; 32]);
    let later_spent = output_at(BlockHeight::new(6), [84; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![early_spent.clone(), later_spent.clone()])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &early_spent),
                spend_at(BlockHeight::new(7), &later_spent),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 10, 3, 3))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );

    // A release floor below the already-swept marker cannot un-sweep, and the
    // sweep must not regress the marker even though the settled tip advanced.
    store.set_transparent_retention_release_height(BlockHeight::new(1))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(3, 10, 8, 8))?;
    store.sweep_transparent_retention_once()?;

    let reader = store.current_chain_epoch_reader()?;
    let spends = reader
        .transparent_spend_facts_by_outpoints(&[early_spent.outpoint, later_spent.outpoint])?;
    assert!(!spends.contains_key(&early_spent.outpoint));
    assert!(spends.contains_key(&later_spent.outpoint));
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );

    Ok(())
}

#[test]
fn non_monotonic_advance_settled_tip_leaves_projections_untouched() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let in_window_output = output_at(BlockHeight::new(1), [31; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![in_window_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(5), &in_window_output)]),
    )?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(3, 5, 3, 2))?;

    let reader = store.current_chain_epoch_reader()?;
    let outputs = reader.transparent_outputs_by_outpoints(&[in_window_output.outpoint])?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[in_window_output.outpoint])?;
    assert!(outputs.contains_key(&in_window_output.outpoint));
    assert!(spends.contains_key(&in_window_output.outpoint));

    Ok(())
}

#[test]
fn secondary_reader_replays_settled_tip_sweep_deletes() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let secondary_dir = tempdir.path().join("secondary");
    let primary_path = tempdir.path().join("primary");
    let store = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [41; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    let secondary = SecondaryChainStore::open(
        &primary_path,
        &secondary_dir,
        ChainStoreOptions::for_local_tests(),
    )?;
    secondary.try_catch_up()?;
    let pre_sweep_reader = secondary.current_chain_epoch_reader()?;
    assert!(
        pre_sweep_reader
            .transparent_outputs_by_outpoints(&[settled_output.outpoint])?
            .contains_key(&settled_output.outpoint)
    );
    drop(pre_sweep_reader);

    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;
    secondary.try_catch_up()?;

    let reader = secondary.current_chain_epoch_reader()?;
    assert!(
        reader
            .transparent_outputs_by_outpoints(&[settled_output.outpoint])?
            .is_empty()
    );
    assert!(
        reader
            .transparent_spend_facts_by_outpoints(&[settled_output.outpoint])?
            .is_empty()
    );
    assert!(
        reader
            .address_output_index(
                settled_output.address_script_hash,
                BlockHeight::new(1),
                NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
            )?
            .is_empty()
    );

    Ok(())
}

#[test]
fn batched_read_matches_visible_unspent_set_and_respects_bounds() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let early_output = output_at(BlockHeight::new(1), [51; 32]);
    let spent_output = output_at(BlockHeight::new(2), [52; 32]);
    let mid_output = output_at(BlockHeight::new(3), [53; 32]);
    let late_output = output_at(BlockHeight::new(4), [54; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                early_output.clone(),
                spent_output.clone(),
                mid_output.clone(),
                late_output.clone(),
            ])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(5), &spent_output)]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    let max_entries = NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?;
    let all_visible = reader.address_output_index(
        early_output.address_script_hash,
        BlockHeight::new(1),
        max_entries,
    )?;
    assert_eq!(
        all_visible,
        vec![
            address_row(&early_output),
            address_row(&mid_output),
            address_row(&late_output),
        ]
    );

    let from_height_three = reader.address_output_index(
        early_output.address_script_hash,
        BlockHeight::new(3),
        max_entries,
    )?;
    assert_eq!(
        from_height_three,
        vec![address_row(&mid_output), address_row(&late_output)]
    );

    let bounded = reader.address_output_index(
        early_output.address_script_hash,
        BlockHeight::new(1),
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max entries"))?,
    )?;
    assert_eq!(
        bounded,
        vec![address_row(&early_output), address_row(&mid_output)]
    );

    Ok(())
}

#[test]
fn utxo_set_summary_counts_unspent_below_the_settled_tip_and_excludes_swept_spends()
-> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let unspent_low = output_at(BlockHeight::new(1), [61; 32]);
    let unspent_mid = output_at(BlockHeight::new(2), [62; 32]);
    let settled_spent = output_at(BlockHeight::new(1), [63; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                unspent_low,
                unspent_mid,
                settled_spent.clone(),
            ])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_spent)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    let reader = store.current_chain_epoch_reader()?;
    let summary = reader.transparent_utxo_set_summary(false)?;

    assert_eq!(summary.utxo_count, 2);
    assert_eq!(summary.total_value_zat, 100_000);
    assert_eq!(summary.summarized_height, BlockHeight::new(3));

    Ok(())
}

#[test]
fn utxo_set_summary_excludes_outputs_above_the_settled_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let settled_output = output_at(BlockHeight::new(1), [71; 32]);
    let in_window_output = output_at(BlockHeight::new(4), [72; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output, in_window_output]),
    )?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;

    let reader = store.current_chain_epoch_reader()?;
    let summary = reader.transparent_utxo_set_summary(false)?;

    assert_eq!(summary.utxo_count, 1);
    assert_eq!(summary.total_value_zat, 50_000);
    assert_eq!(summary.summarized_height, BlockHeight::new(3));

    Ok(())
}

#[test]
fn utxo_set_summary_is_zero_for_an_empty_projection() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    super::commit_synthetic_chain_epoch(&store, epoch_artifacts(1, 1, 5))?;

    let reader = store.current_chain_epoch_reader()?;
    let summary = reader.transparent_utxo_set_summary(false)?;

    assert_eq!(summary.utxo_count, 0);
    assert_eq!(summary.total_value_zat, 0);

    Ok(())
}

#[test]
fn utxo_set_summary_commitment_is_present_only_when_enabled() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let first = output_at(BlockHeight::new(1), [91; 32]);
    let second = output_at(BlockHeight::new(2), [92; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![first, second]),
    )?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;

    let reader = store.current_chain_epoch_reader()?;

    let disabled = reader.transparent_utxo_set_summary(false)?;
    assert_eq!(disabled.utxo_count, 2);
    assert!(disabled.commitment.is_none());

    let enabled = reader.transparent_utxo_set_summary(true)?;
    assert_eq!(enabled.utxo_count, 2);
    let commitment = enabled
        .commitment
        .ok_or_else(|| eyre!("commitment present when enabled"))?;
    assert_eq!(commitment.scheme(), UtxoSetCommitmentScheme::LtHash16);
    assert_ne!(commitment, TransparentUtxoSetCommitment::empty());

    Ok(())
}

#[test]
fn utxo_set_summary_commitment_is_empty_for_an_empty_projection() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    super::commit_synthetic_chain_epoch(&store, epoch_artifacts(1, 1, 5))?;

    let reader = store.current_chain_epoch_reader()?;
    let summary = reader.transparent_utxo_set_summary(true)?;

    assert_eq!(summary.utxo_count, 0);
    assert_eq!(
        summary.commitment,
        Some(TransparentUtxoSetCommitment::empty())
    );

    Ok(())
}

#[test]
fn utxo_set_summary_respects_the_pinned_epoch_settled_tip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let low_output = output_at(BlockHeight::new(1), [81; 32]);
    let mid_output = output_at(BlockHeight::new(2), [82; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![low_output, mid_output]),
    )?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;

    let pinned = store.chain_epoch_reader_at(ChainEpochId::new(1))?;
    let pinned_summary = pinned.transparent_utxo_set_summary(false)?;
    assert_eq!(pinned_summary.summarized_height, BlockHeight::new(1));
    assert_eq!(pinned_summary.utxo_count, 1);
    assert_eq!(pinned_summary.total_value_zat, 50_000);
    drop(pinned);

    let current = store.current_chain_epoch_reader()?;
    let current_summary = current.transparent_utxo_set_summary(false)?;
    assert_eq!(current_summary.summarized_height, BlockHeight::new(3));
    assert_eq!(current_summary.utxo_count, 2);
    assert_eq!(current_summary.total_value_zat, 100_000);

    Ok(())
}

#[test]
fn sweep_backlog_larger_than_cap_advances_one_cap_per_pass() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_sweep_cap(tempdir.path(), 3)?;
    let spent_low = output_at(BlockHeight::new(1), [101; 32]);
    let spent_mid = output_at(BlockHeight::new(1), [102; 32]);
    let spent_high = output_at(BlockHeight::new(1), [103; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![
                spent_low.clone(),
                spent_mid.clone(),
                spent_high.clone(),
            ])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &spent_low),
                spend_at(BlockHeight::new(5), &spent_mid),
                spend_at(BlockHeight::new(8), &spent_high),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;
    let outpoints = [spent_low.outpoint, spent_mid.outpoint, spent_high.outpoint];

    // Ceiling is 9, cap is 3: the first maintenance pass sweeps only heights 1..=3.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(3))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_low.outpoint));
    assert!(spends.contains_key(&spent_mid.outpoint));
    assert!(spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The next pass resumes from the persisted marker: heights 4..=6.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(3, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(6))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_mid.outpoint));
    assert!(spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The final pass drains the remaining backlog: heights 7..=9.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(4, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(9))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_high.outpoint));

    Ok(())
}

#[test]
fn sweep_backlog_within_cap_advances_to_the_full_ceiling() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_sweep_cap(tempdir.path(), 25_000)?;
    let settled_output = output_at(BlockHeight::new(1), [121; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;

    // The ceiling of 3 is well within the cap, so one pass sweeps it all.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(3))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[settled_output.outpoint])?;
    assert!(!spends.contains_key(&settled_output.outpoint));

    Ok(())
}

#[test]
fn capped_sweep_without_deletions_advances_marker_but_not_deleted_through() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_sweep_cap(tempdir.path(), 3)?;
    let later_spent = output_at(BlockHeight::new(1), [111; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![later_spent.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(7), &later_spent)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;

    // The first cap window 1..=3 holds no spend, so the swept marker advances by
    // the cap while the deleted-through marker stays unset.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );
    assert_eq!(store.transparent_retention_deleted_through_height()?, None);
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[later_spent.outpoint])?;
    assert!(spends.contains_key(&later_spent.outpoint));

    Ok(())
}

#[test]
fn sweep_outpoint_budget_stops_at_the_last_fully_swept_height() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_outpoint_budget(tempdir.path(), 2)?;
    let dense_first = output_at(BlockHeight::new(1), [131; 32]);
    let dense_second = output_at(BlockHeight::new(1), [132; 32]);
    let spent_mid = output_at(BlockHeight::new(1), [133; 32]);
    let spent_high = output_at(BlockHeight::new(1), [134; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![
                dense_first.clone(),
                dense_second.clone(),
                spent_mid.clone(),
                spent_high.clone(),
            ])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &dense_first),
                spend_at(BlockHeight::new(2), &dense_second),
                spend_at(BlockHeight::new(5), &spent_mid),
                spend_at(BlockHeight::new(8), &spent_high),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;
    let outpoints = [
        dense_first.outpoint,
        dense_second.outpoint,
        spent_mid.outpoint,
        spent_high.outpoint,
    ];

    // Height 2 alone meets the budget of 2: the marker lands on 2, the last
    // fully-swept height, not on the height-cap ceiling.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(2))
    );
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(2))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&dense_first.outpoint));
    assert!(!spends.contains_key(&dense_second.outpoint));
    assert!(spends.contains_key(&spent_mid.outpoint));
    assert!(spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The next pass resumes from the marker, sweeps height 5, and stops
    // again once height 8 meets the budget.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(3, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(8))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_mid.outpoint));
    assert!(!spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The final pass drains the empty remainder up to the ceiling without
    // touching the deleted-through marker.
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(4, 10, 9, 9))?;
    store.sweep_transparent_retention_once()?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(9))
    );
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(8))
    );

    Ok(())
}

#[test]
fn sweep_never_splits_a_height_denser_than_the_outpoint_budget() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_outpoint_budget(tempdir.path(), 1)?;
    let dense_first = output_at(BlockHeight::new(1), [141; 32]);
    let dense_second = output_at(BlockHeight::new(1), [142; 32]);

    super::commit_synthetic_chain_epoch(
        &store,
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![dense_first.clone(), dense_second.clone()])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &dense_first),
                spend_at(BlockHeight::new(2), &dense_second),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    super::commit_synthetic_chain_epoch(&store, advance_settled_tip_artifacts(2, 5, 3, 3))?;
    store.sweep_transparent_retention_once()?;

    // Height 2 carries more outpoints than the budget of 1, but the marker
    // may only name fully-swept heights, so both spends go in one pass.
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(2))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader
        .transparent_spend_facts_by_outpoints(&[dense_first.outpoint, dense_second.outpoint])?;
    assert!(spends.is_empty());

    Ok(())
}

// Deterministic single-branch chain helpers: the block at height `h`
// hashes to `block_hash(h)`.

fn store_with_sweep_cap(path: &Path, cap: u32) -> eyre::Result<PrimaryChainStore> {
    Ok(PrimaryChainStore::open(
        path,
        ChainStoreOptions {
            retention_sweep_max_heights_per_pass: cap,
            ..ChainStoreOptions::for_local_tests()
        },
    )?)
}

fn store_with_outpoint_budget(path: &Path, budget: u64) -> eyre::Result<PrimaryChainStore> {
    Ok(PrimaryChainStore::open(
        path,
        ChainStoreOptions {
            retention_sweep_max_outpoints_per_pass: budget,
            ..ChainStoreOptions::for_local_tests()
        },
    )?)
}

fn output_at(height: BlockHeight, txid_seed: [u8; 32]) -> TransparentOutputArtifact {
    TransparentOutputArtifact::new(
        TransparentOutPoint::new(TransactionId::from_bytes(txid_seed), 0),
        50_000,
        b"projection-script".to_vec(),
        TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH),
        height,
        block_hash(height.value()),
    )
}

fn output_with_script(
    height: BlockHeight,
    txid_seed: [u8; 32],
    value_zat: u64,
    script_pub_key: Vec<u8>,
    address_script_hash: TransparentAddressScriptHash,
) -> TransparentOutputArtifact {
    TransparentOutputArtifact::new(
        TransparentOutPoint::new(TransactionId::from_bytes(txid_seed), 0),
        value_zat,
        script_pub_key,
        address_script_hash,
        height,
        block_hash(height.value()),
    )
}

fn spend_at(height: BlockHeight, output: &TransparentOutputArtifact) -> TransparentSpendFact {
    let mut spending_txid = output.outpoint.transaction_id.as_bytes();
    spending_txid[0] ^= 0xff;
    TransparentSpendFact::from_input_and_output(
        output.outpoint,
        0,
        TransactionId::from_bytes(spending_txid),
        0,
        height,
        block_hash(height.value()),
        output,
    )
}

fn epoch_artifacts(epoch_id: u64, from: u32, to: u32) -> ChainEpochArtifacts {
    let blocks = (from..=to).map(synthetic_block).collect::<Vec<_>>();
    let compact_blocks = (from..=to).map(synthetic_compact_block).collect();
    super::synthetic_chain_epoch_artifacts(chain_epoch(epoch_id, to), blocks, compact_blocks)
}

fn advance_settled_tip_artifacts(
    epoch_id: u64,
    tip: u32,
    epoch_settled_tip: u32,
    target: u32,
) -> ChainEpochArtifacts {
    let mut chain_epoch = chain_epoch(epoch_id, tip);
    chain_epoch.settled_tip_height = BlockHeight::new(epoch_settled_tip);
    chain_epoch.settled_tip_hash = block_hash(epoch_settled_tip);
    super::synthetic_chain_epoch_artifacts(chain_epoch, Vec::new(), Vec::new())
        .with_reorg_window_change(ReorgWindowChange::AdvanceSettledTipTo {
            height: BlockHeight::new(target),
        })
}

fn replacement_artifacts(
    epoch_id: u64,
    replaced_height: u32,
    replacement_seed: [u8; 32],
) -> ChainEpochArtifacts {
    let height = BlockHeight::new(replaced_height);
    let replacement_hash = BlockHash::from_bytes(replacement_seed);
    let block = super::synthetic_block_header(
        height,
        replacement_hash,
        block_hash(replaced_height.saturating_sub(1)),
        format!("replacement-block-{replaced_height}").as_bytes(),
    );
    let compact_block = super::empty_compact_block_for_header(&block, ChainTipMetadata::empty());
    let mut chain_epoch = chain_epoch(epoch_id, replaced_height);
    chain_epoch.visible_tip_hash = replacement_hash;
    super::synthetic_chain_epoch_artifacts(chain_epoch, vec![block], vec![compact_block])
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: height,
        })
}

fn chain_epoch(epoch_id: u64, tip: u32) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(epoch_id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(tip),
        visible_tip_hash: block_hash(tip),
        settled_tip_height: BlockHeight::new(1),
        settled_tip_hash: block_hash(1),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_200_000 + epoch_id),
    }
}

fn address_row(output: &TransparentOutputArtifact) -> TransparentUnspentOutput {
    TransparentUnspentOutput::new(
        output.address_script_hash,
        output.script_pub_key.clone(),
        output.outpoint,
        output.value_zat,
        output.block_height,
        output.block_hash,
    )
}

fn synthetic_block(height: u32) -> BlockHeaderArtifact {
    super::synthetic_block_header(
        BlockHeight::new(height),
        block_hash(height),
        block_hash(height.saturating_sub(1)),
        format!("projection-block-{height}").as_bytes(),
    )
}

fn synthetic_compact_block(height: u32) -> CompactBlockArtifact {
    super::empty_compact_block_for_header(&synthetic_block(height), ChainTipMetadata::empty())
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
