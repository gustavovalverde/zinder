//! Shared helpers for `zinder-ingest`'s live tests under `tests/live/`.
//!
//! These helpers turn a [`LiveTestEnv`] resolved from the unified env-var
//! schema into the ingest-specific config types (`BackfillConfig`,
//! `TipFollowConfig`) and run the cross-cutting wallet-API assertions every
//! live backfill needs.

#![allow(
    dead_code,
    reason = "Each live test file consumes only a subset of the common helpers."
)]

use std::{num::NonZeroU32, path::Path, process::Command, time::Duration};

use eyre::{Result, eyre};
use prost::Message;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codegen::tokio_stream::StreamExt;
use tonic::{Request, transport::Server};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::{
    BlockHeight, BlockHeightRange, Network, ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange,
};
use zinder_ingest::{
    BackfillConfig, DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, NodeSourceKind, TipFollowConfig,
};
use zinder_proto::{
    compat::lightwalletd::{
        self, CompactBlock as LightwalletdCompactBlock,
        compact_tx_streamer_server::CompactTxStreamer as LightwalletdCompactTxStreamer,
    },
    v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService},
};
use zinder_query::{
    ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter, latest_block_response,
    latest_tree_state_response, subtree_roots_response, tree_state_response,
};
use zinder_source::{NodeSource, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::PrimaryChainStore;
use zinder_testkit::live::LiveTestEnv;

/// Builds a `BackfillConfig` from a resolved live-test env plus per-test
/// runtime knobs.
#[allow(
    clippy::too_many_arguments,
    reason = "Test helper mirrors the resolved BackfillConfig field set."
)]
pub(crate) fn live_backfill_config(
    env: &LiveTestEnv,
    storage_path: &Path,
    from_height: BlockHeight,
    to_height: BlockHeight,
    commit_batch_blocks: NonZeroU32,
    allow_near_tip_finalize: bool,
) -> BackfillConfig {
    BackfillConfig {
        node: env.target.clone(),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: storage_path.to_owned(),
        from_height,
        to_height,
        commit_batch_blocks,
        allow_near_tip_finalize,
        checkpoint: None,
    }
}

/// Builds a `TipFollowConfig` from a resolved live-test env.
pub(crate) fn live_tip_follow_config(
    env: &LiveTestEnv,
    storage_path: &Path,
    reorg_window_blocks: u32,
    commit_batch_blocks: NonZeroU32,
    poll_interval: Duration,
) -> TipFollowConfig {
    TipFollowConfig {
        node: env.target.clone(),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: storage_path.to_owned(),
        reorg_window_blocks,
        commit_batch_blocks,
        poll_interval,
        lag_threshold_blocks: DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
    }
}

/// Builds a `ZebraJsonRpcSource` from a resolved backfill config.
pub(crate) fn zebra_source_from_backfill(
    backfill_config: &BackfillConfig,
) -> Result<ZebraJsonRpcSource> {
    match backfill_config.node_source {
        NodeSourceKind::ZebraJsonRpc => Ok(ZebraJsonRpcSource::with_options(
            backfill_config.node.network,
            &backfill_config.node.json_rpc_addr,
            backfill_config.node.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: backfill_config.node.request_timeout,
                max_response_bytes: backfill_config.node.max_response_bytes,
            },
        )?),
    }
}

/// Builds a `ZebraJsonRpcSource` from a resolved tip-follow config.
pub(crate) fn zebra_source_from_tip_follow(
    tip_follow_config: &TipFollowConfig,
) -> Result<ZebraJsonRpcSource> {
    match tip_follow_config.node_source {
        NodeSourceKind::ZebraJsonRpc => Ok(ZebraJsonRpcSource::with_options(
            tip_follow_config.node.network,
            &tip_follow_config.node.json_rpc_addr,
            tip_follow_config.node.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: tip_follow_config.node.request_timeout,
                max_response_bytes: tip_follow_config.node.max_response_bytes,
            },
        )?),
    }
}

/// Probes the upstream node tip via a fresh source.
pub(crate) async fn fetch_live_tip_height(env: &LiveTestEnv) -> Result<BlockHeight> {
    let probe_source = ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?;
    Ok(NodeSource::tip_id(&probe_source).await?.height)
}

