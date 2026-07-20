//! Live regression: `MinedTransactionChainContext.consensus_branch_id` transitions correctly
//! across a network-upgrade activation height.
//!
//! The companion test [`mined_consensus_branch_id_parity`] checks a single
//! sampled tip coinbase; this file pins the *transition* itself by sampling
//! three heights that straddle the latest activation in the node's
//! upgrade table. The window is `(h-1, h, h+1)` where `h` is the highest
//! activation height that is reachable from the current chain tip.
//!
//! On regtest with ZFND's `z3` default sidecar, the latest activation is NU6
//! at height 2: the test bulk-catches-up heights 1..3 and asserts the consensus
//! branch id at height 1 (Canopy) differs from the branch id at height 2
//! (NU6) and that both match what
//! [`zinder_core::NetworkUpgradeActivations::consensus_branch_id_at`]
//! reports for each height.
//!
//! On testnet/mainnet, the most production-relevant activation is NU6.1.
//! The test resolves its height from the running node and bulk-catches-up a
//! three-block window around it.
//!
//! Reference: `docs/adrs/0008-network-parameter-discovery.md`.
//!
//! [`mined_consensus_branch_id_parity`]: ./mined_consensus_branch_id_parity.rs

#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, ConsensusBranchId, MinedTransaction, Network, NetworkUpgradeActivations, TxStatus,
};
use zinder_ingest::run_bulk_catchup;
use zinder_query::{TransactionStatus, WalletQuery, WalletQueryApi};
use zinder_store::PrimaryChainStore;
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    regtest_generate_blocks, zebra_source_from_bulk_catchup,
};

/// Hard cap on the boundary-window bulk catchup size on testnet/mainnet.
///
/// The window itself is three blocks; anchoring the bulk catchup on a
/// checkpoint just below the window keeps the run under a few seconds
/// even when the activation is millions of blocks back from the tip.
const NEAR_BOUNDARY_DEPTH_BLOCKS: u32 = 16;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "Boundary-crossing live test keeps env resolution, schedule discovery, run_bulk_catchup, and three-height assertions in one auditable path."
)]
async fn mined_consensus_branch_id_advances_across_latest_activation_height() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };

    // Make sure regtest has at least one block above the activation window.
    let tip_before = fetch_live_tip_height(&env).await?;
    if env.network() == Network::ZcashRegtest && tip_before.value() < 4 {
        regtest_generate_blocks(&env, 4_u32.saturating_sub(tip_before.value())).await?;
    }
    let tip_height = fetch_live_tip_height(&env).await?;

    let activations = fetch_live_network_upgrade_activations(&env).await?;

    // Default regtest activates every upgrade at height 1, so there is no
    // mid-chain boundary to cross; the assertion only applies where the node
    // advertises an activation above genesis (testnet, mainnet).
    let Some(activation_height) = latest_reachable_activation_height(&activations, tip_height)
    else {
        return Ok(());
    };
    let window_start = BlockHeight::new(activation_height.value().saturating_sub(1));
    let window_end = BlockHeight::new(activation_height.value().saturating_add(1));

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");

    // On chains where the activation is far above genesis (testnet/mainnet)
    // anchor the bulk catchup at a checkpoint just below the window. On regtest
    // the window can be 1..3 and a checkpoint is structurally unavailable.
    let use_checkpoint = window_start.value() > NEAR_BOUNDARY_DEPTH_BLOCKS + 1;
    let (from_height, checkpoint_height) = if use_checkpoint {
        let checkpoint = BlockHeight::new(window_start.value() - 1);
        (window_start, Some(checkpoint))
    } else {
        (BlockHeight::new(1), None)
    };

    let mut bulk_catchup_config = live_bulk_catchup_run_config(
        &env,
        &storage_path,
        from_height,
        window_end,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid bulk-catchup batch size"))?,
        false,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_bulk_catchup(&bulk_catchup_config)?;
    if let Some(checkpoint_height) = checkpoint_height {
        bulk_catchup_config.checkpoint = Some(
            source
                .fetch_chain_checkpoint(checkpoint_height, &activations)
                .await?,
        );
    }
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;

    let store =
        PrimaryChainStore::open(&storage_path, bulk_catchup_config.canonical_store_options())?;
    let wallet_query = WalletQuery::new(store, (), activations.clone());

    let pre_activation_branch_id = read_coinbase_consensus_branch_id(&wallet_query, window_start)
        .await
        .map_err(|error| {
            eyre!(
                "could not resolve coinbase at pre-activation height {}: {error}",
                window_start.value()
            )
        })?;
    let activation_branch_id = read_coinbase_consensus_branch_id(&wallet_query, activation_height)
        .await
        .map_err(|error| {
            eyre!(
                "could not resolve coinbase at activation height {}: {error}",
                activation_height.value()
            )
        })?;
    let post_activation_branch_id = read_coinbase_consensus_branch_id(&wallet_query, window_end)
        .await
        .map_err(|error| {
            eyre!(
                "could not resolve coinbase at post-activation height {}: {error}",
                window_end.value()
            )
        })?;

    let expected_pre = activations.consensus_branch_id_at(window_start);
    let expected_activation = activations.consensus_branch_id_at(activation_height);
    let expected_post = activations.consensus_branch_id_at(window_end);

    assert_eq!(
        pre_activation_branch_id,
        expected_pre,
        "branch id at pre-activation height {} must match activations table",
        window_start.value()
    );
    assert_eq!(
        activation_branch_id,
        expected_activation,
        "branch id at activation height {} must match activations table",
        activation_height.value()
    );
    assert_eq!(
        post_activation_branch_id,
        expected_post,
        "branch id at post-activation height {} must match activations table",
        window_end.value()
    );
    assert_ne!(
        pre_activation_branch_id, activation_branch_id,
        "consensus branch id must change at the activation boundary; \
         pre={pre_activation_branch_id:?}, activation={activation_branch_id:?}"
    );
    assert_eq!(
        activation_branch_id, post_activation_branch_id,
        "no further activation should occur within the window; \
         activation={activation_branch_id:?}, post={post_activation_branch_id:?}"
    );
    Ok(())
}

/// Highest `activation_height` strictly greater than 1 (so a meaningful
/// transition exists below it) that the chain tip has already crossed
/// (so bulk catchup can sample heights below, at, and above it).
fn latest_reachable_activation_height(
    activations: &NetworkUpgradeActivations,
    tip_height: BlockHeight,
) -> Option<BlockHeight> {
    let tip_value = tip_height.value();
    activations
        .activations()
        .iter()
        .map(|activation| activation.activation_height)
        .filter(|height| height.value() > 1)
        .filter(|height| height.value() < tip_value)
        .max_by_key(|height| height.value())
}

async fn read_coinbase_consensus_branch_id<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    block_height: BlockHeight,
) -> Result<ConsensusBranchId> {
    let coinbase_transaction = wallet_query
        .transaction_at_block_index(block_height, 0, None)
        .await?;
    let coinbase_transaction_id = coinbase_transaction.transaction.transaction_id;
    let status = wallet_query
        .transaction(coinbase_transaction_id, None)
        .await?;
    let TransactionStatus {
        status: TxStatus::Mined(MinedTransaction { chain_context, .. }),
        ..
    } = status
    else {
        return Err(eyre!(
            "coinbase at height {} must surface as TxStatus::Mined; got {:?}",
            block_height.value(),
            status.status
        ));
    };
    Ok(chain_context.consensus_branch_id)
}
