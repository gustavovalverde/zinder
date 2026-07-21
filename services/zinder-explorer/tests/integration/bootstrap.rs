#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! Smoke test that boots an `ExplorerQueryGrpcAdapter` against an in-process
//! tonic server and verifies `ServerInfo` returns the expected capability set.

use std::{net::SocketAddr, num::NonZeroU32, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::{Result, eyre};
use prost::Message as _;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, ChainEpochId,
    MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, Network, TransactionId, TransactionLocation,
    TransparentAddressScriptHash, TransparentInputFact, TransparentOutPoint, TransparentOutputFact,
    TransparentSpendFact,
    wire::{encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex},
};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, BlockSummaryConsumer,
    MaterializedViewStore, MaterializedViewStoreOptions, RECENT_TRANSACTIONS_COLUMN_FAMILY,
    RECENT_TRANSACTIONS_SCHEMA, REORG_INCIDENTS_CONSUMER_NAME, RecentTransactionsConsumer,
    TRANSACTION_FEES_SCHEMA, TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME, TransparentAddressDeltasConsumer,
};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1, EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_BLOCK_TRANSACTIONS_V2, EXPLORER_CHAIN_REORG_HISTORY_V1,
    EXPLORER_OVERVIEW_SNAPSHOT_V1, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4,
    EXPLORER_TRANSACTION_FEES_V1, EXPLORER_TRANSACTION_RECENT_V1,
    EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
};
use zinder_proto::v1::explorer::{
    BlockActivityDistributionRequest, BlockDetailRequest, BlockProductionSeriesRequest,
    BlockSummariesInRangeRequest, BlockSummary, BlockSummaryRecord, BlockTransactionsResponse,
    ChainReorgHistoryRequest, OverviewSnapshotRequest, PrevoutResolutionStatus,
    RecentTransactionEntry, RecentTransactionsRequest, ServerInfoRequest, TransactionDetailRequest,
    TransactionDetailResponse, TransparentAddressDeltasRecord, TransparentAddressDeltasRequest,
    TransparentDeltaKind, block_detail_request, explorer_query_client::ExplorerQueryClient,
};
use zinder_proto::v1::wallet::{AddressLookup, address_lookup::Selector as AddressSelector};
use zinder_proto::wire::{TRANSPARENT_DELTA_KIND_RECEIVED_BYTE, TRANSPARENT_DELTA_KIND_SPENT_BYTE};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_runtime::connect_zinder_grpc;
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceError,
    UPSTREAM_HEALTH_SOURCE_ZEBRA_READY_ENDPOINT, UpstreamHealthSnapshot,
};
use zinder_store::{
    ChainEpochArtifacts, ChainStoreOptions, RawBlobRetention, ReorgWindowChange,
    SecondaryChainStore,
};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, StoreFixture, encode_fixture_block_replay,
    sample_regtest_upgrade_activations, synthetic_transaction_public_facts,
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

