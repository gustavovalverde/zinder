//! Command-line entry point for the fixed-range benchmark harness.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use zinder_bench::{
    BenchError,
    capture::{CaptureConfig, capture_fixed_range},
    recorder::install_recorder,
    replay::{ProjectionReplayScope, ReplayConfig, replay_fixture},
    report::Report,
};
use zinder_core::{BlockHeight, Network, wire::decode_zinder_native_chain_name};
use zinder_derive::ProjectionPreset;
use zinder_source::{CookieSource, NodeAuth};

const DEFAULT_FROM_HEIGHT: u32 = 150_000;
const DEFAULT_TO_HEIGHT: u32 = 200_000;
const DEFAULT_SEGMENT_BLOCKS: u32 = 1_000;
const DEFAULT_FETCH_CONCURRENCY: u32 = 16;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_BLOCK_PREPARE_CONCURRENCY: u32 = 16;

#[derive(Parser)]
#[command(name = "zinder-bench")]
#[command(about = "Zinder fixed-range capture and replay benchmark harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture raw source payloads for a block range into a fixture directory.
    Capture(CaptureArgs),
    /// Replay the bulk-catchup pipeline over a captured fixture.
    Replay(ReplayArgs),
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
struct ReplayArgs {
    /// Captured fixture directory.
    #[arg(long)]
    fixture: PathBuf,
    /// Writable clone of the captured start-state canonical store.
    #[arg(long)]
    store: PathBuf,
    /// Prepare concurrency to run with.
    #[arg(long = "block-prepare-concurrency", default_value_t = DEFAULT_BLOCK_PREPARE_CONCURRENCY)]
    block_prepare_concurrency: u32,
    /// Optional canonical block-cache override in bytes.
    #[arg(long = "block-cache-bytes")]
    block_cache_bytes: Option<u64>,
    /// Projection preset to replay after canonical ingest. Omit for a
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
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliProjectionPreset {
    Wallet,
    Complete,
}

impl From<CliProjectionPreset> for ProjectionPreset {
    fn from(preset: CliProjectionPreset) -> Self {
        match preset {
            CliProjectionPreset::Wallet => Self::Wallet,
            CliProjectionPreset::Complete => Self::Complete,
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
        Command::Replay(args) => run_replay(args).await,
    }
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

async fn run_replay(args: ReplayArgs) -> Result<(), BenchError> {
    let metrics_handle = install_recorder()?;
    let config = ReplayConfig {
        fixture_directory: args.fixture,
        store_path: args.store,
        block_prepare_concurrency: require_nonzero_u32(
            args.block_prepare_concurrency,
            "block-prepare-concurrency",
        )?,
        canonical_block_cache_bytes: args.block_cache_bytes,
        projection_preset: args.projection_preset.map(ProjectionPreset::from),
        projection_replay_scope: args.projection_replay_scope.into(),
    };
    let report = replay_fixture(config, Some(metrics_handle)).await?;
    emit_report(&report, args.report.as_deref())?;
    Ok(())
}

fn emit_report(report: &Report, report_path: Option<&std::path::Path>) -> Result<(), BenchError> {
    let encoded = serde_json::to_vec_pretty(report)?;
    if let Some(path) = report_path {
        std::fs::write(path, encoded).map_err(|source| BenchError::io(path, source))?;
    } else {
        write_report_to_stdout(&encoded);
    }
    Ok(())
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
