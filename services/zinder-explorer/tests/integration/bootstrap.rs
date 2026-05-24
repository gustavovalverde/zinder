#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! Smoke test that boots an `ExplorerQueryGrpcAdapter` against an in-process
//! tonic server and verifies `ServerInfo` returns the expected capability set.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use eyre::{Result, eyre};
use prost::Message as _;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{BlockHeight, ChainEpochId, Network, wire::encode_internal_block_hash};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, BlockSummaryConsumer, DeriveStore,
    DeriveStoreOptions,
};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V1,
    EXPLORER_TRANSACTION_FEES_V1, EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
};
use zinder_proto::v1::explorer::{
    BlockSummariesInRangeRequest, BlockSummary, BlockSummaryRecord, ServerInfoRequest,
    TransactionDetailRequest, explorer_query_client::ExplorerQueryClient,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

type ServerHandle = tokio::task::JoinHandle<Result<(), tonic::transport::Error>>;

struct SeededDeriveStore {
    _tempdir: tempfile::TempDir,
    secondary_store: DeriveStore,
}

#[tokio::test]
async fn explorer_query_server_info_advertises_ready_capability() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    });
    let server_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let channel = await_with_retry(server_addr).await?;
    let mut client = ExplorerQueryClient::new(channel);
    let response = client.server_info(ServerInfoRequest {}).await?.into_inner();
    let explorer_info = response
        .info
        .ok_or_else(|| eyre!("server info response missing info envelope"))?;
    let common = explorer_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;

    assert_eq!(explorer_info.vendor, "Zinder");
    assert_eq!(common.network, "zcash-regtest");
    assert!(
        common
            .capabilities
            .iter()
            .any(|advertised| { advertised == EXPLORER_SERVER_INFO_V1 })
    );

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

