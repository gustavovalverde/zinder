//! CLI adapter for concrete canonical-replay storage round trips.

use std::{env, num::NonZeroU32, path::PathBuf};

use clap::{Args, Subcommand};
use zinder_bench::{
    BenchError,
    canonical_replay_storage::{
        CanonicalReplaySequencePosition,
        postgres::{
            POSTGRES_CANONICAL_REPLAY_SCHEMA_VERSION, POSTGRES_REPLAY_ENVELOPE_COMPRESSION,
            PostgresCanonicalReplayConfig, PostgresCanonicalReplayReport,
            run_postgres_canonical_replay_storage,
        },
        rocksdb::{
            ROCKSDB_CANONICAL_REPLAY_SCHEMA_VERSION, RocksDbCanonicalReplayConfig,
            run_rocksdb_canonical_replay_storage,
        },
    },
    fixture::FixtureManifest,
    report::{
        BenchmarkReport, CanonicalBlockFactsSequenceDigestSummary, CanonicalReplayStorageEvidence,
        CanonicalReplayStorageMeasurements, FixtureCachePolicy, FixtureSummary,
        PostgresBenchmarkRuntimeEvidence, RocksDbResourceBudgetSummary, StorageCandidateIdentity,
        build_canonical_replay_storage_report, is_immutable_image_reference,
        is_valid_benchmark_trial_id,
    },
    rss::peak_rss,
};
use zinder_core::{BlockHash, BlockHeight, CanonicalBlockFactsDigestVersion, UnixTimestampMillis};
use zinder_store::RocksDbResourceBudget;

const DEFAULT_BLOCK_PREPARE_CONCURRENCY: u32 = 16;
const DEFAULT_POSTGRES_DATABASE_URL_ENV: &str = "ZINDER_BENCH_POSTGRES_DATABASE_URL";

/// Concrete engine selected for a canonical-replay round trip.
#[derive(Args)]
pub(super) struct CanonicalReplayStorageArgs {
    #[command(subcommand)]
    engine: CanonicalReplayStorageEngine,
}

#[derive(Subcommand)]
enum CanonicalReplayStorageEngine {
    /// Build and validate a fresh sorted-external-SST `RocksDB` candidate.
    #[command(name = "rocksdb")]
    RocksDb(RocksDbCanonicalReplayArgs),
    /// Build and validate a fresh binary-COPY `PostgreSQL` candidate.
    Postgres(PostgresCanonicalReplayArgs),
}

#[derive(Args)]
struct RocksDbCanonicalReplayArgs {
    #[command(flatten)]
    input: CanonicalReplayStorageInputArgs,
    /// Fresh candidate store path. Existing paths are rejected.
    #[arg(long)]
    store: PathBuf,
    #[command(flatten)]
    report: CanonicalReplayStorageReportArgs,
}

#[derive(Args)]
struct PostgresCanonicalReplayArgs {
    #[command(flatten)]
    input: CanonicalReplayStorageInputArgs,
    /// Environment variable containing an operator-controlled `PostgreSQL` URL.
    #[arg(long, default_value = DEFAULT_POSTGRES_DATABASE_URL_ENV)]
    database_url_env: String,
    /// CPU limit applied to the benchmark client container, in logical cores.
    #[arg(long)]
    client_cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the benchmark client container, in bytes.
    #[arg(long)]
    client_memory_limit_bytes: Option<u64>,
    /// CPU limit applied to the database container, in logical cores.
    #[arg(long)]
    database_cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the database container, in bytes.
    #[arg(long)]
    database_memory_limit_bytes: Option<u64>,
    /// Immutable image reference for the measured database container.
    #[arg(long)]
    database_image_reference: Option<String>,
    #[command(flatten)]
    report: CanonicalReplayStorageReportArgs,
}

