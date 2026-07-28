//! Zinder explorer-plane gRPC server entry point.

use std::{
    future::Future, net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration,
};
use zinder_core::NetworkUpgradeActivations;
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use parking_lot::Mutex;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use zinder_explorer::{
    ExplorerEndpointMetadata, ExplorerQueryEndpointComposition, ExplorerQueryGrpcAdapter,
    ExplorerWalletQueryHealthError, MaterializedViewStore, MaterializedViewStoreError,
    MaterializedViewStoreOptions, describe_request_metrics,
};
use zinder_runtime::{
    OpsEndpointHandle, OpsServerError, Readiness, ReadinessCause, ReadinessState, RuntimeService,
    StartupPhase, TrafficReadinessInterceptor, cancel_on_terminating_signal,
    host_cpu_meets_compiled_baseline, install_tracing_subscriber, spawn_ops_endpoint_for,
};
use zinder_source::{NodeTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};

mod config;

use config::{ExplorerConfig, ExplorerConfigError, ExplorerConfigOverrides};

/// Cadence the background task uses to advance the secondary's view to the
/// primary's latest durable state.
const MATERIALIZED_VIEW_CATCHUP_INTERVAL: Duration = Duration::from_secs(1);

/// Fixed cadence for the admitted `WalletQuery` dependency-health check.
///
/// This is intentionally independent of `[node.health]`: the latter controls
/// optional Zebra observation, not the required native `WalletQuery` contract.
const WALLET_QUERY_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
#[command(version)]
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
    /// Canonical storage root containing the explorer materialized-view store.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Process-unique metadata root for explorer `RocksDB` secondaries.
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
    /// Path to a file containing the bearer token sent to `WalletQuery`.
    #[arg(long = "wallet-query-bearer-token-path")]
    wallet_query_bearer_token_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    install_tracing_subscriber();

    if !host_cpu_meets_compiled_baseline() {
        return ExitCode::FAILURE;
    }

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

