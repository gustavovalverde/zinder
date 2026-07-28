#![allow(
    missing_docs,
    reason = "Integration test names describe the operator-built binary composition contract."
)]

//! Production-shaped contract proof for the operator-built Explorer binary composition.

use std::{fs, net::SocketAddr, process::Stdio, sync::Arc, time::Duration};

use eyre::{Result, WrapErr as _, eyre};
use parking_lot::RwLock;
use tokio::process::Command;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::{
    Code, Response, Status,
    transport::{Channel, Endpoint},
};
use zinder_core::{
    BlockHash, BlockHeight, Network, PrivacyShape, TransactionComponentCounts,
    TransactionFactsArtifact, TransactionId, TransactionVersion, TransparentAddressScriptHash,
    TransparentInputFact, TransparentOutPoint, TransparentOutputFact, TransparentSpendFact,
    wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name},
};
use zinder_explorer::{
    ExplorerEndpointMetadata, ExplorerQueryEndpointComposition, ExplorerQueryGrpcAdapter,
};
use zinder_ingest::{MaterializedViewReplayConfig, MaterializedViewTailer};
use zinder_materialized_views::{
    MaterializedViewPreset, MaterializedViewStore, MaterializedViewStoreOptions,
};
use zinder_proto::{
    capabilities::{self, CapabilitySurface, capabilities_for_surface},
    v1::explorer::{
        self, explorer_query_client::ExplorerQueryClient, explorer_query_server::ExplorerQuery,
    },
};
use zinder_query::{
    WalletEndpointMetadata, WalletQueryGrpcAdapter, WalletServingPairSlot, WalletServingQuery,
    WalletServingReadPair,
};
use zinder_runtime::{Readiness, ReadinessState, TrafficReadinessInterceptor};
use zinder_store::{ChainStoreOptions, RawBlobRetention, RocksDbResourceBudget};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, JsonRpcTestServer, RpcReply, WalletServingStoreFixture,
    method, sample_regtest_upgrade_activations, synthetic_transaction_public_facts,
};

type ServerHandle = tokio::task::JoinHandle<Result<(), tonic::transport::Error>>;

const WALLET_QUERY_TEST_SERVER_BIND_TIMEOUT: Duration = Duration::from_secs(1);
const WALLET_QUERY_TEST_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const BOUND_EXPLORER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BOUND_EXPLORER_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
// This covers the fixed five-second WalletQuery health interval while leaving
// the bounded shutdown path inside nextest's 20-second failure window.
const BOUND_EXPLORER_OPS_STATUS_TIMEOUT: Duration = Duration::from_secs(7);
const WALLET_QUERY_TEST_SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const BOUND_EXPLORER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// The 21 pre-pruning identifiers describe 20 callable RPCs plus one optional
/// fee-field identifier carried by recent and history responses.
const BINARY_COMPOSITION_CAPABILITIES: [&str; 21] = [
    capabilities::EXPLORER_SERVER_INFO_V1,
    capabilities::EXPLORER_BLOCK_SUMMARY_V2,
    capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
    capabilities::EXPLORER_FEE_SUMMARY_V1,
    capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
    capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1,
    capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
    capabilities::EXPLORER_TRANSACTION_FEES_V1,
    capabilities::EXPLORER_TRANSACTION_RECENT_V1,
    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
    capabilities::EXPLORER_MIGRATION_OVERVIEW_V1,
    capabilities::EXPLORER_MIGRATION_COHORTS_V1,
    capabilities::EXPLORER_MIGRATION_DENOMINATIONS_V1,
];

const BINARY_COMPOSITION_RPC_CAPABILITIES: [&str; 20] = [
    capabilities::EXPLORER_SERVER_INFO_V1,
    capabilities::EXPLORER_BLOCK_SUMMARY_V2,
    capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
    capabilities::EXPLORER_FEE_SUMMARY_V1,
    capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
    capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
    capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1,
    capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
    capabilities::EXPLORER_TRANSACTION_RECENT_V1,
    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
    capabilities::EXPLORER_MIGRATION_OVERVIEW_V1,
    capabilities::EXPLORER_MIGRATION_COHORTS_V1,
    capabilities::EXPLORER_MIGRATION_DENOMINATIONS_V1,
];

const BINARY_COMPOSITION_ADDITIONAL_IDENTIFIERS: [&str; 1] =
    [capabilities::EXPLORER_TRANSACTION_FEES_V1];

const BINARY_COMPOSITION_OMISSIONS: [&str; 23] = [
    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
    capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    capabilities::EXPLORER_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
    capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
    capabilities::EXPLORER_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
    capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
    capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
    capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
    capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
    capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
    capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
    capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
    capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
];

const BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES: [&str; 19] = [
    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
    capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
    capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    capabilities::EXPLORER_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
    capabilities::EXPLORER_SEARCH_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
    capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
    capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
    capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
    capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
    capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
    capabilities::EXPLORER_UTXO_SET_SUMMARY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
    capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
];

const BINARY_COMPOSITION_OMITTED_FIELD_CAPABILITIES: [&str; 4] = [
    capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
    capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
    capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1,
    capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
];

const CURRENT_PRODUCT_REQUIRED_OMISSIONS: [&str; 8] = [
    capabilities::EXPLORER_BLOCK_DETAIL_V1,
    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
    capabilities::EXPLORER_SEARCH_V1,
    capabilities::EXPLORER_MEMPOOL_SUMMARY_V2,
    capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1,
    capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
    capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
];

struct BinaryCompositionMaterializedViewStore {
    tempdir: tempfile::TempDir,
    secondary: MaterializedViewStore,
}

struct BoundExplorerBinary {
    _canonical_store: WalletServingStoreFixture,
    _materialized_view_store: BinaryCompositionMaterializedViewStore,
    _node: JsonRpcTestServer,
    listen_addr: SocketAddr,
    ops_addr: SocketAddr,
    child: tokio::process::Child,
}

