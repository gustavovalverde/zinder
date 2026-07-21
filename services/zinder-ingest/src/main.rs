//! Zinder ingestion command-line entry point.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Parser, Subcommand};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, NetworkUpgradeActivations};
use zinder_ingest::{
    CanonicalCheckpointStagingRoot, CanonicalConstructionConfig, CanonicalControlCommand,
    CanonicalControlGrpcAdapter, CanonicalFollowConfig, CanonicalIngestControlGrpcAdapter,
    CanonicalRunOverrides, CanonicalWriterConfig, DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL,
    IngestError, LiveMempoolOwner, NodeSourceKind, canonical_control_channel, classify_phase,
    mempool_ready_channel, run_canonical_writer_with_control, run_live_mempool_owner,
    run_mempool_retention, spawn_runtime_memory_metrics_task, spawn_upstream_health_probe_task,
};
use zinder_runtime::{
    OpsEndpointHandle, Readiness, ReadinessState, RuntimeService, StartupPhase,
    cancel_on_terminating_signal, host_cpu_meets_compiled_baseline, install_tracing_subscriber,
    spawn_ops_endpoint_for,
};
use zinder_source::{
    JsonRpcMempoolSource, MempoolSource, NodeCapabilities, NodeCapability, NodeSource, NodeTarget,
    ZebraIndexerMempoolSource, ZebraIndexerSourceTarget, ZebraJsonRpcSource,
    ZebraJsonRpcSourceOptions,
};
use zinder_store::{ChainStoreOptions, MempoolEventRetentionConfig, SecondaryChainStore};

use crate::config::{
    CanonicalReplayVerificationConfigOverrides, IngestCommandConfig, IngestConfigError,
    IngestConfigOverrides, IngestCoverage,
};

