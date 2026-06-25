//! Zinder wallet query gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use zinder_derive::{DeriveStore, DeriveStoreOptions};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1;
use zinder_proto::v1::explorer::{ServerInfoRequest, explorer_query_client::ExplorerQueryClient};
use zinder_runtime::{
    BearerToken, Readiness, ServiceIdentifier, StartupPhase, cancel_on_terminating_signal,
    connect_zinder_grpc, install_tracing_subscriber, spawn_ops_endpoint_for,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_store::SecondaryChainStore;

mod config;

use config::{QueryConfigError, QueryConfigOverrides};

const REQUIRED_BROADCASTER_NODE_CAPABILITIES: &[NodeCapability] =
    &[NodeCapability::TransactionBroadcast];

#[derive(Parser)]
#[command(name = "zinder-query")]
#[command(about = "Zinder native wallet query gRPC server")]
struct Cli {
    /// TOML configuration file loaded before environment variables and CLI overrides.
    #[arg(long = "config", global = true)]
    config_path: Option<PathBuf>,
    /// Print the resolved command configuration without opening storage or binding.
    #[arg(long = "print-config", global = true)]
    print_config: bool,
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Canonical Zinder store path opened by this query process.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Process-unique `RocksDB` secondary metadata path.
    #[arg(long = "secondary-path")]
    secondary_path: Option<PathBuf>,
    /// Private `zinder-ingest` control gRPC endpoint.
    #[arg(long = "ingest-control-addr")]
    ingest_control_addr: Option<String>,
    /// Path to a file containing the shared-secret bearer token used by the
    /// `IngestControl` writer. Required when the writer enforces auth.
    #[arg(long = "ingest-control-token-path")]
    ingest_control_token_path: Option<PathBuf>,
    /// Chain-event retention window in hours, advertised through `ServerInfo`.
    #[arg(long = "chain-event-retention-hours")]
    chain_event_retention_hours: Option<u64>,
    /// Mined mempool-event retention window in minutes, advertised through `ServerInfo`.
    #[arg(long = "mempool-mined-retention-minutes")]
    mempool_mined_retention_minutes: Option<u64>,
    /// Invalidated mempool-event retention window in hours, advertised through `ServerInfo`.
    #[arg(long = "mempool-invalidated-retention-hours")]
    mempool_invalidated_retention_hours: Option<u64>,
    /// Native wallet query gRPC listen address, such as 127.0.0.1:9101.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
    /// Node JSON-RPC address used for transaction broadcast. Omit to disable broadcast.
    #[arg(long = "node-json-rpc-addr")]
    node_json_rpc_addr: Option<String>,
    /// `zinder-explorer` `ExplorerQuery` endpoint used for federated balance reads.
    #[arg(long = "explorer-endpoint")]
    explorer_endpoint: Option<String>,
    /// Path to a file containing the shared-secret bearer token used by the
    /// `ExplorerQuery` endpoint. Required when `zinder-explorer` enforces auth.
    #[arg(long = "explorer-bearer-token-path")]
    explorer_bearer_token_path: Option<PathBuf>,
    /// Cadence for probing the `ExplorerQuery` capability descriptor.
    #[arg(long = "explorer-probe-interval-ms")]
    explorer_probe_interval_ms: Option<u64>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    install_tracing_subscriber();

    if cli.print_config {
        return run_print_config(cli);
    }

    run_runtime(cli).await
}

#[allow(
    clippy::print_stdout,
    reason = "--print-config is a structured TOML data dump, not a log event"
)]
fn run_print_config(cli: Cli) -> ExitCode {
    let config_path = cli.config_path.clone();
    let render_result = config::load_query_config(config_path, cli.into())
        .and_then(|query_config| config::query_config_toml(&query_config));

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    match run_query(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Runtime startup wires storage, readiness, reflection, health, and shutdown in one auditable sequence."
)]
async fn run_query(cli: Cli) -> Result<(), QueryConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let config_path = cli.config_path.clone();
    let query_config = match config::load_query_config(config_path, cli.into()) {
        Ok(query_config) => {
            load_config_phase.complete();
            query_config
        }
        Err(error) => {
            load_config_phase.fail(&error);
            return Err(error);
        }
    };
    let readiness = Readiness::default();
    let start_api_phase = StartupPhase::StartApi.start();
    let ops_handle = spawn_ops_endpoint_for(
        ServiceIdentifier::Query,
        query_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(query_config.network),
        readiness.clone(),
        zinder_proto::capabilities::always_on_capability_strings(
            zinder_proto::capabilities::CapabilitySurface::Wallet,
        ),
    );
    zinder_query::describe_request_metrics();

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let store = match SecondaryChainStore::open(
        &query_config.storage.path,
        &query_config.storage.secondary_path,
        zinder_store::ChainStoreOptions {
            rocksdb_resource_budget: query_config.storage.canonical_rocksdb_budget,
            ..zinder_store::ChainStoreOptions::for_network(query_config.network)
        },
    ) {
        Ok(store) => store,
        Err(error) => {
            let error = QueryConfigError::Store(error);
            open_storage_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    let derive_store = match DeriveStore::open_secondary(
        DeriveStore::path_for_canonical(&query_config.storage.path),
        query_config.storage.secondary_path.join("derive"),
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: DeriveStore::bundled_consumer_column_families(),
            rocksdb_resource_budget: query_config.storage.derive_rocksdb_budget,
        },
    ) {
        Ok(derive_store) => derive_store,
        Err(error) => {
            let error = QueryConfigError::DeriveStore(error);
            open_storage_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    if let Err(error) = try_catch_up_derive_store_with_timeout(
        derive_store.clone(),
        query_config.storage.initial_catchup_timeout,
    )
    .await
    {
        open_storage_phase.fail(&error);
        start_api_phase.fail(&error);
        return Err(error);
    }
    open_storage_phase.complete();
    let visible_height = store
        .current_chain_epoch()
        .map_err(QueryConfigError::Store)?
        .map(|epoch| epoch.visible_tip_height.value());
    let broadcaster_and_capabilities = build_broadcaster(query_config.broadcaster.as_ref()).await?;
    let transaction_broadcast_enabled = broadcaster_and_capabilities.is_some();
    let upstream_node_capabilities =
        broadcaster_and_capabilities
            .as_ref()
            .map(
                |(_, node_capabilities)| zinder_query::UpstreamNodeCapabilities {
                    version: None,
                    capabilities: node_capabilities
                        .iter()
                        .map(|capability| capability.name().to_owned())
                        .collect(),
                },
            );
    let chain_value_pools_enabled =
        upstream_node_capabilities
            .as_ref()
            .is_some_and(|capabilities| {
                capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "chain_value_pools")
            });
    let broadcaster = broadcaster_and_capabilities.map(|(source, _)| source);
    let network_upgrade_activations = match broadcaster.as_ref() {
        Some(source) => source
            .discover_network_upgrade_activations("zinder-query")
            .await
            .map_err(|error| QueryConfigError::Source(Box::new(error)))?,
        None => {
            return Err(QueryConfigError::Source(Box::new(
                zinder_source::SourceError::SourceProtocolMismatch {
                    reason: "[node] section is required so the consensus branch id schedule \
                             can be discovered at startup",
                },
            )));
        }
    };
    let tree_state_upstream = broadcaster
        .as_ref()
        .map(|source| Arc::new(source.clone()) as Arc<dyn zinder_source::TreeStateUpstream>);
    let mut wallet_query =
        zinder_query::WalletQuery::new(store.clone(), broadcaster, network_upgrade_activations)
            .with_derive_store(derive_store);
    if let Some(tree_state_upstream) = tree_state_upstream {
        wallet_query = wallet_query.with_tree_state_upstream(tree_state_upstream);
    }
    let cancel = CancellationToken::new();
    let server_info = zinder_query::ServerInfoSettings {
        network: encode_zinder_native_chain_name(query_config.network).to_owned(),
        transaction_broadcast_enabled,
        chain_event_retention_seconds: query_config.chain_event_retention_seconds(),
        mempool_mined_retention_seconds: query_config.mempool_mined_retention_seconds(),
        mempool_invalidated_retention_seconds: query_config.mempool_invalidated_retention_seconds(),
        upstream_node_capabilities,
        chain_value_pools_enabled,
        ..zinder_query::ServerInfoSettings::default()
    };
    let grpc_adapter = {
        let mut adapter = zinder_query::WalletQueryGrpcAdapter::with_ingest_control_proxy(
            wallet_query,
            server_info,
            query_config.ingest_control_addr.clone(),
        );
        if let Some(explorer_config) = query_config.explorer_proxy.clone() {
            let explorer_proxy = zinder_query::DeriveProxy::new(
                zinder_query::DeriveProxyConfig {
                    endpoint: explorer_config.endpoint.clone(),
                    bearer_token: explorer_config.bearer_token.clone(),
                    capability: EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
                },
                ExplorerQueryClient::new,
            );
            let readiness = explorer_proxy.readiness();
            let endpoint = explorer_config.endpoint.clone();
            let bearer_token = explorer_config.bearer_token.clone();
            let _explorer_probe_handle = zinder_query::spawn_derive_readiness_probe(
                readiness,
                move || probe_explorer_capability(endpoint.clone(), bearer_token.clone()),
                zinder_query::DeriveReadinessProbeConfig {
                    probe_interval: explorer_config.probe_interval,
                },
                cancel.clone(),
            );
            tracing::info!(
                target: "zinder::query",
                event = "explorer_proxy_configured",
                endpoint = %explorer_config.endpoint,
                "explorer-plane proxy configured"
            );
            adapter = adapter.with_explorer_proxy(explorer_proxy);
        }
        if let Some(token) = query_config.ingest_control_bearer_token.clone() {
            adapter = adapter.with_ingest_control_bearer_token(token);
        }
        adapter
    };
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let _refresh_handle = zinder_query::spawn_secondary_catchup(
        store,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: query_config.storage.secondary_catchup_interval,
            lag_threshold_chain_epochs: query_config
                .storage
                .secondary_replica_lag_threshold_chain_epochs,
            writer_status: Some(zinder_query::WriterStatusConfig {
                endpoint: query_config.ingest_control_addr.clone(),
                network: query_config.network,
                bearer_token: query_config.ingest_control_bearer_token.clone(),
            }),
        },
        cancel.clone(),
    );
    let reflection_service = query_config
        .grpc
        .enable_reflection
        .then(|| {
            tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(zinder_proto::ZINDER_V1_FILE_DESCRIPTOR_SET)
                .build_v1()
        })
        .transpose()?;
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    let _health_reporter_handle = query_config.grpc.enable_health.then(|| {
        spawn_grpc_health_reporter(
            health_reporter,
            readiness.clone(),
            "zinder.v1.wallet.WalletQuery",
            cancel.clone(),
        )
    });
    let health_service = query_config.grpc.enable_health.then_some(health_service);

    tracing::info!(
        target: "zinder::query",
        event = "query_started",
        network = encode_zinder_native_chain_name(query_config.network),
        listen_addr = %query_config.listen_addr,
        visible_height = ?visible_height,
        grpc_reflection_enabled = query_config.grpc.enable_reflection,
        grpc_health_enabled = query_config.grpc.enable_health,
        "wallet query gRPC server started"
    );

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_adapter.into_server())
        .add_optional_service(reflection_service)
        .add_optional_service(health_service)
        .serve_with_shutdown(query_config.listen_addr, cancel.cancelled_owned())
        .await;

    tracing::info!(
        target: "zinder::query",
        event = "query_stopped",
        "wallet query gRPC server stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    server_result.map_err(QueryConfigError::Transport)
}

