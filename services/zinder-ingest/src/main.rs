//! Zinder ingestion command-line entry point.

use std::{
    ffi::OsString,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, NetworkUpgradeActivations};
use zinder_ingest::{
    CommitmentRootBackfillContext, ConventionalFeeDistributionBackfillContext,
    DEFAULT_DERIVE_TAILER_POLL_INTERVAL, DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL,
    IngestControlGrpcAdapter, IngestError, IngestModifiers, MempoolIndex,
    MempoolOrchestratorEventOutcome, MempoolReadySignal, NodeSourceKind,
    PaidFeeDistributionBackfillContext, TipFollowSubsystems, TipFollowSubsystemsLauncher,
    TransactionComponentBackfillContext, TransactionHistoryVerifierContext,
    ValuePoolBalanceBackfillContext, ValuePoolFlowBackfillContext,
    bootstrap_transparent_address_ranking, catch_up_derive_store_to_canonical_until_handoff,
    classify_phase, current_chain_height, ensure_spend_projection_not_behind_retention_sweep,
    mempool_ready_channel, open_primary_derive_store_for_canonical, run_ingest_loop,
    run_mempool_orchestrator, seed_backfill_owned_consumer_cursors,
    seed_paid_fee_distribution_cursor_and_tail, seed_value_pool_flow_cursor_and_tail,
    spawn_block_production_time_backfill_task, spawn_chain_event_retention_task,
    spawn_commitment_root_backfill_task, spawn_conventional_fee_distribution_backfill_task,
    spawn_derive_replay_budget_metrics_task, spawn_derive_tailer_task,
    spawn_mempool_event_retention_task, spawn_paid_fee_distribution_backfill_task,
    spawn_runtime_memory_metrics_task, spawn_transaction_component_backfill_task,
    spawn_transaction_history_verifier_task, spawn_upstream_health_probe_task,
    spawn_value_pool_balance_backfill_task, spawn_value_pool_flow_backfill_task,
};
use zinder_runtime::{
    Readiness, ReadinessCause, ReadinessState, ServiceIdentifier, StartupPhase,
    cancel_on_terminating_signal, host_cpu_meets_compiled_baseline, install_tracing_subscriber,
    spawn_ops_endpoint_for,
};
use zinder_source::{
    ChainTipNotificationSource, JsonRpcMempoolSource, MempoolSource, NodeCapabilities,
    NodeCapability, NodeSource, NodeTarget, ZebraIndexerChainTipSource, ZebraIndexerMempoolSource,
    ZebraIndexerSourceTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};

use crate::config::{
    BackupConfigOverrides, IngestCommandConfig, IngestConfigError, IngestConfigOverrides,
    IngestCoverage,
};

mod cli;
mod config;

