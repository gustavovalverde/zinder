//! Zinder wallet query gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

use clap::Parser;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use zinder_runtime::{
    OpsServer, Readiness, cancel_on_ctrl_c, install_tracing_subscriber, spawn_ops_endpoint,
};
use zinder_source::{ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::SecondaryChainStore;

mod config;

use config::{QueryConfig, QueryConfigError, QueryConfigOverrides};

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
    /// Chain-event retention window in hours, advertised through `ServerInfo`.
    #[arg(long = "chain-event-retention-hours")]
    chain_event_retention_hours: Option<u64>,
    /// Native wallet query gRPC listen address, such as 127.0.0.1:9101.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
    /// Node JSON-RPC address used for transaction broadcast. Omit to disable broadcast.
    #[arg(long = "node-json-rpc-addr")]
    node_json_rpc_addr: Option<String>,
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
    let config_path = cli.config_path.clone();
    let ops_listen_addr = cli.ops_listen_addr;
    let query_config = config::load_query_config(config_path, cli.into())?;
    let readiness = Readiness::default();
    let ops_handle =
        ops_listen_addr.map(|listen_addr| spawn_ops(listen_addr, &query_config, &readiness));
    let store = SecondaryChainStore::open(
        &query_config.storage_path,
        &query_config.secondary_path,
        zinder_store::ChainStoreOptions::for_network(query_config.network),
    )
    .map_err(QueryConfigError::Store)?;
    let visible_height = store
        .current_chain_epoch()
        .map_err(QueryConfigError::Store)?
        .map(|epoch| epoch.tip_height.value());
    let broadcaster = build_broadcaster(query_config.broadcaster.as_ref())?;
    let transaction_broadcast_enabled = broadcaster.is_some();
    let wallet_query = zinder_query::WalletQuery::new(store.clone(), broadcaster);
    let server_info = zinder_query::ServerInfoSettings {
        network: query_config.network.name().to_owned(),
        transaction_broadcast_enabled,
        chain_event_retention_seconds: query_config.chain_event_retention_seconds,
        ..zinder_query::ServerInfoSettings::default()
    };
    let grpc_adapter = zinder_query::WalletQueryGrpcAdapter::with_chain_events_proxy(
        wallet_query,
        server_info,
        query_config.ingest_control_addr.clone(),
    );
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    let _refresh_handle = zinder_query::spawn_secondary_catchup(
        store,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: query_config.secondary_catchup_interval,
            lag_threshold_chain_epochs: query_config.secondary_replica_lag_threshold_chain_epochs,
            writer_status: Some(zinder_query::WriterStatusConfig {
                endpoint: query_config.ingest_control_addr.clone(),
                network: query_config.network,
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
        network = query_config.network.name(),
        listen_addr = %query_config.listen_addr,
        visible_height = ?visible_height,
        grpc_reflection_enabled = query_config.grpc.enable_reflection,
        grpc_health_enabled = query_config.grpc.enable_health,
        "wallet query gRPC server started"
    );

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

fn build_broadcaster(
    broadcaster_target: Option<&zinder_source::NodeTarget>,
) -> Result<Option<ZebraJsonRpcSource>, QueryConfigError> {
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
        },
    )
    .map_err(|source| QueryConfigError::Source(Box::new(source)))?;

    tracing::info!(
        target: "zinder::query",
        event = "transaction_broadcast_enabled",
        json_rpc_addr = %target.json_rpc_addr,
        "transaction broadcast enabled via Zebra JSON-RPC"
    );
    Ok(Some(source))
}

fn spawn_ops(
    listen_addr: SocketAddr,
    query_config: &QueryConfig,
    readiness: &Readiness,
) -> zinder_runtime::OpsEndpointHandle {
    spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-query",
            service_version: env!("CARGO_PKG_VERSION"),
            network_name: query_config.network.name(),
        },
        readiness.clone(),
    )
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
            chain_event_retention_hours: cli.chain_event_retention_hours,
            listen_addr: cli.listen_addr,
            node_json_rpc_addr: cli.node_json_rpc_addr,
        }
    }
}
