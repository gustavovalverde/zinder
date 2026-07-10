#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{collections::HashSet, num::NonZeroU64, time::Duration};

use eyre::eyre;
use serde_json::{Value, json};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, BroadcastAccepted, ChainTipMetadata, Network,
    RawTransactionBytes, ShieldedProtocol, SubtreeRootIndex, TransactionBroadcastResult,
    TransactionId,
};
use zinder_source::{
    NodeAuth, NodeCapability, NodeHealthConfig, NodeSource, SourceChainCursor, SourceChainUpdate,
    SourceError, TransactionBroadcaster, UPSTREAM_HEALTH_REASON_ESTIMATED_GAP_ABOVE_FLOOR,
    UPSTREAM_HEALTH_REASON_VERIFICATION_PROGRESS_BELOW_FLOOR,
    UPSTREAM_HEALTH_SOURCE_VERIFICATION_PROGRESS_FALLBACK, ZebraJsonRpcSource,
    decode_rpc_block_hash,
};
use zinder_testkit::{JsonRpcTestServer, RpcReply, method};

#[tokio::test]
async fn fetch_block_at_uses_expected_json_rpc_methods_and_basic_auth() -> eyre::Result<()> {
    // `fetch_block_at` keys the required RPCs on the requested height
    // directly; no separate `getblockhash` or redundant `getblockheader`
    // round trip. Pin the method set and parameter shape since operators
    // read both through tracing.
    let fixture = fixture_block()?;
    let server = JsonRpcTestServer::start([
        method("getblock").reply(RpcReply::result(json!(fixture["raw_block_hex"])))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::basic("zebra", "zebra"),
        Duration::from_secs(5),
    )?;

    let source_block = source.fetch_block_at(BlockHeight::new(1)).await?;
    let requests = server.requests()?;

    assert_eq!(source_block.height, BlockHeight::new(1));
    assert_eq!(
        source_block.hash,
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?
    );
    let methods_called: HashSet<String> = requests.iter().map(|r| r.method.clone()).collect();
    let expected: HashSet<String> = std::iter::once("getblock").map(str::to_owned).collect();
    assert_eq!(
        methods_called, expected,
        "the block-fetch RPC keys on the height directly; tree state is fetched through its own checkpoint path",
    );
    assert!(
        server.requests_for("getblockhash")?.is_empty(),
        "`getblockhash` must not be called by the fetch path",
    );
    assert!(
        server.requests_for("getblockheader")?.is_empty(),
        "`getblockheader` must not be called by the fetch path",
    );
    assert_eq!(server.requests_for("getblock")?[0].params, json!(["1", 0]),);
    assert!(
        server.requests_for("z_gettreestate")?.is_empty(),
        "`z_gettreestate` must not be called by the raw block fetch path",
    );
    assert!(
        requests
            .iter()
            .all(|request| { request.authorization.as_deref() == Some("Basic emVicmE6emVicmE=") })
    );

    Ok(())
}

#[tokio::test]
async fn fetch_chain_update_after_start_emits_connected_block() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(fixture["hash"]))),
        method("getblockheader").reply(RpcReply::result(json!({
            "hash": fixture["hash"],
            "height": fixture["height"],
            "previousblockhash": fixture["previousblockhash"],
            "time": fixture["time"],
        }))),
        method("getblock").reply(RpcReply::result(json!(fixture["raw_block_hex"]))),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let update = source
        .fetch_chain_update_after(SourceChainCursor::before_first_block())
        .await?
        .ok_or_else(|| eyre!("expected connected block update"))?;

    let SourceChainUpdate::ConnectedBlock { cursor, block } = update else {
        return Err(eyre!("expected connected block update"));
    };
    let block_id = BlockId::new(
        BlockHeight::new(1),
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?,
    );
    assert_eq!(cursor, SourceChainCursor::at_block(block_id));
    assert_eq!(block.height, BlockHeight::new(1));
    assert!(
        server.requests_for("getblockhash")?.is_empty(),
        "the update adapter should stay on the existing height-keyed block fetch path",
    );
    assert!(
        server.requests_for("z_gettreestate")?.is_empty(),
        "tree state is fetched separately from connected block updates",
    );
    Ok(())
}

#[tokio::test]
async fn fetch_chain_update_after_tip_cursor_returns_none() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let block_id = BlockId::new(
        BlockHeight::new(1),
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?,
    );
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(fixture["hash"]))),
        method("getblockheader").reply(RpcReply::result(json!({
            "hash": fixture["hash"],
            "height": fixture["height"],
            "previousblockhash": fixture["previousblockhash"],
            "time": fixture["time"],
        }))),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let update = source
        .fetch_chain_update_after(SourceChainCursor::at_block(block_id))
        .await?;

    assert_eq!(update, None);
    assert!(
        server.requests_for("getblock")?.is_empty(),
        "a cursor already at the observed tip should not fetch block payloads",
    );
    Ok(())
}

