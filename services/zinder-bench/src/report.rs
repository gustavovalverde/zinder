//! Machine-readable benchmark report and its assembly from scraped metrics.

use std::collections::BTreeMap;

use clap::ValueEnum;
use serde::Serialize;
use zinder_core::CanonicalBlockReplayFormatVersion;
use zinder_store::RocksDbResourceBudget;

use crate::{
    canonical_fact_round_trip::postgres::PostgresCanonicalFactServerSettings,
    error::BenchError,
    fixture::{
        CanonicalBlockFactsDigestEvidence, FIXTURE_CONTRACT_IDENTITY, FIXTURE_FORMAT_VERSION,
        FixtureManifest, WorkloadDensity,
    },
    metrics_scrape::{MetricSample, parse_prometheus_samples, sum_by_name},
    rss::PeakRss,
};

/// Machine-readable report schema version.
pub const REPORT_FORMAT_VERSION: u32 = 1;
/// Stable identity stamped into every benchmark report.
pub const REPORT_CONTRACT_IDENTITY: &str = "benchmark-report";

/// Returns whether a container image identity is content addressed.
#[must_use]
pub fn is_immutable_image_reference(reference: &str) -> bool {
    let reference = reference.trim();
    let digest = reference.strip_prefix("sha256:").or_else(|| {
        reference
            .rsplit_once("@sha256:")
            .and_then(|(image_name, digest)| (!image_name.is_empty()).then_some(digest))
    });
    digest.is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Returns whether an operator-supplied trial identity is safe and unambiguous.
#[must_use]
pub fn is_valid_benchmark_trial_id(trial_id: &str) -> bool {
    let mut bytes = trial_id.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

const READ_DURATION_COUNT: &str = "zinder_store_read_duration_seconds_count";
const READ_DURATION_SUM: &str = "zinder_store_read_duration_seconds_sum";
const MULTI_GET_KEYS_TOTAL: &str = "zinder_store_multi_get_keys_total";
const MULTI_GET_RESOLVED_TOTAL: &str = "zinder_store_multi_get_resolved_total";
const ROCKSDB_TICKER: &str = "zinder_store_rocksdb_ticker";
const ROCKSDB_COMPACT_READ_BYTES: &str = "rocksdb.compact.read.bytes";
const ROCKSDB_COMPACT_WRITE_BYTES: &str = "rocksdb.compact.write.bytes";
const PROJECTION_STORE_METRIC_ROLE: &str = "derive_primary";
const COMMIT_DURATION_COUNT: &str = "zinder_ingest_commit_duration_seconds_count";
const COMMIT_FALLBACK_CALLER: &str = "commit_fallback";
pub(crate) const TELEMETRY_COVERAGE_TOTAL: &str = "zinder_bench_telemetry_coverage_total";
pub(crate) const STORE_READ_TELEMETRY_FAMILY: &str = "store_reads";
const HEAD_OF_LINE_WAIT_SUM: &str = "zinder_ingest_bulk_pipeline_head_of_line_wait_seconds_sum";
const HEAD_OF_LINE_WAIT_COUNT: &str = "zinder_ingest_bulk_pipeline_head_of_line_wait_seconds_count";
const BLOCK_PREPARE_STAGE_DURATION_COUNT: &str =
    "zinder_ingest_block_prepare_stage_duration_seconds_count";
const BLOCK_PREPARE_STAGE_DURATION_SUM: &str =
    "zinder_ingest_block_prepare_stage_duration_seconds_sum";
const CANONICAL_BLOCK_CONSTRUCTION_STAGE_DURATION_COUNT: &str =
    "zinder_ingest_canonical_block_construction_stage_duration_seconds_count";
const CANONICAL_BLOCK_CONSTRUCTION_STAGE_DURATION_SUM: &str =
    "zinder_ingest_canonical_block_construction_stage_duration_seconds_sum";

/// Fixture identity echoed into the report.
#[derive(Clone, Debug, Serialize)]
pub struct FixtureSummary {
    /// Stable fixture contract identity.
    pub contract_identity: String,
    /// Fixture manifest format version.
    pub fixture_format_version: u32,
    /// Current-schema oracle artifact version used when capturing the fixture.
    pub current_schema_oracle_artifact_schema_version: u16,
    /// Backend-neutral canonical-fact digest oracle captured with the fixture.
    pub canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence,
    /// Captured range tip hash, hex-encoded in internal byte order.
    pub tip_hash_hex: String,
    /// SHA-256 identity of the normalized fixture manifest and segment digests.
    pub digest_sha256: String,
    /// Network name in Zinder-native encoding.
    pub network: String,
    /// First replayed block height.
    pub from_height: u32,
    /// Last replayed block height.
    pub to_height: u32,
    /// Captured block count.
    pub block_count: u32,
    /// Consensus-byte workload density for the replayed fixture.
    pub workload_density: WorkloadDensity,
    /// Number of segment files.
    pub segment_count: usize,
}

impl TryFrom<&FixtureManifest> for FixtureSummary {
    type Error = BenchError;

    fn try_from(manifest: &FixtureManifest) -> Result<Self, Self::Error> {
        Ok(Self {
            contract_identity: manifest.contract_identity.clone(),
            fixture_format_version: manifest.fixture_format_version,
            current_schema_oracle_artifact_schema_version: manifest
                .current_schema_oracle_artifact_schema_version,
            canonical_block_facts_digest_evidence: manifest
                .canonical_block_facts_digest_evidence
                .clone(),
            tip_hash_hex: manifest.tip_hash_hex.clone(),
            digest_sha256: manifest.digest_sha256()?,
            network: manifest.network.clone(),
            from_height: manifest.from_height,
            to_height: manifest.to_height,
            block_count: manifest.block_count,
            workload_density: manifest.workload_density,
            segment_count: manifest.segments.len(),
        })
    }
}

/// Direct measurements taken around the replay call.
#[derive(Clone, Debug)]
pub struct CurrentSchemaFixtureReplayMeasurements {
    /// Prepare concurrency the run used.
    pub block_prepare_concurrency: u32,
    /// Effective canonical writer schema, resource, and durability settings.
    pub canonical_writer: CurrentSchemaReplayWriterSettings,
    /// Projection preset replayed after canonical ingest, or `None` for a
    /// canonical-only run.
    pub projection_preset: Option<&'static str>,
    /// Projection history scope, or `None` for a canonical-only run.
    pub projection_replay_scope: Option<&'static str>,
    /// Wall-clock seconds spent in the canonical replay call.
    pub wall_clock_seconds: f64,
    /// Logical canonical position and checkpoint identity before replay.
    pub starting_canonical_state: StartingCanonicalState,
    /// Store tip height after replay.
    pub tip_height_after: Option<u32>,
    /// Store tip hash after replay, in the same internal byte order as
    /// [`FixtureSummary::tip_hash_hex`].
    pub tip_hash_after_hex: Option<String>,
    /// Wall-clock seconds spent constructing projections, when driven.
    pub projection_build_wall_clock_seconds: Option<f64>,
    /// Total rows across the selected consumers' owned column families.
    pub projection_row_count: Option<u64>,
    /// Whether every selected projection cursor equals the canonical event tip.
    pub projection_event_cursor_at_tip: Option<bool>,
    /// Final on-disk bytes under the projection-store directory.
    pub projection_store_bytes: Option<u64>,
    /// Seconds required to close and reopen the populated projection store.
    pub projection_store_reopen_seconds: Option<f64>,
    /// Serialized bytes submitted in successful projection write batches.
    pub projection_logical_write_bytes: Option<u64>,
    /// Peak resident-set-size reading.
    pub peak_rss: PeakRss,
    /// Storage implementation measured by this run.
    pub storage_candidate: StorageCandidateIdentity,
    /// Source revision of the measured binary, when supplied by the operator.
    pub software_revision: Option<String>,
    /// Campaign trial identity supplied by the operator, when available.
    pub trial_id: Option<String>,
    /// Declared fixture-cache treatment for this run, when available.
    pub fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Wall-clock Unix timestamp captured before benchmark setup begins.
    pub run_started_at_unix_millis: u64,
    /// Wall-clock Unix timestamp captured after measured work completes.
    pub run_completed_at_unix_millis: u64,
    /// Stable operator label for the runner, when supplied.
    pub runner_id: Option<String>,
    /// CPU limit applied to the benchmark container, in logical cores.
    pub cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the benchmark container.
    pub memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    pub storage_class: Option<String>,
    /// Immutable container image reference, when supplied by the operator.
    pub image_reference: Option<String>,
    /// Acceptance thresholds for the directly driven canonical fixture replay.
    pub canonical_fixture_replay_thresholds: Option<AcceptanceThresholds>,
}

/// Storage implementation identity recorded with every measured run.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct StorageCandidateIdentity {
    /// Stable comparison identifier.
    pub id: &'static str,
    /// Canonical storage engine.
    pub canonical_engine: &'static str,
    /// Canonical logical model exercised by this candidate.
    pub canonical_model: &'static str,
    /// Engine used for current diagnostic projections, when driven.
    pub diagnostic_projection_engine: Option<&'static str>,
    /// Deployment/storage topology used by the candidate.
    pub topology: &'static str,
}

impl StorageCandidateIdentity {
    /// Identifies the existing single-host canonical `RocksDB` implementation.
    #[must_use]
    pub const fn rocksdb_current_schema_oracle() -> Self {
        Self {
            id: "rocksdb-current-schema-oracle",
            canonical_engine: "rocksdb",
            canonical_model: "projection-coupled-current-schema",
            diagnostic_projection_engine: None,
            topology: "rocksdb-single-host",
        }
    }

    /// Identifies the single-host canonical store plus the current diagnostic
    /// projection store.
    #[must_use]
    pub const fn rocksdb_current_schema_with_diagnostic_projections() -> Self {
        Self {
            diagnostic_projection_engine: Some("rocksdb"),
            ..Self::rocksdb_current_schema_oracle()
        }
    }

    /// Identifies the diagnostic fact-first `RocksDB` round-trip arm.
    #[must_use]
    pub const fn rocksdb_fact_first() -> Self {
        Self {
            id: "rocksdb-fact-first",
            canonical_engine: "rocksdb",
            canonical_model: "block-granular-canonical-facts",
            diagnostic_projection_engine: None,
            topology: "rocksdb-single-host",
        }
    }

    /// Identifies the diagnostic fact-first Postgres round-trip arm.
    #[must_use]
    pub const fn postgres_fact_first() -> Self {
        Self {
            id: "postgres-fact-first",
            canonical_engine: "postgres",
            canonical_model: "block-granular-canonical-facts",
            diagnostic_projection_engine: None,
            topology: "postgres-scale-out",
        }
    }
}

/// Effective settings for the current-schema canonical replay writer.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CurrentSchemaReplayWriterSettings {
    /// Durable canonical store schema written by this binary.
    pub store_schema_version: u16,
    /// Durable artifact schema written by this binary.
    pub artifact_schema_version: u16,
    /// Whether each `RocksDB` write batch requests an fsync before returning.
    pub sync_writes: bool,
    /// Stable description of the effective WAL/fsync posture.
    pub durability_mode: &'static str,
    /// Effective bounded `RocksDB` resources.
    pub rocksdb_resource_budget: RocksDbResourceBudgetSummary,
}