mod cli;
mod config;
mod replay_verification;

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
    /// Network name, such as zcash-regtest.
    #[arg(long, global = true)]
    network: Option<String>,
    /// Upstream node source, currently zebra-json-rpc.
    #[arg(long = "node-source", global = true)]
    node_source: Option<String>,
    /// Zebra JSON-RPC address.
    #[arg(long = "json-rpc-addr", global = true)]
    json_rpc_addr: Option<String>,
    /// Node auth method, such as none, basic, or cookie.
    #[arg(long = "node-auth-method", global = true)]
    node_auth_method: Option<String>,
    /// Node auth username when the method is basic.
    #[arg(long = "node-auth-username", global = true)]
    node_auth_username: Option<String>,
    /// Node auth cookie path when the method is cookie.
    #[arg(long = "node-auth-path", global = true)]
    node_auth_path: Option<PathBuf>,
    /// Canonical Zinder store path.
    #[arg(long = "storage-path", global = true)]
    storage_path: Option<PathBuf>,
    /// Node request timeout in seconds.
    #[arg(long = "request-timeout-secs", global = true)]
    request_timeout_secs: Option<u64>,
    /// Maximum JSON-RPC response body size in bytes.
    #[arg(long = "max-response-bytes", global = true)]
    max_response_bytes: Option<u64>,
    /// Number of near-tip blocks that may be replaced by a reorg.
    #[arg(long = "reorg-window-blocks", global = true)]
    reorg_window_blocks: Option<u32>,
    /// Phase-classifier boundary; gaps above this trigger `BulkCatchup`.
    #[arg(long = "catchup-threshold-blocks", global = true)]
    catchup_threshold_blocks: Option<u32>,
    /// Maximum number of blocks committed in one bulk-catchup batch.
    #[arg(long = "canonical-batch-max-blocks", global = true)]
    canonical_batch_max_blocks: Option<u32>,
    /// Maximum canonical artifact bytes accumulated before closing a batch.
    #[arg(long = "canonical-batch-max-artifact-bytes", global = true)]
    canonical_batch_max_artifact_bytes: Option<u64>,
    /// Maximum estimated canonical write bytes accumulated before closing a batch.
    #[arg(long = "canonical-batch-max-estimated-write-bytes", global = true)]
    canonical_batch_max_estimated_write_bytes: Option<u64>,
    /// Minimum blocks before estimated write bytes can close a batch.
    #[arg(
        long = "canonical-batch-min-blocks-before-estimated-write-close",
        global = true
    )]
    canonical_batch_min_blocks_before_estimated_write_close: Option<u32>,
    /// Maximum connected blocks requested from the source in one bulk-catchup segment.
    #[arg(long = "source-segment-max-blocks", global = true)]
    source_segment_max_blocks: Option<u32>,
    /// Target response bytes for adaptive source segment sizing.
    #[arg(long = "source-segment-target-response-bytes", global = true)]
    source_segment_target_response_bytes: Option<u64>,
    /// Maximum concurrent source segment fetch requests.
    #[arg(long = "source-fetch-max-in-flight-requests", global = true)]
    source_fetch_max_in_flight_requests: Option<u32>,
    /// Maximum reserved response bytes across source segment fetches.
    #[arg(long = "source-fetch-max-in-flight-bytes", global = true)]
    source_fetch_max_in_flight_bytes: Option<u64>,
    /// Number of parallel `prepare_canonical_block` invocations on the blocking pool.
    #[arg(long = "block-prepare-concurrency", global = true)]
    block_prepare_concurrency: Option<u32>,
    /// Delay between upstream node tip polls, in milliseconds.
    #[arg(long = "poll-interval-ms", global = true)]
    poll_interval_ms: Option<u64>,
    /// Lag threshold (in blocks) below which tip-follow reports `Ready`.
    #[arg(long = "lag-threshold-blocks", global = true)]
    lag_threshold_blocks: Option<u64>,
    /// Stop committing after reaching this height.
    #[arg(long = "target-height", global = true)]
    target_height: Option<u32>,
    /// Bootstrap an empty store from the upstream node's chain state at
    /// this height (the ingest loop begins at `checkpoint_height + 1`).
    #[arg(long = "checkpoint-height", global = true)]
    checkpoint_height: Option<u32>,
    /// Allow bulk-catchup batches to advance the settled tip inside the upstream
    /// node's reorg window. Disposable-store recovery only.
    #[arg(long = "allow-reorg-window-settlement", action = clap::ArgAction::SetTrue, global = true)]
    allow_reorg_window_settlement: bool,
    /// Derive the bulk-catchup floor needed by native wallets from
    /// node-advertised activation heights.
    #[arg(long = "wallet-serving", action = clap::ArgAction::SetTrue, global = true)]
    wallet_serving: bool,
    /// Private ingest-control gRPC listen address.
    #[arg(long = "ingest-control-listen-addr", global = true)]
    ingest_control_listen_addr: Option<SocketAddr>,
    /// Path to a file containing the shared-secret bearer token enforced
    /// by the ingest-control endpoint.
    #[arg(long = "ingest-control-token-path", global = true)]
    ingest_control_token_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print store + upstream state, the phase the loop would run, and the
    /// upstream-health snapshot, then exit. Diagnostic only; does not ingest
    /// or commit chain data.
    Probe,
    /// Check replay-envelope integrity and canonical-header parity through a
    /// reader-local `RocksDB` secondary.
    VerifyCanonicalReplay(CanonicalReplayVerificationArgs),
}

#[derive(Parser)]
struct CanonicalReplayVerificationArgs {
    /// Network name, such as zcash-regtest.
    #[arg(long)]
    network: Option<String>,
    /// Canonical Zinder store path opened as the secondary's primary source.
    #[arg(long = "storage-path")]
    storage_path: Option<PathBuf>,
    /// Reader-local metadata path for this verification run.
    #[arg(long = "secondary-path")]
    secondary_path: Option<PathBuf>,
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

    Box::pin(run_runtime(cli)).await
}

