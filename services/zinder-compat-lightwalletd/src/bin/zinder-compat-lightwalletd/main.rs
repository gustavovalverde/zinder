//! Zinder lightwalletd-compatible gRPC server entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc};
use zinder_core::wire::encode_zinder_native_chain_name;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use zinder_compat_lightwalletd::{
    IngestControlMempoolSurface, LightwalletdGrpcAdapter, spawn_ingest_control_tip_change_publisher,
};
use zinder_runtime::{
    OpsServerError, Readiness, RuntimeService, StartupPhase, TrafficReadinessInterceptor,
    cancel_on_terminating_signal, install_metrics_recorder_for_service, install_tracing_subscriber,
    spawn_ops_endpoint_for,
};
use zinder_source::{
    DEFAULT_NODE_HEALTH_POLL_INTERVAL_MS, NodeTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};

mod config;
use config::{LightwalletdConfigError, LightwalletdConfigOverrides};
use zinder_query::{
    WalletQueryApi, WalletServingPairConfig, WalletServingPairPublisher, WalletServingReadiness,
    spawn_wallet_node_readiness_probe,
};

#[derive(Parser)]
#[command(name = "zinder-compat-lightwalletd")]
#[command(about = "Zinder lightwalletd-compatible gRPC server")]
#[command(version)]
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
    /// Root containing this process's two canonical secondary generations.
    #[arg(long = "canonical-secondary-root")]
    canonical_secondary_root: Option<PathBuf>,
    /// Wallet primary path replicated only through an immutable secondary.
    #[arg(long = "wallet-primary-path")]
    wallet_primary_path: Option<PathBuf>,
    /// Root containing this process's two wallet secondary generations.
    #[arg(long = "wallet-secondary-root")]
    wallet_secondary_root: Option<PathBuf>,
    /// Private `zinder-ingest` control gRPC endpoint.
    #[arg(long = "ingest-control-addr")]
    ingest_control_addr: Option<String>,
    /// Path to a file containing the shared-secret bearer token used by the
    /// `IngestControl` writer. Required when the writer enforces auth.
    #[arg(long = "ingest-control-token-path")]
    ingest_control_token_path: Option<PathBuf>,
    /// Lightwalletd-compatible gRPC listen address, such as 127.0.0.1:9067.
    #[arg(long = "listen-addr")]
    listen_addr: Option<SocketAddr>,
    /// Exact canonical replacement-depth identity expected from the writer.
    #[arg(long = "reorg-window-blocks")]
    reorg_window_blocks: Option<u32>,
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