#[allow(
    clippy::too_many_lines,
    reason = "startup keeps configuration, secondary admission, and server wiring in one path"
)]
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
    let serving_readiness = ExplorerServingReadiness::new(readiness.clone());
    let start_api_phase = StartupPhase::StartApi.start();

    let materialized_view_store = match open_materialized_view_store(&explorer_config) {
        Ok(materialized_view_store) => materialized_view_store,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    report_materialized_view_workload(&readiness, materialized_view_store.as_ref());

    let cancel = CancellationToken::new();

    let grpc_adapter =
        match build_grpc_adapter(&explorer_config, materialized_view_store.clone()).await {
            Ok(grpc_adapter) => grpc_adapter,
            Err(error) => {
                start_api_phase.fail(&error);
                return Err(error);
            }
        };
    let grpc_listener = match TcpListener::bind(explorer_config.listen_addr)
        .await
        .map_err(|source| ExplorerConfigError::GrpcBind {
            listen_addr: explorer_config.listen_addr,
            source,
        }) {
        Ok(grpc_listener) => grpc_listener,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    let upstream_observation_composition =
        match compose_upstream_observation_probe(&explorer_config) {
            Ok(composition) => composition,
            Err(error) => {
                start_api_phase.fail(&error);
                return Err(error);
            }
        };
    let advertised_capabilities = grpc_adapter.advertised_capabilities();

    let ops_handle = match spawn_ops_endpoint_for(
        RuntimeService::Explorer,
        explorer_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(explorer_config.network),
        readiness.clone(),
        advertised_capabilities,
    )
    .await
    {
        Ok(ops_handle) => ops_handle,
        Err(error) => {
            let error = ExplorerConfigError::from(error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    describe_request_metrics();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let materialized_view_catchup_handle = materialized_view_store.map(|materialized_view_store| {
        spawn_materialized_view_catchup_task(materialized_view_store, cancel.clone())
    });
    let upstream_observation_handle = spawn_upstream_observation_probe(
        upstream_observation_composition,
        &grpc_adapter,
        cancel.clone(),
    );
    let wallet_query_health_handle =
        spawn_wallet_query_health_probe(&grpc_adapter, serving_readiness.clone(), cancel.clone());

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    serving_readiness.publish_started();

    tracing::info!(
        target: "zinder::explorer",
        event = "explorer_started",
        network = encode_zinder_native_chain_name(explorer_config.network),
        listen_addr = %explorer_config.listen_addr,
        storage_path = %explorer_config.storage.canonical_root_path.display(),
        "explorer query gRPC server started"
    );

    let grpc_service = tonic::service::interceptor::InterceptedService::new(
        grpc_adapter.into_server(),
        TrafficReadinessInterceptor::new(readiness),
    );
    let server = tonic::transport::Server::builder()
        .add_service(grpc_service)
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc_listener),
            cancel.clone().cancelled_owned(),
        );
    supervise_explorer_runtime(
        server,
        cancel,
        &serving_readiness,
        ExplorerBackgroundTasks {
            operations: ops_handle,
            wallet_query_health: wallet_query_health_handle,
            upstream_observation: upstream_observation_handle,
            materialized_view_catchup: materialized_view_catchup_handle,
        },
    )
    .await
}

fn report_materialized_view_workload(
    readiness: &Readiness,
    materialized_view_store: Option<&MaterializedViewStore>,
) {
    let Some(materialized_view_preset) =
        materialized_view_store.map(MaterializedViewStore::effective_materialized_view_preset)
    else {
        return;
    };
    readiness.set_materialized_view_workload(
        materialized_view_preset.as_str(),
        materialized_view_store
            .into_iter()
            .flat_map(MaterializedViewStore::declared_consumer_names)
            .map(|name| name.as_str().to_owned())
            .collect(),
    );
}

/// Conjunctive readiness owner for the Explorer runtime lifecycle.
///
/// Startup and the admitted `WalletQuery` dependency publish independent inputs.
/// Shutdown is irreversible and dominates later probe recovery.
#[derive(Clone)]
struct ExplorerServingReadiness {
    runtime: Readiness,
    state: Arc<Mutex<ExplorerServingReadinessState>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExplorerServingReadinessState {
    startup_complete: bool,
    wallet_query_health: WalletQueryHealthState,
    is_shutting_down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletQueryHealthState {
    Available,
    Unavailable,
    ContractMismatch,
}

impl ExplorerServingReadiness {
    fn new(runtime: Readiness) -> Self {
        let readiness = Self {
            runtime,
            state: Arc::new(Mutex::new(ExplorerServingReadinessState {
                startup_complete: false,
                wallet_query_health: WalletQueryHealthState::Available,
                is_shutting_down: false,
            })),
        };
        readiness.publish_projection();
        readiness
    }

    fn publish_started(&self) {
        let mut state = self.state.lock();
        state.startup_complete = true;
        self.runtime.set(Self::projected_readiness(*state));
    }

    fn publish_wallet_query_health(&self, health: WalletQueryHealthState) -> bool {
        let mut state = self.state.lock();
        let changed = state.wallet_query_health != health;
        state.wallet_query_health = health;
        self.runtime.set(Self::projected_readiness(*state));
        changed
    }

    fn publish_shutting_down(&self) {
        let mut state = self.state.lock();
        state.is_shutting_down = true;
        self.runtime.set(Self::projected_readiness(*state));
    }

    fn publish_projection(&self) {
        let state = self.state.lock();
        self.runtime.set(Self::projected_readiness(*state));
    }

    fn projected_readiness(state: ExplorerServingReadinessState) -> ReadinessState {
        if state.is_shutting_down {
            ReadinessState::not_ready(ReadinessCause::ShuttingDown)
        } else if !state.startup_complete {
            ReadinessState::starting()
        } else if !matches!(state.wallet_query_health, WalletQueryHealthState::Available) {
            ReadinessState::not_ready(ReadinessCause::WalletQueryUnavailable)
        } else {
            ReadinessState::ready(None)
        }
    }
}

struct ExplorerBackgroundTasks {
    operations: Option<OpsEndpointHandle>,
    wallet_query_health: Option<JoinHandle<()>>,
    upstream_observation: Option<JoinHandle<()>>,
    materialized_view_catchup: Option<JoinHandle<()>>,
}

enum ExplorerRuntimeExit {
    ShutdownRequested,
    GrpcServer(Result<(), tonic::transport::Error>),
    WalletQueryHealth(Result<(), tokio::task::JoinError>),
    UpstreamObservation(Result<(), tokio::task::JoinError>),
    MaterializedViewCatchup(Result<(), tokio::task::JoinError>),
    Operations(Result<(), OpsServerError>),
}

#[allow(
    clippy::too_many_lines,
    reason = "The supervisor keeps each owned runtime task's exit and drain policy explicit in one exhaustive select."
)]
async fn supervise_explorer_runtime<Server>(
    server: Server,
    cancel: CancellationToken,
    readiness: &ExplorerServingReadiness,
    background_tasks: ExplorerBackgroundTasks,
) -> Result<(), ExplorerConfigError>
where
    Server: Future<Output = Result<(), tonic::transport::Error>>,
{
    let ExplorerBackgroundTasks {
        mut operations,
        mut wallet_query_health,
        mut upstream_observation,
        mut materialized_view_catchup,
    } = background_tasks;
    tokio::pin!(server);
    let exit = tokio::select! {
        biased;
        () = cancel.cancelled() => ExplorerRuntimeExit::ShutdownRequested,
        server_outcome = &mut server => ExplorerRuntimeExit::GrpcServer(server_outcome),
        task_outcome = wait_for_optional_task(&mut wallet_query_health) => {
            ExplorerRuntimeExit::WalletQueryHealth(task_outcome)
        }
        task_outcome = wait_for_optional_task(&mut upstream_observation) => {
            ExplorerRuntimeExit::UpstreamObservation(task_outcome)
        }
        task_outcome = wait_for_optional_task(&mut materialized_view_catchup) => {
            ExplorerRuntimeExit::MaterializedViewCatchup(task_outcome)
        }
        operations_outcome = wait_for_operations_exit(&mut operations) => {
            ExplorerRuntimeExit::Operations(operations_outcome)
        }
    };

    readiness.publish_shutting_down();
    cancel.cancel();

    match exit {
        ExplorerRuntimeExit::ShutdownRequested => {
            let server_result = server.await;
            let wallet_health_result =
                await_optional_task("wallet-query health probe", &mut wallet_query_health).await;
            let upstream_result =
                await_optional_task("upstream-observation probe", &mut upstream_observation).await;
            let catchup_result =
                await_optional_task("materialized-view catchup", &mut materialized_view_catchup)
                    .await;
            let operations_result = shutdown_operations(&mut operations).await;
            server_result.map_err(ExplorerConfigError::Transport)?;
            wallet_health_result?;
            upstream_result?;
            catchup_result?;
            operations_result?;
            Ok(())
        }
        ExplorerRuntimeExit::GrpcServer(server_result) => {
            drain_optional_task("wallet-query health probe", &mut wallet_query_health).await;
            drain_optional_task("upstream-observation probe", &mut upstream_observation).await;
            drain_optional_task("materialized-view catchup", &mut materialized_view_catchup).await;
            drain_operations(&mut operations).await;
            server_result.map_err(ExplorerConfigError::Transport)?;
            Err(ExplorerConfigError::GrpcServerStopped)
        }
        ExplorerRuntimeExit::WalletQueryHealth(task_result) => {
            wallet_query_health.take();
            let primary = unexpected_task_exit("wallet-query health probe", task_result);
            drain_server(server.await);
            drain_optional_task("upstream-observation probe", &mut upstream_observation).await;
            drain_optional_task("materialized-view catchup", &mut materialized_view_catchup).await;
            drain_operations(&mut operations).await;
            Err(primary)
        }
        ExplorerRuntimeExit::UpstreamObservation(task_result) => {
            upstream_observation.take();
            let primary = unexpected_task_exit("upstream-observation probe", task_result);
            drain_server(server.await);
            drain_optional_task("wallet-query health probe", &mut wallet_query_health).await;
            drain_optional_task("materialized-view catchup", &mut materialized_view_catchup).await;
            drain_operations(&mut operations).await;
            Err(primary)
        }
        ExplorerRuntimeExit::MaterializedViewCatchup(task_result) => {
            materialized_view_catchup.take();
            let primary = unexpected_task_exit("materialized-view catchup", task_result);
            drain_server(server.await);
            drain_optional_task("wallet-query health probe", &mut wallet_query_health).await;
            drain_optional_task("upstream-observation probe", &mut upstream_observation).await;
            drain_operations(&mut operations).await;
            Err(primary)
        }
        ExplorerRuntimeExit::Operations(operations_result) => {
            let primary = unexpected_operations_exit(operations_result);
            drain_server(server.await);
            drain_optional_task("wallet-query health probe", &mut wallet_query_health).await;
            drain_optional_task("upstream-observation probe", &mut upstream_observation).await;
            drain_optional_task("materialized-view catchup", &mut materialized_view_catchup).await;
            drain_operations(&mut operations).await;
            Err(primary)
        }
    }
}

async fn wait_for_optional_task(
    task: &mut Option<JoinHandle<()>>,
) -> Result<(), tokio::task::JoinError> {
    match task {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_operations_exit(
    operations: &mut Option<OpsEndpointHandle>,
) -> Result<(), OpsServerError> {
    match operations {
        Some(handle) => handle.wait().await,
        None => std::future::pending().await,
    }
}

async fn await_optional_task(
    task_name: &'static str,
    task: &mut Option<JoinHandle<()>>,
) -> Result<(), ExplorerConfigError> {
    match task.take() {
        Some(handle) => handle
            .await
            .map_err(|source| ExplorerConfigError::RuntimeTaskJoin {
                task: task_name,
                source,
            }),
        None => Ok(()),
    }
}

async fn shutdown_operations(
    operations: &mut Option<OpsEndpointHandle>,
) -> Result<(), ExplorerConfigError> {
    match operations.take() {
        Some(handle) => handle
            .shutdown()
            .await
            .map_err(ExplorerConfigError::OpsServer),
        None => Ok(()),
    }
}

fn unexpected_task_exit(
    task: &'static str,
    outcome: Result<(), tokio::task::JoinError>,
) -> ExplorerConfigError {
    match outcome {
        Ok(()) => ExplorerConfigError::RuntimeTaskStopped { task },
        Err(source) => ExplorerConfigError::RuntimeTaskJoin { task, source },
    }
}

fn unexpected_operations_exit(outcome: Result<(), OpsServerError>) -> ExplorerConfigError {
    match outcome {
        Ok(()) => ExplorerConfigError::RuntimeTaskStopped {
            task: "operations endpoint",
        },
        Err(error) => ExplorerConfigError::OpsServer(error),
    }
}

fn drain_server(outcome: Result<(), tonic::transport::Error>) {
    if let Err(error) = outcome {
        tracing::warn!(
            target: "zinder::explorer",
            event = "explorer_server_drain_failed",
            error = %error,
            "explorer gRPC server failed while draining after another runtime failure"
        );
    }
}

async fn drain_optional_task(task_name: &'static str, task: &mut Option<JoinHandle<()>>) {
    if let Err(error) = await_optional_task(task_name, task).await {
        tracing::warn!(
            target: "zinder::explorer",
            event = "explorer_runtime_task_drain_failed",
            task = task_name,
            error = %error,
            "explorer runtime task failed while draining after another runtime failure"
        );
    }
}

async fn drain_operations(operations: &mut Option<OpsEndpointHandle>) {
    if let Err(error) = shutdown_operations(operations).await {
        tracing::warn!(
            target: "zinder::explorer",
            event = "explorer_operations_drain_failed",
            error = %error,
            "explorer operations endpoint failed while draining after another runtime failure"
        );
    }
}

fn spawn_wallet_query_health_probe(
    grpc_adapter: &ExplorerQueryGrpcAdapter,
    readiness: ExplorerServingReadiness,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !grpc_adapter.has_wallet_query_dependency() {
        return None;
    }
    let grpc_adapter = grpc_adapter.clone();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(WALLET_QUERY_HEALTH_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let health = tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        outcome = grpc_adapter.check_wallet_query_health() => outcome,
                    };
                    publish_wallet_query_health_outcome(&readiness, &health);
                }
            }
        }
    }))
}