#[allow(
    clippy::print_stdout,
    reason = "probe is a CLI diagnostic; the structured snapshot is what operators read."
)]
async fn run_probe(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<(), IngestConfigError> {
    let command_config = config::load_ingest_config(config_path, overrides)?;
    let runtime_config = command_config.runtime_config;
    let source =
        zebra_json_rpc_source_for_target(runtime_config.node_source, &runtime_config.node)?;
    let activations = source
        .discover_network_upgrade_activations("zinder-ingest-probe")
        .await
        .map_err(IngestError::from)?;
    let upstream_tip = source
        .tip_id()
        .await
        .map_err(IngestError::from)?
        .height
        .value();
    let store = zinder_store::RocksDbCanonicalStore::open_ready(
        &runtime_config.storage_path,
        &activations,
        zinder_store::CanonicalStoreWorkload::Wallet,
        zinder_store::CanonicalReorgPolicy::new(runtime_config.reorg_window_blocks)
            .map_err(zinder_ingest::CanonicalWriterError::from)?,
        runtime_config.canonical_rocksdb_budget,
    )
    .map_err(zinder_ingest::CanonicalWriterError::from)
    .map_err(IngestConfigError::from)?;
    let store_tip_height = store.event_fence().visible_tip().height.value();
    let store_tip = Some(store_tip_height);
    let phase = classify_phase(
        store_tip,
        upstream_tip,
        runtime_config.phase_classification.catchup_threshold_blocks,
    );
    let gap_blocks = i64::from(upstream_tip).saturating_sub(i64::from(store_tip_height));
    let upstream_health = match source.poll_upstream_health().await {
        Ok(snapshot) => format!("{}/{}", snapshot.source, snapshot.reason),
        Err(error) => format!("unavailable ({error})"),
    };

    println!(
        "{{\"store_tip\": {store_tip:?}, \"upstream_tip\": {upstream_tip}, \
         \"gap_blocks\": {gap_blocks}, \"phase_that_would_run\": \"{phase}\", \
         \"upstream_health\": \"{upstream_health}\"}}",
        phase = phase.wire_label(),
    );
    Ok(())
}

async fn run_ingest(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<(), IngestConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let mut command_config = config::load_ingest_config(config_path, overrides)?;
    load_config_phase.complete();
    let readiness = Readiness::default();
    let start_api_phase = StartupPhase::StartApi.start();
    let ops_handle = spawn_ops_endpoint_for(
        RuntimeService::Ingest,
        command_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(command_config.runtime_config.node.network),
        readiness.clone(),
        zinder_proto::capabilities::always_on_capability_strings(
            zinder_proto::capabilities::CapabilitySurface::Ingest,
        ),
    );

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let source = zebra_json_rpc_source_for_target(
        command_config.runtime_config.node_source,
        &command_config.runtime_config.node,
    )?;
    connect_node_phase.complete();
    let check_schema_phase = StartupPhase::CheckSchema.start();
    ensure_node_capabilities(&source, &readiness).await?;
    let network_upgrade_activations = source
        .discover_network_upgrade_activations("zinder-ingest")
        .await
        .map_err(IngestError::from)?;
    check_schema_phase.complete();

    let recover_state_phase = StartupPhase::RecoverState.start();
    resolve_wallet_serving_modifiers(&mut command_config);
    recover_state_phase.complete();
    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());
    let _upstream_health_probe_handle = spawn_upstream_health_probe_for(
        &command_config.runtime_config.node,
        &source,
        readiness.clone(),
        cancel.clone(),
    );
    let memory_metrics_handle =
        spawn_runtime_memory_metrics_task(DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL, cancel.clone());
    start_api_phase.complete();
    StartupPhase::Ready.start().complete();

    let mut writer_config =
        canonical_writer_config(&command_config, Arc::clone(&network_upgrade_activations));
    let mut canonical_control_tasks = spawn_canonical_control_tasks(
        &command_config,
        &source,
        &readiness,
        &cancel,
        &mut writer_config,
    );
    log_canonical_writer_start(&command_config, &writer_config);
    let writer_result = run_supervised_canonical_writer(
        &source,
        CanonicalWriterInputs {
            network_upgrade_activations,
            writer_config,
        },
        &readiness,
        &cancel,
        &mut canonical_control_tasks,
    )
    .await;
    shutdown_ingest_tasks(
        &cancel,
        canonical_control_tasks,
        memory_metrics_handle,
        ops_handle,
    )
    .await;
    writer_result.map_err(IngestConfigError::from)
}