const MEMPOOL_ORCHESTRATOR_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const REQUIRED_INGEST_NODE_CAPABILITIES: &[NodeCapability] = &[
    NodeCapability::JsonRpc,
    NodeCapability::BestChainBlocks,
    NodeCapability::TipId,
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
    /// Number of parallel `derive_block` invocations on the blocking pool.
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
    /// this height (the unified loop begins at `checkpoint_height + 1`).
    #[arg(long = "checkpoint-height", global = true)]
    checkpoint_height: Option<u32>,
    /// Allow bulk-catchup batches to finalize blocks inside the upstream
    /// node's reorg window. Disposable-store recovery only.
    #[arg(long = "allow-near-tip-finalize", action = clap::ArgAction::SetTrue, global = true)]
    allow_near_tip_finalize: bool,
    /// Derive the bulk-catchup floor needed by lightwalletd-compatible
    /// wallets from node-advertised activation heights.
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
    /// upstream-health snapshot, then exit. Diagnostic only; does not
    /// open the store for writing.
    Probe,
    /// Create a point-in-time `RocksDB` checkpoint of the canonical store.
    Backup(BackupArgs),
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
    let overrides = ingest_overrides(&cli);
    let config_path = cli.config_path.clone();
    let render_result = match cli.command {
        None | Some(Command::Probe) => print_ingest_config(config_path, overrides),
        Some(Command::Backup(args)) => print_backup_config(config_path, args),
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
        Some(Command::Backup(args)) => run_backup(config_path, args),
        Some(Command::Probe) => run_probe(config_path, overrides).await,
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
        allow_near_tip_finalize: cli.allow_near_tip_finalize.then_some(true),
        wallet_serving: cli.wallet_serving.then_some(true),
        ingest_control_listen_addr: cli.ingest_control_listen_addr,
        ingest_control_bearer_token_path: cli.ingest_control_token_path.clone(),
        ops_listen_addr: ops_listen_addr_override,
    }
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
    let loop_config = command_config.loop_config;
    let source = zebra_json_rpc_source_for_target(loop_config.node_source, &loop_config.node)?;
    let upstream_tip = source
        .tip_id()
        .await
        .map_err(IngestError::from)?
        .height
        .value();
    let store = PrimaryChainStore::open(
        &loop_config.storage_path,
        ChainStoreOptions::for_network(loop_config.node.network),
    )
    .map_err(IngestError::from)?;
    let store_tip = current_chain_height(&store);
    let phase = classify_phase(
        store_tip,
        upstream_tip,
        loop_config.phases.catchup_threshold_blocks,
    );
    let gap_blocks = i64::from(upstream_tip).saturating_sub(i64::from(store_tip.unwrap_or(0)));
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

#[allow(
    clippy::too_many_lines,
    reason = "the unified-ingest startup composes the load_config, start_api, connect_node, check_schema, recover_state, open_storage, ingest_control, and ready phases in one auditable sequence; splitting them would obscure the failure ordering."
)]
async fn run_ingest(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<(), IngestConfigError> {
    let load_config_phase = StartupPhase::LoadConfig.start();
    let mut command_config = match config::load_ingest_config(config_path, overrides) {
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
    let ops_handle = spawn_ops_endpoint_for(
        ServiceIdentifier::Ingest,
        command_config.ops_listen_addr,
        env!("CARGO_PKG_VERSION"),
        encode_zinder_native_chain_name(command_config.loop_config.node.network),
        readiness.clone(),
        zinder_proto::capabilities::always_on_capability_strings(
            zinder_proto::capabilities::CapabilitySurface::Ingest,
        ),
    );

    let connect_node_phase = StartupPhase::ConnectNode.start();
    let source = match zebra_json_rpc_source_for_target(
        command_config.loop_config.node_source,
        &command_config.loop_config.node,
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
        Ok(_capabilities) => {}
        Err(error) => {
            check_schema_phase.fail(&error);
            start_api_phase.fail(&error);
            return Err(error);
        }
    }
    let network_upgrade_activations = match source
        .discover_network_upgrade_activations("zinder-ingest")
        .await
    {
        Ok(activations) => activations,
        Err(error) => {
            let wrapped: IngestConfigError = IngestError::from(error).into();
            check_schema_phase.fail(&wrapped);
            start_api_phase.fail(&wrapped);
            return Err(wrapped);
        }
    };
    check_schema_phase.complete();

    let recover_state_phase = StartupPhase::RecoverState.start();
    if let Err(error) =
        resolve_wallet_serving_modifiers(&mut command_config, &network_upgrade_activations)
    {
        recover_state_phase.fail(&error);
        start_api_phase.fail(&error);
        return Err(error);
    }
    if let Some(checkpoint_height) = command_config.loop_config.modifiers.checkpoint_height {
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
            event = "ingest_checkpoint_resolved",
            checkpoint_height = checkpoint.height.value(),
            sapling_commitment_tree_size = checkpoint.tip_metadata.sapling_commitment_tree_size,
            orchard_commitment_tree_size = checkpoint.tip_metadata.orchard_commitment_tree_size,
            ironwood_commitment_tree_size = checkpoint.tip_metadata.ironwood_commitment_tree_size,
            "fetched bootstrap checkpoint from upstream node"
        );
        command_config.loop_config.modifiers.checkpoint = Some(checkpoint);
    }
    recover_state_phase.complete();

    let cancel = CancellationToken::new();
    let _signal_handle = cancel_on_terminating_signal(cancel.clone());

    let open_storage_phase = StartupPhase::OpenStorage.start();
    let store_options = ChainStoreOptions {
        rocksdb_resource_budget: command_config.loop_config.canonical_rocksdb_budget,
        raw_blob_retention: command_config.loop_config.raw_blob_policy.to_retention(),
        ..ChainStoreOptions::for_network(command_config.loop_config.node.network)
    };
    let store =
        match PrimaryChainStore::open(&command_config.loop_config.storage_path, store_options) {
            Ok(store) => store,
            Err(error) => {
                let wrapped: IngestConfigError = IngestError::from(error).into();
                open_storage_phase.fail(&wrapped);
                start_api_phase.fail(&wrapped);
                return Err(wrapped);
            }
        };
    let derive_store = match open_primary_derive_store_for_canonical(
        &command_config.loop_config.storage_path,
        command_config.loop_config.derive_rocksdb_budget,
    ) {
        Ok(store) => store,
        Err(error) => {
            let wrapped: IngestConfigError = IngestError::from(error).into();
            open_storage_phase.fail(&wrapped);
            start_api_phase.fail(&wrapped);
            return Err(wrapped);
        }
    };
    if let Err(error) = ensure_spend_projection_not_behind_retention_sweep(&store, &derive_store) {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    let paid_fee_distribution_backfill_context = PaidFeeDistributionBackfillContext::new(
        command_config.loop_config.node.request_timeout,
        Arc::clone(&network_upgrade_activations),
        Arc::new(source.clone()),
        store.clone(),
        derive_store.clone(),
    );
    let value_pool_flow_backfill_context = ValuePoolFlowBackfillContext::new(
        store.clone(),
        derive_store.clone(),
        paid_fee_distribution_backfill_context.clone(),
    );
    if let Err(error) = seed_paid_fee_distribution_cursor_and_tail(
        command_config.paid_fee_distribution_backfill,
        &paid_fee_distribution_backfill_context,
    )
    .await
    {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    if let Err(error) = seed_value_pool_flow_cursor_and_tail(
        command_config.value_pool_flow_backfill,
        &value_pool_flow_backfill_context,
    )
    .await
    {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    if let Err(error) = seed_backfill_owned_consumer_cursors(&store, &derive_store) {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    if let Err(error) = catch_up_derive_store_to_canonical_until_handoff(
        &store,
        &derive_store,
        command_config.loop_config.derive,
    )
    .await
    {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    if let Err(error) = bootstrap_transparent_address_ranking(&store, &derive_store).await {
        let wrapped: IngestConfigError = error.into();
        open_storage_phase.fail(&wrapped);
        start_api_phase.fail(&wrapped);
        return Err(wrapped);
    }
    let derive_tailer_handle = spawn_derive_tailer_task(
        store.clone(),
        derive_store.clone(),
        command_config.loop_config.derive,
        DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
        readiness.clone(),
        cancel.clone(),
    );
    let memory_metrics_handle =
        spawn_runtime_memory_metrics_task(DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL, cancel.clone());
    let derive_replay_budget_metrics_handle = spawn_derive_replay_budget_metrics_task(
        command_config.loop_config.derive,
        DEFAULT_DERIVE_TAILER_POLL_INTERVAL,
        DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL,
        readiness.clone(),
        cancel.clone(),
    );
    let commitment_root_backfill_handle = spawn_commitment_root_backfill_task(
        command_config.loop_config.commitment_root_backfill,
        CommitmentRootBackfillContext::new(
            command_config.loop_config.node.request_timeout,
            Arc::clone(&network_upgrade_activations),
            Arc::new(source.clone()),
            store.clone(),
            derive_store.clone(),
        ),
        readiness.clone(),
        cancel.clone(),
    );
    let block_production_time_backfill_handle = spawn_block_production_time_backfill_task(
        derive_store.clone(),
        readiness.clone(),
        cancel.clone(),
    );
    let transaction_component_backfill_handle = spawn_transaction_component_backfill_task(
        command_config.transaction_component_backfill,
        TransactionComponentBackfillContext::new(store.clone(), derive_store.clone()),
        readiness.clone(),
        cancel.clone(),
    );
    let transaction_history_verifier_handle = spawn_transaction_history_verifier_task(
        command_config.transaction_history_verifier,
        TransactionHistoryVerifierContext::new(store.clone(), derive_store.clone()),
        readiness.clone(),
        cancel.clone(),
    );
    let conventional_fee_distribution_backfill_handle =
        spawn_conventional_fee_distribution_backfill_task(
            command_config.conventional_fee_distribution_backfill,
            ConventionalFeeDistributionBackfillContext::new(store.clone(), derive_store.clone()),
            readiness.clone(),
            cancel.clone(),
        );
    let paid_fee_distribution_backfill_handle = spawn_paid_fee_distribution_backfill_task(
        command_config.paid_fee_distribution_backfill,
        paid_fee_distribution_backfill_context,
        readiness.clone(),
        cancel.clone(),
    );
    let value_pool_flow_backfill_handle = spawn_value_pool_flow_backfill_task(
        command_config.value_pool_flow_backfill,
        value_pool_flow_backfill_context,
        readiness.clone(),
        cancel.clone(),
    );
    let value_pool_balance_backfill_handle = spawn_value_pool_balance_backfill_task(
        command_config.value_pool_balance_backfill,
        ValuePoolBalanceBackfillContext::new(
            command_config.loop_config.node.request_timeout,
            Arc::new(source.clone()),
            store.clone(),
            derive_store.clone(),
        ),
        readiness.clone(),
        cancel.clone(),
    );
    open_storage_phase.complete();

    let mempool_index = MempoolIndex::new();
    let ingest_control_handle = if let Some(listen_addr) = command_config.ingest_control_listen_addr
    {
        Some(
            spawn_ingest_control_endpoint(IngestControlEndpointSpec {
                listen_addr,
                network: command_config.loop_config.node.network,
                store: store.clone(),
                mempool_index: mempool_index.clone(),
                node_source: Some(Arc::new(source.clone())),
                bearer_token: command_config.ingest_control_bearer_token.clone(),
                cancel: cancel.clone(),
            })
            .await?,
        )
    } else {
        tracing::info!(
            target: "zinder::ingest",
            event = "ingest_control_endpoint_disabled",
            "ingest-control endpoint disabled by configuration"
        );
        None
    };
    let _upstream_health_probe_handle = spawn_upstream_health_probe_for(
        &command_config.loop_config.node,
        &source,
        readiness.clone(),
        cancel.clone(),
    );

    start_api_phase.complete();
    StartupPhase::Ready.start().complete();
    readiness.set(ReadinessState::syncing(None, None, None));

    tracing::info!(
        target: "zinder::ingest",
        event = "ingest_loop_started",
        network = encode_zinder_native_chain_name(command_config.loop_config.node.network),
        json_rpc_addr = command_config.loop_config.node.json_rpc_addr.as_str(),
        reorg_window_blocks = command_config.loop_config.reorg_window_blocks,
        catchup_threshold_blocks = command_config.loop_config.phases.catchup_threshold_blocks,
        block_prepare_concurrency = command_config.loop_config.bulk_catchup.block_prepare_concurrency.get(),
        canonical_batch_max_blocks = command_config.loop_config.bulk_catchup.canonical_batch_max_blocks.get(),
        canonical_batch_max_artifact_bytes = command_config.loop_config.bulk_catchup.canonical_batch_max_artifact_bytes.get(),
        canonical_batch_max_estimated_write_bytes = command_config.loop_config.bulk_catchup.canonical_batch_max_estimated_write_bytes.get(),
        canonical_batch_min_blocks_before_estimated_write_close = command_config.loop_config.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close.get(),
        source_segment_max_blocks = command_config.loop_config.bulk_catchup.source_segment_max_blocks.get(),
        source_segment_target_response_bytes = command_config.loop_config.bulk_catchup.source_segment_target_response_bytes.get(),
        source_fetch_max_in_flight_requests = command_config.loop_config.bulk_catchup.source_fetch_max_in_flight_requests.get(),
        source_fetch_max_in_flight_bytes = command_config.loop_config.bulk_catchup.source_fetch_max_in_flight_bytes.get(),
        block_prepare_max_in_flight_artifact_bytes = command_config.loop_config.bulk_catchup.block_prepare_max_in_flight_artifact_bytes.get(),
        commit_reassembly_max_queued_artifact_bytes = command_config.loop_config.bulk_catchup.commit_reassembly_max_queued_artifact_bytes.get(),
        derive_replay_batch_blocks = command_config.loop_config.derive.replay_batch_blocks.get(),
        derive_memory_degrade_ratio = command_config.loop_config.derive.memory_degrade_ratio,
        derive_memory_pause_ratio = command_config.loop_config.derive.memory_pause_ratio,
        derive_memory_resume_ratio = command_config.loop_config.derive.memory_resume_ratio,
        derive_min_replay_batch_blocks = command_config.loop_config.derive.min_replay_batch_blocks.get(),
        commitment_root_backfill_enabled = command_config.loop_config.commitment_root_backfill.enabled,
        commitment_root_backfill_batch_blocks = command_config.loop_config.commitment_root_backfill.batch_blocks.get(),
        commitment_root_backfill_fetch_concurrency = command_config.loop_config.commitment_root_backfill.fetch_concurrency.get(),
        conventional_fee_distribution_backfill_enabled = command_config.conventional_fee_distribution_backfill.enabled,
        conventional_fee_distribution_backfill_batch_blocks = command_config.conventional_fee_distribution_backfill.batch_blocks.get(),
        paid_fee_distribution_backfill_enabled = command_config.paid_fee_distribution_backfill.enabled,
        paid_fee_distribution_backfill_batch_blocks = command_config.paid_fee_distribution_backfill.batch_blocks.get(),
        paid_fee_distribution_backfill_fetch_concurrency = command_config.paid_fee_distribution_backfill.fetch_concurrency.get(),
        paid_fee_distribution_backfill_history_days = command_config.paid_fee_distribution_backfill.history_days.get(),
        paid_fee_distribution_backfill_timestamp_safety_seconds = command_config.paid_fee_distribution_backfill.timestamp_safety_seconds,
        transaction_component_backfill_enabled = command_config.transaction_component_backfill.enabled,
        transaction_component_backfill_batch_blocks = command_config.transaction_component_backfill.batch_blocks.get(),
        transaction_history_verifier_enabled = command_config.transaction_history_verifier.enabled,
        transaction_history_verifier_batch_blocks = command_config.transaction_history_verifier.batch_blocks.get(),
        value_pool_flow_backfill_enabled = command_config.value_pool_flow_backfill.enabled,
        value_pool_flow_backfill_batch_blocks = command_config.value_pool_flow_backfill.batch_blocks.get(),
        value_pool_flow_backfill_fetch_concurrency = command_config.value_pool_flow_backfill.fetch_concurrency.get(),
        value_pool_balance_backfill_enabled = command_config.value_pool_balance_backfill.enabled,
        value_pool_balance_backfill_batch_blocks = command_config.value_pool_balance_backfill.batch_blocks.get(),
        value_pool_balance_backfill_fetch_concurrency = command_config.value_pool_balance_backfill.fetch_concurrency.get(),
        poll_interval_ms = u64::try_from(
            command_config.loop_config.tip_follow.poll_interval.as_millis()
        )
        .unwrap_or(u64::MAX),
        lag_threshold_blocks = command_config.loop_config.tip_follow.lag_threshold_blocks,
        ingest_control_listen_addr = ?command_config.ingest_control_listen_addr,
        "unified ingest loop started"
    );

    let launcher = build_tip_follow_subsystems_launcher(
        command_config.loop_config.node.clone(),
        source.clone(),
        store.clone(),
        derive_store.clone(),
        mempool_index,
        readiness.clone(),
        command_config.chain_event_retention(),
        command_config.mempool_event_retention(),
        cancel.clone(),
    );

    let loop_outcome = run_ingest_loop(
        &command_config.loop_config,
        network_upgrade_activations,
        Arc::new(source),
        store.clone(),
        &readiness,
        cancel.clone(),
        Some(launcher),
    )
    .await;

    let final_result = handle_loop_outcome(loop_outcome, &readiness, &cancel).await;
    cancel.cancel();
    if let Err(join_error) = derive_tailer_handle.await {
        tracing::warn!(
            target: "zinder::ingest",
            event = "derive_tailer_join_failed",
            error = %join_error,
            "derive tailer task failed during shutdown"
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
    if let Err(join_error) = derive_replay_budget_metrics_handle.await {
        tracing::warn!(
            target: "zinder::ingest",
            event = "derive_replay_budget_metrics_join_failed",
            error = %join_error,
            "derive replay budget metrics task failed during shutdown"
        );
    }
    if let Some(handle) = commitment_root_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "commitment_root_backfill_join_failed",
            error = %join_error,
            "commitment-root backfill task failed during shutdown"
        );
    }
    if let Err(join_error) = block_production_time_backfill_handle.await {
        tracing::warn!(
            target: "zinder::ingest",
            event = "block_production_time_backfill_join_failed",
            error = %join_error,
            "block-production time backfill task failed during shutdown"
        );
    }
    if let Some(handle) = conventional_fee_distribution_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "conventional_fee_distribution_backfill_join_failed",
            error = %join_error,
            "conventional-fee distribution backfill task failed during shutdown"
        );
    }
    if let Some(handle) = paid_fee_distribution_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "paid_fee_distribution_backfill_join_failed",
            error = %join_error,
            "paid-fee distribution backfill task failed during shutdown"
        );
    }
    if let Some(handle) = transaction_component_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "transaction_component_backfill_join_failed",
            error = %join_error,
            "transaction-component backfill task failed during shutdown"
        );
    }
    if let Some(handle) = transaction_history_verifier_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "transaction_history_verifier_join_failed",
            error = %join_error,
            "transaction-history verifier task failed during shutdown"
        );
    }
    if let Some(handle) = value_pool_flow_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "value_pool_flow_backfill_join_failed",
            error = %join_error,
            "value-pool flow backfill task failed during shutdown"
        );
    }
    if let Some(handle) = value_pool_balance_backfill_handle
        && let Err(join_error) = handle.await
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "value_pool_balance_backfill_join_failed",
            error = %join_error,
            "value-pool balance backfill task failed during shutdown"
        );
    }

    tracing::info!(
        target: "zinder::ingest",
        event = "ingest_loop_stopped",
        "unified ingest loop stopped"
    );

    if let Some(handle) = ops_handle {
        handle.shutdown().await;
    }
    if let Some(handle) = ingest_control_handle {
        handle.shutdown().await;
    }

    final_result
}

