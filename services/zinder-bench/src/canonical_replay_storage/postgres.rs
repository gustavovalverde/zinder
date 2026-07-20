//! Concrete `PostgreSQL` storage benchmark for canonical replay records.

use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    time::Instant,
};

use futures_util::TryStreamExt as _;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_postgres::{
    Client, NoTls, Row, Transaction,
    binary_copy::BinaryCopyInWriter,
    types::{FromSqlOwned, ToSql, Type},
};
use zinder_core::{
    BlockHash, BlockHeight, CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigest,
    CanonicalBlockFactsSequenceDigestVersion,
};

use crate::{
    BenchError,
    canonical_replay_storage::{
        CanonicalReplayRecord, CanonicalReplaySequenceAccumulator, CanonicalReplaySequencePosition,
        CanonicalReplayStorageTimings, PersistedCanonicalReplayRow, prepare_fixture_segment,
    },
    fixture::FixtureManifest,
};

const CANDIDATE_ID: &str = "postgres-canonical-replay-storage";
const CANONICAL_REPLAY_CONTRACT_IDENTITY: &str = "zinder-canonical-replay";
const SCHEMA_NAME: &str = "zinder_bench_canonical_replay_storage";

type PostgresConnectionTask = JoinHandle<Result<(), tokio_postgres::Error>>;

/// Physical schema version written by the concrete `PostgreSQL` replay candidate.
pub const POSTGRES_CANONICAL_REPLAY_SCHEMA_VERSION: u16 = 1;
/// Explicit TOAST compression used for canonical-replay replay encodings.
pub const POSTGRES_REPLAY_ENVELOPE_COMPRESSION: &str = "lz4";

const SCHEMA_SQL: &str = r"
CREATE SCHEMA zinder_bench_canonical_replay_storage;

CREATE TABLE zinder_bench_canonical_replay_storage.canonical_replay_storage (
    height BIGINT NOT NULL,
    block_hash BYTEA NOT NULL,
    parent_hash BYTEA NOT NULL,
    transaction_count BIGINT NOT NULL,
    digest_version SMALLINT NOT NULL,
    block_facts_digest BYTEA NOT NULL,
    replay_envelope BYTEA COMPRESSION lz4 NOT NULL
);

CREATE TABLE zinder_bench_canonical_replay_storage.round_trip_completion (
    singleton BOOLEAN PRIMARY KEY CHECK (singleton),
    physical_schema_version SMALLINT NOT NULL,
    fixture_format_version INTEGER NOT NULL,
    fixture_digest_sha256 BYTEA NOT NULL CHECK (octet_length(fixture_digest_sha256) = 32),
    network TEXT NOT NULL,
    from_height BIGINT NOT NULL,
    to_height BIGINT NOT NULL,
    block_count BIGINT NOT NULL,
    transaction_count BIGINT NOT NULL,
    tip_hash BYTEA NOT NULL CHECK (octet_length(tip_hash) = 32),
    block_digest_version SMALLINT NOT NULL,
    replay_format_version BIGINT NOT NULL
        CHECK (replay_format_version BETWEEN 1 AND 4294967295),
    sequence_digest_version SMALLINT NOT NULL,
    sequence_digest BYTEA NOT NULL CHECK (octet_length(sequence_digest) = 32),
    logical_replay_bytes BIGINT NOT NULL,
    contract_identity TEXT NOT NULL CHECK (contract_identity = 'zinder-canonical-replay')
);
";

const COPY_SQL: &str = r"
COPY zinder_bench_canonical_replay_storage.canonical_replay_storage (
    height,
    block_hash,
    parent_hash,
    transaction_count,
    digest_version,
    block_facts_digest,
    replay_envelope
) FROM STDIN BINARY
";

const FINALIZE_SCHEMA_SQL: &str = r"
ALTER TABLE zinder_bench_canonical_replay_storage.canonical_replay_storage
    ADD CONSTRAINT canonical_replay_storage_shape_check CHECK (
        height BETWEEN 0 AND 4294967295
        AND octet_length(block_hash) = 32
        AND octet_length(parent_hash) = 32
        AND transaction_count BETWEEN 0 AND 4294967295
        AND digest_version > 0
        AND octet_length(block_facts_digest) = 32
        AND octet_length(replay_envelope) > 0
    ) NOT VALID;
ALTER TABLE zinder_bench_canonical_replay_storage.canonical_replay_storage
    VALIDATE CONSTRAINT canonical_replay_storage_shape_check;
CREATE UNIQUE INDEX canonical_replay_storage_height_uq
    ON zinder_bench_canonical_replay_storage.canonical_replay_storage (height);
ALTER TABLE zinder_bench_canonical_replay_storage.canonical_replay_storage
    ADD CONSTRAINT canonical_replay_storage_pkey
    PRIMARY KEY USING INDEX canonical_replay_storage_height_uq;
";

const ANALYZE_SQL: &str = "ANALYZE zinder_bench_canonical_replay_storage.canonical_replay_storage";

const READ_BACK_SQL: &str = r"
SELECT
    height,
    block_hash,
    parent_hash,
    transaction_count,
    digest_version,
    block_facts_digest,
    replay_envelope
FROM zinder_bench_canonical_replay_storage.canonical_replay_storage
ORDER BY height
";

const INSERT_COMPLETION_SQL: &str = r"
INSERT INTO zinder_bench_canonical_replay_storage.round_trip_completion (
    singleton,
    physical_schema_version,
    fixture_format_version,
    fixture_digest_sha256,
    network,
    from_height,
    to_height,
    block_count,
    transaction_count,
    tip_hash,
    block_digest_version,
    replay_format_version,
    sequence_digest_version,
    sequence_digest,
    logical_replay_bytes,
    contract_identity
) VALUES (
    TRUE, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
)
";

