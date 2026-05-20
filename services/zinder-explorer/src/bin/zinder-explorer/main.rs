//! Zinder explorer-plane gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_explorer::{
    BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter,
    ExplorerServerInfoSettings, MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY,
    RECENT_TRANSACTIONS_COLUMN_FAMILY, TRANSACTION_FEES_COLUMN_FAMILIES,
    TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILIES, describe_request_metrics,
};
use zinder_runtime::{
    Readiness, ReadinessState, ServiceIdentifier, StartupPhase, cancel_on_terminating_signal,
    install_tracing_subscriber, spawn_ops_endpoint_for,
};

mod config;
mod consumer_runner;

use config::{ExplorerConfig, ExplorerConfigError, ExplorerConfigOverrides};

/// Column-family list passed to `DeriveStore::open` at startup.
///
/// Aggregated from each consumer's `*_COLUMN_FAMILIES` constant so a new
/// consumer's additional column families are picked up by adding one
/// slice entry here.
fn consumer_column_families() -> Vec<&'static str> {
    let mut families: Vec<&'static str> = Vec::new();
    families.push(BLOCK_SUMMARY_COLUMN_FAMILY);
    families.push(MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY);
    families.push(RECENT_TRANSACTIONS_COLUMN_FAMILY);
    families.extend_from_slice(TRANSACTION_FEES_COLUMN_FAMILIES);
    families.extend_from_slice(TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILIES);
    families
}

/// Leaked &'static slice the store retains across its lifetime. Built once at
/// startup; size is bounded by the number of consumers compiled into the
/// binary.
fn consumer_column_families_static() -> &'static [&'static str] {
    Box::leak(consumer_column_families().into_boxed_slice())
}

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
    /// `WalletQuery` gRPC endpoint that backs the federated read paths.
    /// Empty/unset disables every capability that needs upstream reads.
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

    let store = match open_derive_store(&explorer_config) {
        Ok(store) => store,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());

    let (consumer_handles, prevouts_online) =
        match maybe_spawn_consumers(&explorer_config, store.clone(), cancel.clone()).await {
            Ok(spawned) => spawned,
            Err(error) => {
                start_api_phase.fail(&error);
                return Err(error);
            }
        };

    let grpc_adapter = build_grpc_adapter(&explorer_config, store, prevouts_online);
    let advertised_capabilities = grpc_adapter.advertised_capabilities();

    let ops_handle = spawn_ops_endpoint_for(
        ServiceIdentifier::Explorer,
        explorer_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(explorer_config.network),
        readiness.clone(),
        advertised_capabilities,
    );
    describe_request_metrics();

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
    for handle in consumer_handles {
        let _ = handle.await;
    }

    server_result.map_err(ExplorerConfigError::Transport)
}

fn open_derive_store(explorer_config: &ExplorerConfig) -> Result<DeriveStore, ExplorerConfigError> {
    let open_storage_phase = StartupPhase::OpenStorage.start();
    match DeriveStore::open(
        &explorer_config.storage_path,
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: consumer_column_families_static(),
            tuning: explorer_config.storage_tuning,
        },
    ) {
        Ok(handle) => {
            open_storage_phase.complete();
            Ok(handle)
        }
        Err(error) => {
            let wrapped = ExplorerConfigError::Store(error);
            open_storage_phase.fail(&wrapped);
            Err(wrapped)
        }
    }
}

fn build_grpc_adapter(
    explorer_config: &ExplorerConfig,
    store: DeriveStore,
    prevouts_online: bool,
) -> ExplorerQueryGrpcAdapter {
    let server_info = ExplorerServerInfoSettings {
        network: explorer_config.network,
    };
    let mut grpc_adapter = ExplorerQueryGrpcAdapter::new(server_info)
        .with_derive_store(store)
        .with_prevout_resolution_online(prevouts_online);
    if let Some(endpoint) = explorer_config.wallet_query_endpoint.clone() {
        grpc_adapter = grpc_adapter.with_wallet_query_endpoint(endpoint);
    }
    if let Some(token) = explorer_config.bearer_token.clone() {
        grpc_adapter = grpc_adapter.with_bearer_token(token);
    }
    grpc_adapter
}

async fn maybe_spawn_consumers(
    explorer_config: &ExplorerConfig,
    store: DeriveStore,
    cancel: CancellationToken,
) -> Result<(Vec<tokio::task::JoinHandle<()>>, bool), ExplorerConfigError> {
    let Some(endpoint) = explorer_config.wallet_query_endpoint.as_deref() else {
        return Ok((Vec::new(), false));
    };
    let (env, prevouts_online) = consumer_runner::build_environment(store, endpoint)
        .await
        .map_err(|error| ExplorerConfigError::ConsumerSetup(error.to_string()))?;
    let handles = consumer_runner::spawn_all(env, cancel);
    Ok((handles, prevouts_online))
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
