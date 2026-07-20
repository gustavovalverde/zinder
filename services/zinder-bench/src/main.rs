//! Command-line entry point for the fixed-range benchmark harness.

use std::{
    fs::OpenOptions,
    io::Write,
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use zinder_bench::{
    BenchError,
    canonical_fixture_replay::capture_canonical_fixture_replay_plan,
    canonical_fixture_transport_server::{
        CanonicalFixtureTransportServerConfig, run_canonical_fixture_transport_server,
    },
    capture::{CaptureConfig, capture_fixed_range},
    fixture::FixtureManifest,
    recorder::install_recorder,
    replay::{ProjectionReplayScope, ReplayConfig, replay_fixture},
    report::{AcceptanceThresholds, BenchmarkReport, FixtureCachePolicy},
};
use zinder_core::{
    BlockHeight, Network, UnixTimestampMillis, wire::decode_zinder_native_chain_name,
};
use zinder_materialized_views::ProjectionPreset;
use zinder_source::{CookieSource, NodeAuth, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions};

#[path = "canonical_replay_storage/command.rs"]
mod canonical_replay_storage_command;
mod rocksdb_canonical_fixture_replay;
mod rocksdb_compact_block_range;
mod rocksdb_storage_lifecycle;
mod rocksdb_wallet_rebuild;

use canonical_replay_storage_command::{CanonicalReplayStorageArgs, run_canonical_replay_storage};
use rocksdb_canonical_fixture_replay::{
    RocksDbCanonicalFixtureReplayArgs, run_rocksdb_canonical_fixture_replay,
};
use rocksdb_compact_block_range::{RocksDbCompactBlockRangeArgs, run_rocksdb_compact_block_range};
use rocksdb_storage_lifecycle::{RocksDbStorageLifecycleArgs, run_rocksdb_storage_lifecycle};
use rocksdb_wallet_rebuild::{RocksDbWalletRebuildArgs, run_rocksdb_wallet_rebuild};

const DEFAULT_FROM_HEIGHT: u32 = 150_000;
const DEFAULT_TO_HEIGHT: u32 = 200_000;
const DEFAULT_SEGMENT_BLOCKS: u32 = 1_000;
const DEFAULT_FETCH_CONCURRENCY: u32 = 16;
const DEFAULT_CAPTURE_PREPARE_CONCURRENCY: u32 = 10;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_BLOCK_PREPARE_CONCURRENCY: u32 = 16;

#[derive(Parser)]
#[command(name = "zinder-bench")]
#[command(about = "Zinder fixed-range storage benchmark harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture raw source payloads for a block range into a fixture directory.
    Capture(CaptureArgs),
    /// Capture predecessor and fixed-tip checkpoints for canonical fixture replay.
    #[command(name = "capture-canonical-fixture-checkpoints")]
    CaptureCanonicalFixtureCheckpoints(CaptureCanonicalFixtureCheckpointsArgs),
    /// Replay the current projection-coupled schema over a captured fixture.
    ProjectionCoupledReplay(ProjectionCoupledReplayArgs),
    /// Persist and read back backend-neutral canonical replay records.
    #[command(name = "canonical-replay-storage")]
    CanonicalReplayStorage(CanonicalReplayStorageArgs),
    /// Replay an authenticated fixture into a fresh canonical `RocksDB` store.
    #[command(name = "rocksdb-canonical-fixture-replay")]
    RocksDbCanonicalFixtureReplay(RocksDbCanonicalFixtureReplayArgs),
    /// Serve an immutable canonical fixture through JSON-RPC and indexer gRPC.
    #[command(name = "serve-canonical-fixture-transports")]
    ServeCanonicalFixtureTransports(ServeCanonicalFixtureTransportsArgs),
    /// Build and cold-admit complete canonical and wallet stores.
    #[command(name = "rocksdb-storage-lifecycle")]
    RocksDbStorageLifecycle(RocksDbStorageLifecycleArgs),
    /// Rebuild and cold-admit a wallet store from an existing READY canonical store.
    #[command(name = "rocksdb-wallet-rebuild")]
    RocksDbWalletRebuild(RocksDbWalletRebuildArgs),
    /// Measure version-1 compact-block ranges through an admitted secondary.
    #[command(name = "rocksdb-compact-block-range")]
    RocksDbCompactBlockRange(RocksDbCompactBlockRangeArgs),
}