#[derive(Args)]
struct CanonicalReplayStorageInputArgs {
    /// Captured fixture directory.
    #[arg(long)]
    fixture: PathBuf,
    /// Maximum number of fixture blocks prepared concurrently.
    #[arg(long, default_value_t = DEFAULT_BLOCK_PREPARE_CONCURRENCY)]
    block_prepare_concurrency: u32,
}

#[derive(Args)]
struct CanonicalReplayStorageReportArgs {
    /// Write the JSON report to this path instead of stdout.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Source revision of the measured binary.
    #[arg(long)]
    software_revision: Option<String>,
    /// Campaign trial identity; requires `--fixture-cache-policy`.
    #[arg(long)]
    trial_id: Option<String>,
    /// Controlled fixture-cache treatment; requires `--trial-id`.
    #[arg(long, value_enum)]
    fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Stable operator label for the complete benchmark arm.
    #[arg(long)]
    runner_id: Option<String>,
    /// Aggregate CPU limit for the complete benchmark arm.
    #[arg(long)]
    cpu_limit_cores: Option<f64>,
    /// Aggregate memory limit for the complete benchmark arm, in bytes.
    #[arg(long)]
    memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    #[arg(long)]
    storage_class: Option<String>,
    /// Immutable container image reference for the measured binary.
    #[arg(long)]
    image_reference: Option<String>,
}

pub(super) struct CanonicalReplayStorageOutput {
    pub report: BenchmarkReport,
    pub report_path: Option<PathBuf>,
}

pub(super) async fn run_canonical_replay_storage(
    args: CanonicalReplayStorageArgs,
) -> Result<CanonicalReplayStorageOutput, BenchError> {
    let run_started_at_unix_millis = UnixTimestampMillis::now().value();
    match args.engine {
        CanonicalReplayStorageEngine::RocksDb(args) => {
            run_rocksdb(args, run_started_at_unix_millis).await
        }
        CanonicalReplayStorageEngine::Postgres(args) => {
            run_postgres(args, run_started_at_unix_millis).await
        }
    }
}

