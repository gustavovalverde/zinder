#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::path::Path;

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, UnixTimestampMillis,
};
use zinder_ingest::{IngestError, ensure_spend_projection_not_behind_retention_sweep};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore,
    ReorgWindowChange,
};
use zinder_testkit::seed_transparent_outpoint_spends;

fn bundled_derive_store(storage_path: &Path) -> Result<zinder_derive::DeriveStore> {
    Ok(zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumers: zinder_derive::DeriveStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?)
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}

fn block_header(height: u32) -> BlockHeaderArtifact {
    BlockHeaderArtifact::new(
        BlockHeight::new(height),
        block_hash(height),
        block_hash(height.saturating_sub(1)),
        [0; 32],
        [0; 32],
        0,
        0,
        [0; 32],
        0,
        32,
    )
}

fn compact_block(height: u32) -> CompactBlockArtifact {
    CompactBlockArtifact::new(
        BlockHeight::new(height),
        block_hash(height),
        format!("guard-compact-{height}").into_bytes(),
    )
}

fn chain_epoch(epoch_id: u64, visible_tip: u32, settled_tip: u32) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(epoch_id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(visible_tip),
        visible_tip_hash: block_hash(visible_tip),
        settled_tip_height: BlockHeight::new(settled_tip),
        settled_tip_hash: block_hash(settled_tip),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_400_000 + epoch_id),
    }
}

fn spent_outpoint() -> TransparentOutPoint {
    TransparentOutPoint::new(TransactionId::from_bytes([0x71; 32]), 0)
}

fn spend_fact_at(height: BlockHeight) -> TransparentSpendFact {
    TransparentSpendFact::new(
        spent_outpoint(),
        1,
        TransactionId::from_bytes([0x72; 32]),
        0,
        height,
        block_hash(height.value()),
        1_000,
        TransparentAddressScriptHash::from_bytes([0x74; 32]),
        BlockHeight::new(1),
        block_hash(1),
    )
}

fn bootstrap_swept_marker(store: &PrimaryChainStore, settled_tip: u32) -> Result<()> {
    let height = BlockHeight::new(settled_tip);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            chain_epoch(1, settled_tip, settled_tip),
            Vec::new(),
            Vec::new(),
        )
        .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo { height }),
    )?;
    Ok(())
}

/// Commits a settled spend and advances the safe tip so the retention sweep
/// deletes it, returning the height the deleted-through marker records.
fn commit_real_sweep(store: &PrimaryChainStore) -> Result<BlockHeight> {
    let output = TransparentOutputArtifact::new(
        spent_outpoint(),
        1_000,
        b"guard-script".to_vec(),
        TransparentAddressScriptHash::from_bytes([0x74; 32]),
        BlockHeight::new(1),
        block_hash(1),
    );
    let blocks = (1..=5).map(block_header).collect::<Vec<_>>();
    let compact_blocks = (1..=5).map(compact_block).collect::<Vec<_>>();
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch(1, 5, 1), blocks, compact_blocks)
            .with_transparent_outputs_by_outpoint(vec![output])
            .with_transparent_spend_facts(vec![spend_fact_at(BlockHeight::new(2))]),
    )?;
    store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch(2, 5, 3), Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo {
                height: BlockHeight::new(3),
            }),
    )?;
    Ok(BlockHeight::new(3))
}

#[test]
fn fresh_stores_pass_the_spend_projection_guard() -> Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let derive_store = bundled_derive_store(tempdir.path())?;

    ensure_spend_projection_not_behind_retention_sweep(&store, &derive_store)?;
    Ok(())
}

#[test]
fn projection_behind_a_real_sweep_is_refused() -> Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let derive_store = bundled_derive_store(tempdir.path())?;

    let deleted_through = commit_real_sweep(&store)?;

    let outcome = ensure_spend_projection_not_behind_retention_sweep(&store, &derive_store);
    match outcome {
        Err(IngestError::SpendProjectionBehindRetentionSweep {
            projection_height,
            deleted_through: refused_at,
        }) => {
            assert_eq!(projection_height, 0);
            assert_eq!(refused_at, deleted_through.value());
        }
        other => return Err(eyre!("expected a spend-projection refusal, got {other:?}")),
    }
    Ok(())
}

#[test]
fn projection_caught_up_to_the_sweep_passes() -> Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let derive_store = bundled_derive_store(tempdir.path())?;

    let deleted_through = commit_real_sweep(&store)?;
    seed_transparent_outpoint_spends(&derive_store, &[spend_fact_at(deleted_through)])?;

    ensure_spend_projection_not_behind_retention_sweep(&store, &derive_store)?;
    Ok(())
}

#[test]
fn bootstrap_swept_marker_without_deletion_does_not_refuse() -> Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let derive_store = bundled_derive_store(tempdir.path())?;

    // Checkpoint bootstrap advances the swept cursor without deleting facts and
    // without recording a deleted-through marker. The guard must not read that
    // cursor as a real deletion the empty projection cannot cover.
    bootstrap_swept_marker(&store, 5)?;

    ensure_spend_projection_not_behind_retention_sweep(&store, &derive_store)?;
    Ok(())
}