/// Serializable form of the effective `RocksDB` resource budget.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RocksDbResourceBudgetSummary {
    /// Shared block-cache allocation.
    pub block_cache_bytes: u64,
    /// Live WAL size ceiling.
    pub max_wal_bytes: u64,
    /// Open `SST` file-handle limit.
    pub max_open_files: i32,
    /// Per-column-family write-buffer size.
    pub write_buffer_bytes: u64,
    /// Mutable plus immutable write-buffer count per column family.
    pub max_write_buffer_count: i32,
    /// Background flush and compaction job limit.
    pub max_background_jobs: i32,
    /// Aggregate memtable budget.
    pub memtable_budget_bytes: u64,
    /// Enabled `RocksDB` statistics level.
    pub statistics_level: &'static str,
}

impl From<RocksDbResourceBudget> for RocksDbResourceBudgetSummary {
    fn from(budget: RocksDbResourceBudget) -> Self {
        Self {
            block_cache_bytes: budget.block_cache_bytes,
            max_wal_bytes: budget.max_wal_bytes,
            max_open_files: budget.max_open_files,
            write_buffer_bytes: budget.write_buffer_bytes,
            max_write_buffer_count: budget.max_write_buffer_count,
            max_background_jobs: budget.max_background_jobs,
            memtable_budget_bytes: budget.memtable_budget_bytes,
            statistics_level: budget.statistics_level.as_str(),
        }
    }
}

/// Target and hard-limit durations for one measured acceptance boundary.
#[derive(Clone, Copy, Debug)]
pub struct AcceptanceThresholds {
    target_seconds: f64,
    hard_limit_seconds: f64,
}

impl AcceptanceThresholds {
    /// Validates and constructs acceptance-boundary thresholds in seconds.
    pub fn try_from_seconds(
        target_seconds: f64,
        hard_limit_seconds: f64,
    ) -> Result<Self, BenchError> {
        if !target_seconds.is_finite() || target_seconds <= 0.0 {
            return Err(BenchError::invalid_argument(
                "acceptance target seconds must be finite and greater than zero",
            ));
        }
        if !hard_limit_seconds.is_finite() || hard_limit_seconds <= 0.0 {
            return Err(BenchError::invalid_argument(
                "acceptance hard-limit seconds must be finite and greater than zero",
            ));
        }
        if target_seconds > hard_limit_seconds {
            return Err(BenchError::invalid_argument(
                "acceptance target seconds must not exceed hard-limit seconds",
            ));
        }
        Ok(Self {
            target_seconds,
            hard_limit_seconds,
        })
    }
}

/// Build and runtime provenance for one report.
#[derive(Clone, Debug, Serialize)]
pub struct ReportProvenance {
    /// `zinder-bench` package version.
    pub benchmark_version: &'static str,
    /// Source revision supplied by the operator, when available.
    pub software_revision: Option<String>,
    /// Per-invocation trial, cache-policy, and timing provenance.
    pub run: BenchmarkRunProvenance,
    /// Structured runner and complete-arm resource provenance.
    pub runner: RunnerProvenance,
    /// Immutable container image reference supplied by the operator.
    pub image_reference: Option<String>,
    /// Operating system for which the benchmark binary was built.
    pub target_os: &'static str,
    /// CPU architecture for which the benchmark binary was built.
    pub target_arch: &'static str,
}

/// Per-invocation provenance used to bind reports into a benchmark campaign.
#[derive(Clone, Debug, Serialize)]
pub struct BenchmarkRunProvenance {
    /// Operator-assigned trial identity shared by the paired candidate arms.
    pub trial_id: Option<String>,
    /// Declared treatment of the fixture's filesystem cache.
    pub fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Wall-clock Unix timestamp captured before benchmark setup begins.
    pub started_at_unix_millis: u64,
    /// Wall-clock Unix timestamp captured after measured work completes.
    pub completed_at_unix_millis: u64,
}

/// Controlled fixture-cache treatment for a benchmark run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureCachePolicy {
    /// The operator evicted the fixture from the filesystem cache before the run.
    Cold,
    /// The operator deliberately retained or preloaded the fixture cache.
    Warm,
}

/// Operator-supplied runner and complete-arm resource provenance.
#[derive(Clone, Debug, Serialize)]
pub struct RunnerProvenance {
    /// Stable runner identity; this label is not a substitute for the fields below.
    pub id: Option<String>,
    /// Aggregate CPU allocation for the measured benchmark arm, in logical cores.
    pub cpu_limit_cores: Option<f64>,
    /// Aggregate memory allocation for the measured benchmark arm.
    pub memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    pub storage_class: Option<String>,
}

/// Logical canonical position and checkpoint identity before replay.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StartingCanonicalState {
    /// How the replay starting state was established.
    pub kind: StartingCanonicalStateKind,
    /// Monotonic chain epoch identifier, or `None` for an empty store.
    pub chain_epoch_id: Option<u64>,
    /// Visible canonical tip height, or `None` for an empty store.
    pub tip_height: Option<u32>,
    /// Visible tip hash in RPC display byte order, or `None` for an empty store.
    pub tip_hash_rpc_hex: Option<String>,
    /// Artifact schema version opened from the store.
    pub artifact_schema_version: Option<u16>,
    /// SHA-256 of the raw backup manifest, when the clone came from a backup.
    pub checkpoint_manifest_sha256: Option<String>,
}

/// Provenance class for the canonical state opened before replay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartingCanonicalStateKind {
    /// A true empty store used for a fresh-genesis replay.
    #[default]
    Empty,
    /// A store whose logical position matches a supplied backup manifest.
    Checkpoint,
    /// A nonempty manual clone without backup-manifest provenance.
    UnverifiedClone,
}

impl StartingCanonicalStateKind {
    /// Stable machine-readable spelling used in validation diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Checkpoint => "checkpoint",
            Self::UnverifiedClone => "unverified-clone",
        }
    }
}

/// One directly measured acceptance boundary.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AcceptanceMeasurementSummary {
    /// Exact history scope covered by this measurement.
    pub scope: &'static str,
    /// Direct wall-clock duration of the stage.
    pub wall_clock_seconds: f64,
    /// Supplied target and hard-limit evaluation, when configured.
    pub thresholds: Option<AcceptanceThresholdSummary>,
}

/// Target and hard-limit evaluation for one measured duration.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AcceptanceThresholdSummary {
    /// Desired completion time.
    pub target_seconds: f64,
    /// Maximum accepted completion time.
    pub hard_limit_seconds: f64,
    /// Whether the measured stage completed within the target.
    pub target_met: bool,
    /// Whether the measured stage completed within the hard limit.
    pub hard_limit_met: bool,
}

/// Acceptance results for boundaries this command directly drives.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CurrentSchemaFixtureReplayAcceptance {
    /// Fixture replay into the supplied canonical-store clone.
    pub canonical_fixture_replay: AcceptanceMeasurementSummary,
}

/// Aggregated timing for one canonical-store read caller.
#[derive(Clone, Debug, Serialize)]
pub struct CallerReadStat {
    /// Pipeline stage that issued the read.
    pub caller: String,
    /// Column family read.
    pub table: String,
    /// Read operation kind.
    pub operation: String,
    /// Number of read calls.
    pub call_count: u64,
    /// Cumulative histogram seconds across the calls.
    pub task_seconds: f64,
}

/// Aggregated `multi_get` key accounting for one caller and table.
#[derive(Clone, Debug, Serialize)]
pub struct MultiGetStat {
    /// Column family read.
    pub table: String,
    /// Pipeline stage that issued the read.
    pub caller: String,
    /// Requested key count across all `multi_get` calls.
    pub keys_total: u64,
    /// Resolved (present) key count across all `multi_get` calls.
    pub resolved_total: u64,
}

/// Aggregated head-of-line wait for one bulk-pipeline stage.
///
/// A large source-fetch wait relative to the replay wall clock signals the run
/// was source-bound rather than limited by the knob under test.
#[derive(Clone, Debug, Serialize)]
pub struct StageWaitStat {
    /// Bulk-pipeline stage that stalled waiting on its input.
    pub stage: String,
    /// Number of recorded head-of-line waits.
    pub wait_count: u64,
    /// Cumulative seconds the stage spent waiting on its input.
    pub wait_seconds: f64,
}