const READ_COMPLETION_SQL: &str = r"
SELECT
    physical_schema_version,
    fixture_format_version,
    fixture_digest_sha256,
    network,
    from_height,
    to_height,
    block_count,
    transaction_count,
    tip_hash,
    block_digest_version,
    replay_format_version,
    sequence_digest_version,
    sequence_digest,
    logical_replay_bytes,
    contract_identity
FROM zinder_bench_canonical_replay_storage.round_trip_completion
WHERE singleton = TRUE
";

const SERVER_SETTINGS_SQL: &str = r"
SELECT
    current_setting('server_version'),
    current_setting('server_version_num')::INTEGER,
    current_setting('max_connections')::INTEGER,
    pg_size_bytes(current_setting('shared_buffers'))::BIGINT,
    pg_size_bytes(current_setting('effective_cache_size'))::BIGINT,
    pg_size_bytes(current_setting('maintenance_work_mem'))::BIGINT,
    pg_size_bytes(current_setting('work_mem'))::BIGINT,
    pg_size_bytes(current_setting('max_wal_size'))::BIGINT,
    pg_size_bytes(current_setting('min_wal_size'))::BIGINT,
    EXTRACT(EPOCH FROM current_setting('checkpoint_timeout')::INTERVAL)::BIGINT,
    current_setting('checkpoint_completion_target')::DOUBLE PRECISION,
    current_setting('wal_compression'),
    current_setting('password_encryption'),
    current_setting('max_worker_processes')::INTEGER,
    current_setting('max_parallel_workers')::INTEGER,
    current_setting('max_parallel_maintenance_workers')::INTEGER,
    current_setting('track_io_timing')::BOOLEAN,
    current_setting('huge_pages'),
    current_setting('fsync')::BOOLEAN,
    current_setting('full_page_writes')::BOOLEAN,
    current_setting('synchronous_commit'),
    current_setting('wal_level'),
    current_setting('data_checksums')::BOOLEAN
";

const STORAGE_BYTES_SQL: &str = r"
SELECT
    pg_table_size(
        'zinder_bench_canonical_replay_storage.canonical_replay_storage'::regclass
    )::BIGINT,
    pg_indexes_size(
        'zinder_bench_canonical_replay_storage.canonical_replay_storage'::regclass
    )::BIGINT,
    COALESCE(SUM(pg_total_relation_size(class.oid)), 0)::BIGINT
FROM pg_class AS class
JOIN pg_namespace AS namespace ON namespace.oid = class.relnamespace
WHERE namespace.nspname = 'zinder_bench_canonical_replay_storage'
  AND class.relkind = 'r'
";

/// Inputs for one disposable `PostgreSQL` canonical-replay round trip.
///
/// The database URL is intentionally private and this type does not implement
/// `Debug`, preventing routine diagnostics from printing credentials. The URL
/// must identify an operator-controlled server; this diagnostic is not a client
/// for untrusted `PostgreSQL` endpoints.
pub struct PostgresCanonicalReplayConfig {
    fixture_directory: PathBuf,
    database_url: String,
    block_prepare_concurrency: NonZeroU32,
}

impl PostgresCanonicalReplayConfig {
    /// Creates a concrete round-trip configuration.
    #[must_use]
    pub fn new(
        fixture_directory: impl Into<PathBuf>,
        database_url: impl Into<String>,
        block_prepare_concurrency: NonZeroU32,
    ) -> Self {
        Self {
            fixture_directory: fixture_directory.into(),
            database_url: database_url.into(),
            block_prepare_concurrency,
        }
    }

    fn validate(&self) -> Result<(), BenchError> {
        if self.database_url.trim().is_empty() {
            return Err(candidate_error("database URL must not be blank"));
        }
        Ok(())
    }
}

/// Effective server settings that define this measured durability posture.
#[allow(
    clippy::struct_excessive_bools,
    reason = "the evidence mirrors independent PostgreSQL boolean settings without collapsing their meaning"
)]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PostgresServerSettings {
    /// Server version string.
    pub server_version: String,
    /// Numeric server version.
    pub server_version_number: u32,
    /// Maximum concurrent database connections.
    pub max_connections: u32,
    /// Shared buffer pool size.
    pub shared_buffers_bytes: u64,
    /// Planner estimate of the effective filesystem cache.
    pub effective_cache_size_bytes: u64,
    /// Memory available to maintenance operations.
    pub maintenance_work_mem_bytes: u64,
    /// Memory available to each query work operation.
    pub work_mem_bytes: u64,
    /// Maximum WAL retained between checkpoints.
    pub max_wal_size_bytes: u64,
    /// Minimum WAL retained between checkpoints.
    pub min_wal_size_bytes: u64,
    /// Maximum time between automatic checkpoints.
    pub checkpoint_timeout_seconds: u64,
    /// Target fraction of a checkpoint interval used to flush pages.
    pub checkpoint_completion_target: f64,
    /// Effective WAL compression mode.
    pub wal_compression: String,
    /// Server default used when future role passwords are created or changed.
    ///
    /// The controlled test topology separately proves its host authentication
    /// rule; this setting alone does not identify the current session's method.
    pub password_encryption_default: String,
    /// Maximum worker processes.
    pub max_worker_processes: u32,
    /// Maximum parallel workers.
    pub max_parallel_workers: u32,
    /// Maximum parallel maintenance workers.
    pub max_parallel_maintenance_workers: u32,
    /// Whether server I/O timing is collected.
    pub track_io_timing: bool,
    /// Effective huge-page allocation mode.
    pub huge_pages: String,
    /// Whether server fsync is enabled.
    pub fsync: bool,
    /// Whether full-page writes are enabled.
    pub full_page_writes: bool,
    /// Effective synchronous-commit mode.
    pub synchronous_commit: String,
    /// Effective WAL level.
    pub wal_level: String,
    /// Whether the database cluster uses data checksums.
    pub data_checksums: bool,
}