#[allow(
    clippy::too_many_lines,
    reason = "lightwalletd bootstrap composes the readiness, ops, store, broadcaster, mempool surface, tip-change publisher, secondary-catchup, and gRPC-server subsystems; the linear sequence is the operator-facing flow and intentionally lives in one function so failure ordering is auditable."
)]
async fn run_lightwalletd(cli: Cli) -> Result<(), LightwalletdConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let config_path = cli.config_path.clone();
    let lightwalletd_config = match config::load_lightwalletd_config(config_path, cli.into()) {
        Ok(cfg) => {
            load_config_phase.complete();
            cfg
        }
        Err(error) => {
            load_config_phase.fail(&error);
            return Err(error);
        }
    };
    if lightwalletd_config.ops_listen_addr.is_some() {
        install_metrics_recorder_for_service(
            RuntimeService::CompatLightwalletd,
            env!("CARGO_PKG_VERSION"),
            encode_zinder_native_chain_name(lightwalletd_config.network),
        )
        .map_err(OpsServerError::from)?;
    }
    let readiness = Readiness::default();
    let serving_readiness = WalletServingReadiness::awaiting_node_source(readiness.clone());
    let start_api_phase = StartupPhase::StartApi.start();

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let broadcaster = match build_broadcaster(lightwalletd_config.broadcaster.as_ref()) {
        Ok(broadcaster) => broadcaster,
        Err(error) => {
            connect_node_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };
    let Some(broadcaster_source) = broadcaster.as_ref() else {
        let wrapped = LightwalletdConfigError::Source(Box::new(
            zinder_source::SourceError::SourceProtocolMismatch {
                reason: "[node] section is required so GetLightdInfo can serve a \
                         node-discovered consensus branch id",
            },
        ));
        connect_node_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    };
    broadcaster_source
        .probe_capabilities()
        .await
        .map_err(|source_error| LightwalletdConfigError::Source(Box::new(source_error)))?;
    let network_upgrade_activations = match broadcaster_source
        .discover_network_upgrade_activations("zinder-compat-lightwalletd")
        .await
    {
        Ok(activations) => {
            connect_node_phase.complete();
            activations
        }
        Err(source_error) => {
            let wrapped = LightwalletdConfigError::Source(Box::new(source_error));
            connect_node_phase.fail(&wrapped);
            start_api_phase.fail(&wrapped);
            return Err(wrapped);
        }
    };
    let open_storage_phase = StartupPhase::OpenStorage.start();
    let (serving_pair_publisher, serving_pair_slot) = WalletServingPairPublisher::bootstrap(
        WalletServingPairConfig {
            canonical_primary_path: lightwalletd_config.storage.path.clone(),
            canonical_secondary_root: lightwalletd_config.storage.secondary_path.clone(),
            wallet_primary_path: lightwalletd_config.wallet_primary_path.clone(),
            wallet_secondary_root: lightwalletd_config.wallet_secondary_root.clone(),
            network: lightwalletd_config.network,
            network_upgrade_activations: Arc::clone(&network_upgrade_activations),
            expected_raw_blob_retention: lightwalletd_config.storage.expected_raw_blob_retention,
            canonical_reorg_policy: lightwalletd_config.canonical_reorg_policy,
            canonical_resource_budget: lightwalletd_config.storage.canonical_rocksdb_budget,
            wallet_resource_budget: lightwalletd_config.wallet_rocksdb_budget,
            catchup_interval: lightwalletd_config.storage.secondary_catchup_interval,
            convergence_timeout: lightwalletd_config.storage.initial_catchup_timeout,
            convergence_attempts: lightwalletd_config.pair_convergence_attempts,
            replica_lag_threshold_chain_epochs: lightwalletd_config
                .storage
                .secondary_replica_lag_threshold_chain_epochs,
        },
        serving_readiness.clone(),
        &lightwalletd_config.ingest_control_addr,
        lightwalletd_config.ingest_control_bearer_token.as_ref(),
    )
    .await?;
    let visible_height = Some(
        serving_pair_slot
            .capture()
            .canonical_fence()
            .visible_tip()
            .height
            .value(),
    );
    open_storage_phase.complete();

    let wallet_query = zinder_query::WalletServingQuery::from_probed_node_source(
        serving_pair_slot.clone(),
        broadcaster_source.clone(),
        network_upgrade_activations.clone(),
    )?;
    let native_endpoint_capabilities = wallet_query.native_endpoint_capabilities().clone();
    let ops_handle = spawn_ops_endpoint_for(
        RuntimeService::CompatLightwalletd,
        lightwalletd_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(lightwalletd_config.network),
        readiness.clone(),
        Arc::from([]),
    )
    .await?;
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let node_readiness_handle = spawn_wallet_node_readiness_probe(
        broadcaster_source.clone(),
        &native_endpoint_capabilities,
        serving_readiness.clone(),
        lightwalletd_config
            .broadcaster
            .as_ref()
            .and_then(|target| target.health.as_ref())
            .map_or_else(
                || std::time::Duration::from_millis(DEFAULT_NODE_HEALTH_POLL_INTERVAL_MS),
                |health| health.poll_interval,
            ),
        cancel.clone(),
    )?;
    let serving_pair_publisher_handle = serving_pair_publisher.spawn(cancel.clone());
    let mempool_surface = Arc::new({
        let mut surface =
            IngestControlMempoolSurface::new(lightwalletd_config.ingest_control_addr.clone());
        if let Some(token) = lightwalletd_config.ingest_control_bearer_token.clone() {
            surface = surface.with_bearer_token(token);
        }
        surface
    });
    let (tip_change_watcher, tip_publisher_handle) = spawn_ingest_control_tip_change_publisher(
        lightwalletd_config.ingest_control_addr.clone(),
        lightwalletd_config.ingest_control_bearer_token.clone(),
        cancel.clone(),
    );
    let grpc_adapter = LightwalletdGrpcAdapter::new(wallet_query, network_upgrade_activations)
        .with_serving_pair_slot(serving_pair_slot)
        .with_transparent_address_support()
        .with_mempool_surface(mempool_surface)
        .with_tip_change_watcher(tip_change_watcher);

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "compat_started",
        network = encode_zinder_native_chain_name(lightwalletd_config.network),
        listen_addr = %lightwalletd_config.listen_addr,
        visible_height = ?visible_height,
        "lightwalletd-compatible gRPC server started"
    );

    // Expose `grpc.reflection.v1.ServerReflection` so legacy lightwalletd
    // wallets and `grpcurl` can discover the served `CompactTxStreamer`
    // surface without an out-of-band proto.
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(zinder_proto::LIGHTWALLETD_COMPAT_FILE_DESCRIPTOR_SET)
        .build_v1()?;

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();

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
    let server_result = tonic::transport::Server::builder()
        .add_service(grpc_service)
        .add_service(reflection_service)
        .serve_with_shutdown(
            lightwalletd_config.listen_addr,
            cancel.clone().cancelled_owned(),
        )
        .await;

    serving_readiness.publish_shutting_down();
    cancel.cancel();

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "compat_stopped",
        "lightwalletd-compatible gRPC server stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await?;
    }

    // Drain the tip-change publisher so a panic in its task surfaces in
    // the operator's log instead of vanishing silently. The publisher's
    // run loop exits when `cancel` fires, so the join completes promptly.
    match tip_publisher_handle.await {
        Ok(()) => {}
        Err(join_error) if join_error.is_panic() => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "tip_change_publisher_panic",
            error = %join_error,
            "tip-change publisher task panicked",
        ),
        Err(join_error) => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "tip_change_publisher_join_failed",
            error = %join_error,
            "tip-change publisher task did not exit cleanly",
        ),
    }

    match serving_pair_publisher_handle.await {
        Ok(()) => {}
        Err(join_error) if join_error.is_panic() => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "serving_pair_publisher_panic",
            error = %join_error,
            "wallet-serving pair publisher task panicked"
        ),
        Err(join_error) => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "serving_pair_publisher_join_failed",
            error = %join_error,
            "wallet-serving pair publisher task did not exit cleanly"
        ),
    }

    match node_readiness_handle.await {
        Ok(()) => {}
        Err(join_error) if join_error.is_panic() => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "wallet_node_readiness_probe_panic",
            error = %join_error,
            "wallet node-readiness probe task panicked"
        ),
        Err(join_error) => tracing::warn!(
            target: "zinder::compat_lightwalletd",
            event = "wallet_node_readiness_probe_join_failed",
            error = %join_error,
            "wallet node-readiness probe task did not exit cleanly"
        ),
    }

    server_result.map_err(LightwalletdConfigError::Transport)
}

fn build_broadcaster(
    broadcaster_target: Option<&NodeTarget>,
) -> Result<Option<ZebraJsonRpcSource>, LightwalletdConfigError> {
    let Some(broadcaster_target) = broadcaster_target else {
        tracing::info!(
            target: "zinder::compat_lightwalletd",
            event = "transaction_broadcast_disabled",
            "transaction broadcast disabled because [node] is not configured"
        );
        return Ok(None);
    };

    let source = ZebraJsonRpcSource::with_options(
        broadcaster_target.network,
        broadcaster_target.json_rpc_addr.clone(),
        broadcaster_target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: broadcaster_target.request_timeout,
            max_response_bytes: broadcaster_target.max_response_bytes,
            broadcast_timeout: broadcaster_target.broadcast_timeout,
        },
    )
    .map_err(|source| LightwalletdConfigError::Source(Box::new(source)))?
    .with_health_config(broadcaster_target.health.clone());

    tracing::info!(
        target: "zinder::compat_lightwalletd",
        event = "transaction_broadcast_enabled",
        json_rpc_addr = %broadcaster_target.json_rpc_addr,
        "transaction broadcast enabled via Zebra JSON-RPC"
    );
    Ok(Some(source))
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