/// Aggregated work time for one bulk-catchup or canonical-construction substage.
#[derive(Clone, Debug, Serialize)]
pub struct StageDurationStat {
    /// Metric family that owns the stage.
    pub family: String,
    /// Stable stage label.
    pub stage: String,
    /// Outcome label.
    pub status: String,
    /// Number of completed stage invocations.
    pub call_count: u64,
    /// Cumulative histogram seconds across the invocations.
    pub task_seconds: f64,
}

/// One exported `RocksDB` statistics ticker.
#[derive(Clone, Debug, Serialize)]
pub struct TickerStat {
    /// Upstream `RocksDB` ticker name.
    pub ticker: String,
    /// Store role that owns the ticker.
    pub store_role: String,
    /// Ticker reading.
    pub reading: f64,
}

/// Replay-derived scalars folded into the report.
#[derive(Clone, Debug, Serialize)]
pub struct CurrentSchemaFixtureReplaySummary {
    /// Prepare concurrency the run used.
    pub block_prepare_concurrency: u32,
    /// Effective canonical writer schema, resource, and durability settings.
    pub canonical_writer: CurrentSchemaReplayWriterSettings,
    /// Projection preset replayed after canonical ingest, or `None` for a
    /// canonical-only run.
    pub projection_preset: Option<&'static str>,
    /// Projection history scope, or `None` for a canonical-only run.
    pub projection_replay_scope: Option<&'static str>,
    /// Wall-clock seconds spent in the canonical replay call.
    pub wall_clock_seconds: f64,
    /// Logical canonical position and checkpoint identity before replay.
    pub starting_canonical_state: StartingCanonicalState,
    /// Store tip height after replay.
    pub tip_height_after: Option<u32>,
    /// Store tip hash after replay, in the same internal byte order as
    /// [`FixtureSummary::tip_hash_hex`].
    pub tip_hash_after_hex: Option<String>,
    /// Blocks committed during replay (tip delta).
    pub blocks_committed: u64,
    /// Chain epochs committed during replay, when telemetry covered the family.
    pub epochs_committed: Option<u64>,
    /// Committed blocks per wall-clock second.
    pub blocks_per_second: f64,
    /// Commit-fallback read calls when store-read telemetry was covered.
    pub commit_fallback_reads: Option<u64>,
    /// Wall-clock seconds spent constructing projections, when driven.
    pub projection_build_wall_clock_seconds: Option<f64>,
    /// Total rows across the selected consumers' owned column families.
    pub projection_row_count: Option<u64>,
    /// Whether every selected projection cursor equals the canonical event tip.
    pub projection_event_cursor_at_tip: Option<bool>,
    /// Final on-disk bytes under the projection-store directory.
    pub projection_store_bytes: Option<u64>,
    /// Serialized bytes submitted in successful projection write batches.
    pub projection_logical_write_bytes: Option<u64>,
    /// Bytes read plus written by projection-store compactions.
    pub projection_compaction_bytes: Option<u64>,
    /// Seconds required to close and reopen the populated projection store.
    pub projection_store_reopen_seconds: Option<f64>,
    /// Peak resident-set-size reading.
    pub peak_rss: PeakRss,
}

/// Digest evidence recomputed from persisted canonical fact encodings.
#[derive(Clone, Debug, Serialize)]
pub struct CanonicalFactSequenceDigestSummary {
    /// Per-block reference-encoding digest version.
    pub block_digest_version: u16,
    /// Ordered sequence-digest version.
    pub sequence_digest_version: u16,
    /// Number of ordered block digests committed by the sequence.
    pub block_count: u64,
    /// Sequence SHA-256, hex encoded.
    pub sha256: String,
}

impl CanonicalFactSequenceDigestSummary {
    /// Converts the typed core digest into report-safe scalar evidence.
    #[must_use]
    pub fn from_digest(
        block_digest_version: zinder_core::CanonicalBlockFactsDigestVersion,
        sequence_digest: zinder_core::CanonicalBlockFactsSequenceDigest,
    ) -> Self {
        Self {
            block_digest_version: block_digest_version.value(),
            sequence_digest_version: sequence_digest.version().value(),
            block_count: sequence_digest.block_count(),
            sha256: hex::encode(sequence_digest.as_bytes()),
        }
    }
}

/// `PostgreSQL` client and database runtime evidence for one measured arm.
#[derive(Clone, Debug, Serialize)]
pub struct PostgresBenchmarkRuntimeEvidence {
    /// Immutable database container image reference supplied by the operator.
    pub database_image_reference: Option<String>,
    /// CPU limit applied to the benchmark client container.
    pub client_cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the benchmark client container.
    pub client_memory_limit_bytes: Option<u64>,
    /// CPU limit applied to the database container.
    pub database_cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the database container.
    pub database_memory_limit_bytes: Option<u64>,
}

/// Engine-specific settings and physical write evidence for one fact-first arm.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "engine", rename_all = "kebab-case")]
pub enum CanonicalBlockFactsStorageEvidence {
    /// `RocksDB` external-SST construction settings.
    #[serde(rename = "rocksdb")]
    RocksDb {
        /// Candidate-owned physical schema version.
        storage_schema_version: u16,
        /// Stable construction mechanism label.
        ingestion_mode: &'static str,
        /// Stable durability description for ingestion and publication.
        durability_mode: &'static str,
        /// Resolved direct or buffered filesystem I/O mode for database access.
        database_io_mode: String,
        /// Filesystem I/O mode used while constructing the external SST.
        external_sst_io_mode: &'static str,
        /// Explicit block-compression algorithm used by both SST construction and the column family.
        compression: &'static str,
        /// Bytes written into the external SST before ingestion.
        external_sst_bytes: u64,
        /// Effective bounded `RocksDB` resources.
        rocksdb_resource_budget: RocksDbResourceBudgetSummary,
    },
    /// Postgres binary-COPY construction settings.
    Postgres {
        /// Candidate-owned physical schema version.
        storage_schema_version: u16,
        /// Stable construction mechanism label.
        ingestion_mode: &'static str,
        /// Whether the candidate fact table is durable and WAL logged.
        tables_logged: bool,
        /// Explicit TOAST compression used for canonical replay encodings.
        replay_envelope_compression: &'static str,
        /// Queried performance and durability settings for the measured database arm.
        server_settings: Box<PostgresCanonicalFactServerSettings>,
        /// Bytes owned by the canonical-fact heap and its auxiliary forks.
        fact_table_bytes: u64,
        /// Bytes owned by indexes created after fact loading.
        index_bytes: u64,
        /// WAL bytes advanced during construction and publication.
        wal_bytes: u64,
        /// Client/database images and component resource limits.
        benchmark_runtime: PostgresBenchmarkRuntimeEvidence,
    },
}

/// Direct measurements from a persisted canonical-block-facts round trip.
#[derive(Clone, Debug)]
pub struct CanonicalBlockFactsRoundTripMeasurements {
    /// Prepare concurrency used while parsing fixture blocks.
    pub block_prepare_concurrency: u32,
    /// Total measured round-trip seconds, including validation and publication.
    pub wall_clock_seconds: f64,
    /// Fixture metadata validation plus fresh backend and physical-schema initialization.
    pub storage_initialization_wall_clock_seconds: f64,
    /// Fixture read, parse, and semantic replay encoding seconds.
    pub fact_preparation_wall_clock_seconds: f64,
    /// Primary table or external-SST construction and ingestion seconds.
    pub fact_persistence_wall_clock_seconds: f64,
    /// Index construction performed after primary fact loading.
    pub index_construction_wall_clock_seconds: f64,
    /// Post-load storage optimization, such as `PostgreSQL` `ANALYZE`.
    pub storage_optimization_wall_clock_seconds: f64,
    /// Persisted read-back and digest-validation seconds.
    pub validation_wall_clock_seconds: f64,
    /// Completion-fence publication seconds.
    pub publication_wall_clock_seconds: f64,
    /// Validation through a new reader: a database reopen or server reconnection.
    pub fresh_reader_validation_wall_clock_seconds: f64,
    /// Final physical storage and write-amplification measurement seconds.
    pub storage_measurement_wall_clock_seconds: f64,
    /// First persisted block height.
    pub first_height: u32,
    /// First persisted block hash in internal byte order.
    pub first_hash_hex: String,
    /// Last persisted block height.
    pub tip_height: u32,
    /// Last persisted block hash in internal byte order.
    pub tip_hash_hex: String,
    /// Logical semantic replay encoding bytes submitted to storage.
    pub logical_fact_bytes: u64,
    /// Final physical bytes owned by the candidate tables/store.
    pub physical_storage_bytes: u64,
    /// Ordered digest recomputed from persisted rows.
    pub persisted_sequence_digest: CanonicalFactSequenceDigestSummary,
    /// Semantic replay format version decoded from every persisted row.
    pub replay_format_version: u32,
    /// Whether every persisted replay envelope decoded into complete canonical facts.
    pub semantic_replay_validated: bool,
    /// Engine-specific settings and physical write evidence.
    pub storage: CanonicalBlockFactsStorageEvidence,
    /// Peak resident-set-size reading for the benchmark client process.
    ///
    /// This includes the embedded database in the `RocksDB` arm and excludes
    /// the separate database server in the `PostgreSQL` arm.
    pub benchmark_client_peak_rss: PeakRss,
    /// Storage implementation measured by this run.
    pub storage_candidate: StorageCandidateIdentity,
    /// Source revision of the measured binary, when supplied.
    pub software_revision: Option<String>,
    /// Campaign trial identity supplied by the operator, when available.
    pub trial_id: Option<String>,
    /// Declared fixture-cache treatment for this run, when available.
    pub fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Wall-clock Unix timestamp captured before benchmark setup begins.
    pub run_started_at_unix_millis: u64,
    /// Wall-clock Unix timestamp captured after measured work completes.
    pub run_completed_at_unix_millis: u64,
    /// Stable operator label for the complete benchmark arm.
    pub runner_id: Option<String>,
    /// Aggregate CPU limit for the complete benchmark arm.
    pub cpu_limit_cores: Option<f64>,
    /// Aggregate memory limit for the complete benchmark arm.
    pub memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    pub storage_class: Option<String>,
    /// Immutable container image reference, when supplied.
    pub image_reference: Option<String>,
}