type CanonicalControlServer = JoinHandle<Result<(), tonic::transport::Error>>;

struct CanonicalControlTasks {
    server: Option<CanonicalControlServer>,
    commands: Option<mpsc::Receiver<CanonicalControlCommand>>,
    mempool_owner: Option<JoinHandle<()>>,
    mempool_retention: Option<JoinHandle<()>>,
    server_completed: bool,
}

fn spawn_canonical_control_tasks(
    command_config: &IngestCommandConfig,
    source: &ZebraJsonRpcSource,
    readiness: &Readiness,
    cancel: &CancellationToken,
    writer_config: &mut CanonicalWriterConfig,
) -> CanonicalControlTasks {
    if let Some(listen_addr) = command_config.ingest_control_listen_addr {
        let (canonical_control_handle, canonical_control_commands) = canonical_control_channel();
        let mempool_owner = LiveMempoolOwner::default();
        let (mempool_ready_signal, mempool_ready_gate) = mempool_ready_channel();
        writer_config.follow.mempool_ready_gate = Some(mempool_ready_gate);
        let mempool_source = build_live_mempool_source(&command_config.runtime_config.node, source);
        let mempool_owner_task = tokio::spawn(run_live_mempool_owner(
            mempool_source,
            canonical_control_handle.clone(),
            mempool_owner.clone(),
            mempool_ready_signal,
            cancel.clone(),
        ));
        let mempool_retention = MempoolEventRetentionConfig::new(
            command_config.retention.mempool_mined_window(),
            command_config.retention.mempool_invalidated_window(),
        );
        let mempool_retention_task = tokio::spawn(run_mempool_retention(
            canonical_control_handle.clone(),
            mempool_owner.clone(),
            mempool_retention,
            command_config.retention.mempool_check_interval(),
            cancel.clone(),
        ));
        let canonical_adapter = CanonicalControlGrpcAdapter::new(
            canonical_control_handle.clone(),
            CanonicalCheckpointStagingRoot::new(
                command_config
                    .ingest_control_checkpoint_staging_root
                    .clone(),
            ),
            command_config.runtime_config.canonical_rocksdb_budget,
        )
        .with_bearer_token(command_config.ingest_control_bearer_token.clone())
        .with_checkpoint_bearer_token(
            command_config
                .ingest_control_checkpoint_bearer_token
                .clone(),
        );
        let node_source: Arc<dyn NodeSource> = Arc::new(source.clone());
        let ingest_adapter = CanonicalIngestControlGrpcAdapter::new(
            command_config.runtime_config.node.network,
            canonical_control_handle,
            mempool_owner,
            node_source,
            readiness.clone(),
        )
        .with_bearer_token(command_config.ingest_control_bearer_token.clone());
        let server_cancel = cancel.clone();
        let canonical_control_server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(canonical_adapter.into_server())
                .add_service(ingest_adapter.into_server())
                .serve_with_shutdown(listen_addr, server_cancel.cancelled_owned())
                .await
        });
        CanonicalControlTasks {
            server: Some(canonical_control_server),
            commands: Some(canonical_control_commands),
            mempool_owner: Some(mempool_owner_task),
            mempool_retention: Some(mempool_retention_task),
            server_completed: false,
        }
    } else {
        CanonicalControlTasks {
            server: None,
            commands: None,
            mempool_owner: None,
            mempool_retention: None,
            server_completed: false,
        }
    }
}

struct CanonicalWriterInputs {
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    writer_config: CanonicalWriterConfig,
}

