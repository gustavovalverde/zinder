#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! Smoke test that boots an `ExplorerQueryGrpcAdapter` against an in-process
//! tonic server and verifies `ServerInfo` returns the expected capability set.

use std::{net::SocketAddr, str::FromStr as _, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::{Result, eyre};
use prost::Message as _;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, ChainEpochId, Network,
    NetworkUpgradeActivations, TransactionId, TransactionLocation, TransparentAddressScriptHash,
    TransparentInputFact, TransparentOutPoint, TransparentOutputFact, TransparentSpendFact,
    wire::{encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex},
};
use zinder_explorer::{
    ExplorerEndpointAdmissionError, ExplorerEndpointMetadata, ExplorerQueryGrpcAdapter,
};
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA,
    BlockSummaryConsumer, MaterializedViewStore, MaterializedViewStoreOptions,
    REORG_INCIDENTS_CONSUMER_NAME,
};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1, EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    EXPLORER_BLOCK_TRANSACTIONS_V2, EXPLORER_CHAIN_REORG_HISTORY_V1, EXPLORER_OVERVIEW_SNAPSHOT_V1,
    EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4,
};
use zinder_proto::v1::explorer::{
    BlockActivityDistributionRequest, BlockDetailRequest, BlockProductionSeriesRequest,
    BlockSummary, BlockSummaryRecord, BlockTransactionsResponse, ChainReorgHistoryRequest,
    OverviewSnapshotRequest, ServerInfoRequest, TransactionDetailRequest, block_detail_request,
    explorer_query_client::ExplorerQueryClient,
};
use zinder_query::{
    WalletEndpointMetadata, WalletQuery, WalletQueryGrpcAdapter, WalletServingPairSlot,
    WalletServingQuery, WalletServingReadPair,
};
use zinder_runtime::{BearerToken, BearerTokenServerInterceptor};
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceError,
    UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT, UpstreamHealthSnapshot,
};
use zinder_store::{
    ChainEpochArtifacts, ChainStoreOptions, RawBlobRetention, ReorgWindowChange,
    SecondaryChainStore,
};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, StoreFixture, WalletServingStoreFixture,
    encode_fixture_block_replay, sample_regtest_upgrade_activations,
    synthetic_transaction_public_facts,
};

type ServerHandle = tokio::task::JoinHandle<Result<(), tonic::transport::Error>>;

struct SeededMaterializedViewStore {
    _tempdir: tempfile::TempDir,
    secondary_store: MaterializedViewStore,
}

#[tokio::test]
async fn explorer_query_server_info_advertises_ready_capability() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .build()
    .await?;
    let server_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let channel = await_with_retry(server_addr).await?;
    let mut client = ExplorerQueryClient::new(channel);
    let response = client.server_info(ServerInfoRequest {}).await?.into_inner();
    assert!(
        response
            .freshness
            .as_ref()
            .and_then(|freshness| freshness.chain_view.as_ref())
            .is_none(),
        "ServerInfo without a materialized-view store or upstream probe carries no chain_view",
    );
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

#[tokio::test]
async fn explorer_query_server_info_reports_the_exact_materialized_view_manifest() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let primary_path = temporary.path().join("primary");
    let secondary_path = temporary.path().join("secondary");
    let _primary = MaterializedViewStore::open(
        &primary_path,
        Network::ZcashRegtest,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: &[BLOCK_SUMMARY_SCHEMA],
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    let secondary = MaterializedViewStore::open_secondary(
        primary_path,
        secondary_path,
        Network::ZcashRegtest,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: &[BLOCK_SUMMARY_SCHEMA],
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(secondary)
    .build()
    .await?;

    let response = zinder_proto::v1::explorer::explorer_query_server::ExplorerQuery::server_info(
        &adapter,
        tonic::Request::new(ServerInfoRequest {}),
    )
    .await?
    .into_inner();
    let common = response
        .info
        .and_then(|info| info.common)
        .ok_or_else(|| eyre!("explorer ServerInfo omitted common identity"))?;

    assert_eq!(
        common.materialized_view_identities,
        vec![BLOCK_SUMMARY_CONSUMER_NAME.as_str().to_owned()]
    );
    assert_eq!(common.materialized_view_preset, "explorer");
    Ok(())
}

#[tokio::test]
async fn wallet_query_admission_fails_before_explorer_serving_when_endpoint_is_unreachable()
-> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let unreachable_addr = listener.local_addr()?;
    drop(listener);

    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_wallet_query_endpoint(format!("http://{unreachable_addr}"))
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("unreachable wallet endpoint was admitted"))?;

    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::WalletEndpointUnreachable(_)
    ));
    Ok(())
}

#[tokio::test]
async fn wallet_query_admission_uses_the_configured_outbound_bearer_token() -> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let expected_token =
        BearerToken::from_str("expected").map_err(|error| eyre!("token parse: {error}"))?;
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server_with_bearer_token(&chain_fixture, Some(expected_token.clone()))
            .await?;
    let endpoint = format!("http://{wallet_addr}");

    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_wallet_query_endpoint(endpoint.clone())
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("wallet endpoint admitted without its bearer token"))?;
    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::WalletServerInfo(ref status)
            if status.code() == tonic::Code::Unauthenticated
    ));

    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_wallet_query_endpoint(endpoint)
    .with_wallet_query_bearer_token(expected_token)
    .build()
    .await?;
    assert!(
        adapter
            .advertised_capabilities()
            .contains(&EXPLORER_SERVER_INFO_V1)
    );

    wallet_handle.abort();
    Ok(())
}