/// Persisted fact-round-trip scalars. This is evidence for the fact aggregate
/// only, not a chain epoch, query-readiness, reorg, or projection lifecycle.
#[derive(Clone, Debug, Serialize)]
pub struct CanonicalBlockFactsRoundTripSummary {
    /// Exact diagnostic boundary this report drove.
    pub scope: &'static str,
    /// Prepare concurrency used while parsing fixture blocks.
    pub block_prepare_concurrency: u32,
    /// Total measured round-trip seconds.
    pub wall_clock_seconds: f64,
    /// Fixture metadata validation plus fresh backend and physical-schema initialization.
    pub storage_initialization_wall_clock_seconds: f64,
    /// Fixture read, parse, and semantic replay encoding seconds.
    pub fact_preparation_wall_clock_seconds: f64,
    /// Primary table or external-SST construction and ingestion seconds.
    pub fact_persistence_wall_clock_seconds: f64,
    /// Index construction performed after primary fact loading.
    pub index_construction_wall_clock_seconds: f64,
    /// Post-load storage optimization seconds.
    pub storage_optimization_wall_clock_seconds: f64,
    /// Persisted read-back and digest-validation seconds.
    pub validation_wall_clock_seconds: f64,
    /// Completion-fence publication seconds.
    pub publication_wall_clock_seconds: f64,
    /// Validation through a new reader: a database reopen or server reconnection.
    pub fresh_reader_validation_wall_clock_seconds: f64,
    /// Final physical storage and write-amplification measurement seconds.
    pub storage_measurement_wall_clock_seconds: f64,
    /// Wall-clock time not attributed to an explicitly measured stage.
    pub unattributed_wall_clock_seconds: f64,
    /// First persisted block height.
    pub first_height: u32,
    /// First persisted block hash in internal byte order.
    pub first_hash_hex: String,
    /// Last persisted block height.
    pub tip_height: u32,
    /// Last persisted block hash in internal byte order.
    pub tip_hash_hex: String,
    /// Persisted block count.
    pub block_count: u64,
    /// End-to-end persisted blocks per second.
    pub blocks_per_second: f64,
    /// Logical semantic replay encoding bytes submitted to storage.
    pub logical_fact_bytes: u64,
    /// Final physical bytes owned by the candidate tables/store.
    pub physical_storage_bytes: u64,
    /// Ordered digest recomputed from persisted rows.
    pub persisted_sequence_digest: CanonicalFactSequenceDigestSummary,
    /// Whether the persisted sequence equals the fixture capture oracle.
    pub fixture_sequence_digest_match: bool,
    /// Semantic replay format version decoded from every persisted row.
    pub replay_format_version: u32,
    /// Whether every persisted replay envelope decoded into complete canonical facts.
    pub semantic_replay_validated: bool,
    /// Engine-specific settings and physical write evidence.
    pub storage: CanonicalBlockFactsStorageEvidence,
    /// Peak resident-set-size reading for the benchmark client process.
    ///
    /// This includes the embedded database in the `RocksDB` arm and excludes
    /// the separate database server in the `PostgreSQL` arm.
    pub benchmark_client_peak_rss: PeakRss,
}

/// Full report for one canonical-block-facts persisted round trip.
#[derive(Clone, Debug, Serialize)]
pub struct CanonicalBlockFactsRoundTripReport {
    /// Stable report contract identity.
    pub contract_identity: String,
    /// Machine-readable report schema version.
    pub report_format_version: u32,
    /// Build and source provenance.
    pub provenance: ReportProvenance,
    /// Fixture identity.
    pub fixture: FixtureSummary,
    /// Storage candidate measured by this invocation.
    pub storage_candidate: StorageCandidateIdentity,
    /// Fact-only round-trip evidence.
    pub round_trip: CanonicalBlockFactsRoundTripSummary,
}

/// Current-schema fixture replay report.
#[derive(Clone, Debug, Serialize)]
pub struct CurrentSchemaFixtureReplayReport {
    /// Stable report contract identity.
    pub contract_identity: String,
    /// Machine-readable report schema version.
    pub report_format_version: u32,
    /// Build and source provenance.
    pub provenance: ReportProvenance,
    /// Fixture identity.
    pub fixture: FixtureSummary,
    /// Storage candidate measured by this invocation.
    pub storage_candidate: StorageCandidateIdentity,
    /// Acceptance results for boundaries driven by this command.
    pub acceptance: CurrentSchemaFixtureReplayAcceptance,
    /// Replay-derived scalars.
    pub replay: CurrentSchemaFixtureReplaySummary,
    /// Per-caller canonical-store read timing.
    pub store_reads: Vec<CallerReadStat>,
    /// Per-caller `multi_get` key accounting.
    pub multi_get: Vec<MultiGetStat>,
    /// Per-stage bulk-pipeline head-of-line wait totals.
    pub head_of_line_wait: Vec<StageWaitStat>,
    /// Per-substage block preparation and canonical construction timing.
    pub stage_durations: Vec<StageDurationStat>,
    /// Exported `RocksDB` statistics tickers.
    pub rocksdb_tickers: Vec<TickerStat>,
}

/// One versioned benchmark report with a closed, candidate-honest measurement
/// shape.
///
/// The tagged variants deliberately prevent fact-only storage comparisons
/// from serializing current-schema lifecycle acceptance or telemetry fields.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "measurement_kind", rename_all = "kebab-case")]
pub enum BenchmarkReport {
    /// Existing bulk-catchup replay into the projection-coupled schema.
    CurrentSchemaFixtureReplay(Box<CurrentSchemaFixtureReplayReport>),
    /// Persisted round trip of block-local canonical facts only.
    CanonicalBlockFactsRoundTrip(Box<CanonicalBlockFactsRoundTripReport>),
}

impl From<CurrentSchemaFixtureReplayReport> for BenchmarkReport {
    fn from(report: CurrentSchemaFixtureReplayReport) -> Self {
        Self::CurrentSchemaFixtureReplay(Box::new(report))
    }
}

impl From<CanonicalBlockFactsRoundTripReport> for BenchmarkReport {
    fn from(report: CanonicalBlockFactsRoundTripReport) -> Self {
        Self::CanonicalBlockFactsRoundTrip(Box::new(report))
    }
}

impl BenchmarkReport {
    /// Validates the evidence boundary owned by the selected measurement.
    pub fn validate(&self) -> Result<(), BenchError> {
        self.validate_contract_identity()?;
        match self {
            Self::CurrentSchemaFixtureReplay(report) => report.validate_acceptance(),
            Self::CanonicalBlockFactsRoundTrip(report) => {
                if report.round_trip.replay_format_version
                    != CanonicalBlockReplayFormatVersion::CURRENT.value()
                    || !report.round_trip.semantic_replay_validated
                {
                    return Err(BenchError::canonical_fact_sequence_mismatch(
                        "persisted canonical facts lack complete semantic replay validation",
                    ));
                }
                if !report.round_trip.fixture_sequence_digest_match {
                    return Err(BenchError::canonical_fact_sequence_mismatch(
                        "persisted sequence digest does not match the fixture capture oracle",
                    ));
                }
                if report.round_trip.block_count != u64::from(report.fixture.block_count)
                    || report.round_trip.first_height != report.fixture.from_height
                    || report.round_trip.tip_height != report.fixture.to_height
                    || report.round_trip.tip_hash_hex != report.fixture.tip_hash_hex
                {
                    return Err(BenchError::canonical_fact_sequence_mismatch(format!(
                        "expected {} blocks over heights {}..={} ending at hash {}, observed {} blocks over heights {}..={} ending at hash {}",
                        report.fixture.block_count,
                        report.fixture.from_height,
                        report.fixture.to_height,
                        report.fixture.tip_hash_hex,
                        report.round_trip.block_count,
                        report.round_trip.first_height,
                        report.round_trip.tip_height,
                        report.round_trip.tip_hash_hex
                    )));
                }
                Ok(())
            }
        }
    }

    fn validate_contract_identity(&self) -> Result<(), BenchError> {
        let (contract_identity, report_format_version, fixture) = match self {
            Self::CurrentSchemaFixtureReplay(report) => (
                report.contract_identity.as_str(),
                report.report_format_version,
                &report.fixture,
            ),
            Self::CanonicalBlockFactsRoundTrip(report) => (
                report.contract_identity.as_str(),
                report.report_format_version,
                &report.fixture,
            ),
        };
        if contract_identity != REPORT_CONTRACT_IDENTITY {
            return Err(BenchError::report_format(format!(
                "report contract identity {contract_identity:?} does not match {REPORT_CONTRACT_IDENTITY:?}"
            )));
        }
        if report_format_version != REPORT_FORMAT_VERSION {
            return Err(BenchError::report_format(format!(
                "report format version {report_format_version} does not match {REPORT_FORMAT_VERSION}"
            )));
        }
        if fixture.contract_identity != FIXTURE_CONTRACT_IDENTITY {
            return Err(BenchError::report_format(format!(
                "fixture contract identity {:?} does not match {FIXTURE_CONTRACT_IDENTITY:?}",
                fixture.contract_identity
            )));
        }
        if fixture.fixture_format_version != FIXTURE_FORMAT_VERSION {
            return Err(BenchError::report_format(format!(
                "fixture format version {} does not match {FIXTURE_FORMAT_VERSION}",
                fixture.fixture_format_version
            )));
        }
        Ok(())
    }