async fn run_supervised_canonical_writer(
    source: &ZebraJsonRpcSource,
    inputs: CanonicalWriterInputs,
    readiness: &Readiness,
    cancel: &CancellationToken,
    control_tasks: &mut CanonicalControlTasks,
) -> Result<(), zinder_ingest::CanonicalWriterError> {
    let writer = run_canonical_writer_with_control(
        source,
        inputs.network_upgrade_activations,
        inputs.writer_config,
        readiness,
        cancel,
        control_tasks.commands.take(),
    );
    tokio::pin!(writer);
    let writer_result = if let Some(canonical_control_server) = control_tasks.server.as_mut() {
        tokio::select! {
            writer_result = &mut writer => writer_result,
            server_result = canonical_control_server => {
                control_tasks.server_completed = true;
                cancel.cancel();
                let reason = match server_result {
                    Ok(Ok(())) => "canonical control server stopped unexpectedly".to_owned(),
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                };
                Err(zinder_ingest::CanonicalWriterError::ControlServer { reason })
            }
        }
    } else {
        writer.await
    };
    writer_result.map(drop)
}

async fn shutdown_ingest_tasks(
    cancel: &CancellationToken,
    mut control_tasks: CanonicalControlTasks,
    memory_metrics_handle: JoinHandle<()>,
    ops_handle: Option<OpsEndpointHandle>,
) {
    cancel.cancel();
    if !control_tasks.server_completed {
        await_canonical_control_server_shutdown(control_tasks.server.take()).await;
    }
    if let Some(mempool_owner_task) = control_tasks.mempool_owner.take()
        && let Err(join_error) = mempool_owner_task.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "mempool_owner_join_failed",
            error = %join_error,
            "live mempool owner did not shut down cleanly"
        );
    }
    if let Some(mempool_retention_task) = control_tasks.mempool_retention.take()
        && let Err(join_error) = mempool_retention_task.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "mempool_retention_join_failed",
            error = %join_error,
            "mempool retention task did not shut down cleanly"
        );
    }
    if let Err(join_error) = memory_metrics_handle.await {
        tracing::warn!(
            target: "zinder::ingest",
            event = "runtime_memory_metrics_join_failed",
            error = %join_error,
            "runtime memory metrics task failed during shutdown"
        );
    }
    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }
}

async fn await_canonical_control_server_shutdown(server: Option<CanonicalControlServer>) {
    let Some(server) = server else {
        return;
    };
    if let Err(join_error) = server.await {
        tracing::warn!(
            target: "zinder::ingest",
            event = "canonical_control_server_join_failed",
            error = %join_error,
            "canonical control server did not shut down cleanly"
        );
    }
}

fn canonical_writer_config(
    command_config: &IngestCommandConfig,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
) -> CanonicalWriterConfig {
    CanonicalWriterConfig {
        storage_path: command_config.runtime_config.storage_path.clone(),
        resource_budget: command_config.runtime_config.canonical_rocksdb_budget,
        construction: CanonicalConstructionConfig {
            request_timeout: command_config.runtime_config.node.request_timeout,
            pipeline_limits: command_config.runtime_config.construction.pipeline_limits,
            network_upgrade_activations,
        },
        checkpoint_height: command_config
            .runtime_config
            .run_overrides
            .checkpoint_height,
        reorg_window_blocks: command_config.runtime_config.reorg_window_blocks,
        follow: CanonicalFollowConfig {
            request_timeout: command_config.runtime_config.node.request_timeout,
            poll_interval: command_config.runtime_config.follow.poll_interval,
            lag_threshold_blocks: command_config.runtime_config.follow.lag_threshold_blocks,
            target_height: command_config.runtime_config.run_overrides.target_height,
            event_retention_window: command_config.retention.chain_event_window(),
            event_retention_check_interval: command_config.retention.chain_event_check_interval(),
            mempool_ready_gate: None,
        },
    }
}

fn log_canonical_writer_start(
    command_config: &IngestCommandConfig,
    writer_config: &CanonicalWriterConfig,
) {
    tracing::info!(
        target: "zinder::ingest",
        event = "canonical_writer_started",
        network = encode_zinder_native_chain_name(command_config.runtime_config.node.network),
        storage_path = %writer_config.storage_path.display(),
        json_rpc_addr = command_config.runtime_config.node.json_rpc_addr.as_str(),
        workload = "wallet",
        schema_version = zinder_store::CANONICAL_STORE_SCHEMA_VERSION,
        reorg_window_blocks = writer_config.reorg_window_blocks,
        checkpoint_height = ?writer_config.checkpoint_height.map(BlockHeight::value),
        target_height = ?writer_config.follow.target_height.map(BlockHeight::value),
        historical_prevout_reads = 0_u64,
        cross_block_wallet_reads = 0_u64,
        "canonical writer started"
    );
}

