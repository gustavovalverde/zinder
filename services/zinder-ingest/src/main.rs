//! Zinder ingestion command-line entry point.

use std::{
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::config::{
    BackfillConfigOverrides, BackfillCoverage, BackupConfigOverrides, IngestConfigError,
    TipFollowConfigOverrides,
};
use clap::{Parser, Subcommand};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_ingest::{
    BackfillOutcome, IngestControlGrpcAdapter, IngestError, NodeSourceKind, backfill,
    open_tip_follow_store, spawn_chain_event_retention_task, tip_follow_with_primary_store,
};
use zinder_runtime::{
    OpsEndpointHandle, OpsServer, Readiness, ReadinessState, cancel_on_ctrl_c,
    install_tracing_subscriber, spawn_ops_endpoint,
};
use zinder_source::{NodeTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};

mod cli;
mod config;

#[derive(Parser)]
#[command(name = "zinder-ingest")]
#[command(about = "Zinder canonical chain ingestion")]
struct Cli {
    /// TOML configuration file loaded before environment variables and CLI overrides.
    #[arg(long = "config", global = true)]
    config_path: Option<PathBuf>,
    /// Print the resolved command configuration without opening storage or connecting.
    #[arg(long = "print-config", global = true)]
    print_config: bool,
    /// Operational HTTP endpoint listen address for /healthz, /readyz, /metrics.
    #[arg(long = "ops-listen-addr", global = true)]
    ops_listen_addr: Option<SocketAddr>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Backfill a historical block height range that is already outside the reorg window.
    Backfill(BackfillArgs),
    /// Follow the upstream node tip and commit live chain changes.
    TipFollow(TipFollowArgs),
    /// Create a point-in-time `RocksDB` checkpoint of the canonical store.
    Backup(BackupArgs),
}

#[derive(Parser)]
struct BackfillArgs {
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Upstream node source, currently zebra-json-rpc.
    #[arg(long = "node-source")]
    node_source: Option<String>,
    /// Zebra JSON-RPC address.
    #[arg(long = "json-rpc-addr")]
    json_rpc_addr: Option<String>,
    /// Node auth method, such as none, basic, or cookie.
    #[arg(long = "node-auth-method")]
    node_auth_method: Option<String>,
    /// Node auth username when the method is basic.
    #[arg(long = "node-auth-username")]
    node_auth_username: Option<String>,
    /// Node auth cookie path when the method is cookie.
    #[arg(long = "node-auth-path")]
    node_auth_path: Option<PathBuf>,
    /// Canonical Zinder store path.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// First block height to backfill.
    #[arg(long = "from-height")]
    from_height: Option<u32>,
    /// Last block height to backfill.
    #[arg(long = "to-height")]
    to_height: Option<u32>,
    /// Node request timeout in seconds.
    #[arg(long = "request-timeout-secs")]
    request_timeout_secs: Option<u64>,
    /// Maximum JSON-RPC response body size in bytes.
    #[arg(long = "max-response-bytes")]
    max_response_bytes: Option<u64>,
    /// Maximum number of blocks committed in one chain epoch.
    #[arg(long = "commit-batch-blocks")]
    commit_batch_blocks: Option<u32>,
    /// Allow finalizing blocks inside the upstream node's current reorg window.
    #[arg(long = "allow-near-tip-finalize", action = clap::ArgAction::SetTrue)]
    allow_near_tip_finalize: bool,
    /// Bootstrap an empty store from the upstream node's chain state at this
    /// height (`from_height` must equal `checkpoint_height + 1`). Reads at
    /// heights below the checkpoint return `ArtifactUnavailable`.
    #[arg(long = "checkpoint-height")]
    checkpoint_height: Option<u32>,
    /// Derive the backfill floor needed by lightwalletd-compatible wallets
    /// from node-advertised activation heights.
    #[arg(long = "wallet-serving", action = clap::ArgAction::SetTrue)]
    wallet_serving: bool,
}

#[derive(Parser)]
struct TipFollowArgs {
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Upstream node source, currently zebra-json-rpc.
    #[arg(long = "node-source")]
    node_source: Option<String>,
    /// Zebra JSON-RPC address.
    #[arg(long = "json-rpc-addr")]
    json_rpc_addr: Option<String>,
    /// Node auth method, such as none, basic, or cookie.
    #[arg(long = "node-auth-method")]
    node_auth_method: Option<String>,
    /// Node auth username when the method is basic.
    #[arg(long = "node-auth-username")]
    node_auth_username: Option<String>,
    /// Node auth cookie path when the method is cookie.
    #[arg(long = "node-auth-path")]
    node_auth_path: Option<PathBuf>,
    /// Canonical Zinder store path.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Node request timeout in seconds.
    #[arg(long = "request-timeout-secs")]
    request_timeout_secs: Option<u64>,
    /// Maximum JSON-RPC response body size in bytes.
    #[arg(long = "max-response-bytes")]
    max_response_bytes: Option<u64>,
    /// Number of near-tip blocks that may be replaced by a reorg.
    #[arg(long = "reorg-window-blocks")]
    reorg_window_blocks: Option<u32>,
    /// Maximum number of blocks committed in one chain epoch.
    #[arg(long = "commit-batch-blocks")]
    commit_batch_blocks: Option<u32>,
    /// Delay between upstream node tip polls, in milliseconds.
    #[arg(long = "poll-interval-ms")]
    poll_interval_ms: Option<u64>,
    /// Lag threshold (in blocks) below which tip-follow reports `Ready`.
    #[arg(long = "lag-threshold-blocks")]
    lag_threshold_blocks: Option<u64>,
    /// Private ingest-control gRPC listen address used by secondary readers and subscribers.
    #[arg(long = "ingest-control-listen-addr")]
    ingest_control_listen_addr: Option<SocketAddr>,
}

#[derive(Parser)]
struct BackupArgs {
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Canonical Zinder store path.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Destination directory for the `RocksDB` checkpoint.
    #[arg(long = "to")]
    to_path: Option<PathBuf>,
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
    let render_result = match cli.command {
        Command::Backfill(args) => print_backfill_config(cli.config_path, args),
        Command::TipFollow(args) => print_tip_follow_config(cli.config_path, args),
        Command::Backup(args) => print_backup_config(cli.config_path, args),
    };

    match render_result {
        Ok(rendered_toml) => {
            println!("{rendered_toml}");
            ExitCode::SUCCESS
        }
        Err(error) => emit_runtime_error(&error),
    }
}

async fn run_runtime(cli: Cli) -> ExitCode {
    let ops_listen_addr = cli.ops_listen_addr;
    let runtime_result = match cli.command {
        Command::Backfill(args) => run_backfill(cli.config_path, args, ops_listen_addr).await,
        Command::TipFollow(args) => run_tip_follow(cli.config_path, args, ops_listen_addr).await,
        Command::Backup(args) => run_backup(cli.config_path, args),
    };

    match runtime_result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => emit_runtime_error(&error),
    }
}