#[tokio::test]
async fn fetch_chain_update_after_diverged_tip_emits_reverted_block() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let old_block_id = BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([7; 32]));
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(fixture["hash"]))),
        method("getblockheader").reply(RpcReply::result(json!({
            "hash": fixture["hash"],
            "height": fixture["height"],
            "previousblockhash": fixture["previousblockhash"],
            "time": fixture["time"],
        }))),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let update = source
        .fetch_chain_update_after(SourceChainCursor::at_block(old_block_id))
        .await?
        .ok_or_else(|| eyre!("expected reverted block update"))?;

    assert_eq!(update, SourceChainUpdate::reverted_block(old_block_id));
    assert!(
        server.requests_for("getblock")?.is_empty(),
        "same-height cursor divergence should emit a revert before fetching replacement payloads",
    );
    Ok(())
}

#[tokio::test]
async fn json_rpc_error_maps_to_block_unavailable_with_view_changed_class() -> eyre::Result<()> {
    // Zebra's "height out of range" surfaces when the upstream's best
    // chain shifted between the parallel calls. The error rides on any
    // height-keyed block-fetch call. The adapter classifies it as
    // UpstreamViewChanged so the loop treats it as a recoverable signal
    // instead of a fatal exit.
    let server = JsonRpcTestServer::start([
        method("getblock").reply(RpcReply::error("height out of range"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.fetch_block_at(BlockHeight::new(1)).await {
        Ok(source_block) => {
            return Err(eyre!("expected fetch error, got {source_block:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceError::BlockUnavailable { height, .. } if height == BlockHeight::new(1)
    ));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::UpstreamViewChanged,
    );

    Ok(())
}

#[tokio::test]
async fn json_rpc_warming_up_error_keeps_block_unavailable_classification() -> eyre::Result<()> {
    // Warming-up on the block payload path classifies as UpstreamViewChanged
    // so the loop recovers.
    let server = JsonRpcTestServer::start([
        method("getblock").reply(RpcReply::error_with_code(-28, "node warming up"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.fetch_block_at(BlockHeight::new(1)).await {
        Ok(source_block) => {
            return Err(eyre!("expected fetch error, got {source_block:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceError::BlockUnavailable { height, .. } if height == BlockHeight::new(1)
    ));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::UpstreamViewChanged,
    );

    Ok(())
}

#[tokio::test]
async fn missing_json_rpc_result_maps_to_protocol_mismatch() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([method("getblock").reply(RpcReply::empty())])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.fetch_block_at(BlockHeight::new(1)).await {
        Ok(source_block) => {
            return Err(eyre!("expected fetch error, got {source_block:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, SourceError::NodeUnavailable { .. }));

    Ok(())
}

#[tokio::test]
async fn json_rpc_response_size_limit_is_configurable() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start(
            [method("getbestblockhash").reply(RpcReply::result(json!("00")))],
        )?;
    let source = ZebraJsonRpcSource::with_options(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        zinder_source::ZebraJsonRpcSourceOptions {
            request_timeout: Duration::from_secs(5),
            max_response_bytes: NonZeroU64::MIN,
            broadcast_timeout: None,
        },
    )?;

    let error = match source.tip_id().await {
        Ok(tip_id) => {
            return Err(eyre!("expected response limit error, got {tip_id:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceError::SourceResponseTooLarge {
            operation: "getbestblockhash",
            max_response_bytes: 1,
        }
    ));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::Configuration,
    );

    Ok(())
}

#[tokio::test]
async fn http_503_marks_node_unavailable_retryable() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getbestblockhash").reply(RpcReply::http_status(503))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.tip_id().await {
        Ok(tip_id) => {
            return Err(eyre!("expected HTTP status error, got {tip_id:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, SourceError::NodeUnavailable { .. }));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::NodeUnreachable,
    );

    Ok(())
}

#[tokio::test]
async fn json_rpc_warming_up_error_marks_tip_node_unreachable() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::error_with_code(-28, "node warming up"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.tip_id().await {
        Ok(tip_id) => {
            return Err(eyre!("expected tip error, got {tip_id:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, SourceError::NodeUnavailable { .. }));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::NodeUnreachable,
    );

    Ok(())
}

#[tokio::test]
async fn tip_id_uses_header_height_from_observed_best_hash() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(fixture["hash"]))),
        method("getblockheader").reply(RpcReply::result(json!({
            "hash": fixture["hash"],
            "height": fixture["height"],
            "previousblockhash": fixture["previousblockhash"],
            "time": fixture["time"],
        }))),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let tip_id = source.tip_id().await?;

    assert_eq!(tip_id.height, BlockHeight::new(1));
    assert_eq!(
        tip_id.hash,
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?
    );
    let methods_called: HashSet<String> =
        server.requests()?.into_iter().map(|r| r.method).collect();
    let expected: HashSet<String> = ["getbestblockhash", "getblockheader"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(methods_called, expected);
    assert_eq!(
        server.requests_for("getbestblockhash")?[0].params,
        Value::Null
    );
    assert_eq!(
        server.requests_for("getblockheader")?[0].params,
        json!([fixture["hash"], true])
    );

    Ok(())
}

#[tokio::test]
async fn tip_id_reobserves_after_zebra_invalidates_the_observed_tip() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let stale_tip_hash = "ab".repeat(32);
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(stale_tip_hash))),
        method("getblockheader").reply(RpcReply::error("block height not in best chain")),
        method("getbestblockhash").reply(RpcReply::result(json!(fixture["hash"]))),
        method("getblockheader").reply(RpcReply::result(json!({
            "hash": fixture["hash"],
            "height": fixture["height"],
            "previousblockhash": fixture["previousblockhash"],
            "time": fixture["time"],
        }))),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let tip_id = source.tip_id().await?;

    assert_eq!(tip_id.height, BlockHeight::new(1));
    assert_eq!(
        tip_id.hash,
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?
    );
    assert_eq!(server.requests_for("getbestblockhash")?.len(), 2);
    assert_eq!(server.requests_for("getblockheader")?.len(), 2);

    Ok(())
}

#[tokio::test]
async fn tip_id_classifies_an_unstable_best_chain_view_as_recoverable() -> eyre::Result<()> {
    let stale_tip_hash = "ab".repeat(32);
    let server = JsonRpcTestServer::start([
        method("getbestblockhash").reply(RpcReply::result(json!(stale_tip_hash))),
        method("getblockheader").reply(RpcReply::error("block height not in best chain")),
        method("getbestblockhash").reply(RpcReply::result(json!(stale_tip_hash))),
        method("getblockheader").reply(RpcReply::error("block height not in best chain")),
        method("getbestblockhash").reply(RpcReply::result(json!(stale_tip_hash))),
        method("getblockheader").reply(RpcReply::error("block height not in best chain")),
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.tip_id().await {
        Ok(tip_id) => return Err(eyre!("expected tip error, got {tip_id:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, SourceError::TipViewChanged { .. }));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::UpstreamViewChanged,
    );
    assert_eq!(server.requests_for("getbestblockhash")?.len(), 3);
    assert_eq!(server.requests_for("getblockheader")?.len(), 3);

    Ok(())
}

#[tokio::test]
async fn bad_raw_block_hex_maps_to_invalid_raw_block_hex() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblock").reply(RpcReply::result(json!("not-hex")))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source.fetch_block_at(BlockHeight::new(1)).await {
        Ok(source_block) => {
            return Err(eyre!("expected fetch error, got {source_block:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, SourceError::InvalidRawBlockHex { .. }));

    Ok(())
}

#[tokio::test]
async fn tree_state_hash_disagreement_maps_to_block_reorg_during_fetch() -> eyre::Result<()> {
    // Same shape as the header-hash test: tree-state hash disagreeing
    // with the parsed raw block hash is the second reorg-during-fetch
    // signature. Classified as UpstreamViewChanged so the loop recovers.
    let fixture = fixture_block()?;
    let server =
        JsonRpcTestServer::start([method("z_gettreestate").reply(RpcReply::result(json!({
            "network": "regtest",
            "height": fixture["height"],
            "hash": "1111111111111111111111111111111111111111111111111111111111111111",
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;
    let requested_block = BlockId::new(
        BlockHeight::new(1),
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?,
    );

    let error = match source.fetch_tree_state_for_block(requested_block).await {
        Ok(tree_state) => {
            return Err(eyre!("expected fetch error, got {tree_state:?}"));
        }
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            SourceError::BlockReorgDuringFetch { height, .. } if height == BlockHeight::new(1)
        ),
        "expected BlockReorgDuringFetch, got {error:?}",
    );
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::UpstreamViewChanged,
    );

    Ok(())
}

#[tokio::test]
async fn tree_state_height_mismatch_maps_to_protocol_mismatch() -> eyre::Result<()> {
    let fixture = fixture_block()?;
    let server =
        JsonRpcTestServer::start([method("z_gettreestate").reply(RpcReply::result(json!({
            "network": "regtest",
            "height": 2,
            "hash": fixture["hash"],
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;
    let requested_block = BlockId::new(
        BlockHeight::new(1),
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?,
    );

    let error = match source.fetch_tree_state_for_block(requested_block).await {
        Ok(tree_state) => {
            return Err(eyre!("expected fetch error, got {tree_state:?}"));
        }
        Err(error) => error,
    };

    // Tree-state returned the wrong height for the same request id —
    // a Zebra wire-contract violation, not a reorg.
    assert!(matches!(error, SourceError::SourceProtocolMismatch { .. }));

    Ok(())
}

#[tokio::test]
async fn zebra_json_rpc_advertises_transaction_broadcast() -> eyre::Result<()> {
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        "http://127.0.0.1:18232",
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    assert!(
        source
            .capabilities()
            .supports(NodeCapability::TransactionBroadcast)
    );

    Ok(())
}

#[tokio::test]
async fn zebra_json_rpc_advertises_subtree_roots() -> eyre::Result<()> {
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        "http://127.0.0.1:18232",
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    assert!(source.capabilities().supports(NodeCapability::SubtreeRoots));

    Ok(())
}

#[tokio::test]
async fn fetch_subtree_roots_uses_expected_json_rpc_request() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([method("z_getsubtreesbyindex").reply(
        RpcReply::result(json!({
            "pool": "sapling",
            "start_index": 4,
            "subtrees": [
                {
                    "root": "1111111111111111111111111111111111111111111111111111111111111111",
                    "end_height": 558_822
                },
                {
                    "root": "2222222222222222222222222222222222222222222222222222222222222222",
                    "end_height": 670_209
                }
            ]
        })),
    )])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::basic("zebra", "zebra"),
        Duration::from_secs(5),
    )?;

    let subtree_roots = source
        .fetch_subtree_roots(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(4),
            std::num::NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max entries"))?,
        )
        .await?;
    let requests = server.requests_for("z_getsubtreesbyindex")?;

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].params, json!(["sapling", 4, 2]));
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Basic emVicmE6emVicmE=")
    );
    assert_eq!(subtree_roots.protocol, ShieldedProtocol::Sapling);
    assert_eq!(subtree_roots.start_index, SubtreeRootIndex::new(4));
    assert_eq!(subtree_roots.subtree_roots.len(), 2);
    assert_eq!(
        subtree_roots.subtree_roots[0].subtree_index,
        SubtreeRootIndex::new(4)
    );
    assert_eq!(
        subtree_roots.subtree_roots[0].root_hash.as_bytes(),
        [0x11; 32]
    );
    assert_eq!(
        subtree_roots.subtree_roots[0].completing_block_height,
        BlockHeight::new(558_822)
    );
    assert_eq!(
        subtree_roots.subtree_roots[1].subtree_index,
        SubtreeRootIndex::new(5)
    );
    assert_eq!(
        subtree_roots.subtree_roots[1].root_hash.as_bytes(),
        [0x22; 32]
    );
    assert_eq!(
        subtree_roots.subtree_roots[1].completing_block_height,
        BlockHeight::new(670_209)
    );

    Ok(())
}

#[tokio::test]
async fn json_rpc_warming_up_error_marks_subtree_roots_unavailable_retryable() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([
        method("z_getsubtreesbyindex").reply(RpcReply::error_with_code(-28, "node warming up"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let error = match source
        .fetch_subtree_roots(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        )
        .await
    {
        Ok(subtree_roots) => {
            return Err(eyre!("expected subtree roots error, got {subtree_roots:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SourceError::SubtreeRootsUnavailable {
            protocol: ShieldedProtocol::Sapling,
            start_index,
            ..
        } if start_index == SubtreeRootIndex::new(0)
    ));
    assert_eq!(
        error.upstream_classification(),
        zinder_source::SourceFailureClass::UpstreamViewChanged,
    );

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_maps_success_to_transaction_id() -> eyre::Result<()> {
    let display_transaction_id = "1111111111111111111111111111111111111111111111111111111111111111";
    let server = JsonRpcTestServer::start([
        method("sendrawtransaction").reply(RpcReply::result(json!(display_transaction_id)))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::basic("zebra", "zebra"),
        Duration::from_secs(5),
    )?;

    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x00, 0x01, 0x02]))
        .await?;
    let requests = server.requests_for("sendrawtransaction")?;

    assert_eq!(
        broadcast_result,
        TransactionBroadcastResult::Accepted(BroadcastAccepted {
            transaction_id: TransactionId::from_bytes([0x11; 32]),
        })
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].params, json!(["000102"]));
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Basic emVicmE6emVicmE=")
    );

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_classifies_invalid_encoding() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([
        method("sendrawtransaction").reply(RpcReply::error_with_code(-22, "TX decode failed"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x00]))
        .await?;

    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::InvalidEncoding(invalid_encoding)
            if invalid_encoding.error_code == Some(-22)
                && invalid_encoding.message == "TX decode failed"
    ));

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_classifies_duplicate() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([
            method("sendrawtransaction").reply(RpcReply::error_with_code(
                -27,
                "transaction already in mempool",
            )),
        ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x01]))
        .await?;

    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::Duplicate(duplicate)
            if duplicate.error_code == Some(-27)
                && duplicate.message == "transaction already in mempool"
    ));

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_does_not_classify_unknown_as_duplicate() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([method("sendrawtransaction")
        .reply(RpcReply::error_with_code(-8, "transaction unknown to node"))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x01]))
        .await?;

    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::Rejected(rejected)
            if rejected.error_code == Some(-8)
                && rejected.message == "transaction unknown to node"
    ));

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_without_error_code_returns_unknown() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([method("sendrawtransaction").reply(
        RpcReply::error_without_code("duplicate field contains a hex branch id already checked"),
    )])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let broadcast_result = source
        .broadcast_transaction(RawTransactionBytes::new([0x01]))
        .await?;

    // jsonrpsee's ErrorObject requires a code; an error without one comes
    // through as a rejected/unclassified result via the broadcast classifier.
    assert!(matches!(
        broadcast_result,
        TransactionBroadcastResult::Rejected(_) | TransactionBroadcastResult::Unknown(_),
    ));

    Ok(())
}

#[tokio::test]
async fn probe_capabilities_parses_openrpc_method_list() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("rpc.discover").reply(RpcReply::result(json!({
            "openrpc": "1.3.2",
            "info": {"title": "Zebra", "version": "8.0.0"},
            "methods": [
                {"name": "getblock"},
                {"name": "getbestblockhash"},
                {"name": "getblockheader"},
                {"name": "z_gettreestate"},
                {"name": "z_getsubtreesbyindex"},
                {"name": "sendrawtransaction"},
                {"name": "getblockchaininfo"},
                {"name": "rpc.discover"},
                {"name": "ping"},
            ],
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let probed = source.probe_capabilities().await?;

    assert!(probed.supports(NodeCapability::JsonRpc));
    assert!(probed.supports(NodeCapability::OpenRpcDiscovery));
    assert!(probed.supports(NodeCapability::BestChainBlocks));
    assert!(probed.supports(NodeCapability::TipId));
    assert!(probed.supports(NodeCapability::TreeState));
    assert!(probed.supports(NodeCapability::SubtreeRoots));
    assert!(probed.supports(NodeCapability::TransactionBroadcast));
    assert!(probed.supports(NodeCapability::ChainValuePools));
    assert_eq!(source.capabilities(), probed);

    Ok(())
}

#[tokio::test]
async fn probe_capabilities_falls_back_when_method_not_found() -> eyre::Result<()> {
    let server = JsonRpcTestServer::start([
        method("rpc.discover").reply(RpcReply::error_with_code(-32601, "Method not found"))
    ])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let probed = source.probe_capabilities().await?;

    assert!(probed.supports(NodeCapability::JsonRpc));
    assert!(!probed.supports(NodeCapability::OpenRpcDiscovery));
    assert!(probed.supports(NodeCapability::BestChainBlocks));
    assert!(probed.supports(NodeCapability::TipId));
    assert!(probed.supports(NodeCapability::TreeState));
    assert!(probed.supports(NodeCapability::SubtreeRoots));
    assert!(probed.supports(NodeCapability::ChainValuePools));

    Ok(())
}

#[tokio::test]
async fn probe_capabilities_requires_tip_id_method_set() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("rpc.discover").reply(RpcReply::result(json!({
            "openrpc": "1.3.2",
            "methods": [
                {"name": "getbestblockhash"},
                {"name": "rpc.discover"},
            ],
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let probed = source.probe_capabilities().await?;

    assert!(!probed.supports(NodeCapability::TipId));
    assert!(probed.supports(NodeCapability::OpenRpcDiscovery));
    assert!(!probed.supports(NodeCapability::BestChainBlocks));
    assert!(!probed.supports(NodeCapability::TreeState));
    assert!(!probed.supports(NodeCapability::SubtreeRoots));
    assert!(!probed.supports(NodeCapability::TransactionBroadcast));
    assert!(!probed.supports(NodeCapability::ChainValuePools));

    Ok(())
}

#[tokio::test]
async fn probe_capabilities_keeps_only_advertised_capabilities_on_success() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("rpc.discover").reply(RpcReply::result(json!({
            "openrpc": "1.3.2",
            "methods": [
                {"name": "getbestblockhash"},
                {"name": "getblockheader"},
                {"name": "rpc.discover"},
            ],
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let probed = source.probe_capabilities().await?;

    assert!(probed.supports(NodeCapability::TipId));
    assert!(probed.supports(NodeCapability::OpenRpcDiscovery));
    assert!(!probed.supports(NodeCapability::BestChainBlocks));
    assert!(!probed.supports(NodeCapability::TreeState));
    assert!(!probed.supports(NodeCapability::SubtreeRoots));
    assert!(!probed.supports(NodeCapability::TransactionBroadcast));
    assert!(!probed.supports(NodeCapability::ChainValuePools));

    Ok(())
}

#[tokio::test]
async fn fetch_chain_checkpoint_parses_getblock_trees_field() -> eyre::Result<()> {
    let block_hash_hex = "010101010101010101010101010101010101010101010101010101010101010f";
    let server = JsonRpcTestServer::start([method("getblock").reply(RpcReply::result(json!({
        "height": 100,
        "hash": block_hash_hex,
        "trees": {
            "sapling": {"size": 1234},
            "orchard": {"size": 567},
            "ironwood": {"size": 89},
        },
    })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let checkpoint = source.fetch_chain_checkpoint(BlockHeight::new(100)).await?;

    assert_eq!(checkpoint.height, BlockHeight::new(100));
    assert_eq!(checkpoint.hash, decode_rpc_block_hash(block_hash_hex)?);
    assert_eq!(
        checkpoint.tip_metadata,
        ChainTipMetadata::new(1234, 567, 89)
    );
    assert!(
        server.requests_for("getblockhash")?.is_empty(),
        "checkpoint fetch should use height-keyed getblock directly"
    );
    assert_eq!(
        server.requests_for("getblock")?[0].params,
        json!(["100", 1])
    );
    Ok(())
}

#[tokio::test]
async fn fetch_chain_checkpoint_defaults_missing_tree_pools_to_zero() -> eyre::Result<()> {
    let block_hash_hex = "010101010101010101010101010101010101010101010101010101010101010f";
    let server = JsonRpcTestServer::start([method("getblock").reply(RpcReply::result(json!({
        "height": 100,
        "hash": block_hash_hex,
        "trees": {},
    })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let checkpoint = source.fetch_chain_checkpoint(BlockHeight::new(100)).await?;

    assert_eq!(checkpoint.height, BlockHeight::new(100));
    assert_eq!(checkpoint.hash, decode_rpc_block_hash(block_hash_hex)?);
    assert_eq!(checkpoint.tip_metadata, ChainTipMetadata::empty());
    Ok(())
}

#[tokio::test]
async fn fetch_chain_checkpoint_rejects_response_without_trees() -> eyre::Result<()> {
    let block_hash_hex = "010101010101010101010101010101010101010101010101010101010101010f";
    let server = JsonRpcTestServer::start([method("getblock").reply(RpcReply::result(json!({
        "height": 100,
        "hash": block_hash_hex,
    })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let outcome = source.fetch_chain_checkpoint(BlockHeight::new(100)).await;

    assert!(matches!(
        outcome,
        Err(SourceError::SourceProtocolMismatch { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn fetch_network_upgrade_activations_parses_getblockchaininfo_upgrades() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "upgrades": {
                "76b809bb": {
                    "name": "Sapling",
                    "activationheight": 280_000,
                    "status": "active"
                },
                "c2d6d0b4": {
                    "name": "NU5",
                    "activationheight": 1_842_420,
                    "status": "active"
                }
            }
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashTestnet,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let activations = source.fetch_network_upgrade_activations().await?;

    assert_eq!(activations.network(), Network::ZcashTestnet);
    assert_eq!(
        activations.activation_height_by_name("Sapling"),
        Some(zinder_core::BlockHeight::new(280_000))
    );
    assert_eq!(
        activations.activation_height_by_name("NU5"),
        Some(zinder_core::BlockHeight::new(1_842_420))
    );
    assert_eq!(
        activations
            .activation_height_by_branch_id(zinder_core::ConsensusBranchId::new(0x76b8_09bb)),
        Some(zinder_core::BlockHeight::new(280_000))
    );
    assert_eq!(
        activations.consensus_branch_id_at(zinder_core::BlockHeight::new(2_000_000)),
        zinder_core::ConsensusBranchId::new(0xc2d6_d0b4)
    );
    let earliest = activations
        .earliest_wallet_servable_activation()
        .ok_or_else(|| eyre::eyre!("Sapling and NU5 must yield an earliest activation"))?;
    assert_eq!(earliest.name, "Sapling");
    assert_eq!(
        earliest.activation_height,
        zinder_core::BlockHeight::new(280_000)
    );
    Ok(())
}

#[tokio::test]
async fn fetch_network_upgrade_activations_activates_nu6_2() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "upgrades": {
                "c8e71055": {
                    "name": "NU6",
                    "activationheight": 2_976_000,
                    "status": "active"
                },
                "5437f330": {
                    "name": "NU6.2",
                    "activationheight": 3_146_400,
                    "status": "active"
                }
            }
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashTestnet,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let activations = source.fetch_network_upgrade_activations().await?;

    assert_eq!(
        activations.activation_height_by_name("NU6.2"),
        Some(zinder_core::BlockHeight::new(3_146_400))
    );
    assert_eq!(
        activations
            .activation_height_by_branch_id(zinder_core::ConsensusBranchId::new(0x5437_f330)),
        Some(zinder_core::BlockHeight::new(3_146_400))
    );
    assert_eq!(
        activations.consensus_branch_id_at(zinder_core::BlockHeight::new(3_146_400)),
        zinder_core::ConsensusBranchId::new(0x5437_f330)
    );
    assert_eq!(
        activations.consensus_branch_id_at(zinder_core::BlockHeight::new(3_146_399)),
        zinder_core::ConsensusBranchId::new(0xc8e7_1055)
    );
    Ok(())
}

#[tokio::test]
async fn fetch_chain_value_pools_at_tip_preserves_upstream_pool_entries() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 1_234_567,
            "valuePools": [
                {
                    "id": "transparent",
                    "monitored": true,
                    "chainValueZat": 1_000_000
                },
                {
                    "id": "sapling",
                    "monitored": true,
                    "chainValueZat": 2_000_000
                },
                {
                    "id": "lockbox",
                    "monitored": false
                }
            ]
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let value_pools = source.fetch_chain_value_pools_at_tip().await?;

    assert_eq!(value_pools.tip_height, BlockHeight::new(1_234_567));
    assert_eq!(value_pools.pools.len(), 3);
    assert_eq!(value_pools.pools[0].id, "transparent");
    assert!(value_pools.pools[0].monitored);
    assert_eq!(value_pools.pools[0].chain_value_zat, Some(1_000_000));
    assert_eq!(value_pools.pools[1].id, "sapling");
    assert!(value_pools.pools[1].monitored);
    assert_eq!(value_pools.pools[1].chain_value_zat, Some(2_000_000));
    assert_eq!(value_pools.pools[2].id, "lockbox");
    assert!(!value_pools.pools[2].monitored);
    assert_eq!(value_pools.pools[2].chain_value_zat, None);
    Ok(())
}

#[tokio::test]
async fn fetch_chain_value_pools_at_tip_requires_value_pools_field() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 1_234_567
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let outcome = source.fetch_chain_value_pools_at_tip().await;

    assert!(matches!(
        outcome,
        Err(SourceError::NodeCapabilityMissing {
            capability: NodeCapability::ChainValuePools
        })
    ));
    Ok(())
}

fn fixture_block() -> eyre::Result<Value> {
    serde_json::from_str(include_str!(
        "../../../../services/zinder-ingest/tests/fixtures/z3-regtest-block-1.json"
    ))
    .map_err(|error| eyre!("failed to parse fixture block: {error}"))
}

fn string_field<'fixture>(
    fixture: &'fixture Value,
    field_name: &'static str,
) -> eyre::Result<&'fixture str> {
    fixture
        .get(field_name)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture field {field_name} must be a string"))
}

#[tokio::test]
async fn poll_upstream_health_falls_back_to_verification_progress() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 50,
            "estimatedheight": 1_000,
            "verificationprogress": 0.5,
            "valuePools": [],
            "upgrades": {},
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let snapshot = source.poll_upstream_health().await?;
    assert!(!snapshot.ready_for_queries);
    assert_eq!(
        snapshot.source,
        UPSTREAM_HEALTH_SOURCE_VERIFICATION_PROGRESS_FALLBACK
    );
    assert_eq!(
        snapshot.reason.as_ref(),
        UPSTREAM_HEALTH_REASON_VERIFICATION_PROGRESS_BELOW_FLOOR
    );
    assert_eq!(snapshot.upstream_committed_height, Some(50));
    assert_eq!(snapshot.upstream_estimated_height, Some(1_000));
    assert_eq!(snapshot.upstream_verification_progress, Some(0.5));
    Ok(())
}

#[tokio::test]
async fn poll_upstream_health_flags_estimated_gap_when_progress_is_above_floor() -> eyre::Result<()>
{
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 100,
            "estimatedheight": 200,
            "verificationprogress": 0.9995,
            "valuePools": [],
            "upgrades": {},
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let snapshot = source.poll_upstream_health().await?;
    assert!(!snapshot.ready_for_queries);
    assert_eq!(
        snapshot.reason.as_ref(),
        UPSTREAM_HEALTH_REASON_ESTIMATED_GAP_ABOVE_FLOOR
    );
    assert_eq!(snapshot.upstream_committed_height, Some(100));
    assert_eq!(snapshot.upstream_estimated_height, Some(200));
    Ok(())
}

#[tokio::test]
async fn poll_upstream_health_reports_ready_when_progress_and_gap_within_floors() -> eyre::Result<()>
{
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 2_500_000,
            "estimatedheight": 2_500_005,
            "verificationprogress": 0.9999,
            "valuePools": [],
            "upgrades": {},
        })))])?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?;

    let snapshot = source.poll_upstream_health().await?;
    assert!(snapshot.ready_for_queries);
    assert_eq!(
        snapshot.source,
        UPSTREAM_HEALTH_SOURCE_VERIFICATION_PROGRESS_FALLBACK
    );
    assert_eq!(snapshot.upstream_committed_height, Some(2_500_000));
    assert_eq!(snapshot.upstream_estimated_height, Some(2_500_005));
    Ok(())
}

#[tokio::test]
async fn poll_upstream_health_falls_back_when_ready_endpoint_unreachable() -> eyre::Result<()> {
    let server =
        JsonRpcTestServer::start([method("getblockchaininfo").reply(RpcReply::result(json!({
            "blocks": 50,
            "estimatedheight": 1_000,
            "verificationprogress": 0.4,
            "valuePools": [],
            "upgrades": {},
        })))])?;
    // Point the health probe at a port that nothing listens on so the
    // first call errors out and the source falls back to the JSON-RPC
    // path within the same `poll_upstream_health` invocation.
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?
    .with_health_config(Some(NodeHealthConfig::new(
        "http://127.0.0.1:1/ready".to_owned(),
        Duration::from_millis(500),
        0.999,
        10,
    )));

    let snapshot = source.poll_upstream_health().await?;
    assert!(!snapshot.ready_for_queries);
    assert_eq!(
        snapshot.source,
        UPSTREAM_HEALTH_SOURCE_VERIFICATION_PROGRESS_FALLBACK
    );
    assert_eq!(snapshot.upstream_committed_height, Some(50));
    Ok(())
}

#[tokio::test]
async fn poll_upstream_health_uses_ready_endpoint_when_configured() -> eyre::Result<()> {
    use std::net::SocketAddr;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let bound: SocketAddr = listener.local_addr()?;
    let accept_handle = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| eyre!("accept failed: {error}"))?;
        let mut request_buffer = [0_u8; 1024];
        let _bytes_read = stream
            .read(&mut request_buffer)
            .await
            .map_err(|error| eyre!("request read failed: {error}"))?;
        stream
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\n\
                  Content-Length: 18\r\n\
                  Content-Type: text/plain\r\n\
                  Connection: close\r\n\
                  \r\n\
                  insufficient peers",
            )
            .await
            .map_err(|error| eyre!("response write failed: {error}"))?;
        stream
            .shutdown()
            .await
            .map_err(|error| eyre!("stream shutdown failed: {error}"))?;
        Ok::<(), eyre::Report>(())
    });

    let json_rpc_server = JsonRpcTestServer::start(Vec::<zinder_testkit::JsonRpcStub>::new())?;
    let source = ZebraJsonRpcSource::new(
        Network::ZcashRegtest,
        json_rpc_server.url(),
        NodeAuth::None,
        Duration::from_secs(5),
    )?
    .with_health_config(Some(NodeHealthConfig::new(
        format!("http://{bound}/ready"),
        Duration::from_secs(5),
        0.999,
        10,
    )));

    let snapshot = source.poll_upstream_health().await?;
    accept_handle
        .await
        .map_err(|error| eyre!("ready endpoint task panicked: {error}"))??;

    assert!(!snapshot.ready_for_queries);
    assert_eq!(snapshot.source, "zebra_ready_endpoint");
    assert_eq!(snapshot.reason.as_ref(), "insufficient peers");
    Ok(())
}