struct WalletQueryTestServer {
    _store_fixture: WalletServingStoreFixture,
    address: SocketAddr,
    readiness: Readiness,
    graceful_shutdown_sender: Option<tokio::sync::oneshot::Sender<()>>,
    handle: ServerHandle,
}

impl WalletQueryTestServer {
    fn publish_available(&self) {
        self.readiness
            .set(ReadinessState::ready(Some(BINARY_COMPOSITION_TIP_HEIGHT)));
    }

    fn publish_unavailable(&self) {
        self.readiness.set(ReadinessState::starting());
    }

    fn signal_graceful_shutdown(&mut self) {
        if let Some(shutdown_sender) = self.graceful_shutdown_sender.take() {
            let _ = shutdown_sender.send(());
        }
    }

    async fn stop(mut self) -> Result<()> {
        self.signal_graceful_shutdown();
        let outcome = tokio::time::timeout(WALLET_QUERY_TEST_SERVER_STOP_TIMEOUT, &mut self.handle)
            .await
            .map_err(|_| {
                self.handle.abort();
                eyre!(
                    "WalletQuery test server at {} did not close accepted connections and stop \
                     within {:?} after graceful shutdown; aborted the server task",
                    self.address,
                    WALLET_QUERY_TEST_SERVER_STOP_TIMEOUT,
                )
            })?;
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for WalletQueryTestServer {
    fn drop(&mut self) {
        self.signal_graceful_shutdown();
        self.handle.abort();
    }
}

impl BoundExplorerBinary {
    fn spawn(chain: &ChainFixture, wallet_addr: SocketAddr) -> Result<Self> {
        let (canonical_store, materialized_view_store) =
            replayed_explorer_materialized_view_store(chain)?;
        let materialized_view_primary_path =
            MaterializedViewStore::path_for_canonical(&canonical_store.canonical_primary_path());
        let detected_preset = MaterializedViewStore::detect_materialized_view_preset_at_path(
            &materialized_view_primary_path,
            Network::ZcashRegtest,
        )?;
        if detected_preset != Some(MaterializedViewPreset::Explorer) {
            return Err(eyre!(
                "binary-composition fixture primary at {} did not expose the Explorer preset: {detected_preset:?}",
                materialized_view_primary_path.display(),
            ));
        }
        let node = JsonRpcTestServer::start([
            method("getblockchaininfo").reply(RpcReply::result(serde_json::json!({
                "upgrades": {
                    "76b809bb": {
                        "name": "Sapling",
                        "activationheight": 1
                    }
                }
            }))),
            method("getblockchaininfo").reply(RpcReply::result(serde_json::json!({
                "blocks": BINARY_COMPOSITION_TIP_HEIGHT,
                "estimatedheight": BINARY_COMPOSITION_TIP_HEIGHT,
                "verificationprogress": 1.0
            }))),
        ])?;
        let listen_addr = unused_loopback_addr()?;
        let ops_addr = unused_loopback_addr()?;
        let secondary_root = materialized_view_store
            .tempdir
            .path()
            .join("binary-runtime-secondary");
        fs::create_dir_all(&secondary_root)?;
        let config_path = materialized_view_store
            .tempdir
            .path()
            .join("zinder-explorer.toml");
        fs::write(
            &config_path,
            format!(
                r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[explorer]
listen_addr = "{listen_addr}"
wallet_query_endpoint = "http://{wallet_addr}"

[ops]
listen_addr = "{ops_addr}"

[node]
json_rpc_addr = "{}"
request_timeout_secs = 2
"#,
                canonical_store.canonical_primary_path().display(),
                secondary_root.display(),
                node.url(),
            ),
        )?;

        let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
        command
            .env_clear()
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .args(["--config", path_str(&config_path)?]);
        let child = command.spawn()?;
        Ok(Self {
            _canonical_store: canonical_store,
            _materialized_view_store: materialized_view_store,
            _node: node,
            listen_addr,
            ops_addr,
            child,
        })
    }

    async fn client(&self) -> Result<ExplorerQueryClient<Channel>> {
        Ok(ExplorerQueryClient::new(
            await_bound_explorer_start(self.listen_addr).await?,
        ))
    }

    async fn stop(mut self) -> Result<std::process::Output> {
        let child_id = self.child.id();
        if let Err(kill_error) = self.child.start_kill()
            && self.child.try_wait()?.is_none()
        {
            return Err(kill_error).wrap_err_with(|| {
                format!("failed to terminate bound Explorer child {child_id:?}")
            });
        }
        tokio::time::timeout(BOUND_EXPLORER_STOP_TIMEOUT, self.child.wait_with_output())
            .await
            .map_err(|_| {
                eyre!(
                    "bound Explorer child {child_id:?} did not exit within {:?} after termination",
                    BOUND_EXPLORER_STOP_TIMEOUT,
                )
            })?
            .wrap_err_with(|| format!("failed to collect bound Explorer child {child_id:?} output"))
    }
}

const BINARY_COMPOSITION_TIP_HEIGHT: u32 = 1;

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "One binary-composition proof keeps the exact 21/23 allocation beside the 20 positive and 19 negative dispatch audits."
)]
async fn operator_built_binary_composition_proves_every_advertised_contract() -> Result<()> {
    let chain = binary_composition_chain_fixture()?;
    let wallet_server = spawn_binary_composition_wallet_query_server(&chain).await?;
    let (_canonical_store, materialized_view_store) =
        replayed_explorer_materialized_view_store(&chain)?;
    let adapter =
        binary_composition_adapter(materialized_view_store.secondary, wallet_server.address)
            .await?;

    assert_eq!(
        adapter.advertised_capabilities().as_ref(),
        BINARY_COMPOSITION_CAPABILITIES,
    )?;
    assert_eq!(
        BINARY_COMPOSITION_RPC_CAPABILITIES.len() + BINARY_COMPOSITION_ADDITIONAL_IDENTIFIERS.len(),
        BINARY_COMPOSITION_CAPABILITIES.len(),
    );
    let rpc_capabilities_in_advertised_order = BINARY_COMPOSITION_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| BINARY_COMPOSITION_RPC_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    let additional_identifiers_in_advertised_order = BINARY_COMPOSITION_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| BINARY_COMPOSITION_ADDITIONAL_IDENTIFIERS.contains(capability))
        .collect::<Vec<_>>();
    assert_eq!(
        rpc_capabilities_in_advertised_order,
        BINARY_COMPOSITION_RPC_CAPABILITIES,
    );
    assert_eq!(
        additional_identifiers_in_advertised_order,
        BINARY_COMPOSITION_ADDITIONAL_IDENTIFIERS,
    );
    assert!(BINARY_COMPOSITION_CAPABILITIES.iter().all(|capability| {
        BINARY_COMPOSITION_RPC_CAPABILITIES.contains(capability)
            ^ BINARY_COMPOSITION_ADDITIONAL_IDENTIFIERS.contains(capability)
    }));
    let registry = capabilities_for_surface(CapabilitySurface::Explorer)
        .map(|spec| spec.string)
        .collect::<Vec<_>>();
    assert_eq!(registry.len(), 44);
    let registry_complement = registry
        .into_iter()
        .filter(|capability| !BINARY_COMPOSITION_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    assert_eq!(registry_complement, BINARY_COMPOSITION_OMISSIONS);
    let omitted_rpc_registry_order = BINARY_COMPOSITION_OMISSIONS
        .iter()
        .copied()
        .filter(|capability| BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    let omitted_field_registry_order = BINARY_COMPOSITION_OMISSIONS
        .iter()
        .copied()
        .filter(|capability| BINARY_COMPOSITION_OMITTED_FIELD_CAPABILITIES.contains(capability))
        .collect::<Vec<_>>();
    assert_eq!(
        omitted_rpc_registry_order,
        BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES,
    );
    assert_eq!(
        omitted_field_registry_order,
        BINARY_COMPOSITION_OMITTED_FIELD_CAPABILITIES,
    );
    assert_eq!(
        BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES.len()
            + BINARY_COMPOSITION_OMITTED_FIELD_CAPABILITIES.len(),
        BINARY_COMPOSITION_OMISSIONS.len(),
    );
    assert!(BINARY_COMPOSITION_OMISSIONS.iter().all(|capability| {
        BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES.contains(capability)
            ^ BINARY_COMPOSITION_OMITTED_FIELD_CAPABILITIES.contains(capability)
    }));
    assert!(
        CURRENT_PRODUCT_REQUIRED_OMISSIONS
            .iter()
            .all(|capability| BINARY_COMPOSITION_OMISSIONS.contains(capability))
    );

    let dispatched_capabilities =
        assert_binary_composition_methods_dispatch(&adapter, &chain).await?;
    assert_eq!(dispatched_capabilities, BINARY_COMPOSITION_RPC_CAPABILITIES,);
    let rejected_capabilities =
        assert_binary_composition_omitted_methods_fail_before_request_handling(&adapter).await?;
    assert_eq!(
        rejected_capabilities,
        BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES,
    );

    wallet_server.stop().await?;
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "One bound-binary lifecycle proof keeps admission, dependency-loss traffic gating, recovery, and frozen discovery together."
)]
async fn bound_binary_gates_traffic_when_admitted_wallet_contract_is_unavailable() -> Result<()> {
    let chain = binary_composition_chain_fixture()?;
    let wallet_server = spawn_binary_composition_wallet_query_server(&chain).await?;
    let runtime = BoundExplorerBinary::spawn(&chain, wallet_server.address)?;

    let lifecycle_outcome: Result<()> = async {
        let mut client = runtime.client().await?;
        let initial_ready = await_ops_status(runtime.ops_addr, "/readyz", 200).await?;
        assert_eq!(initial_ready["cause"], "ready");
        assert_bound_binary_requests_dispatch(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;

        wallet_server.publish_unavailable();
        let unavailable = await_ops_status(runtime.ops_addr, "/readyz", 503).await?;
        assert_eq!(unavailable["cause"], "wallet_query_unavailable");
        assert_bound_binary_requests_are_unavailable(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;

        wallet_server.publish_available();
        let recovered = await_ops_status(runtime.ops_addr, "/readyz", 200).await?;
        assert_eq!(recovered["cause"], "ready");
        assert_bound_binary_requests_dispatch(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;
        Ok(())
    }
    .await;

    finish_bound_binary_test(lifecycle_outcome, runtime, Some(wallet_server), None).await
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "One bound-binary replacement proof keeps same-address mismatch admission, global traffic gating, recovery, and cleanup explicit."
)]
async fn bound_binary_rejects_same_address_incompatible_wallet_replacement() -> Result<()> {
    let chain = binary_composition_chain_fixture()?;
    let mut wallet_server = Some(spawn_binary_composition_wallet_query_server(&chain).await?);
    let wallet_addr = wallet_server
        .as_ref()
        .ok_or_else(|| eyre!("wallet query test server was not started"))?
        .address;
    let runtime = BoundExplorerBinary::spawn(&chain, wallet_addr)?;

    let lifecycle_outcome: Result<()> = async {
        let mut client = runtime.client().await?;
        let initial_ready = await_ops_status(runtime.ops_addr, "/readyz", 200).await?;
        assert_eq!(initial_ready["cause"], "ready");
        assert_bound_binary_requests_dispatch(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;

        let admitted_server = wallet_server
            .take()
            .ok_or_else(|| eyre!("wallet query test server disappeared before replacement"))?;
        admitted_server.stop().await?;
        let incompatible_server =
            spawn_wallet_query_test_server(&chain, Network::ZcashTestnet, Some(wallet_addr))
                .await?;
        if incompatible_server.address != wallet_addr {
            return Err(eyre!(
                "incompatible WalletQuery replacement moved from {wallet_addr} to {}",
                incompatible_server.address,
            ));
        }
        wallet_server = Some(incompatible_server);

        let incompatible = await_ops_status(runtime.ops_addr, "/readyz", 503).await?;
        assert_eq!(incompatible["cause"], "wallet_query_unavailable");
        assert_bound_binary_requests_are_unavailable(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;

        let incompatible_server = wallet_server
            .take()
            .ok_or_else(|| eyre!("incompatible wallet query test server disappeared"))?;
        incompatible_server.stop().await?;
        let recovered_server =
            spawn_wallet_query_test_server(&chain, Network::ZcashRegtest, Some(wallet_addr))
                .await?;
        if recovered_server.address != wallet_addr {
            return Err(eyre!(
                "recovered WalletQuery endpoint moved from {wallet_addr} to {}",
                recovered_server.address,
            ));
        }
        wallet_server = Some(recovered_server);

        let recovered = await_ops_status(runtime.ops_addr, "/readyz", 200).await?;
        assert_eq!(recovered["cause"], "ready");
        assert_bound_binary_requests_dispatch(&mut client).await?;
        assert_bound_binary_health_capabilities(runtime.ops_addr).await?;
        Ok(())
    }
    .await;

    finish_bound_binary_test(
        lifecycle_outcome,
        runtime,
        wallet_server,
        Some("wallet_query_contract_mismatch"),
    )
    .await
}

async fn finish_bound_binary_test(
    lifecycle_outcome: Result<()>,
    runtime: BoundExplorerBinary,
    wallet_server: Option<WalletQueryTestServer>,
    required_stderr_event: Option<&str>,
) -> Result<()> {
    let runtime_shutdown = runtime.stop().await;
    let wallet_shutdown = match wallet_server {
        Some(server) => server.stop().await,
        None => Ok(()),
    };
    let child_output = runtime_shutdown?;
    let stderr = String::from_utf8_lossy(&child_output.stderr);
    if let Err(error) = lifecycle_outcome {
        return Err(error).wrap_err_with(|| format!("bound explorer stderr:\n{stderr}"));
    }
    wallet_shutdown?;
    if let Some(required_stderr_event) = required_stderr_event
        && !stderr.contains(required_stderr_event)
    {
        return Err(eyre!(
            "bound explorer stderr omitted required event {required_stderr_event:?}:\n{stderr}"
        ));
    }
    Ok(())
}

async fn assert_bound_binary_requests_dispatch(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<()> {
    let server_info = client
        .server_info(explorer::ServerInfoRequest {})
        .await?
        .into_inner();
    let native_capabilities = server_info
        .info
        .and_then(|endpoint_info| endpoint_info.common)
        .ok_or_else(|| eyre!("bound ExplorerQuery.ServerInfo omitted common identity"))?
        .capabilities;
    let expected_capabilities = expected_binary_composition_capabilities();
    if native_capabilities != expected_capabilities {
        return Err(eyre!(
            "bound ExplorerQuery.ServerInfo capabilities differ: actual={native_capabilities:?}, expected={expected_capabilities:?}"
        ));
    }

    let upgrades = client
        .network_upgrade_status(explorer::NetworkUpgradeStatusRequest {})
        .await?
        .into_inner();
    assert!(
        !upgrades.upgrades.is_empty(),
        "local Explorer handler must dispatch while the runtime is ready",
    );

    let summaries = client
        .block_summaries_in_range(explorer::BlockSummariesInRangeRequest {
            start_height: BINARY_COMPOSITION_TIP_HEIGHT,
            end_height: BINARY_COMPOSITION_TIP_HEIGHT,
        })
        .await?
        .into_inner();
    assert_eq!(
        summaries.summaries.len(),
        1,
        "WalletQuery-backed Explorer handler must dispatch while the dependency is healthy",
    );
    Ok(())
}

async fn assert_bound_binary_requests_are_unavailable(
    client: &mut ExplorerQueryClient<Channel>,
) -> Result<()> {
    assert_unavailable(
        client.server_info(explorer::ServerInfoRequest {}).await,
        "ServerInfo",
    )?;
    assert_unavailable(
        client
            .network_upgrade_status(explorer::NetworkUpgradeStatusRequest {})
            .await,
        "NetworkUpgradeStatus",
    )?;
    assert_unavailable(
        client
            .block_summaries_in_range(explorer::BlockSummariesInRangeRequest {
                start_height: BINARY_COMPOSITION_TIP_HEIGHT,
                end_height: BINARY_COMPOSITION_TIP_HEIGHT,
            })
            .await,
        "BlockSummariesInRange",
    )
}

async fn assert_bound_binary_health_capabilities(ops_addr: SocketAddr) -> Result<()> {
    let healthz = await_ops_status(ops_addr, "/healthz", 200).await?;
    let operational_capabilities = healthz["capabilities"]
        .as_array()
        .ok_or_else(|| eyre!("bound /healthz omitted capabilities"))?
        .iter()
        .map(|capability_value| {
            capability_value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| eyre!("/healthz capability is not a string"))
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        operational_capabilities,
        expected_binary_composition_capabilities(),
    );
    Ok(())
}

fn expected_binary_composition_capabilities() -> Vec<String> {
    BINARY_COMPOSITION_CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect()
}

fn assert_unavailable<T>(outcome: Result<Response<T>, Status>, method_name: &str) -> Result<()> {
    let status = outcome
        .err()
        .ok_or_else(|| eyre!("{method_name} unexpectedly dispatched while Explorer was unready"))?;
    assert_eq!(
        status.code(),
        Code::Unavailable,
        "{method_name} must fail at the runtime traffic gate while Explorer is unready",
    );
    Ok(())
}

async fn binary_composition_adapter(
    materialized_view_store: MaterializedViewStore,
    wallet_addr: SocketAddr,
) -> Result<ExplorerQueryGrpcAdapter> {
    Ok(ExplorerQueryEndpointComposition {
        metadata: ExplorerEndpointMetadata {
            network: Network::ZcashRegtest,
        },
        materialized_view_store: Some(materialized_view_store),
        network_upgrade_activations: Some(Arc::new(sample_regtest_upgrade_activations())),
        wallet_query_endpoint: Some(format!("http://{wallet_addr}")),
        wallet_query_bearer_token: None,
        bearer_token: None,
    }
    .compose()
    .await?)
}

#[allow(
    clippy::too_many_lines,
    reason = "One binary-composition proof keeps every advertised method and field contract explicit."
)]
async fn assert_binary_composition_methods_dispatch(
    adapter: &ExplorerQueryGrpcAdapter,
    chain_fixture: &ChainFixture,
) -> Result<Vec<&'static str>> {
    let mut invoked_rpc_capabilities =
        Vec::with_capacity(BINARY_COMPOSITION_RPC_CAPABILITIES.len());
    macro_rules! call_advertised_unary {
        ($adapter:expr, $method:ident, $request:expr, $capability:expr) => {{
            let response = ExplorerQuery::$method($adapter, tonic::Request::new($request))
                .await?
                .into_inner();
            assert_freshness_capability(response.freshness.as_ref(), $capability)?;
            invoked_rpc_capabilities.push($capability);
            response
        }};
    }

    let block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("binary-composition fixture block missing"))?;
    let block_time = i64::from(block.block_time_seconds);

    let server_info =
        ExplorerQuery::server_info(adapter, tonic::Request::new(explorer::ServerInfoRequest {}))
            .await?
            .into_inner();
    let common = server_info
        .info
        .and_then(|explorer_info| explorer_info.common)
        .ok_or_else(|| eyre!("binary-composition ServerInfo omitted common identity"))?;
    assert_eq!(
        common.capabilities,
        BINARY_COMPOSITION_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(common.materialized_view_preset, "explorer");
    invoked_rpc_capabilities.push(capabilities::EXPLORER_SERVER_INFO_V1);

    let block_summaries = call_advertised_unary!(
        adapter,
        block_summaries_in_range,
        explorer::BlockSummariesInRangeRequest {
            start_height: 1,
            end_height: 1,
        },
        capabilities::EXPLORER_BLOCK_SUMMARY_V2
    );
    assert_eq!(block_summaries.summaries.len(), 1);
    assert_eq!(
        block_summaries.summaries[0].block_hash,
        encode_rpc_block_hash_hex(block.hash)
    );

    let block_activity = call_advertised_unary!(
        adapter,
        block_activity_distribution,
        explorer::BlockActivityDistributionRequest {
            start_height: 1,
            end_height: 1,
        },
        capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1
    );
    assert_eq!(block_activity.materialized_block_count, 1);

    let fee_summary = call_advertised_unary!(
        adapter,
        fee_summary,
        explorer::FeeSummaryRequest {
            start_height: 1,
            end_height: 1,
        },
        capabilities::EXPLORER_FEE_SUMMARY_V1
    );
    assert_eq!(fee_summary.block_count, 1);

    let _conventional_fees = call_advertised_unary!(
        adapter,
        conventional_fee_distribution,
        explorer::ConventionalFeeDistributionRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
        },
        capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1
    );

    let component_summary = call_advertised_unary!(
        adapter,
        transaction_component_summary,
        explorer::TransactionComponentSummaryRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
            totals_only: false,
        },
        capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2
    );
    assert!(component_summary.totals.is_some());

    let upgrades = call_advertised_unary!(
        adapter,
        network_upgrade_status,
        explorer::NetworkUpgradeStatusRequest {},
        capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1
    );
    assert!(!upgrades.upgrades.is_empty());

    let flow_history = call_advertised_unary!(
        adapter,
        value_pool_flow_history,
        explorer::ValuePoolFlowHistoryRequest {
            page_size: 10,
            include_total_count: true,
            ..Default::default()
        },
        capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1
    );
    assert!(flow_history.coverage.is_some());

    let _flow_events = call_advertised_unary!(
        adapter,
        value_pool_flow_events_in_range,
        explorer::ValuePoolFlowEventsInRangeRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
            max_events: 10,
            ..Default::default()
        },
        capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1
    );

    let _flow_summary = call_advertised_unary!(
        adapter,
        value_pool_flow_summary,
        explorer::ValuePoolFlowSummaryRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
            resolution: explorer::ValuePoolFlowSummaryResolution::Hour as i32,
            ..Default::default()
        },
        capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1
    );

    let flow_thresholds = call_advertised_unary!(
        adapter,
        value_pool_flow_amount_threshold_summary,
        explorer::ValuePoolFlowAmountThresholdSummaryRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
            minimum_amounts_zat: vec![1, 10_000],
            ..Default::default()
        },
        capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1
    );
    assert_eq!(flow_thresholds.thresholds.len(), 2);

    let _rounded_flows = call_advertised_unary!(
        adapter,
        value_pool_flow_rounded_amount_summary,
        explorer::ValuePoolFlowRoundedAmountSummaryRequest {
            start_time_unix_seconds: block_time.saturating_sub(1),
            end_time_unix_seconds: block_time.saturating_add(1),
            rounding_quantum_zat: 1_000,
            max_rows: 10,
            ..Default::default()
        },
        capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1
    );

    let balance_history = call_advertised_unary!(
        adapter,
        value_pool_balance_history,
        explorer::ValuePoolBalanceHistoryRequest {
            page_size: 10,
            cursor: Vec::new(),
        },
        capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1
    );
    assert!(balance_history.coverage.is_some());

    let _reorg_history = call_advertised_unary!(
        adapter,
        chain_reorg_history,
        explorer::ChainReorgHistoryRequest {
            max_events: 10,
            from_cursor: Vec::new(),
        },
        capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1
    );

    let _mempool_counts = call_advertised_unary!(
        adapter,
        mempool_event_counts,
        explorer::MempoolEventCountsRequest { window_seconds: 60 },
        capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1
    );

    let recent_stream = ExplorerQuery::recent_transactions(
        adapter,
        tonic::Request::new(explorer::RecentTransactionsRequest {
            max_entries: 10,
            from_cursor: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let recent_chunks = recent_stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, tonic::Status>>()?;
    assert!(recent_chunks.iter().all(|chunk| {
        chunk.freshness.as_ref().is_some_and(|freshness| {
            freshness.capability_version == capabilities::EXPLORER_TRANSACTION_RECENT_V1
        })
    }));
    assert!(
        recent_chunks
            .iter()
            .flat_map(|chunk| chunk.entries.iter())
            .any(|entry| entry.paid_fee_zat == Some(10_000)),
        "advertised transaction-fee field must carry a production-shaped resolved fee",
    );
    invoked_rpc_capabilities.push(capabilities::EXPLORER_TRANSACTION_RECENT_V1);

    let transaction_history = call_advertised_unary!(
        adapter,
        transaction_history,
        explorer::TransactionHistoryRequest {
            page_size: 10,
            include_total_count: true,
            ..Default::default()
        },
        capabilities::EXPLORER_TRANSACTION_HISTORY_V2
    );
    assert!(
        transaction_history
            .entries
            .iter()
            .any(|entry| entry.paid_fee_zat == Some(10_000))
    );
    assert!(
        transaction_history
            .entries
            .iter()
            .all(|entry| entry.intrinsic_value_balances.is_none()),
        "omitted intrinsic-value field must remain absent",
    );
    // `binary_composition_adapter` deliberately composes no canonical store. This
    // successful populated-history response therefore proves the omitted field
    // suppresses the canonical join instead of attempting a canonical read;
    // the handler-level no-join test makes that branch mutation-sensitive.
    assert!(transaction_history.read_fence.is_some());
    assert!(transaction_history.coverage.is_some());
    assert!(transaction_history.total_matching_transactions.is_some());

    let _migration_overview = call_advertised_unary!(
        adapter,
        migration_overview,
        explorer::MigrationOverviewRequest::default(),
        capabilities::EXPLORER_MIGRATION_OVERVIEW_V1
    );
    let _migration_cohorts = call_advertised_unary!(
        adapter,
        migration_cohorts,
        explorer::MigrationCohortsRequest {
            start_height: 1,
            end_height: 1,
            at_epoch_id: Some(1),
        },
        capabilities::EXPLORER_MIGRATION_COHORTS_V1
    );
    let _migration_denominations = call_advertised_unary!(
        adapter,
        migration_denominations,
        explorer::MigrationDenominationsRequest {
            start_height: 1,
            end_height: 1,
            at_epoch_id: Some(1),
        },
        capabilities::EXPLORER_MIGRATION_DENOMINATIONS_V1
    );
    Ok(invoked_rpc_capabilities)
}

fn assert_freshness_capability(
    freshness: Option<&explorer::ExplorerFreshness>,
    expected: &str,
) -> Result<()> {
    let freshness = freshness.ok_or_else(|| eyre!("{expected} response omitted freshness"))?;
    assert_eq!(freshness.capability_version, expected);
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "The ordered list keeps all 19 omitted RPC guards mechanically paired with their identifiers."
)]
async fn assert_binary_composition_omitted_methods_fail_before_request_handling(
    adapter: &ExplorerQueryGrpcAdapter,
) -> Result<Vec<&'static str>> {
    let mut rejected_rpc_capabilities =
        Vec::with_capacity(BINARY_COMPOSITION_OMITTED_RPC_CAPABILITIES.len());
    macro_rules! assert_omitted_unary {
        ($method:ident, $request:ty, $capability:expr) => {{
            assert_unimplemented(
                ExplorerQuery::$method(adapter, tonic::Request::new(<$request>::default())).await,
                stringify!($method),
            )?;
            rejected_rpc_capabilities.push($capability);
        }};
    }

    assert_omitted_unary!(
        transaction_detail,
        explorer::TransactionDetailRequest,
        capabilities::EXPLORER_TRANSACTION_DETAIL_V4
    );
    assert_omitted_unary!(
        block_production_series,
        explorer::BlockProductionSeriesRequest,
        capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2
    );
    assert_omitted_unary!(
        block_production_in_time_range,
        explorer::BlockProductionInTimeRangeRequest,
        capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1
    );
    assert_omitted_unary!(
        block_detail,
        explorer::BlockDetailRequest,
        capabilities::EXPLORER_BLOCK_DETAIL_V1
    );
    assert_omitted_unary!(
        block_transactions,
        explorer::BlockDetailRequest,
        capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2
    );
    assert_omitted_unary!(
        search,
        explorer::SearchRequest,
        capabilities::EXPLORER_SEARCH_V1
    );
    assert_omitted_unary!(
        commitment_root_search,
        explorer::CommitmentRootSearchRequest,
        capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1
    );
    assert_omitted_unary!(
        mempool_summary,
        explorer::MempoolSummaryRequest,
        capabilities::EXPLORER_MEMPOOL_SUMMARY_V2
    );
    assert_omitted_unary!(
        mempool_snapshot,
        explorer::MempoolSnapshotRequest,
        capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1
    );
    assert_omitted_unary!(
        mempool_activity,
        explorer::MempoolActivityRequest,
        capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1
    );
    assert_omitted_unary!(
        transparent_address_activity,
        explorer::TransparentAddressActivityRequest,
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2
    );
    assert_omitted_unary!(
        transparent_address_deltas,
        explorer::TransparentAddressDeltasRequest,
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1
    );
    assert_omitted_unary!(
        paid_fee_distribution,
        explorer::PaidFeeDistributionRequest,
        capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1
    );
    assert_omitted_unary!(
        transparent_address_ranking,
        explorer::TransparentAddressRankingRequest,
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1
    );
    assert_omitted_unary!(
        value_pool_summary,
        explorer::ValuePoolSummaryRequest,
        capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1
    );
    assert_omitted_unary!(
        utxo_set_summary,
        explorer::UtxoSetSummaryRequest,
        capabilities::EXPLORER_UTXO_SET_SUMMARY_V1
    );
    assert_omitted_unary!(
        displaced_block_history,
        explorer::DisplacedBlockHistoryRequest,
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1
    );
    assert_omitted_unary!(
        displaced_block_detail,
        explorer::DisplacedBlockDetailRequest,
        capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1
    );
    assert_omitted_unary!(
        overview_snapshot,
        explorer::OverviewSnapshotRequest,
        capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1
    );
    Ok(rejected_rpc_capabilities)
}

