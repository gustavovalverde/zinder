//! Zinder explorer-plane gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_explorer::{
    ExplorerQueryEndpointComposition, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
    MaterializedViewStore, MaterializedViewStoreError, MaterializedViewStoreOptions,
    describe_request_metrics,
};
use zinder_runtime::{
    OpsEndpointHandle, Readiness, ReadinessCause, ReadinessState, RuntimeService, StartupPhase,
    TrafficReadinessInterceptor, cancel_on_terminating_signal, host_cpu_meets_compiled_baseline,
    install_tracing_subscriber, spawn_ops_endpoint_for,
};
use zinder_source::{NodeTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};

mod config;

use config::{ExplorerConfig, ExplorerConfigError, ExplorerConfigOverrides};

/// Cadence the background task uses to advance the secondary's view to the
/// primary's latest durable state.
const MATERIALIZED_VIEW_CATCHUP_INTERVAL: Duration = Duration::from_secs(1);

/// Cadence for rechecking the admitted Wallet endpoint's frozen evidence.
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
    /// `WalletQuery` gRPC endpoint admitted before Explorer accepts traffic.
    #[arg(long = "wallet-query-endpoint")]
    wallet_query_endpoint: Option<String>,
    /// Optional bearer token sent to the admitted `WalletQuery` endpoint.
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
    readiness.set(ReadinessState::starting());
    let start_api_phase = StartupPhase::StartApi.start();

    let materialized_view_store = match open_materialized_view_store(&explorer_config) {
        Ok(materialized_view_store) => materialized_view_store,
        Err(error) => {
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    report_materialized_view_workload(&readiness, &materialized_view_store);

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());

    let grpc_adapter =
        build_grpc_adapter(&explorer_config, materialized_view_store.clone()).await?;
    materialized_view_store.try_catch_up()?;
    let upstream_observation_handle =
        spawn_upstream_observation_probe(&explorer_config, &grpc_adapter, cancel.clone())?;
    let advertised_capabilities = grpc_adapter.advertised_capabilities();

    let ops_handle = spawn_ops_endpoint_for(
        RuntimeService::Explorer,
        explorer_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(explorer_config.network),
        readiness.clone(),
        Arc::from(advertised_capabilities),
    )
    .await?;
    describe_request_metrics();

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    readiness.set(ReadinessState::ready(None));
    let materialized_view_catchup_handle = Some(spawn_materialized_view_catchup_task(
        materialized_view_store,
        readiness.clone(),
        cancel.clone(),
    ));
    let wallet_query_health_handle = Some(spawn_wallet_query_health_probe(
        grpc_adapter.clone(),
        readiness.clone(),
        cancel.clone(),
    ));

    tracing::info!(
        target: "zinder::explorer",
        event = "explorer_started",
        network = encode_zinder_native_chain_name(explorer_config.network),
        listen_addr = %explorer_config.listen_addr,
        storage_path = %explorer_config.storage.path.display(),
        "explorer query gRPC server started"
    );

    let traffic_readiness = TrafficReadinessInterceptor::new(readiness.clone());
    let server_result = tonic::transport::Server::builder()
        .add_service(tonic::service::interceptor::InterceptedService::new(
            grpc_adapter.into_server(),
            traffic_readiness,
        ))
        .serve_with_shutdown(
            explorer_config.listen_addr,
            cancel.clone().cancelled_owned(),
        )
        .await;
    cancel.cancel();

    tracing::info!(
        target: "zinder::explorer",
        event = "explorer_stopped",
        "explorer query gRPC server stopped"
    );

    let background_shutdown_result = shutdown_background_tasks(
        ops_handle,
        upstream_observation_handle,
        materialized_view_catchup_handle,
        wallet_query_health_handle,
    )
    .await;

    match server_result {
        Err(error) => {
            if let Err(ops_error) = background_shutdown_result {
                tracing::warn!(
                    target: "zinder::explorer",
                    event = "ops_endpoint_shutdown_failed",
                    error = %ops_error,
                    "operational endpoint shutdown also failed"
                );
            }
            Err(ExplorerConfigError::Transport(error))
        }
        Ok(()) => background_shutdown_result,
    }
}

fn report_materialized_view_workload(
    readiness: &Readiness,
    materialized_view_store: &MaterializedViewStore,
) {
    let materialized_view_preset = materialized_view_store.effective_materialized_view_preset();
    readiness.set_materialized_view_workload(
        materialized_view_preset.as_str(),
        materialized_view_preset
            .consumer_schemas()
            .iter()
            .map(|schema| schema.name.as_str().to_owned())
            .collect(),
    );
}