#[tokio::test]
async fn endpoint_admission_rejects_wallet_authorization_without_an_endpoint() -> Result<()> {
    let bearer_token =
        BearerToken::from_str("unused").map_err(|error| eyre!("token parse: {error}"))?;
    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_wallet_query_bearer_token(bearer_token)
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("wallet authorization without an endpoint was admitted"))?;

    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::WalletAuthorizationRequiresEndpoint
    ));
    Ok(())
}

#[tokio::test]
async fn endpoint_admission_rejects_a_canonical_secondary_from_another_network() -> Result<()> {
    let canonical_store_fixture = StoreFixture::open_with_options(ChainStoreOptions {
        network: Some(Network::ZcashTestnet),
        ..ChainStoreOptions::for_local_tests()
    })?;
    let canonical_store = SecondaryChainStore::open(
        canonical_store_fixture.tempdir_path(),
        canonical_store_fixture
            .tempdir_path()
            .join("cross-network-secondary"),
        ChainStoreOptions {
            network: Some(Network::ZcashTestnet),
            ..ChainStoreOptions::for_local_tests()
        },
    )?;
    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_canonical_store(canonical_store)
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("cross-network canonical secondary was admitted"))?;

    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::CanonicalStoreNetworkMismatch {
            expected: Network::ZcashRegtest,
            actual: Network::ZcashTestnet,
        }
    ));
    Ok(())
}

#[tokio::test]
async fn endpoint_admission_rejects_a_canonical_secondary_without_network_identity() -> Result<()> {
    let canonical_store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let canonical_store = SecondaryChainStore::open(
        canonical_store_fixture.tempdir_path(),
        canonical_store_fixture
            .tempdir_path()
            .join("network-agnostic-secondary"),
        ChainStoreOptions {
            network: None,
            ..ChainStoreOptions::for_local_tests()
        },
    )?;
    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_canonical_store(canonical_store)
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("network-agnostic canonical secondary was admitted"))?;

    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::CanonicalStoreNetworkUnspecified
    ));
    Ok(())
}

#[tokio::test]
async fn endpoint_admission_rejects_an_activation_table_from_another_network() -> Result<()> {
    let error = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_network_upgrade_activations(Arc::new(NetworkUpgradeActivations::empty(
        Network::ZcashTestnet,
    )))
    .build()
    .await
    .err()
    .ok_or_else(|| eyre!("cross-network activation table was admitted"))?;

    assert!(matches!(
        error,
        ExplorerEndpointAdmissionError::NetworkUpgradeActivationsNetworkMismatch {
            expected: Network::ZcashRegtest,
            actual: Network::ZcashTestnet,
        }
    ));
    Ok(())
}

#[tokio::test]
async fn wallet_serving_composition_omits_transaction_detail_until_it_owns_the_required_contract()
-> Result<()> {
    let retained_chain = ChainFixture::new(Network::ZcashRegtest)
        .with_raw_blob_retention(RawBlobRetention::Transactions)
        .extend_blocks(1);
    let (_wallet_store, wallet_addr, wallet_handle) =
        spawn_wallet_serving_query_server(&retained_chain).await?;
    let materialized_view_store = seeded_block_summary_materialized_view_store(&retained_chain)?;
    let canonical_store_fixture =
        StoreFixture::with_chain_committed(&retained_chain, ChainEpochId::new(1))?;
    let canonical_store = SecondaryChainStore::open(
        canonical_store_fixture.tempdir_path(),
        canonical_store_fixture
            .tempdir_path()
            .join("transaction-detail-binary-secondary"),
        ChainStoreOptions::for_local_tests(),
    )?;
    canonical_store.try_catch_up()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store.secondary_store)
    .with_canonical_store(canonical_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .build()
    .await?;
    assert!(
        !adapter
            .advertised_capabilities()
            .contains(&EXPLORER_TRANSACTION_DETAIL_V4),
        "retained bytes and a canonical secondary cannot replace missing native T+B claims"
    );
    let status =
        zinder_proto::v1::explorer::explorer_query_server::ExplorerQuery::transaction_detail(
            &adapter,
            tonic::Request::new(TransactionDetailRequest {
                transaction_id: "not-a-transaction-id".to_owned(),
                at_epoch_id: None,
            }),
        );
    let status = status
        .await
        .err()
        .ok_or_else(|| eyre!("unadvertised transaction detail unexpectedly succeeded"))?;
    assert_eq!(status.code(), tonic::Code::Unimplemented);

    wallet_handle.abort();
    Ok(())
}