async fn run_rocksdb(
    args: RocksDbCanonicalReplayArgs,
    run_started_at_unix_millis: u64,
) -> Result<CanonicalReplayStorageOutput, BenchError> {
    let block_prepare_concurrency =
        require_nonzero_concurrency(args.input.block_prepare_concurrency)?;
    let report_context = args
        .report
        .validate_and_resolve(run_started_at_unix_millis)?;
    let manifest = FixtureManifest::read(&args.input.fixture)?;
    let fixture = FixtureSummary::try_from(&manifest)?;
    let rocksdb_resource_budget = RocksDbResourceBudget::canonical_writer_defaults();
    let round_trip = run_rocksdb_canonical_replay_storage(RocksDbCanonicalReplayConfig {
        fixture_directory: args.input.fixture,
        candidate_path: args.store,
        block_prepare_concurrency,
        rocksdb_resource_budget,
    })
    .await?;
    let block_digest_version = block_digest_version(&manifest)?;
    let measurements = CanonicalReplayStorageMeasurements {
        block_prepare_concurrency: round_trip.block_prepare_concurrency.get(),
        wall_clock_seconds: round_trip.timings.wall_clock_seconds,
        storage_initialization_wall_clock_seconds: round_trip
            .timings
            .storage_initialization_seconds,
        replay_preparation_wall_clock_seconds: round_trip.timings.replay_preparation_seconds,
        replay_persistence_wall_clock_seconds: round_trip.timings.replay_persistence_seconds,
        index_construction_wall_clock_seconds: round_trip.timings.index_construction_seconds,
        storage_optimization_wall_clock_seconds: round_trip.timings.storage_optimization_seconds,
        validation_wall_clock_seconds: round_trip.timings.validation_seconds,
        publication_wall_clock_seconds: round_trip.timings.publication_seconds,
        fresh_reader_validation_wall_clock_seconds: round_trip
            .timings
            .fresh_reader_validation_seconds,
        storage_measurement_wall_clock_seconds: round_trip.timings.storage_measurement_seconds,
        first_height: round_trip.validation.first_height.value(),
        first_hash_hex: hex::encode(round_trip.validation.first_hash.as_bytes()),
        tip_height: round_trip.validation.tip_height.value(),
        tip_hash_hex: hex::encode(round_trip.validation.tip_hash.as_bytes()),
        logical_replay_bytes: round_trip.logical_replay_bytes,
        physical_storage_bytes: round_trip.physical_storage_bytes,
        persisted_sequence_digest: CanonicalBlockFactsSequenceDigestSummary::from_digest(
            block_digest_version,
            round_trip.validation.sequence_digest,
        ),
        replay_format_version: round_trip.validation.replay_format_version,
        semantic_replay_validated: true,
        storage: CanonicalReplayStorageEvidence::RocksDb {
            storage_schema_version: ROCKSDB_CANONICAL_REPLAY_SCHEMA_VERSION,
            ingestion_mode: "sorted-external-sst",
            durability_mode: "external-sst-ingest-with-synchronous-completion-marker",
            database_io_mode: round_trip.database_io_mode.as_str().to_owned(),
            external_sst_io_mode: round_trip.external_sst_io_mode,
            compression: round_trip.compression,
            external_sst_bytes: round_trip.external_sst_bytes,
            rocksdb_resource_budget: RocksDbResourceBudgetSummary::from(
                round_trip.rocksdb_resource_budget,
            ),
        },
        benchmark_client_peak_rss: peak_rss(),
        storage_candidate: StorageCandidateIdentity::rocksdb_canonical_replay_storage(),
        software_revision: report_context.software_revision,
        trial_id: report_context.trial_id,
        fixture_cache_policy: report_context.fixture_cache_policy,
        run_started_at_unix_millis: report_context.run_started_at_unix_millis,
        run_completed_at_unix_millis: UnixTimestampMillis::now().value(),
        runner_id: report_context.runner_id,
        cpu_limit_cores: report_context.cpu_limit_cores,
        memory_limit_bytes: report_context.memory_limit_bytes,
        storage_class: report_context.storage_class,
        image_reference: report_context.image_reference,
    };
    let report = build_canonical_replay_storage_report(fixture, measurements);
    Ok(CanonicalReplayStorageOutput {
        report,
        report_path: report_context.report_path,
    })
}

async fn run_postgres(
    args: PostgresCanonicalReplayArgs,
    run_started_at_unix_millis: u64,
) -> Result<CanonicalReplayStorageOutput, BenchError> {
    let block_prepare_concurrency =
        require_nonzero_concurrency(args.input.block_prepare_concurrency)?;
    validate_positive_f64(args.database_cpu_limit_cores, "--database-cpu-limit-cores")?;
    validate_positive_f64(args.client_cpu_limit_cores, "--client-cpu-limit-cores")?;
    validate_positive_u64(
        args.client_memory_limit_bytes,
        "--client-memory-limit-bytes",
    )?;
    validate_positive_u64(
        args.database_memory_limit_bytes,
        "--database-memory-limit-bytes",
    )?;
    validate_immutable_image_reference(
        args.database_image_reference.as_deref(),
        "--database-image-reference",
    )?;
    let report_context = args
        .report
        .validate_and_resolve(run_started_at_unix_millis)?;
    validate_postgres_resource_partition(
        args.client_cpu_limit_cores,
        args.client_memory_limit_bytes,
        args.database_cpu_limit_cores,
        args.database_memory_limit_bytes,
        &report_context,
    )?;
    let database_url = read_database_url(&args.database_url_env)?;
    let manifest = FixtureManifest::read(&args.input.fixture)?;
    let round_trip = run_postgres_canonical_replay_storage(PostgresCanonicalReplayConfig::new(
        &args.input.fixture,
        database_url,
        block_prepare_concurrency,
    ))
    .await?;
    let benchmark_runtime = PostgresBenchmarkRuntimeEvidence {
        database_image_reference: args.database_image_reference,
        client_cpu_limit_cores: args.client_cpu_limit_cores,
        client_memory_limit_bytes: args.client_memory_limit_bytes,
        database_cpu_limit_cores: args.database_cpu_limit_cores,
        database_memory_limit_bytes: args.database_memory_limit_bytes,
    };
    build_postgres_round_trip_output(
        &manifest,
        block_prepare_concurrency,
        round_trip,
        benchmark_runtime,
        report_context,
    )
}