fn emit_runtime_error(error: &IngestConfigError) -> ExitCode {
    tracing::error!(
        target: "zinder::ingest",
        event = "ingest_run_failed",
        error = %error,
        "ingest run failed"
    );
    ExitCode::FAILURE
}

async fn run_backfill(
    config_path: Option<PathBuf>,
    args: BackfillArgs,
    ops_listen_addr: Option<SocketAddr>,
) -> Result<(), IngestConfigError> {
    let mut command_config = config::load_backfill_config(config_path, args.into())?;
    let readiness = Readiness::default();
    let ops_handle = ops_listen_addr.map(|listen_addr| {
        spawn_ingest_ops(listen_addr, command_config.node.network.name(), &readiness)
    });
    let source =
        zebra_json_rpc_source_for_target(command_config.node_source, &command_config.node)?;
    probe_node_capabilities(&source).await;
    resolve_backfill_coverage(&mut command_config, &source).await?;
    let checkpoint = if let Some(checkpoint_height) = command_config.checkpoint_height {
        let checkpoint = source
            .fetch_chain_checkpoint(checkpoint_height)
            .await
            .map_err(IngestError::from)?;
        tracing::info!(
            target: "zinder::ingest",
            event = "backfill_checkpoint_resolved",
            checkpoint_height = checkpoint.height.value(),
            sapling_commitment_tree_size =
                checkpoint.tip_metadata.sapling_commitment_tree_size,
            orchard_commitment_tree_size =
                checkpoint.tip_metadata.orchard_commitment_tree_size,
            "fetched bootstrap checkpoint from upstream node"
        );
        Some(checkpoint)
    } else {
        None
    };
    let backfill_config = command_config.resolved_backfill_config(checkpoint)?;
    readiness.set(ReadinessState::syncing(None, None, None));
    let backfill_outcome = backfill(&backfill_config, &source).await?;

    let chain_epoch = backfill_outcome.chain_epoch();
    readiness.set(ReadinessState::ready(Some(chain_epoch.tip_height.value())));

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "non-exhaustive library backfill outcomes must surface conservatively"
    )]
    match backfill_outcome {
        BackfillOutcome::Committed(_) => {
            // record_commit_outcome already emitted chain_committed for the final batch.
        }
        BackfillOutcome::AlreadyComplete { chain_epoch } => {
            tracing::info!(
                target: "zinder::ingest",
                event = "backfill_already_complete",
                chain_epoch_id = chain_epoch.id.value(),
                tip_height = chain_epoch.tip_height.value(),
                "backfill range already covered by the visible chain epoch"
            );
        }
        _ => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "backfill_outcome_unrecognized",
                "backfill outcome variant is not handled by this binary"
            );
        }
    }

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }

    Ok(())
}