fn publish_wallet_query_health_outcome(
    readiness: &ExplorerServingReadiness,
    outcome: &Result<(), ExplorerWalletQueryHealthError>,
) {
    let health = match outcome {
        Ok(()) => WalletQueryHealthState::Available,
        Err(error) if error.is_contract_mismatch() => WalletQueryHealthState::ContractMismatch,
        Err(_) => WalletQueryHealthState::Unavailable,
    };
    if !readiness.publish_wallet_query_health(health) {
        return;
    }
    match outcome {
        Ok(()) => tracing::info!(
            target: "zinder::explorer",
            event = "wallet_query_health_recovered",
            "admitted wallet query dependency recovered",
        ),
        Err(error) if error.is_contract_mismatch() => tracing::warn!(
            target: "zinder::explorer",
            event = "wallet_query_contract_mismatch",
            error = %error,
            "wallet query dependency no longer matches the admitted contract",
        ),
        Err(error) => tracing::warn!(
            target: "zinder::explorer",
            event = "wallet_query_unavailable",
            error = %error,
            "wallet query dependency is unavailable",
        ),
    }
}

struct UpstreamObservationProbeComposition {
    source: Arc<ZebraJsonRpcSource>,
    poll_interval: Duration,
    json_rpc_addr: String,
}

fn compose_upstream_observation_probe(
    explorer_config: &ExplorerConfig,
) -> Result<Option<UpstreamObservationProbeComposition>, ExplorerConfigError> {
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
    Ok(Some(UpstreamObservationProbeComposition {
        source: Arc::new(source),
        poll_interval,
        json_rpc_addr: node.json_rpc_addr.clone(),
    }))
}