async fn handle_loop_outcome(
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
                event = "ingest_loop_reorg_window_exceeded",
                from_height = from_height.value(),
                replacement_depth,
                configured_window_blocks,
                "ingest reorg replacement crossed the configured reorg window; readiness drained for operator review"
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

#[allow(
    clippy::too_many_arguments,
    reason = "tip-follow subsystems capture the source, store, mempool index, readiness handle, two retention configs, and the cancel token; bundling them into a struct adds indirection without changing the binding count the binary must make."
)]
fn build_tip_follow_subsystems_launcher(
    node_target: NodeTarget,
    source: ZebraJsonRpcSource,
    store: PrimaryChainStore,
    derive_store: zinder_derive::DeriveStore,
    mempool_index: MempoolIndex,
    readiness: Readiness,
    chain_event_retention: zinder_ingest::ChainEventRetentionConfig,
    mempool_event_retention: zinder_ingest::MempoolEventRetentionWorkerConfig,
    cancel: CancellationToken,
) -> TipFollowSubsystemsLauncher {
    Box::new(move || {
        let retention_handle = spawn_chain_event_retention_task(
            store.clone(),
            readiness.clone(),
            chain_event_retention,
            cancel.clone(),
        );
        let mempool_retention_handle = spawn_mempool_event_retention_task(
            store.clone(),
            readiness.clone(),
            mempool_event_retention,
            cancel.clone(),
        );
        let mempool_source = build_mempool_source(&node_target, &source);
        let chain_tip_source = build_chain_tip_notification_source(&node_target);
        let (mempool_ready_signal, mempool_ready_gate) = mempool_ready_channel();
        let mempool_handle = spawn_mempool_orchestrator(
            mempool_source,
            store.clone(),
            derive_store.clone(),
            mempool_index,
            readiness.clone(),
            mempool_ready_signal,
            cancel.clone(),
        );

        TipFollowSubsystems {
            mempool_ready_gate: Some(mempool_ready_gate),
            chain_tip_source,
            spawned_tasks: vec![retention_handle, mempool_retention_handle, mempool_handle],
        }
    })
}

