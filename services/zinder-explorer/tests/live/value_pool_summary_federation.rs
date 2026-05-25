//! Live federation test for [`ExplorerQuery::ValuePoolSummary`].
//!
//! Exercises the 4-layer plumbing end-to-end against a real upstream
//! node: explorer handler → `WalletQuery.ChainValuePoolsAtTip` → ingest
//! control proxy → `NodeSource::fetch_chain_value_pools_at_tip` →
//! Zebra's `getblockchaininfo`. The test stands up the wallet + ingest
//! stack in-process with a real `ZebraJsonRpcSource` wired into the
//! ingest control adapter so the source-boundary read traverses every
//! layer.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Request;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, Network};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, run_bulk_catchup};
use zinder_proto::capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1;
use zinder_proto::v1::explorer::{
    ValuePoolSummaryRequest, ValuePoolSummaryResponse,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    zebra_source_from_bulk_catchup,
};

const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn value_pool_summary_returns_upstream_pools_through_federation() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = ValuePoolSummaryFixture::open(&env).await?;
    let response = fixture.value_pool_summary().await?;
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("ValuePoolSummary response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_VALUE_POOL_SUMMARY_V1);
    assert!(
        freshness.chain_epoch.is_some(),
        "value pool summary freshness must carry a chain epoch",
    );
    assert!(
        response.tip_height > 0,
        "tip_height should be non-zero on any non-empty chain",
    );
    assert!(
        !response.pools.is_empty(),
        "value pool summary must carry at least one upstream pool entry",
    );
    let has_known_pool = response
        .pools
        .iter()
        .any(|pool| matches!(pool.id.as_str(), "transparent" | "sapling" | "orchard"));
    assert!(
        has_known_pool,
        "expected at least one of transparent / sapling / orchard in the upstream report; got {:?}",
        response
            .pools
            .iter()
            .map(|pool| pool.id.as_str())
            .collect::<Vec<_>>(),
    );

    tracing::info!(
        target: "zinder::live",
        event = "value_pool_summary_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        tip_height = response.tip_height,
        pool_count = response.pools.len(),
        "explorer value pool summary validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

struct ValuePoolSummaryFixture {
    network: Network,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
}

impl ValuePoolSummaryFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, source) = bulk_catchup_store(env).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new(), source).await?;
        let (wallet_grpc_addr, wallet_server_handle) = serve_wallet_query_grpc(
            wallet_query,
            network,
            format!("http://{ingest_control_addr}"),
        )
        .await?;
        let wallet_endpoint = format!("http://{wallet_grpc_addr}");

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            _store_tempdir: store_tempdir,
        })
    }

    async fn value_pool_summary(&self) -> Result<ValuePoolSummaryResponse> {
        let response = ExplorerQueryService::value_pool_summary(
            &self.explorer_adapter,
            Request::new(ValuePoolSummaryRequest {}),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn shutdown(&mut self) {
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
    }
}

async fn bulk_catchup_store(
    env: &LiveTestEnv,
) -> Result<(
    TempDir,
    PrimaryChainStore,
    zinder_source::ZebraJsonRpcSource,
)> {
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
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    bulk_catchup_config.checkpoint = Some(checkpoint);
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, source))
}

async fn serve_wallet_query_grpc(
    wallet_query: WalletQuery<PrimaryChainStore>,
    network: Network,
    ingest_control_endpoint: String,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let server_info = ServerInfoSettings {
        network: encode_zinder_native_chain_name(network).to_owned(),
        ..ServerInfoSettings::default()
    };
    let adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        server_info,
        ingest_control_endpoint,
    );
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

async fn serve_ingest_control_grpc(
    network: Network,
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
    source: zinder_source::ZebraJsonRpcSource,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let adapter = IngestControlGrpcAdapter::new(network, store)
        .with_mempool(mempool_index)
        .with_node_source(Arc::new(source));
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

async fn await_grpc_endpoint(addr: SocketAddr) -> Result<()> {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(eyre!("gRPC endpoint {addr} did not become reachable"))
}
