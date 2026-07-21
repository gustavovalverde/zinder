//! Zinder native wallet-query gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc};

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_query::{
    ServerInfoSettings, WalletCapabilityProfile, WalletQueryGrpcAdapter, WalletServingPairConfig,
    WalletServingPairPublisher, WalletServingQuery, wallet_capability_strings,
};
use zinder_runtime::{
    Readiness, RuntimeService, StartupPhase, TrafficReadinessInterceptor,
    cancel_on_terminating_signal, install_tracing_subscriber, spawn_ops_endpoint_for,
};
use zinder_source::{ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};

mod config;

use config::{QueryConfigError, QueryConfigOverrides};

#[derive(Parser)]
#[command(name = "zinder-query")]
#[command(about = "Zinder native WalletQuery gRPC server")]
struct Cli {
    /// TOML configuration file loaded before environment variables and CLI overrides.
    #[arg(long = "config", global = true)]
    config_path: Option<PathBuf>,
    /// Print the resolved configuration without opening storage or binding.
    #[arg(long = "print-config", global = true)]
    print_config: bool,
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Canonical primary path replicated only through an immutable secondary.
    #[arg(long = "canonical-primary-path")]
    canonical_primary_path: Option<PathBuf>,
    /// Root containing this process's canonical secondary generations.
    #[arg(long = "canonical-secondary-root")]
    canonical_secondary_root: Option<PathBuf>,
    /// Wallet primary path replicated only through an immutable secondary.
    #[arg(long = "wallet-primary-path")]
    wallet_primary_path: Option<PathBuf>,
    /// Root containing this process's wallet secondary generations.
    #[arg(long = "wallet-secondary-root")]
    wallet_secondary_root: Option<PathBuf>,
    /// Private `zinder-ingest` control gRPC endpoint.
    #[arg(long = "ingest-control-addr")]
    ingest_control_addr: Option<String>,
    /// File containing the shared-secret bearer token used by `IngestControl`.
    #[arg(long = "ingest-control-token-path")]
    ingest_control_token_path: Option<PathBuf>,
    /// Native `WalletQuery` gRPC listen address.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Exact canonical replacement-depth identity expected from the writer.
    #[arg(long = "reorg-window-blocks")]
    reorg_window_blocks: Option<u32>,
    /// Operational HTTP endpoint listen address for health and metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
    /// Zebra JSON-RPC address used to discover the canonical activation table.
    #[arg(long = "node-json-rpc-addr")]
    node_json_rpc_addr: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    install_tracing_subscriber();
    if cli.print_config {
        return print_config(cli);
    }
    match run_query(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

#[allow(
    clippy::print_stdout,
    reason = "--print-config is a structured TOML data dump, not a log event"
)]
fn print_config(cli: Cli) -> ExitCode {
    let config_path = cli.config_path.clone();
    let rendered = config::load_query_config(config_path, cli.into())
        .and_then(|query_config| config::query_config_toml(&query_config));
    match rendered {
        Ok(rendered) => {
            println!("{rendered}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the linear startup sequence keeps operator-visible failure ordering auditable"
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
    readiness.set(zinder_runtime::ReadinessState::starting());
    let start_api_phase = StartupPhase::StartApi.start();
    let server_info = ServerInfoSettings {
        network: encode_zinder_native_chain_name(query_config.network).to_owned(),
        service_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_version: u32::from(zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value()),
        reorg_window_blocks: query_config.canonical_reorg_policy.reorg_window_blocks(),
        transaction_broadcast_enabled: true,
        chain_events_enabled: true,
        transparent_address_history_available: false,
        capability_profile: WalletCapabilityProfile::ExactPair,
        ..ServerInfoSettings::default()
    };
    let advertised_capabilities = wallet_capability_strings(&server_info);
    let ops_handle = spawn_ops_endpoint_for(
        RuntimeService::Query,
        query_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(query_config.network),
        readiness.clone(),
        advertised_capabilities.clone(),
    );

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let source = ZebraJsonRpcSource::with_options(
        query_config.node.network,
        query_config.node.json_rpc_addr.clone(),
        query_config.node.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: query_config.node.request_timeout,
            max_response_bytes: query_config.node.max_response_bytes,
            broadcast_timeout: query_config.node.broadcast_timeout,
        },
    )
    .map_err(|error| QueryConfigError::Source(Box::new(error)))?;
    let network_upgrade_activations = Arc::new(
        source
            .discover_network_upgrade_activations("zinder-query")
            .await
            .map_err(|error| QueryConfigError::Source(Box::new(error)))?,
    );
    connect_node_phase.complete();

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let (pair_publisher, pair_slot) = WalletServingPairPublisher::bootstrap(
        WalletServingPairConfig {
            canonical_primary_path: query_config.storage.path.clone(),
            canonical_secondary_root: query_config.storage.secondary_path.clone(),
            wallet_primary_path: query_config.wallet_primary_path.clone(),
            wallet_secondary_root: query_config.wallet_secondary_root.clone(),
            network: query_config.network,
            network_upgrade_activations: Arc::clone(&network_upgrade_activations),
            canonical_reorg_policy: query_config.canonical_reorg_policy,
            canonical_resource_budget: query_config.storage.canonical_rocksdb_budget,
            wallet_resource_budget: query_config.wallet_rocksdb_budget,
            catchup_interval: query_config.storage.secondary_catchup_interval,
            convergence_timeout: query_config.storage.initial_catchup_timeout,
            convergence_attempts: query_config.pair_convergence_attempts,
            replica_lag_threshold_chain_epochs: query_config
                .storage
                .secondary_replica_lag_threshold_chain_epochs,
        },
        readiness.clone(),
        &query_config.ingest_control_addr,
        query_config.ingest_control_bearer_token.as_ref(),
    )
    .await?;
    let visible_height = pair_slot
        .load_full()
        .canonical_fence()
        .visible_tip()
        .height
        .value();
    open_storage_phase.complete();

    let query = WalletServingQuery::from_serving_pair_slot(
        pair_slot,
        source.clone(),
        Arc::clone(&network_upgrade_activations),
    )
    .with_tree_state_upstream(Arc::new(source));
    let mut grpc_adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        query,
        server_info,
        query_config.ingest_control_addr.clone(),
    );
    if let Some(token) = query_config.ingest_control_bearer_token.clone() {
        grpc_adapter = grpc_adapter.with_ingest_control_bearer_token(token);
    }

    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(zinder_proto::ZINDER_V1_FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let publisher_handle = pair_publisher.spawn(cancel.clone());
    let traffic_readiness = TrafficReadinessInterceptor::new(readiness.clone());
    let reflection_readiness = TrafficReadinessInterceptor::new(readiness.clone());
    let grpc_service = tonic::service::interceptor::InterceptedService::new(
        grpc_adapter.into_server(),
        traffic_readiness,
    );
    let reflection_service = tonic::service::interceptor::InterceptedService::new(
        reflection_service,
        reflection_readiness,
    );

    tracing::info!(
        target: "zinder::query",
        event = "query_started",
        network = encode_zinder_native_chain_name(query_config.network),
        listen_addr = %query_config.listen_addr,
        visible_height,
        "native WalletQuery gRPC server started"
    );
    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_service)
        .add_service(reflection_service)
        .serve_with_shutdown(query_config.listen_addr, cancel.clone().cancelled_owned())
        .await;
    cancel.cancel();
    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }
    if let Err(join_error) = publisher_handle.await {
        tracing::warn!(
            target: "zinder::query",
            event = "wallet_serving_pair_publisher_join_failed",
            error = %join_error,
            "wallet-serving pair publisher task did not exit cleanly"
        );
    }
    server_result.map_err(QueryConfigError::Transport)
}

fn emit_runtime_error(error: &QueryConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::query",
        event = "query_run_failed",
        error = %error,
        "native query run failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for QueryConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            canonical_primary_path: cli.canonical_primary_path,
            canonical_secondary_root: cli.canonical_secondary_root,
            wallet_primary_path: cli.wallet_primary_path,
            wallet_secondary_root: cli.wallet_secondary_root,
            ingest_control_addr: cli.ingest_control_addr,
            ingest_control_bearer_token_path: cli.ingest_control_token_path,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
            node_json_rpc_addr: cli.node_json_rpc_addr,
            reorg_window_blocks: cli.reorg_window_blocks,
        }
    }
}
