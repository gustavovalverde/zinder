//! Command-line entry point for the fixed-range benchmark harness.

use std::{
    fs::OpenOptions,
    io::Write,
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
    report::{AcceptanceThresholds, BenchmarkReport, FixtureCachePolicy},
};
use zinder_core::{
    BlockHeight, Network, UnixTimestampMillis, wire::decode_zinder_native_chain_name,
};
use zinder_derive::ProjectionPreset;
use zinder_source::{CookieSource, NodeAuth};

#[path = "canonical_fact_round_trip/command.rs"]
mod fact_round_trip_command;

use fact_round_trip_command::{CanonicalFactsRoundTripArgs, run_canonical_facts_round_trip};

const DEFAULT_FROM_HEIGHT: u32 = 150_000;
const DEFAULT_TO_HEIGHT: u32 = 200_000;
const DEFAULT_SEGMENT_BLOCKS: u32 = 1_000;
const DEFAULT_FETCH_CONCURRENCY: u32 = 16;
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
    /// Replay the current projection-coupled schema over a captured fixture.
    CurrentSchemaReplay(CurrentSchemaReplayArgs),
    /// Persist and read back backend-neutral canonical block facts.
    CanonicalFactsRoundTrip(CanonicalFactsRoundTripArgs),
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
struct CurrentSchemaReplayArgs {
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
    /// Projection preset to replay after canonical ingest. `complete` is a
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
        Command::CurrentSchemaReplay(args) => run_current_schema_replay(args).await,
        Command::CanonicalFactsRoundTrip(args) => {
            let output = run_canonical_facts_round_trip(args).await?;
            emit_report(&output.report, output.report_path.as_deref())?;
            output.report.validate()
        }
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

async fn run_current_schema_replay(args: CurrentSchemaReplayArgs) -> Result<(), BenchError> {
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
    args: &CurrentSchemaReplayArgs,
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
    report: &BenchmarkReport,
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

    fn current_schema_replay_args(extra: &[&str]) -> Vec<String> {
        [
            "zinder-bench",
            "current-schema-replay",
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
                Cli::try_parse_from(current_schema_replay_args(&[removed_flag, "10"])).is_err()
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
    fn acceptance_threshold_flags_must_be_supplied_as_a_pair() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(current_schema_replay_args(&[
            "--canonical-fixture-replay-target-secs",
            "10",
        ]))?;
        let Command::CurrentSchemaReplay(args) = cli.command else {
            return Err("expected current-schema-replay command".into());
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
        let cli = Cli::try_parse_from(current_schema_replay_args(&[
            "--canonical-fixture-replay-target-secs",
            "10",
            "--canonical-fixture-replay-hard-limit-secs",
            "20",
        ]))?;
        let Command::CurrentSchemaReplay(args) = cli.command else {
            return Err("expected current-schema-replay command".into());
        };

        assert!(canonical_fixture_replay_thresholds(&args)?.is_some());
        Ok(())
    }

    #[test]
    fn unthresholded_replay_allows_omitted_provenance() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from(current_schema_replay_args(&[]))?;
        let Command::CurrentSchemaReplay(args) = cli.command else {
            return Err("expected current-schema-replay command".into());
        };

        assert!(canonical_fixture_replay_thresholds(&args)?.is_none());
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
