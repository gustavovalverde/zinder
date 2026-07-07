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
    ReorgWindowChange, SecondaryChainStore,
};

const ADDRESS_SCRIPT_HASH: [u8; 32] = [71; 32];

#[test]
fn spend_reverted_by_replace_resurfaces_address_row_without_new_writes() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let output = output_at(BlockHeight::new(2), [11; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 3).with_transparent_outputs_by_outpoint(vec![output.clone()]),
    )?;
    store.commit_chain_epoch(
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
    store.commit_chain_epoch(replacement_artifacts(3, 4, [99; 32]))?;

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
fn safe_tip_sweep_deletes_finalized_spends_and_keeps_in_window_spends() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let finalized_output = output_at(BlockHeight::new(1), [21; 32]);
    let in_window_output = output_at(BlockHeight::new(1), [22; 32]);
    let unspent_output = output_at(BlockHeight::new(1), [23; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                finalized_output.clone(),
                in_window_output.clone(),
                unspent_output.clone(),
            ])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &finalized_output),
                spend_at(BlockHeight::new(5), &in_window_output),
            ]),
    )?;
    // The durable spend projection has consumed through the tip, so the sweep
    // may release retention up to the settled tip.
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

    let reader = store.current_chain_epoch_reader()?;
    let outpoints = [
        finalized_output.outpoint,
        in_window_output.outpoint,
        unspent_output.outpoint,
    ];

    // Spent at height 2 (at or below the new safe tip 3): physically gone
    // from all three projections.
    let outputs = reader.transparent_outputs_by_outpoints(&outpoints)?;
    assert!(!outputs.contains_key(&finalized_output.outpoint));
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&finalized_output.outpoint));

    // Spent at height 5 (above the safe tip): retained for reorg repair.
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
    let finalized_output = output_at(BlockHeight::new(1), [26; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![finalized_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &finalized_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![unspent_output]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    // The durable projection has only consumed through height 1, below the
    // spend at height 2, so the sweep must not delete the spend fact even
    // though it settled below the safe tip.
    store.set_transparent_retention_release_height(BlockHeight::new(1))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &settled_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(3))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![early_spent.clone(), later_spent.clone()])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &early_spent),
                spend_at(BlockHeight::new(7), &later_spent),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 10, 3, 3))?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );

    // A release floor below the already-swept marker cannot un-sweep, and the
    // sweep must not regress the marker even though the settled tip advanced.
    store.set_transparent_retention_release_height(BlockHeight::new(1))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(3, 10, 8, 8))?;

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
fn non_monotonic_advance_safe_tip_leaves_projections_untouched() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let in_window_output = output_at(BlockHeight::new(1), [31; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![in_window_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(5), &in_window_output)]),
    )?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(3, 5, 3, 2))?;

    let reader = store.current_chain_epoch_reader()?;
    let outputs = reader.transparent_outputs_by_outpoints(&[in_window_output.outpoint])?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[in_window_output.outpoint])?;
    assert!(outputs.contains_key(&in_window_output.outpoint));
    assert!(spends.contains_key(&in_window_output.outpoint));

    Ok(())
}

#[test]
fn secondary_reader_replays_safe_tip_sweep_deletes() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let secondary_dir = tempdir.path().join("secondary");
    let primary_path = tempdir.path().join("primary");
    let store = PrimaryChainStore::open(&primary_path, ChainStoreOptions::for_local_tests())?;
    let finalized_output = output_at(BlockHeight::new(1), [41; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![finalized_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &finalized_output)]),
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
            .transparent_outputs_by_outpoints(&[finalized_output.outpoint])?
            .contains_key(&finalized_output.outpoint)
    );
    drop(pre_sweep_reader);

    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;
    secondary.try_catch_up()?;

    let reader = secondary.current_chain_epoch_reader()?;
    assert!(
        reader
            .transparent_outputs_by_outpoints(&[finalized_output.outpoint])?
            .is_empty()
    );
    assert!(
        reader
            .transparent_spend_facts_by_outpoints(&[finalized_output.outpoint])?
            .is_empty()
    );
    assert!(
        reader
            .address_output_index(
                finalized_output.address_script_hash,
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

    store.commit_chain_epoch(
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
    let finalized_spent = output_at(BlockHeight::new(1), [63; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![
                unspent_low,
                unspent_mid,
                finalized_spent.clone(),
            ])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &finalized_spent)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![settled_output, in_window_output]),
    )?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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
    store.commit_chain_epoch(epoch_artifacts(1, 1, 5))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![first, second]),
    )?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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
    store.commit_chain_epoch(epoch_artifacts(1, 1, 5))?;

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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5).with_transparent_outputs_by_outpoint(vec![low_output, mid_output]),
    )?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

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
fn sweep_backlog_larger_than_cap_advances_one_cap_per_commit() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_sweep_cap(tempdir.path(), 3)?;
    let spent_low = output_at(BlockHeight::new(1), [101; 32]);
    let spent_mid = output_at(BlockHeight::new(1), [102; 32]);
    let spent_high = output_at(BlockHeight::new(1), [103; 32]);

    store.commit_chain_epoch(
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

    // Ceiling is 9, cap is 3: the first commit sweeps only heights 1..=3.
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 10, 9, 9))?;
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

    // The next commit resumes from the persisted marker: heights 4..=6.
    store.commit_chain_epoch(advance_safe_tip_artifacts(3, 10, 9, 9))?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(6))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_mid.outpoint));
    assert!(spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The final commit drains the remaining backlog: heights 7..=9.
    store.commit_chain_epoch(advance_safe_tip_artifacts(4, 10, 9, 9))?;
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
    let finalized_output = output_at(BlockHeight::new(1), [121; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![finalized_output.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(2), &finalized_output)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;

    // The ceiling of 3 is well within the cap, so one commit sweeps it all.
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(3))
    );
    assert_eq!(
        store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(3))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&[finalized_output.outpoint])?;
    assert!(!spends.contains_key(&finalized_output.outpoint));

    Ok(())
}