    /// Validates telemetry coverage and the configured hard acceptance limit.
    pub fn validate_acceptance(&self) -> Result<(), BenchError> {
        let Self::CurrentSchemaFixtureReplay(report) = self else {
            return Ok(());
        };
        report.validate_acceptance()
    }

    /// Returns the current-schema measurement when this is the oracle variant.
    #[must_use]
    pub const fn current_schema_fixture_replay(&self) -> Option<&CurrentSchemaFixtureReplayReport> {
        match self {
            Self::CurrentSchemaFixtureReplay(report) => Some(report),
            Self::CanonicalBlockFactsRoundTrip(_) => None,
        }
    }

    /// Returns the fact round-trip measurement when this is a fact-first arm.
    #[must_use]
    pub const fn canonical_block_facts_round_trip(
        &self,
    ) -> Option<&CanonicalBlockFactsRoundTripReport> {
        match self {
            Self::CurrentSchemaFixtureReplay(_) => None,
            Self::CanonicalBlockFactsRoundTrip(report) => Some(report),
        }
    }
}

impl CurrentSchemaFixtureReplayReport {
    /// Validates telemetry coverage and the configured hard acceptance limit.
    pub fn validate_acceptance(&self) -> Result<(), BenchError> {
        if self
            .acceptance
            .canonical_fixture_replay
            .thresholds
            .is_some()
        {
            if self.replay.tip_height_after != Some(self.fixture.to_height)
                || self.replay.tip_hash_after_hex.as_deref()
                    != Some(self.fixture.tip_hash_hex.as_str())
                || self.replay.blocks_committed != u64::from(self.fixture.block_count)
            {
                return Err(BenchError::acceptance_completion_mismatch(format!(
                    "expected height {} hash {} and {} committed blocks, observed height {:?} hash {:?} and {} committed blocks",
                    self.fixture.to_height,
                    self.fixture.tip_hash_hex,
                    self.fixture.block_count,
                    self.replay.tip_height_after,
                    self.replay.tip_hash_after_hex,
                    self.replay.blocks_committed
                )));
            }
            let mut missing_telemetry = Vec::new();
            if self.replay.epochs_committed.is_none() {
                missing_telemetry.push("epochs_committed");
            }
            if self.replay.commit_fallback_reads.is_none() {
                missing_telemetry.push("commit_fallback_reads");
            }
            if !missing_telemetry.is_empty() {
                return Err(BenchError::acceptance_telemetry_missing(
                    missing_telemetry.join(", "),
                ));
            }
        }
        if self
            .acceptance
            .canonical_fixture_replay
            .thresholds
            .is_some_and(|thresholds| !thresholds.hard_limit_met)
        {
            return Err(BenchError::acceptance_hard_limit_missed(
                "canonical_fixture_replay",
            ));
        }
        Ok(())
    }
}

/// Builds the report from direct measurements and the scraped exposition text.
#[must_use]
pub fn build_current_schema_fixture_replay_report(
    fixture: FixtureSummary,
    measurements: &CurrentSchemaFixtureReplayMeasurements,
    exposition: Option<&str>,
) -> CurrentSchemaFixtureReplayReport {
    let samples = exposition.map(parse_prometheus_samples).unwrap_or_default();
    let store_reads = aggregate_store_reads(&samples);
    let rocksdb_tickers = aggregate_tickers(&samples);
    let replay = build_replay_summary(measurements, &samples, &store_reads, &rocksdb_tickers);
    CurrentSchemaFixtureReplayReport {
        contract_identity: REPORT_CONTRACT_IDENTITY.to_owned(),
        report_format_version: REPORT_FORMAT_VERSION,
        provenance: ReportProvenance {
            benchmark_version: env!("CARGO_PKG_VERSION"),
            software_revision: measurements.software_revision.clone(),
            run: BenchmarkRunProvenance {
                trial_id: measurements.trial_id.clone(),
                fixture_cache_policy: measurements.fixture_cache_policy,
                started_at_unix_millis: measurements.run_started_at_unix_millis,
                completed_at_unix_millis: measurements.run_completed_at_unix_millis,
            },
            runner: RunnerProvenance {
                id: measurements.runner_id.clone(),
                cpu_limit_cores: measurements.cpu_limit_cores,
                memory_limit_bytes: measurements.memory_limit_bytes,
                storage_class: measurements.storage_class.clone(),
            },
            image_reference: measurements.image_reference.clone(),
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
        },
        fixture,
        storage_candidate: measurements.storage_candidate,
        acceptance: build_acceptance_summary(measurements),
        replay,
        store_reads,
        multi_get: aggregate_multi_get(&samples),
        head_of_line_wait: aggregate_head_of_line_wait(&samples),
        stage_durations: aggregate_stage_durations(&samples),
        rocksdb_tickers,
    }
}

/// Builds a fact-only persisted round-trip report.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "the report builder keeps the versioned measurement-to-contract mapping explicit"
)]
pub fn build_canonical_block_facts_round_trip_report(
    fixture: FixtureSummary,
    measurements: CanonicalBlockFactsRoundTripMeasurements,
) -> BenchmarkReport {
    let block_count = measurements.persisted_sequence_digest.block_count;
    let blocks_per_second = if measurements.wall_clock_seconds > 0.0 {
        u64_to_f64(block_count) / measurements.wall_clock_seconds
    } else {
        0.0
    };
    let fixture_sequence_digest_match = block_count == u64::from(fixture.block_count)
        && measurements.persisted_sequence_digest.block_digest_version
            == fixture
                .canonical_block_facts_digest_evidence
                .block_digest_version
        && measurements
            .persisted_sequence_digest
            .sequence_digest_version
            == fixture
                .canonical_block_facts_digest_evidence
                .sequence_digest_version
        && measurements.persisted_sequence_digest.sha256.as_str()
            == fixture
                .canonical_block_facts_digest_evidence
                .sequence_digest_sha256
                .as_str();
    let provenance = ReportProvenance {
        benchmark_version: env!("CARGO_PKG_VERSION"),
        software_revision: measurements.software_revision,
        run: BenchmarkRunProvenance {
            trial_id: measurements.trial_id,
            fixture_cache_policy: measurements.fixture_cache_policy,
            started_at_unix_millis: measurements.run_started_at_unix_millis,
            completed_at_unix_millis: measurements.run_completed_at_unix_millis,
        },
        runner: RunnerProvenance {
            id: measurements.runner_id,
            cpu_limit_cores: measurements.cpu_limit_cores,
            memory_limit_bytes: measurements.memory_limit_bytes,
            storage_class: measurements.storage_class,
        },
        image_reference: measurements.image_reference,
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
    };
    let attributed_wall_clock_seconds = measurements.storage_initialization_wall_clock_seconds
        + measurements.fact_preparation_wall_clock_seconds
        + measurements.fact_persistence_wall_clock_seconds
        + measurements.index_construction_wall_clock_seconds
        + measurements.storage_optimization_wall_clock_seconds
        + measurements.validation_wall_clock_seconds
        + measurements.publication_wall_clock_seconds
        + measurements.fresh_reader_validation_wall_clock_seconds
        + measurements.storage_measurement_wall_clock_seconds;
    let round_trip = CanonicalBlockFactsRoundTripSummary {
        scope: "canonical-block-facts-fixture-round-trip",
        block_prepare_concurrency: measurements.block_prepare_concurrency,
        wall_clock_seconds: measurements.wall_clock_seconds,
        storage_initialization_wall_clock_seconds: measurements
            .storage_initialization_wall_clock_seconds,
        fact_preparation_wall_clock_seconds: measurements.fact_preparation_wall_clock_seconds,
        fact_persistence_wall_clock_seconds: measurements.fact_persistence_wall_clock_seconds,
        index_construction_wall_clock_seconds: measurements.index_construction_wall_clock_seconds,
        storage_optimization_wall_clock_seconds: measurements
            .storage_optimization_wall_clock_seconds,
        validation_wall_clock_seconds: measurements.validation_wall_clock_seconds,
        publication_wall_clock_seconds: measurements.publication_wall_clock_seconds,
        fresh_reader_validation_wall_clock_seconds: measurements
            .fresh_reader_validation_wall_clock_seconds,
        storage_measurement_wall_clock_seconds: measurements.storage_measurement_wall_clock_seconds,
        unattributed_wall_clock_seconds: (measurements.wall_clock_seconds
            - attributed_wall_clock_seconds)
            .max(0.0),
        first_height: measurements.first_height,
        first_hash_hex: measurements.first_hash_hex,
        tip_height: measurements.tip_height,
        tip_hash_hex: measurements.tip_hash_hex,
        block_count,
        blocks_per_second,
        logical_fact_bytes: measurements.logical_fact_bytes,
        physical_storage_bytes: measurements.physical_storage_bytes,
        persisted_sequence_digest: measurements.persisted_sequence_digest,
        fixture_sequence_digest_match,
        replay_format_version: measurements.replay_format_version,
        semantic_replay_validated: measurements.semantic_replay_validated,
        storage: measurements.storage,
        benchmark_client_peak_rss: measurements.benchmark_client_peak_rss,
    };
    CanonicalBlockFactsRoundTripReport {
        contract_identity: REPORT_CONTRACT_IDENTITY.to_owned(),
        report_format_version: REPORT_FORMAT_VERSION,
        provenance,
        fixture,
        storage_candidate: measurements.storage_candidate,
        round_trip,
    }
    .into()
}