/// Asserts that the backfilled store answers every wallet read RPC consistently
/// for `[start_height..=end_height]` against the visible chain epoch.
pub(crate) async fn assert_native_wallet_read_responses(
    store: &PrimaryChainStore,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let wallet_query = WalletQuery::new(store.clone(), ());
    assert_native_compact_block_range_chunks(&wallet_query, network, start_height, end_height)
        .await?;
    assert_native_latest_block_response(&wallet_query, network, end_height).await?;
    assert_native_tree_state_response(&wallet_query, network, end_height).await?;
    assert_native_latest_tree_state_response(&wallet_query, network, end_height).await?;
    assert_native_subtree_roots_response(&wallet_query, network).await?;
    assert_native_wallet_grpc_responses(store, network, start_height, end_height).await?;
    assert_lightwalletd_compat_responses(store, network, start_height, end_height).await?;
    Ok(())
}

/// Asserts that the lightwalletd compat shim rejects an obviously malformed
/// transaction with a non-zero error code.
pub(crate) async fn assert_lightwalletd_send_transaction_classifies_invalid(
    store: &PrimaryChainStore,
    backfill_config: &BackfillConfig,
) -> Result<()> {
    let source = zebra_source_from_backfill(backfill_config)?;
    let wallet_query = WalletQuery::new(store.clone(), source);
    let grpc_adapter = LightwalletdGrpcAdapter::new(wallet_query);

    let response = LightwalletdCompactTxStreamer::send_transaction(
        &grpc_adapter,
        Request::new(lightwalletd::RawTransaction {
            data: vec![0xff, 0xff, 0xff, 0xff],
            height: 0,
        }),
    )
    .await?
    .into_inner();

    assert_ne!(
        response.error_code, 0,
        "node must reject malformed transaction bytes"
    );
    assert!(
        !response.error_message.is_empty(),
        "node must surface a rejection message"
    );
    Ok(())
}

async fn assert_lightwalletd_compat_responses(
    store: &PrimaryChainStore,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let wallet_query = WalletQuery::new(store.clone(), ());
    let grpc_adapter = LightwalletdGrpcAdapter::new(wallet_query);

    assert_lightwalletd_trait_responses(&grpc_adapter, network, start_height, end_height).await?;
    assert_generated_lightwalletd_client_responses(store, network, start_height, end_height)
        .await?;
    Ok(())
}