#[test]
fn capped_sweep_without_deletions_advances_marker_but_not_deleted_through() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = store_with_sweep_cap(tempdir.path(), 3)?;
    let later_spent = output_at(BlockHeight::new(1), [111; 32]);

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 10)
            .with_transparent_outputs_by_outpoint(vec![later_spent.clone()])
            .with_transparent_spend_facts(vec![spend_at(BlockHeight::new(7), &later_spent)]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(10))?;

    // The first cap window 1..=3 holds no spend, so the swept marker advances by
    // the cap while the deleted-through marker stays unset.
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 10, 9, 9))?;
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

    store.commit_chain_epoch(
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
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 10, 9, 9))?;
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

    // The next commit resumes from the marker, sweeps height 5, and stops
    // again once height 8 meets the budget.
    store.commit_chain_epoch(advance_safe_tip_artifacts(3, 10, 9, 9))?;
    assert_eq!(
        store.transparent_retention_swept_height()?,
        Some(BlockHeight::new(8))
    );
    let reader = store.current_chain_epoch_reader()?;
    let spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
    assert!(!spends.contains_key(&spent_mid.outpoint));
    assert!(!spends.contains_key(&spent_high.outpoint));
    drop(reader);

    // The final commit drains the empty remainder up to the ceiling without
    // touching the deleted-through marker.
    store.commit_chain_epoch(advance_safe_tip_artifacts(4, 10, 9, 9))?;
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

    store.commit_chain_epoch(
        epoch_artifacts(1, 1, 5)
            .with_transparent_outputs_by_outpoint(vec![dense_first.clone(), dense_second.clone()])
            .with_transparent_spend_facts(vec![
                spend_at(BlockHeight::new(2), &dense_first),
                spend_at(BlockHeight::new(2), &dense_second),
            ]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

    // Height 2 carries more outpoints than the budget of 1, but the marker
    // may only name fully-swept heights, so both spends go in one commit.
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
            retention_sweep_max_heights_per_commit: cap,
            ..ChainStoreOptions::for_local_tests()
        },
    )?)
}

fn store_with_outpoint_budget(path: &Path, budget: u64) -> eyre::Result<PrimaryChainStore> {
    Ok(PrimaryChainStore::open(
        path,
        ChainStoreOptions {
            retention_sweep_max_outpoints_per_commit: budget,
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
    ChainEpochArtifacts::new(chain_epoch(epoch_id, to), blocks, compact_blocks)
}

fn advance_safe_tip_artifacts(
    epoch_id: u64,
    tip: u32,
    epoch_safe_tip: u32,
    target: u32,
) -> ChainEpochArtifacts {
    let mut chain_epoch = chain_epoch(epoch_id, tip);
    chain_epoch.settled_tip_height = BlockHeight::new(epoch_safe_tip);
    chain_epoch.settled_tip_hash = block_hash(epoch_safe_tip);
    ChainEpochArtifacts::new(chain_epoch, Vec::new(), Vec::new()).with_reorg_window_change(
        ReorgWindowChange::AdvanceSafeTipTo {
            height: BlockHeight::new(target),
        },
    )
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
    let compact_block = CompactBlockArtifact::new(
        height,
        replacement_hash,
        format!("replacement-compact-{replaced_height}").into_bytes(),
    );
    let mut chain_epoch = chain_epoch(epoch_id, replaced_height);
    chain_epoch.visible_tip_hash = replacement_hash;
    ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
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
    CompactBlockArtifact::new(
        BlockHeight::new(height),
        block_hash(height),
        format!("projection-compact-{height}").into_bytes(),
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
