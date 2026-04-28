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
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, TransactionId, TransparentAddressScriptHash,
    TransparentAddressUtxoArtifact, TransparentOutPoint, UnixTimestampMillis,
};
use zinder_store::{ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange};

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
        reader_1.block_at(BlockHeight::new(1))?,
        Some(block_1.clone())
    );
    assert_eq!(reader_1.compact_block_at(BlockHeight::new(2))?, None);

    assert_eq!(reader_2.chain_epoch(), epoch_2);
    assert_eq!(reader_2.block_at(BlockHeight::new(1))?, Some(block_1));
    assert_eq!(reader_2.block_at(BlockHeight::new(2))?, Some(block_2));

    Ok(())
}

#[test]
fn chain_epoch_reader_stays_pinned_after_replacement_deletes_visibility() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (finalized_epoch, finalized_block, finalized_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.finalized_height = finalized_epoch.tip_height;
    initial_epoch.finalized_hash = finalized_epoch.tip_hash;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        initial_epoch,
        vec![finalized_block.clone(), initial_block],
        vec![finalized_compact_block, initial_compact_block.clone()],
    ))?;
    let pre_reorg_reader = store.current_chain_epoch_reader()?;

    let replacement_hash = BlockHash::from_bytes([42; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        tip_height: replacement_height,
        tip_hash: replacement_hash,
        finalized_height: finalized_epoch.tip_height,
        finalized_hash: finalized_epoch.tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    let replacement_block = BlockArtifact::new(
        replacement_height,
        replacement_hash,
        finalized_block.block_hash,
        b"replacement-block-2".to_vec(),
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
fn transparent_address_utxos_return_visible_remined_outpoint_after_reorg() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (finalized_epoch, finalized_block, finalized_compact_block) = synthetic_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_epoch(1, 2);
    initial_epoch.finalized_height = finalized_epoch.tip_height;
    initial_epoch.finalized_hash = finalized_epoch.tip_hash;

    let replacement_hash = BlockHash::from_bytes([42; 32]);
    let replacement_height = BlockHeight::new(2);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        tip_height: replacement_height,
        tip_hash: replacement_hash,
        finalized_height: finalized_epoch.tip_height,
        finalized_hash: finalized_epoch.tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = BlockArtifact::new(
        replacement_height,
        replacement_hash,
        finalized_block.block_hash,
        b"replacement-block-2".to_vec(),
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );

    let address_script_hash = TransparentAddressScriptHash::from_bytes([17; 32]);
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([23; 32]), 0);
    let stale_utxo = TransparentAddressUtxoArtifact::new(
        address_script_hash,
        b"stale-script".to_vec(),
        outpoint,
        50_000,
        replacement_height,
        initial_block.block_hash,
    );
    let visible_utxo = TransparentAddressUtxoArtifact::new(
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
            vec![finalized_block, initial_block],
            vec![finalized_compact_block, initial_compact_block],
        )
        .with_transparent_address_utxos(vec![stale_utxo]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_transparent_address_utxos(vec![visible_utxo.clone()])
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    let utxos = reader.transparent_address_utxos(
        address_script_hash,
        BlockHeight::new(1),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max entries"))?,
    )?;

    assert_eq!(utxos, vec![visible_utxo]);

    Ok(())
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
    assert_eq!(reader.block_at(BlockHeight::new(2))?, Some(expected_block));
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
            .block_at(BlockHeight::new(2))?
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
    assert_eq!(reader.block_at(BlockHeight::new(2))?, Some(expected_block));
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
    let below_lower_bound = match reader.block_at(BlockHeight::new(1)) {
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
    assert_eq!(reader.block_at(BlockHeight::new(2))?, Some(block));
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
    assert_eq!(reader.block_at(BlockHeight::new(1))?, first_block);

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
    initial_epoch.finalized_height = first_epoch.tip_height;
    initial_epoch.finalized_hash = first_epoch.tip_hash;
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
        tip_height: replacement_height,
        tip_hash: replacement_hash,
        finalized_height: first_epoch.tip_height,
        finalized_hash: first_epoch.tip_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_020),
    };
    let replacement_block = BlockArtifact::new(
        replacement_height,
        replacement_hash,
        first_block.block_hash,
        b"replacement-block-2".to_vec(),
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