#[derive(Args)]
struct ServeCanonicalFixtureTransportsArgs {
    /// Directory containing the immutable fixture manifest and segments.
    #[arg(long)]
    fixture: PathBuf,
    /// Local JSON-RPC listener for the current batched source.
    #[arg(long, default_value = "127.0.0.1:19432")]
    json_rpc_listen_addr: SocketAddr,
    /// Local Zebra indexer gRPC listener for unary `GetBlock`.
    #[arg(long, default_value = "127.0.0.1:19430")]
    indexer_grpc_listen_addr: SocketAddr,
    /// Fixed delay per JSON request or batch and per unary gRPC call.
    #[arg(long, default_value_t = 0)]
    response_delay_millis: u64,
    /// Maximum JSON-RPC or protobuf response bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    max_response_bytes: u64,
    /// Write the shutdown report to this path instead of stdout.
    #[arg(long)]
    report: Option<PathBuf>,
}

#[derive(Args)]
struct CaptureArgs {
    /// Network name, such as zcash-mainnet.
    #[arg(long)]
    network: String,
    /// Zebra JSON-RPC base URL.
    #[arg(long = "json-rpc-addr")]
    json_rpc_addr: String,
    /// Optional node cookie file path.
    #[arg(long = "node-auth-cookie")]
    node_auth_cookie: Option<PathBuf>,
    /// First block height to capture.
    #[arg(long = "from-height", default_value_t = DEFAULT_FROM_HEIGHT)]
    from_height: u32,
    /// Last block height to capture.
    #[arg(long = "to-height", default_value_t = DEFAULT_TO_HEIGHT)]
    to_height: u32,
    /// Blocks written per segment file.
    #[arg(long = "segment-blocks", default_value_t = DEFAULT_SEGMENT_BLOCKS)]
    segment_blocks: u32,
    /// Concurrent block fetches issued against the node.
    #[arg(long = "fetch-concurrency", default_value_t = DEFAULT_FETCH_CONCURRENCY)]
    fetch_concurrency: u32,
    /// Concurrent block-local preparations used to measure captured blocks.
    #[arg(
        long = "prepare-concurrency",
        default_value_t = DEFAULT_CAPTURE_PREPARE_CONCURRENCY
    )]
    prepare_concurrency: u32,
    /// Per-request source timeout in seconds.
    #[arg(long = "request-timeout-secs", default_value_t = DEFAULT_REQUEST_TIMEOUT_SECS)]
    request_timeout_secs: u64,
    /// Maximum JSON-RPC response body size in bytes.
    #[arg(long = "max-response-bytes", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    max_response_bytes: u64,
    /// Destination fixture directory.
    #[arg(long = "out")]
    out: PathBuf,
}

#[derive(Args)]
struct CaptureCanonicalFixtureCheckpointsArgs {
    /// Existing captured fixture directory to augment.
    #[arg(long)]
    fixture: PathBuf,
    /// Network name, such as zcash-mainnet.
    #[arg(long)]
    network: String,
    /// Zebra JSON-RPC base URL.
    #[arg(long = "json-rpc-addr")]
    json_rpc_addr: String,
    /// Optional node cookie file path.
    #[arg(long = "node-auth-cookie")]
    node_auth_cookie: Option<PathBuf>,
    /// Per-request source timeout in seconds.
    #[arg(long = "request-timeout-secs", default_value_t = DEFAULT_REQUEST_TIMEOUT_SECS)]
    request_timeout_secs: u64,
    /// Maximum JSON-RPC response body size in bytes.
    #[arg(long = "max-response-bytes", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    max_response_bytes: u64,
}

