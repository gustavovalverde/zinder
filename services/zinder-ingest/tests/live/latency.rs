#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::Network;
use zinder_core::{BlockHeight, BlockHeightRange};
use zinder_ingest::backfill;
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live_for};

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
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
    let _latest = wallet_query.latest_block().await?;
    let latest_block_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let _block = wallet_query.compact_block_at(BlockHeight::new(1)).await?;
    let compact_block_at_micros = measurement_start.elapsed().as_micros();

    let measurement_start = std::time::Instant::now();
    let range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(tip_height.value().min(50)),
        ))
        .await?;
    let compact_block_range_50_micros = measurement_start.elapsed().as_micros();
    assert!(!range.compact_blocks.is_empty());

    let measurement_start = std::time::Instant::now();
    let _tree = wallet_query.tree_state_at(BlockHeight::new(1)).await?;
    let tree_state_at_micros = measurement_start.elapsed().as_micros();

    #[allow(
        clippy::print_stderr,
        reason = "calibration test reports measurements for operator review"
    )]
    {
        eprintln!(
            "live_latency_baseline network={} tip={} latest_block={}us compact_block_at={}us \
             compact_block_range_50={}us tree_state_at={}us",
            env.network().name(),
            tip_height.value(),
            latest_block_micros,
            compact_block_at_micros,
            compact_block_range_50_micros,
            tree_state_at_micros,
        );
    }
    Ok(())
}