fn assert_unimplemented<T>(outcome: Result<Response<T>, Status>, method_name: &str) -> Result<()> {
    let status = outcome
        .err()
        .ok_or_else(|| eyre!("{method_name} unexpectedly dispatched"))?;
    assert_eq!(
        status.code(),
        tonic::Code::Unimplemented,
        "{method_name} must reject an unadvertised contract before request handling",
    );
    Ok(())
}

async fn spawn_binary_composition_wallet_query_server(
    chain_fixture: &ChainFixture,
) -> Result<WalletQueryTestServer> {
    spawn_wallet_query_test_server(chain_fixture, Network::ZcashRegtest, None).await
}

async fn spawn_wallet_query_test_server(
    chain_fixture: &ChainFixture,
    advertised_network: Network,
    listen_addr: Option<SocketAddr>,
) -> Result<WalletQueryTestServer> {
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
    let listener = bind_wallet_query_test_listener(listen_addr).await?;
    let addr = listener.local_addr()?;
    let readiness = Readiness::new(ReadinessState::ready(Some(BINARY_COMPOSITION_TIP_HEIGHT)));
    let adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        WalletEndpointMetadata {
            network: encode_zinder_native_chain_name(advertised_network).to_owned(),
            ..WalletEndpointMetadata::default()
        },
    );
    let service = tonic::service::interceptor::InterceptedService::new(
        adapter.into_server(),
        TrafficReadinessInterceptor::new(readiness.clone()),
    );
    let (graceful_shutdown_sender, graceful_shutdown_receiver) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = graceful_shutdown_receiver.await;
            })
            .await
    });
    let server = WalletQueryTestServer {
        _store_fixture: store_fixture,
        address: addr,
        readiness,
        graceful_shutdown_sender: Some(graceful_shutdown_sender),
        handle,
    };
    if let Err(start_error) = await_wallet_query_test_server_start(addr).await {
        return match server.stop().await {
            Ok(()) => Err(start_error),
            Err(stop_error) => Err(eyre!(
                "{start_error:#}; WalletQuery test-server startup cleanup also failed: \
                 {stop_error:#}"
            )),
        };
    }
    Ok(server)
}