fn build_postgres_round_trip_output(
    manifest: &FixtureManifest,
    block_prepare_concurrency: NonZeroU32,
    round_trip: PostgresCanonicalReplayReport,
    benchmark_runtime: PostgresBenchmarkRuntimeEvidence,
    report_context: CanonicalReplayStorageReportContext,
) -> Result<CanonicalReplayStorageOutput, BenchError> {
    let fixture = FixtureSummary::try_from(manifest)?;
    let (first_height, first_hash, tip_height, tip_hash) =
        require_populated_position(round_trip.validation.position)?;
    let block_digest_version = block_digest_version(manifest)?;
    let measurements = CanonicalReplayStorageMeasurements {
        block_prepare_concurrency: block_prepare_concurrency.get(),
        wall_clock_seconds: round_trip.timings.wall_clock_seconds,
        storage_initialization_wall_clock_seconds: round_trip
            .timings
            .storage_initialization_seconds,
        replay_preparation_wall_clock_seconds: round_trip.timings.replay_preparation_seconds,
        replay_persistence_wall_clock_seconds: round_trip.timings.replay_persistence_seconds,
        index_construction_wall_clock_seconds: round_trip.timings.index_construction_seconds,
        storage_optimization_wall_clock_seconds: round_trip.timings.storage_optimization_seconds,
        validation_wall_clock_seconds: round_trip.timings.validation_seconds,
        publication_wall_clock_seconds: round_trip.timings.publication_seconds,
        fresh_reader_validation_wall_clock_seconds: round_trip
            .timings
            .fresh_reader_validation_seconds,
        storage_measurement_wall_clock_seconds: round_trip.timings.storage_measurement_seconds,
        first_height: first_height.value(),
        first_hash_hex: hex::encode(first_hash.as_bytes()),
        tip_height: tip_height.value(),
        tip_hash_hex: hex::encode(tip_hash.as_bytes()),
        logical_replay_bytes: round_trip.storage.logical_replay_bytes,
        physical_storage_bytes: round_trip.storage.schema_bytes,
        persisted_sequence_digest: CanonicalBlockFactsSequenceDigestSummary::from_digest(
            block_digest_version,
            round_trip.validation.sequence_digest,
        ),
        replay_format_version: round_trip.validation.replay_format_version,
        semantic_replay_validated: true,
        storage: CanonicalReplayStorageEvidence::Postgres {
            storage_schema_version: POSTGRES_CANONICAL_REPLAY_SCHEMA_VERSION,
            ingestion_mode: "binary-copy-single-load-transaction-with-deferred-index",
            tables_logged: true,
            replay_envelope_compression: POSTGRES_REPLAY_ENVELOPE_COMPRESSION,
            server_settings: Box::new(round_trip.server_settings),
            replay_table_bytes: round_trip.storage.replay_table_bytes,
            index_bytes: round_trip.storage.index_bytes,
            wal_bytes: round_trip.storage.wal_bytes,
            benchmark_runtime,
        },
        benchmark_client_peak_rss: peak_rss(),
        storage_candidate: StorageCandidateIdentity::postgres_canonical_replay_storage(),
        software_revision: report_context.software_revision,
        trial_id: report_context.trial_id,
        fixture_cache_policy: report_context.fixture_cache_policy,
        run_started_at_unix_millis: report_context.run_started_at_unix_millis,
        run_completed_at_unix_millis: UnixTimestampMillis::now().value(),
        runner_id: report_context.runner_id,
        cpu_limit_cores: report_context.cpu_limit_cores,
        memory_limit_bytes: report_context.memory_limit_bytes,
        storage_class: report_context.storage_class,
        image_reference: report_context.image_reference,
    };
    let report = build_canonical_replay_storage_report(fixture, measurements);
    Ok(CanonicalReplayStorageOutput {
        report,
        report_path: report_context.report_path,
    })
}

