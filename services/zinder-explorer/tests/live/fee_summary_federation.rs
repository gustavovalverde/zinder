//! Live federation tests for [`ExplorerQuery::FeeSummary`].
//!
//! The handler reads typed block-summary facts from the derive store and
//! aggregates per-transaction ZIP-317 conventional fee floors via
//! `zinder_core::TransactionComponentCounts::zip317_conventional_fee_zat`.
//! The test exercises the full pipeline against a real upstream node:
//! bulk catch up a window, ask the explorer for a fee summary over the
//! window, and assert the freshness envelope plus the structural
//! invariants the wire shape promises (`block_count`,
//! `transaction_count`, min/max bounds, total ≥ count × floor).

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
use zinder_explorer::{
    DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, run_bulk_catchup};
use zinder_proto::capabilities::EXPLORER_FEE_SUMMARY_V1;
use zinder_proto::v1::explorer::{
    FeeSummaryRequest, FeeSummaryResponse,
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

/// ZIP-317 conventional-fee minimum: `MARGINAL_FEE × GRACE_ACTIONS`.
/// Every non-coinbase transaction's `zip317_conventional_fee_zat` is at
/// least this floor.
const MIN_ZIP317_FLOOR_ZAT: u64 = 5_000 * 2;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fee_summary_aggregates_zip317_floors_across_window() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = FeeSummaryFixture::open(&env).await?;
    let tip = fixture.sample_block_height.value();
    let start = tip.saturating_sub(9);
    let response = fixture.fee_summary(start, tip).await?;
    assert_fee_summary_shape(&response, tip - start + 1)?;

    tracing::info!(
        target: "zinder::live",
        event = "fee_summary_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        block_count = response.block_count,
        transaction_count = response.transaction_count,
        total_zat = response.total_zip317_conventional_fee_zat,
        "explorer fee summary validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn fee_summary_rejects_inverted_and_oversized_ranges() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = FeeSummaryFixture::open(&env).await?;
    let tip = fixture.sample_block_height.value();
    let inverted = fixture.fee_summary(tip, tip.saturating_sub(1)).await;
    assert!(
        matches!(inverted, Err(status) if status.code() == tonic::Code::InvalidArgument),
        "inverted range must return InvalidArgument",
    );
    let oversized = fixture.fee_summary(0, 1024).await;
    assert!(
        matches!(oversized, Err(status) if status.code() == tonic::Code::InvalidArgument),
        "range > 256 blocks must return InvalidArgument",
    );
    fixture.shutdown().await;
    Ok(())
}

struct FeeSummaryFixture {
    network: Network,
    sample_block_height: BlockHeight,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
}

impl FeeSummaryFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, tip_height) = bulk_catchup_store(env).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new()).await?;
        let (wallet_grpc_addr, wallet_server_handle) = serve_wallet_query_grpc(
            wallet_query,
            network,
            format!("http://{ingest_control_addr}"),
        )
        .await?;
        let wallet_endpoint = format!("http://{wallet_grpc_addr}");
        let derive_store = DeriveStore::open_secondary(
            DeriveStore::path_for_canonical(&store_tempdir.path().join("zinder-store")),
            store_tempdir
                .path()
                .join("zinder-derive-secondary-explorer"),
            DeriveStoreOptions {
                sync_writes: false,
                consumer_column_families: DeriveStore::bundled_consumer_column_families(),
                tuning: zinder_store::StorageTuning::for_local_tests(),
            },
        )?;
        derive_store.try_catch_up()?;

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_derive_store(derive_store)
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            sample_block_height: tip_height,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            _store_tempdir: store_tempdir,
        })
    }

    async fn fee_summary(
        &self,
        start_height: u32,
        end_height: u32,
    ) -> std::result::Result<FeeSummaryResponse, tonic::Status> {
        let response = ExplorerQueryService::fee_summary(
            &self.explorer_adapter,
            Request::new(FeeSummaryRequest {
                start_height,
                end_height,
            }),
        )
        .await?;
        Ok(response.into_inner())
    }

    async fn shutdown(&mut self) {
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
    }
}

fn assert_fee_summary_shape(response: &FeeSummaryResponse, requested_blocks: u32) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("FeeSummary response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_FEE_SUMMARY_V1);
    assert!(
        freshness.chain_epoch.is_some(),
        "fee summary freshness must carry a chain epoch",
    );
    assert!(
        response.block_count <= requested_blocks,
        "block_count {} cannot exceed requested span {requested_blocks}",
        response.block_count,
    );
    if response.transaction_count == 0 {
        assert_eq!(response.total_zip317_conventional_fee_zat, 0);
        assert_eq!(response.min_zip317_conventional_fee_zat, 0);
        assert_eq!(response.max_zip317_conventional_fee_zat, 0);
        return Ok(());
    }
    assert!(
        response.min_zip317_conventional_fee_zat >= MIN_ZIP317_FLOOR_ZAT,
        "ZIP-317 fee floor is {MIN_ZIP317_FLOOR_ZAT} zat; min was {}",
        response.min_zip317_conventional_fee_zat,
    );
    assert!(
        response.max_zip317_conventional_fee_zat >= response.min_zip317_conventional_fee_zat,
        "max {} must be >= min {}",
        response.max_zip317_conventional_fee_zat,
        response.min_zip317_conventional_fee_zat,
    );
    let count_u64 = u64::from(response.transaction_count);
    assert!(
        response.total_zip317_conventional_fee_zat
            >= count_u64.saturating_mul(response.min_zip317_conventional_fee_zat),
        "total {} must be at least count × min ({} × {})",
        response.total_zip317_conventional_fee_zat,
        count_u64,
        response.min_zip317_conventional_fee_zat,
    );
    assert!(
        response.total_zip317_conventional_fee_zat
            <= count_u64.saturating_mul(response.max_zip317_conventional_fee_zat),
        "total {} must be at most count × max ({} × {})",
        response.total_zip317_conventional_fee_zat,
        count_u64,
        response.max_zip317_conventional_fee_zat,
    );
    Ok(())
}

async fn bulk_catchup_store(
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
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    bulk_catchup_config.checkpoint = Some(checkpoint);
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, tip_height))
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
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let adapter = IngestControlGrpcAdapter::new(network, store).with_mempool(mempool_index);
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