fn resolve_wallet_serving_modifiers(
    command_config: &mut IngestCommandConfig,
    activations: &NetworkUpgradeActivations,
) -> Result<(), IngestConfigError> {
    if !matches!(command_config.coverage, IngestCoverage::WalletServing) {
        return Ok(());
    }

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
                reason: "wallet-serving bulk_catchup floor cannot be the genesis block",
            })
            .into(),
        );
    }

    let checkpoint_height = BlockHeight::new(wallet_serving_floor.value().saturating_sub(1));
    command_config.loop_config.modifiers = IngestModifiers {
        target_height: command_config.loop_config.modifiers.target_height,
        checkpoint_height: Some(checkpoint_height),
        allow_near_tip_finalize: command_config.loop_config.modifiers.allow_near_tip_finalize,
        checkpoint: command_config.loop_config.modifiers.checkpoint,
    };
    tracing::info!(
        target: "zinder::ingest",
        event = "wallet_serving_modifiers_resolved",
        from_height = wallet_serving_floor.value(),
        checkpoint_height = checkpoint_height.value(),
        earliest_upgrade = %earliest.name,
        "resolved wallet-serving floor from node activation heights"
    );

    Ok(())
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

const MEMPOOL_HYDRATION_LAGGING_THRESHOLD: u64 = 5;