async fn resolve_backfill_coverage(
    command_config: &mut config::BackfillCommandConfig,
    source: &ZebraJsonRpcSource,
) -> Result<(), IngestConfigError> {
    if !matches!(command_config.coverage, BackfillCoverage::WalletServing) {
        return Ok(());
    }

    let activation_heights = source
        .fetch_network_upgrade_activation_heights()
        .await
        .map_err(IngestError::from)?;
    let Some(wallet_serving_floor) = activation_heights.wallet_serving_floor() else {
        return Err(
            IngestError::from(zinder_source::SourceError::SourceProtocolMismatch {
                reason: "getblockchaininfo did not advertise Sapling or NU5 activation heights",
            })
            .into(),
        );
    };
    if wallet_serving_floor == BlockHeight::new(0) {
        return Err(
            IngestError::from(zinder_source::SourceError::SourceProtocolMismatch {
                reason: "wallet-serving backfill floor cannot be the genesis block",
            })
            .into(),
        );
    }

    let checkpoint_height = BlockHeight::new(wallet_serving_floor.value().saturating_sub(1));
    command_config.from_height = Some(wallet_serving_floor);
    command_config.checkpoint_height = Some(checkpoint_height);
    tracing::info!(
        target: "zinder::ingest",
        event = "wallet_serving_backfill_floor_resolved",
        from_height = wallet_serving_floor.value(),
        checkpoint_height = checkpoint_height.value(),
        sapling_activation_height = activation_heights
            .sapling
            .map(zinder_core::BlockHeight::value),
        nu5_activation_height = activation_heights.nu5.map(zinder_core::BlockHeight::value),
        "resolved wallet-serving backfill floor from node activation heights"
    );

    Ok(())
}

async fn probe_node_capabilities(source: &ZebraJsonRpcSource) {
    match source.probe_capabilities().await {
        Ok(probed_capabilities) => {
            let advertised: Vec<&'static str> = probed_capabilities
                .iter()
                .map(zinder_source::NodeCapability::name)
                .collect();
            tracing::info!(
                target: "zinder::ingest",
                event = "node_capabilities_probed",
                advertised = ?advertised,
                "node advertised capabilities discovered via rpc.discover"
            );
        }
        Err(probe_error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "node_capability_probe_failed",
                error = %probe_error,
                "node capability probe failed; continuing with baseline capabilities"
            );
        }
    }
}