struct CanonicalReplayStorageReportContext {
    report_path: Option<PathBuf>,
    software_revision: Option<String>,
    trial_id: Option<String>,
    fixture_cache_policy: Option<FixtureCachePolicy>,
    run_started_at_unix_millis: u64,
    runner_id: Option<String>,
    cpu_limit_cores: Option<f64>,
    memory_limit_bytes: Option<u64>,
    storage_class: Option<String>,
    image_reference: Option<String>,
}

impl CanonicalReplayStorageReportArgs {
    fn validate_and_resolve(
        self,
        run_started_at_unix_millis: u64,
    ) -> Result<CanonicalReplayStorageReportContext, BenchError> {
        validate_positive_f64(self.cpu_limit_cores, "--cpu-limit-cores")?;
        validate_positive_u64(self.memory_limit_bytes, "--memory-limit-bytes")?;
        validate_immutable_image_reference(self.image_reference.as_deref(), "--image-reference")?;
        validate_trial_provenance(self.trial_id.as_deref(), self.fixture_cache_policy)?;
        Ok(CanonicalReplayStorageReportContext {
            report_path: self.report,
            software_revision: self.software_revision,
            trial_id: self.trial_id,
            fixture_cache_policy: self.fixture_cache_policy,
            run_started_at_unix_millis,
            runner_id: self.runner_id,
            cpu_limit_cores: self.cpu_limit_cores,
            memory_limit_bytes: self.memory_limit_bytes,
            storage_class: self.storage_class,
            image_reference: self.image_reference,
        })
    }
}

fn validate_trial_provenance(
    trial_id: Option<&str>,
    fixture_cache_policy: Option<FixtureCachePolicy>,
) -> Result<(), BenchError> {
    match (trial_id, fixture_cache_policy) {
        (None, None) => Ok(()),
        (Some(trial_id), Some(_)) if is_valid_benchmark_trial_id(trial_id) => Ok(()),
        (Some(_), Some(_)) => Err(BenchError::invalid_argument(
            "--trial-id must start with an ASCII alphanumeric character and contain only ASCII alphanumeric characters, '.', '_', or '-'",
        )),
        _ => Err(BenchError::invalid_argument(
            "--trial-id and --fixture-cache-policy must be supplied together",
        )),
    }
}

fn require_nonzero_concurrency(candidate: u32) -> Result<NonZeroU32, BenchError> {
    NonZeroU32::new(candidate).ok_or_else(|| {
        BenchError::invalid_argument("--block-prepare-concurrency must be greater than zero")
    })
}

fn validate_positive_f64(candidate: Option<f64>, flag: &str) -> Result<(), BenchError> {
    if candidate.is_some_and(|candidate| !candidate.is_finite() || candidate <= 0.0) {
        return Err(BenchError::invalid_argument(format!(
            "{flag} must be finite and greater than zero"
        )));
    }
    Ok(())
}

fn validate_positive_u64(candidate: Option<u64>, flag: &str) -> Result<(), BenchError> {
    if candidate == Some(0) {
        return Err(BenchError::invalid_argument(format!(
            "{flag} must be greater than zero"
        )));
    }
    Ok(())
}

