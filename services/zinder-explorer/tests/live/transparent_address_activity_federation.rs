//! Live federation tests for [`ExplorerQuery::TransparentAddressActivity`].
//!
//! Validates the composition of the `TransparentAddressActivity`
//! derive-consumer projection against a real upstream node. The test does
//! not assume the queried address has activity; it asserts the response
//! shape (freshness envelope, ordering) without requiring a mined
//! transaction at the sampled height.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, Network};

use zinder_explorer::{
    DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
    TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY, TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME,
    TransparentAddressActivityConsumer, run_chain_events_subscriber,
};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, backfill};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1;
use zinder_proto::v1::explorer::{
    TransparentAddressActivityRequest, TransparentAddressActivityResponse,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_proto::v1::wallet::{
    AddressLookup, ChainEventStreamFamily as WireChainEventStreamFamily, ChainEventsRequest,
    address_lookup, wallet_query_client::WalletQueryClient,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_runtime::connect_authenticated_channel;
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

const BACKFILL_DEPTH_BLOCKS: u32 = 50;
const REGTEST_TRANSPARENT_ADDRESS: &str = "tmDpFafuBHKGUYmuwLsrxWJrwcnSyzEEtYx";
const MAINNET_TRANSPARENT_ADDRESS: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
const CONSUMER_CATCHUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONSUMER_CATCHUP_MAX_POLLS: usize = 600;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn transparent_address_activity_returns_descending_unified_feed() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = ActivityFixture::open(&env).await?;
    fixture.wait_until_consumer_caught_up().await?;
    let address = address_for_network(fixture.network)?;
    let request = TransparentAddressActivityRequest {
        address: Some(AddressLookup {
            selector: Some(address_lookup::Selector::Address(address.to_owned())),
        }),
        start_height: 0,
        end_height: fixture.sample_block_height.value(),
        max_entries: 16,
        from_cursor: Vec::new(),
        at_epoch: None,
    };
    let response = fixture.activity(request).await?;
    assert_activity_shape(&response)?;

    tracing::info!(
        target: "zinder::live",
        event = "transparent_address_activity_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        address = address,
        entry_count = response.entries.len(),
        "explorer transparent-address activity validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn transparent_address_activity_skips_mempool_overlay_on_continuation_page() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = ActivityFixture::open(&env).await?;
    fixture.wait_until_consumer_caught_up().await?;
    let address = address_for_network(fixture.network)?;
    let first_page = fixture
        .activity(TransparentAddressActivityRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::Address(address.to_owned())),
            }),
            start_height: 0,
            end_height: fixture.sample_block_height.value(),
            max_entries: 1,
            from_cursor: Vec::new(),
            at_epoch: None,
        })
        .await?;
    if first_page.next_cursor.is_empty() {
        fixture.shutdown().await;
        return Ok(());
    }
    let continuation = fixture
        .activity(TransparentAddressActivityRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::Address(address.to_owned())),
            }),
            start_height: 0,
            end_height: fixture.sample_block_height.value(),
            max_entries: 1,
            from_cursor: first_page.next_cursor,
            at_epoch: None,
        })
        .await?;
    assert!(
        continuation
            .entries
            .iter()
            .all(|entry| entry.block_height > 0),
        "continuation pages must only emit confirmed entries",
    );
    fixture.shutdown().await;
    Ok(())
}

struct ActivityFixture {
    network: Network,
    sample_block_height: BlockHeight,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    derive_store: DeriveStore,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    consumer_handle: Option<JoinHandle<()>>,
    consumer_cancel: CancellationToken,
    _store_tempdir: TempDir,
    _derive_tempdir: TempDir,
}