fn spawn_upstream_observation_probe(
    composition: Option<UpstreamObservationProbeComposition>,
    grpc_adapter: &ExplorerQueryGrpcAdapter,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let UpstreamObservationProbeComposition {
        source,
        poll_interval,
        json_rpc_addr,
    } = composition?;
    tracing::info!(
        target: "zinder::explorer",
        event = "upstream_observation_probe_started",
        json_rpc_addr,
        poll_interval_ms = u64::try_from(poll_interval.as_millis()).unwrap_or(u64::MAX),
        "upstream observation probe started",
    );
    Some(grpc_adapter.spawn_upstream_observation_probe(source, poll_interval, cancel))
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

fn open_materialized_view_store(
    explorer_config: &ExplorerConfig,
) -> Result<Option<MaterializedViewStore>, ExplorerConfigError> {
    let materialized_view_path =
        MaterializedViewStore::path_for_canonical(&explorer_config.storage.canonical_root_path);
    let secondary_path = explorer_config
        .storage
        .secondary_root_path
        .join("materialized-views");
    let open_storage_phase = StartupPhase::OpenStorage.start();
    let materialized_view_preset =
        match MaterializedViewStore::detect_materialized_view_preset_at_path(
            &materialized_view_path,
            explorer_config.network,
        ) {
            Ok(Some(materialized_view_preset)) => materialized_view_preset,
            Ok(None) => {
                tracing::info!(
                    target: "zinder::explorer",
                    event = "materialized_view_store_unavailable",
                    "materialized-view store unavailable; materialized-view-backed explorer capabilities disabled"
                );
                open_storage_phase.complete();
                return Ok(None);
            }
            Err(error @ MaterializedViewStoreError::Open { .. }) => {
                tracing::info!(
                    target: "zinder::explorer",
                    event = "materialized_view_store_unavailable",
                    error = %error,
                    "materialized-view store unavailable; materialized-view-backed explorer capabilities disabled"
                );
                open_storage_phase.complete();
                return Ok(None);
            }
            Err(error) => {
                let wrapped = ExplorerConfigError::Store(error);
                open_storage_phase.fail(&wrapped);
                return Err(wrapped);
            }
        };
    match MaterializedViewStore::open_secondary_with_materialized_view_preset(
        &materialized_view_path,
        &secondary_path,
        explorer_config.network,
        materialized_view_preset,
        MaterializedViewStoreOptions {
            sync_writes: false,
            rocksdb_resource_budget: explorer_config.storage.rocksdb_budget,
            ..MaterializedViewStoreOptions::default()
        },
    ) {
        Ok(handle) => {
            open_storage_phase.complete();
            Ok(Some(handle))
        }
        Err(error @ MaterializedViewStoreError::Open { .. }) => {
            tracing::info!(
                target: "zinder::explorer",
                event = "materialized_view_store_unavailable",
                error = %error,
                "materialized-view store unavailable; materialized-view-backed explorer capabilities disabled"
            );
            open_storage_phase.complete();
            Ok(None)
        }
        Err(error) => {
            let wrapped = ExplorerConfigError::Store(error);
            open_storage_phase.fail(&wrapped);
            Err(wrapped)
        }
    }
}

fn spawn_materialized_view_catchup_task(
    store: MaterializedViewStore,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(MATERIALIZED_VIEW_CATCHUP_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = store.try_catch_up() {
                        tracing::warn!(
                            target: "zinder::explorer",
                            event = "materialized_view_secondary_catchup_failed",
                            error = %error,
                            "materialized-view store secondary catchup failed"
                        );
                    }
                }
                () = cancel.cancelled() => break,
            }
        }
    })
}