fn build_acceptance_summary(
    measurements: &CurrentSchemaFixtureReplayMeasurements,
) -> CurrentSchemaFixtureReplayAcceptance {
    CurrentSchemaFixtureReplayAcceptance {
        canonical_fixture_replay: summarize_acceptance_measurement(
            "fixture-range",
            measurements.wall_clock_seconds,
            measurements.canonical_fixture_replay_thresholds,
        ),
    }
}

fn build_replay_summary(
    measurements: &CurrentSchemaFixtureReplayMeasurements,
    samples: &[MetricSample],
    store_reads: &[CallerReadStat],
    rocksdb_tickers: &[TickerStat],
) -> CurrentSchemaFixtureReplaySummary {
    let projection_compaction_bytes = measurements.projection_preset.and_then(|_| {
        ticker_reading(
            rocksdb_tickers,
            ROCKSDB_COMPACT_READ_BYTES,
            PROJECTION_STORE_METRIC_ROLE,
        )
        .zip(ticker_reading(
            rocksdb_tickers,
            ROCKSDB_COMPACT_WRITE_BYTES,
            PROJECTION_STORE_METRIC_ROLE,
        ))
        .map(|(read_bytes, write_bytes)| read_bytes.saturating_add(write_bytes))
    });
    let epochs_committed = samples
        .iter()
        .any(|sample| sample.name == COMMIT_DURATION_COUNT)
        .then(|| round_to_u64(sum_by_name(samples, COMMIT_DURATION_COUNT)));
    let commit_fallback_reads = samples
        .iter()
        .any(|sample| {
            sample.name == TELEMETRY_COVERAGE_TOTAL
                && sample.label("family") == Some(STORE_READ_TELEMETRY_FAMILY)
        })
        .then(|| {
            store_reads
                .iter()
                .filter(|stat| stat.caller == COMMIT_FALLBACK_CALLER)
                .map(|stat| stat.call_count)
                .sum()
        });
    let blocks_committed = blocks_committed(
        measurements.starting_canonical_state.tip_height,
        measurements.tip_height_after,
    );
    let blocks_per_second = if measurements.wall_clock_seconds > 0.0 {
        u64_to_f64(blocks_committed) / measurements.wall_clock_seconds
    } else {
        0.0
    };
    CurrentSchemaFixtureReplaySummary {
        block_prepare_concurrency: measurements.block_prepare_concurrency,
        canonical_writer: measurements.canonical_writer,
        projection_preset: measurements.projection_preset,
        projection_replay_scope: measurements.projection_replay_scope,
        wall_clock_seconds: measurements.wall_clock_seconds,
        starting_canonical_state: measurements.starting_canonical_state.clone(),
        tip_height_after: measurements.tip_height_after,
        tip_hash_after_hex: measurements.tip_hash_after_hex.clone(),
        blocks_committed,
        epochs_committed,
        blocks_per_second,
        commit_fallback_reads,
        projection_build_wall_clock_seconds: measurements.projection_build_wall_clock_seconds,
        projection_row_count: measurements.projection_row_count,
        projection_event_cursor_at_tip: measurements.projection_event_cursor_at_tip,
        projection_store_bytes: measurements.projection_store_bytes,
        projection_logical_write_bytes: measurements.projection_logical_write_bytes,
        projection_compaction_bytes,
        projection_store_reopen_seconds: measurements.projection_store_reopen_seconds,
        peak_rss: measurements.peak_rss,
    }
}

fn summarize_acceptance_measurement(
    scope: &'static str,
    wall_clock_seconds: f64,
    thresholds: Option<AcceptanceThresholds>,
) -> AcceptanceMeasurementSummary {
    AcceptanceMeasurementSummary {
        scope,
        wall_clock_seconds,
        thresholds: thresholds.map(|thresholds| AcceptanceThresholdSummary {
            target_seconds: thresholds.target_seconds,
            hard_limit_seconds: thresholds.hard_limit_seconds,
            target_met: wall_clock_seconds <= thresholds.target_seconds,
            hard_limit_met: wall_clock_seconds <= thresholds.hard_limit_seconds,
        }),
    }
}

fn ticker_reading(tickers: &[TickerStat], ticker: &str, store_role: &str) -> Option<u64> {
    tickers
        .iter()
        .find(|stat| stat.ticker == ticker && stat.store_role == store_role)
        .map(|stat| round_to_u64(stat.reading))
}

fn blocks_committed(before: Option<u32>, after: Option<u32>) -> u64 {
    match (before, after) {
        (Some(before), Some(after)) => u64::from(after.saturating_sub(before)),
        (None, Some(after)) => u64::from(after),
        _ => 0,
    }
}

fn aggregate_store_reads(samples: &[MetricSample]) -> Vec<CallerReadStat> {
    let mut counts: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    let mut seconds: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for sample in samples {
        let is_count = sample.name == READ_DURATION_COUNT;
        let is_sum = sample.name == READ_DURATION_SUM;
        if !is_count && !is_sum {
            continue;
        }
        let (Some(caller), Some(table), Some(operation)) = (
            sample.label("caller"),
            sample.label("table"),
            sample.label("operation"),
        ) else {
            continue;
        };
        let key = (caller.to_owned(), table.to_owned(), operation.to_owned());
        if is_count {
            *counts.entry(key).or_insert(0) += round_to_u64(sample.reading);
        } else {
            *seconds.entry(key).or_insert(0.0) += sample.reading;
        }
    }
    counts
        .into_iter()
        .map(|((caller, table, operation), call_count)| {
            let task_seconds = seconds
                .get(&(caller.clone(), table.clone(), operation.clone()))
                .copied()
                .unwrap_or(0.0);
            CallerReadStat {
                caller,
                table,
                operation,
                call_count,
                task_seconds,
            }
        })
        .collect()
}

fn aggregate_multi_get(samples: &[MetricSample]) -> Vec<MultiGetStat> {
    let mut keys: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut resolved: BTreeMap<(String, String), u64> = BTreeMap::new();
    for sample in samples {
        let is_keys = sample.name == MULTI_GET_KEYS_TOTAL;
        let is_resolved = sample.name == MULTI_GET_RESOLVED_TOTAL;
        if !is_keys && !is_resolved {
            continue;
        }
        let (Some(table), Some(caller)) = (sample.label("table"), sample.label("caller")) else {
            continue;
        };
        let key = (table.to_owned(), caller.to_owned());
        if is_keys {
            *keys.entry(key).or_insert(0) += round_to_u64(sample.reading);
        } else {
            *resolved.entry(key).or_insert(0) += round_to_u64(sample.reading);
        }
    }
    keys.into_iter()
        .map(|((table, caller), keys_total)| {
            let resolved_total = resolved
                .get(&(table.clone(), caller.clone()))
                .copied()
                .unwrap_or(0);
            MultiGetStat {
                table,
                caller,
                keys_total,
                resolved_total,
            }
        })
        .collect()
}

fn aggregate_head_of_line_wait(samples: &[MetricSample]) -> Vec<StageWaitStat> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut seconds: BTreeMap<String, f64> = BTreeMap::new();
    for sample in samples {
        let is_count = sample.name == HEAD_OF_LINE_WAIT_COUNT;
        let is_sum = sample.name == HEAD_OF_LINE_WAIT_SUM;
        if !is_count && !is_sum {
            continue;
        }
        let Some(stage) = sample.label("stage") else {
            continue;
        };
        if is_count {
            *counts.entry(stage.to_owned()).or_insert(0) += round_to_u64(sample.reading);
        } else {
            *seconds.entry(stage.to_owned()).or_insert(0.0) += sample.reading;
        }
    }
    counts
        .into_iter()
        .map(|(stage, wait_count)| {
            let wait_seconds = seconds.get(&stage).copied().unwrap_or(0.0);
            StageWaitStat {
                stage,
                wait_count,
                wait_seconds,
            }
        })
        .collect()
}

fn aggregate_stage_durations(samples: &[MetricSample]) -> Vec<StageDurationStat> {
    let metric_families = [
        (
            "block_prepare",
            BLOCK_PREPARE_STAGE_DURATION_COUNT,
            BLOCK_PREPARE_STAGE_DURATION_SUM,
        ),
        (
            "canonical_block_construction",
            CANONICAL_BLOCK_CONSTRUCTION_STAGE_DURATION_COUNT,
            CANONICAL_BLOCK_CONSTRUCTION_STAGE_DURATION_SUM,
        ),
    ];
    let mut counts: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    let mut seconds: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for sample in samples {
        let Some((family, is_count)) =
            metric_families
                .iter()
                .find_map(|(family, count_name, sum_name)| {
                    if sample.name == *count_name {
                        Some((*family, true))
                    } else if sample.name == *sum_name {
                        Some((*family, false))
                    } else {
                        None
                    }
                })
        else {
            continue;
        };
        let (Some(stage), Some(status)) = (sample.label("stage"), sample.label("status")) else {
            continue;
        };
        let key = (family.to_owned(), stage.to_owned(), status.to_owned());
        if is_count {
            *counts.entry(key).or_insert(0) += round_to_u64(sample.reading);
        } else {
            *seconds.entry(key).or_insert(0.0) += sample.reading;
        }
    }
    counts
        .into_iter()
        .map(|((family, stage, status), call_count)| {
            let task_seconds = seconds
                .get(&(family.clone(), stage.clone(), status.clone()))
                .copied()
                .unwrap_or(0.0);
            StageDurationStat {
                family,
                stage,
                status,
                call_count,
                task_seconds,
            }
        })
        .collect()
}