async fn assert_lightwalletd_trait_responses(
    grpc_adapter: &LightwalletdGrpcAdapter<WalletQuery<PrimaryChainStore>>,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let latest_block = LightwalletdCompactTxStreamer::get_latest_block(
        grpc_adapter,
        Request::new(lightwalletd::ChainSpec {}),
    )
    .await?
    .into_inner();
    let block_range = LightwalletdCompactTxStreamer::get_block_range(
        grpc_adapter,
        Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: u64::from(start_height),
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: u64::from(end_height),
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let compact_blocks = collect_lightwalletd_stream(block_range).await?;
    let latest_tree_state = LightwalletdCompactTxStreamer::get_latest_tree_state(
        grpc_adapter,
        Request::new(lightwalletd::Empty {}),
    )
    .await?
    .into_inner();
    let lightd_info = LightwalletdCompactTxStreamer::get_lightd_info(
        grpc_adapter,
        Request::new(lightwalletd::Empty {}),
    )
    .await?
    .into_inner();

    assert_eq!(latest_block.height, u64::from(end_height));
    assert_eq!(
        compact_blocks.len(),
        usize::try_from(end_height - start_height + 1)?
    );
    assert_eq!(
        compact_blocks
            .first()
            .ok_or_else(|| eyre!("lightwalletd range response missing compact block"))?
            .height,
        u64::from(start_height)
    );
    assert_eq!(latest_tree_state.network, lightwalletd_chain_name(network)?);
    assert_eq!(latest_tree_state.height, u64::from(end_height));
    assert_eq!(lightd_info.vendor, "Zinder");
    assert_eq!(lightd_info.chain_name, lightwalletd_chain_name(network)?);
    assert_eq!(lightd_info.block_height, u64::from(end_height));
    assert_eq!(
        lightd_info.lightwallet_protocol_version,
        lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT
    );

    for protocol in [
        lightwalletd::ShieldedProtocol::Sapling,
        lightwalletd::ShieldedProtocol::Orchard,
    ] {
        let subtree_roots = LightwalletdCompactTxStreamer::get_subtree_roots(
            grpc_adapter,
            Request::new(lightwalletd::GetSubtreeRootsArg {
                start_index: 0,
                shielded_protocol: protocol as i32,
                max_entries: 1,
            }),
        )
        .await?
        .into_inner();
        let subtree_roots = collect_lightwalletd_stream(subtree_roots).await?;
        assert!(subtree_roots.is_empty());
    }

    Ok(())
}

async fn assert_generated_lightwalletd_client_responses(
    store: &PrimaryChainStore,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(store.clone(), ())).into_server();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let mut client = lightwalletd::compact_tx_streamer_client::CompactTxStreamerClient::connect(
        format!("http://{server_addr}"),
    )
    .await?;
    let latest_block = client
        .get_latest_block(lightwalletd::ChainSpec {})
        .await?
        .into_inner();
    let mut compact_block_stream = client
        .get_block_range(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: u64::from(start_height),
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: u64::from(end_height),
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        })
        .await?
        .into_inner();
    let latest_tree_state = client
        .get_latest_tree_state(lightwalletd::Empty {})
        .await?
        .into_inner();

    let mut compact_blocks = Vec::new();
    while let Some(compact_block) = compact_block_stream.message().await? {
        compact_blocks.push(compact_block);
    }

    assert_eq!(latest_block.height, u64::from(end_height));
    assert_eq!(
        compact_blocks.len(),
        usize::try_from(end_height - start_height + 1)?
    );
    assert_eq!(latest_tree_state.network, lightwalletd_chain_name(network)?);
    assert_eq!(latest_tree_state.height, u64::from(end_height));
    assert!(!latest_tree_state.hash.is_empty());

    for (offset, compact_block) in compact_blocks.iter().enumerate() {
        let offset = u64::try_from(offset)?;
        assert_eq!(compact_block.height, u64::from(start_height) + offset);
        assert_eq!(compact_block.hash.len(), 32);
    }

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "Live native gRPC acceptance keeps the read-sync RPC matrix together."
)]
async fn assert_native_wallet_grpc_responses(
    store: &PrimaryChainStore,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let wallet_query = WalletQuery::new(store.clone(), ());
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let latest_block = WalletQueryService::latest_block(
        &grpc_adapter,
        Request::new(wallet::LatestBlockRequest { at_epoch: None }),
    )
    .await?
    .into_inner();
    let mut compact_block_stream = WalletQueryService::compact_block_range(
        &grpc_adapter,
        Request::new(wallet::CompactBlockRangeRequest {
            start_height,
            end_height,
            at_epoch: None,
        }),
    )
    .await?
    .into_inner();
    let mut compact_block_range = Vec::new();
    while let Some(compact_block_chunk) = compact_block_stream.next().await {
        compact_block_range.push(compact_block_chunk?);
    }
    let tree_state = WalletQueryService::tree_state(
        &grpc_adapter,
        Request::new(wallet::TreeStateRequest {
            height: end_height,
            at_epoch: None,
        }),
    )
    .await?
    .into_inner();
    let latest_tree_state = WalletQueryService::latest_tree_state(
        &grpc_adapter,
        Request::new(wallet::LatestTreeStateRequest { at_epoch: None }),
    )
    .await?
    .into_inner();

    assert_native_grpc_response_epoch(&latest_block, network, end_height)?;
    for compact_block_chunk in &compact_block_range {
        assert_native_grpc_response_epoch(compact_block_chunk, network, end_height)?;
    }
    assert_native_grpc_response_epoch(&tree_state, network, end_height)?;
    assert_native_grpc_response_epoch(&latest_tree_state, network, end_height)?;

    assert_eq!(
        latest_block
            .latest_block
            .ok_or_else(|| eyre!("native gRPC latest block missing metadata"))?
            .height,
        end_height
    );
    assert_eq!(
        compact_block_range.len(),
        usize::try_from(end_height - start_height + 1)?
    );
    for (offset, compact_block_chunk) in compact_block_range.iter().enumerate() {
        let offset = u32::try_from(offset)?;
        let compact_block = compact_block_chunk
            .compact_block
            .as_ref()
            .ok_or_else(|| eyre!("native gRPC compact-block chunk missing compact block"))?;
        assert_eq!(compact_block.height, start_height + offset);
        assert_eq!(compact_block.block_hash.len(), 32);
        assert!(!compact_block.payload_bytes.is_empty());
    }
    assert_eq!(tree_state.height, end_height);
    assert!(!tree_state.payload_bytes.is_empty());
    assert_eq!(latest_tree_state.height, end_height);
    assert!(!latest_tree_state.payload_bytes.is_empty());

    for protocol in [
        wallet::ShieldedProtocol::Sapling,
        wallet::ShieldedProtocol::Orchard,
    ] {
        let subtree_roots = WalletQueryService::subtree_roots(
            &grpc_adapter,
            Request::new(wallet::SubtreeRootsRequest {
                shielded_protocol: protocol as i32,
                start_index: 0,
                max_entries: 8,
                at_epoch: None,
            }),
        )
        .await?
        .into_inner();
        assert_native_grpc_response_epoch(&subtree_roots, network, end_height)?;
        assert_eq!(subtree_roots.start_index, 0);
        assert!(subtree_roots.subtree_roots.is_empty());
    }

    Ok(())
}