impl PostgresServerSettings {
    fn validate_required_posture(&self) -> Result<(), BenchError> {
        if !self.fsync {
            return Err(candidate_error("server setting fsync must be on"));
        }
        if !self.full_page_writes {
            return Err(candidate_error(
                "server setting full_page_writes must be on",
            ));
        }
        if self.synchronous_commit != "on" {
            return Err(candidate_error(
                "server setting synchronous_commit must be on",
            ));
        }
        if self.password_encryption_default != "scram-sha-256" {
            return Err(candidate_error(
                "server setting password_encryption must be scram-sha-256",
            ));
        }
        Ok(())
    }
}

/// Persisted logical position and backend-neutral digest proven by read-back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PostgresCanonicalReplayValidation {
    /// Ordered first/tip identity, counts, and logical payload bytes.
    pub position: CanonicalReplaySequencePosition,
    /// Sum of transaction counts stored in the block envelopes.
    pub transaction_count: u64,
    /// Semantic replay format version decoded from every persisted row.
    pub replay_format_version: u32,
    /// Ordered backend-neutral digest recomputed from persisted replay records.
    pub sequence_digest: CanonicalBlockFactsSequenceDigest,
}

/// Physical bytes written by the concrete `PostgreSQL` candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PostgresStorageMeasurements {
    /// Semantic replay encoding bytes submitted across all block rows.
    pub logical_replay_bytes: u64,
    /// Replay-table heap, TOAST, free-space-map, and visibility-map bytes.
    pub replay_table_bytes: u64,
    /// Deferred replay-table index bytes.
    pub index_bytes: u64,
    /// Total bytes across ordinary tables and their indexes in the fixed schema.
    pub schema_bytes: u64,
    /// WAL bytes advanced between schema creation and completed publication.
    pub wal_bytes: u64,
}

/// Completed concrete `PostgreSQL` canonical-replay round trip.
#[derive(Clone, Debug, PartialEq)]
pub struct PostgresCanonicalReplayReport {
    /// Effective server and durability settings.
    pub server_settings: PostgresServerSettings,
    /// Persisted replay records validated before publication and through a fresh connection.
    pub validation: PostgresCanonicalReplayValidation,
    /// Logical and physical byte measurements.
    pub storage: PostgresStorageMeasurements,
    /// Direct stage timings.
    pub timings: CanonicalReplayStorageTimings,
}

/// Creates, loads, validates, publishes, reconnects, and revalidates the fixed
/// `PostgreSQL` canonical-replay schema.
///
/// The target schema must not already exist. This function never drops or
/// truncates database objects and never includes the supplied database URL in
/// an error.
#[allow(
    clippy::too_many_lines,
    reason = "the concrete lifecycle keeps its measured stage boundaries visible"
)]
pub async fn run_postgres_canonical_replay_storage(
    config: PostgresCanonicalReplayConfig,
) -> Result<PostgresCanonicalReplayReport, BenchError> {
    let total_started_at = Instant::now();
    let initialization_started_at = Instant::now();
    config.validate()?;
    let manifest = FixtureManifest::read(&config.fixture_directory)?;
    validate_supported_oracle(&manifest)?;
    let network = manifest.network_typed()?;
    let activations = Arc::new(manifest.activations_typed()?);

    let (mut client, connection_task) = connect_postgres(&config.database_url).await?;
    let server_settings = read_server_settings(&client).await?;
    server_settings.validate_required_posture()?;
    reject_existing_schema(&client).await?;
    let starting_wal_lsn = current_wal_insert_lsn(&client).await?;
    create_schema(&mut client).await?;
    let mut timings = CanonicalReplayStorageTimings {
        storage_initialization_seconds: initialization_started_at.elapsed().as_secs_f64(),
        ..CanonicalReplayStorageTimings::default()
    };

    let mut input_accumulator = CanonicalReplaySequenceAccumulator::new();
    let mut input_transaction_count = 0_u64;
    let load_transaction_started_at = Instant::now();
    let load_transaction = client
        .transaction()
        .await
        .map_err(|error| postgres_error("replay load transaction start", &error))?;
    timings.replay_persistence_seconds += load_transaction_started_at.elapsed().as_secs_f64();
    for descriptor in &manifest.segments {
        let prepare_started_at = Instant::now();
        let records = prepare_fixture_segment(
            &config.fixture_directory,
            descriptor,
            network,
            Arc::clone(&activations),
            config.block_prepare_concurrency,
        )
        .await?;
        validate_prepared_segment(descriptor.block_count, &records)?;
        for record in &records {
            input_accumulator.append(record)?;
            input_transaction_count = input_transaction_count
                .checked_add(u64::from(record.transaction_count))
                .ok_or_else(|| candidate_error("transaction count exceeds u64::MAX"))?;
        }
        timings.replay_preparation_seconds += prepare_started_at.elapsed().as_secs_f64();

        let copy_started_at = Instant::now();
        copy_replay_segment(&load_transaction, &records).await?;
        timings.replay_persistence_seconds += copy_started_at.elapsed().as_secs_f64();
    }
    let source_validation_started_at = Instant::now();
    let input_validation = finish_validation(input_accumulator, input_transaction_count)?;
    validate_against_fixture(&manifest, &input_validation)?;
    timings.replay_preparation_seconds += source_validation_started_at.elapsed().as_secs_f64();
    let load_commit_started_at = Instant::now();
    load_transaction
        .commit()
        .await
        .map_err(|error| postgres_error("replay load transaction commit", &error))?;
    timings.replay_persistence_seconds += load_commit_started_at.elapsed().as_secs_f64();

    let indexes_started_at = Instant::now();
    client
        .batch_execute(FINALIZE_SCHEMA_SQL)
        .await
        .map_err(|error| postgres_error("deferred index construction", &error))?;
    timings.index_construction_seconds = indexes_started_at.elapsed().as_secs_f64();

    let analyze_started_at = Instant::now();
    client
        .batch_execute(ANALYZE_SQL)
        .await
        .map_err(|error| postgres_error("replay-table analysis", &error))?;
    timings.storage_optimization_seconds = analyze_started_at.elapsed().as_secs_f64();

    let validation_started_at = Instant::now();
    let validation = validate_persisted_replay(&client).await?;
    validate_against_fixture(&manifest, &validation)?;
    if validation != input_validation {
        return Err(candidate_error(
            "persisted read-back does not equal the prepared input sequence",
        ));
    }
    timings.validation_seconds = validation_started_at.elapsed().as_secs_f64();

    let publication_started_at = Instant::now();
    publish_completion(&mut client, &manifest, &validation).await?;
    timings.publication_seconds = publication_started_at.elapsed().as_secs_f64();

    let fresh_reader_validation_started_at = Instant::now();
    close_postgres(client, connection_task).await?;
    let (fresh_reader, fresh_reader_connection_task) =
        connect_postgres(&config.database_url).await?;
    let fresh_reader_validation =
        validate_completed_postgres_candidate(&fresh_reader, &manifest).await?;
    timings.fresh_reader_validation_seconds =
        fresh_reader_validation_started_at.elapsed().as_secs_f64();
    if fresh_reader_validation != validation {
        return Err(candidate_error(
            "fresh-connection validation does not equal pre-publication validation",
        ));
    }
    let storage_measurement_started_at = Instant::now();
    let storage = read_storage_measurements(&fresh_reader, &starting_wal_lsn, &validation).await?;
    close_postgres(fresh_reader, fresh_reader_connection_task).await?;
    timings.storage_measurement_seconds = storage_measurement_started_at.elapsed().as_secs_f64();
    timings.wall_clock_seconds = total_started_at.elapsed().as_secs_f64();

    Ok(PostgresCanonicalReplayReport {
        server_settings,
        validation,
        storage,
        timings,
    })
}

