#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, TransparentUnspentOutput,
    UnixTimestampMillis,
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
    store.commit_chain_epoch(advance_safe_tip_artifacts(2, 5, 3, 3))?;

    let reader = store.current_chain_epoch_reader()?;
    let summary = reader.transparent_utxo_set_summary()?;

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
    let summary = reader.transparent_utxo_set_summary()?;

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
    let summary = reader.transparent_utxo_set_summary()?;

    assert_eq!(summary.utxo_count, 0);
    assert_eq!(summary.total_value_zat, 0);

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
    let pinned_summary = pinned.transparent_utxo_set_summary()?;
    assert_eq!(pinned_summary.summarized_height, BlockHeight::new(1));
    assert_eq!(pinned_summary.utxo_count, 1);
    assert_eq!(pinned_summary.total_value_zat, 50_000);
    drop(pinned);

    let current = store.current_chain_epoch_reader()?;
    let current_summary = current.transparent_utxo_set_summary()?;
    assert_eq!(current_summary.summarized_height, BlockHeight::new(3));
    assert_eq!(current_summary.utxo_count, 2);
    assert_eq!(current_summary.total_value_zat, 100_000);

    Ok(())
}

// Deterministic single-branch chain helpers: the block at height `h`
// hashes to `block_hash(h)`.

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