fn zebra_json_rpc_source_for_target(
    node_source: NodeSourceKind,
    target: &NodeTarget,
) -> Result<ZebraJsonRpcSource, IngestConfigError> {
    match node_source {
        NodeSourceKind::ZebraJsonRpc => ZebraJsonRpcSource::with_options(
            target.network,
            &target.json_rpc_addr,
            target.node_auth.clone(),
            ZebraJsonRpcSourceOptions {
                request_timeout: target.request_timeout,
                max_response_bytes: target.max_response_bytes,
            },
        )
        .map_err(IngestError::from)
        .map_err(IngestConfigError::from),
    }
}

async fn run_tip_follow(
    config_path: Option<PathBuf>,
    args: TipFollowArgs,
    ops_listen_addr: Option<SocketAddr>,
) -> Result<(), IngestConfigError> {
    let command_config = config::load_tip_follow_config(config_path, args.into())?;
    let tip_follow_config = command_config.tip_follow;
    let readiness = Readiness::default();
    let ops_handle = ops_listen_addr.map(|listen_addr| {
        spawn_ingest_ops(
            listen_addr,
            tip_follow_config.node.network.name(),
            &readiness,
        )
    });
    let store = open_tip_follow_store(&tip_follow_config)?;
    let source =
        zebra_json_rpc_source_for_target(tip_follow_config.node_source, &tip_follow_config.node)?;
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    let ingest_control_handle = spawn_ingest_control_endpoint(
        command_config.ingest_control_listen_addr,
        tip_follow_config.node.network,
        store.clone(),
        cancel.clone(),
    )
    .await?;
    let _retention_handle = spawn_chain_event_retention_task(
        store.clone(),
        readiness.clone(),
        command_config.chain_event_retention,
        cancel.clone(),
    );

    tracing::info!(
        target: "zinder::ingest",
        event = "tip_follow_started",
        network = tip_follow_config.node.network.name(),
        json_rpc_addr = tip_follow_config.node.json_rpc_addr.as_str(),
        reorg_window_blocks = tip_follow_config.reorg_window_blocks,
        lag_threshold_blocks = tip_follow_config.lag_threshold_blocks,
        poll_interval_ms = u64::try_from(tip_follow_config.poll_interval.as_millis())
            .unwrap_or(u64::MAX),
        ingest_control_listen_addr = %command_config.ingest_control_listen_addr,
        chain_event_retention_hours = command_config
            .chain_event_retention
            .retention_window
            .map_or(0, |duration| duration.as_secs() / 3_600),
        chain_event_retention_check_interval_ms = u64::try_from(command_config
            .chain_event_retention
            .check_interval
            .as_millis())
            .unwrap_or(u64::MAX),
        cursor_at_risk_warning_hours = command_config
            .chain_event_retention
            .cursor_at_risk_warning
            .as_secs() / 3_600,
        "tip-follow started"
    );

    probe_node_capabilities(&source).await;
    readiness.set(ReadinessState::syncing(None, None, None));
    let tip_follow_outcome = tip_follow_with_primary_store(
        &tip_follow_config,
        &source,
        store,
        &readiness,
        cancel.clone(),
    )
    .await;
    let tip_follow_result =
        handle_tip_follow_outcome(tip_follow_outcome, &readiness, &cancel).await;

    tracing::info!(
        target: "zinder::ingest",
        event = "tip_follow_stopped",
        "tip-follow stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }
    ingest_control_handle.shutdown().await;

    tip_follow_result
}

async fn handle_tip_follow_outcome(
    outcome: Result<(), IngestError>,
    readiness: &Readiness,
    cancel: &CancellationToken,
) -> Result<(), IngestConfigError> {
    match outcome {
        Ok(()) => Ok(()),
        Err(IngestError::ReorgWindowExceeded {
            from_height,
            replacement_depth,
            configured_window_blocks,
        }) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "tip_follow_reorg_window_exceeded",
                from_height = from_height.value(),
                replacement_depth,
                configured_window_blocks,
                "tip-follow reorg replacement crossed the configured non-finalized window; readiness drained for operator review"
            );
            readiness.set(ReadinessState::reorg_window_exceeded(
                u64::from(replacement_depth),
                u64::from(configured_window_blocks),
                Some(from_height.value().saturating_sub(1)),
            ));
            cancel.cancelled().await;
            Ok(())
        }
        Err(error) => Err(IngestConfigError::from(error)),
    }
}