async fn bind_wallet_query_test_listener(listen_addr: Option<SocketAddr>) -> Result<TcpListener> {
    let Some(listen_addr) = listen_addr else {
        let listener = tokio::time::timeout(
            WALLET_QUERY_TEST_SERVER_BIND_TIMEOUT,
            TcpListener::bind("127.0.0.1:0"),
        )
        .await
        .map_err(|_| {
            eyre!(
                "WalletQuery test server did not bind an ephemeral loopback listener within \
                 {WALLET_QUERY_TEST_SERVER_BIND_TIMEOUT:?}",
            )
        })??;
        return Ok(listener);
    };
    let mut last_address_in_use = None;
    let bind_result = tokio::time::timeout(WALLET_QUERY_TEST_SERVER_BIND_TIMEOUT, async {
        loop {
            match TcpListener::bind(listen_addr).await {
                Ok(listener) => return Ok(listener),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    last_address_in_use = Some(error);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await;
    match bind_result {
        Ok(Ok(listener)) => Ok(listener),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => {
            let error = last_address_in_use.ok_or_else(|| {
                eyre!("same-address WalletQuery listener timed out without a bind attempt")
            })?;
            Err(error).wrap_err_with(|| {
                format!(
                    "same-address WalletQuery listener {listen_addr} remained occupied for \
                     {WALLET_QUERY_TEST_SERVER_BIND_TIMEOUT:?}",
                )
            })
        }
    }
}

fn binary_composition_chain_fixture() -> Result<ChainFixture> {
    let base = ChainFixture::new(Network::ZcashRegtest)
        .with_raw_blob_retention(RawBlobRetention::Transactions)
        .extend_blocks(1);
    let block = base
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("binary-composition fixture block missing"))?;
    let coinbase_transaction_id = TransactionId::from_bytes([0x41; 32]);
    let spend_transaction_id = TransactionId::from_bytes([0x42; 32]);
    let coinbase = coinbase_transaction_row(block.height, block.hash, coinbase_transaction_id);

    let spent_script_pub_key = vec![0x51];
    let spent_script_hash = TransparentAddressScriptHash::of_script_pub_key(&spent_script_pub_key);
    let output_script_pub_key = vec![0x52];
    let mut public_facts = synthetic_transaction_public_facts(spend_transaction_id, 80);
    public_facts.version = TransactionVersion::V5;
    public_facts.counts = TransactionComponentCounts {
        transparent_input_count: 1,
        transparent_output_count: 1,
        ..TransactionComponentCounts::EMPTY
    };
    public_facts.privacy_shape = PrivacyShape::TransparentOnly;
    let spent_outpoint = TransparentOutPoint::new(coinbase_transaction_id, 0);
    let mut spend = FixtureTransactionRows::from_raw_transaction(
        spend_transaction_id,
        block.height,
        block.hash,
        1,
        vec![0x42; 80],
    );
    spend.facts = TransactionFactsArtifact::new(spend.location, public_facts)
        .with_transparent_facts(
            vec![TransparentInputFact::new(0, spent_outpoint)],
            vec![transparent_output_fact(0, 11_000, output_script_pub_key)],
        );
    let transparent_spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        spend_transaction_id,
        1,
        block.height,
        block.hash,
        21_000,
        spent_script_hash,
        block.height,
        block.hash,
    );

    Ok(base
        .with_transaction_rows(coinbase)
        .with_transaction_rows(spend)
        .with_transparent_spend_fact(transparent_spend))
}

fn replayed_explorer_materialized_view_store(
    chain_fixture: &ChainFixture,
) -> Result<(
    WalletServingStoreFixture,
    BinaryCompositionMaterializedViewStore,
)> {
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let mut canonical_store_fixture =
        WalletServingStoreFixture::from_chain(chain_fixture, &activations)?;
    let (canonical_secondary, wallet_secondary) = canonical_store_fixture.take_readers()?;
    drop(wallet_secondary);

    let materialized_view_tempdir = tempfile::tempdir()?;
    let primary_path = MaterializedViewStore::path_for_canonical(
        &canonical_store_fixture.canonical_primary_path(),
    );
    let secondary_path = materialized_view_tempdir.path().join("secondary");
    let primary_store = MaterializedViewStore::open_with_materialized_view_preset(
        &primary_path,
        Network::ZcashRegtest,
        MaterializedViewPreset::Explorer,
        MaterializedViewStoreOptions {
            sync_writes: false,
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            ..MaterializedViewStoreOptions::default()
        },
    )?;
    MaterializedViewTailer::new(
        Arc::new(RwLock::new(canonical_secondary)),
        primary_store,
        MaterializedViewReplayConfig::DEFAULT,
        activations,
        ChainStoreOptions::for_local_tests().reorg_window_blocks,
        None,
        Duration::from_hours(24),
    )?
    .catch_up()?;

    let secondary = MaterializedViewStore::open_secondary_with_materialized_view_preset(
        &primary_path,
        &secondary_path,
        Network::ZcashRegtest,
        MaterializedViewPreset::Explorer,
        MaterializedViewStoreOptions {
            sync_writes: false,
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            ..MaterializedViewStoreOptions::default()
        },
    )?;
    secondary.try_catch_up()?;
    Ok((
        canonical_store_fixture,
        BinaryCompositionMaterializedViewStore {
            tempdir: materialized_view_tempdir,
            secondary,
        },
    ))
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
    let mut transaction = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block_height,
        block_hash,
        0,
        vec![0x41; 120],
    );
    transaction.facts = TransactionFactsArtifact::new(transaction.location, public_facts)
        .with_transparent_facts(
            Vec::new(),
            vec![
                transparent_output_fact(0, 21_000, first_script_pub_key),
                transparent_output_fact(1, 34_000, second_script_pub_key),
            ],
        );
    transaction
}

fn transparent_output_fact(
    output_index: u32,
    value_zat: u64,
    script_pub_key: Vec<u8>,
) -> TransparentOutputFact {
    let script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    TransparentOutputFact::new(output_index, value_zat, script_pub_key, script_hash)
}

async fn await_wallet_query_test_server_start(addr: SocketAddr) -> Result<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}"))?
        .connect_timeout(WALLET_QUERY_TEST_SERVER_CONNECT_TIMEOUT);
    tokio::time::timeout(WALLET_QUERY_TEST_SERVER_CONNECT_TIMEOUT, async {
        loop {
            if let Ok(channel) = endpoint.connect().await {
                return channel;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "WalletQuery test server at {addr} did not accept connections within {:?}",
            WALLET_QUERY_TEST_SERVER_CONNECT_TIMEOUT,
        )
    })
}