fn aggregate_tickers(samples: &[MetricSample]) -> Vec<TickerStat> {
    let mut tickers: BTreeMap<(String, String), f64> = BTreeMap::new();
    for sample in samples {
        if sample.name != ROCKSDB_TICKER {
            continue;
        }
        let (Some(ticker), Some(store_role)) = (sample.label("ticker"), sample.label("store_role"))
        else {
            continue;
        };
        tickers.insert((ticker.to_owned(), store_role.to_owned()), sample.reading);
    }
    tickers
        .into_iter()
        .map(|((ticker, store_role), reading)| TickerStat {
            ticker,
            store_role,
            reading,
        })
        .collect()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Prometheus counters are rendered as non-negative integers within u64 range"
)]
fn round_to_u64(reading: f64) -> u64 {
    if reading.is_finite() && reading >= 0.0 {
        reading.round() as u64
    } else {
        0
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "block counts fit well within f64 exact-integer range for a bounded benchmark range"
)]
fn u64_to_f64(amount: u64) -> f64 {
    amount as f64
}

#[cfg(test)]
mod tests {
    use crate::{fixture::WorkloadDensity, rss::PEAK_RSS_SOURCE_UNAVAILABLE};

    use super::{
        AcceptanceThresholds, CanonicalBlockFactsRoundTripMeasurements,
        CanonicalBlockFactsStorageEvidence, CanonicalFactSequenceDigestSummary,
        CurrentSchemaFixtureReplayMeasurements, CurrentSchemaReplayWriterSettings,
        FixtureCachePolicy, FixtureSummary, RocksDbResourceBudgetSummary, StartingCanonicalState,
        StartingCanonicalStateKind, StorageCandidateIdentity, aggregate_stage_durations,
        build_canonical_block_facts_round_trip_report, build_current_schema_fixture_replay_report,
        is_valid_benchmark_trial_id, parse_prometheus_samples,
    };

    #[test]
    fn benchmark_trial_ids_are_filename_safe() {
        for trial_id in ["trial-01", "A.2_warm", "9"] {
            assert!(is_valid_benchmark_trial_id(trial_id), "{trial_id}");
        }
        for trial_id in ["", "-trial", "trial 01", "trial/01", "trialé"] {
            assert!(!is_valid_benchmark_trial_id(trial_id), "{trial_id}");
        }
    }

    #[test]
    fn canonical_replay_reports_acceptance_provenance_and_current_schema_oracle() {
        let mut measurements = canonical_measurements();
        measurements.software_revision = Some("0123456789abcdef".to_owned());
        measurements.runner_id = Some("linux-amd64-runner-01".to_owned());
        measurements.cpu_limit_cores = Some(8.0);
        measurements.memory_limit_bytes = Some(16 * 1024 * 1024 * 1024);
        measurements.storage_class = Some("local-nvme".to_owned());
        measurements.image_reference = Some(format!("sha256:{}", "a".repeat(64)));
        measurements.trial_id = Some("trial-01".to_owned());
        measurements.fixture_cache_policy = Some(FixtureCachePolicy::Warm);
        let report =
            build_current_schema_fixture_replay_report(fixture_summary(), &measurements, None);

        assert_eq!(report.report_format_version, 1);
        assert_eq!(report.contract_identity, super::REPORT_CONTRACT_IDENTITY);
        assert_provenance_and_writer(&report);
        assert_eq!(report.provenance.run.trial_id.as_deref(), Some("trial-01"));
        assert!(matches!(
            report.provenance.run.fixture_cache_policy,
            Some(FixtureCachePolicy::Warm)
        ));
        assert_eq!(report.provenance.run.started_at_unix_millis, 1_000);
        assert_eq!(report.provenance.run.completed_at_unix_millis, 14_000);
        assert_eq!(report.fixture.fixture_format_version, 1);
        assert_eq!(
            report.fixture.contract_identity,
            crate::fixture::FIXTURE_CONTRACT_IDENTITY
        );
        assert_eq!(
            report.fixture.current_schema_oracle_artifact_schema_version,
            18
        );
        assert_eq!(report.fixture.tip_hash_hex, "abcd");
        assert_eq!(report.fixture.digest_sha256, "fixture-digest");
        assert_eq!(
            report.replay.starting_canonical_state.chain_epoch_id,
            Some(42)
        );
        assert_eq!(
            report
                .replay
                .starting_canonical_state
                .tip_hash_rpc_hex
                .as_deref(),
            Some("starting-tip-rpc-hash")
        );
        assert_eq!(
            report
                .replay
                .starting_canonical_state
                .checkpoint_manifest_sha256
                .as_deref(),
            Some("checkpoint-manifest-digest")
        );
        assert_eq!(
            report
                .replay
                .starting_canonical_state
                .artifact_schema_version,
            Some(18)
        );
        assert!(
            (report
                .acceptance
                .canonical_fixture_replay
                .wall_clock_seconds
                - 12.5)
                .abs()
                < f64::EPSILON
        );
        assert_eq!(
            report.acceptance.canonical_fixture_replay.scope,
            "fixture-range"
        );
        assert!(
            report
                .acceptance
                .canonical_fixture_replay
                .thresholds
                .is_none()
        );
    }

    #[test]
    fn current_schema_report_wrapper_serializes_the_measurement_kind()
    -> Result<(), serde_json::Error> {
        let report = build_current_schema_fixture_replay_report(
            fixture_summary(),
            &canonical_measurements(),
            None,
        );
        let encoded = serde_json::to_value(super::BenchmarkReport::from(report))?;

        assert_eq!(encoded["measurement_kind"], "current-schema-fixture-replay");
        assert_eq!(encoded["contract_identity"], "benchmark-report");
        assert_eq!(encoded["fixture"]["contract_identity"], "canonical-fixture");
        Ok(())
    }