async fn collect_lightwalletd_stream<T, Stream>(
    mut stream: Stream,
) -> std::result::Result<Vec<T>, tonic::Status>
where
    Stream:
        tonic::codegen::tokio_stream::Stream<Item = std::result::Result<T, tonic::Status>> + Unpin,
{
    use tonic::codegen::tokio_stream::StreamExt;

    let mut values = Vec::new();
    while let Some(stream_item) = stream.next().await {
        values.push(stream_item?);
    }
    Ok(values)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive core networks must fail closed until lightwalletd mapping exists"
)]
fn lightwalletd_chain_name(network: Network) -> Result<&'static str> {
    match network {
        Network::ZcashMainnet => Ok("main"),
        Network::ZcashTestnet => Ok("test"),
        Network::ZcashRegtest => Ok("regtest"),
        _ => Err(eyre!(
            "network is not supported by lightwalletd compatibility"
        )),
    }
}

trait HasNativeGrpcChainEpoch {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch>;
}

impl HasNativeGrpcChainEpoch for wallet::LatestBlockResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasNativeGrpcChainEpoch for wallet::CompactBlockRangeChunk {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasNativeGrpcChainEpoch for wallet::TreeStateResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasNativeGrpcChainEpoch for wallet::SubtreeRootsResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

fn assert_native_grpc_response_epoch(
    response: &impl HasNativeGrpcChainEpoch,
    network: Network,
    end_height: u32,
) -> Result<()> {
    let chain_epoch = response
        .chain_epoch()
        .ok_or_else(|| eyre!("native gRPC response missing chain epoch"))?;

    assert_eq!(chain_epoch.network_name, network.name());
    assert_eq!(chain_epoch.tip_height, end_height);
    Ok(())
}

async fn assert_native_compact_block_range_chunks<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    network: Network,
    start_height: u32,
    end_height: u32,
) -> Result<()> {
    let compact_block_range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(start_height),
            BlockHeight::new(end_height),
        ))
        .await?;
    let range_chain_epoch = compact_block_range.chain_epoch;
    assert_eq!(range_chain_epoch.network, network);
    assert_eq!(range_chain_epoch.tip_height, BlockHeight::new(end_height));
    assert_eq!(
        compact_block_range.compact_blocks.len(),
        usize::try_from(end_height - start_height + 1)?
    );

    for (height, compact_block) in
        (start_height..=end_height).zip(compact_block_range.compact_blocks)
    {
        let chunk = wallet::CompactBlockRangeChunk {
            chain_epoch: Some(wallet::ChainEpoch {
                chain_epoch_id: range_chain_epoch.id.value(),
                network_name: range_chain_epoch.network.name().to_owned(),
                tip_height: range_chain_epoch.tip_height.value(),
                tip_hash: range_chain_epoch.tip_hash.as_bytes().into(),
                finalized_height: range_chain_epoch.finalized_height.value(),
                finalized_hash: range_chain_epoch.finalized_hash.as_bytes().into(),
                artifact_schema_version: u32::from(
                    range_chain_epoch.artifact_schema_version.value(),
                ),
                created_at_millis: range_chain_epoch.created_at.value(),
                sapling_commitment_tree_size: range_chain_epoch
                    .tip_metadata
                    .sapling_commitment_tree_size,
                orchard_commitment_tree_size: range_chain_epoch
                    .tip_metadata
                    .orchard_commitment_tree_size,
            }),
            compact_block: Some(wallet::CompactBlock {
                height: compact_block.height.value(),
                block_hash: compact_block.block_hash.as_bytes().into(),
                payload_bytes: compact_block.payload_bytes,
            }),
        };
        let encoded_chunk = chunk.encode_to_vec();
        let decoded_chunk = wallet::CompactBlockRangeChunk::decode(encoded_chunk.as_slice())?;
        let chunk_chain_epoch = decoded_chunk
            .chain_epoch
            .ok_or_else(|| eyre!("native compact-block chunk missing chain epoch"))?;
        let compact_block = decoded_chunk
            .compact_block
            .ok_or_else(|| eyre!("native compact-block chunk missing compact block"))?;

        assert_eq!(chunk_chain_epoch.network_name, network.name());
        assert_eq!(chunk_chain_epoch.tip_height, end_height);
        assert_eq!(compact_block.height, height);
        assert!(!compact_block.payload_bytes.is_empty());
        assert_lightwalletd_compact_block_payload(
            &compact_block.payload_bytes,
            height,
            compact_block.block_hash.as_slice(),
        )?;
    }

    Ok(())
}

