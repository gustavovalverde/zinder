//! Zinder explorer-plane gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_explorer::{
    DeriveStore, DeriveStoreOptions, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
    describe_request_metrics,
};
use zinder_runtime::{
    Readiness, ReadinessState, ServiceIdentifier, StartupPhase, cancel_on_terminating_signal,
    install_tracing_subscriber, spawn_ops_endpoint_for,
};
use zinder_source::{NodeTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::{ChainStoreOptions, SecondaryChainStore};

mod config;

use config::{ExplorerConfig, ExplorerConfigError, ExplorerConfigOverrides};

/// Cadence the background task uses to advance the secondary's view to the
/// primary's latest durable state.
const DERIVE_CATCHUP_INTERVAL: Duration = Duration::from_secs(1);

/// Default cadence for the upstream-observation probe.
///
/// Used when the resolved [`NodeTarget`] does not pin a `[node.health]`
/// `poll_interval`. Mirrors the source plane's default so an
/// explorer-side operator sees the same cadence as the ingest-side
/// probe.
const DEFAULT_UPSTREAM_OBSERVATION_POLL_INTERVAL: Duration =
    Duration::from_millis(zinder_source::DEFAULT_NODE_HEALTH_POLL_INTERVAL_MS);

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
    /// Filesystem path of the canonical store the writer opens as primary.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Process-unique `RocksDB` secondary metadata path.
    #[arg(long = "secondary-path")]
    secondary_path: Option<PathBuf>,
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

    let canonical_store = match open_canonical_store(&explorer_config) {
        Ok(store) => store,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };

    let store = match open_derive_store(&explorer_config) {
        Ok(store) => store,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());

    let catchup_handle = spawn_derive_catchup_task(store.clone(), cancel.clone());
    let canonical_catchup_handle =
        spawn_canonical_catchup_task(canonical_store.clone(), cancel.clone());

    let grpc_adapter = build_grpc_adapter(&explorer_config, canonical_store, store);
    let upstream_observation_handle =
        spawn_upstream_observation_probe(&explorer_config, &grpc_adapter, cancel.clone())?;
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
        storage_path = %explorer_config.storage.path.display(),
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
    if let Some(handle) = upstream_observation_handle {
        let _ = handle.await;
    }
    let _ = canonical_catchup_handle.await;
    let _ = catchup_handle.await;

    server_result.map_err(ExplorerConfigError::Transport)
}

fn spawn_upstream_observation_probe(
    explorer_config: &ExplorerConfig,
    grpc_adapter: &ExplorerQueryGrpcAdapter,
    cancel: CancellationToken,
) -> Result<Option<JoinHandle<()>>, ExplorerConfigError> {
    let Some(node) = explorer_config.node.as_ref() else {
        tracing::info!(
            target: "zinder::explorer",
            event = "upstream_observation_probe_skipped",
            reason = "no [node] section configured; ExplorerFreshness.chain_view.upstream_tip stays unset",
        );
        return Ok(None);
    };
    let source = build_zebra_json_rpc_source(node)?;
    let poll_interval = node
        .health
        .as_ref()
        .map_or(DEFAULT_UPSTREAM_OBSERVATION_POLL_INTERVAL, |health| {
            health.poll_interval
        });
    tracing::info!(
        target: "zinder::explorer",
        event = "upstream_observation_probe_started",
        json_rpc_addr = node.json_rpc_addr.as_str(),
        poll_interval_ms = u64::try_from(poll_interval.as_millis()).unwrap_or(u64::MAX),
        "upstream observation probe started",
    );
    Ok(Some(grpc_adapter.spawn_upstream_observation_probe(
        Arc::new(source),
        poll_interval,
        cancel,
    )))
}

fn build_zebra_json_rpc_source(
    node: &NodeTarget,
) -> Result<ZebraJsonRpcSource, ExplorerConfigError> {
    let source = ZebraJsonRpcSource::with_options(
        node.network,
        &node.json_rpc_addr,
        node.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: node.request_timeout,
            max_response_bytes: node.max_response_bytes,
            broadcast_timeout: node.broadcast_timeout,
        },
    )?;
    Ok(source.with_health_config(node.health.clone()))
}

fn open_canonical_store(
    explorer_config: &ExplorerConfig,
) -> Result<SecondaryChainStore, ExplorerConfigError> {
    let open_storage_phase = StartupPhase::OpenStorage.start();
    match SecondaryChainStore::open(
        &explorer_config.storage.path,
        &explorer_config.storage.secondary_path,
        ChainStoreOptions {
            rocksdb_resource_budget: explorer_config.storage.canonical_rocksdb_budget,
            ..ChainStoreOptions::for_network(explorer_config.network)
        },
    ) {
        Ok(handle) => {
            handle.try_catch_up()?;
            open_storage_phase.complete();
            Ok(handle)
        }
        Err(error) => {
            let wrapped = ExplorerConfigError::CanonicalStore(error);
            open_storage_phase.fail(&wrapped);
            Err(wrapped)
        }
    }
}

fn open_derive_store(explorer_config: &ExplorerConfig) -> Result<DeriveStore, ExplorerConfigError> {
    let derive_path = DeriveStore::path_for_canonical(&explorer_config.storage.path);
    let secondary_path = explorer_config.storage.secondary_path.join("derive");
    let open_storage_phase = StartupPhase::OpenStorage.start();
    match DeriveStore::open_secondary(
        &derive_path,
        &secondary_path,
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: DeriveStore::bundled_consumer_column_families(),
            rocksdb_resource_budget: explorer_config.storage.derive_rocksdb_budget,
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

fn spawn_canonical_catchup_task(
    store: SecondaryChainStore,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DERIVE_CATCHUP_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = store.try_catch_up() {
                        tracing::warn!(
                            target: "zinder::explorer",
                            event = "canonical_secondary_catchup_failed",
                            error = %error,
                            "canonical store secondary catchup failed"
                        );
                    }
                }
                () = cancel.cancelled() => break,
            }
        }
    })
}

fn spawn_derive_catchup_task(
    store: DeriveStore,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DERIVE_CATCHUP_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = store.try_catch_up() {
                        tracing::warn!(
                            target: "zinder::explorer",
                            event = "derive_secondary_catchup_failed",
                            error = %error,
                            "derive store secondary catchup failed"
                        );
                    }
                }
                () = cancel.cancelled() => break,
            }
        }
    })
}

fn build_grpc_adapter(
    explorer_config: &ExplorerConfig,
    canonical_store: SecondaryChainStore,
    store: DeriveStore,
) -> ExplorerQueryGrpcAdapter {
    let server_info = ExplorerServerInfoSettings {
        network: explorer_config.network,
    };
    let mut grpc_adapter = ExplorerQueryGrpcAdapter::new(server_info)
        .with_canonical_store(canonical_store)
        .with_derive_store(store)
        .with_prevout_resolution_online(true);
    if let Some(endpoint) = explorer_config.wallet_query_endpoint.clone() {
        grpc_adapter = grpc_adapter.with_wallet_query_endpoint(endpoint);
    }
    if let Some(token) = explorer_config.bearer_token.clone() {
        grpc_adapter = grpc_adapter.with_bearer_token(token);
    }
    grpc_adapter
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
            secondary_path: cli.secondary_path,
            listen_addr: cli.listen_addr,
            ops_listen_addr: cli.ops_listen_addr,
            bearer_token_path: cli.bearer_token_path,
            wallet_query_endpoint: cli.wallet_query_endpoint,
        }
    }
}
