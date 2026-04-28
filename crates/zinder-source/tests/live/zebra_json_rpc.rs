#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use eyre::{Result, eyre};
use zinder_core::{
    BlockHeight, ChainTipMetadata, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootIndex, TransactionBroadcastResult,
};
use zinder_source::{
    NodeCapability, NodeSource, TransactionBroadcaster, ZebraJsonRpcSource,
    ZebraJsonRpcSourceOptions,
};
use zinder_testkit::live::{init, require_live, require_live_mainnet};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fetch_chain_checkpoint_at_tip_returns_zero_tree_sizes_on_regtest() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    if !matches!(env.network(), Network::ZcashRegtest) {
        return Err(eyre!(
            "this test asserts regtest semantics; set ZINDER_NETWORK=zcash-regtest"
        ));
    }
    let source = zebra_source(&env)?;
    let tip = NodeSource::tip_id(&source).await?.height;
    let checkpoint = source.fetch_chain_checkpoint(tip).await?;

    assert_eq!(checkpoint.height, tip);
    assert_eq!(
        checkpoint.tip_metadata,
        ChainTipMetadata::new(0, 0),
        "regtest blocks have no shielded payload; checkpoint tree sizes should be zero"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fetch_chain_checkpoint_returns_advancing_tree_sizes_on_mainnet() -> Result<()> {
    let _guard = init();
    let env = require_live_mainnet()?;
    let source = zebra_source(&env)?;
    let tip = NodeSource::tip_id(&source).await?.height;
    let checkpoint_height = BlockHeight::new(tip.value().saturating_sub(1_000));
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;

    assert_eq!(checkpoint.height, checkpoint_height);
    assert!(
        checkpoint.tip_metadata.sapling_commitment_tree_size > 100_000,
        "mainnet sapling tree size at recent height should be well above 100k; got {}",
        checkpoint.tip_metadata.sapling_commitment_tree_size
    );
    assert!(
        checkpoint.tip_metadata.orchard_commitment_tree_size > 10_000,
        "mainnet orchard tree size at recent height should be above 10k; got {}",
        checkpoint.tip_metadata.orchard_commitment_tree_size
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn capability_probe_discovers_zebra_methods() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    let source = zebra_source(&env)?;

    let probed = source.probe_capabilities().await?;

    assert!(probed.supports(NodeCapability::JsonRpc));
    assert!(probed.supports(NodeCapability::OpenRpcDiscovery));
    assert!(probed.supports(NodeCapability::BestChainBlocks));
    assert!(probed.supports(NodeCapability::TipId));
    assert!(probed.supports(NodeCapability::TreeState));
    assert!(probed.supports(NodeCapability::SubtreeRoots));
    assert!(probed.supports(NodeCapability::TransactionBroadcast));

    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn mainnet_tip_id_advances_above_one_million() -> Result<()> {
    let _guard = init();
    let env = require_live_mainnet()?;
    let source = zebra_source(&env)?;
    let tip = source.tip_id().await?;

    assert!(
        tip.height.value() > 1_000_000,
        "mainnet tip height should be well above 1,000,000; got {tip:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn broadcast_classifies_invalid_transaction() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    let source = zebra_source(&env)?;

    let subtree_roots = source
        .fetch_subtree_roots(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        )
        .await?;
    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x00]))
        .await?;

    assert_eq!(subtree_roots.protocol, ShieldedProtocol::Sapling);
    assert_eq!(subtree_roots.start_index, SubtreeRootIndex::new(0));
    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::InvalidEncoding(_)
            | TransactionBroadcastResult::Rejected(_)
            | TransactionBroadcastResult::Unknown(_)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fetches_tip_block_and_rejects_invalid_transaction() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    let source = zebra_source(&env)?;

    let tip_height = source.tip_id().await?.height;
    let source_block = source.fetch_block_by_height(tip_height).await?;
    let subtree_roots = source
        .fetch_subtree_roots(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        )
        .await?;
    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x00]))
        .await?;

    assert!(tip_height.value() > 0);
    assert_eq!(source_block.network, env.network());
    assert_eq!(source_block.height, tip_height);
    assert_eq!(subtree_roots.protocol, ShieldedProtocol::Sapling);
    assert_eq!(subtree_roots.start_index, SubtreeRootIndex::new(0));
    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::InvalidEncoding(_)
            | TransactionBroadcastResult::Rejected(_)
            | TransactionBroadcastResult::Unknown(_)
    ));
    Ok(())
}

fn zebra_source(env: &zinder_testkit::live::LiveTestEnv) -> Result<ZebraJsonRpcSource> {
    Ok(ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?)
}