struct IngestControlEndpointHandle {
    cancel: CancellationToken,
    join: JoinHandle<Result<(), IngestConfigError>>,
}

impl IngestControlEndpointHandle {
    async fn shutdown(self) {
        self.cancel.cancel();
        match self.join.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                target: "zinder::ingest",
                event = "ingest_control_endpoint_error",
                error = %error,
                "ingest-control endpoint exited with error"
            ),
            Err(join_error) => tracing::warn!(
                target: "zinder::ingest",
                event = "ingest_control_endpoint_panic",
                error = %join_error,
                "ingest-control endpoint task failed"
            ),
        }
    }
}

async fn spawn_ingest_control_endpoint(
    listen_addr: SocketAddr,
    network: zinder_core::Network,
    store: PrimaryChainStore,
    cancel: CancellationToken,
) -> Result<IngestControlEndpointHandle, IngestConfigError> {
    let listener = TcpListener::bind(listen_addr).await.map_err(|source| {
        IngestConfigError::IngestControlBind {
            listen_addr,
            source,
        }
    })?;
    let incoming = TcpListenerStream::new(listener);
    let endpoint_cancel = CancellationToken::new();
    let endpoint_cancel_for_task = endpoint_cancel.clone();
    let shutdown_cancel = cancel.clone();
    let adapter = IngestControlGrpcAdapter::new(network, store);
    let join = tokio::spawn(async move {
        tracing::info!(
            target: "zinder::ingest",
            event = "ingest_control_endpoint_started",
            listen_addr = %listen_addr,
            "ingest-control endpoint started"
        );
        let serve_result = tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(incoming, async move {
                tokio::select! {
                    () = shutdown_cancel.cancelled() => {}
                    () = endpoint_cancel_for_task.cancelled() => {}
                }
            })
            .await;
        tracing::info!(
            target: "zinder::ingest",
            event = "ingest_control_endpoint_stopped",
            "ingest-control endpoint stopped"
        );
        serve_result.map_err(|source| IngestConfigError::IngestControlTransport { source })
    });

    Ok(IngestControlEndpointHandle {
        cancel: endpoint_cancel,
        join,
    })
}

fn run_backup(config_path: Option<PathBuf>, args: BackupArgs) -> Result<(), IngestConfigError> {
    let backup_config = config::load_backup_config(config_path, args.into())?;
    let store = PrimaryChainStore::open(
        &backup_config.storage_path,
        ChainStoreOptions::for_network(backup_config.network),
    )
    .map_err(IngestError::from)?;
    let started_at = Instant::now();
    let backup_outcome = store
        .create_checkpoint(&backup_config.to_path)
        .map_err(IngestError::from)
        .map_err(IngestConfigError::from);
    record_backup_outcome(backup_config.network, started_at, &backup_outcome);
    backup_outcome?;

    tracing::info!(
        target: "zinder::ingest",
        event = "backup_created",
        network = backup_config.network.name(),
        storage_path = %backup_config.storage_path.display(),
        checkpoint_path = %backup_config.to_path.display(),
        "backup checkpoint created"
    );

    Ok(())
}