async fn shutdown_background_tasks(
    ops_handle: Option<OpsEndpointHandle>,
    upstream_observation_handle: Option<JoinHandle<()>>,
    materialized_view_catchup_handle: Option<JoinHandle<Result<(), MaterializedViewStoreError>>>,
    wallet_query_health_handle: Option<JoinHandle<()>>,
) -> Result<(), ExplorerConfigError> {
    if let Some(handle) = ops_handle {
        handle
            .shutdown()
            .await
            .map_err(ExplorerConfigError::OpsServer)?;
    }
    if let Some(handle) = upstream_observation_handle {
        let _ = handle.await;
    }
    if let Some(handle) = materialized_view_catchup_handle {
        let catchup_outcome = handle
            .await
            .map_err(ExplorerConfigError::MaterializedViewCatchupTask)?;
        catchup_outcome?;
    }
    if let Some(handle) = wallet_query_health_handle {
        let _ = handle.await;
    }
    Ok(())
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

fn open_materialized_view_store(
    explorer_config: &ExplorerConfig,
) -> Result<MaterializedViewStore, ExplorerConfigError> {
    let materialized_view_path =
        MaterializedViewStore::path_for_canonical(&explorer_config.storage.path);
    let secondary_path = explorer_config
        .storage
        .secondary_path
        .join("materialized-views");
    let open_storage_phase = StartupPhase::OpenStorage.start();
    let materialized_view_preset =
        match MaterializedViewStore::detect_materialized_view_preset_at_path(
            &materialized_view_path,
        ) {
            Ok(Some(materialized_view_preset)) => materialized_view_preset,
            Ok(None) => {
                let error = ExplorerConfigError::RequiredMaterializedViewStore;
                open_storage_phase.fail(&error);
                return Err(error);
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
        materialized_view_preset,
        MaterializedViewStoreOptions {
            sync_writes: false,
            rocksdb_resource_budget: explorer_config.storage.materialized_view_rocksdb_budget,
            ..MaterializedViewStoreOptions::default()
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

fn spawn_materialized_view_catchup_task(
    store: MaterializedViewStore,
    readiness: Readiness,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), MaterializedViewStoreError>> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(MATERIALIZED_VIEW_CATCHUP_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = store.try_catch_up() {
                        return Err(close_readiness_after_materialized_view_catchup_failure(
                            &readiness,
                            &cancel,
                            error,
                        ));
                    }
                }
                () = cancel.cancelled() => return Ok(()),
            }
        }
    })
}

fn close_readiness_after_materialized_view_catchup_failure(
    readiness: &Readiness,
    cancel: &CancellationToken,
    error: MaterializedViewStoreError,
) -> MaterializedViewStoreError {
    readiness.set(ReadinessState::not_ready(
        ReadinessCause::StorageUnavailable,
    ));
    cancel.cancel();
    tracing::error!(
        target: "zinder::explorer",
        event = "materialized_view_secondary_catchup_failed",
        error = %error,
        "materialized-view store secondary catchup failed; Explorer readiness closed"
    );
    error
}

fn spawn_wallet_query_health_probe(
    grpc_adapter: ExplorerQueryGrpcAdapter,
    readiness: Readiness,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WALLET_QUERY_HEALTH_POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let outcome = grpc_adapter.check_wallet_endpoint_health().await;
                    if cancel.is_cancelled() {
                        return;
                    }
                    match outcome {
                        Ok(()) => readiness.set(ReadinessState::ready(None)),
                        Err(error) if error.is_contract_mismatch() => {
                            readiness.set(ReadinessState::not_ready(ReadinessCause::SchemaMismatch));
                            tracing::warn!(
                                target: "zinder::explorer",
                                event = "wallet_query_contract_changed",
                                error = %error,
                                "admitted Wallet evidence changed; Explorer readiness closed"
                            );
                        }
                        Err(error) => {
                            readiness.set(ReadinessState::not_ready(ReadinessCause::StorageUnavailable));
                            tracing::warn!(
                                target: "zinder::explorer",
                                event = "wallet_query_health_failed",
                                error = %error,
                                "admitted Wallet health check failed; Explorer readiness closed"
                            );
                        }
                    }
                }
                () = cancel.cancelled() => return,
            }
        }
    })
}

async fn build_grpc_adapter(
    explorer_config: &ExplorerConfig,
    materialized_view_store: MaterializedViewStore,
) -> Result<ExplorerQueryGrpcAdapter, ExplorerConfigError> {
    let server_info = ExplorerServerInfoSettings {
        network: explorer_config.network,
    };
    let mut composition = ExplorerQueryEndpointComposition::new(
        server_info,
        materialized_view_store,
        explorer_config.wallet_query_endpoint.clone(),
    )
    .with_prevout_resolution_online(true);
    if let Some(token) = explorer_config.wallet_query_bearer_token.clone() {
        composition = composition.with_wallet_query_bearer_token(token);
    }
    if let Some(token) = explorer_config.bearer_token.clone() {
        composition = composition.with_bearer_token(token);
    }
    composition
        .compose()
        .await
        .map_err(ExplorerConfigError::EndpointAdmission)
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
    use tonic::service::Interceptor as _;

    #[test]
    fn catchup_failure_closes_readiness_and_cancels_before_returning() {
        let readiness = Readiness::new(ReadinessState::ready(None));
        let cancel = CancellationToken::new();
        let error = MaterializedViewStoreError::InvalidOptions {
            reason: "injected terminal catch-up failure",
        };

        let returned =
            close_readiness_after_materialized_view_catchup_failure(&readiness, &cancel, error);

        let report = readiness.report();
        assert!(!report.is_ready);
        assert!(matches!(report.cause, ReadinessCause::StorageUnavailable));
        assert!(cancel.is_cancelled());
        assert!(matches!(
            returned,
            MaterializedViewStoreError::InvalidOptions { .. }
        ));

        let mut traffic_gate = TrafficReadinessInterceptor::new(readiness);
        assert!(traffic_gate.call(tonic::Request::new(())).is_err());
    }
}