fn validate_postgres_resource_partition(
    client_cpu_limit_cores: Option<f64>,
    client_memory_limit_bytes: Option<u64>,
    database_cpu_limit_cores: Option<f64>,
    database_memory_limit_bytes: Option<u64>,
    report: &CanonicalReplayStorageReportContext,
) -> Result<(), BenchError> {
    match (
        report.cpu_limit_cores,
        client_cpu_limit_cores,
        database_cpu_limit_cores,
    ) {
        (None, None, None) => {}
        (Some(aggregate), Some(client), Some(database))
            if (client + database - aggregate).abs() <= f64::EPSILON * aggregate.max(1.0) => {}
        (Some(_), Some(_), Some(_)) => {
            return Err(BenchError::invalid_argument(
                "--client-cpu-limit-cores plus --database-cpu-limit-cores must equal --cpu-limit-cores",
            ));
        }
        _ => {
            return Err(BenchError::invalid_argument(
                "PostgreSQL CPU evidence requires --cpu-limit-cores, --client-cpu-limit-cores, and --database-cpu-limit-cores together",
            ));
        }
    }
    match (
        report.memory_limit_bytes,
        client_memory_limit_bytes,
        database_memory_limit_bytes,
    ) {
        (None, None, None) => {}
        (Some(aggregate), Some(client), Some(database))
            if client.checked_add(database) == Some(aggregate) => {}
        (Some(_), Some(_), Some(_)) => {
            return Err(BenchError::invalid_argument(
                "--client-memory-limit-bytes plus --database-memory-limit-bytes must equal --memory-limit-bytes",
            ));
        }
        _ => {
            return Err(BenchError::invalid_argument(
                "PostgreSQL memory evidence requires --memory-limit-bytes, --client-memory-limit-bytes, and --database-memory-limit-bytes together",
            ));
        }
    }
    Ok(())
}

fn validate_immutable_image_reference(
    candidate: Option<&str>,
    flag: &str,
) -> Result<(), BenchError> {
    if candidate.is_some_and(|reference| !is_immutable_image_reference(reference)) {
        return Err(BenchError::invalid_argument(format!(
            "{flag} must be a sha256 image ID or digest-pinned image reference"
        )));
    }
    Ok(())
}

fn read_database_url(environment_variable: &str) -> Result<String, BenchError> {
    if environment_variable.trim().is_empty() {
        return Err(BenchError::invalid_argument(
            "--database-url-env must name a nonblank environment variable",
        ));
    }
    env::var(environment_variable).map_err(|_| {
        BenchError::invalid_argument(format!(
            "environment variable {environment_variable} must contain the PostgreSQL URL"
        ))
    })
}

fn block_digest_version(
    manifest: &FixtureManifest,
) -> Result<CanonicalBlockFactsDigestVersion, BenchError> {
    CanonicalBlockFactsDigestVersion::try_from(
        manifest
            .canonical_block_facts_digest_evidence
            .block_digest_version,
    )
    .map_err(|source| BenchError::canonical_replay_storage_sequence_mismatch(source.to_string()))
}