/// Reconnects to an already completed candidate and revalidates every persisted
/// replay encoding, digest, chain position, fixture oracle, and completion marker.
///
/// The database URL is never included in returned errors.
pub async fn validate_postgres_canonical_replay_storage_with_fresh_connection(
    database_url: &str,
    fixture_directory: &Path,
) -> Result<PostgresCanonicalReplayValidation, BenchError> {
    if database_url.trim().is_empty() {
        return Err(candidate_error("database URL must not be blank"));
    }
    let manifest = FixtureManifest::read(fixture_directory)?;
    validate_supported_oracle(&manifest)?;
    let (client, connection_task) = connect_postgres(database_url).await?;
    let validation = validate_completed_postgres_candidate(&client, &manifest).await?;
    close_postgres(client, connection_task).await?;
    Ok(validation)
}

async fn validate_completed_postgres_candidate(
    client: &Client,
    manifest: &FixtureManifest,
) -> Result<PostgresCanonicalReplayValidation, BenchError> {
    let completion = read_completion(client).await?;
    let validation = validate_persisted_replay(client).await?;
    validate_against_fixture(manifest, &validation)?;
    validate_completion(manifest, &validation, &completion)?;
    Ok(validation)
}

fn validate_supported_oracle(manifest: &FixtureManifest) -> Result<(), BenchError> {
    let evidence = &manifest.canonical_block_facts_digest_evidence;
    let block_version = CanonicalBlockFactsDigestVersion::try_from(evidence.block_digest_version)
        .map_err(|source| candidate_error(source.to_string()))?;
    if block_version != CanonicalBlockFactsDigestVersion::CURRENT {
        return Err(candidate_error(
            "fixture block-digest version is not the current writer version",
        ));
    }
    let sequence_version =
        CanonicalBlockFactsSequenceDigestVersion::try_from(evidence.sequence_digest_version)
            .map_err(|source| candidate_error(source.to_string()))?;
    if sequence_version != CanonicalBlockFactsSequenceDigestVersion::CURRENT {
        return Err(candidate_error(
            "fixture sequence-digest version is not the current writer version",
        ));
    }
    Ok(())
}

fn validate_prepared_segment(
    expected_block_count: u32,
    records: &[CanonicalReplayRecord],
) -> Result<(), BenchError> {
    let actual = u32::try_from(records.len())
        .map_err(|_| candidate_error("prepared segment contains more than u32::MAX blocks"))?;
    if actual != expected_block_count {
        return Err(candidate_error(format!(
            "prepared segment contains {actual} blocks, expected {expected_block_count}"
        )));
    }
    Ok(())
}

fn finish_validation(
    accumulator: CanonicalReplaySequenceAccumulator,
    transaction_count: u64,
) -> Result<PostgresCanonicalReplayValidation, BenchError> {
    let position = accumulator.position();
    let replay_format_version = position
        .replay_format_version
        .ok_or_else(|| candidate_error("canonical replay sequence has no replay format version"))?;
    let sequence_digest = accumulator.finish();
    Ok(PostgresCanonicalReplayValidation {
        position,
        transaction_count,
        replay_format_version,
        sequence_digest,
    })
}

fn validate_against_fixture(
    manifest: &FixtureManifest,
    validation: &PostgresCanonicalReplayValidation,
) -> Result<(), BenchError> {
    let evidence = &manifest.canonical_block_facts_digest_evidence;
    let expected_tip = manifest.tip_id()?;
    if validation.position.first_height != Some(BlockHeight::new(manifest.from_height))
        || validation.position.tip_height != Some(expected_tip.height)
        || validation.position.tip_hash != Some(expected_tip.hash)
        || validation.position.block_count != u64::from(manifest.block_count)
    {
        return Err(candidate_error(
            "canonical replay position does not match the fixture range and tip",
        ));
    }
    if validation.transaction_count != manifest.workload_density.transaction_count {
        return Err(candidate_error(format!(
            "canonical replay transaction count {} does not match fixture count {}",
            validation.transaction_count, manifest.workload_density.transaction_count
        )));
    }
    if validation.sequence_digest.version().value() != evidence.sequence_digest_version
        || validation.sequence_digest.block_count() != evidence.block_count
        || hex::encode(validation.sequence_digest.as_bytes()) != evidence.sequence_digest_sha256
    {
        return Err(candidate_error(
            "canonical replay sequence digest does not match the fixture oracle",
        ));
    }
    Ok(())
}

