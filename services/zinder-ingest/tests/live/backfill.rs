#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{io, num::NonZeroU32};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, BlockHeightRange, Network, SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange,
};
use zinder_ingest::{BackfillOutcome, backfill};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_store::{ChainStoreOptions, PrimaryChainStore, StoreError};
use zinder_testkit::live::{init, require_live, require_live_for};

use crate::common::{
    assert_lightwalletd_send_transaction_classifies_invalid, assert_native_wallet_read_responses,
    fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill,
};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn backfills_initial_range() -> Result<()> {
    let _guard = init();
    let env = require_live()?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let to_height = match env.network() {
        Network::ZcashRegtest => BlockHeight::new(1),
        Network::ZcashTestnet | Network::ZcashMainnet => BlockHeight::new(2),
        other => return Err(eyre!("unsupported network for backfill test: {other:?}")),
    };
    let backfill_config = live_backfill_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        to_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let BackfillOutcome::Committed(commit_outcome) = backfill(&backfill_config, &source).await?
    else {
        return Err(eyre!("expected committed backfill outcome"));
    };

    assert_eq!(commit_outcome.chain_epoch.network, env.network());
    assert_eq!(commit_outcome.chain_epoch.tip_height, to_height);

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let reader = store.current_chain_epoch_reader()?;
    let block = reader
        .block_at(to_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip block artifact"))?;
    let compact_block = reader
        .compact_block_at(to_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip compact block artifact"))?;
    let tree_state = reader
        .tree_state_at(to_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip tree-state artifact"))?;

    assert_eq!(block.height, to_height);
    assert_eq!(compact_block.height, to_height);
    assert_eq!(tree_state.height, to_height);
    assert!(!tree_state.payload_bytes.is_empty());
    assert_native_wallet_read_responses(&store, env.network(), 1, to_height.value()).await?;
    assert_lightwalletd_send_transaction_classifies_invalid(&store, &backfill_config).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn backfills_from_checkpoint() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashRegtest])?;

    let tip_height = fetch_live_tip_height(&env).await?;
    if tip_height.value() < 60 {
        return Err(eyre!(
            "checkpoint backfill test needs tip >= 60; got {}; advance the chain (e.g. via the regtest `generate` RPC)",
            tip_height.value()
        ));
    }

    let checkpoint_height = BlockHeight::new(tip_height.value() - 50);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        &env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(25).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);

    let outcome = backfill(&backfill_config, &source).await?;
    let chain_epoch = outcome.chain_epoch();
    assert_eq!(chain_epoch.network, env.network());
    assert_eq!(chain_epoch.tip_height, tip_height);
    assert_eq!(chain_epoch.finalized_height, tip_height);

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let reader = store.current_chain_epoch_reader()?;

    // Bootstrap commit carried no artifacts, only the chain-epoch envelope, so
    // reads at and below the checkpoint surface ArtifactMissing instead of
    // succeeding with stale data or returning Ok(None).
    assert!(matches!(
        reader.block_at(checkpoint_height),
        Err(StoreError::ArtifactMissing { .. })
    ));
    assert!(matches!(
        reader.compact_block_at(checkpoint_height),
        Err(StoreError::ArtifactMissing { .. })
    ));
    let below_checkpoint = BlockHeight::new(checkpoint_height.value() - 1);
    assert!(matches!(
        reader.block_at(below_checkpoint),
        Err(StoreError::ArtifactMissing { .. })
    ));

    let first_filled = reader
        .block_at(from_height)?
        .ok_or_else(|| eyre!("missing first backfilled block at {}", from_height.value()))?;
    assert_eq!(first_filled.height, from_height);
    let tip_block = reader
        .block_at(tip_height)?
        .ok_or_else(|| eyre!("missing tip block at {}", tip_height.value()))?;
    assert_eq!(tip_block.height, tip_height);

    assert_native_wallet_read_responses(
        &store,
        env.network(),
        from_height.value(),
        tip_height.value(),
    )
    .await?;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "Calibration test reads ten endpoints; per-endpoint timing variables follow the endpoint name."
)]
#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn backfills_last_1000_blocks_from_checkpoint() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashMainnet])?;

    let tip_height = fetch_live_tip_height(&env).await?;
    if tip_height.value() < 1100 {
        return Err(eyre!(
            "mainnet calibration needs tip >= 1100; got {}",
            tip_height.value()
        ));
    }

    let checkpoint_height = BlockHeight::new(tip_height.value() - 1000);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        &env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    let checkpoint_sapling_size = checkpoint.tip_metadata.sapling_commitment_tree_size;
    let checkpoint_orchard_size = checkpoint.tip_metadata.orchard_commitment_tree_size;
    backfill_config.checkpoint = Some(checkpoint);

    let backfill_started_at = std::time::Instant::now();
    let outcome = backfill(&backfill_config, &source).await?;
    let backfill_seconds = backfill_started_at.elapsed().as_secs();
    let chain_epoch = outcome.chain_epoch();
    assert_eq!(chain_epoch.network, env.network());
    assert_eq!(chain_epoch.tip_height, tip_height);

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    {
        let reader = store.current_chain_epoch_reader()?;
        assert!(matches!(
            reader.block_at(checkpoint_height),
            Err(StoreError::ArtifactMissing { .. })
        ));
    }

    let wallet_query = WalletQuery::new(store, ());

    let measurement_start = std::time::Instant::now();
    let _latest = wallet_query.latest_block().await?;
    let latest_block_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _block = wallet_query.compact_block_at(from_height).await?;
    let compact_block_at_first_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _block = wallet_query.compact_block_at(tip_height).await?;
    let compact_block_at_tip_micros = measurement_start.elapsed().as_micros();

    let one_block_range = BlockHeightRange::inclusive(from_height, from_height);
    let measurement_start = std::time::Instant::now();
    let one_block = wallet_query.compact_block_range(one_block_range).await?;
    let compact_block_range_1_micros = measurement_start.elapsed().as_micros();
    assert_eq!(one_block.compact_blocks.len(), 1);

    let ten_block_range =
        BlockHeightRange::inclusive(from_height, BlockHeight::new(from_height.value() + 9));
    let measurement_start = std::time::Instant::now();
    let ten_blocks = wallet_query.compact_block_range(ten_block_range).await?;
    let compact_block_range_10_micros = measurement_start.elapsed().as_micros();
    assert_eq!(ten_blocks.compact_blocks.len(), 10);

    let fifty_block_range =
        BlockHeightRange::inclusive(from_height, BlockHeight::new(from_height.value() + 49));
    let measurement_start = std::time::Instant::now();
    let fifty_blocks = wallet_query.compact_block_range(fifty_block_range).await?;
    let compact_block_range_50_micros = measurement_start.elapsed().as_micros();
    assert_eq!(fifty_blocks.compact_blocks.len(), 50);

    let full_range = BlockHeightRange::inclusive(from_height, tip_height);
    let measurement_start = std::time::Instant::now();
    let full_blocks = wallet_query.compact_block_range(full_range).await?;
    let compact_block_range_1000_micros = measurement_start.elapsed().as_micros();
    assert_eq!(full_blocks.compact_blocks.len(), 1000);

    let measurement_start = std::time::Instant::now();
    let _tree_state = wallet_query.tree_state_at(tip_height).await?;
    let tree_state_at_micros = measurement_start.elapsed().as_micros();

    // A checkpoint-bootstrapped store only carries subtree roots completed
    // after the checkpoint. Wallets bootstrapping against the same checkpoint
    // ask for indexes >= the checkpoint's completed-subtree count; anything
    // before is the operator's responsibility to seed out-of-band.
    let checkpoint_completed_sapling_subtrees = checkpoint_sapling_size / SUBTREE_LEAF_COUNT;
    let subtree_root_range = SubtreeRootRange::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(checkpoint_completed_sapling_subtrees),
        NonZeroU32::new(8).ok_or_else(|| eyre!("invalid max entries"))?,
    );
    let measurement_start = std::time::Instant::now();
    let _subtree_roots = wallet_query.subtree_roots(subtree_root_range).await?;
    let subtree_roots_micros = measurement_start.elapsed().as_micros();

    #[allow(
        clippy::print_stderr,
        reason = "calibration test reports measurements for operator review"
    )]
    {
        eprintln!(
            "live_mainnet_calibration tip={} checkpoint_height={} \
             checkpoint_sapling_size={} checkpoint_orchard_size={} \
             backfill_seconds={} latest_block={}us compact_block_at_first={}us \
             compact_block_at_tip={}us compact_block_range_1={}us \
             compact_block_range_10={}us compact_block_range_50={}us \
             compact_block_range_1000={}us tree_state_at={}us subtree_roots_8={}us",
            tip_height.value(),
            checkpoint_height.value(),
            checkpoint_sapling_size,
            checkpoint_orchard_size,
            backfill_seconds,
            latest_block_micros,
            compact_block_at_first_micros,
            compact_block_at_tip_micros,
            compact_block_range_1_micros,
            compact_block_range_10_micros,
            compact_block_range_50_micros,
            compact_block_range_1000_micros,
            tree_state_at_micros,
            subtree_roots_micros,
        );
    }
    Ok(())
}