fn resolve_wallet_serving_modifiers(command_config: &mut IngestCommandConfig) {
    if !matches!(command_config.coverage, IngestCoverage::WalletServing) {
        return;
    }

    let checkpoint_height = BlockHeight::new(0);
    command_config.runtime_config.run_overrides = CanonicalRunOverrides {
        target_height: command_config.runtime_config.run_overrides.target_height,
        checkpoint_height: Some(checkpoint_height),
        allow_reorg_window_settlement: command_config
            .runtime_config
            .run_overrides
            .allow_reorg_window_settlement,
        checkpoint: command_config
            .runtime_config
            .run_overrides
            .checkpoint
            .clone(),
    };
    tracing::info!(
        target: "zinder::ingest",
        event = "wallet_serving_modifiers_resolved",
        from_height = 1_u32,
        checkpoint_height = checkpoint_height.value(),
        "resolved complete wallet-serving history"
    );
}

async fn ensure_node_capabilities(
    source: &ZebraJsonRpcSource,
    readiness: &Readiness,
) -> Result<NodeCapabilities, IngestConfigError> {
    let probed_capabilities = match source.probe_capabilities().await {
        Ok(capabilities) => capabilities,
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
                broadcast_timeout: target.broadcast_timeout,
            },
        )
        .map(|source| source.with_health_config(target.health.clone()))
        .map_err(IngestError::from)
        .map_err(IngestConfigError::from),
    }
}

fn build_live_mempool_source(
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
            Arc::new(ZebraIndexerMempoolSource::new(
                ZebraIndexerSourceTarget::new(indexer_endpoint.clone()),
                json_rpc.clone(),
            )) as Arc<dyn MempoolSource>
        },
    )
}

fn spawn_upstream_health_probe_for(
    node_target: &NodeTarget,
    json_rpc_source: &ZebraJsonRpcSource,
    readiness: Readiness,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let health_config = node_target.health.as_ref()?;
    tracing::info!(
        target: "zinder::ingest",
        event = "upstream_health_probe_started",
        addr = health_config.addr.as_str(),
        poll_interval_ms = u64::try_from(health_config.poll_interval.as_millis())
            .unwrap_or(u64::MAX),
        "upstream health probe started"
    );
    Some(spawn_upstream_health_probe_task(
        Arc::new(json_rpc_source.clone()),
        readiness,
        health_config.poll_interval,
        cancel,
    ))
}