async fn connect_postgres(
    database_url: &str,
) -> Result<(Client, PostgresConnectionTask), BenchError> {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .map_err(|error| postgres_error("database connection", &error))?;
    Ok((client, tokio::spawn(connection)))
}

async fn close_postgres(
    client: Client,
    connection_task: PostgresConnectionTask,
) -> Result<(), BenchError> {
    drop(client);
    connection_task
        .await
        .map_err(|_| candidate_error("database connection task failed to join"))?
        .map_err(|error| postgres_error("database connection close", &error))
}

async fn reject_existing_schema(client: &Client) -> Result<(), BenchError> {
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)",
            &[&SCHEMA_NAME],
        )
        .await
        .map_err(|error| postgres_error("schema precondition check", &error))?;
    let exists: bool = get_column(&row, 0, "schema existence")?;
    if exists {
        return Err(candidate_error(
            "candidate schema already exists; use a fresh disposable database",
        ));
    }
    Ok(())
}

async fn create_schema(client: &mut Client) -> Result<(), BenchError> {
    let transaction = client
        .transaction()
        .await
        .map_err(|error| postgres_error("schema transaction start", &error))?;
    transaction
        .batch_execute(SCHEMA_SQL)
        .await
        .map_err(|error| postgres_error("schema creation", &error))?;
    transaction
        .commit()
        .await
        .map_err(|error| postgres_error("schema transaction commit", &error))
}

async fn copy_replay_segment(
    transaction: &Transaction<'_>,
    records: &[CanonicalReplayRecord],
) -> Result<(), BenchError> {
    let copy = transaction
        .copy_in(COPY_SQL)
        .await
        .map_err(|error| postgres_error("binary replay COPY start", &error))?;
    let writer = BinaryCopyInWriter::new(
        copy,
        &[
            Type::INT8,
            Type::BYTEA,
            Type::BYTEA,
            Type::INT8,
            Type::INT2,
            Type::BYTEA,
            Type::BYTEA,
        ],
    );
    let mut writer = pin!(writer);
    for record in records {
        let height = i64::from(record.height.value());
        let block_hash_bytes = record.block_hash.as_bytes();
        let block_hash = block_hash_bytes.as_slice();
        let parent_hash_bytes = record.parent_hash.as_bytes();
        let parent_hash = parent_hash_bytes.as_slice();
        let transaction_count = i64::from(record.transaction_count);
        let digest_version = u16_to_i16(record.digest.version().value(), "digest version")?;
        let block_facts_digest_bytes = record.digest.as_bytes();
        let block_facts_digest = block_facts_digest_bytes.as_slice();
        let replay_envelope_bytes: &[u8] = &record.replay_envelope_bytes;
        writer
            .as_mut()
            .write(&[
                &height,
                &block_hash,
                &parent_hash,
                &transaction_count,
                &digest_version,
                &block_facts_digest,
                &replay_envelope_bytes,
            ])
            .await
            .map_err(|error| postgres_error("binary replay COPY row", &error))?;
    }
    let copied_rows = writer
        .as_mut()
        .finish()
        .await
        .map_err(|error| postgres_error("binary replay COPY finish", &error))?;
    let expected_rows = u64::try_from(records.len()).unwrap_or(u64::MAX);
    if copied_rows != expected_rows {
        return Err(candidate_error(format!(
            "binary replay COPY wrote {copied_rows} rows, expected {expected_rows}"
        )));
    }
    Ok(())
}

async fn validate_persisted_replay(
    client: &Client,
) -> Result<PostgresCanonicalReplayValidation, BenchError> {
    let rows = client
        .query_raw(READ_BACK_SQL, std::iter::empty::<&(dyn ToSql + Sync)>())
        .await
        .map_err(|error| postgres_error("persisted replay read-back start", &error))?;
    let mut rows = pin!(rows);
    let mut accumulator = CanonicalReplaySequenceAccumulator::new();
    let mut transaction_count = 0_u64;
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| postgres_error("persisted replay read-back", &error))?
    {
        let record = decode_replay_row(&row)?;
        transaction_count = transaction_count
            .checked_add(u64::from(record.transaction_count))
            .ok_or_else(|| candidate_error("persisted transaction count exceeds u64::MAX"))?;
        accumulator.append(&record)?;
    }
    finish_validation(accumulator, transaction_count)
}

fn decode_replay_row(row: &Row) -> Result<CanonicalReplayRecord, BenchError> {
    let height = i64_to_u32(get_column(row, 0, "height")?, "height")?;
    let block_hash = fixed_32(get_column(row, 1, "block_hash")?, "block_hash")?;
    let parent_hash = fixed_32(get_column(row, 2, "parent_hash")?, "parent_hash")?;
    let transaction_count = i64_to_u32(
        get_column(row, 3, "transaction_count")?,
        "transaction_count",
    )?;
    let digest_version_number =
        i16_to_u16(get_column(row, 4, "digest_version")?, "digest_version")?;
    let digest_version = CanonicalBlockFactsDigestVersion::try_from(digest_version_number)
        .map_err(|source| candidate_error(source.to_string()))?;
    let stored_digest = fixed_32(
        get_column(row, 5, "block_facts_digest")?,
        "block_facts_digest",
    )?;
    let replay_envelope_bytes = get_column(row, 6, "replay_envelope")?;
    CanonicalReplayRecord::from_persisted(PersistedCanonicalReplayRow {
        height: BlockHeight::new(height),
        block_hash: BlockHash::from_bytes(block_hash),
        parent_hash: BlockHash::from_bytes(parent_hash),
        transaction_count,
        digest_version,
        stored_digest,
        replay_envelope_bytes,
    })
}

