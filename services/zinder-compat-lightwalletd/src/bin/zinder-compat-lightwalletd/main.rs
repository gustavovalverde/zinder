//! Zinder lightwalletd-compatible gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_runtime::{
    OpsServer, Readiness, cancel_on_ctrl_c, install_tracing_subscriber, spawn_ops_endpoint,
};
use zinder_source::{ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::SecondaryChainStore;

mod config;

use config::{
    BroadcasterConfig, LightwalletdConfig, LightwalletdConfigError, LightwalletdConfigOverrides,
};

#[derive(Parser)]
#[command(name = "zinder-compat-lightwalletd")]
#[command(about = "Zinder lightwalletd-compatible gRPC server")]
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
    /// Canonical Zinder store path opened by this compatibility process.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Process-unique `RocksDB` secondary metadata path.
    #[arg(long = "secondary-path")]
    secondary_path: Option<PathBuf>,
    /// Private `zinder-ingest` control gRPC endpoint.
    #[arg(long = "ingest-control-addr")]
    ingest_control_addr: Option<String>,
    /// Lightwalletd-compatible gRPC listen address, such as 127.0.0.1:9067.
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
    let render_result = config::load_lightwalletd_config(config_path, cli.into())
        .and_then(|cfg| config::lightwalletd_config_toml(&cfg));

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    match run_lightwalletd(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_lightwalletd(cli: Cli) -> Result<(), LightwalletdConfigError> {
    let config_path = cli.config_path.clone();
    let ops_listen_addr = cli.ops_listen_addr;
    let lightwalletd_config = config::load_lightwalletd_config(config_path, cli.into())?;
    let readiness = Readiness::default();
    let ops_handle =
        ops_listen_addr.map(|listen_addr| spawn_ops(listen_addr, &lightwalletd_config, &readiness));
    let store = SecondaryChainStore::open(
        &lightwalletd_config.storage_path,
        &lightwalletd_config.secondary_path,
        zinder_store::ChainStoreOptions::for_network(lightwalletd_config.network),
    )
    .map_err(LightwalletdConfigError::Store)?;
    let visible_height = store
        .current_chain_epoch()
        .map_err(LightwalletdConfigError::Store)?
        .map(|epoch| epoch.tip_height.value());
    let broadcaster = build_broadcaster(
        lightwalletd_config.network,
        lightwalletd_config.broadcaster.as_ref(),
    )?;
    let wallet_query = zinder_query::WalletQuery::new(store.clone(), broadcaster);
    let grpc_adapter = LightwalletdGrpcAdapter::new(wallet_query);
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    let _refresh_handle = zinder_query::spawn_secondary_catchup(
        store,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: lightwalletd_config.secondary_catchup_interval,
            lag_threshold_chain_epochs: lightwalletd_config
                .secondary_replica_lag_threshold_chain_epochs,
            writer_status: Some(zinder_query::WriterStatusConfig {
                endpoint: lightwalletd_config.ingest_control_addr.clone(),
                network: lightwalletd_config.network,
            }),
        },
        cancel.clone(),
    );

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "compat_started",
        network = lightwalletd_config.network.name(),
        listen_addr = %lightwalletd_config.listen_addr,
        visible_height = ?visible_height,
        "lightwalletd-compatible gRPC server started"
    );

    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_adapter.into_server())
        .serve_with_shutdown(lightwalletd_config.listen_addr, cancel.cancelled_owned())
        .await;

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "compat_stopped",
        "lightwalletd-compatible gRPC server stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    server_result.map_err(LightwalletdConfigError::Transport)
}

fn build_broadcaster(
    network: zinder_core::Network,
    broadcaster_config: Option<&BroadcasterConfig>,
) -> Result<Option<ZebraJsonRpcSource>, LightwalletdConfigError> {
    let Some(broadcaster_config) = broadcaster_config else {
        tracing::info!(
            target: "zinder::compat_lightwalletd",
            event = "transaction_broadcast_disabled",
            "transaction broadcast disabled because [node] is not configured"
        );
        return Ok(None);
    };

    let source = ZebraJsonRpcSource::with_options(
        network,
        broadcaster_config.json_rpc_addr.clone(),
        broadcaster_config.auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: broadcaster_config.request_timeout,
            max_response_bytes: broadcaster_config.max_response_bytes,
        },
    )
    .map_err(|source| LightwalletdConfigError::Source(Box::new(source)))?;

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "transaction_broadcast_enabled",
        json_rpc_addr = %broadcaster_config.json_rpc_addr,
        "transaction broadcast enabled via Zebra JSON-RPC"
    );
    Ok(Some(source))
}

fn spawn_ops(
    listen_addr: SocketAddr,
    lightwalletd_config: &LightwalletdConfig,
    readiness: &Readiness,
) -> zinder_runtime::OpsEndpointHandle {
    spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-compat-lightwalletd",
            service_version: env!("CARGO_PKG_VERSION"),
            network_name: lightwalletd_config.network.name(),
        },
        readiness.clone(),
    )
}

fn emit_runtime_error(error: &LightwalletdConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::compat_lightwalletd",
        event = "compat_run_failed",
        error = %error,
        "compat run failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for LightwalletdConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            storage_path: cli.storage_path,
            secondary_path: cli.secondary_path,
            ingest_control_addr: cli.ingest_control_addr,
            listen_addr: cli.listen_addr,
            node_json_rpc_addr: cli.node_json_rpc_addr,
        }
    }
}
