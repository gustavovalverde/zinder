//! Zinder ingestion command-line entry point.

use std::{
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::config::{
    BackfillConfigOverrides, BackfillCoverage, BackupConfigOverrides, IngestConfigError,
    TipFollowConfigOverrides,
};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;

use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_ingest::{
    BackfillOutcome, IngestControlGrpcAdapter, IngestError, MempoolIndex,
    MempoolOrchestratorEventOutcome, MempoolReadySignal, NodeSourceKind, backfill_until_complete,
    mempool_ready_channel, open_tip_follow_store, run_mempool_orchestrator,
    spawn_chain_event_retention_task, spawn_mempool_event_retention_task,
    tip_follow_with_primary_store,
};
use zinder_runtime::{
    OpsEndpointHandle, OpsServer, Readiness, ReadinessCause, ReadinessState, StartupPhase,
    cancel_on_ctrl_c, install_tracing_subscriber, spawn_ops_endpoint,
};
use zinder_source::{
    ChainTipNotificationSource, JsonRpcMempoolSource, MempoolSource, NodeCapabilities,
    NodeCapability, NodeSource, NodeTarget, ZebraIndexerChainTipSource, ZebraIndexerMempoolSource,
    ZebraIndexerSourceTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};

mod cli;
mod config;

const MEMPOOL_ORCHESTRATOR_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const REQUIRED_INGEST_NODE_CAPABILITIES: &[NodeCapability] = &[
    NodeCapability::JsonRpc,
    NodeCapability::BestChainBlocks,
    NodeCapability::TipId,
    NodeCapability::TreeState,
    NodeCapability::SubtreeRoots,
];

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
    /// Path to a file containing the shared-secret bearer token enforced by
    /// the ingest-control endpoint. When unset, the endpoint accepts every
    /// caller (the localhost-only default).
    #[arg(long = "ingest-control-token-path")]
    ingest_control_token_path: Option<PathBuf>,
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

#[allow(
    clippy::too_many_lines,
    reason = "Backfill bootstrap composes the load_config, start_api, connect_node, check_schema, recover_state, validate_config, open_storage, and ready phases plus the actual backfill in one auditable sequence; splitting the phases out would obscure the ordering."
)]
async fn run_backfill(
    config_path: Option<PathBuf>,
    args: BackfillArgs,
    ops_listen_addr: Option<SocketAddr>,
) -> Result<(), IngestConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let mut command_config = match config::load_backfill_config(config_path, args.into()) {
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
    let start_api_phase = StartupPhase::StartApi.start();
    let ops_handle = ops_listen_addr.map(|listen_addr| {
        spawn_ingest_ops(
            listen_addr,
            encode_zinder_native_chain_name(command_config.node.network),
            &readiness,
        )
    });

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let source =
        match zebra_json_rpc_source_for_target(command_config.node_source, &command_config.node) {
            Ok(source) => {
                connect_node_phase.complete();
                source
            }
            Err(error) => {
                connect_node_phase.fail(&error);
                start_api_phase.fail(&error);
                return Err(error);
            }
        };

    let check_schema_phase = StartupPhase::CheckSchema.start();
    match ensure_node_capabilities(&source, &readiness).await {
        Ok(_probed_capabilities) => check_schema_phase.complete(),
        Err(error) => {
            check_schema_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    }

    let recover_state_phase = StartupPhase::RecoverState.start();
    if let Err(error) = resolve_backfill_coverage(&mut command_config, &source).await {
        recover_state_phase.fail(&error);
        start_api_phase.fail(&error);
        return Err(error);
    }
    let checkpoint = if let Some(checkpoint_height) = command_config.checkpoint_height {
        let checkpoint = match source.fetch_chain_checkpoint(checkpoint_height).await {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                let wrapped: IngestConfigError = IngestError::from(error).into();
                recover_state_phase.fail(&wrapped);
                start_api_phase.fail(&wrapped);
                return Err(wrapped);
            }
        };
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
    recover_state_phase.complete();

    let validate_config_phase = StartupPhase::ValidateConfig.start();
    let backfill_config = match command_config.resolved_backfill_config(checkpoint) {
        Ok(cfg) => {
            validate_config_phase.complete();
            cfg
        }
        Err(error) => {
            validate_config_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let store_options = zinder_store::ChainStoreOptions::for_network(backfill_config.node.network);
    let store =
        match zinder_store::PrimaryChainStore::open(&backfill_config.storage_path, store_options) {
            Ok(store) => {
                open_storage_phase.complete();
                store
            }
            Err(store_error) => {
                let wrapped: IngestConfigError = IngestError::from(store_error).into();
                open_storage_phase.fail(&wrapped);
                start_api_phase.fail(&wrapped);
                return Err(wrapped);
            }
        };

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    readiness.set(ReadinessState::syncing(None, None, None));

    let _ingest_control_handle =
        if let Some(listen_addr) = command_config.ingest_control_listen_addr {
            Some(
                spawn_ingest_control_endpoint(IngestControlEndpointSpec {
                    listen_addr,
                    network: backfill_config.node.network,
                    store: store.clone(),
                    mempool_index: MempoolIndex::new(),
                    node_source: Some(Arc::new(source.clone())),
                    bearer_token: command_config.ingest_control_bearer_token.clone(),
                    cancel: cancel.clone(),
                })
                .await?,
            )
        } else {
            None
        };

    let backfill_outcome =
        backfill_until_complete(&backfill_config, &source, &store, &readiness).await?;

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

    let activations = source
        .fetch_network_upgrade_activations()
        .await
        .map_err(IngestError::from)?;
    let Some(earliest) = activations.earliest_wallet_servable_activation() else {
        return Err(
            IngestError::from(zinder_source::SourceError::SourceProtocolMismatch {
                reason: "getblockchaininfo did not advertise Sapling or NU5 activation heights",
            })
            .into(),
        );
    };
    let wallet_serving_floor = earliest.activation_height;
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
        earliest_upgrade = %earliest.name,
        "resolved wallet-serving backfill floor from node activation heights"
    );

    Ok(())
}

/// Probes upstream node capabilities and verifies the ingest-required set.
///
/// On `Err`, transitions `readiness` into the failure cause
/// (`NodeUnavailable` for transport-level failures,
/// `node_capability_missing` for a missing required capability) so the ops
/// endpoint surfaces the same diagnosis the caller will exit with.
async fn ensure_node_capabilities(
    source: &ZebraJsonRpcSource,
    readiness: &Readiness,
) -> Result<NodeCapabilities, IngestConfigError> {
    let probed_capabilities = match source.probe_capabilities().await {
        Ok(probed_capabilities) => probed_capabilities,
        Err(probe_error) => {
            let detail = zinder_runtime::NodeUnavailableDetail::first_iteration(
                probe_error.upstream_classification().label(),
                probe_error.to_string(),
            );
            readiness.set(ReadinessState::node_unavailable_with_detail(detail, None));
            tracing::warn!(
                target: "zinder::ingest",
                event = "node_capability_probe_failed",
                error = %probe_error,
                "node capability probe failed"
            );
            return Err(IngestError::from(probe_error).into());
        }
    };

    let advertised: Vec<&'static str> = probed_capabilities
        .iter()
        .map(zinder_source::NodeCapability::name)
        .collect();
    if probed_capabilities.supports(NodeCapability::OpenRpcDiscovery) {
        tracing::info!(
            target: "zinder::ingest",
            event = "node_capabilities_probed",
            advertised = ?advertised,
            "node advertised capabilities discovered via rpc.discover"
        );
    } else {
        tracing::warn!(
            target: "zinder::ingest",
            event = "node_capability_probe_fallback",
            advertised = ?advertised,
            "node capability probe used baseline capabilities because rpc.discover was unavailable"
        );
    }

    if let Err(error) = require_ingest_node_capabilities(probed_capabilities) {
        if let zinder_source::SourceError::NodeCapabilityMissing { capability } = &error {
            readiness.set(ReadinessState::node_capability_missing(capability.name()));
            tracing::error!(
                target: "zinder::ingest",
                event = "node_capability_missing",
                capability = capability.name(),
                "upstream node is missing a capability required by ingestion"
            );
        }
        return Err(IngestError::from(error).into());
    }

    Ok(probed_capabilities)
}

fn require_ingest_node_capabilities(
    capabilities: NodeCapabilities,
) -> Result<(), zinder_source::SourceError> {
    for required in REQUIRED_INGEST_NODE_CAPABILITIES {
        if !capabilities.supports(*required) {
            return Err(zinder_source::SourceError::NodeCapabilityMissing {
                capability: *required,
            });
        }
    }

    Ok(())
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

#[allow(
    clippy::too_many_lines,
    reason = "tip-follow startup composes the readiness, ops, ingest-control, retention, mempool source, and orchestrator subsystems; the linear sequence is the operator-facing flow and intentionally lives in one function so failure ordering is auditable."
)]
async fn run_tip_follow(
    config_path: Option<PathBuf>,
    args: TipFollowArgs,
    ops_listen_addr: Option<SocketAddr>,
) -> Result<(), IngestConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let command_config = match config::load_tip_follow_config(config_path, args.into()) {
        Ok(command_config) => {
            load_config_phase.complete();
            command_config
        }
        Err(error) => {
            load_config_phase.fail(&error);
            return Err(error);
        }
    };
    let tip_follow_config = command_config.tip_follow;
    let readiness = Readiness::default();

    let start_api_phase = StartupPhase::StartApi.start();
    let ops_handle = ops_listen_addr.map(|listen_addr| {
        spawn_ingest_ops(
            listen_addr,
            encode_zinder_native_chain_name(tip_follow_config.node.network),
            &readiness,
        )
    });

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let store = match open_tip_follow_store(&tip_follow_config) {
        Ok(store) => {
            open_storage_phase.complete();
            store
        }
        Err(error) => {
            open_storage_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error.into());
        }
    };

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let source = match zebra_json_rpc_source_for_target(
        tip_follow_config.node_source,
        &tip_follow_config.node,
    ) {
        Ok(source) => {
            connect_node_phase.complete();
            source
        }
        Err(error) => {
            connect_node_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    };

    let check_schema_phase = StartupPhase::CheckSchema.start();
    match ensure_node_capabilities(&source, &readiness).await {
        Ok(_probed_capabilities) => {
            check_schema_phase.complete();
        }
        Err(error) => {
            check_schema_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    }

    start_api_phase.complete();
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_ctrl_c(cancel.clone());
    let mempool_index = MempoolIndex::new();
    let ingest_control_handle = spawn_ingest_control_endpoint(IngestControlEndpointSpec {
        listen_addr: command_config.ingest_control_listen_addr,
        network: tip_follow_config.node.network,
        store: store.clone(),
        mempool_index: mempool_index.clone(),
        node_source: Some(Arc::new(source.clone())),
        bearer_token: command_config.ingest_control_bearer_token.clone(),
        cancel: cancel.clone(),
    })
    .await?;
    let _retention_handle = spawn_chain_event_retention_task(
        store.clone(),
        readiness.clone(),
        command_config.chain_event_retention,
        cancel.clone(),
    );
    let _mempool_retention_handle = spawn_mempool_event_retention_task(
        store.clone(),
        readiness.clone(),
        command_config.mempool_event_retention,
        cancel.clone(),
    );
    let mempool_source = build_mempool_source(&tip_follow_config.node, &source);
    let chain_tip_source = build_chain_tip_notification_source(&tip_follow_config.node);
    let (mempool_ready_signal, mempool_ready_gate) = mempool_ready_channel();
    let _mempool_orchestrator_handle = spawn_mempool_orchestrator(
        mempool_source,
        store.clone(),
        mempool_index,
        readiness.clone(),
        mempool_ready_signal,
        cancel.clone(),
    );

    tracing::info!(
        target: "zinder::ingest",
        event = "tip_follow_started",
        network = encode_zinder_native_chain_name(tip_follow_config.node.network),
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

    readiness.set(ReadinessState::syncing(None, None, None));
    StartupPhase::Ready.start().complete();
    let tip_follow_outcome = tip_follow_with_primary_store(
        &tip_follow_config,
        &source,
        store,
        &readiness,
        Some(&mempool_ready_gate),
        chain_tip_source,
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

struct IngestControlEndpointSpec {
    listen_addr: SocketAddr,
    network: zinder_core::Network,
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
    node_source: Option<Arc<dyn NodeSource>>,
    bearer_token: Option<zinder_runtime::BearerToken>,
    cancel: CancellationToken,
}

async fn spawn_ingest_control_endpoint(
    spec: IngestControlEndpointSpec,
) -> Result<IngestControlEndpointHandle, IngestConfigError> {
    let listen_addr = spec.listen_addr;
    let listener = TcpListener::bind(listen_addr).await.map_err(|source| {
        IngestConfigError::IngestControlBind {
            listen_addr,
            source,
        }
    })?;
    let incoming = TcpListenerStream::new(listener);
    let endpoint_cancel = CancellationToken::new();
    let endpoint_cancel_for_task = endpoint_cancel.clone();
    let shutdown_cancel = spec.cancel.clone();
    let adapter = {
        let mut adapter = IngestControlGrpcAdapter::new(spec.network, spec.store)
            .with_mempool(spec.mempool_index);
        if let Some(node_source) = spec.node_source {
            adapter = adapter.with_node_source(node_source);
        }
        if let Some(bearer_token) = spec.bearer_token {
            adapter = adapter.with_bearer_token(bearer_token);
        }
        adapter
    };
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

fn build_chain_tip_notification_source(
    node_target: &NodeTarget,
) -> Option<Arc<dyn ChainTipNotificationSource>> {
    let indexer_endpoint = node_target.indexer_grpc_addr.as_ref()?;
    let target = ZebraIndexerSourceTarget::new(indexer_endpoint.clone());
    Some(Arc::new(ZebraIndexerChainTipSource::new(target)))
}

fn build_mempool_source(
    node_target: &NodeTarget,
    json_rpc: &ZebraJsonRpcSource,
) -> Arc<dyn MempoolSource> {
    node_target.indexer_grpc_addr.as_ref().map_or_else(
        || {
            tracing::info!(
                target: "zinder::ingest",
                event = "mempool_source_selected",
                backend = "zebra-json-rpc-polling",
                "selected polling mempool source"
            );
            Arc::new(JsonRpcMempoolSource::new(json_rpc.clone())) as Arc<dyn MempoolSource>
        },
        |indexer_endpoint| {
            tracing::info!(
                target: "zinder::ingest",
                event = "mempool_source_selected",
                backend = "zebra-indexer-grpc",
                indexer_endpoint = %indexer_endpoint,
                "selected streaming mempool source"
            );
            let indexer_target = ZebraIndexerSourceTarget::new(indexer_endpoint.clone());
            Arc::new(ZebraIndexerMempoolSource::new(
                indexer_target,
                json_rpc.clone(),
            )) as Arc<dyn MempoolSource>
        },
    )
}

/// Threshold above which `MempoolHydrationLagging` is reported.
///
/// Hydration failures are typically a single-tx race (the mempool source
/// observed an `Added` for a transaction the upstream node has already
/// dropped). Surfacing one or two of those as a readiness regression would
/// be noisy. Five within a single source session is a sustained pattern
/// worth alerting on.
const MEMPOOL_HYDRATION_LAGGING_THRESHOLD: u64 = 5;

#[allow(
    clippy::too_many_arguments,
    reason = "spawn_mempool_orchestrator threads the source, store, live index, readiness, prime signal, and cancel through the orchestrator's spawn loop; bundling them into a struct adds indirection without changing the binding count callers must make."
)]
#[must_use = "drop the handle to detach the orchestrator or await it for symmetric shutdown"]
fn spawn_mempool_orchestrator(
    mempool_source: Arc<dyn MempoolSource>,
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
    readiness: Readiness,
    mempool_ready_signal: MempoolReadySignal,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut hydration_failures: u64 = 0;
            let outcome_callback = {
                let readiness = readiness.clone();
                let store = store.clone();
                let signal = mempool_ready_signal.clone();
                move |outcome: MempoolOrchestratorEventOutcome| {
                    if matches!(outcome, MempoolOrchestratorEventOutcome::SourceStreamOpened) {
                        signal.set_primed();
                        clear_mempool_orchestrator_readiness(&readiness, &store);
                    } else if matches!(outcome, MempoolOrchestratorEventOutcome::HydrationFailed) {
                        hydration_failures = hydration_failures.saturating_add(1);
                        if hydration_failures >= MEMPOOL_HYDRATION_LAGGING_THRESHOLD {
                            set_mempool_hydration_lagging(&readiness, &store, hydration_failures);
                        }
                    }
                }
            };
            let orchestrator = run_mempool_orchestrator(
                Arc::clone(&mempool_source),
                store.clone(),
                mempool_index.clone(),
                outcome_callback,
            );
            tokio::pin!(orchestrator);
            tokio::select! {
                outcome = &mut orchestrator => {
                    match outcome {
                        Ok(()) => {
                            tracing::info!(
                                target: "zinder::ingest",
                                event = "mempool_orchestrator_stream_closed",
                                "mempool source stream ended; reconnecting"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "zinder::ingest",
                                event = "mempool_orchestrator_failed",
                                error = %error,
                                "mempool orchestrator returned an error; reconnecting"
                            );
                            set_mempool_source_unavailable(&readiness, &store);
                        }
                    }
                }
                () = cancel.cancelled() => {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "mempool_orchestrator_cancelled",
                        "mempool orchestrator cancelled"
                    );
                    return;
                }
            }
            tokio::select! {
                () = tokio::time::sleep(MEMPOOL_ORCHESTRATOR_RECONNECT_BACKOFF) => {}
                () = cancel.cancelled() => return,
            }
        }
    })
}