async fn publish_completion(
    client: &mut Client,
    manifest: &FixtureManifest,
    validation: &PostgresCanonicalReplayValidation,
) -> Result<(), BenchError> {
    let fixture_digest = decode_digest_hex(&manifest.digest_sha256()?, "fixture digest")?;
    let Some(tip_height) = validation.position.tip_height else {
        return Err(candidate_error(
            "cannot publish an empty canonical replay sequence",
        ));
    };
    let Some(tip_hash) = validation.position.tip_hash else {
        return Err(candidate_error(
            "cannot publish a canonical tip without a hash",
        ));
    };
    let fixture_format_version = i32::try_from(manifest.fixture_format_version)
        .map_err(|_| candidate_error("fixture format version exceeds i32::MAX"))?;
    let from_height = i64::from(manifest.from_height);
    let to_height = i64::from(tip_height.value());
    let block_count = u64_to_i64(validation.position.block_count, "block_count")?;
    let transaction_count = u64_to_i64(validation.transaction_count, "transaction_count")?;
    let tip_hash = tip_hash.as_bytes();
    let block_digest_version = u16_to_i16(
        manifest
            .canonical_block_facts_digest_evidence
            .block_digest_version,
        "block_digest_version",
    )?;
    let replay_format_version = i64::from(validation.replay_format_version);
    let sequence_digest_version = u16_to_i16(
        validation.sequence_digest.version().value(),
        "sequence_digest_version",
    )?;
    let sequence_digest = validation.sequence_digest.as_bytes();
    let logical_replay_bytes = u64_to_i64(
        validation.position.logical_replay_bytes,
        "logical_replay_bytes",
    )?;
    let physical_schema_version = u16_to_i16(
        POSTGRES_CANONICAL_REPLAY_SCHEMA_VERSION,
        "physical_schema_version",
    )?;
    let fixture_digest: &[u8] = &fixture_digest;
    let tip_hash: &[u8] = &tip_hash;
    let sequence_digest: &[u8] = &sequence_digest;

    let transaction = client
        .transaction()
        .await
        .map_err(|error| postgres_error("completion transaction start", &error))?;
    let inserted = transaction
        .execute(
            INSERT_COMPLETION_SQL,
            &[
                &physical_schema_version,
                &fixture_format_version,
                &fixture_digest,
                &manifest.network,
                &from_height,
                &to_height,
                &block_count,
                &transaction_count,
                &tip_hash,
                &block_digest_version,
                &replay_format_version,
                &sequence_digest_version,
                &sequence_digest,
                &logical_replay_bytes,
                &CANONICAL_REPLAY_CONTRACT_IDENTITY,
            ],
        )
        .await
        .map_err(|error| postgres_error("completion publication", &error))?;
    if inserted != 1 {
        return Err(candidate_error(format!(
            "completion publication inserted {inserted} rows, expected 1"
        )));
    }
    transaction
        .commit()
        .await
        .map_err(|error| postgres_error("completion transaction commit", &error))
}

#[derive(Debug)]
struct CompletionRow {
    physical_schema_version: i16,
    fixture_format_version: i32,
    fixture_digest_sha256: [u8; 32],
    network: String,
    from_height: u32,
    to_height: u32,
    block_count: u64,
    transaction_count: u64,
    tip_hash: BlockHash,
    block_digest_version: u16,
    replay_format_version: u32,
    sequence_digest_version: u16,
    sequence_digest: [u8; 32],
    logical_replay_bytes: u64,
    contract_identity: String,
}

async fn read_completion(client: &Client) -> Result<CompletionRow, BenchError> {
    let Some(row) = client
        .query_opt(READ_COMPLETION_SQL, &[])
        .await
        .map_err(|error| postgres_error("completion marker read", &error))?
    else {
        return Err(candidate_error("completion marker is absent"));
    };
    Ok(CompletionRow {
        physical_schema_version: get_column(&row, 0, "physical_schema_version")?,
        fixture_format_version: get_column(&row, 1, "fixture_format_version")?,
        fixture_digest_sha256: fixed_32(
            get_column(&row, 2, "fixture_digest_sha256")?,
            "fixture_digest_sha256",
        )?,
        network: get_column(&row, 3, "network")?,
        from_height: i64_to_u32(get_column(&row, 4, "from_height")?, "from_height")?,
        to_height: i64_to_u32(get_column(&row, 5, "to_height")?, "to_height")?,
        block_count: i64_to_u64(get_column(&row, 6, "block_count")?, "block_count")?,
        transaction_count: i64_to_u64(
            get_column(&row, 7, "transaction_count")?,
            "transaction_count",
        )?,
        tip_hash: BlockHash::from_bytes(fixed_32(get_column(&row, 8, "tip_hash")?, "tip_hash")?),
        block_digest_version: i16_to_u16(
            get_column(&row, 9, "block_digest_version")?,
            "block_digest_version",
        )?,
        replay_format_version: i64_to_u32(
            get_column(&row, 10, "replay_format_version")?,
            "replay_format_version",
        )?,
        sequence_digest_version: i16_to_u16(
            get_column(&row, 11, "sequence_digest_version")?,
            "sequence_digest_version",
        )?,
        sequence_digest: fixed_32(get_column(&row, 12, "sequence_digest")?, "sequence_digest")?,
        logical_replay_bytes: i64_to_u64(
            get_column(&row, 13, "logical_replay_bytes")?,
            "logical_replay_bytes",
        )?,
        contract_identity: get_column(&row, 14, "contract_identity")?,
    })
}

