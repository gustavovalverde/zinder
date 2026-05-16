//! Zinder explorer-plane gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_explorer::{
    DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
use zinder_runtime::{
    OpsServer, Readiness, ReadinessState, StartupPhase, cancel_on_ctrl_c,
    install_tracing_subscriber, spawn_ops_endpoint,
};

mod config;

use config::{ExplorerConfig, ExplorerConfigError, ExplorerConfigOverrides};

#[derive(Parser)]
#[command(name = "zinder-explorer")]
#[command(about = "Zinder explorer-plane gRPC server")]
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
    /// Filesystem path opened by `DeriveStore`.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// `ExplorerQuery` gRPC listen address, such as 127.0.0.1:9068.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Path to a file containing the shared-secret bearer token enforced by
    /// the `ExplorerQuery` endpoint.
    #[arg(long = "bearer-token-path")]
    bearer_token_path: Option<PathBuf>,
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
    /// `WalletQuery` gRPC endpoint that backs the `TransparentAddressBalance`
    /// federated read path. Empty/unset disables the balance capability.
    #[arg(long = "wallet-query-endpoint")]
    wallet_query_endpoint: Option<String>,
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
    let render_result = config::load_explorer_config(config_path, cli.into())
        .and_then(|explorer_config| config::explorer_config_toml(&explorer_config));

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    match run_explorer(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_explorer(cli: Cli) -> Result<(), ExplorerConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let config_path = cli.config_path.clone();
    let ops_listen_addr_override = cli.ops_listen_addr;
    let explorer_config = match config::load_explorer_config(config_path, cli.into()) {
        Ok(cfg) => {
            load_config_phase.complete();
            cfg
        }
        Err(error) => {
            load_config_phase.fail(&error);
            return Err(error);
        }
    };
    let readiness = Readiness::default();
    readiness.set(ReadinessState::starting());
    let start_api_phase = StartupPhase::StartApi.start();
    let ops_handle = explorer_config
        .ops_listen_addr
        .or(ops_listen_addr_override)
        .map(|listen_addr| spawn_ops(listen_addr, &explorer_config, &readiness));

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let _store =
        match DeriveStore::open(&explorer_config.storage_path, DeriveStoreOptions::default()) {
            Ok(handle) => {
                open_storage_phase.complete();
                handle
            }
            Err(error) => {
                let wrapped = ExplorerConfigError::Store(error);
                open_storage_phase.fail(&wrapped);
                start_api_phase.fail(&wrapped);
                return Err(wrapped);
            }
        };
    let server_info = ExplorerServerInfoSettings {
        network: encode_zinder_native_chain_name(explorer_config.network).to_owned(),
    };
    let mut grpc_adapter = ExplorerQueryGrpcAdapter::new(server_info);
    if let Some(endpoint) = explorer_config.wallet_query_endpoint.clone() {
        grpc_adapter = grpc_adapter.with_wallet_query_endpoint(endpoint);
    }
    if let Some(token) = explorer_config.bearer_token.clone() {
        grpc_adapter = grpc_adapter.with_bearer_token(token);
    }
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    readiness.set(ReadinessState::ready(None));

    tracing::info!(
        target: "zinder::explorer",
        event = "explorer_started",
        network = encode_zinder_native_chain_name(explorer_config.network),
        listen_addr = %explorer_config.listen_addr,
        storage_path = %explorer_config.storage_path.display(),
        "explorer query gRPC server started"
    );

    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_adapter.into_server())
        .serve_with_shutdown(explorer_config.listen_addr, cancel.cancelled_owned())
        .await;

    tracing::info!(
        target: "zinder::explorer",
        event = "explorer_stopped",
        "explorer query gRPC server stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    server_result.map_err(ExplorerConfigError::Transport)
}

fn spawn_ops(
    listen_addr: SocketAddr,
    explorer_config: &ExplorerConfig,
    readiness: &Readiness,
) -> zinder_runtime::OpsEndpointHandle {
    spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-explorer",
            service_version: env!("CARGO_PKG_VERSION"),
            network_name: encode_zinder_native_chain_name(explorer_config.network),
        },
        readiness.clone(),
    )
}

fn emit_runtime_error(error: &ExplorerConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::explorer",
        event = "explorer_run_failed",
        error = %error,
        "explorer run failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for ExplorerConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            storage_path: cli.storage_path,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
            bearer_token_path: cli.bearer_token_path,
            wallet_query_endpoint: cli.wallet_query_endpoint,
        }
    }
}