async fn await_bound_explorer_start(addr: SocketAddr) -> Result<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}"))?
        .connect_timeout(BOUND_EXPLORER_START_TIMEOUT)
        .timeout(BOUND_EXPLORER_REQUEST_TIMEOUT);
    tokio::time::timeout(BOUND_EXPLORER_START_TIMEOUT, async {
        loop {
            if let Ok(channel) = endpoint.connect().await {
                return channel;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "bound Explorer gRPC server at {addr} did not accept connections within {:?}",
            BOUND_EXPLORER_START_TIMEOUT,
        )
    })
}

async fn await_ops_status(
    addr: SocketAddr,
    path: &'static str,
    expected_status: u16,
) -> Result<serde_json::Value> {
    let mut last_status = None;
    tokio::time::timeout(BOUND_EXPLORER_OPS_STATUS_TIMEOUT, async {
        loop {
            if let Ok((status, json_body)) = fetch_ops_response(addr, path).await {
                last_status = Some(status);
                if status == expected_status {
                    return json_body;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "bound Explorer operations endpoint http://{addr}{path} did not reach HTTP \
             {expected_status} within {:?}; last status was {last_status:?}",
            BOUND_EXPLORER_OPS_STATUS_TIMEOUT,
        )
    })
}

async fn fetch_ops_response(
    addr: SocketAddr,
    path: &'static str,
) -> Result<(u16, serde_json::Value)> {
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let body_offset = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| offset + 4)
        .ok_or_else(|| eyre!("operations response omitted HTTP body delimiter"))?;
    let status_line = std::str::from_utf8(&response[..body_offset.saturating_sub(4)])?
        .lines()
        .next()
        .ok_or_else(|| eyre!("operations response omitted HTTP status line"))?;
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .ok_or_else(|| eyre!("operations response status line omitted status code"))?
        .parse()?;
    Ok((status, serde_json::from_slice(&response[body_offset..])?))
}

fn unused_loopback_addr() -> Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

fn path_str(path: &std::path::Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("path is not valid UTF-8: {}", path.display()))
}
