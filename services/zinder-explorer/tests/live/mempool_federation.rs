//! Live federation tests for [`ExplorerQuery::MempoolSummary`] and
//! [`ExplorerQuery::MempoolActivity`].
//!
//! Both handlers compose `WalletQuery.MempoolSnapshot` at request time
//! and parse every entry via `zinder_source::parse_transaction_public_facts`.
//! The tests stand up the wallet plane in process, drive a known
//! mempool state via the upstream node (or accept an empty mempool on
//! testnet/mainnet), and assert the wire shape, freshness envelope, and
//! pagination contract.

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
use zinder_proto::capabilities::{EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_SUMMARY_V1};
use zinder_proto::v1::explorer::{
    MempoolActivityRequest, MempoolActivityResponse, MempoolSummaryRequest, MempoolSummaryResponse,
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
async fn mempool_summary_and_activity_return_freshness_envelope() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = MempoolFixture::open(&env).await?;
    let summary = fixture.summary().await?;
    assert_summary_freshness(&summary)?;
    let activity = fixture.activity(0).await?;
    assert_activity_freshness(&activity)?;

    tracing::info!(
        target: "zinder::live",
        event = "mempool_summary_and_activity_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        transaction_count = summary.transaction_count,
        activity_entries = activity.entries.len(),
        "explorer mempool views validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn mempool_activity_pagination_emits_unique_entries() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = MempoolFixture::open(&env).await?;
    let first_page = fixture.activity(2).await?;
    if first_page.next_cursor.is_empty() {
        // Mempool is empty or fit in the first page; nothing further to
        // assert beyond the freshness contract. The summary path proves
        // the snapshot was readable.
        fixture.shutdown().await;
        return Ok(());
    }

    let second_page = fixture
        .activity_with_cursor(2, first_page.next_cursor)
        .await?;

    let first_ids: Vec<_> = first_page
        .entries
        .iter()
        .map(|entry| entry.transaction_id.clone())
        .collect();
    for entry in &second_page.entries {
        assert!(
            !first_ids.contains(&entry.transaction_id),
            "MempoolActivity pagination must not repeat entry {}",
            hex::encode(&entry.transaction_id),
        );
    }

    fixture.shutdown().await;
    Ok(())
}

struct MempoolFixture {
    network: Network,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
}

impl MempoolFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store) = bulk_catchup_store(env).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
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

    async fn summary(&self) -> Result<MempoolSummaryResponse> {
        let response = ExplorerQueryService::mempool_summary(
            &self.explorer_adapter,
            Request::new(MempoolSummaryRequest { at_epoch: None }),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn activity(&self, max_entries: u32) -> Result<MempoolActivityResponse> {
        self.activity_with_cursor(max_entries, Vec::new()).await
    }

    async fn activity_with_cursor(
        &self,
        max_entries: u32,
        from_cursor: Vec<u8>,
    ) -> Result<MempoolActivityResponse> {
        let response = ExplorerQueryService::mempool_activity(
            &self.explorer_adapter,
            Request::new(MempoolActivityRequest {
                max_entries,
                from_cursor,
            }),
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

fn assert_summary_freshness(response: &MempoolSummaryResponse) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("MempoolSummary response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_MEMPOOL_SUMMARY_V1);
    assert!(
        freshness
            .chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
            .is_some(),
        "summary freshness must carry a chain epoch",
    );
    // When the snapshot is empty the totals are zero and the privacy
    // distribution has no entries; both states are valid.
    if response.transaction_count > 0 {
        assert!(
            !response.privacy_shape_distribution.is_empty(),
            "non-empty mempool must populate the privacy-shape distribution",
        );
    }
    Ok(())
}

fn assert_activity_freshness(response: &MempoolActivityResponse) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("MempoolActivity response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_MEMPOOL_ACTIVITY_V1);
    let mut prior_first_seen: Option<u64> = None;
    for entry in &response.entries {
        assert_eq!(
            entry.transaction_id.len(),
            32,
            "transaction_id must be 32 bytes (internal byte order)",
        );
        if let Some(prior) = prior_first_seen {
            assert!(
                entry.first_seen_unix_millis <= prior,
                "MempoolActivity must be ordered newest-first",
            );
        }
        prior_first_seen = Some(entry.first_seen_unix_millis);
    }
    Ok(())
}

async fn bulk_catchup_store(env: &LiveTestEnv) -> Result<(TempDir, PrimaryChainStore)> {
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
    Ok((tempdir, store))
}

async fn serve_wallet_query_grpc(
    wallet_query: WalletQuery<PrimaryChainStore>,
    ingest_control_endpoint: String,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        ServerInfoSettings::default(),
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