fn assert_lightwalletd_compact_block_payload(
    payload_bytes: &[u8],
    expected_height: u32,
    expected_block_hash: &[u8],
) -> Result<()> {
    let compact_block = LightwalletdCompactBlock::decode(payload_bytes)?;
    let chain_metadata = compact_block
        .chain_metadata
        .ok_or_else(|| eyre!("lightwalletd compact block missing chain metadata"))?;

    assert_eq!(compact_block.proto_version, 1);
    assert_eq!(compact_block.height, u64::from(expected_height));
    assert_eq!(compact_block.hash, expected_block_hash);
    assert_eq!(compact_block.hash.len(), 32);
    assert_eq!(compact_block.prev_hash.len(), 32);
    assert!(!compact_block.vtx.is_empty());
    assert!(compact_block.vtx.iter().any(|transaction| {
        !transaction.spends.is_empty()
            || !transaction.outputs.is_empty()
            || !transaction.actions.is_empty()
            || !transaction.vin.is_empty()
            || !transaction.vout.is_empty()
    }));
    assert_eq!(chain_metadata.sapling_commitment_tree_size, 0);
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 0);
    Ok(())
}

async fn assert_native_latest_block_response<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    network: Network,
    end_height: u32,
) -> Result<()> {
    let response = latest_block_response(wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::LatestBlockResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_epoch
        .ok_or_else(|| eyre!("native response missing chain epoch"))?;
    let latest_block = decoded_response
        .latest_block
        .ok_or_else(|| eyre!("native response missing latest block"))?;

    assert_eq!(response_chain_epoch.network_name, network.name());
    assert_eq!(response_chain_epoch.tip_height, end_height);
    assert_eq!(latest_block.height, end_height);
    assert!(!latest_block.block_hash.is_empty());
    Ok(())
}

async fn assert_native_tree_state_response<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    network: Network,
    end_height: u32,
) -> Result<()> {
    let response = tree_state_response(wallet_query, BlockHeight::new(end_height), None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_epoch
        .ok_or_else(|| eyre!("native response missing chain epoch"))?;

    assert_eq!(response_chain_epoch.network_name, network.name());
    assert_eq!(response_chain_epoch.tip_height, end_height);
    assert_eq!(decoded_response.height, end_height);
    assert!(!decoded_response.block_hash.is_empty());
    assert!(!decoded_response.payload_bytes.is_empty());
    Ok(())
}

async fn assert_native_latest_tree_state_response<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    network: Network,
    end_height: u32,
) -> Result<()> {
    let response = latest_tree_state_response(wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_epoch
        .ok_or_else(|| eyre!("native latest tree-state response missing chain epoch"))?;

    assert_eq!(response_chain_epoch.network_name, network.name());
    assert_eq!(response_chain_epoch.tip_height, end_height);
    assert_eq!(decoded_response.height, end_height);
    assert!(!decoded_response.block_hash.is_empty());
    assert!(!decoded_response.payload_bytes.is_empty());
    Ok(())
}

async fn assert_native_subtree_roots_response<QueryApi: WalletQueryApi>(
    wallet_query: &QueryApi,
    network: Network,
) -> Result<()> {
    for protocol in [ShieldedProtocol::Sapling, ShieldedProtocol::Orchard] {
        let response = subtree_roots_response(
            wallet_query,
            SubtreeRootRange::new(
                protocol,
                SubtreeRootIndex::new(0),
                NonZeroU32::new(8).ok_or_else(|| eyre!("invalid max entries"))?,
            ),
            None,
        )
        .await?;
        let encoded_response = response.encode_to_vec();
        let decoded_response = wallet::SubtreeRootsResponse::decode(encoded_response.as_slice())?;
        let response_chain_epoch = decoded_response
            .chain_epoch
            .ok_or_else(|| eyre!("native subtree-roots response missing chain epoch"))?;

        assert_eq!(response_chain_epoch.network_name, network.name());
        assert_eq!(decoded_response.start_index, 0);
        assert!(decoded_response.subtree_roots.is_empty());
    }
    Ok(())
}

/// Returns a `Command` that runs the in-tree `zinder-ingest` binary with a
/// fully cleared environment, ready for env-var injection.
#[must_use]
pub(crate) fn zinder_ingest_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-ingest"));
    command.env_clear();
    command
}