    fn assert_provenance_and_writer(report: &super::CurrentSchemaFixtureReplayReport) {
        assert_eq!(report.storage_candidate.id, "rocksdb-current-schema-oracle");
        assert_eq!(report.storage_candidate.canonical_engine, "rocksdb");
        assert_eq!(
            report.storage_candidate.canonical_model,
            "projection-coupled-current-schema"
        );
        assert_eq!(report.storage_candidate.diagnostic_projection_engine, None);
        assert_eq!(report.storage_candidate.topology, "rocksdb-single-host");
        assert_eq!(
            report.provenance.software_revision.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            report.provenance.runner.id.as_deref(),
            Some("linux-amd64-runner-01")
        );
        assert_eq!(report.provenance.runner.cpu_limit_cores, Some(8.0));
        assert_eq!(
            report.provenance.runner.memory_limit_bytes,
            Some(16 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            report.provenance.runner.storage_class.as_deref(),
            Some("local-nvme")
        );
        assert_eq!(
            report
                .replay
                .canonical_writer
                .rocksdb_resource_budget
                .block_cache_bytes,
            64 * 1024 * 1024
        );
        assert!(report.replay.canonical_writer.sync_writes);
    }

    #[test]
    fn measured_acceptance_reports_threshold_results() -> Result<(), crate::BenchError> {
        let mut measurements = canonical_measurements();
        measurements.canonical_fixture_replay_thresholds =
            Some(AcceptanceThresholds::try_from_seconds(10.0, 15.0)?);
        let exposition = "zinder_ingest_commit_duration_seconds_count 1\n\
            zinder_bench_telemetry_coverage_total{family=\"store_reads\"} 0\n";
        let report = build_current_schema_fixture_replay_report(
            fixture_summary(),
            &measurements,
            Some(exposition),
        );

        let Some(thresholds) = report.acceptance.canonical_fixture_replay.thresholds else {
            return Err(crate::BenchError::invalid_argument(
                "expected supplied acceptance thresholds in the report",
            ));
        };
        assert!((thresholds.target_seconds - 10.0).abs() < f64::EPSILON);
        assert!((thresholds.hard_limit_seconds - 15.0).abs() < f64::EPSILON);
        assert!(!thresholds.target_met);
        assert!(thresholds.hard_limit_met);
        report.validate_acceptance()?;

        Ok(())
    }

    #[test]
    fn thresholded_acceptance_rejects_missing_telemetry() -> Result<(), crate::BenchError> {
        let mut measurements = canonical_measurements();
        measurements.canonical_fixture_replay_thresholds =
            Some(AcceptanceThresholds::try_from_seconds(10.0, 15.0)?);
        let report =
            build_current_schema_fixture_replay_report(fixture_summary(), &measurements, None);

        let Some(error) = report.validate_acceptance().err() else {
            return Err(crate::BenchError::invalid_argument(
                "missing telemetry must fail thresholded acceptance",
            ));
        };
        assert!(error.to_string().contains("epochs_committed"));
        assert!(error.to_string().contains("commit_fallback_reads"));
        Ok(())
    }

    #[test]
    fn thresholded_acceptance_rejects_incomplete_fixture_evidence() -> Result<(), crate::BenchError>
    {
        let mut measurements = canonical_measurements();
        measurements.canonical_fixture_replay_thresholds =
            Some(AcceptanceThresholds::try_from_seconds(10.0, 15.0)?);
        measurements.tip_hash_after_hex = Some("wrong-tip".to_owned());
        let exposition = "zinder_ingest_commit_duration_seconds_count 1\n\
            zinder_bench_telemetry_coverage_total{family=\"store_reads\"} 0\n";
        let report = build_current_schema_fixture_replay_report(
            fixture_summary(),
            &measurements,
            Some(exposition),
        );

        let Some(error) = report.validate_acceptance().err() else {
            return Err(crate::BenchError::invalid_argument(
                "mismatched final tip must fail thresholded acceptance",
            ));
        };
        assert!(error.to_string().contains("completion evidence mismatch"));
        Ok(())
    }

    #[test]
    fn acceptance_thresholds_reject_invalid_duration_pairs() {
        assert!(AcceptanceThresholds::try_from_seconds(0.0, 1.0).is_err());
        assert!(AcceptanceThresholds::try_from_seconds(2.0, 1.0).is_err());
        assert!(AcceptanceThresholds::try_from_seconds(f64::NAN, 1.0).is_err());
        assert!(AcceptanceThresholds::try_from_seconds(1.0, f64::INFINITY).is_err());
    }

    #[test]
    fn diagnostic_projection_presets_do_not_create_target_plane_acceptance_contracts()
    -> Result<(), Box<dyn std::error::Error>> {
        for projection_preset in ["wallet", "explorer"] {
            let mut measurements = canonical_measurements();
            measurements.projection_preset = Some(projection_preset);
            measurements.projection_replay_scope = Some("retained-history");
            measurements.projection_build_wall_clock_seconds = Some(5.0);
            measurements.storage_candidate =
                StorageCandidateIdentity::rocksdb_current_schema_with_diagnostic_projections();

            let report =
                build_current_schema_fixture_replay_report(fixture_summary(), &measurements, None);
            let report_json = serde_json::to_value(&report)?;

            assert!(report_json.get("lifecycle").is_none());
            assert!(report_json["acceptance"].get("wallet_build").is_none());
            assert!(
                report_json["acceptance"]
                    .get("wallet_build_lifecycle")
                    .is_none()
            );
            assert_eq!(
                report.storage_candidate.diagnostic_projection_engine,
                Some("rocksdb")
            );
        }
        Ok(())
    }

    #[test]
    fn fact_round_trip_reports_semantic_replay_and_omits_unmeasured_contracts()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut report = build_canonical_block_facts_round_trip_report(
            fixture_summary(),
            CanonicalBlockFactsRoundTripMeasurements {
                block_prepare_concurrency: 8,
                wall_clock_seconds: 2.3,
                storage_initialization_wall_clock_seconds: 0.1,
                fact_preparation_wall_clock_seconds: 0.8,
                fact_persistence_wall_clock_seconds: 0.7,
                index_construction_wall_clock_seconds: 0.0,
                storage_optimization_wall_clock_seconds: 0.0,
                validation_wall_clock_seconds: 0.4,
                publication_wall_clock_seconds: 0.1,
                fresh_reader_validation_wall_clock_seconds: 0.1,
                storage_measurement_wall_clock_seconds: 0.1,
                first_height: 11,
                first_hash_hex: "first-hash".to_owned(),
                tip_height: 20,
                tip_hash_hex: "abcd".to_owned(),
                logical_fact_bytes: 4_096,
                physical_storage_bytes: 8_192,
                persisted_sequence_digest: CanonicalFactSequenceDigestSummary {
                    block_digest_version: 1,
                    sequence_digest_version: 1,
                    block_count: 10,
                    sha256: "persisted-digest".to_owned(),
                },
                replay_format_version: 1,
                semantic_replay_validated: true,
                storage: CanonicalBlockFactsStorageEvidence::RocksDb {
                    storage_schema_version: 1,
                    ingestion_mode: "external-sst",
                    durability_mode: "external-sst-ingest-with-synchronous-completion-marker",
                    database_io_mode: "buffered".to_owned(),
                    external_sst_io_mode: "buffered",
                    compression: "snappy",
                    external_sst_bytes: 7_000,
                    rocksdb_resource_budget: RocksDbResourceBudgetSummary::from(
                        zinder_store::RocksDbResourceBudget::for_local_tests(),
                    ),
                },
                benchmark_client_peak_rss: crate::rss::PeakRss {
                    bytes: None,
                    source: PEAK_RSS_SOURCE_UNAVAILABLE,
                },
                storage_candidate: StorageCandidateIdentity::rocksdb_fact_first(),
                software_revision: None,
                trial_id: Some("trial-01".to_owned()),
                fixture_cache_policy: Some(FixtureCachePolicy::Warm),
                run_started_at_unix_millis: 1_000,
                run_completed_at_unix_millis: 4_000,
                runner_id: None,
                cpu_limit_cores: Some(8.0),
                memory_limit_bytes: Some(16 * 1024 * 1024 * 1024),
                storage_class: None,
                image_reference: None,
            },
        );
        report.validate()?;
        let json = serde_json::to_value(&report)?;

        assert_eq!(json["measurement_kind"], "canonical-block-facts-round-trip");
        assert_eq!(json["contract_identity"], "benchmark-report");
        assert_eq!(json["fixture"]["contract_identity"], "canonical-fixture");
        assert_eq!(json["storage_candidate"]["id"], "rocksdb-fact-first");
        assert_eq!(json["round_trip"]["replay_format_version"], 1);
        assert_eq!(json["round_trip"]["semantic_replay_validated"], true);
        assert_eq!(json["provenance"]["run"]["trial_id"], "trial-01");
        assert_eq!(json["provenance"]["run"]["fixture_cache_policy"], "warm");
        for omitted_field in ["acceptance", "replay", "lifecycle", "store_reads"] {
            assert!(json.get(omitted_field).is_none(), "{omitted_field}");
        }
        if let super::BenchmarkReport::CanonicalBlockFactsRoundTrip(fact_report) = &mut report {
            fact_report.round_trip.replay_format_version = 99;
        }
        assert!(report.validate().is_err());
        Ok(())
    }

    #[test]
    fn report_v1_rejects_old_contract_identities() {
        let mut report = super::BenchmarkReport::from(build_current_schema_fixture_replay_report(
            fixture_summary(),
            &canonical_measurements(),
            None,
        ));
        if let super::BenchmarkReport::CurrentSchemaFixtureReplay(report) = &mut report {
            report.contract_identity = "zinder-benchmark-report".to_owned();
        }
        assert!(report.validate().is_err());

        let mut report = super::BenchmarkReport::from(build_current_schema_fixture_replay_report(
            fixture_summary(),
            &canonical_measurements(),
            None,
        ));
        if let super::BenchmarkReport::CurrentSchemaFixtureReplay(report) = &mut report {
            report.fixture.contract_identity = "zinder-bench-fixture-manifest".to_owned();
        }
        assert!(report.validate().is_err());
    }

    #[test]
    fn stage_duration_report_preserves_family_stage_and_status() {
        let samples = parse_prometheus_samples(
            "zinder_ingest_canonical_block_construction_stage_duration_seconds_count{stage=\"block_parse\",status=\"ok\"} 4\n\
             zinder_ingest_canonical_block_construction_stage_duration_seconds_sum{stage=\"block_parse\",status=\"ok\"} 1.5\n",
        );

        let stats = aggregate_stage_durations(&samples);

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].family, "canonical_block_construction");
        assert_eq!(stats[0].stage, "block_parse");
        assert_eq!(stats[0].status, "ok");
        assert_eq!(stats[0].call_count, 4);
        assert!((stats[0].task_seconds - 1.5).abs() < f64::EPSILON);
    }

    fn fixture_summary() -> FixtureSummary {
        FixtureSummary {
            contract_identity: crate::fixture::FIXTURE_CONTRACT_IDENTITY.to_owned(),
            fixture_format_version: 1,
            current_schema_oracle_artifact_schema_version: 18,
            canonical_block_facts_digest_evidence:
                crate::fixture::CanonicalBlockFactsDigestEvidence {
                    block_digest_version: 1,
                    sequence_digest_version: 1,
                    block_count: 10,
                    sequence_digest_sha256: "persisted-digest".to_owned(),
                },
            tip_hash_hex: "abcd".to_owned(),
            digest_sha256: "fixture-digest".to_owned(),
            network: "zcash-regtest".to_owned(),
            from_height: 11,
            to_height: 20,
            block_count: 10,
            workload_density: WorkloadDensity {
                block_count: 10,
                ..WorkloadDensity::default()
            },
            segment_count: 1,
        }
    }

    fn canonical_measurements() -> CurrentSchemaFixtureReplayMeasurements {
        CurrentSchemaFixtureReplayMeasurements {
            block_prepare_concurrency: 8,
            canonical_writer: CurrentSchemaReplayWriterSettings {
                store_schema_version: 13,
                artifact_schema_version: 18,
                sync_writes: true,
                durability_mode: "rocksdb-wal-fsync-per-write-batch",
                rocksdb_resource_budget: RocksDbResourceBudgetSummary {
                    block_cache_bytes: 64 * 1024 * 1024,
                    max_wal_bytes: 32 * 1024 * 1024,
                    max_open_files: 128,
                    write_buffer_bytes: 8 * 1024 * 1024,
                    max_write_buffer_count: 2,
                    max_background_jobs: 2,
                    memtable_budget_bytes: 16 * 1024 * 1024,
                    statistics_level: "tickers",
                },
            },
            projection_preset: None,
            projection_replay_scope: None,
            wall_clock_seconds: 12.5,
            starting_canonical_state: StartingCanonicalState {
                kind: StartingCanonicalStateKind::Checkpoint,
                chain_epoch_id: Some(42),
                tip_height: Some(10),
                tip_hash_rpc_hex: Some("starting-tip-rpc-hash".to_owned()),
                artifact_schema_version: Some(18),
                checkpoint_manifest_sha256: Some("checkpoint-manifest-digest".to_owned()),
            },
            tip_height_after: Some(20),
            tip_hash_after_hex: Some("abcd".to_owned()),
            projection_build_wall_clock_seconds: None,
            projection_row_count: None,
            projection_event_cursor_at_tip: None,
            projection_store_bytes: None,
            projection_store_reopen_seconds: None,
            projection_logical_write_bytes: None,
            peak_rss: crate::rss::PeakRss {
                bytes: None,
                source: PEAK_RSS_SOURCE_UNAVAILABLE,
            },
            storage_candidate: StorageCandidateIdentity::rocksdb_current_schema_oracle(),
            software_revision: None,
            trial_id: None,
            fixture_cache_policy: None,
            run_started_at_unix_millis: 1_000,
            run_completed_at_unix_millis: 14_000,
            runner_id: None,
            cpu_limit_cores: None,
            memory_limit_bytes: None,
            storage_class: None,
            image_reference: None,
            canonical_fixture_replay_thresholds: None,
        }
    }
}