async fn probe_explorer_capability(endpoint: String, bearer_token: Option<BearerToken>) -> bool {
    let Ok(channel) = connect_zinder_grpc(&endpoint, bearer_token.as_ref()).await else {
        return false;
    };
    let mut client = ExplorerQueryClient::new(channel);
    let Ok(response) = client.server_info(ServerInfoRequest {}).await else {
        return false;
    };
    response
        .into_inner()
        .info
        .as_ref()
        .and_then(|explorer_info| explorer_info.common.as_ref())
        .is_some_and(|common| {
            common
                .capabilities
                .iter()
                .any(|capability| capability == EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1)
        })
}

fn spawn_grpc_health_reporter(
    reporter: tonic_health::server::HealthReporter,
    readiness: Readiness,
    service_name: &'static str,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            update_grpc_health(&reporter, &readiness, service_name).await;
            tokio::select! {
                () = cancel.cancelled() => break,
                () = sleep(Duration::from_secs(1)) => {}
            }
        }
    })
}

async fn try_catch_up_derive_store_with_timeout(
    derive_store: DeriveStore,
    timeout: Duration,
) -> Result<(), QueryConfigError> {
    let handle = tokio::task::spawn_blocking(move || derive_store.try_catch_up());
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(catchup_outcome)) => catchup_outcome.map_err(QueryConfigError::DeriveStore),
        Ok(Err(join_error)) => Err(QueryConfigError::Config(
            zinder_runtime::ConfigError::invalid(format!(
                "derive initial catchup blocking task failed: {join_error}"
            )),
        )),
        Err(_) => {
            tracing::warn!(
                target: "zinder::query",
                event = "initial_secondary_catchup_timed_out",
                role = "derive",
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "initial derive secondary catchup timed out; starting with the current secondary view"
            );
            Ok(())
        }
    }
}

