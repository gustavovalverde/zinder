#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! Performance regression-only smoke tests for the wallet query plane.
//!
//! These tests guarantee that a representative range read completes within
//! a generous CI budget. They are not benchmarks; the dedicated `criterion`
//! suite (W6 follow-up) measures P50/P99 numbers, while this file catches
//! catastrophic regressions on every CI run.
//!
//! Budgets are deliberately loose so this test stays green under contended
//! CI workers. Tight per-percentile numbers live in
//! [`docs/architecture/wallet-data-plane.md`] under §Published Budgets.

use std::time::{Duration, Instant};

use zinder_core::{BlockHeight, BlockHeightRange, ChainEpochId, Network};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_testkit::{ChainFixture, StoreFixture};

const PERF_SMOKE_BLOCK_COUNT: u32 = 1_000;
const PERF_SMOKE_RANGE_BUDGET: Duration = Duration::from_secs(2);
const PERF_SMOKE_LATEST_BUDGET: Duration = Duration::from_millis(250);

#[tokio::test(flavor = "multi_thread")]
async fn compact_block_range_one_thousand_blocks_stays_under_budget() -> eyre::Result<()> {
    let chain_fixture =
        ChainFixture::new(Network::ZcashRegtest).extend_blocks(PERF_SMOKE_BLOCK_COUNT);
    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let wallet_query = WalletQuery::new(store_fixture.chain_store().clone(), ());

    let start = Instant::now();
    let response = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(PERF_SMOKE_BLOCK_COUNT),
        ))
        .await?;
    let elapsed = start.elapsed();

    assert_eq!(
        response.compact_blocks.len(),
        usize::try_from(PERF_SMOKE_BLOCK_COUNT)?
    );
    assert!(
        elapsed <= PERF_SMOKE_RANGE_BUDGET,
        "compact_block_range over {PERF_SMOKE_BLOCK_COUNT} blocks took {elapsed:?}, budget is {PERF_SMOKE_RANGE_BUDGET:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn latest_block_stays_under_budget() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let wallet_query = WalletQuery::new(store_fixture.chain_store().clone(), ());

    let start = Instant::now();
    let response = wallet_query.latest_block().await?;
    let elapsed = start.elapsed();

    assert_eq!(response.height, BlockHeight::new(1));
    assert!(
        elapsed <= PERF_SMOKE_LATEST_BUDGET,
        "latest_block read took {elapsed:?}, budget is {PERF_SMOKE_LATEST_BUDGET:?}"
    );

    Ok(())
}