/// Builder for a TOML config file used by the CLI live tests.
pub(crate) struct BackfillConfigToml<'fields> {
    pub(crate) network_name: &'fields str,
    pub(crate) json_rpc_addr: &'fields str,
    pub(crate) node_auth_username: &'fields str,
    pub(crate) node_auth_password: &'fields str,
    pub(crate) storage_path: &'fields Path,
    pub(crate) from_height: u32,
    pub(crate) to_height: u32,
    pub(crate) request_timeout_secs: u64,
    pub(crate) allow_near_tip_finalize: bool,
}

/// Renders a `BackfillConfigToml` into the TOML shape `zinder-ingest` accepts.
pub(crate) fn backfill_config_toml(config_toml: &BackfillConfigToml<'_>) -> Result<String> {
    Ok(format!(
        r#"[network]
name = "{}"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "{}"
request_timeout_secs = {}

[node.auth]
method = "basic"
username = "{}"
password = "{}"

[storage]
path = "{}"

[ingest]
commit_batch_blocks = 1000

[backfill]
from_height = {}
to_height = {}
allow_near_tip_finalize = {}
"#,
        config_toml.network_name,
        config_toml.json_rpc_addr,
        config_toml.request_timeout_secs,
        config_toml.node_auth_username,
        config_toml.node_auth_password,
        path_str(config_toml.storage_path)?,
        config_toml.from_height,
        config_toml.to_height,
        config_toml.allow_near_tip_finalize
    ))
}

/// Returns the node's basic-auth `(username, password)` from a resolved
/// live-test env, or an error if the env did not select Basic.
pub(crate) fn basic_auth_credentials(env: &LiveTestEnv) -> Result<(&str, &str)> {
    use secrecy::ExposeSecret;
    use zinder_source::NodeAuth;
    match &env.target.node_auth {
        NodeAuth::Basic { username, password } => Ok((username, password.expose_secret())),
        NodeAuth::None | NodeAuth::Cookie { .. } => Err(eyre!(
            "live CLI test requires basic auth; set ZINDER_NODE__AUTH__METHOD=basic"
        )),
    }
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("path is not valid UTF-8: {}", path.display()))
}
