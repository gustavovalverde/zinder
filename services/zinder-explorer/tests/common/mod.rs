//! Shared helpers for `zinder-explorer`'s integration and live tests.
//!
//! Subset of `services/zinder-ingest/tests/common/mod.rs` that the
//! `services/zinder-explorer` test crate needs to bulk catch up against a live
//! upstream node and probe the federated balance read path. Duplicated
//! deliberately so the live gating contract stays colocated with the
//! consumer; a third consumer is the prompt for consolidating into
//! `zinder-testkit::live`.

#![allow(
    dead_code,
    reason = "Each test file consumes only a subset of the common helpers."
)]

use std::{
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::Path,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use eyre::{Result, eyre};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, Network, NetworkUpgradeActivations, wire::encode_zinder_native_chain_name,
};
use zinder_ingest::{
    BulkCatchupRunConfig, CanonicalPipelineLimits, NodeSourceKind, run_bulk_catchup,
};
use zinder_proto::v1::{
    ingest::{
        MempoolTransactionRequest, ServerInfoRequest, ServerInfoResponse, WriterStatusRequest,
        WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    wallet,
};
use zinder_query::{WalletEndpointMetadata, WalletQuery, WalletQueryGrpcAdapter};
use zinder_source::{NodeSource, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::PrimaryChainStore;
use zinder_testkit::live::LiveTestEnv;

const BACKFILL_DEPTH_BLOCKS: u32 = 50;

/// Builds a [`BulkCatchupRunConfig`] from a resolved live-test env plus per-test
/// runtime knobs.
#[allow(
    clippy::too_many_arguments,
    reason = "Test helper mirrors the resolved BulkCatchupRunConfig field set."
)]
pub(crate) fn live_bulk_catchup_run_config(
    env: &LiveTestEnv,
    storage_path: &Path,
    from_height: BlockHeight,
    to_height: BlockHeight,
    canonical_batch_max_blocks: NonZeroU32,
    allow_reorg_window_settlement: bool,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
) -> BulkCatchupRunConfig {
    const SOURCE_SEGMENT_MAX_BLOCKS: NonZeroU32 = NonZeroU32::MIN.saturating_add(7);
    BulkCatchupRunConfig {
        node: env.target.clone(),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: storage_path.to_owned(),
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        reorg_window_blocks: 100,
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations,
        from_height,
        to_height,
        canonical_batch_max_blocks,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .unwrap_or(NonZeroU64::MIN),
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .unwrap_or(NonZeroU32::MIN),
        pipeline_limits: CanonicalPipelineLimits {
            max_response_bytes: env.target.max_response_bytes,
            source_segment_max_blocks: SOURCE_SEGMENT_MAX_BLOCKS,
            source_segment_target_response_bytes: NonZeroU64::new(12 * 1024 * 1024)
                .unwrap_or(NonZeroU64::MIN),
            source_fetch_max_in_flight_requests: NonZeroU32::new(8).unwrap_or(NonZeroU32::MIN),
            source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
                .unwrap_or(NonZeroU64::MIN),
            block_prepare_concurrency: SOURCE_SEGMENT_MAX_BLOCKS,
            block_prepare_memory_watermark_bytes: NonZeroU64::new(128 * 1024 * 1024)
                .unwrap_or(NonZeroU64::MIN),
        },
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        flush_interval_epochs: NonZeroU32::MIN.saturating_add(4),
        upstream_tip_hint: None,
        allow_reorg_window_settlement,
        checkpoint: None,
    }
}

/// Builds a [`ZebraJsonRpcSource`] from a resolved bulk-catchup config.
pub(crate) fn zebra_source_from_bulk_catchup(
    bulk_catchup_config: &BulkCatchupRunConfig,
) -> Result<ZebraJsonRpcSource> {
    match bulk_catchup_config.node_source {
        NodeSourceKind::ZebraJsonRpc => Ok(ZebraJsonRpcSource::with_options(
            bulk_catchup_config.node.network,
            &bulk_catchup_config.node.json_rpc_addr,
            bulk_catchup_config.node.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: bulk_catchup_config.node.request_timeout,
                max_response_bytes: bulk_catchup_config.node.max_response_bytes,
                broadcast_timeout: None,
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
            broadcast_timeout: None,
        },
    )?;
    Ok(NodeSource::tip_id(&probe_source).await?.height)
}

/// Fetches the node-advertised upgrade table for live bulk-catchup derivation.
pub(crate) async fn fetch_live_network_upgrade_activations(
    env: &LiveTestEnv,
) -> Result<Arc<NetworkUpgradeActivations>> {
    let probe_source = ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
            broadcast_timeout: None,
        },
    )?;
    Ok(Arc::new(
        probe_source.fetch_network_upgrade_activations().await?,
    ))
}

/// Bulk catches up the window ending at the live upstream tip and opens the
/// resulting canonical store.
pub(crate) async fn bulk_catchup_store(
    env: &LiveTestEnv,
) -> Result<(TempDir, PrimaryChainStore, BlockHeight)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}",
            tip_height.value(),
        ));
    }
    let checkpoint_height = BlockHeight::new(tip_height.value() - BACKFILL_DEPTH_BLOCKS - 1);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let activations = fetch_live_network_upgrade_activations(env).await?;
    let mut bulk_catchup_config = live_bulk_catchup_run_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
        activations,
    );
    let source = zebra_source_from_bulk_catchup(&bulk_catchup_config)?;
    let checkpoint = source
        .fetch_chain_checkpoint(
            checkpoint_height,
            &bulk_catchup_config.network_upgrade_activations,
        )
        .await?;
    bulk_catchup_config.checkpoint = Some(checkpoint);
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;
    let store =
        PrimaryChainStore::open(&storage_path, bulk_catchup_config.canonical_store_options())?;
    Ok((tempdir, store, tip_height))
}

