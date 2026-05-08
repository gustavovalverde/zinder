//! Zinder derive-plane gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_derive::{
    DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
use zinder_runtime::{
    OpsServer, Readiness, ReadinessState, cancel_on_ctrl_c, install_tracing_subscriber,
    spawn_ops_endpoint,
};

mod config;

use config::{DeriveConfig, DeriveConfigError, DeriveConfigOverrides};

#[derive(Parser)]
#[command(name = "zinder-derive")]
#[command(about = "Zinder derive-plane gRPC server")]
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
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr")]
    ops_listen_addr: Option<SocketAddr>,
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
    let render_result = config::load_derive_config(config_path, cli.into())
        .and_then(|derive_config| config::derive_config_toml(&derive_config));

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    match run_derive(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_derive(cli: Cli) -> Result<(), DeriveConfigError> {
    let config_path = cli.config_path.clone();
    let ops_listen_addr_override = cli.ops_listen_addr;
    let derive_config = config::load_derive_config(config_path, cli.into())?;
    let readiness = Readiness::default();
    readiness.set(ReadinessState::starting());
    let ops_handle = derive_config
        .ops_listen_addr
        .or(ops_listen_addr_override)
        .map(|listen_addr| spawn_ops(listen_addr, &derive_config, &readiness));
    let _store = DeriveStore::open(&derive_config.storage_path, DeriveStoreOptions::default())
        .map_err(DeriveConfigError::Store)?;
    let server_info = ExplorerServerInfoSettings {
        network: derive_config.network.name().to_owned(),
    };
    let grpc_adapter = ExplorerQueryGrpcAdapter::new(server_info);
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    readiness.set(ReadinessState::ready(None));

    tracing::info!(
        target: "zinder::derive",
        event = "derive_started",
        network = derive_config.network.name(),
        listen_addr = %derive_config.listen_addr,
        storage_path = %derive_config.storage_path.display(),
        "explorer query gRPC server started"
    );

    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_adapter.into_server())
        .serve_with_shutdown(derive_config.listen_addr, cancel.cancelled_owned())
        .await;

    tracing::info!(
        target: "zinder::derive",
        event = "derive_stopped",
        "explorer query gRPC server stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    server_result.map_err(DeriveConfigError::Transport)
}

fn spawn_ops(
    listen_addr: SocketAddr,
    derive_config: &DeriveConfig,
    readiness: &Readiness,
) -> zinder_runtime::OpsEndpointHandle {
    spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-derive",
            service_version: env!("CARGO_PKG_VERSION"),
            network_name: derive_config.network.name(),
        },
        readiness.clone(),
    )
}

fn emit_runtime_error(error: &DeriveConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::derive",
        event = "derive_run_failed",
        error = %error,
        "derive run failed"
    );
    ExitCode::FAILURE
}

impl From<Cli> for DeriveConfigOverrides {
    fn from(cli: Cli) -> Self {
        Self {
            network: cli.network,
            storage_path: cli.storage_path,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
        }
    }
}
