#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live regression: `MinedDetails.consensus_branch_id` returned by the typed
//! `WalletQueryApi::transaction` lookup matches what the running node's
//! upgrade activations say is active at the mined height.
//!
//! Closes the wire-level gap left open by `lightwalletd_grpc` integration
//! tests, which only exercise the in-process adapter against a synthetic
//! table. This test follows the end-to-end production path:
//!
//! 1. Fetch the activations from the running Zebra via
//!    `ZebraJsonRpcSource::fetch_network_upgrade_activations()`.
//! 2. Backfill a small near-tip window through `zinder-ingest`.
//! 3. Open `zinder-query::WalletQuery` with the discovered activations.
//! 4. Pick the tip block's coinbase via
//!    `WalletQueryApi::transaction_at_block_index(tip, 0)`.
//! 5. Look up that txid via `WalletQueryApi::transaction(...)`.
//! 6. Assert `MinedDetails.consensus_branch_id == activations.consensus_branch_id_at(mined_height)`.
//!
//! Pins the regtest active-upgrade behavior in CI and proves parity on
//! testnet and mainnet by opting in via `require_live_for`.

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{BlockHeight, MinedTransaction, Network, TransactionArtifact, TxStatus};
use zinder_ingest::{BackfillOutcome, backfill};
use zinder_query::{TransactionStatus, WalletQuery, WalletQueryApi};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    fetch_live_tip_height, live_backfill_config, regtest_generate_blocks,
    zebra_source_from_backfill,
};

/// Near-tip backfill depth on testnet/mainnet. Small enough to finish in
/// seconds; large enough that the window contains a stable coinbase to query.
const NEAR_TIP_DEPTH_BLOCKS: u32 = 16;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "Near-tip parity sweep keeps env resolution, backfill, multi-height assertions, and one optional regtest branch in one auditable scenario."
)]
async fn mined_details_consensus_branch_id_matches_node_upgrade_activations() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };

    let tip_before = fetch_live_tip_height(&env).await?;
    if env.network() == Network::ZcashRegtest && tip_before.value() == 0 {
        regtest_generate_blocks(&env, 1).await?;
    }
    let tip_height = fetch_live_tip_height(&env).await?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");

    // On chains with non-trivial history (testnet/mainnet) anchor the
    // backfill at a recent checkpoint so the test stays fast. On regtest the
    // tip can be too shallow for that, so backfill from genesis.
    let use_checkpoint = tip_height.value() > NEAR_TIP_DEPTH_BLOCKS + 1;
    let (from_height, checkpoint_height) = if use_checkpoint {
        let checkpoint = BlockHeight::new(tip_height.value() - NEAR_TIP_DEPTH_BLOCKS - 1);
        (BlockHeight::new(checkpoint.value() + 1), Some(checkpoint))
    } else {
        (BlockHeight::new(1), None)
    };

    let mut backfill_config = live_backfill_config(
        &env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid backfill batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let activations = source
        .discover_network_upgrade_activations("zinder-ingest-tests")
        .await?;
    if let Some(checkpoint_height) = checkpoint_height {
        backfill_config.checkpoint = Some(source.fetch_chain_checkpoint(checkpoint_height).await?);
    }
    let BackfillOutcome::Committed(_outcome) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected committed backfill outcome"));
    };

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let wallet_query = WalletQuery::new(store, (), activations.clone());

    let coinbase_transaction = wallet_query
        .transaction_at_block_index(tip_height, 0, None)
        .await?;
    let coinbase_transaction_id = coinbase_transaction.transaction.transaction_id;
    let coinbase_block_height = coinbase_transaction.transaction.block_height;

    let status = wallet_query
        .transaction(coinbase_transaction_id, None)
        .await?;
    let TransactionStatus {
        status:
            TxStatus::Mined(MinedTransaction {
                artifact: TransactionArtifact { block_height, .. },
                details,
            }),
        ..
    } = status
    else {
        return Err(eyre!(
            "tip coinbase {coinbase_transaction_id:?} must surface as TxStatus::Mined, got {:?}",
            status.status
        ));
    };

    assert_eq!(
        block_height, coinbase_block_height,
        "TxStatus::Mined artifact block_height must match the coinbase's block height"
    );
    let expected_branch_id = activations.consensus_branch_id_at(coinbase_block_height);
    assert_eq!(
        details.consensus_branch_id,
        expected_branch_id,
        "MinedDetails.consensus_branch_id must match the activations' branch id at the mined \
         height (mined_height={}, activations_say={:#010x}, MinedDetails_says={:#010x})",
        coinbase_block_height.value(),
        expected_branch_id,
        details.consensus_branch_id,
    );
    Ok(())
}
