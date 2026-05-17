//! Live federation tests for [`ExplorerQuery::TransparentAddressActivity`].
//!
//! Validates the composition of `TransparentAddressTxIdsInRange` with
//! the mempool overlay against a real upstream node. The test does not
//! assume the queried address has activity; it asserts the response
//! shape (freshness envelope, ordering, dedup) without requiring a
//! mined transaction at the sampled height.

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
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, backfill};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1;
use zinder_proto::v1::explorer::{
    TransparentAddressActivityRequest, TransparentAddressActivityResponse,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_proto::v1::wallet::{AddressLookup, address_lookup};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

const BACKFILL_DEPTH_BLOCKS: u32 = 50;
const REGTEST_TRANSPARENT_ADDRESS: &str = "tmDpFafuBHKGUYmuwLsrxWJrwcnSyzEEtYx";
const MAINNET_TRANSPARENT_ADDRESS: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";

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
    let address = address_for_network(fixture.network)?;
    let request = TransparentAddressActivityRequest {
        address: Some(AddressLookup {
            selector: Some(address_lookup::Selector::Address(address.to_owned())),
        }),
        start_height: 0,
        end_height: fixture.sample_block_height.value(),
        max_entries: 16,
        from_cursor: Vec::new(),
        descending: true,
        include_mempool: true,
        at_epoch: None,
    };
    let response = fixture.activity(request).await?;
    assert_activity_shape(&response)?;
    assert_mempool_leads_when_descending(&response);

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
            descending: true,
            include_mempool: true,
            at_epoch: None,
        })
        .await?;
    if first_page.next_cursor.is_empty() {
        // No confirmed history at the sampled address; nothing to assert
        // beyond the shape contract.
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
            descending: true,
            include_mempool: true,
            at_epoch: None,
        })
        .await?;
    for entry in &continuation.entries {
        assert!(
            !entry.in_mempool,
            "continuation pages must not re-emit mempool entries",
        );
    }
    fixture.shutdown().await;
    Ok(())
}

struct ActivityFixture {
    network: Network,
    sample_block_height: BlockHeight,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
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

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
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
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
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
    let mut seen_confirmed = false;
    let mut prior_height: Option<u32> = None;
    for entry in &response.entries {
        if entry.in_mempool {
            assert_eq!(entry.block_height, 0, "mempool entry must not carry height");
            assert!(
                entry.block_hash.is_empty(),
                "mempool entry must not carry block hash",
            );
            assert!(
                !seen_confirmed,
                "mempool entries must precede confirmed entries when descending",
            );
        } else {
            seen_confirmed = true;
            assert_eq!(entry.block_hash.len(), 32, "block hash must be 32 bytes");
            if let Some(prior) = prior_height {
                assert!(
                    entry.block_height <= prior,
                    "confirmed entries must be ordered newest-first",
                );
            }
            prior_height = Some(entry.block_height);
        }
    }
    Ok(())
}

fn assert_mempool_leads_when_descending(response: &TransparentAddressActivityResponse) {
    let Some(first) = response.entries.first() else {
        return;
    };
    let has_mempool = response.entries.iter().any(|entry| entry.in_mempool);
    if has_mempool {
        assert!(
            first.in_mempool,
            "first entry must be a mempool entry when overlay is non-empty and descending",
        );
    }
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