fn validate_completion(
    manifest: &FixtureManifest,
    validation: &PostgresCanonicalReplayValidation,
    completion: &CompletionRow,
) -> Result<(), BenchError> {
    let fixture_digest = decode_digest_hex(&manifest.digest_sha256()?, "fixture digest")?;
    let evidence = &manifest.canonical_block_facts_digest_evidence;
    if i16_to_u16(
        completion.physical_schema_version,
        "physical_schema_version",
    )? != POSTGRES_CANONICAL_REPLAY_SCHEMA_VERSION
        || completion.contract_identity != CANONICAL_REPLAY_CONTRACT_IDENTITY
        || u32::try_from(completion.fixture_format_version).ok()
            != Some(manifest.fixture_format_version)
        || completion.fixture_digest_sha256 != fixture_digest
        || completion.network != manifest.network
        || completion.from_height != manifest.from_height
        || completion.to_height != manifest.to_height
        || completion.block_count != validation.position.block_count
        || completion.transaction_count != validation.transaction_count
        || Some(completion.tip_hash) != validation.position.tip_hash
        || completion.block_digest_version != evidence.block_digest_version
        || completion.replay_format_version != validation.replay_format_version
        || completion.sequence_digest_version != validation.sequence_digest.version().value()
        || completion.sequence_digest != validation.sequence_digest.as_bytes()
        || completion.logical_replay_bytes != validation.position.logical_replay_bytes
    {
        return Err(candidate_error(
            "completion marker does not match the fixture and persisted replay sequence",
        ));
    }
    Ok(())
}

async fn read_server_settings(client: &Client) -> Result<PostgresServerSettings, BenchError> {
    let row = client
        .query_one(SERVER_SETTINGS_SQL, &[])
        .await
        .map_err(|error| postgres_error("server settings read", &error))?;
    let server_version_number = i32_to_u32(
        get_column(&row, 1, "server_version_num")?,
        "server_version_num",
    )?;
    Ok(PostgresServerSettings {
        server_version: get_column(&row, 0, "server_version")?,
        server_version_number,
        max_connections: i32_to_u32(get_column(&row, 2, "max_connections")?, "max_connections")?,
        shared_buffers_bytes: i64_to_u64(
            get_column(&row, 3, "shared_buffers_bytes")?,
            "shared_buffers_bytes",
        )?,
        effective_cache_size_bytes: i64_to_u64(
            get_column(&row, 4, "effective_cache_size_bytes")?,
            "effective_cache_size_bytes",
        )?,
        maintenance_work_mem_bytes: i64_to_u64(
            get_column(&row, 5, "maintenance_work_mem_bytes")?,
            "maintenance_work_mem_bytes",
        )?,
        work_mem_bytes: i64_to_u64(get_column(&row, 6, "work_mem_bytes")?, "work_mem_bytes")?,
        max_wal_size_bytes: i64_to_u64(
            get_column(&row, 7, "max_wal_size_bytes")?,
            "max_wal_size_bytes",
        )?,
        min_wal_size_bytes: i64_to_u64(
            get_column(&row, 8, "min_wal_size_bytes")?,
            "min_wal_size_bytes",
        )?,
        checkpoint_timeout_seconds: i64_to_u64(
            get_column(&row, 9, "checkpoint_timeout_seconds")?,
            "checkpoint_timeout_seconds",
        )?,
        checkpoint_completion_target: get_column(&row, 10, "checkpoint_completion_target")?,
        wal_compression: get_column(&row, 11, "wal_compression")?,
        password_encryption_default: get_column(&row, 12, "password_encryption")?,
        max_worker_processes: i32_to_u32(
            get_column(&row, 13, "max_worker_processes")?,
            "max_worker_processes",
        )?,
        max_parallel_workers: i32_to_u32(
            get_column(&row, 14, "max_parallel_workers")?,
            "max_parallel_workers",
        )?,
        max_parallel_maintenance_workers: i32_to_u32(
            get_column(&row, 15, "max_parallel_maintenance_workers")?,
            "max_parallel_maintenance_workers",
        )?,
        track_io_timing: get_column(&row, 16, "track_io_timing")?,
        huge_pages: get_column(&row, 17, "huge_pages")?,
        fsync: get_column(&row, 18, "fsync")?,
        full_page_writes: get_column(&row, 19, "full_page_writes")?,
        synchronous_commit: get_column(&row, 20, "synchronous_commit")?,
        wal_level: get_column(&row, 21, "wal_level")?,
        data_checksums: get_column(&row, 22, "data_checksums")?,
    })
}

async fn current_wal_insert_lsn(client: &Client) -> Result<String, BenchError> {
    let row = client
        .query_one("SELECT pg_current_wal_insert_lsn()::TEXT", &[])
        .await
        .map_err(|error| postgres_error("starting WAL position read", &error))?;
    get_column(&row, 0, "pg_current_wal_insert_lsn")
}

async fn read_storage_measurements(
    client: &Client,
    starting_wal_lsn: &str,
    validation: &PostgresCanonicalReplayValidation,
) -> Result<PostgresStorageMeasurements, BenchError> {
    let row = client
        .query_one(STORAGE_BYTES_SQL, &[])
        .await
        .map_err(|error| postgres_error("storage byte measurement", &error))?;
    let replay_table_bytes = i64_to_u64(
        get_column(&row, 0, "replay_table_bytes")?,
        "replay_table_bytes",
    )?;
    let index_bytes = i64_to_u64(get_column(&row, 1, "index_bytes")?, "index_bytes")?;
    let schema_bytes = i64_to_u64(get_column(&row, 2, "schema_bytes")?, "schema_bytes")?;
    let wal_row = client
        .query_one(
            "SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(), $1::TEXT::pg_lsn)::BIGINT",
            &[&starting_wal_lsn],
        )
        .await
        .map_err(|error| postgres_error("WAL byte measurement", &error))?;
    let wal_bytes = i64_to_u64(get_column(&wal_row, 0, "wal_bytes")?, "wal_bytes")?;
    Ok(PostgresStorageMeasurements {
        logical_replay_bytes: validation.position.logical_replay_bytes,
        replay_table_bytes,
        index_bytes,
        schema_bytes,
        wal_bytes,
    })
}

