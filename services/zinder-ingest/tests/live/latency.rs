#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::Network;
use zinder_core::{BlockHeight, BlockHeightRange};
use zinder_ingest::{BackfillOutcome, backfill};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live_for};

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

const TRANSACTION_LOOKUP_ITERATIONS: u32 = 100;
const HOSTED_BACKFILL_DEPTH_BLOCKS: u32 = 150;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "single calibration test reports five read-path measurements end-to-end against a real node; splitting per-measurement obscures the operator-facing trace line at the bottom"
)]
async fn read_endpoint_latency_baseline() -> Result<()> {
    let _guard = init();
    // The baseline backfills [1, tip], which only fits in CI budgets on
    // regtest. The hosted-network calibration is pending the
    // checkpoint-bounded backfill path (BackfillConfig::checkpoint).
    let env = require_live_for(&[Network::ZcashRegtest])?;

    let tip_height = fetch_live_tip_height(&env).await?;
    if tip_height.value() < 50 {
        return Err(eyre!(
            "latency baseline test needs at least 50 blocks; got tip {}",
            tip_height.value()
        ));
    }

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let backfill_config = live_backfill_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        tip_height,
        NonZeroU32::new(50).ok_or_else(|| eyre!("invalid batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let _outcome = backfill(&backfill_config, &source).await?;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let wallet_query = WalletQuery::new(store, ());

    let measurement_start = std::time::Instant::now();
    let _latest = wallet_query.latest_block(None).await?;
    let latest_block_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _block = wallet_query
        .compact_block_at(BlockHeight::new(1), None)
        .await?;
    let compact_block_at_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let range = wallet_query
        .compact_block_range(
            BlockHeightRange::inclusive(
                BlockHeight::new(1),
                BlockHeight::new(tip_height.value().min(50)),
            ),
            None,
        )
        .await?;
    let compact_block_range_50_micros = measurement_start.elapsed().as_micros();
    assert!(!range.compact_blocks.is_empty());

    let measurement_start = std::time::Instant::now();
    let _tree = wallet_query
        .tree_state_at(BlockHeight::new(1), None)
        .await?;
    let tree_state_at_micros = measurement_start.elapsed().as_micros();

    // Pick a real txid from the backfilled chain by reading the coinbase at
    // height 1 through the indexed compact block. Then call `transaction()`
    // repeatedly so the average covers the artifact-lookup + header-parse
    // + consensus-branch-id path that powers `MinedDetails` enrichment.
    // Header parsing only succeeds against real Zcash block bytes, so this
    // measurement only fires on live nodes (the synthetic test fixtures
    // produce non-parseable raw block bytes).
    let coinbase_lookup = wallet_query
        .transaction_at_block_index(BlockHeight::new(1), 0, None)
        .await?;
    let coinbase_txid = coinbase_lookup.transaction.transaction_id;

    let measurement_start = std::time::Instant::now();
    for _ in 0..TRANSACTION_LOOKUP_ITERATIONS {
        let _status = wallet_query.transaction(coinbase_txid, None).await?;
    }
    let transaction_total_micros = measurement_start.elapsed().as_micros();
    let transaction_avg_micros =
        transaction_total_micros / u128::from(TRANSACTION_LOOKUP_ITERATIONS);

    #[allow(
        clippy::print_stderr,
        reason = "calibration test reports measurements for operator review"
    )]
    {
        eprintln!(
            "live_latency_baseline network={} tip={} latest_block={}us compact_block_at={}us \
             compact_block_range_50={}us tree_state_at={}us transaction_avg={}us \
             (n={} total={}us)",
            env.network().name(),
            tip_height.value(),
            latest_block_micros,
            compact_block_at_micros,
            compact_block_range_50_micros,
            tree_state_at_micros,
            transaction_avg_micros,
            TRANSACTION_LOOKUP_ITERATIONS,
            transaction_total_micros,
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "hosted-network calibration keeps checkpoint bootstrap, timing, and trace emission in one auditable test"
)]
async fn checkpoint_bounded_read_endpoint_latency_baseline() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashTestnet, Network::ZcashMainnet])?;

    let tip_height = fetch_live_tip_height(&env).await?;
    if tip_height.value() <= HOSTED_BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "checkpoint latency baseline needs tip > {HOSTED_BACKFILL_DEPTH_BLOCKS}; got {}",
            tip_height.value()
        ));
    }

    let checkpoint_height = BlockHeight::new(tip_height.value() - HOSTED_BACKFILL_DEPTH_BLOCKS);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        &env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(HOSTED_BACKFILL_DEPTH_BLOCKS)
            .ok_or_else(|| eyre!("invalid hosted batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);

    let backfill_started_at = std::time::Instant::now();
    let BackfillOutcome::Committed(commit_outcome) = backfill(&backfill_config, &source).await?
    else {
        return Err(eyre!(
            "expected committed checkpoint-bounded backfill outcome"
        ));
    };
    let backfill_micros = backfill_started_at.elapsed().as_micros();
    assert_eq!(commit_outcome.chain_epoch.network, env.network());
    assert_eq!(commit_outcome.chain_epoch.tip_height, tip_height);

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let wallet_query = WalletQuery::new(store, ());

    let measurement_start = std::time::Instant::now();
    let _latest = wallet_query.latest_block(None).await?;
    let latest_block_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _first_block = wallet_query.compact_block_at(from_height, None).await?;
    let compact_block_at_first_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _tip_block = wallet_query.compact_block_at(tip_height, None).await?;
    let compact_block_at_tip_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(from_height, tip_height), None)
        .await?;
    let compact_block_range_micros = measurement_start.elapsed().as_micros();
    let expected_block_count = usize::try_from(HOSTED_BACKFILL_DEPTH_BLOCKS)
        .map_err(|error| eyre!("hosted depth did not fit usize: {error}"))?;
    assert_eq!(range.compact_blocks.len(), expected_block_count);

    let measurement_start = std::time::Instant::now();
    let _tree = wallet_query.tree_state_at(tip_height, None).await?;
    let tree_state_at_micros = measurement_start.elapsed().as_micros();

    let coinbase_lookup = wallet_query
        .transaction_at_block_index(from_height, 0, None)
        .await?;
    let coinbase_txid = coinbase_lookup.transaction.transaction_id;

    let measurement_start = std::time::Instant::now();
    for _ in 0..TRANSACTION_LOOKUP_ITERATIONS {
        let _status = wallet_query.transaction(coinbase_txid, None).await?;
    }
    let transaction_total_micros = measurement_start.elapsed().as_micros();
    let transaction_avg_micros =
        transaction_total_micros / u128::from(TRANSACTION_LOOKUP_ITERATIONS);

    #[allow(
        clippy::print_stderr,
        reason = "calibration test reports measurements for operator review"
    )]
    {
        eprintln!(
            "live_checkpoint_latency_baseline network={} tip={} checkpoint_height={} \
             backfill={}us latest_block={}us compact_block_at_first={}us \
             compact_block_at_tip={}us compact_block_range_{}={}us tree_state_at={}us \
             transaction_avg={}us (n={} total={}us)",
            env.network().name(),
            tip_height.value(),
            checkpoint_height.value(),
            backfill_micros,
            latest_block_micros,
            compact_block_at_first_micros,
            compact_block_at_tip_micros,
            HOSTED_BACKFILL_DEPTH_BLOCKS,
            compact_block_range_micros,
            tree_state_at_micros,
            transaction_avg_micros,
            TRANSACTION_LOOKUP_ITERATIONS,
            transaction_total_micros,
        );
    }
    Ok(())
}