fn current_chain_height(store: &PrimaryChainStore) -> Option<u32> {
    store
        .current_chain_epoch()
        .ok()
        .flatten()
        .map(|chain_epoch| chain_epoch.tip_height.value())
}

fn clear_mempool_orchestrator_readiness(readiness: &Readiness, store: &PrimaryChainStore) {
    let cause = readiness.report().cause;
    if matches!(
        cause,
        ReadinessCause::MempoolSourceUnavailable | ReadinessCause::MempoolHydrationLagging { .. }
    ) {
        readiness.set(ReadinessState::ready(current_chain_height(store)));
    }
}

fn set_mempool_source_unavailable(readiness: &Readiness, store: &PrimaryChainStore) {
    let cause = readiness.report().cause;
    if !matches!(cause, ReadinessCause::ReorgWindowExceeded { .. }) {
        readiness.set(ReadinessState::mempool_source_unavailable(
            current_chain_height(store),
        ));
    }
}

fn set_mempool_hydration_lagging(
    readiness: &Readiness,
    store: &PrimaryChainStore,
    recent_hydration_failures: u64,
) {
    let cause = readiness.report().cause;
    if matches!(
        cause,
        ReadinessCause::Ready | ReadinessCause::MempoolHydrationLagging { .. }
    ) {
        readiness.set(ReadinessState::mempool_hydration_lagging(
            recent_hydration_failures,
            current_chain_height(store),
        ));
    }
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
        network = encode_zinder_native_chain_name(backup_config.network),
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
        "network" => encode_zinder_native_chain_name(network),
        "status" => outcome_status(backup_outcome),
        "error_class" => ingest_config_error_class(backup_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_backup_total",
        "network" => encode_zinder_native_chain_name(network),
        "status" => outcome_status(backup_outcome),
        "error_class" => ingest_config_error_class(backup_outcome.as_ref().err())
    )
    .increment(1);
    if backup_outcome.is_ok() {
        metrics::gauge!(
            "zinder_ingest_backup_last_success_unix_seconds",
            "network" => encode_zinder_native_chain_name(network)
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
        Some(IngestConfigError::BearerToken(_)) => "bearer_token",
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
            ingest_control_token_path: args.ingest_control_token_path,
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

#[cfg(test)]
mod tests {
    use super::{
        NodeCapabilities, NodeCapability, ZebraJsonRpcSource, require_ingest_node_capabilities,
    };

    #[test]
    fn ingest_capability_validation_accepts_zebra_baseline()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = ZebraJsonRpcSource::baseline_capabilities();

        require_ingest_node_capabilities(capabilities)?;

        Ok(())
    }

    #[test]
    fn ingest_capability_validation_rejects_missing_tree_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = NodeCapabilities::new([
            NodeCapability::JsonRpc,
            NodeCapability::BestChainBlocks,
            NodeCapability::TipId,
            NodeCapability::SubtreeRoots,
        ])?;

        let Err(error) = require_ingest_node_capabilities(capabilities) else {
            return Err(Box::new(std::io::Error::other(
                "missing tree-state support passed startup validation",
            )));
        };

        assert!(matches!(
            error,
            zinder_source::SourceError::NodeCapabilityMissing {
                capability: NodeCapability::TreeState,
            }
        ));

        Ok(())
    }
}