#[allow(
    clippy::print_stdout,
    reason = "--print-config is a structured TOML data dump, not a log event"
)]
fn run_print_config(cli: Cli) -> ExitCode {
    let overrides = ingest_overrides(&cli);
    let config_path = cli.config_path.clone();
    let render_result = match cli.command {
        None | Some(Command::Probe) => print_ingest_config(config_path, overrides),
        Some(Command::VerifyCanonicalReplay(args)) => {
            print_canonical_replay_verification_config(config_path, args)
        }
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
    let ops_listen_addr_override = cli.ops_listen_addr;
    let overrides = ingest_overrides_with_ops(&cli, ops_listen_addr_override);
    let config_path = cli.config_path.clone();
    let runtime_result = match cli.command {
        Some(Command::Probe) => run_probe(config_path, overrides).await,
        Some(Command::VerifyCanonicalReplay(args)) => {
            run_canonical_replay_verification(config_path, args)
        }
        None => run_ingest(config_path, overrides).await,
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

fn ingest_overrides(cli: &Cli) -> IngestConfigOverrides {
    ingest_overrides_with_ops(cli, None)
}

fn ingest_overrides_with_ops(
    cli: &Cli,
    ops_listen_addr_override: Option<SocketAddr>,
) -> IngestConfigOverrides {
    IngestConfigOverrides {
        network: cli.network.clone(),
        node_source: cli.node_source.clone(),
        json_rpc_addr: cli.json_rpc_addr.clone(),
        node_auth_method: cli.node_auth_method.clone(),
        node_auth_username: cli.node_auth_username.clone(),
        node_auth_path: cli.node_auth_path.clone(),
        storage_path: cli.storage_path.clone(),
        request_timeout_secs: cli.request_timeout_secs,
        max_response_bytes: cli.max_response_bytes,
        reorg_window_blocks: cli.reorg_window_blocks,
        catchup_threshold_blocks: cli.catchup_threshold_blocks,
        canonical_batch_max_blocks: cli.canonical_batch_max_blocks,
        canonical_batch_max_artifact_bytes: cli.canonical_batch_max_artifact_bytes,
        canonical_batch_max_estimated_write_bytes: cli.canonical_batch_max_estimated_write_bytes,
        canonical_batch_min_blocks_before_estimated_write_close: cli
            .canonical_batch_min_blocks_before_estimated_write_close,
        source_segment_max_blocks: cli.source_segment_max_blocks,
        source_segment_target_response_bytes: cli.source_segment_target_response_bytes,
        source_fetch_max_in_flight_requests: cli.source_fetch_max_in_flight_requests,
        source_fetch_max_in_flight_bytes: cli.source_fetch_max_in_flight_bytes,
        block_prepare_concurrency: cli.block_prepare_concurrency,
        poll_interval_ms: cli.poll_interval_ms,
        lag_threshold_blocks: cli.lag_threshold_blocks,
        target_height: cli.target_height,
        checkpoint_height: cli.checkpoint_height,
        allow_reorg_window_settlement: cli.allow_reorg_window_settlement.then_some(true),
        wallet_serving: cli.wallet_serving.then_some(true),
        ingest_control_listen_addr: cli.ingest_control_listen_addr,
        ingest_control_bearer_token_path: cli.ingest_control_token_path.clone(),
        ingest_control_checkpoint_bearer_token_path: None,
        ops_listen_addr: ops_listen_addr_override,
    }
}

#[allow(
    clippy::print_stdout,
    reason = "canonical replay verification emits one machine-readable operator report"
)]
fn run_canonical_replay_verification(
    config_path: Option<PathBuf>,
    args: CanonicalReplayVerificationArgs,
) -> Result<(), IngestConfigError> {
    let command_config =
        config::load_canonical_replay_verification_config(config_path, args.into())?;
    let secondary_store = SecondaryChainStore::open(
        &command_config.storage_path,
        &command_config.secondary_path,
        ChainStoreOptions {
            rocksdb_resource_budget: command_config.canonical_rocksdb_budget,
            ..ChainStoreOptions::for_network(command_config.network)
        },
    )
    .map_err(IngestError::from)?;
    secondary_store.try_catch_up().map_err(IngestError::from)?;
    let report = replay_verification::verify_canonical_replay_store(&secondary_store)?;
    let report_json = report.to_json()?;
    println!("{report_json}");

    Ok(())
}

fn print_ingest_config(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<String, IngestConfigError> {
    let command_config = config::load_ingest_config(config_path, overrides)?;
    config::redacted_ingest_config_toml(&command_config)
}

fn print_canonical_replay_verification_config(
    config_path: Option<PathBuf>,
    args: CanonicalReplayVerificationArgs,
) -> Result<String, IngestConfigError> {
    let command_config =
        config::load_canonical_replay_verification_config(config_path, args.into())?;
    config::redacted_canonical_replay_verification_config_toml(&command_config)
}

impl From<CanonicalReplayVerificationArgs> for CanonicalReplayVerificationConfigOverrides {
    fn from(args: CanonicalReplayVerificationArgs) -> Self {
        Self {
            network: args.network,
            storage_path: args.storage_path,
            secondary_path: args.secondary_path,
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

        let error = require_ingest_node_capabilities(capabilities)
            .err()
            .ok_or("missing tree-state capability must be rejected")?;
        assert!(matches!(
            error,
            zinder_source::SourceError::NodeCapabilityMissing {
                capability: NodeCapability::TreeState
            }
        ));

        Ok(())
    }
}