/// Without a configured `wallet_query_endpoint`, the wallet-backed explorer
/// capabilities are omitted from `ServerInfo` and the corresponding methods
/// return `UNIMPLEMENTED`.
///
/// This pins the operational contract that capability advertisement gates on
/// a wired federation, not on the binary's mere presence.
#[tokio::test]
async fn explorer_query_omits_uncomposed_methods_without_wallet_query_endpoint() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .build()
    .await?;
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
            .any(|advertised| { advertised == EXPLORER_TRANSACTION_DETAIL_V4 }),
        "transaction_detail capability must not advertise without a wallet_query_endpoint",
    );
    let detail_outcome = client
        .transaction_detail(TransactionDetailRequest {
            transaction_id: "00".repeat(32),
            at_epoch_id: None,
        })
        .await;
    let detail_status = detail_outcome
        .err()
        .ok_or_else(|| eyre!("expected UNIMPLEMENTED without wallet_query_endpoint"))?;
    assert_eq!(detail_status.code(), tonic::Code::Unimplemented);

    assert!(
        !common
            .capabilities
            .iter()
            .any(|advertised| { advertised == EXPLORER_OVERVIEW_SNAPSHOT_V1 }),
        "overview_snapshot capability must not advertise without a materialized-view store",
    );
    let overview_outcome = client
        .overview_snapshot(OverviewSnapshotRequest {
            recent_blocks_limit: 0,
            recent_transactions_limit: 0,
            mempool_window_seconds: 0,
            fee_summary_block_count: 0,
        })
        .await;
    let overview_status = overview_outcome
        .err()
        .ok_or_else(|| eyre!("expected UNIMPLEMENTED without materialized-view store"))?;
    assert_eq!(overview_status.code(), tonic::Code::Unimplemented);

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario seeds a coinbase-bearing fixture, spawns wallet and explorer servers, and asserts every block-production and coinbase field in one request; splitting it obscures the end-to-end flow"
)]
async fn explorer_query_serves_block_production_series_with_explicit_coverage() -> Result<()> {
    let base_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let fixture_block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let coinbase_transaction_id = TransactionId::from_bytes([0xCB; 32]);
    let coinbase_location = TransactionLocation::new(
        coinbase_transaction_id,
        fixture_block.height,
        fixture_block.hash,
        0,
    );
    let mut coinbase_facts = synthetic_transaction_public_facts(coinbase_transaction_id, 64);
    coinbase_facts.is_coinbase = true;
    coinbase_facts.counts.transparent_input_count = 1;
    coinbase_facts.counts.transparent_output_count = 1;
    let mut coinbase_rows =
        FixtureTransactionRows::from_public_facts(coinbase_location, coinbase_facts);
    let script_pub_key = vec![0x51];
    coinbase_rows.facts = coinbase_rows.facts.with_transparent_facts(
        Vec::new(),
        vec![TransparentOutputFact::new(
            0,
            137_500_000,
            script_pub_key.clone(),
            TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
        )],
    );
    let chain_fixture = base_fixture.with_transaction_rows(coinbase_rows);
    let (store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let canonical_store = SecondaryChainStore::open(
        store_fixture.tempdir_path(),
        store_fixture
            .tempdir_path()
            .join("block-production-canonical-secondary"),
        ChainStoreOptions::for_local_tests(),
    )?;
    canonical_store.try_catch_up()?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store_with_transaction_ids(
            &chain_fixture,
            &[encode_rpc_transaction_id_hex(coinbase_transaction_id)],
        )?;
    let (mut client, explorer_handle) = spawn_explorer_query_server_with_canonical_store(
        seeded_materialized_view_store.secondary_store,
        canonical_store,
        wallet_addr,
    )
    .await?;

    let common = client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .and_then(|info| info.common)
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_advertises_capability(&common.capabilities, EXPLORER_BLOCK_PRODUCTION_SERIES_V2);

    let response = client
        .block_production_series(BlockProductionSeriesRequest {
            start_height: 0,
            end_height: 1,
            at_epoch_id: None,
        })
        .await?
        .into_inner();
    assert_eq!(response.start_height, 0);
    assert_eq!(response.end_height, 1);
    assert_eq!(response.covered_block_count, 1);
    assert_eq!(response.missing_block_count, 1);
    assert_eq!(response.points.len(), 1);
    assert_eq!(response.points[0].bits, 0);
    let summary = response.points[0]
        .summary
        .as_ref()
        .ok_or_else(|| eyre!("block production point missing summary"))?;
    assert_eq!(summary.block_height, 1);
    assert_eq!(summary.confirmations, 1);
    let coinbase = response.points[0]
        .coinbase
        .as_ref()
        .ok_or_else(|| eyre!("block production point missing coinbase"))?;
    assert_eq!(
        coinbase.transaction_id,
        encode_rpc_transaction_id_hex(coinbase_transaction_id)
    );
    assert_eq!(coinbase.transparent_outputs.len(), 1);
    assert_eq!(coinbase.transparent_outputs[0].value_zat, 137_500_000);
    assert_eq!(
        coinbase.transparent_outputs[0].script_pub_key,
        script_pub_key
    );
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("block production response missing freshness"))?;
    assert_eq!(
        freshness.capability_version,
        EXPLORER_BLOCK_PRODUCTION_SERIES_V2
    );
    assert_eq!(freshness_visible_tip(freshness)?.height, 1);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

#[tokio::test]
async fn explorer_query_aggregates_block_activity_with_explicit_coverage() -> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store(&chain_fixture)?;
    let (mut client, explorer_handle) =
        spawn_explorer_query_server(seeded_materialized_view_store.secondary_store, wallet_addr)
            .await?;

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
    assert_advertises_capability(
        &common.capabilities,
        EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
    );

    let response = client
        .block_activity_distribution(BlockActivityDistributionRequest {
            start_height: 0,
            end_height: 1,
        })
        .await?
        .into_inner();
    assert_eq!(response.start_height, 0);
    assert_eq!(response.end_height, 1);
    assert_eq!(response.materialized_block_count, 1);
    assert_eq!(response.missing_block_count, 1);
    assert_eq!(response.transaction_count, 0);
    assert_eq!(response.buckets.len(), 168);
    assert!(response.first_block_time_unix_seconds.is_some());
    assert!(response.last_block_time_unix_seconds.is_some());

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

/// Block transaction rows retain canonical order when facts are unavailable.
///
/// The materialized block record supplies each id and index without fabricating
/// an all-zero transaction for an absent canonical facts artifact.
#[tokio::test]
async fn explorer_query_serves_canonical_block_transactions_with_partial_fact_retention()
-> Result<()> {
    let mut fixture = block_transactions_test_fixture().await?;

    let explorer_info = fixture
        .client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .ok_or_else(|| eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_advertises_capability(&common.capabilities, EXPLORER_BLOCK_TRANSACTIONS_V2);

    let response = fixture
        .client
        .block_transactions(BlockDetailRequest {
            at_epoch_id: Some(1),
            selector: Some(block_detail_request::Selector::BlockHeight(1)),
        })
        .await?
        .into_inner();
    assert_block_transactions_response(&response, &fixture.transaction_id_strings)?;

    fixture.explorer_handle.abort();
    let _ = fixture.explorer_handle.await;
    fixture.wallet_handle.abort();
    let _ = fixture.wallet_handle.await;
    Ok(())
}

struct BlockTransactionsTestFixture {
    _canonical_store_fixture: StoreFixture,
    client: ExplorerQueryClient<Channel>,
    explorer_handle: ServerHandle,
    transaction_id_strings: Vec<String>,
    wallet_handle: ServerHandle,
}

async fn block_transactions_test_fixture() -> Result<BlockTransactionsTestFixture> {
    let (chain_fixture, transaction_id_strings, missing_transaction_id) =
        block_transactions_chain_fixture()?;
    let (_wallet_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let (canonical_store_fixture, canonical_store) =
        canonical_store_without_transaction_facts(&chain_fixture, missing_transaction_id)?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store_with_transaction_ids(
            &chain_fixture,
            &transaction_id_strings,
        )?;
    let (client, explorer_handle) = spawn_explorer_query_server_with_canonical_store(
        seeded_materialized_view_store.secondary_store,
        canonical_store,
        wallet_addr,
    )
    .await?;
    Ok(BlockTransactionsTestFixture {
        _canonical_store_fixture: canonical_store_fixture,
        client,
        explorer_handle,
        transaction_id_strings,
        wallet_handle,
    })
}

fn block_transactions_chain_fixture() -> Result<(ChainFixture, Vec<String>, TransactionId)> {
    let base_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let transaction_ids = block_transaction_ids();
    let transaction_id_strings = transaction_ids
        .iter()
        .copied()
        .map(encode_rpc_transaction_id_hex)
        .collect();
    let [first, second, unavailable] =
        block_transaction_fixture_rows(block.height, block.hash, transaction_ids);
    let spent_script_hash = TransparentAddressScriptHash::of_script_pub_key(&[0x51]);
    let same_block_spend = TransparentSpendFact::new(
        TransparentOutPoint::new(transaction_ids[0], 0),
        1,
        transaction_ids[1],
        1,
        block.height,
        block.hash,
        21_000,
        spent_script_hash,
        block.height,
        block.hash,
    );
    let chain_fixture = base_fixture
        .with_transaction_rows(first)
        .with_transaction_rows(second)
        .with_transaction_rows(unavailable)
        .with_transparent_spend_fact(same_block_spend);
    Ok((chain_fixture, transaction_id_strings, transaction_ids[2]))
}

fn block_transaction_ids() -> [TransactionId; 3] {
    [
        TransactionId::from_bytes([0x01; 32]),
        TransactionId::from_bytes([0x02; 32]),
        TransactionId::from_bytes([0x03; 32]),
    ]
}

fn block_transaction_fixture_rows(
    block_height: BlockHeight,
    block_hash: BlockHash,
    transaction_ids: [TransactionId; 3],
) -> [FixtureTransactionRows; 3] {
    [
        coinbase_transaction_row(block_height, block_hash, transaction_ids[0]),
        transparent_spend_transaction_row(
            block_height,
            block_hash,
            transaction_ids[0],
            transaction_ids[1],
        ),
        FixtureTransactionRows::from_public_facts(
            TransactionLocation::new(transaction_ids[2], block_height, block_hash, 2),
            synthetic_transaction_public_facts(transaction_ids[2], 64),
        ),
    ]
}

fn coinbase_transaction_row(
    block_height: BlockHeight,
    block_hash: BlockHash,
    transaction_id: TransactionId,
) -> FixtureTransactionRows {
    let first_script_pub_key = vec![0x51];
    let second_script_pub_key = vec![0x52];
    let mut public_facts = synthetic_transaction_public_facts(transaction_id, 120);
    public_facts.is_coinbase = true;
    public_facts.counts.transparent_input_count = 1;
    public_facts.counts.transparent_output_count = 2;
    let transaction = FixtureTransactionRows::from_public_facts(
        TransactionLocation::new(transaction_id, block_height, block_hash, 0),
        public_facts,
    );
    FixtureTransactionRows {
        facts: transaction.facts.with_transparent_facts(
            Vec::new(),
            vec![
                transparent_output_fact(0, 21_000, first_script_pub_key),
                transparent_output_fact(1, 34_000, second_script_pub_key),
            ],
        ),
        ..transaction
    }
}

fn transparent_output_fact(
    output_index: u32,
    value_zat: u64,
    script_pub_key: Vec<u8>,
) -> TransparentOutputFact {
    let script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    TransparentOutputFact::new(output_index, value_zat, script_pub_key, script_hash)
}

fn transparent_spend_transaction_row(
    block_height: BlockHeight,
    block_hash: BlockHash,
    spent_transaction_id: TransactionId,
    transaction_id: TransactionId,
) -> FixtureTransactionRows {
    let mut public_facts = synthetic_transaction_public_facts(transaction_id, 80);
    public_facts.counts.transparent_input_count = 2;
    let transaction = FixtureTransactionRows::from_public_facts(
        TransactionLocation::new(transaction_id, block_height, block_hash, 1),
        public_facts,
    );
    FixtureTransactionRows {
        facts: transaction.facts.with_transparent_facts(
            vec![
                TransparentInputFact::new(
                    0,
                    TransparentOutPoint::new(TransactionId::from_bytes([0xA1; 32]), 4),
                ),
                TransparentInputFact::new(1, TransparentOutPoint::new(spent_transaction_id, 0)),
            ],
            Vec::new(),
        ),
        ..transaction
    }
}

fn historical_parent_transaction_row(
    block_height: BlockHeight,
    block_hash: BlockHash,
) -> FixtureTransactionRows {
    let transaction_id = TransactionId::from_bytes([0xA1; 32]);
    let location = TransactionLocation::new(transaction_id, block_height, block_hash, 2);
    let mut public_facts = synthetic_transaction_public_facts(transaction_id, 64);
    public_facts.counts.transparent_output_count = 5;
    let transaction = FixtureTransactionRows::from_public_facts(location, public_facts);
    FixtureTransactionRows {
        facts: transaction.facts.with_transparent_facts(
            Vec::new(),
            vec![
                transparent_output_fact(0, 1, vec![0x50]),
                transparent_output_fact(1, 2, vec![0x51]),
                transparent_output_fact(2, 3, vec![0x52]),
                transparent_output_fact(3, 4, vec![0x54]),
                transparent_output_fact(4, 60_000, vec![0x53]),
            ],
        ),
        ..transaction
    }
}

fn canonical_store_without_transaction_facts(
    chain_fixture: &ChainFixture,
    missing_transaction_id: TransactionId,
) -> Result<(StoreFixture, SecondaryChainStore)> {
    let mut artifacts = chain_fixture
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("fixture chain epoch artifacts missing"))?;
    let block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let mut retained_transaction_rows =
        block_transaction_fixture_rows(block.height, block.hash, block_transaction_ids())
            .into_iter()
            .filter(|transaction_rows| {
                transaction_rows.location.transaction_id != missing_transaction_id
            })
            .collect::<Vec<_>>();
    retained_transaction_rows.push(historical_parent_transaction_row(block.height, block.hash));
    artifacts.compact_blocks = retained_transaction_rows
        .iter()
        .cloned()
        .fold(
            ChainFixture::new(chain_fixture.network()).extend_blocks(1),
            ChainFixture::with_transaction_rows,
        )
        .compact_block_artifacts();
    artifacts.block_replay_envelopes = vec![encode_fixture_block_replay(
        &block.block_header_artifact(),
        &retained_transaction_rows,
    )];
    artifacts.block_transaction_index = retained_transaction_rows
        .iter()
        .map(|transaction_rows| transaction_rows.block_transaction_index)
        .collect();
    artifacts.transaction_locations = retained_transaction_rows
        .iter()
        .map(|transaction_rows| transaction_rows.location)
        .collect();
    artifacts.transaction_facts = retained_transaction_rows
        .iter()
        .map(|transaction_rows| transaction_rows.facts.clone())
        .collect();
    artifacts.transaction_intrinsic_value_balances = retained_transaction_rows
        .iter()
        .filter_map(FixtureTransactionRows::intrinsic_value_balances_artifact)
        .collect();
    artifacts.transaction_blobs = retained_transaction_rows
        .iter()
        .filter_map(|transaction_rows| transaction_rows.blob.clone())
        .collect();
    artifacts.transparent_outputs_by_outpoint = retained_transaction_rows
        .iter()
        .flat_map(FixtureTransactionRows::transparent_output_artifacts)
        .collect();
    let store_fixture = StoreFixture::open()?;
    store_fixture.chain_store().commit_chain_epoch(artifacts)?;
    let secondary_store = SecondaryChainStore::open(
        store_fixture.tempdir_path(),
        store_fixture.tempdir_path().join("canonical-secondary"),
        ChainStoreOptions::for_local_tests(),
    )?;
    secondary_store.try_catch_up()?;
    Ok((store_fixture, secondary_store))
}

fn assert_block_transactions_response(
    response: &BlockTransactionsResponse,
    transaction_id_strings: &[String],
) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("block transactions response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_BLOCK_TRANSACTIONS_V2);
    assert_eq!(freshness_visible_tip(freshness)?.height, 1);
    assert_eq!(response.transactions.len(), 3);
    assert_eq!(
        response
            .transactions
            .iter()
            .map(|transaction| transaction.transaction_id.as_str())
            .collect::<Vec<_>>(),
        transaction_id_strings
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        response
            .transactions
            .iter()
            .map(|transaction| transaction.transaction_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2],
    );

    let first_row = &response.transactions[0];
    assert!(
        first_row
            .public_facts
            .as_ref()
            .is_some_and(|facts| facts.is_coinbase)
    );
    assert_eq!(
        first_row
            .transparent_outputs
            .iter()
            .map(|output| (output.value_zat, output.script_pub_key.as_slice()))
            .collect::<Vec<_>>(),
        vec![(21_000, &[0x51][..]), (34_000, &[0x52][..])],
    );
    assert!(first_row.transparent_inputs.is_empty());

    let second_row = &response.transactions[1];
    assert_eq!(second_row.transparent_inputs.len(), 2);
    let historical_parent = second_row.transparent_inputs[0]
        .spent_outpoint
        .as_ref()
        .ok_or_else(|| eyre!("block transaction input missing historical outpoint"))?;
    assert_eq!(
        historical_parent.transaction_id,
        encode_rpc_transaction_id_hex(TransactionId::from_bytes([0xA1; 32]))
    );
    assert_eq!(historical_parent.output_index, 4);
    assert_eq!(second_row.transparent_inputs[0].value_zat, Some(60_000));
    assert_eq!(
        second_row.transparent_inputs[0].script_pub_key.as_deref(),
        Some([0x53].as_slice())
    );
    let same_block_parent = second_row.transparent_inputs[1]
        .spent_outpoint
        .as_ref()
        .ok_or_else(|| eyre!("block transaction input missing same-block outpoint"))?;
    assert_eq!(same_block_parent.transaction_id, transaction_id_strings[0]);
    assert_eq!(same_block_parent.output_index, 0);
    assert_eq!(second_row.transparent_inputs[1].value_zat, Some(21_000));
    assert_eq!(
        second_row.transparent_inputs[1].script_pub_key.as_deref(),
        Some([0x51].as_slice())
    );

    let unavailable_row = &response.transactions[2];
    assert!(unavailable_row.public_facts.is_none());
    assert!(unavailable_row.transparent_outputs.is_empty());
    assert!(unavailable_row.transparent_inputs.is_empty());
    Ok(())
}

/// The upstream-observation probe surfaces the cached `UpstreamHealthSnapshot`
/// on every `ExplorerFreshness` once the probe has fired.
///
/// Wires a stub `NodeSource` that reports a synthetic snapshot, spawns the
/// adapter's background probe at a short cadence, and asserts an admitted
/// block-activity response carries the same fields the stub returned.
#[tokio::test]
async fn explorer_query_freshness_carries_upstream_observation_after_probe_fires() -> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store(&chain_fixture)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let explorer_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(seeded_materialized_view_store.secondary_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .build()
    .await?;
    let probe_cancel = CancellationToken::new();
    let probe_handle = adapter.spawn_upstream_observation_probe(
        Arc::new(StubUpstreamSource::ready(2_530_000, 2_544_375, 0.9943)),
        Duration::from_millis(10),
        probe_cancel.clone(),
    );
    let explorer_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = await_with_retry(explorer_addr).await?;
    let mut client = ExplorerQueryClient::new(channel);

    // The probe loop waits one `poll_interval` before its first tick. Spin
    // a few requests with a short pause between them so the test passes
    // deterministically once the first snapshot lands.
    let mut observed_upstream = None;
    for _ in 0..50 {
        let response = client
            .block_activity_distribution(BlockActivityDistributionRequest {
                start_height: 0,
                end_height: 1,
            })
            .await?
            .into_inner();
        let upstream_tip = response
            .freshness
            .and_then(|freshness| freshness.chain_view)
            .and_then(|chain_view| chain_view.upstream_tip);
        if let Some(upstream) = upstream_tip {
            observed_upstream = Some(upstream);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let upstream = observed_upstream
        .ok_or_else(|| eyre!("upstream observation probe never refreshed the cached snapshot"))?;
    assert_eq!(upstream.committed_height, Some(2_530_000));
    assert_eq!(upstream.estimated_height, Some(2_544_375));

    let server_info = client.server_info(ServerInfoRequest {}).await?.into_inner();
    let server_info_upstream = server_info
        .freshness
        .and_then(|freshness| freshness.chain_view)
        .and_then(|chain_view| chain_view.upstream_tip)
        .ok_or_else(|| eyre!("ServerInfo freshness missing upstream observation"))?;
    assert_eq!(server_info_upstream.committed_height, Some(2_530_000));
    assert_eq!(server_info_upstream.estimated_height, Some(2_544_375));

    probe_cancel.cancel();
    let _ = probe_handle.await;
    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

#[tokio::test]
async fn explorer_query_serves_recorded_chain_reorg_history() -> Result<()> {
    let store_fixture = StoreFixture::open()?;
    seed_recorded_chain_reorg(&store_fixture)?;

    let seeded_materialized_view_store =
        seeded_reorg_history_materialized_view_store(&store_fixture)?;
    let (mut client, explorer_handle) = spawn_explorer_query_server_with_materialized_view_store(
        seeded_materialized_view_store.secondary_store,
    )
    .await?;

    assert_reorg_history_capability(&mut client).await?;
    let reorg_cursor = assert_recorded_reorg_history_page(&mut client).await?;
    assert_reorg_history_empty_after(&mut client, reorg_cursor).await?;

    explorer_handle.abort();
    let _ = explorer_handle.await;
    Ok(())
}

fn seed_recorded_chain_reorg(store_fixture: &StoreFixture) -> Result<()> {
    let initial_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let settled_block = initial_chain
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("initial fixture missing height 1"))?;
    let mut initial_epoch = initial_chain
        .chain_epoch(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("initial fixture missing chain epoch"))?;
    initial_epoch.settled_tip_height = settled_block.height;
    initial_epoch.settled_tip_hash = settled_block.hash;
    let initial_block_headers = initial_chain.block_header_artifacts();
    let initial_block_replay_envelopes = initial_block_headers
        .iter()
        .map(|block_header| encode_fixture_block_replay(block_header, &[]))
        .collect();
    store_fixture.chain_store().commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            initial_block_headers,
            initial_block_replay_envelopes,
            initial_chain.compact_block_artifacts(),
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
        }),
    )?;

    let replacement_chain = initial_chain.fork_at(BlockHeight::new(2))?.extend_blocks(1);
    let replacement_block = replacement_chain
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("replacement fixture missing height 2"))?;
    let mut replacement_epoch = replacement_chain
        .chain_epoch(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("replacement fixture missing chain epoch"))?;
    replacement_epoch.settled_tip_height = settled_block.height;
    replacement_epoch.settled_tip_hash = settled_block.hash;
    let replacement_block_header = replacement_block.block_header_artifact();
    let replacement_replay = encode_fixture_block_replay(&replacement_block_header, &[]);
    store_fixture.chain_store().commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block_header],
            vec![replacement_replay],
            vec![replacement_block.compact_block_artifact()],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;
    Ok(())
}