/// Without a configured `wallet_query_endpoint`, the explorer-balance
/// capability is omitted from `ServerInfo` and the federated method returns
/// `UNAVAILABLE`.
///
/// This pins the operational contract that capability advertisement gates on
/// a wired federation, not on the binary's mere presence.
#[tokio::test]
async fn explorer_query_balance_unavailable_without_wallet_query_endpoint() -> Result<()> {
    use zinder_proto::v1::wallet::TransparentAddressBalanceRequest;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    });
    let server_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let channel = await_with_retry(server_addr).await?;
    let mut client = ExplorerQueryClient::new(channel);
    let explorer_info = client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .ok_or_else(|| eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert!(
        !common
            .capabilities
            .iter()
            .any(|advertised| { advertised == EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1 }),
        "balance capability must not advertise without a wallet_query_endpoint",
    );

    let outcome = client
        .transparent_address_balance(TransparentAddressBalanceRequest {
            addresses: Vec::new(),
            at_epoch: None,
        })
        .await;
    let status = outcome
        .err()
        .ok_or_else(|| eyre!("expected UNAVAILABLE without wallet_query_endpoint"))?;
    assert_eq!(status.code(), tonic::Code::Unavailable);

    assert!(
        !common
            .capabilities
            .iter()
            .any(|advertised| { advertised == EXPLORER_TRANSACTION_DETAIL_V1 }),
        "transaction_detail capability must not advertise without a wallet_query_endpoint",
    );
    let detail_outcome = client
        .transaction_detail(TransactionDetailRequest {
            transaction_id: vec![0_u8; 32],
            at_epoch: None,
        })
        .await;
    let detail_status = detail_outcome
        .err()
        .ok_or_else(|| eyre!("expected UNAVAILABLE without wallet_query_endpoint"))?;
    assert_eq!(detail_status.code(), tonic::Code::Unavailable);

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn explorer_query_serves_block_summary_from_secondary_derive_store() -> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_derive_store = seeded_block_summary_derive_store(&chain_fixture)?;
    let (mut client, explorer_handle) =
        spawn_explorer_query_server(seeded_derive_store.secondary_store, wallet_addr).await?;

    let explorer_info = client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .ok_or_else(|| eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_advertises_capability(&common.capabilities, EXPLORER_BLOCK_SUMMARY_V1);
    assert_advertises_capability(&common.capabilities, EXPLORER_TRANSACTION_FEES_V1);

    let response = client
        .block_summaries_in_range(BlockSummariesInRangeRequest {
            start_height: 1,
            end_height: 1,
            at_epoch: None,
        })
        .await?
        .into_inner();
    assert_eq!(response.summaries.len(), 1);
    assert_eq!(response.summaries[0].block_height, 1);
    assert_eq!(response.summaries[0].confirmations, 1);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

async fn spawn_wallet_query_server(
    chain_fixture: &ChainFixture,
) -> Result<(StoreFixture, SocketAddr, ServerHandle)> {
    let store_fixture = StoreFixture::with_chain_committed(chain_fixture, ChainEpochId::new(1))?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let _channel = await_with_retry(addr).await?;
    Ok((store_fixture, addr, handle))
}

fn seeded_block_summary_derive_store(chain_fixture: &ChainFixture) -> Result<SeededDeriveStore> {
    let tempdir = tempfile::tempdir()?;
    let primary_path = tempdir.path().join("derive-primary");
    let secondary_path = tempdir.path().join("derive-secondary");
    let primary_store = DeriveStore::open(
        &primary_path,
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: DeriveStore::bundled_consumer_column_families(),
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?;
    seed_block_summary(&primary_store, chain_fixture)?;

    let secondary_store = DeriveStore::open_secondary(
        &primary_path,
        &secondary_path,
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: DeriveStore::bundled_consumer_column_families(),
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?;
    secondary_store.try_catch_up()?;
    Ok(SeededDeriveStore {
        _tempdir: tempdir,
        secondary_store,
    })
}

fn seed_block_summary(derive_store: &DeriveStore, chain_fixture: &ChainFixture) -> Result<()> {
    let fixture_block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let record = BlockSummaryRecord {
        summary: Some(BlockSummary {
            block_height: fixture_block.height.value(),
            block_hash: encode_internal_block_hash(fixture_block.hash).to_vec(),
            block_time_unix_seconds: i64::from(fixture_block.block_time_seconds),
            transaction_count: 0,
            previous_block_hash: encode_internal_block_hash(fixture_block.parent_hash).to_vec(),
            total_size_bytes: u64::try_from(fixture_block.raw_block_bytes.len())?,
            fees_collected_zat: 0,
            paid_fees_collected_zat: None,
            coinbase_reward_zat: 0,
            sapling_output_count: 0,
            orchard_action_count: 0,
            confirmations: 0,
            is_canonical: true,
        }),
        transaction_ids: Vec::new(),
        fee_transaction_count: 0,
        min_zip317_conventional_fee_zat: 0,
        max_zip317_conventional_fee_zat: 0,
    };
    derive_store.put_consumer(
        BLOCK_SUMMARY_COLUMN_FAMILY,
        &BlockSummaryConsumer::key_for_height(fixture_block.height),
        &record.encode_to_vec(),
    )?;
    derive_store.put_chain_event_cursor(BLOCK_SUMMARY_CONSUMER_NAME, &[1])?;
    Ok(())
}

async fn spawn_explorer_query_server(
    derive_store: DeriveStore,
    wallet_addr: SocketAddr,
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_derive_store(derive_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .with_prevout_resolution_online(true);
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = await_with_retry(addr).await?;
    Ok((ExplorerQueryClient::new(channel), handle))
}

fn assert_advertises_capability(capabilities: &[String], capability: &str) {
    assert!(
        capabilities
            .iter()
            .any(|advertised| advertised == capability),
        "expected capability {capability}",
    );
}

#[tokio::test]
async fn explorer_query_bearer_token_rejects_unauthenticated_clients() -> Result<()> {
    use std::str::FromStr as _;
    use zinder_runtime::{BearerToken, BearerTokenClientInterceptor};

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let server_token =
        BearerToken::from_str("expected").map_err(|error| eyre!("token parse: {error}"))?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_bearer_token(server_token.clone());
    let server_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let unauthenticated_channel = await_with_retry(server_addr).await?;
    let mut unauthenticated_client = ExplorerQueryClient::new(unauthenticated_channel);
    let unauthenticated_outcome = unauthenticated_client
        .server_info(ServerInfoRequest {})
        .await;
    let unauthenticated_status = unauthenticated_outcome
        .err()
        .ok_or_else(|| eyre!("expected unauthenticated rejection"))?;
    assert_eq!(unauthenticated_status.code(), tonic::Code::Unauthenticated);

    let wrong_token =
        BearerToken::from_str("wrong").map_err(|error| eyre!("token parse: {error}"))?;
    let wrong_channel = Endpoint::from_shared(format!("http://{server_addr}"))?
        .connect()
        .await?;
    let wrong_interceptor = BearerTokenClientInterceptor::new(Some(&wrong_token))
        .map_err(|error| eyre!("interceptor build: {error}"))?;
    let mut wrong_client = ExplorerQueryClient::with_interceptor(wrong_channel, wrong_interceptor);
    let wrong_outcome = wrong_client.server_info(ServerInfoRequest {}).await;
    let wrong_status = wrong_outcome
        .err()
        .ok_or_else(|| eyre!("expected wrong-token rejection"))?;
    assert_eq!(wrong_status.code(), tonic::Code::Unauthenticated);

    let correct_channel = Endpoint::from_shared(format!("http://{server_addr}"))?
        .connect()
        .await?;
    let correct_interceptor = BearerTokenClientInterceptor::new(Some(&server_token))
        .map_err(|error| eyre!("interceptor build: {error}"))?;
    let mut correct_client =
        ExplorerQueryClient::with_interceptor(correct_channel, correct_interceptor);
    let correct_response = correct_client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner();
    let correct_info = correct_response
        .info
        .ok_or_else(|| eyre!("server info missing info envelope"))?;
    let correct_common = correct_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_eq!(correct_common.network, "zcash-regtest");

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

async fn await_with_retry(addr: std::net::SocketAddr) -> Result<Channel> {
    let endpoint = format!("http://{addr}");
    for _ in 0..20 {
        if let Ok(channel) = Channel::from_shared(endpoint.clone())?.connect().await {
            return Ok(channel);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre!(
        "explorer query gRPC server did not accept connections"
    ))
}