async fn update_grpc_health(
    reporter: &tonic_health::server::HealthReporter,
    readiness: &Readiness,
    service_name: &'static str,
) {
    let status = if readiness.report().is_ready {
        tonic_health::ServingStatus::Serving
    } else {
        tonic_health::ServingStatus::NotServing
    };
    reporter.set_service_status("", status).await;
    reporter.set_service_status(service_name, status).await;
}

async fn build_broadcaster(
    broadcaster_target: Option<&zinder_source::NodeTarget>,
) -> Result<Option<(ZebraJsonRpcSource, NodeCapabilities)>, QueryConfigError> {
    let Some(target) = broadcaster_target else {
        tracing::info!(
            target: "zinder::query",
            event = "transaction_broadcast_disabled",
            "transaction broadcast disabled because [node] is not configured"
        );
        return Ok(None);
    };

    let source = ZebraJsonRpcSource::with_options(
        target.network,
        target.json_rpc_addr.clone(),
        target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: target.request_timeout,
            max_response_bytes: target.max_response_bytes,
            broadcast_timeout: target.broadcast_timeout,
        },
    )
    .map_err(|source| QueryConfigError::Source(Box::new(source)))?;

    let capabilities = source
        .probe_capabilities()
        .await
        .map_err(|source| QueryConfigError::Source(Box::new(source)))?;
    let advertised: Vec<&'static str> = capabilities
        .iter()
        .map(zinder_source::NodeCapability::name)
        .collect();
    if capabilities.supports(NodeCapability::OpenRpcDiscovery) {
        tracing::info!(
            target: "zinder::query",
            event = "transaction_broadcast_capabilities_probed",
            advertised = ?advertised,
            "transaction broadcast node capabilities discovered via rpc.discover"
        );
    } else {
        tracing::warn!(
            target: "zinder::query",
            event = "transaction_broadcast_capabilities_probe_fallback",
            advertised = ?advertised,
            "transaction broadcast node capability probe used baseline capabilities because rpc.discover was unavailable"
        );
    }
    require_broadcaster_node_capabilities(capabilities)
        .map_err(|source| QueryConfigError::Source(Box::new(source)))?;

    tracing::info!(
        target: "zinder::query",
        event = "transaction_broadcast_enabled",
        json_rpc_addr = %target.json_rpc_addr,
        "transaction broadcast enabled via Zebra JSON-RPC"
    );
    Ok(Some((source, capabilities)))
}