fn seeded_reorg_history_materialized_view_store(
    store_fixture: &StoreFixture,
) -> Result<SeededMaterializedViewStore> {
    let primary_materialized_view_store = zinder_ingest::open_primary_materialized_view_store(
        store_fixture.tempdir_path(),
        Network::ZcashRegtest,
        zinder_materialized_views::MaterializedViewPreset::Explorer,
        zinder_store::RocksDbResourceBudget::for_local_tests(),
    )?;
    record_reorg_incidents(store_fixture, &primary_materialized_view_store)?;
    let reorg_cursor = primary_materialized_view_store
        .get_chain_event_cursor(REORG_INCIDENTS_CONSUMER_NAME)?
        .ok_or_else(|| eyre!("reorg incidents cursor missing after materialized-view replay"))?;
    assert!(!reorg_cursor.is_empty());

    let materialized_view_secondary_tempdir = tempfile::tempdir()?;
    let materialized_view_store = MaterializedViewStore::open_secondary(
        MaterializedViewStore::path_for_canonical(store_fixture.tempdir_path()),
        materialized_view_secondary_tempdir.path(),
        Network::ZcashRegtest,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: MaterializedViewStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    materialized_view_store.try_catch_up()?;
    Ok(SeededMaterializedViewStore {
        _tempdir: materialized_view_secondary_tempdir,
        secondary_store: materialized_view_store,
    })
}

async fn assert_reorg_history_capability(client: &mut ExplorerQueryClient<Channel>) -> Result<()> {
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
    assert_advertises_capability(&common.capabilities, EXPLORER_CHAIN_REORG_HISTORY_V1);
    Ok(())
}

async fn assert_recorded_reorg_history_page(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<Vec<u8>> {
    let first_page = client
        .chain_reorg_history(ChainReorgHistoryRequest {
            max_events: 1,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    let freshness = first_page
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("reorg history response missing freshness"))?;
    assert_eq!(
        freshness.capability_version,
        EXPLORER_CHAIN_REORG_HISTORY_V1
    );
    assert!(first_page.next_cursor.is_empty());
    assert_eq!(first_page.events.len(), 1);
    let reorg = first_page
        .events
        .first()
        .ok_or_else(|| eyre!("reorg event missing"))?;
    let reorg_cursor = reorg.cursor.clone();
    assert_eq!(reorg.event_sequence, 2);
    assert!(!reorg_cursor.is_empty());
    assert_eq!(reorg.chain_epoch_id, 2);
    assert_eq!(
        reorg
            .visible_tip
            .as_ref()
            .ok_or_else(|| eyre!("reorg visible tip missing"))?
            .height,
        2
    );
    assert_eq!(
        reorg
            .settled_tip
            .as_ref()
            .ok_or_else(|| eyre!("reorg settled tip missing"))?
            .height,
        1
    );
    let reverted = reorg
        .reverted
        .as_ref()
        .ok_or_else(|| eyre!("reorg reverted range missing"))?;
    assert_eq!(reverted.start_height, 2);
    assert_eq!(reverted.end_height, 2);
    let committed = reorg
        .committed
        .as_ref()
        .ok_or_else(|| eyre!("reorg committed range missing"))?;
    assert_eq!(committed.start_height, 2);
    assert_eq!(committed.end_height, 2);
    Ok(reorg_cursor)
}

async fn assert_reorg_history_empty_after(
    client: &mut ExplorerQueryClient<Channel>,
    cursor: Vec<u8>,
) -> Result<()> {
    let empty_page = client
        .chain_reorg_history(ChainReorgHistoryRequest {
            max_events: 10,
            from_cursor: cursor,
        })
        .await?
        .into_inner();
    assert!(empty_page.events.is_empty());
    assert!(empty_page.next_cursor.is_empty());
    Ok(())
}

/// Minimal `NodeSource` stub used by the upstream-observation probe test.
///
/// Returns a fixed [`UpstreamHealthSnapshot`] from
/// `poll_upstream_health` and surfaces `NodeCapabilityMissing` from
/// every other method. Used only to exercise the adapter's
/// upstream-observation probe; never hits a real node.
struct StubUpstreamSource {
    snapshot: UpstreamHealthSnapshot,
}

impl StubUpstreamSource {
    fn ready(committed: u32, estimated: u32, progress: f64) -> Self {
        Self {
            snapshot: UpstreamHealthSnapshot::ready(
                UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT,
                Some(committed),
                Some(estimated),
                Some(progress),
            ),
        }
    }
}

#[async_trait]
impl NodeSource for StubUpstreamSource {
    fn admitted_capabilities(&self) -> Option<NodeCapabilities> {
        Some(NodeCapabilities::default())
    }

    async fn fetch_block_at(&self, _height: BlockHeight) -> Result<SourceBlock, SourceError> {
        Err(SourceError::NodeCapabilityMissing {
            capability: zinder_source::NodeCapability::ReadinessProbe,
        })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        Err(SourceError::NodeCapabilityMissing {
            capability: zinder_source::NodeCapability::ReadinessProbe,
        })
    }

    async fn poll_upstream_health(&self) -> Result<UpstreamHealthSnapshot, SourceError> {
        Ok(self.snapshot.clone())
    }
}

async fn spawn_wallet_query_server(
    chain_fixture: &ChainFixture,
) -> Result<(StoreFixture, SocketAddr, ServerHandle)> {
    spawn_wallet_query_server_with_bearer_token(chain_fixture, None).await
}

async fn spawn_wallet_query_server_with_bearer_token(
    chain_fixture: &ChainFixture,
    bearer_token: Option<BearerToken>,
) -> Result<(StoreFixture, SocketAddr, ServerHandle)> {
    let store_fixture = StoreFixture::with_chain_committed(chain_fixture, ChainEpochId::new(1))?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let server = tonic::service::interceptor::InterceptedService::new(
        adapter.into_server(),
        BearerTokenServerInterceptor::new(bearer_token),
    );
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let _channel = await_with_retry(addr).await?;
    Ok((store_fixture, addr, handle))
}

async fn spawn_wallet_serving_query_server(
    chain_fixture: &ChainFixture,
) -> Result<(WalletServingStoreFixture, SocketAddr, ServerHandle)> {
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let mut store_fixture = WalletServingStoreFixture::from_chain(chain_fixture, &activations)?;
    let (canonical_reader, wallet_reader) = store_fixture.take_readers()?;
    let serving_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(canonical_reader),
        Arc::new(wallet_reader),
    )?);
    let wallet_query = WalletServingQuery::from_serving_pair_slot(
        WalletServingPairSlot::new(serving_pair),
        (),
        activations,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let _channel = await_with_retry(addr).await?;
    Ok((store_fixture, addr, handle))
}

fn seeded_block_summary_materialized_view_store(
    chain_fixture: &ChainFixture,
) -> Result<SeededMaterializedViewStore> {
    seeded_block_summary_materialized_view_store_with_transaction_ids(chain_fixture, &[])
}

fn seeded_block_summary_materialized_view_store_with_transaction_ids(
    chain_fixture: &ChainFixture,
    transaction_ids: &[String],
) -> Result<SeededMaterializedViewStore> {
    let tempdir = tempfile::tempdir()?;
    let primary_path = tempdir.path().join("materialized-view-primary");
    let secondary_path = tempdir.path().join("materialized-view-secondary");
    let primary_store = MaterializedViewStore::open(
        &primary_path,
        Network::ZcashRegtest,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: MaterializedViewStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    seed_block_summary(&primary_store, chain_fixture, transaction_ids)?;

    let secondary_store = MaterializedViewStore::open_secondary(
        &primary_path,
        &secondary_path,
        Network::ZcashRegtest,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: MaterializedViewStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    secondary_store.try_catch_up()?;
    Ok(SeededMaterializedViewStore {
        _tempdir: tempdir,
        secondary_store,
    })
}

/// Replays the fixture's retained chain events into the reorg-incident log.
fn record_reorg_incidents(
    store_fixture: &StoreFixture,
    materialized_view_store: &MaterializedViewStore,
) -> Result<()> {
    let blocks = std::collections::HashMap::new();
    for envelope in store_fixture.chain_store().chain_event_history(
        zinder_store::ChainEventHistoryRequest::with_default_limit(None),
    )? {
        let mut reorg_incidents = zinder_materialized_views::ReorgIncidentsConsumer::new();
        let mut block_consumers: [&mut dyn zinder_materialized_views::BlockKeyedConsumer; 0] = [];
        let mut event_consumers: [&mut dyn zinder_materialized_views::MaterializedViewConsumer; 1] =
            [&mut reorg_incidents];
        materialized_view_store.write_chain_event_chunk_with_event_consumers(
            zinder_materialized_views::ChainEventDispatchConsumers {
                block_consumers: &mut block_consumers,
                event_consumers: &mut event_consumers,
            },
            zinder_materialized_views::ChainEventDispatchInputs {
                chain_epoch: envelope.chain_epoch,
                chain_event: &envelope.event,
                chain_cursor: envelope.cursor.as_bytes(),
                event_sequence: envelope.event_sequence,
                settled_tip_height: envelope.settled_tip_height,
            },
            &blocks,
            true,
        )?;
    }
    Ok(())
}

fn seed_block_summary(
    materialized_view_store: &MaterializedViewStore,
    chain_fixture: &ChainFixture,
    transaction_ids: &[String],
) -> Result<()> {
    let fixture_block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let record = BlockSummaryRecord {
        summary: Some(BlockSummary {
            block_height: fixture_block.height.value(),
            block_hash: encode_rpc_block_hash_hex(fixture_block.hash),
            block_time_unix_seconds: i64::from(fixture_block.block_time_seconds),
            transaction_count: u32::try_from(transaction_ids.len())?,
            previous_block_hash: encode_rpc_block_hash_hex(fixture_block.parent_hash),
            total_size_bytes: u64::try_from(fixture_block.raw_block_bytes.len())?,
            fees_collected_zat: 0,
            paid_fees_collected_zat: None,
            coinbase_reward_zat: 0,
            sapling_output_count: 0,
            orchard_action_count: 0,
            ironwood_action_count: 0,
            confirmations: 0,
            is_canonical: true,
        }),
        transaction_ids: transaction_ids.to_vec(),
        fee_transaction_count: 0,
        min_zip317_conventional_fee_zat: 0,
        max_zip317_conventional_fee_zat: 0,
    };
    materialized_view_store.put_consumer(
        BLOCK_SUMMARY_COLUMN_FAMILY,
        &BlockSummaryConsumer::key_for_height(fixture_block.height),
        &record.encode_to_vec(),
    )?;
    materialized_view_store.put_chain_event_cursor(BLOCK_SUMMARY_CONSUMER_NAME, &[1])?;
    Ok(())
}

async fn spawn_explorer_query_server(
    materialized_view_store: MaterializedViewStore,
    wallet_addr: SocketAddr,
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .build()
    .await?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = await_with_retry(addr).await?;
    Ok((ExplorerQueryClient::new(channel), handle))
}

async fn spawn_explorer_query_server_with_canonical_store(
    materialized_view_store: MaterializedViewStore,
    canonical_store: SecondaryChainStore,
    wallet_addr: SocketAddr,
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store)
    .with_canonical_store(canonical_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .build()
    .await?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = await_with_retry(addr).await?;
    Ok((ExplorerQueryClient::new(channel), handle))
}

async fn spawn_explorer_query_server_with_materialized_view_store(
    materialized_view_store: MaterializedViewStore,
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store)
    .build()
    .await?;
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
    let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata {
        network: Network::ZcashRegtest,
    })
    .with_bearer_token(server_token.clone())
    .build()
    .await?;
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

/// Extracts the visible tip from a freshness envelope's chain view, the common
/// path the overview-snapshot coherence assertions read.
fn freshness_visible_tip(
    freshness: &zinder_proto::v1::explorer::ExplorerFreshness,
) -> Result<zinder_proto::v1::wallet::BlockTip> {
    freshness
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .and_then(|chain_epoch| chain_epoch.visible_tip.clone())
        .ok_or_else(|| eyre!("freshness missing chain_view.chain_epoch.visible_tip"))
}