#[derive(Args)]
struct ProjectionCoupledReplayArgs {
    /// Captured fixture directory.
    #[arg(long)]
    fixture: PathBuf,
    /// Writable clone of the captured start-state canonical store.
    #[arg(long)]
    store: PathBuf,
    /// Prepare concurrency to run with.
    #[arg(long = "block-prepare-concurrency", default_value_t = DEFAULT_BLOCK_PREPARE_CONCURRENCY)]
    block_prepare_concurrency: u32,
    /// Maximum accepted source-segment response size in bytes.
    #[arg(long = "max-response-bytes")]
    max_response_bytes: Option<u64>,
    /// Maximum connected blocks requested in one source segment.
    #[arg(long = "source-segment-max-blocks")]
    source_segment_max_blocks: Option<u32>,
    /// Adaptive target size for one source-segment response in bytes.
    #[arg(long = "source-segment-target-response-bytes")]
    source_segment_target_response_bytes: Option<u64>,
    /// Maximum concurrent source-segment requests.
    #[arg(long = "source-fetch-max-in-flight-requests")]
    source_fetch_max_in_flight_requests: Option<u32>,
    /// Aggregate byte watermark for concurrent source-segment responses.
    #[arg(long = "source-fetch-max-in-flight-bytes")]
    source_fetch_max_in_flight_bytes: Option<u64>,
    /// Aggregate byte watermark for canonical block preparation.
    #[arg(long = "block-prepare-memory-watermark-bytes")]
    block_prepare_memory_watermark_bytes: Option<u64>,
    /// Deterministic delay applied to every captured source-segment response.
    #[arg(long = "source-segment-delay-millis", default_value_t = 0)]
    source_segment_delay_millis: u64,
    /// Optional canonical block-cache override in bytes.
    #[arg(long = "block-cache-bytes")]
    block_cache_bytes: Option<u64>,
    /// Projection preset to replay after canonical ingest. `explorer` is a
    /// diagnostic replay, not projection-readiness certification. Omit for a
    /// canonical-only run.
    #[arg(long = "projection-preset")]
    projection_preset: Option<CliProjectionPreset>,
    /// Projection history to replay. Fixed-range seeds fresh projection
    /// cursors at the cloned canonical tip; retained-history rebuilds all
    /// retained events.
    #[arg(
        long = "projection-replay-scope",
        value_enum,
        default_value_t = CliProjectionReplayScope::FixedRange
    )]
    projection_replay_scope: CliProjectionReplayScope,
    /// Write the JSON report to this path instead of stdout.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Source revision of the measured binary (commit SHA or image digest).
    #[arg(long = "software-revision")]
    software_revision: Option<String>,
    /// Campaign trial identity; requires `--fixture-cache-policy`.
    #[arg(long = "trial-id")]
    trial_id: Option<String>,
    /// Controlled fixture-cache treatment; requires `--trial-id`.
    #[arg(long = "fixture-cache-policy", value_enum)]
    fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Stable operator label for the runner; resource facts are separate flags.
    #[arg(long = "runner-id")]
    runner_id: Option<String>,
    /// CPU limit applied to the benchmark container, in logical cores.
    #[arg(long = "cpu-limit-cores")]
    cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the benchmark container, in bytes.
    #[arg(long = "memory-limit-bytes")]
    memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    #[arg(long = "storage-class")]
    storage_class: Option<String>,
    /// Immutable container image reference for the measured binary.
    #[arg(long = "image-reference")]
    image_reference: Option<String>,
    /// Desired canonical fixture replay time, in seconds.
    #[arg(long = "canonical-fixture-replay-target-secs")]
    canonical_fixture_replay_target_secs: Option<f64>,
    /// Maximum accepted canonical fixture replay time, in seconds.
    #[arg(long = "canonical-fixture-replay-hard-limit-secs")]
    canonical_fixture_replay_hard_limit_secs: Option<f64>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliProjectionPreset {
    Wallet,
    Explorer,
}