#[allow(
    clippy::too_many_arguments,
    reason = "spawn_mempool_orchestrator threads the source, store, live index, readiness, prime signal, and cancel through the orchestrator's spawn loop; bundling them into a struct adds indirection without changing the binding count callers must make."
)]
#[must_use = "drop the handle to detach the orchestrator or await it for symmetric shutdown"]
fn spawn_mempool_orchestrator(
    mempool_source: Arc<dyn MempoolSource>,
    store: PrimaryChainStore,
    derive_store: zinder_derive::DeriveStore,
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
                derive_store.clone(),
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
    let canonical_store = PrimaryChainStore::open(
        &backup_config.storage_path,
        ChainStoreOptions {
            rocksdb_resource_budget: backup_config.canonical_rocksdb_budget,
            ..ChainStoreOptions::for_network(backup_config.network)
        },
    )
    .map_err(IngestError::from)?;
    let started_at = Instant::now();
    let backup_outcome = create_backup_checkpoints(&backup_config, &canonical_store)
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

fn create_backup_checkpoints(
    backup_config: &config::BackupCommandConfig,
    canonical_store: &PrimaryChainStore,
) -> Result<(), IngestError> {
    let derive_storage_path =
        zinder_derive::DeriveStore::path_for_canonical(&backup_config.storage_path);
    if !derive_storage_path.exists() {
        return Err(IngestError::DeriveStoreMissing {
            path: derive_storage_path,
        });
    }
    let derive_store = zinder_derive::DeriveStore::open(
        &derive_storage_path,
        zinder_derive::DeriveStoreOptions {
            consumers: zinder_derive::DeriveStore::bundled_consumers(),
            rocksdb_resource_budget: backup_config.derive_rocksdb_budget,
            ..zinder_derive::DeriveStoreOptions::default()
        },
    )?;
    let derive_checkpoint_staging_path = derive_checkpoint_staging_path(&backup_config.to_path);
    let derive_checkpoint_path =
        zinder_derive::DeriveStore::path_for_canonical(&backup_config.to_path);

    if derive_checkpoint_staging_path.exists() {
        return Err(IngestError::BackupDeriveCheckpointStagingExists {
            path: derive_checkpoint_staging_path,
        });
    }
    derive_store.create_checkpoint(&derive_checkpoint_staging_path)?;
    canonical_store.create_checkpoint(&backup_config.to_path)?;
    fs::rename(&derive_checkpoint_staging_path, &derive_checkpoint_path).map_err(|source| {
        IngestError::BackupDeriveCheckpointInstall {
            from_path: derive_checkpoint_staging_path,
            to_path: derive_checkpoint_path,
            source,
        }
    })?;

    Ok(())
}

fn derive_checkpoint_staging_path(checkpoint_path: &Path) -> PathBuf {
    let mut extension = checkpoint_path
        .extension()
        .map_or_else(|| OsString::from("derive"), OsString::from);
    if checkpoint_path.extension().is_some() {
        extension.push(".derive");
    }
    let mut staging_path = checkpoint_path.to_path_buf();
    staging_path.set_extension(extension);
    staging_path
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

fn print_ingest_config(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<String, IngestConfigError> {
    let command_config = config::load_ingest_config(config_path, overrides)?;
    config::redacted_ingest_config_toml(&command_config)
}

fn print_backup_config(
    config_path: Option<PathBuf>,
    args: BackupArgs,
) -> Result<String, IngestConfigError> {
    let backup_config = config::load_backup_config(config_path, args.into())?;
    config::redacted_backup_config_toml(&backup_config)
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
    fn ingest_capability_validation_accepts_missing_tree_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = NodeCapabilities::new([
            NodeCapability::JsonRpc,
            NodeCapability::BestChainBlocks,
            NodeCapability::TipId,
            NodeCapability::SubtreeRoots,
        ])?;

        require_ingest_node_capabilities(capabilities)?;

        Ok(())
    }
}