fn require_populated_position(
    position: CanonicalReplaySequencePosition,
) -> Result<(BlockHeight, BlockHash, BlockHeight, BlockHash), BenchError> {
    let first_height = position.first_height.ok_or_else(|| {
        BenchError::canonical_replay_storage_sequence_mismatch(
            "persisted canonical replay sequence is empty",
        )
    })?;
    let first_hash = position.first_hash.ok_or_else(|| {
        BenchError::canonical_replay_storage_sequence_mismatch(
            "persisted canonical replay sequence has no first hash",
        )
    })?;
    let tip_height = position.tip_height.ok_or_else(|| {
        BenchError::canonical_replay_storage_sequence_mismatch(
            "persisted canonical replay sequence has no tip height",
        )
    })?;
    let tip_hash = position.tip_hash.ok_or_else(|| {
        BenchError::canonical_replay_storage_sequence_mismatch(
            "persisted canonical replay sequence has no tip hash",
        )
    })?;
    Ok((first_height, first_hash, tip_height, tip_hash))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        CanonicalReplayStorageEngine, CanonicalReplayStorageReportContext,
        DEFAULT_POSTGRES_DATABASE_URL_ENV, validate_postgres_resource_partition,
        validate_trial_provenance,
    };
    use crate::{Cli, Command};
    use zinder_bench::report::FixtureCachePolicy;

    #[test]
    fn trial_provenance_requires_a_valid_paired_cache_policy() {
        assert!(validate_trial_provenance(None, None).is_ok());
        assert!(
            validate_trial_provenance(Some("trial-01"), Some(FixtureCachePolicy::Warm)).is_ok()
        );
        assert!(validate_trial_provenance(Some("trial-01"), None).is_err());
        assert!(validate_trial_provenance(None, Some(FixtureCachePolicy::Cold)).is_err());
        assert!(
            validate_trial_provenance(Some("../trial"), Some(FixtureCachePolicy::Warm)).is_err()
        );
    }

    #[test]
    fn canonical_replay_storage_engines_have_disjoint_required_arguments() {
        assert!(
            Cli::try_parse_from([
                "zinder-bench",
                "canonical-replay-storage",
                "rocksdb",
                "--fixture",
                "fixture",
                "--store",
                "store",
                "--database-url-env",
                "DATABASE_URL",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "zinder-bench",
                "canonical-replay-storage",
                "postgres",
                "--fixture",
                "fixture",
                "--store",
                "store",
            ])
            .is_err()
        );
    }

    #[test]
    fn postgres_uses_the_credential_safe_environment_default() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "zinder-bench",
            "canonical-replay-storage",
            "postgres",
            "--fixture",
            "fixture",
        ])?;
        let Command::CanonicalReplayStorage(args) = cli.command else {
            unreachable!("the parsed command must be canonical-replay-storage");
        };
        let CanonicalReplayStorageEngine::Postgres(args) = args.engine else {
            unreachable!("the parsed engine must be postgres");
        };
        assert_eq!(args.database_url_env, DEFAULT_POSTGRES_DATABASE_URL_ENV);
        Ok(())
    }

    #[test]
    fn postgres_resource_partition_accepts_exact_component_totals() {
        let report = report_context(Some(8.0), Some(16 * 1024 * 1024 * 1024));

        assert!(
            validate_postgres_resource_partition(
                Some(2.0),
                Some(4 * 1024 * 1024 * 1024),
                Some(6.0),
                Some(12 * 1024 * 1024 * 1024),
                &report,
            )
            .is_ok()
        );
        assert!(
            validate_postgres_resource_partition(
                None,
                None,
                None,
                None,
                &report_context(None, None)
            )
            .is_ok()
        );
    }

    #[test]
    fn postgres_resource_partition_rejects_mismatch_and_partial_evidence() {
        let report = report_context(Some(8.0), Some(16 * 1024 * 1024 * 1024));

        assert!(
            validate_postgres_resource_partition(
                Some(1.0),
                Some(4 * 1024 * 1024 * 1024),
                Some(6.0),
                Some(12 * 1024 * 1024 * 1024),
                &report,
            )
            .is_err()
        );
        assert!(
            validate_postgres_resource_partition(
                Some(2.0),
                None,
                Some(6.0),
                Some(12 * 1024 * 1024 * 1024),
                &report,
            )
            .is_err()
        );
    }

    fn report_context(
        cpu_limit_cores: Option<f64>,
        memory_limit_bytes: Option<u64>,
    ) -> CanonicalReplayStorageReportContext {
        CanonicalReplayStorageReportContext {
            report_path: None,
            software_revision: None,
            trial_id: None,
            fixture_cache_policy: None,
            run_started_at_unix_millis: 0,
            runner_id: None,
            cpu_limit_cores,
            memory_limit_bytes,
            storage_class: None,
            image_reference: None,
        }
    }
}