impl ActivityFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, tip_height) = backfill_store(env).await?;
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

        let derive_tempdir = tempdir()?;
        let derive_store = DeriveStore::open(
            derive_tempdir.path(),
            DeriveStoreOptions {
                sync_writes: false,
                consumer_column_families: &[TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY],
            },
        )?;

        let consumer_cancel = CancellationToken::new();
        let consumer_handle = spawn_consumer(
            derive_store.clone(),
            wallet_endpoint.clone(),
            consumer_cancel.clone(),
        );

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_derive_store(derive_store.clone())
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            sample_block_height: tip_height,
            explorer_adapter,
            derive_store,
            wallet_server_handle,
            ingest_control_handle,
            consumer_handle: Some(consumer_handle),
            consumer_cancel,
            _store_tempdir: store_tempdir,
            _derive_tempdir: derive_tempdir,
        })
    }

    async fn wait_until_consumer_caught_up(&self) -> Result<()> {
        for _ in 0..CONSUMER_CATCHUP_MAX_POLLS {
            let cursor = self
                .derive_store
                .get_cursor(TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME)?;
            if cursor.is_some() {
                return Ok(());
            }
            tokio::time::sleep(CONSUMER_CATCHUP_POLL_INTERVAL).await;
        }
        Err(eyre!(
            "TransparentAddressActivity consumer did not advance its cursor within {} polls",
            CONSUMER_CATCHUP_MAX_POLLS,
        ))
    }

    async fn activity(
        &self,
        request: TransparentAddressActivityRequest,
    ) -> Result<TransparentAddressActivityResponse> {
        let response = ExplorerQueryService::transparent_address_activity(
            &self.explorer_adapter,
            Request::new(request),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn shutdown(&mut self) {
        self.consumer_cancel.cancel();
        if let Some(handle) = self.consumer_handle.take() {
            let _ = handle.await;
        }
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
    }
}

fn spawn_consumer(
    store: DeriveStore,
    wallet_endpoint: String,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = drive_once(&store, &wallet_endpoint, &cancel).await {
            tracing::warn!(
                target: "zinder::live",
                event = "transparent_address_activity_consumer_failed",
                error = %error,
                "consumer driver returned an error",
            );
        }
    })
}

async fn drive_once(
    store: &DeriveStore,
    wallet_endpoint: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let cursor = store
        .get_cursor(TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME)?
        .unwrap_or_default();
    let channel_for_stream = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| eyre!("consumer stream connect: {error}"))?;
    let mut stream_client = WalletQueryClient::new(channel_for_stream);
    let stream = stream_client
        .chain_events(Request::new(ChainEventsRequest {
            from_cursor: cursor,
            family: WireChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }))
        .await?
        .into_inner();
    let channel_for_consumer = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| eyre!("consumer fetch connect: {error}"))?;
    let block_source = zinder_explorer::BlockSource::new(
        WalletQueryClient::new(channel_for_consumer),
        zinder_explorer::PrevoutResolver::Offline,
    );
    let mut consumer = TransparentAddressActivityConsumer::new(block_source);
    tokio::select! {
        outcome = run_chain_events_subscriber(&mut consumer, store, stream) => {
            outcome.map(|_| ()).map_err(|error| eyre!("subscriber: {error}"))
        }
        () = cancel.cancelled() => Ok(()),
    }
}

fn assert_activity_shape(response: &TransparentAddressActivityResponse) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("TransparentAddressActivity response missing freshness"))?;
    assert_eq!(
        freshness.capability_version,
        EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
    );
    let mut prior_height: Option<u32> = None;
    for entry in &response.entries {
        assert!(entry.block_height > 0, "confirmed entry must carry height");
        if let Some(prior) = prior_height {
            assert!(
                entry.block_height <= prior,
                "confirmed entries must be ordered newest-first",
            );
        }
        prior_height = Some(entry.block_height);
    }
    Ok(())
}

fn address_for_network(network: Network) -> Result<&'static str> {
    match network {
        Network::ZcashMainnet => Ok(MAINNET_TRANSPARENT_ADDRESS),
        Network::ZcashTestnet | Network::ZcashRegtest => Ok(REGTEST_TRANSPARENT_ADDRESS),
        other => Err(eyre!(
            "address_for_network called with unsupported network: {other:?}"
        )),
    }
}

async fn backfill_store(env: &LiveTestEnv) -> Result<(TempDir, PrimaryChainStore, BlockHeight)> {
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
    let mut backfill_config = live_backfill_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);
    backfill(&backfill_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed backfill outcome"))?;
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