/// Fetches the node-advertised network-upgrade activation table.
///
/// Feeds the `NetworkUpgradeStatus` and commitment-root handlers. An
/// unconfigured `[node]` section deliberately omits those contracts. Once the
/// operator configures `[node]`, source construction, discovery, and required
/// Sapling evidence are startup admission checks.
async fn fetch_network_upgrade_activations(
    explorer_config: &ExplorerConfig,
) -> Result<Option<Arc<NetworkUpgradeActivations>>, ExplorerConfigError> {
    let Some(node) = explorer_config.node.as_ref() else {
        return Ok(None);
    };
    let source = build_zebra_json_rpc_source(node)?;
    let activations = source.fetch_network_upgrade_activations().await?;
    admit_network_upgrade_activations(activations).map(Some)
}

fn admit_network_upgrade_activations(
    activations: NetworkUpgradeActivations,
) -> Result<Arc<NetworkUpgradeActivations>, ExplorerConfigError> {
    if activations.activation_height_by_name("Sapling").is_none() {
        return Err(ExplorerConfigError::MissingSaplingActivation);
    }
    Ok(Arc::new(activations))
}

async fn build_grpc_adapter(
    explorer_config: &ExplorerConfig,
    materialized_view_store: Option<MaterializedViewStore>,
) -> Result<ExplorerQueryGrpcAdapter, ExplorerConfigError> {
    let network_upgrade_activations = fetch_network_upgrade_activations(explorer_config).await?;
    Ok(ExplorerQueryEndpointComposition {
        metadata: ExplorerEndpointMetadata {
            network: explorer_config.network,
        },
        materialized_view_store,
        network_upgrade_activations,
        wallet_query_endpoint: explorer_config.wallet_query_endpoint.clone(),
        wallet_query_bearer_token: explorer_config.wallet_query_bearer_token.clone(),
        bearer_token: explorer_config.bearer_token.clone(),
    }
    .compose()
    .await?)
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
            wallet_query_bearer_token_path: cli.wallet_query_bearer_token_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_node_admission_requires_sapling_activation() {
        let activations = NetworkUpgradeActivations::empty(zinder_core::Network::ZcashRegtest);

        assert!(matches!(
            admit_network_upgrade_activations(activations),
            Err(ExplorerConfigError::MissingSaplingActivation)
        ));
    }

    #[tokio::test]
    async fn unexpected_background_exit_cancels_server_before_draining() {
        let readiness = Readiness::new(ReadinessState::ready(None));
        let serving_readiness = ExplorerServingReadiness::new(readiness.clone());
        serving_readiness.publish_started();
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server = async move {
            server_cancel.cancelled().await;
            Ok(())
        };

        let outcome = supervise_explorer_runtime(
            server,
            cancel,
            &serving_readiness,
            ExplorerBackgroundTasks {
                operations: None,
                wallet_query_health: None,
                upstream_observation: Some(tokio::spawn(async {})),
                materialized_view_catchup: None,
            },
        )
        .await;

        assert!(matches!(
            outcome,
            Err(ExplorerConfigError::RuntimeTaskStopped {
                task: "upstream-observation probe"
            })
        ));
        assert!(matches!(
            readiness.report().cause,
            ReadinessCause::ShuttingDown
        ));
    }

    #[test]
    fn wallet_query_loss_mismatch_recovery_and_shutdown_are_conjunctive() {
        let runtime = Readiness::default();
        let serving = ExplorerServingReadiness::new(runtime.clone());
        serving.publish_started();
        assert!(matches!(runtime.report().cause, ReadinessCause::Ready));

        let loss = Err(ExplorerWalletQueryHealthError::Request(
            tonic::Status::unavailable("wallet traffic gate is not ready"),
        ));
        publish_wallet_query_health_outcome(&serving, &loss);
        assert!(matches!(
            runtime.report().cause,
            ReadinessCause::WalletQueryUnavailable
        ));
        assert_eq!(
            serving.state.lock().wallet_query_health,
            WalletQueryHealthState::Unavailable
        );

        let mismatch = Err(ExplorerWalletQueryHealthError::ContractChanged);
        publish_wallet_query_health_outcome(&serving, &mismatch);
        assert!(matches!(
            runtime.report().cause,
            ReadinessCause::WalletQueryUnavailable
        ));
        assert_eq!(
            serving.state.lock().wallet_query_health,
            WalletQueryHealthState::ContractMismatch
        );

        publish_wallet_query_health_outcome(&serving, &Ok(()));
        assert!(matches!(runtime.report().cause, ReadinessCause::Ready));

        serving.publish_shutting_down();
        publish_wallet_query_health_outcome(&serving, &Ok(()));
        assert!(matches!(
            runtime.report().cause,
            ReadinessCause::ShuttingDown
        ));
    }

    #[test]
    fn unexpected_operations_exit_is_a_typed_runtime_failure() {
        assert!(matches!(
            unexpected_operations_exit(Ok(())),
            ExplorerConfigError::RuntimeTaskStopped {
                task: "operations endpoint"
            }
        ));
    }
}