impl From<CliProjectionPreset> for ProjectionPreset {
    fn from(preset: CliProjectionPreset) -> Self {
        match preset {
            CliProjectionPreset::Wallet => Self::Wallet,
            CliProjectionPreset::Explorer => Self::Explorer,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum CliProjectionReplayScope {
    #[default]
    FixedRange,
    RetainedHistory,
}

impl From<CliProjectionReplayScope> for ProjectionReplayScope {
    fn from(scope: CliProjectionReplayScope) -> Self {
        match scope {
            CliProjectionReplayScope::FixedRange => Self::FixedRange,
            CliProjectionReplayScope::RetainedHistory => Self::RetainedHistory,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(
                target: "zinder::bench",
                event = "bench_run_failed",
                error = %error,
                "benchmark run failed"
            );
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), BenchError> {
    match cli.command {
        Command::Capture(args) => run_capture(args).await,
        Command::CaptureCanonicalFixtureCheckpoints(args) => {
            run_capture_canonical_fixture_checkpoints(args).await
        }
        Command::ProjectionCoupledReplay(args) => run_projection_coupled_replay(args).await,
        Command::CanonicalReplayStorage(args) => {
            let output = run_canonical_replay_storage(args).await?;
            emit_report(&output.report, output.report_path.as_deref())?;
            output.report.validate()
        }
        Command::RocksDbCanonicalFixtureReplay(args) => {
            let output = run_rocksdb_canonical_fixture_replay(args).await?;
            emit_report(&output.report, output.report_path.as_deref())?;
            output.report.validate()
        }
        Command::ServeCanonicalFixtureTransports(args) => {
            let max_response_bytes = u32::try_from(args.max_response_bytes).map_err(|_| {
                BenchError::invalid_argument("--max-response-bytes must fit in u32")
            })?;
            let report =
                run_canonical_fixture_transport_server(CanonicalFixtureTransportServerConfig {
                    fixture_directory: args.fixture,
                    json_rpc_listen_addr: args.json_rpc_listen_addr,
                    indexer_grpc_listen_addr: args.indexer_grpc_listen_addr,
                    response_delay: Duration::from_millis(args.response_delay_millis),
                    max_response_bytes,
                })
                .await?;
            let encoded = serde_json::to_vec_pretty(&report)?;
            if let Some(path) = args.report.as_deref() {
                create_report_file(path, &encoded)?;
            } else {
                write_report_to_stdout(&encoded);
            }
            Ok(())
        }
        Command::RocksDbStorageLifecycle(args) => {
            let output = run_rocksdb_storage_lifecycle(args).await?;
            emit_report(&output.report, output.report_path.as_deref())?;
            output.report.validate()
        }
        Command::RocksDbWalletRebuild(args) => {
            let output = run_rocksdb_wallet_rebuild(args).await?;
            let encoded = serde_json::to_vec_pretty(&output)?;
            write_report_to_stdout(&encoded);
            Ok(())
        }
        Command::RocksDbCompactBlockRange(args) => {
            let output = run_rocksdb_compact_block_range(args)?;
            emit_report(&output.report, output.report_path.as_deref())
        }
    }
}

async fn run_capture_canonical_fixture_checkpoints(
    args: CaptureCanonicalFixtureCheckpointsArgs,
) -> Result<(), BenchError> {
    let network = parse_network(&args.network)?;
    let manifest = FixtureManifest::read(&args.fixture)?;
    if manifest.network_typed()? != network {
        return Err(BenchError::invalid_argument(
            "--network does not match the captured fixture network",
        ));
    }
    let node_auth = args.node_auth_cookie.map_or(NodeAuth::None, |path| {
        NodeAuth::Cookie(CookieSource::File(path))
    });
    let source = ZebraJsonRpcSource::with_options(
        network,
        &args.json_rpc_addr,
        node_auth,
        ZebraJsonRpcSourceOptions {
            request_timeout: Duration::from_secs(args.request_timeout_secs),
            max_response_bytes: require_nonzero_u64(args.max_response_bytes, "max-response-bytes")?,
            broadcast_timeout: None,
        },
    )?;
    let activations = source
        .discover_network_upgrade_activations("zinder-bench")
        .await?;
    let replay_plan =
        capture_canonical_fixture_replay_plan(&args.fixture, &source, &activations).await?;
    tracing::info!(
        target: "zinder::bench",
        event = "canonical_fixture_checkpoints_captured",
        fixture_manifest_sha256 = replay_plan.fixture_manifest_sha256,
        predecessor_height = replay_plan.history_predecessor.block_id.height,
        source_tip_height = replay_plan.source_tip_checkpoint.block_id.height,
        "canonical fixture checkpoints captured"
    );
    Ok(())
}

async fn run_capture(args: CaptureArgs) -> Result<(), BenchError> {
    let network = parse_network(&args.network)?;
    let node_auth = args.node_auth_cookie.map_or(NodeAuth::None, |path| {
        NodeAuth::Cookie(CookieSource::File(path))
    });
    let config = CaptureConfig {
        network,
        json_rpc_addr: args.json_rpc_addr,
        node_auth,
        from_height: BlockHeight::new(args.from_height),
        to_height: BlockHeight::new(args.to_height),
        segment_blocks: require_nonzero_u32(args.segment_blocks, "segment-blocks")?,
        fetch_concurrency: require_nonzero_u32(args.fetch_concurrency, "fetch-concurrency")?,
        prepare_concurrency: require_nonzero_u32(args.prepare_concurrency, "prepare-concurrency")?,
        request_timeout: Duration::from_secs(args.request_timeout_secs),
        max_response_bytes: require_nonzero_u64(args.max_response_bytes, "max-response-bytes")?,
        output_directory: args.out,
    };
    let manifest = capture_fixed_range(config).await?;
    tracing::info!(
        target: "zinder::bench",
        event = "capture_complete",
        from_height = manifest.from_height,
        to_height = manifest.to_height,
        block_count = manifest.block_count,
        transaction_count = manifest.workload_density.transaction_count,
        transparent_input_count = manifest.workload_density.transparent_input_count,
        transparent_output_count = manifest.workload_density.transparent_output_count,
        segment_count = manifest.segments.len(),
        "capture complete"
    );
    Ok(())
}

async fn run_projection_coupled_replay(
    args: ProjectionCoupledReplayArgs,
) -> Result<(), BenchError> {
    let run_started_at_unix_millis = UnixTimestampMillis::now().value();
    let canonical_fixture_replay_thresholds = canonical_fixture_replay_thresholds(&args)?;
    let metrics_handle = install_recorder()?;
    let config = ReplayConfig {
        fixture_directory: args.fixture,
        store_path: args.store,
        block_prepare_concurrency: require_nonzero_u32(
            args.block_prepare_concurrency,
            "block-prepare-concurrency",
        )?,
        max_response_bytes: args
            .max_response_bytes
            .map(|bytes| require_nonzero_u64(bytes, "max-response-bytes"))
            .transpose()?,
        source_segment_max_blocks: args
            .source_segment_max_blocks
            .map(|blocks| require_nonzero_u32(blocks, "source-segment-max-blocks"))
            .transpose()?,
        source_segment_target_response_bytes: args
            .source_segment_target_response_bytes
            .map(|bytes| require_nonzero_u64(bytes, "source-segment-target-response-bytes"))
            .transpose()?,
        source_fetch_max_in_flight_requests: args
            .source_fetch_max_in_flight_requests
            .map(|requests| require_nonzero_u32(requests, "source-fetch-max-in-flight-requests"))
            .transpose()?,
        source_fetch_max_in_flight_bytes: args
            .source_fetch_max_in_flight_bytes
            .map(|bytes| require_nonzero_u64(bytes, "source-fetch-max-in-flight-bytes"))
            .transpose()?,
        block_prepare_memory_watermark_bytes: args
            .block_prepare_memory_watermark_bytes
            .map(|bytes| require_nonzero_u64(bytes, "block-prepare-memory-watermark-bytes"))
            .transpose()?,
        source_segment_delay_millis: args.source_segment_delay_millis,
        canonical_block_cache_bytes: args.block_cache_bytes,
        projection_preset: args.projection_preset.map(ProjectionPreset::from),
        projection_replay_scope: args.projection_replay_scope.into(),
        software_revision: args.software_revision,
        trial_id: args.trial_id,
        fixture_cache_policy: args.fixture_cache_policy,
        run_started_at_unix_millis,
        runner_id: args.runner_id,
        cpu_limit_cores: args.cpu_limit_cores,
        memory_limit_bytes: args.memory_limit_bytes,
        storage_class: args.storage_class,
        image_reference: args.image_reference,
        canonical_fixture_replay_thresholds,
    };
    let report = BenchmarkReport::from(replay_fixture(config, Some(metrics_handle)).await?);
    emit_report(&report, args.report.as_deref())?;
    report.validate()?;
    Ok(())
}

fn canonical_fixture_replay_thresholds(
    args: &ProjectionCoupledReplayArgs,
) -> Result<Option<AcceptanceThresholds>, BenchError> {
    let thresholds = match (
        args.canonical_fixture_replay_target_secs,
        args.canonical_fixture_replay_hard_limit_secs,
    ) {
        (None, None) => Ok(None),
        (Some(target_seconds), Some(hard_limit_seconds)) => {
            AcceptanceThresholds::try_from_seconds(target_seconds, hard_limit_seconds).map(Some)
        }
        _ => Err(BenchError::invalid_argument(
            "--canonical-fixture-replay-target-secs and --canonical-fixture-replay-hard-limit-secs must be supplied together",
        )),
    }?;
    Ok(thresholds)
}

fn emit_report(
    report: &impl serde::Serialize,
    report_path: Option<&std::path::Path>,
) -> Result<(), BenchError> {
    let encoded = serde_json::to_vec_pretty(report)?;
    if let Some(path) = report_path {
        create_report_file(path, &encoded)?;
    } else {
        write_report_to_stdout(&encoded);
    }
    Ok(())
}

fn create_report_file(path: &std::path::Path, encoded: &[u8]) -> Result<(), BenchError> {
    let mut report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| BenchError::io(path, source))?;
    report_file
        .write_all(encoded)
        .map_err(|source| BenchError::io(path, source))?;
    report_file
        .sync_all()
        .map_err(|source| BenchError::io(path, source))
}

#[allow(
    clippy::print_stdout,
    reason = "the JSON report is the tool's structured output stream"
)]
fn write_report_to_stdout(encoded: &[u8]) {
    let rendered = String::from_utf8_lossy(encoded);
    println!("{rendered}");
}

fn parse_network(name: &str) -> Result<Network, BenchError> {
    decode_zinder_native_chain_name(name)
        .map_err(|source| BenchError::invalid_argument(source.to_string()))
}

fn require_nonzero_u32(candidate: u32, flag: &str) -> Result<NonZeroU32, BenchError> {
    NonZeroU32::new(candidate)
        .ok_or_else(|| BenchError::invalid_argument(format!("--{flag} must be greater than zero")))
}

fn require_nonzero_u64(candidate: u64, flag: &str) -> Result<NonZeroU64, BenchError> {
    NonZeroU64::new(candidate)
        .ok_or_else(|| BenchError::invalid_argument(format!("--{flag} must be greater than zero")))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use clap::Parser;
    use tempfile::tempdir;

    use super::{Cli, Command, canonical_fixture_replay_thresholds, create_report_file};

    fn projection_coupled_replay_args(extra: &[&str]) -> Vec<String> {
        [
            "zinder-bench",
            "projection-coupled-replay",
            "--fixture",
            "fixture",
            "--store",
            "store",
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn removed_target_plane_and_old_canonical_flags_are_rejected() {
        for removed_flag in [
            "--canonical-build-target-secs",
            "--canonical-build-hard-limit-secs",
            "--wallet-build-target-secs",
            "--wallet-build-hard-limit-secs",
            "--wallet-build-lifecycle-target-secs",
            "--wallet-build-lifecycle-hard-limit-secs",
        ] {
            assert!(
                Cli::try_parse_from(projection_coupled_replay_args(&[removed_flag, "10"])).is_err()
            );
        }
    }

    #[test]
    fn ambiguous_legacy_replay_command_is_rejected() {
        assert!(
            Cli::try_parse_from([
                "zinder-bench",
                "replay",
                "--fixture",
                "fixture",
                "--store",
                "store",
            ])
            .is_err()
        );
    }

    #[test]
    fn rocksdb_storage_lifecycle_command_spelling_is_stable() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "rocksdb-storage-lifecycle",
            "--network",
            "zcash-testnet",
            "--json-rpc-addr",
            "http://zebra:18232",
            "--canonical-store",
            "canonical",
            "--wallet-store",
            "wallet",
            "--cpu-limit-cores",
            "10",
            "--memory-limit-bytes",
            "10737418240",
        ])?;

        assert!(matches!(cli.command, Command::RocksDbStorageLifecycle(_)));
        Ok(())
    }

    #[test]
    fn rocksdb_canonical_fixture_replay_command_spelling_is_stable() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "rocksdb-canonical-fixture-replay",
            "--fixture",
            "fixture",
            "--canonical-store",
            "canonical",
        ])?;

        assert!(matches!(
            cli.command,
            Command::RocksDbCanonicalFixtureReplay(_)
        ));
        Ok(())
    }

    #[test]
    fn capture_canonical_fixture_checkpoints_command_spelling_is_stable()
    -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "capture-canonical-fixture-checkpoints",
            "--fixture",
            "fixture",
            "--network",
            "zcash-mainnet",
            "--json-rpc-addr",
            "http://zebra:8232",
        ])?;

        assert!(matches!(
            cli.command,
            Command::CaptureCanonicalFixtureCheckpoints(_)
        ));
        Ok(())
    }

    #[test]
    fn rocksdb_wallet_rebuild_command_spelling_is_stable() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "rocksdb-wallet-rebuild",
            "--network",
            "zcash-testnet",
            "--json-rpc-addr",
            "http://zebra:18232",
            "--canonical-store",
            "canonical",
            "--wallet-store",
            "wallet",
        ])?;

        assert!(matches!(cli.command, Command::RocksDbWalletRebuild(_)));
        Ok(())
    }

    #[test]
    fn rocksdb_compact_block_range_command_spelling_is_stable() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "rocksdb-compact-block-range",
            "--fixture",
            "fixture",
            "--canonical-store",
            "canonical",
            "--secondary-root",
            "secondary",
            "--software-revision",
            "5079515",
        ])?;

        assert!(matches!(cli.command, Command::RocksDbCompactBlockRange(_)));
        Ok(())
    }

    #[test]
    fn acceptance_threshold_flags_must_be_supplied_as_a_pair() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(projection_coupled_replay_args(&[
            "--canonical-fixture-replay-target-secs",
            "10",
        ]))?;
        let Command::ProjectionCoupledReplay(args) = cli.command else {
            return Err("expected projection-coupled-replay command".into());
        };

        let Some(error) = canonical_fixture_replay_thresholds(&args).err() else {
            return Err("unpaired acceptance threshold must be rejected".into());
        };
        assert!(error.to_string().contains(
            "--canonical-fixture-replay-target-secs and --canonical-fixture-replay-hard-limit-secs must be supplied together"
        ));
        Ok(())
    }

    #[test]
    fn acceptance_threshold_pair_parses() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(projection_coupled_replay_args(&[
            "--canonical-fixture-replay-target-secs",
            "10",
            "--canonical-fixture-replay-hard-limit-secs",
            "20",
        ]))?;
        let Command::ProjectionCoupledReplay(args) = cli.command else {
            return Err("expected projection-coupled-replay command".into());
        };

        assert!(canonical_fixture_replay_thresholds(&args)?.is_some());
        Ok(())
    }

    #[test]
    fn unthresholded_replay_allows_omitted_provenance() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(projection_coupled_replay_args(&[]))?;
        let Command::ProjectionCoupledReplay(args) = cli.command else {
            return Err("expected projection-coupled-replay command".into());
        };

        assert!(canonical_fixture_replay_thresholds(&args)?.is_none());
        Ok(())
    }

    #[test]
    fn source_admission_experiment_flags_parse() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(projection_coupled_replay_args(&[
            "--max-response-bytes",
            "67108864",
            "--source-segment-max-blocks",
            "64",
            "--source-segment-target-response-bytes",
            "33554432",
            "--source-fetch-max-in-flight-requests",
            "12",
            "--source-fetch-max-in-flight-bytes",
            "156249984",
            "--block-prepare-memory-watermark-bytes",
            "156249984",
            "--source-segment-delay-millis",
            "250",
        ]))?;
        let Command::ProjectionCoupledReplay(args) = cli.command else {
            return Err("expected projection-coupled-replay command".into());
        };

        assert_eq!(args.max_response_bytes, Some(67_108_864));
        assert_eq!(args.source_segment_max_blocks, Some(64));
        assert_eq!(args.source_segment_target_response_bytes, Some(33_554_432));
        assert_eq!(args.source_fetch_max_in_flight_requests, Some(12));
        assert_eq!(args.source_fetch_max_in_flight_bytes, Some(156_249_984));
        assert_eq!(args.block_prepare_memory_watermark_bytes, Some(156_249_984));
        assert_eq!(args.source_segment_delay_millis, 250);
        Ok(())
    }

    #[tokio::test]
    async fn capture_rejects_zero_prepare_concurrency_before_source_io()
    -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "capture",
            "--network",
            "zcash-mainnet",
            "--json-rpc-addr",
            "http://zebra.invalid:8232",
            "--prepare-concurrency",
            "0",
            "--out",
            "fixture",
        ])?;

        let Some(error) = super::run(cli).await.err() else {
            return Err("zero capture preparation concurrency must be rejected".into());
        };
        assert_eq!(
            error.to_string(),
            "invalid argument: --prepare-concurrency must be greater than zero"
        );
        Ok(())
    }

    #[test]
    fn capture_defaults_prepare_concurrency_to_cpu_envelope() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "capture",
            "--network",
            "zcash-mainnet",
            "--json-rpc-addr",
            "http://zebra:8232",
            "--out",
            "fixture",
        ])?;
        let Command::Capture(args) = cli.command else {
            return Err("expected capture command".into());
        };

        assert_eq!(args.prepare_concurrency, 10);
        Ok(())
    }

    #[test]
    fn report_file_creation_refuses_to_replace_existing_evidence() -> Result<(), Box<dyn Error>> {
        let directory = tempdir()?;
        let report_path = directory.path().join("report.json");
        create_report_file(&report_path, b"first")?;

        assert!(create_report_file(&report_path, b"second").is_err());
        assert_eq!(std::fs::read(&report_path)?, b"first");
        Ok(())
    }
}