fn get_column<T>(row: &Row, index: usize, column: &'static str) -> Result<T, BenchError>
where
    T: FromSqlOwned,
{
    row.try_get(index)
        .map_err(|error| postgres_error(column, &error))
}

fn fixed_32(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 32], BenchError> {
    bytes
        .try_into()
        .map_err(|_| candidate_error(format!("{field} must contain exactly 32 bytes")))
}

fn decode_digest_hex(encoded: &str, field: &'static str) -> Result<[u8; 32], BenchError> {
    let bytes = hex::decode(encoded)
        .map_err(|_| candidate_error(format!("{field} must be lowercase hexadecimal")))?;
    fixed_32(bytes, field)
}

fn i64_to_u32(number: i64, field: &'static str) -> Result<u32, BenchError> {
    u32::try_from(number).map_err(|_| candidate_error(format!("{field} does not fit u32")))
}

fn i32_to_u32(number: i32, field: &'static str) -> Result<u32, BenchError> {
    u32::try_from(number).map_err(|_| candidate_error(format!("{field} does not fit u32")))
}

fn i16_to_u16(number: i16, field: &'static str) -> Result<u16, BenchError> {
    u16::try_from(number).map_err(|_| candidate_error(format!("{field} must not be negative")))
}

fn i64_to_u64(number: i64, field: &'static str) -> Result<u64, BenchError> {
    u64::try_from(number).map_err(|_| candidate_error(format!("{field} must not be negative")))
}

fn u16_to_i16(number: u16, field: &'static str) -> Result<i16, BenchError> {
    i16::try_from(number).map_err(|_| candidate_error(format!("{field} exceeds i16::MAX")))
}

fn u64_to_i64(number: u64, field: &'static str) -> Result<i64, BenchError> {
    i64::try_from(number).map_err(|_| candidate_error(format!("{field} exceeds i64::MAX")))
}

fn candidate_error(reason: impl Into<String>) -> BenchError {
    BenchError::canonical_replay_storage_candidate(CANDIDATE_ID, reason)
}

fn postgres_error(operation: &'static str, error: &tokio_postgres::Error) -> BenchError {
    let sql_state = error
        .code()
        .map_or("unavailable", tokio_postgres::error::SqlState::code);
    candidate_error(format!("{operation} failed (SQLSTATE {sql_state})"))
}

#[cfg(test)]
mod tests {
    use super::{COPY_SQL, FINALIZE_SCHEMA_SQL, PostgresServerSettings, SCHEMA_SQL, fixed_32};

    #[test]
    fn schema_stays_logged_and_defers_the_height_primary_key() {
        assert!(!SCHEMA_SQL.contains("UNLOGGED"));
        assert!(!SCHEMA_SQL.contains("canonical_replay_storage_pkey"));
        assert!(FINALIZE_SCHEMA_SQL.contains("canonical_replay_storage_pkey"));
        assert!(!FINALIZE_SCHEMA_SQL.contains("block_hash_uq"));
        assert!(COPY_SQL.contains("FROM STDIN BINARY"));
        assert!(SCHEMA_SQL.contains("replay_envelope BYTEA COMPRESSION lz4 NOT NULL"));
        assert!(SCHEMA_SQL.contains("replay_format_version BIGINT NOT NULL"));
    }

    #[test]
    fn required_posture_rejects_unsafe_durability_or_password_encryption_default() {
        let durable = settings(true, true, "on");
        assert!(durable.validate_required_posture().is_ok());
        assert!(
            settings(false, true, "on")
                .validate_required_posture()
                .is_err()
        );
        assert!(
            settings(true, false, "on")
                .validate_required_posture()
                .is_err()
        );
        assert!(
            settings(true, true, "off")
                .validate_required_posture()
                .is_err()
        );
        let mut md5 = settings(true, true, "on");
        md5.password_encryption_default = "md5".to_owned();
        assert!(md5.validate_required_posture().is_err());
    }

    #[test]
    fn fixed_hash_decoder_rejects_wrong_width() {
        assert!(fixed_32(vec![0; 32], "digest").is_ok());
        assert!(fixed_32(vec![0; 31], "digest").is_err());
        assert!(fixed_32(vec![0; 33], "digest").is_err());
    }

    fn settings(
        fsync: bool,
        full_page_writes: bool,
        synchronous_commit: &str,
    ) -> PostgresServerSettings {
        PostgresServerSettings {
            server_version: "test".to_owned(),
            server_version_number: 180_000,
            max_connections: 50,
            shared_buffers_bytes: 3 * 1024 * 1024 * 1024,
            effective_cache_size_bytes: 9 * 1024 * 1024 * 1024,
            maintenance_work_mem_bytes: 2 * 1024 * 1024 * 1024,
            work_mem_bytes: 64 * 1024 * 1024,
            max_wal_size_bytes: 16 * 1024 * 1024 * 1024,
            min_wal_size_bytes: 2 * 1024 * 1024 * 1024,
            checkpoint_timeout_seconds: 15 * 60,
            checkpoint_completion_target: 0.9,
            wal_compression: "pglz".to_owned(),
            password_encryption_default: "scram-sha-256".to_owned(),
            max_worker_processes: 6,
            max_parallel_workers: 6,
            max_parallel_maintenance_workers: 4,
            track_io_timing: true,
            huge_pages: "off".to_owned(),
            fsync,
            full_page_writes,
            synchronous_commit: synchronous_commit.to_owned(),
            wal_level: "replica".to_owned(),
            data_checksums: true,
        }
    }
}