fn record_backup_outcome(
    network: zinder_core::Network,
    started_at: Instant,
    backup_outcome: &Result<(), IngestConfigError>,
) {
    metrics::histogram!(
        "zinder_ingest_backup_duration_seconds",
        "network" => network.name(),
        "status" => outcome_status(backup_outcome),
        "error_class" => ingest_config_error_class(backup_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_backup_total",
        "network" => network.name(),
        "status" => outcome_status(backup_outcome),
        "error_class" => ingest_config_error_class(backup_outcome.as_ref().err())
    )
    .increment(1);
    if backup_outcome.is_ok() {
        metrics::gauge!(
            "zinder_ingest_backup_last_success_unix_seconds",
            "network" => network.name()
        )
        .set(current_unix_seconds_f64());
    }
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn ingest_config_error_class(error: Option<&IngestConfigError>) -> &'static str {
    match error {
        None => "none",
        Some(IngestConfigError::Config(_)) => "config",
        Some(IngestConfigError::Ingest(_)) => "ingest",
        Some(IngestConfigError::IngestControlBind { .. }) => "ingest_control_bind",
        Some(IngestConfigError::IngestControlTransport { .. }) => "ingest_control_transport",
    }
}

fn current_unix_seconds_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn spawn_ingest_ops(
    listen_addr: SocketAddr,
    network_name: &'static str,
    readiness: &Readiness,
) -> OpsEndpointHandle {
    spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-ingest",
            service_version: env!("CARGO_PKG_VERSION"),
            network_name,
        },
        readiness.clone(),
    )
}

fn print_backfill_config(
    config_path: Option<PathBuf>,
    args: BackfillArgs,
) -> Result<String, IngestConfigError> {
    let backfill_config = config::load_backfill_config(config_path, args.into())?;
    config::redacted_backfill_config_toml(&backfill_config)
}

fn print_tip_follow_config(
    config_path: Option<PathBuf>,
    args: TipFollowArgs,
) -> Result<String, IngestConfigError> {
    let tip_follow_config = config::load_tip_follow_config(config_path, args.into())?;
    config::redacted_tip_follow_config_toml(&tip_follow_config)
}

fn print_backup_config(
    config_path: Option<PathBuf>,
    args: BackupArgs,
) -> Result<String, IngestConfigError> {
    let backup_config = config::load_backup_config(config_path, args.into())?;
    config::redacted_backup_config_toml(&backup_config)
}

impl From<BackfillArgs> for BackfillConfigOverrides {
    fn from(args: BackfillArgs) -> Self {
        Self {
            network: args.network,
            node_source: args.node_source,
            json_rpc_addr: args.json_rpc_addr,
            node_auth_method: args.node_auth_method,
            node_auth_username: args.node_auth_username,
            node_auth_path: args.node_auth_path,
            storage_path: args.storage_path,
            from_height: args.from_height,
            to_height: args.to_height,
            request_timeout_secs: args.request_timeout_secs,
            max_response_bytes: args.max_response_bytes,
            commit_batch_blocks: args.commit_batch_blocks,
            allow_near_tip_finalize: args.allow_near_tip_finalize.then_some(true),
            checkpoint_height: args.checkpoint_height,
            wallet_serving: args.wallet_serving.then_some(true),
        }
    }
}

impl From<TipFollowArgs> for TipFollowConfigOverrides {
    fn from(args: TipFollowArgs) -> Self {
        Self {
            network: args.network,
            node_source: args.node_source,
            json_rpc_addr: args.json_rpc_addr,
            node_auth_method: args.node_auth_method,
            node_auth_username: args.node_auth_username,
            node_auth_path: args.node_auth_path,
            storage_path: args.storage_path,
            request_timeout_secs: args.request_timeout_secs,
            max_response_bytes: args.max_response_bytes,
            reorg_window_blocks: args.reorg_window_blocks,
            commit_batch_blocks: args.commit_batch_blocks,
            poll_interval_ms: args.poll_interval_ms,
            lag_threshold_blocks: args.lag_threshold_blocks,
            ingest_control_listen_addr: args.ingest_control_listen_addr,
        }
    }
}

impl From<BackupArgs> for BackupConfigOverrides {
    fn from(args: BackupArgs) -> Self {
        Self {
            network: args.network,
            storage_path: args.storage_path,
            to_path: args.to_path,
        }
    }
}