/// Without a configured `wallet_query_endpoint`, the wallet-backed explorer
/// capabilities are omitted from `ServerInfo` and the corresponding methods
/// return `FAILED_PRECONDITION`.
///
/// This pins the operational contract that capability advertisement gates on
/// a wired federation, not on the binary's mere presence.
#[tokio::test]
async fn explorer_query_failed_precondition_without_wallet_query_endpoint() -> Result<()> {
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
        .ok_or_else(|| eyre!("expected FAILED_PRECONDITION without wallet_query_endpoint"))?;
    assert_eq!(detail_status.code(), tonic::Code::FailedPrecondition);

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
        .ok_or_else(|| eyre!("expected FAILED_PRECONDITION without materialized-view store"))?;
    assert_eq!(overview_status.code(), tonic::Code::FailedPrecondition);

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn explorer_query_serves_block_summary_from_secondary_materialized_view_store() -> Result<()>
{
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store(&chain_fixture)?;
    let (mut client, explorer_handle) =
        spawn_explorer_query_server(seeded_materialized_view_store.secondary_store, wallet_addr)
            .await?;

    let server_info = client.server_info(ServerInfoRequest {}).await?.into_inner();
    let explorer_info = server_info
        .info
        .as_ref()
        .ok_or_else(|| eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_advertises_capability(&common.capabilities, EXPLORER_BLOCK_SUMMARY_V1);
    assert_advertises_capability(&common.capabilities, EXPLORER_TRANSACTION_FEES_V1);

    let chain_view = server_info
        .freshness
        .as_ref()
        .and_then(|freshness| freshness.chain_view.as_ref())
        .ok_or_else(|| eyre!("ServerInfo freshness missing chain_view"))?;
    assert!(
        chain_view.chain_epoch.is_none(),
        "ServerInfo makes no snapshot-consistency claim",
    );
    let indexed_tip = chain_view
        .indexed_tip
        .as_ref()
        .and_then(|indexed_tip| indexed_tip.tip.as_ref())
        .ok_or_else(|| eyre!("ServerInfo freshness missing indexed_tip"))?;
    let fixture_block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    assert_eq!(indexed_tip.height, 1);
    assert_eq!(
        indexed_tip.hash,
        encode_rpc_block_hash_hex(fixture_block.hash)
    );

    let response = client
        .block_summaries_in_range(BlockSummariesInRangeRequest {
            start_height: 1,
            end_height: 1,
            at_epoch_id: None,
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

#[tokio::test]
async fn recent_transactions_returns_exact_newest_hundred_in_descending_block_position()
-> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_materialized_view_store =
        seeded_recent_transactions_materialized_view_store(&chain_fixture)?;
    let (mut client, explorer_handle) =
        spawn_explorer_query_server(seeded_materialized_view_store.secondary_store, wallet_addr)
            .await?;

    let server_info = client.server_info(ServerInfoRequest {}).await?.into_inner();
    let capabilities = server_info
        .info
        .as_ref()
        .and_then(|info| info.common.as_ref())
        .ok_or_else(|| eyre!("explorer server info missing common descriptor"))?
        .capabilities
        .as_slice();
    assert_advertises_capability(capabilities, EXPLORER_TRANSACTION_RECENT_V1);

    let mut first_stream = client
        .recent_transactions(RecentTransactionsRequest {
            max_entries: 100,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    let first_chunk = first_stream
        .message()
        .await?
        .ok_or_else(|| eyre!("recent-transactions stream ended before its first chunk"))?;
    assert!(first_stream.message().await?.is_none());
    assert_eq!(first_chunk.entries.len(), 100);
    let expected_first_page = (1_u32..=100)
        .rev()
        .map(recent_transaction_id)
        .collect::<Vec<_>>();
    assert_eq!(
        first_chunk
            .entries
            .iter()
            .map(|entry| entry.transaction_id.clone())
            .collect::<Vec<_>>(),
        expected_first_page,
    );
    assert!(
        first_chunk
            .entries
            .iter()
            .all(|entry| entry.block_height == 2)
    );

    let mut resumed_stream = client
        .recent_transactions(RecentTransactionsRequest {
            max_entries: 10,
            from_cursor: first_chunk.cursor,
        })
        .await?
        .into_inner();
    let resumed_chunk = resumed_stream
        .message()
        .await?
        .ok_or_else(|| eyre!("resumed recent-transactions stream ended before its first chunk"))?;
    assert!(resumed_stream.message().await?.is_none());
    assert_eq!(resumed_chunk.entries.len(), 2);
    assert_eq!(
        resumed_chunk.entries[0].transaction_id,
        recent_transaction_id(0)
    );
    assert_eq!(resumed_chunk.entries[0].block_height, 2);
    assert_eq!(
        resumed_chunk.entries[1].transaction_id,
        recent_transaction_id(1_000),
    );
    assert_eq!(resumed_chunk.entries[1].block_height, 1);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
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

#[tokio::test]
async fn explorer_query_transaction_detail_preserves_canonical_transparent_rows() -> Result<()> {
    let (chain_fixture, transaction_id_strings) = stateless_transaction_detail_chain_fixture()?;
    let (_wallet_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let (mut client, explorer_handle) = spawn_stateless_explorer_query_server(wallet_addr).await?;
    let first = client
        .transaction_detail(TransactionDetailRequest {
            transaction_id: transaction_id_strings[0].clone(),
            at_epoch_id: Some(1),
        })
        .await?
        .into_inner();
    assert_transaction_detail_output_spends(&first, &transaction_id_strings)?;

    let second = client
        .transaction_detail(TransactionDetailRequest {
            transaction_id: transaction_id_strings[1].clone(),
            at_epoch_id: Some(1),
        })
        .await?
        .into_inner();
    assert_transaction_detail_inputs(&second, &transaction_id_strings)?;

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

fn stateless_transaction_detail_chain_fixture() -> Result<(ChainFixture, Vec<String>)> {
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .with_raw_blob_retention(RawBlobRetention::Transactions)
        .extend_blocks(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let activations = sample_regtest_upgrade_activations();

    let first_bytes = transparent_v1_transaction_bytes(
        &[TransparentOutPoint::new(
            TransactionId::from_bytes([0xA5; 32]),
            0,
        )],
        &[(21_000, vec![0x51]), (34_000, vec![0x52])],
    )?;
    let first = parsed_transaction_rows(block.height, block.hash, 0, first_bytes, &activations)?;
    let parent_bytes = transparent_v1_transaction_bytes(
        &[TransparentOutPoint::new(
            TransactionId::from_bytes([0xA1; 32]),
            0,
        )],
        &[
            (1_000, vec![0x51]),
            (2_000, vec![0x51]),
            (3_000, vec![0x51]),
            (4_000, vec![0x51]),
            (60_000, vec![0x53]),
        ],
    )?;
    let parent = parsed_transaction_rows(block.height, block.hash, 2, parent_bytes, &activations)?;
    let parent_transaction_id = parent.location.transaction_id;
    let second_bytes = transparent_v1_transaction_bytes(
        &[
            TransparentOutPoint::new(parent_transaction_id, 4),
            TransparentOutPoint::new(first.location.transaction_id, 0),
        ],
        &[],
    )?;
    let second = parsed_transaction_rows(block.height, block.hash, 1, second_bytes, &activations)?;
    let spend = TransparentSpendFact::new(
        TransparentOutPoint::new(first.location.transaction_id, 0),
        1,
        second.location.transaction_id,
        1,
        block.height,
        block.hash,
        21_000,
        TransparentAddressScriptHash::of_script_pub_key(&[0x51]),
        block.height,
        block.hash,
    );
    let transaction_ids = vec![
        encode_rpc_transaction_id_hex(first.location.transaction_id),
        encode_rpc_transaction_id_hex(second.location.transaction_id),
        encode_rpc_transaction_id_hex(parent_transaction_id),
    ];
    Ok((
        base_fixture
            .with_transaction_rows(first)
            .with_transaction_rows(second)
            .with_transaction_rows(parent)
            .with_transparent_spend_fact(spend),
        transaction_ids,
    ))
}

fn parsed_transaction_rows(
    block_height: BlockHeight,
    block_hash: BlockHash,
    transaction_index: u32,
    raw_transaction_bytes: Vec<u8>,
    activations: &zinder_core::NetworkUpgradeActivations,
) -> Result<FixtureTransactionRows> {
    let parsed = zinder_source::parse_transaction_public_fact_set(
        &raw_transaction_bytes,
        Some(block_height),
        activations,
    )?;
    let transaction_id = parsed.public_facts.transaction_id;
    let rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block_height,
        block_hash,
        transaction_index,
        raw_transaction_bytes,
    );
    Ok(FixtureTransactionRows {
        facts: zinder_core::TransactionFactsArtifact::new(rows.location, parsed.public_facts)
            .with_transparent_facts(parsed.transparent_inputs, parsed.transparent_outputs),
        intrinsic_value_balances: Some(parsed.intrinsic_value_balances),
        ..rows
    })
}

fn transparent_v1_transaction_bytes(
    inputs: &[TransparentOutPoint],
    outputs: &[(u64, Vec<u8>)],
) -> Result<Vec<u8>> {
    let mut bytes = vec![1, 0, 0, 0];
    append_compact_size(&mut bytes, inputs.len())?;
    for input in inputs {
        bytes.extend_from_slice(&input.transaction_id.as_bytes());
        bytes.extend_from_slice(&input.output_index.to_le_bytes());
        if input.is_coinbase_sentinel() {
            bytes.extend_from_slice(&[2, 1, 1]);
        } else {
            bytes.push(0);
        }
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    }
    append_compact_size(&mut bytes, outputs.len())?;
    for (value_zat, script_pub_key) in outputs {
        bytes.extend_from_slice(&value_zat.to_le_bytes());
        append_compact_size(&mut bytes, script_pub_key.len())?;
        bytes.extend_from_slice(script_pub_key);
    }
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    Ok(bytes)
}

fn append_compact_size(bytes: &mut Vec<u8>, count: usize) -> Result<()> {
    if count < 253 {
        bytes.push(u8::try_from(count)?);
    } else if let Ok(encoded_count) = u16::try_from(count) {
        bytes.push(253);
        bytes.extend_from_slice(&encoded_count.to_le_bytes());
    } else if let Ok(encoded_count) = u32::try_from(count) {
        bytes.push(254);
        bytes.extend_from_slice(&encoded_count.to_le_bytes());
    } else {
        bytes.push(255);
        bytes.extend_from_slice(&u64::try_from(count)?.to_le_bytes());
    }
    Ok(())
}

#[tokio::test]
async fn transaction_detail_batches_spent_output_lookup_beyond_wallet_request_limit() -> Result<()>
{
    let (chain_fixture, transaction_ids) = many_output_spend_chain_fixture()?;
    let (_wallet_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let (mut client, explorer_handle) = spawn_stateless_explorer_query_server(wallet_addr).await?;

    let detail = client
        .transaction_detail(TransactionDetailRequest {
            transaction_id: transaction_ids[0].clone(),
            at_epoch_id: Some(1),
        })
        .await?
        .into_inner();

    assert_eq!(
        detail.transparent_outputs.len(),
        MAX_TRANSPARENT_OUTPUTS_PER_REQUEST + 1
    );
    assert!(detail.transparent_outputs[0].spent_by.is_none());
    let final_output = detail
        .transparent_outputs
        .last()
        .ok_or_else(|| eyre!("transaction detail missing final transparent output"))?;
    assert_eq!(
        usize::try_from(final_output.output_index)?,
        MAX_TRANSPARENT_OUTPUTS_PER_REQUEST
    );
    assert_eq!(
        final_output
            .spent_by
            .as_ref()
            .ok_or_else(|| eyre!("final transparent output missing canonical spender"))?
            .spending_transaction_id,
        transaction_ids[1]
    );

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

fn many_output_spend_chain_fixture() -> Result<(ChainFixture, Vec<String>)> {
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .with_raw_blob_retention(RawBlobRetention::Transactions)
        .extend_blocks(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing"))?;
    let output_count = MAX_TRANSPARENT_OUTPUTS_PER_REQUEST + 1;
    let outputs = (0..output_count)
        .map(|output_index| Ok((u64::try_from(output_index)? + 1, vec![0x51])))
        .collect::<Result<Vec<_>>>()?;
    let activations = sample_regtest_upgrade_activations();
    let creating_bytes = transparent_v1_transaction_bytes(
        &[TransparentOutPoint::new(
            TransactionId::from_bytes([0xD0; 32]),
            0,
        )],
        &outputs,
    )?;
    let creating_transaction =
        parsed_transaction_rows(block.height, block.hash, 0, creating_bytes, &activations)?;
    let creating_transaction_id = creating_transaction.location.transaction_id;
    let spent_outpoint = TransparentOutPoint::new(
        creating_transaction_id,
        u32::try_from(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST)?,
    );
    let spending_bytes = transparent_v1_transaction_bytes(&[spent_outpoint], &[])?;
    let spending_transaction =
        parsed_transaction_rows(block.height, block.hash, 1, spending_bytes, &activations)?;
    let spending_transaction_id = spending_transaction.location.transaction_id;
    let final_output_value = u64::try_from(output_count)?;
    let spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        spending_transaction_id,
        1,
        block.height,
        block.hash,
        final_output_value,
        TransparentAddressScriptHash::of_script_pub_key(&[0x51]),
        block.height,
        block.hash,
    );
    let chain_fixture = base_fixture
        .with_transaction_rows(creating_transaction)
        .with_transaction_rows(spending_transaction)
        .with_transparent_spend_fact(spend);
    Ok((
        chain_fixture,
        vec![
            encode_rpc_transaction_id_hex(creating_transaction_id),
            encode_rpc_transaction_id_hex(spending_transaction_id),
        ],
    ))
}

fn assert_transaction_detail_output_spends(
    detail: &TransactionDetailResponse,
    transaction_ids: &[String],
) -> Result<()> {
    assert_eq!(detail.transparent_inputs.len(), 1);
    assert!(detail.transparent_inputs[0].value_zat.is_none());
    assert_eq!(
        detail.prevout_resolution_status,
        PrevoutResolutionStatus::Partial as i32
    );
    assert_eq!(detail.paid_fee_zat, None);
    assert_eq!(detail.transparent_outputs.len(), 2);
    assert_eq!(detail.transparent_outputs[0].output_index, 0);
    let first_output = detail.transparent_outputs[0]
        .output
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail missing first transparent output"))?;
    assert_eq!(first_output.value_zat, 21_000);
    assert_eq!(first_output.script_pub_key, [0x51]);
    let first_spend = detail.transparent_outputs[0]
        .spent_by
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail missing first output spender"))?;
    assert_eq!(first_spend.spending_transaction_id, transaction_ids[1]);
    assert_eq!(first_spend.input_index, 1);
    let first_spending_block = first_spend
        .spending_block
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail output spender missing block"))?;
    assert_eq!(first_spending_block.height, 1);
    assert_eq!(detail.transparent_outputs[1].output_index, 1);
    let second_output = detail.transparent_outputs[1]
        .output
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail missing second transparent output"))?;
    assert_eq!(second_output.value_zat, 34_000);
    assert_eq!(second_output.script_pub_key, [0x52]);
    assert!(detail.transparent_outputs[1].spent_by.is_none());
    Ok(())
}

fn assert_transaction_detail_inputs(
    detail: &TransactionDetailResponse,
    transaction_ids: &[String],
) -> Result<()> {
    assert_eq!(detail.transparent_outputs.len(), 0);
    assert_eq!(detail.transparent_inputs.len(), 2);
    let transparent_input = &detail.transparent_inputs[0];
    let spent_outpoint = transparent_input
        .spent_outpoint
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail input missing spent outpoint"))?;
    assert_eq!(spent_outpoint.transaction_id, transaction_ids[2]);
    assert_eq!(spent_outpoint.output_index, 4);
    assert_eq!(
        detail.prevout_resolution_status,
        PrevoutResolutionStatus::Resolved as i32
    );
    assert_eq!(transparent_input.input_index, 0);
    assert_eq!(transparent_input.value_zat, Some(60_000));
    assert_eq!(
        transparent_input.script_pub_key.as_deref(),
        Some([0x53].as_slice())
    );
    let same_block_input = &detail.transparent_inputs[1];
    let same_block_outpoint = same_block_input
        .spent_outpoint
        .as_ref()
        .ok_or_else(|| eyre!("transaction detail same-block input missing spent outpoint"))?;
    assert_eq!(same_block_input.input_index, 1);
    assert_eq!(same_block_outpoint.transaction_id, transaction_ids[0]);
    assert_eq!(same_block_outpoint.output_index, 0);
    assert_eq!(same_block_input.value_zat, Some(21_000));
    assert_eq!(
        same_block_input.script_pub_key.as_deref(),
        Some([0x51].as_slice())
    );

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

/// `OverviewSnapshot` returns one coherent bundle anchored to a single chain epoch.
///
/// Two consecutive calls against the same upstream tip return the same
/// `tip_hash`; the response's `recent_blocks[0]` carries the seeded
/// block's height and timestamp; the bundle's single
/// `freshness.capability_version` is the overview capability string.
#[tokio::test]
async fn explorer_query_serves_overview_snapshot_with_seeded_materialized_view_store() -> Result<()>
{
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
    assert_advertises_capability(&common.capabilities, EXPLORER_OVERVIEW_SNAPSHOT_V1);

    let first = client
        .overview_snapshot(OverviewSnapshotRequest {
            recent_blocks_limit: 0,
            recent_transactions_limit: 0,
            mempool_window_seconds: 0,
            fee_summary_block_count: 0,
        })
        .await?
        .into_inner();
    let first_freshness = first
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("overview response missing freshness"))?;
    let first_visible_tip = freshness_visible_tip(first_freshness)?;
    assert_eq!(
        first_freshness.capability_version,
        EXPLORER_OVERVIEW_SNAPSHOT_V1
    );
    assert_eq!(first_visible_tip.height, 1);
    assert_eq!(first.recent_blocks.len(), 1);
    assert_eq!(first.recent_blocks[0].block_height, 1);
    assert_eq!(first.recent_blocks[0].confirmations, 1);
    assert!(first.recent_blocks[0].is_canonical);
    assert_eq!(
        first.tip_block_time_unix_seconds,
        first.recent_blocks[0].block_time_unix_seconds
    );
    assert_eq!(first.value_pools.len(), 0);
    let first_mempool = first
        .mempool
        .as_ref()
        .ok_or_else(|| eyre!("mempool sub-field missing"))?;
    assert_eq!(first_mempool.transaction_count, 0);

    // Coherence guarantee: a second call against the same upstream tip
    // returns the same snapshot identity (tip_hash). The bundle never
    // straddles two tips.
    let second = client
        .overview_snapshot(OverviewSnapshotRequest {
            recent_blocks_limit: 0,
            recent_transactions_limit: 0,
            mempool_window_seconds: 0,
            fee_summary_block_count: 0,
        })
        .await?
        .into_inner();
    let second_freshness = second
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("second response missing freshness"))?;
    let second_visible_tip = freshness_visible_tip(second_freshness)?;
    assert_eq!(second_visible_tip.hash, first_visible_tip.hash);
    assert_eq!(second_visible_tip.height, first_visible_tip.height);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

/// The upstream-observation probe surfaces the cached `UpstreamHealthSnapshot`
/// on every `ExplorerFreshness` once the probe has fired.
///
/// Wires a stub `NodeSource` that reports a synthetic snapshot, spawns the
/// adapter's background probe at a short cadence, and asserts the resulting
/// `OverviewSnapshot` carries the same fields the stub returned.
#[tokio::test]
async fn explorer_query_freshness_carries_upstream_observation_after_probe_fires() -> Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded_materialized_view_store =
        seeded_block_summary_materialized_view_store(&chain_fixture)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let explorer_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(seeded_materialized_view_store.secondary_store)
    .with_wallet_query_endpoint(format!("http://{wallet_addr}"))
    .with_prevout_resolution_online(true);
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
            .overview_snapshot(OverviewSnapshotRequest {
                recent_blocks_limit: 0,
                recent_transactions_limit: 0,
                mempool_window_seconds: 0,
                fee_summary_block_count: 0,
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
        seeded_reorg_history_materialized_view_store(&store_fixture).await?;
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

async fn seeded_reorg_history_materialized_view_store(
    store_fixture: &StoreFixture,
) -> Result<SeededMaterializedViewStore> {
    let primary_materialized_view_store =
        zinder_ingest::open_primary_materialized_view_store_for_canonical(
            store_fixture.tempdir_path(),
            zinder_store::RocksDbResourceBudget::for_local_tests(),
        )?;
    zinder_ingest::catch_up_materialized_view_store_to_canonical(
        store_fixture.chain_store(),
        &primary_materialized_view_store,
        test_materialized_view_config(),
    )
    .await?;
    let reorg_cursor = primary_materialized_view_store
        .get_chain_event_cursor(REORG_INCIDENTS_CONSUMER_NAME)?
        .ok_or_else(|| eyre!("reorg incidents cursor missing after materialized-view replay"))?;
    assert!(!reorg_cursor.is_empty());

    let materialized_view_secondary_tempdir = tempfile::tempdir()?;
    let materialized_view_store = MaterializedViewStore::open_secondary(
        MaterializedViewStore::path_for_canonical(store_fixture.tempdir_path()),
        materialized_view_secondary_tempdir.path(),
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
    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities::default()
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

fn recent_transaction_id(marker: u32) -> String {
    let mut bytes = [0_u8; 32];
    bytes[..4].copy_from_slice(&marker.to_be_bytes());
    encode_rpc_transaction_id_hex(TransactionId::from_bytes(bytes))
}

fn seeded_recent_transactions_materialized_view_store(
    chain_fixture: &ChainFixture,
) -> Result<SeededMaterializedViewStore> {
    let tempdir = tempfile::tempdir()?;
    let primary_path = tempdir.path().join("materialized-view-primary");
    let secondary_path = tempdir.path().join("materialized-view-secondary");
    let primary_store = MaterializedViewStore::open(
        &primary_path,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: &[RECENT_TRANSACTIONS_SCHEMA, TRANSACTION_FEES_SCHEMA],
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    let older_block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block 1 missing"))?;
    let newer_block = chain_fixture
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture block 2 missing"))?;
    seed_recent_transaction_row(&primary_store, older_block, 0, 1_000)?;
    for position in 0_u32..=100 {
        seed_recent_transaction_row(&primary_store, newer_block, position, position)?;
    }

    let secondary_store = MaterializedViewStore::open_secondary(
        &primary_path,
        &secondary_path,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: &[RECENT_TRANSACTIONS_SCHEMA, TRANSACTION_FEES_SCHEMA],
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    secondary_store.try_catch_up()?;
    Ok(SeededMaterializedViewStore {
        _tempdir: tempdir,
        secondary_store,
    })
}

fn seed_recent_transaction_row(
    store: &MaterializedViewStore,
    block: &zinder_testkit::FixtureBlock,
    position: u32,
    marker: u32,
) -> Result<()> {
    let entry = RecentTransactionEntry {
        transaction_id: recent_transaction_id(marker),
        block_height: block.height.value(),
        block_hash: encode_rpc_block_hash_hex(block.hash),
        block_time_unix_seconds: i64::from(block.block_time_seconds),
        ..Default::default()
    };
    store.put_consumer(
        RECENT_TRANSACTIONS_COLUMN_FAMILY,
        &RecentTransactionsConsumer::key_for_row(block.height, position),
        &entry.encode_to_vec(),
    )?;
    Ok(())
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

fn test_materialized_view_config() -> zinder_ingest::MaterializedViewReplayConfig {
    zinder_ingest::MaterializedViewReplayConfig {
        replay_batch_blocks: NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN),
        replay_policy: zinder_ingest::MaterializedViewReplayPolicy::CanonicalFirst,
        memory_budget_bytes: None,
        memory_degrade_ratio: 0.85,
        memory_pause_ratio: 0.95,
        memory_resume_ratio: 0.75,
        min_replay_batch_blocks: NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN),
        startup_handoff_lag_blocks: 1_000,
    }
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
    let wallet_endpoint = format!("http://{wallet_addr}");
    let wallet_channel = connect_zinder_grpc(&wallet_endpoint, None).await?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store)
    .with_wallet_query_endpoint(wallet_endpoint)
    .with_admitted_transaction_detail_wallet_channel(wallet_channel)
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

async fn spawn_stateless_explorer_query_server(
    wallet_addr: SocketAddr,
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let wallet_endpoint = format!("http://{wallet_addr}");
    let wallet_channel = connect_zinder_grpc(&wallet_endpoint, None).await?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_network_upgrade_activations(Arc::new(sample_regtest_upgrade_activations()))
    .with_admitted_transaction_detail_wallet_channel(wallet_channel);
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
    let wallet_endpoint = format!("http://{wallet_addr}");
    let wallet_channel = connect_zinder_grpc(&wallet_endpoint, None).await?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store)
    .with_canonical_store(canonical_store)
    .with_wallet_query_endpoint(wallet_endpoint)
    .with_admitted_transaction_detail_wallet_channel(wallet_channel);
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
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: Network::ZcashRegtest,
    })
    .with_materialized_view_store(materialized_view_store);
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

/// One value event the deltas seeder writes into the materialized-view store.
struct SeedDelta {
    height: u32,
    in_block_position: u32,
    kind_byte: u8,
    event_index: u32,
    transaction_id: String,
    value_zat: i64,
}

/// Seeds the `transparent_address_deltas` column family for one address with
/// the given events, mirroring what `TransparentAddressDeltasConsumer` writes
/// at commit time.
fn seed_transparent_address_deltas(
    materialized_view_store: &MaterializedViewStore,
    address: TransparentAddressScriptHash,
    deltas: &[SeedDelta],
) -> Result<()> {
    for delta in deltas {
        let key = TransparentAddressDeltasConsumer::key_for_event(
            address,
            BlockHeight::new(delta.height),
            delta.in_block_position,
            delta.kind_byte,
            delta.event_index,
        );
        let record = TransparentAddressDeltasRecord {
            transaction_id: delta.transaction_id.clone(),
            block_time_unix_seconds: 1_700_000_000 + i64::from(delta.height),
            value_zat: delta.value_zat,
        };
        materialized_view_store.put_consumer(
            TRANSPARENT_ADDRESS_DELTAS_COLUMN_FAMILY,
            &key,
            &record.encode_to_vec(),
        )?;
    }
    materialized_view_store
        .put_chain_event_cursor(TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME, &[1])?;
    Ok(())
}

/// Opens a primary materialized-view store seeded with the given address deltas, then
/// returns a caught-up secondary handle for the explorer to read.
fn seeded_deltas_materialized_view_store(
    address: TransparentAddressScriptHash,
    deltas: &[SeedDelta],
) -> Result<SeededMaterializedViewStore> {
    let tempdir = tempfile::tempdir()?;
    let primary_path = tempdir.path().join("materialized-view-primary");
    let secondary_path = tempdir.path().join("materialized-view-secondary");
    let primary_store = MaterializedViewStore::open(
        &primary_path,
        MaterializedViewStoreOptions {
            sync_writes: false,
            consumers: MaterializedViewStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?;
    seed_transparent_address_deltas(&primary_store, address, deltas)?;

    let secondary_store = MaterializedViewStore::open_secondary(
        &primary_path,
        &secondary_path,
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

fn deltas_request(
    address: TransparentAddressScriptHash,
    start_height: u32,
    end_height: u32,
    max_entries: u32,
    from_cursor: Vec<u8>,
) -> TransparentAddressDeltasRequest {
    TransparentAddressDeltasRequest {
        address: Some(AddressLookup {
            selector: Some(AddressSelector::ScriptHash(address.as_bytes().to_vec())),
        }),
        start_height,
        end_height,
        max_entries,
        from_cursor,
        at_epoch_id: None,
    }
}

const DELTAS_TEST_ADDRESS: TransparentAddressScriptHash =
    TransparentAddressScriptHash::from_bytes([42; 32]);

/// The fixture seeds two receives and one spend; the spend at height 12 nets
/// the second receive to zero so the activity sum is unambiguous.
fn seeded_deltas() -> [SeedDelta; 3] {
    [
        SeedDelta {
            height: 10,
            in_block_position: 1,
            kind_byte: TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
            event_index: 0,
            transaction_id: "a".repeat(64),
            value_zat: 9_000,
        },
        SeedDelta {
            height: 12,
            in_block_position: 2,
            kind_byte: TRANSPARENT_DELTA_KIND_RECEIVED_BYTE,
            event_index: 3,
            transaction_id: "b".repeat(64),
            value_zat: 5_000,
        },
        SeedDelta {
            height: 12,
            in_block_position: 2,
            kind_byte: TRANSPARENT_DELTA_KIND_SPENT_BYTE,
            event_index: 1,
            transaction_id: "b".repeat(64),
            value_zat: -9_000,
        },
    ]
}

async fn spawn_deltas_explorer(
    deltas: &[SeedDelta],
) -> Result<(ExplorerQueryClient<Channel>, ServerHandle, ServerHandle)> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (_store_fixture, wallet_addr, wallet_handle) =
        spawn_wallet_query_server(&chain_fixture).await?;
    let seeded = seeded_deltas_materialized_view_store(DELTAS_TEST_ADDRESS, deltas)?;
    let (client, explorer_handle) =
        spawn_explorer_query_server(seeded.secondary_store, wallet_addr).await?;
    Ok((client, explorer_handle, wallet_handle))
}

/// Per-event rows arrive ascending by height with correct signs and indices,
/// the advertised capability is present, and the net equals the delta sum.
#[tokio::test]
async fn explorer_query_serves_transparent_address_deltas_ascending() -> Result<()> {
    let deltas = seeded_deltas();
    let (mut client, explorer_handle, wallet_handle) = spawn_deltas_explorer(&deltas).await?;

    let common = client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .and_then(|info| info.common)
        .ok_or_else(|| eyre!("explorer info missing common ops.ServerInfo"))?;
    assert_advertises_capability(&common.capabilities, EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1);

    let entries = client
        .transparent_address_deltas(deltas_request(DELTAS_TEST_ADDRESS, 0, 100, 0, Vec::new()))
        .await?
        .into_inner()
        .entries;
    assert_eq!(entries.len(), 3);

    let heights: Vec<u32> = entries.iter().map(|entry| entry.block_height).collect();
    assert_eq!(heights, vec![10, 12, 12]);
    assert_eq!(entries[0].kind, TransparentDeltaKind::Received as i32);
    assert_eq!(entries[0].index, 0);
    assert_eq!(entries[0].value_zat, 9_000);
    assert_eq!(entries[1].index, 3);
    assert_eq!(entries[1].value_zat, 5_000);
    assert_eq!(entries[2].kind, TransparentDeltaKind::Spent as i32);
    assert_eq!(entries[2].index, 1);
    assert_eq!(entries[2].value_zat, -9_000);

    let net: i64 = entries.iter().map(|entry| entry.value_zat).sum();
    assert_eq!(net, 9_000 + 5_000 - 9_000);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}

/// The height range filters the series, an out-of-range window returns no rows
/// and no cursor, and the page cursor resumes strictly after the prior page.
#[tokio::test]
async fn explorer_query_pages_transparent_address_deltas() -> Result<()> {
    let deltas = seeded_deltas();
    let (mut client, explorer_handle, wallet_handle) = spawn_deltas_explorer(&deltas).await?;

    let ranged = client
        .transparent_address_deltas(deltas_request(DELTAS_TEST_ADDRESS, 11, 100, 0, Vec::new()))
        .await?
        .into_inner();
    assert_eq!(ranged.entries.len(), 2);
    assert!(ranged.entries.iter().all(|entry| entry.block_height == 12));

    let empty = client
        .transparent_address_deltas(deltas_request(DELTAS_TEST_ADDRESS, 200, 300, 0, Vec::new()))
        .await?
        .into_inner();
    assert!(empty.entries.is_empty());
    assert!(empty.next_cursor.is_empty());

    let first_page = client
        .transparent_address_deltas(deltas_request(DELTAS_TEST_ADDRESS, 0, 100, 1, Vec::new()))
        .await?
        .into_inner();
    assert_eq!(first_page.entries.len(), 1);
    assert_eq!(first_page.entries[0].block_height, 10);
    assert!(!first_page.next_cursor.is_empty());

    let second_page = client
        .transparent_address_deltas(deltas_request(
            DELTAS_TEST_ADDRESS,
            0,
            100,
            1,
            first_page.next_cursor,
        ))
        .await?
        .into_inner();
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(second_page.entries[0].block_height, 12);

    explorer_handle.abort();
    let _ = explorer_handle.await;
    wallet_handle.abort();
    let _ = wallet_handle.await;
    Ok(())
}