/// Per-test knobs for the in-process `WalletQuery` gRPC fixture.
pub(crate) struct WalletQueryServerOptions {
    /// Overrides the advertised `ServerInfo` chain name.
    pub(crate) network: Option<Network>,
    /// Routes ingest-owned reads through an in-process `IngestControl` server.
    pub(crate) ingest_control_endpoint: Option<String>,
}

/// Serves `WalletQuery` over gRPC on an ephemeral loopback port.
pub(crate) async fn serve_wallet_query_grpc(
    wallet_query: WalletQuery<PrimaryChainStore>,
    options: WalletQueryServerOptions,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let server_info = options
        .network
        .map_or_else(WalletEndpointMetadata::default, |network| {
            WalletEndpointMetadata {
                network: encode_zinder_native_chain_name(network).to_owned(),
                ..WalletEndpointMetadata::default()
            }
        });
    let adapter = match options.ingest_control_endpoint {
        Some(endpoint) => {
            WalletQueryGrpcAdapter::with_ingest_control_proxy(wallet_query, server_info, endpoint)
        }
        None => WalletQueryGrpcAdapter::new(wallet_query, server_info),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    await_grpc_endpoint(addr).await?;
    Ok((addr, handle))
}

/// Waits until `addr` accepts connections.
pub(crate) async fn await_grpc_endpoint(addr: SocketAddr) -> Result<()> {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(eyre!("gRPC endpoint {addr} did not become reachable"))
}

type TestChainEventStream =
    Pin<Box<dyn Stream<Item = Result<wallet::ChainEventEnvelope, Status>> + Send>>;
type TestMempoolEventStream =
    Pin<Box<dyn Stream<Item = Result<wallet::MempoolEventEnvelope, Status>> + Send>>;

/// Serves a deliberately small current `IngestControl` implementation for
/// Explorer federation tests.
///
/// The writer owns canonical-control composition; this fixture only supplies
/// the current private responses that Explorer reaches through `WalletQuery`.
pub(crate) async fn serve_test_ingest_control(
    value_pools_response: Option<wallet::ChainValuePoolsAtTipResponse>,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(IngestControlServer::new(TestIngestControl {
                value_pools_response,
            }))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    Ok((address, handle))
}

#[derive(Clone)]
struct TestIngestControl {
    value_pools_response: Option<wallet::ChainValuePoolsAtTipResponse>,
}

#[tonic::async_trait]
impl IngestControl for TestIngestControl {
    type VisibleChainEventsStream = TestChainEventStream;
    type MempoolEventsStream = TestMempoolEventStream;

    async fn server_info(
        &self,
        _: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        Ok(Response::new(ServerInfoResponse { server_info: None }))
    }

    async fn writer_status(
        &self,
        _: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        Err(Status::unimplemented(
            "Explorer test fixture does not expose writer status",
        ))
    }

    async fn visible_chain_events(
        &self,
        _: Request<wallet::EventStreamStart>,
    ) -> Result<Response<Self::VisibleChainEventsStream>, Status> {
        Err(Status::unimplemented(
            "Explorer test fixture does not expose visible-chain events",
        ))
    }

    async fn mempool_snapshot(
        &self,
        _: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        Ok(Response::new(wallet::MempoolSnapshotResponse {
            chain_view: Some(test_chain_view()),
            events_resume_cursor: Vec::new(),
            snapshot_age_millis: 0,
            entries: Vec::new(),
            next_cursor: Vec::new(),
            source_tip: Some(wallet::BlockTip {
                height: 1,
                hash: "01".repeat(32),
            }),
        }))
    }

    async fn mempool_transaction(
        &self,
        _: Request<MempoolTransactionRequest>,
    ) -> Result<Response<wallet::TransactionStatusResponse>, Status> {
        Err(Status::not_found("transaction is not in the test mempool"))
    }

    async fn mempool_events(
        &self,
        _: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        Err(Status::unimplemented(
            "Explorer test fixture does not expose mempool events",
        ))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        _: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        Ok(Response::new(
            wallet::TransparentMempoolOutputsByAddressResponse {
                chain_view: Some(test_chain_view()),
                outputs: Vec::new(),
            },
        ))
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        _: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        Ok(Response::new(
            wallet::TransparentMempoolSpendsByOutpointResponse {
                chain_view: Some(test_chain_view()),
                spends: Vec::new(),
            },
        ))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        _: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        Ok(Response::new(
            wallet::TransparentOutputsByOutpointResponse {
                chain_view: Some(test_chain_view()),
                entries: Vec::new(),
            },
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        self.value_pools_response
            .clone()
            .map(Response::new)
            .ok_or_else(|| {
                Status::unimplemented("Explorer test fixture does not expose source value pools")
            })
    }
}

fn test_chain_view() -> wallet::ChainView {
    wallet::ChainView {
        chain_epoch: Some(wallet::ChainEpoch {
            chain_epoch_id: 1,
            network_name: "zcash-regtest".to_owned(),
            artifact_schema_version: 1,
            created_at_millis: 0,
            visible_tip: Some(wallet::BlockTip {
                height: 1,
                hash: "01".repeat(32),
            }),
            settled_tip: Some(wallet::BlockTip {
                height: 0,
                hash: "00".repeat(32),
            }),
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        }),
        indexed_tip: None,
        upstream_tip: None,
        materialized_views: None,
    }
}