fn require_broadcaster_node_capabilities(
    capabilities: NodeCapabilities,
) -> Result<(), zinder_source::SourceError> {
    for required in REQUIRED_BROADCASTER_NODE_CAPABILITIES {
        if !capabilities.supports(*required) {
            return Err(zinder_source::SourceError::NodeCapabilityMissing {
                capability: *required,
            });
        }
    }

    Ok(())
}

fn emit_runtime_error(error: &QueryConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::query",
        event = "query_run_failed",
        error = %error,
        "query run failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for QueryConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            storage_path: cli.storage_path,
            secondary_path: cli.secondary_path,
            ingest_control_addr: cli.ingest_control_addr,
            ingest_control_bearer_token_path: cli.ingest_control_token_path,
            chain_event_retention_hours: cli.chain_event_retention_hours,
            mempool_mined_retention_minutes: cli.mempool_mined_retention_minutes,
            mempool_invalidated_retention_hours: cli.mempool_invalidated_retention_hours,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
            node_json_rpc_addr: cli.node_json_rpc_addr,
            explorer_endpoint: cli.explorer_endpoint,
            explorer_bearer_token_path: cli.explorer_bearer_token_path,
            explorer_probe_interval_ms: cli.explorer_probe_interval_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NodeCapabilities, NodeCapability, ZebraJsonRpcSource, require_broadcaster_node_capabilities,
    };

    #[test]
    fn broadcaster_capability_validation_accepts_zebra_baseline()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = ZebraJsonRpcSource::baseline_capabilities();

        require_broadcaster_node_capabilities(capabilities)?;

        Ok(())
    }

    #[test]
    fn broadcaster_capability_validation_rejects_missing_sendrawtransaction()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = NodeCapabilities::new([
            NodeCapability::JsonRpc,
            NodeCapability::BestChainBlocks,
            NodeCapability::TipId,
            NodeCapability::TreeState,
            NodeCapability::SubtreeRoots,
        ])?;

        let Err(error) = require_broadcaster_node_capabilities(capabilities) else {
            return Err(Box::new(std::io::Error::other(
                "missing transaction-broadcast support passed startup validation",
            )));
        };

        assert!(matches!(
            error,
            zinder_source::SourceError::NodeCapabilityMissing {
                capability: NodeCapability::TransactionBroadcast,
            }
        ));

        Ok(())
    }
}
