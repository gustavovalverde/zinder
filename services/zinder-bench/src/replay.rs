//! Fixed-range replay: run the real bulk-catchup pipeline over a captured
//! fixture and a cloned canonical store, then assemble a report.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zinder_core::{ChainEpoch, UnixTimestampMillis, wire::encode_rpc_block_hash_hex};
use zinder_ingest::{
    BulkCatchupRunConfig, CanonicalPipelineLimits, RawBlobPolicy,
    bench_support::{
        BENCH_MAX_RESPONSE_BYTES, BenchBulkCatchupParams, bench_bulk_catchup_run_config,
        bench_materialized_view_config,
    },
    bootstrap_transparent_address_ranking, catch_up_materialized_view_store_to_canonical,
    open_primary_materialized_view_store_for_canonical_with_materialized_view_preset,
    run_bulk_catchup_with_store,
};
use zinder_materialized_views::{
    MaterializedViewPreset, MaterializedViewStore, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, CURRENT_STORE_SCHEMA_VERSION, ChainEventStreamFamily,
    ChainStoreOptions, EventStreamStartPosition, PrimaryChainStore, RocksDbResourceBudget,
    StreamCursorTokenV1,
};

use crate::{
    error::BenchError,
    fixture::{FixtureManifest, FixtureNodeSource},
    report::{
        AcceptanceThresholds, CanonicalStoreRangeReplayMeasurements,
        CanonicalStoreRangeReplayReport, CanonicalStoreRangeReplayWriterSettings,
        FixtureCachePolicy, FixtureSummary, RocksDbResourceBudgetSummary,
        STORE_READ_TELEMETRY_FAMILY, StartingCanonicalState, StartingCanonicalStateKind,
        StorageCandidateIdentity, TELEMETRY_COVERAGE_TOTAL,
        build_canonical_store_range_replay_report, is_valid_benchmark_trial_id,
    },
    rss::peak_rss,
};

/// Inputs for one fixed-range replay.
#[derive(Clone, Debug)]
pub struct ReplayConfig {
    /// Captured fixture directory.
    pub fixture_directory: PathBuf,
    /// Writable clone of the captured start-state canonical store.
    pub store_path: PathBuf,
    /// Prepare concurrency to run with (the primary sweep knob).
    pub block_prepare_concurrency: NonZeroU32,
    /// Optional maximum source-segment response-size override.
    pub max_response_bytes: Option<NonZeroU64>,
    /// Optional maximum connected blocks per source segment override.
    pub source_segment_max_blocks: Option<NonZeroU32>,
    /// Optional adaptive source-segment response target override.
    pub source_segment_target_response_bytes: Option<NonZeroU64>,
    /// Optional concurrent source-segment request-count override.
    pub source_fetch_max_in_flight_requests: Option<NonZeroU32>,
    /// Optional source-response admission watermark override.
    pub source_fetch_max_in_flight_bytes: Option<NonZeroU64>,
    /// Optional canonical block-preparation memory watermark override.
    pub block_prepare_memory_watermark_bytes: Option<NonZeroU64>,
    /// Deterministic delay applied to each captured source-segment response.
    pub source_segment_delay_millis: u64,
    /// Optional canonical block-cache override in bytes (the cache-size knob).
    pub canonical_block_cache_bytes: Option<u64>,
    /// Materialized-view preset to replay after canonical ingest, or `None` for a
    /// canonical-only run.
    pub materialized_view_preset: Option<MaterializedViewPreset>,
    /// Portion of canonical event history presented to the selected
    /// materialized views.
    pub materialized_view_replay_scope: MaterializedViewReplayScope,
    /// Source revision of the measured binary, when known.
    pub software_revision: Option<String>,
    /// Campaign trial identity, paired with `fixture_cache_policy` when supplied.
    pub trial_id: Option<String>,
    /// Controlled fixture-cache treatment for this campaign run.
    pub fixture_cache_policy: Option<FixtureCachePolicy>,
    /// Wall-clock Unix timestamp captured before benchmark setup begins.
    pub run_started_at_unix_millis: u64,
    /// Stable operator label for the runner, when known.
    pub runner_id: Option<String>,
    /// CPU limit applied to the benchmark container, in logical cores.
    pub cpu_limit_cores: Option<f64>,
    /// Memory limit applied to the benchmark container.
    pub memory_limit_bytes: Option<u64>,
    /// Stable operator-defined storage performance class.
    pub storage_class: Option<String>,
    /// Immutable container image reference, when known.
    pub image_reference: Option<String>,
    /// Acceptance thresholds for the directly driven canonical fixture replay.
    pub canonical_fixture_replay_thresholds: Option<AcceptanceThresholds>,
}

impl ReplayConfig {
    /// Validates acceptance provenance and telemetry requirements before I/O.
    pub fn validate(&self, metrics_recorder_available: bool) -> Result<(), BenchError> {
        match (&self.trial_id, self.fixture_cache_policy) {
            (None, None) => {}
            (Some(trial_id), Some(_)) if is_valid_benchmark_trial_id(trial_id) => {}
            (Some(_), Some(_)) => {
                return Err(BenchError::invalid_argument(
                    "--trial-id must start with an ASCII alphanumeric character and contain only ASCII alphanumeric characters, '.', '_', or '-'",
                ));
            }
            _ => {
                return Err(BenchError::invalid_argument(
                    "--trial-id and --fixture-cache-policy must be supplied together",
                ));
            }
        }
        if self
            .image_reference
            .as_deref()
            .is_some_and(|reference| !crate::report::is_immutable_image_reference(reference))
        {
            return Err(BenchError::invalid_argument(
                "--image-reference must be a sha256 image ID or digest-pinned image reference",
            ));
        }
        if self
            .cpu_limit_cores
            .is_some_and(|cores| !cores.is_finite() || cores <= 0.0)
        {
            return Err(BenchError::invalid_argument(
                "--cpu-limit-cores must be finite and greater than zero",
            ));
        }
        if self.memory_limit_bytes == Some(0) {
            return Err(BenchError::invalid_argument(
                "--memory-limit-bytes must be greater than zero",
            ));
        }

        let Some(_thresholds) = self.canonical_fixture_replay_thresholds else {
            return Ok(());
        };
        if self.materialized_view_preset.is_some() {
            return Err(BenchError::invalid_argument(
                "canonical fixture replay thresholds require a canonical-only run without --materialized-view-preset",
            ));
        }
        require_nonblank_provenance(self.software_revision.as_deref(), "--software-revision")?;
        require_nonblank_provenance(self.runner_id.as_deref(), "--runner-id")?;
        require_nonblank_provenance(self.image_reference.as_deref(), "--image-reference")?;
        require_nonblank_provenance(self.storage_class.as_deref(), "--storage-class")?;
        if self.cpu_limit_cores.is_none() {
            return Err(BenchError::invalid_argument(
                "canonical fixture replay thresholds require --cpu-limit-cores",
            ));
        }
        if self.memory_limit_bytes.is_none() {
            return Err(BenchError::invalid_argument(
                "canonical fixture replay thresholds require --memory-limit-bytes",
            ));
        }
        if !metrics_recorder_available {
            return Err(BenchError::invalid_argument(
                "canonical fixture replay thresholds require an installed metrics recorder",
            ));
        }
        Ok(())
    }
}

fn require_nonblank_provenance(candidate: Option<&str>, flag: &str) -> Result<(), BenchError> {
    if candidate.is_none_or(|candidate| candidate.trim().is_empty()) {
        return Err(BenchError::invalid_argument(format!(
            "canonical fixture replay thresholds require a nonblank {flag}"
        )));
    }
    Ok(())
}

/// Canonical event-history scope used for a materialized-view benchmark arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MaterializedViewReplayScope {
    /// Seed fresh materialized-view cursors at the cloned store tip, then measure only
    /// events produced by the captured range.
    #[default]
    FixedRange,
    /// Start fresh materialized views without cursors and rebuild all retained
    /// canonical event history.
    RetainedHistory,
}

impl MaterializedViewReplayScope {
    /// Stable report spelling for this benchmark scope.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FixedRange => "fixed-range",
            Self::RetainedHistory => "retained-history",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct MaterializedViewMeasurements {
    wall_clock_seconds: Option<f64>,
    row_count: Option<u64>,
    event_cursor_at_tip: Option<bool>,
    store_bytes: Option<u64>,
    reopen_seconds: Option<f64>,
    logical_write_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
struct CanonicalReplayMeasurements {
    wall_clock_seconds: f64,
    tip_height_after: Option<u32>,
    tip_hash_after_hex: Option<String>,
}

struct ReplayRunMeasurements {
    canonical: CanonicalReplayMeasurements,
    materialized_view: MaterializedViewMeasurements,
    pipeline_limits: CanonicalPipelineLimits,
    completed_at_unix_millis: u64,
}

const STARTING_CHECKPOINT_MANIFEST_FILE_NAME: &str = "zinder-benchmark-starting-store.json";
const STARTING_CHECKPOINT_MANIFEST_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
struct StartingCheckpointManifest {
    format_version: u32,
    network: String,
    canonical_position: Option<StartingCheckpointCanonicalPosition>,
}

#[derive(Debug, Deserialize)]
struct StartingCheckpointCanonicalPosition {
    chain_epoch_id: u64,
    visible_tip_height: u32,
    visible_tip_hash: String,
    artifact_schema_version: u16,
}

#[derive(Debug)]
struct StartingCheckpointManifestEvidence {
    sha256: String,
    manifest: StartingCheckpointManifest,
}

/// Replays the captured range and returns a machine-readable report.
///
/// When `metrics_handle` is `Some`, the report includes per-caller store reads
/// and `RocksDB` tickers scraped from the in-process recorder; when `None`, the
/// report carries only the directly measured scalars.
pub async fn replay_fixture(
    config: ReplayConfig,
    metrics_handle: Option<PrometheusHandle>,
) -> Result<CanonicalStoreRangeReplayReport, BenchError> {
    config.validate(metrics_handle.is_some())?;
    if metrics_handle.is_some() {
        metrics::counter!(
            TELEMETRY_COVERAGE_TOTAL,
            "family" => STORE_READ_TELEMETRY_FAMILY
        )
        .increment(0);
    }
    let manifest = FixtureManifest::read(&config.fixture_directory)?;
    let network = manifest.network_typed()?;
    let activations = Arc::new(manifest.activations_typed()?);
    let checkpoint_manifest_required =
        config.canonical_fixture_replay_thresholds.is_some() && manifest.from_height > 1;
    let starting_checkpoint_manifest =
        read_starting_checkpoint_manifest(&config.store_path, checkpoint_manifest_required)?;
    let source = open_replay_source(&config, &manifest)?;

    let (store, canonical_options) = open_canonical_store(&config, network)?;

    let starting_canonical_state = starting_canonical_state(
        &store,
        starting_checkpoint_manifest.as_ref(),
        &manifest.network,
    )?;
    validate_starting_tip(manifest.from_height, starting_canonical_state.tip_height)?;
    validate_acceptance_starting_state(
        config.canonical_fixture_replay_thresholds.is_some(),
        manifest.from_height,
        &starting_canonical_state,
    )?;
    let materialized_view_replay_start_cursor = prepare_materialized_view_replay(&config, &store)?;

    let run_config = benchmark_run_config(
        &config,
        &manifest,
        network,
        activations,
        canonical_options.rocksdb_resource_budget,
    )?;

    let started_at = Instant::now();
    run_bulk_catchup_with_store(&run_config, &source, &store).await?;
    let wall_clock_seconds = started_at.elapsed().as_secs_f64();
    let ending_chain_epoch = store.current_chain_epoch()?;
    let canonical_measurements = CanonicalReplayMeasurements {
        wall_clock_seconds,
        tip_height_after: ending_chain_epoch
            .as_ref()
            .map(|chain_epoch| chain_epoch.visible_tip_height.value()),
        tip_hash_after_hex: ending_chain_epoch
            .as_ref()
            .map(|chain_epoch| hex::encode(chain_epoch.visible_tip_hash.as_bytes())),
    };
    let materialized_view_store =
        open_materialized_view_store(&config, materialized_view_replay_start_cursor.as_ref())?;

    let materialized_view_measurements = measure_materialized_view_replay(
        &config.store_path,
        &store,
        config.materialized_view_preset,
        materialized_view_store,
    )
    .await?;
    let run_completed_at_unix_millis = UnixTimestampMillis::now().value();
    let exposition = metrics_handle.map(|handle| handle.render());
    let fixture = FixtureSummary::try_from(&manifest)?;
    let run = ReplayRunMeasurements {
        canonical: canonical_measurements,
        materialized_view: materialized_view_measurements,
        pipeline_limits: run_config.pipeline_limits,
        completed_at_unix_millis: run_completed_at_unix_millis,
    };
    let measurements =
        assemble_replay_measurements(&config, canonical_options, starting_canonical_state, run)?;
    Ok(build_canonical_store_range_replay_report(
        fixture,
        &measurements,
        exposition.as_deref(),
    ))
}

fn open_replay_source(
    config: &ReplayConfig,
    manifest: &FixtureManifest,
) -> Result<FixtureNodeSource, BenchError> {
    FixtureNodeSource::open_with_segment_delay(
        &config.fixture_directory,
        manifest,
        Duration::from_millis(config.source_segment_delay_millis),
    )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the validated positive finite CPU limit is rounded up and capped before conversion"
)]
fn logical_core_count_from_limit(cpu_limit_cores: Option<f64>, fallback: NonZeroU32) -> NonZeroU32 {
    let Some(cpu_limit_cores) = cpu_limit_cores else {
        return fallback;
    };
    let logical_core_count = cpu_limit_cores.ceil().min(f64::from(u32::MAX)) as u32;
    NonZeroU32::new(logical_core_count).unwrap_or(fallback)
}

fn benchmark_pipeline_resource_inputs(
    config: &ReplayConfig,
) -> (NonZeroU32, Option<NonZeroU64>, NonZeroU64) {
    (
        logical_core_count_from_limit(config.cpu_limit_cores, config.block_prepare_concurrency),
        config.memory_limit_bytes.and_then(NonZeroU64::new),
        config.max_response_bytes.unwrap_or_else(|| {
            NonZeroU64::new(BENCH_MAX_RESPONSE_BYTES).unwrap_or(NonZeroU64::MIN)
        }),
    )
}

fn benchmark_run_config(
    config: &ReplayConfig,
    manifest: &FixtureManifest,
    network: zinder_core::Network,
    network_upgrade_activations: Arc<zinder_core::NetworkUpgradeActivations>,
    canonical_rocksdb_budget: RocksDbResourceBudget,
) -> Result<BulkCatchupRunConfig, BenchError> {
    let (logical_core_count, memory_budget_bytes, max_response_bytes) =
        benchmark_pipeline_resource_inputs(config);
    let mut run_config = bench_bulk_catchup_run_config(BenchBulkCatchupParams {
        network,
        storage_path: config.store_path.clone(),
        from_height: zinder_core::BlockHeight::new(manifest.from_height),
        to_height: zinder_core::BlockHeight::new(manifest.to_height),
        block_prepare_concurrency: config.block_prepare_concurrency,
        logical_core_count,
        memory_budget_bytes,
        max_response_bytes,
        canonical_rocksdb_budget,
        raw_blob_policy: RawBlobPolicy::None,
        network_upgrade_activations,
    });
    run_config.pipeline_limits =
        resolve_replay_pipeline_limits(config, run_config.pipeline_limits)?;
    Ok(run_config)
}

fn resolve_replay_pipeline_limits(
    config: &ReplayConfig,
    mut pipeline_limits: CanonicalPipelineLimits,
) -> Result<CanonicalPipelineLimits, BenchError> {
    if let Some(max_response_bytes) = config.max_response_bytes {
        pipeline_limits.max_response_bytes = max_response_bytes;
    }
    if let Some(source_segment_max_blocks) = config.source_segment_max_blocks {
        pipeline_limits.source_segment_max_blocks = source_segment_max_blocks;
    }
    if let Some(source_segment_target_response_bytes) = config.source_segment_target_response_bytes
    {
        pipeline_limits.source_segment_target_response_bytes = source_segment_target_response_bytes;
    }
    if let Some(source_fetch_max_in_flight_requests) = config.source_fetch_max_in_flight_requests {
        pipeline_limits.source_fetch_max_in_flight_requests = source_fetch_max_in_flight_requests;
    }
    if let Some(source_fetch_max_in_flight_bytes) = config.source_fetch_max_in_flight_bytes {
        pipeline_limits.source_fetch_max_in_flight_bytes = source_fetch_max_in_flight_bytes;
    }
    if let Some(block_prepare_memory_watermark_bytes) = config.block_prepare_memory_watermark_bytes
    {
        pipeline_limits.block_prepare_memory_watermark_bytes = block_prepare_memory_watermark_bytes;
    }
    pipeline_limits.validate().map_err(|source| {
        BenchError::invalid_argument(format!("invalid replay pipeline limits: {source}"))
    })
}

fn assemble_replay_measurements(
    config: &ReplayConfig,
    canonical_options: ChainStoreOptions,
    starting_canonical_state: StartingCanonicalState,
    run: ReplayRunMeasurements,
) -> Result<CanonicalStoreRangeReplayMeasurements, BenchError> {
    let ReplayRunMeasurements {
        canonical,
        materialized_view,
        pipeline_limits,
        completed_at_unix_millis,
    } = run;
    Ok(CanonicalStoreRangeReplayMeasurements {
        block_prepare_concurrency: pipeline_limits.block_prepare_concurrency.get(),
        max_response_bytes: pipeline_limits.max_response_bytes.get(),
        source_segment_max_blocks: pipeline_limits.source_segment_max_blocks.get(),
        source_segment_target_response_bytes: pipeline_limits
            .source_segment_target_response_bytes
            .get(),
        source_fetch_max_in_flight_requests: pipeline_limits
            .source_fetch_max_in_flight_requests
            .get(),
        source_fetch_max_in_flight_bytes: pipeline_limits.source_fetch_max_in_flight_bytes.get(),
        block_prepare_memory_watermark_bytes: pipeline_limits
            .block_prepare_memory_watermark_bytes
            .get(),
        source_segment_delay_millis: config.source_segment_delay_millis,
        canonical_writer: canonical_writer_settings(canonical_options),
        materialized_view_preset: config
            .materialized_view_preset
            .map(MaterializedViewPreset::as_str),
        materialized_view_replay_scope: config
            .materialized_view_preset
            .map(|_| config.materialized_view_replay_scope.as_str()),
        wall_clock_seconds: canonical.wall_clock_seconds,
        starting_canonical_state,
        tip_height_after: canonical.tip_height_after,
        tip_hash_after_hex: canonical.tip_hash_after_hex,
        materialized_view_build_wall_clock_seconds: materialized_view.wall_clock_seconds,
        materialized_view_row_count: materialized_view.row_count,
        materialized_view_event_cursor_at_tip: materialized_view.event_cursor_at_tip,
        materialized_view_store_bytes: materialized_view.store_bytes,
        materialized_view_store_reopen_seconds: materialized_view.reopen_seconds,
        materialized_view_logical_write_bytes: materialized_view.logical_write_bytes,
        peak_rss: peak_rss(),
        storage_candidate: storage_candidate_identity(config.materialized_view_preset)?,
        software_revision: config.software_revision.clone(),
        trial_id: config.trial_id.clone(),
        fixture_cache_policy: config.fixture_cache_policy,
        run_started_at_unix_millis: config.run_started_at_unix_millis,
        run_completed_at_unix_millis: completed_at_unix_millis,
        runner_id: config.runner_id.clone(),
        cpu_limit_cores: config.cpu_limit_cores,
        memory_limit_bytes: config.memory_limit_bytes,
        storage_class: config.storage_class.clone(),
        image_reference: config.image_reference.clone(),
        canonical_fixture_replay_thresholds: config.canonical_fixture_replay_thresholds,
    })
}

fn validate_acceptance_starting_state(
    thresholded: bool,
    fixture_from_height: u32,
    starting_state: &StartingCanonicalState,
) -> Result<(), BenchError> {
    if !thresholded {
        return Ok(());
    }
    let expected_kind = if fixture_from_height == 1 {
        StartingCanonicalStateKind::Empty
    } else {
        StartingCanonicalStateKind::Checkpoint
    };
    if starting_state.kind != expected_kind {
        return Err(BenchError::invalid_argument(format!(
            "thresholded canonical fixture replay from height {fixture_from_height} requires starting state kind {}, found {}",
            expected_kind.as_str(),
            starting_state.kind.as_str()
        )));
    }
    Ok(())
}

fn storage_candidate_identity(
    materialized_view_preset: Option<MaterializedViewPreset>,
) -> Result<StorageCandidateIdentity, BenchError> {
    match materialized_view_preset {
        None => Ok(StorageCandidateIdentity::rocksdb_canonical_store_range_replay()),
        Some(MaterializedViewPreset::Wallet | MaterializedViewPreset::Explorer) => {
            Ok(StorageCandidateIdentity::rocksdb_canonical_store_range_replay_with_diagnostic_materialized_view())
        }
        Some(_) => Err(BenchError::invalid_argument(
            "unsupported materialized-view preset for benchmark candidate identity",
        )),
    }
}

fn read_starting_checkpoint_manifest(
    canonical_store_path: &Path,
    required: bool,
) -> Result<Option<StartingCheckpointManifestEvidence>, BenchError> {
    let manifest_path = canonical_store_path.join(STARTING_CHECKPOINT_MANIFEST_FILE_NAME);
    let raw_manifest = match std::fs::read(&manifest_path) {
        Ok(raw_manifest) => raw_manifest,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(None);
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(BenchError::starting_checkpoint_manifest(format!(
                "thresholded replay requires {} at the canonical store root",
                manifest_path.display()
            )));
        }
        Err(source) => return Err(BenchError::io(&manifest_path, source)),
    };
    let manifest: StartingCheckpointManifest =
        serde_json::from_slice(&raw_manifest).map_err(|source| {
            BenchError::starting_checkpoint_manifest(format!(
                "{} is not a valid benchmark starting-store manifest: {source}",
                manifest_path.display()
            ))
        })?;
    if manifest.format_version != STARTING_CHECKPOINT_MANIFEST_FORMAT_VERSION {
        return Err(BenchError::starting_checkpoint_manifest(format!(
            "{} uses format version {}, expected {}",
            manifest_path.display(),
            manifest.format_version,
            STARTING_CHECKPOINT_MANIFEST_FORMAT_VERSION
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(&raw_manifest);
    Ok(Some(StartingCheckpointManifestEvidence {
        sha256: hex::encode(hasher.finalize()),
        manifest,
    }))
}

fn validate_starting_checkpoint_manifest(
    evidence: &StartingCheckpointManifestEvidence,
    expected_network: &str,
    chain_epoch: Option<&ChainEpoch>,
) -> Result<(), BenchError> {
    if evidence.manifest.network != expected_network {
        return Err(BenchError::starting_checkpoint_manifest(format!(
            "manifest network {} does not match fixture network {expected_network}",
            evidence.manifest.network
        )));
    }
    match (&evidence.manifest.canonical_position, chain_epoch) {
        (None, None) => Ok(()),
        (Some(expected), Some(actual)) => {
            let actual_tip_hash = encode_rpc_block_hash_hex(actual.visible_tip_hash);
            if expected.chain_epoch_id != actual.id.value()
                || expected.visible_tip_height != actual.visible_tip_height.value()
                || !expected
                    .visible_tip_hash
                    .eq_ignore_ascii_case(&actual_tip_hash)
                || expected.artifact_schema_version != actual.artifact_schema_version.value()
            {
                return Err(BenchError::starting_checkpoint_manifest(format!(
                    "manifest canonical position ({}, {}, {}, schema {}) does not match opened store ({}, {}, {}, schema {})",
                    expected.chain_epoch_id,
                    expected.visible_tip_height,
                    expected.visible_tip_hash,
                    expected.artifact_schema_version,
                    actual.id.value(),
                    actual.visible_tip_height.value(),
                    actual_tip_hash,
                    actual.artifact_schema_version.value()
                )));
            }
            Ok(())
        }
        (None, Some(actual)) => Err(BenchError::starting_checkpoint_manifest(format!(
            "manifest has no canonical position but opened store is at epoch {} height {}",
            actual.id.value(),
            actual.visible_tip_height.value()
        ))),
        (Some(expected), None) => Err(BenchError::starting_checkpoint_manifest(format!(
            "manifest expects canonical epoch {} height {} but opened store is empty",
            expected.chain_epoch_id, expected.visible_tip_height
        ))),
    }
}

fn starting_canonical_state(
    store: &PrimaryChainStore,
    checkpoint_manifest: Option<&StartingCheckpointManifestEvidence>,
    expected_network: &str,
) -> Result<StartingCanonicalState, BenchError> {
    let chain_epoch = store.current_chain_epoch()?;
    if let Some(evidence) = checkpoint_manifest {
        validate_starting_checkpoint_manifest(evidence, expected_network, chain_epoch.as_ref())?;
    }
    Ok(StartingCanonicalState {
        kind: match (checkpoint_manifest, chain_epoch.as_ref()) {
            (Some(_), _) => StartingCanonicalStateKind::Checkpoint,
            (None, None) => StartingCanonicalStateKind::Empty,
            (None, Some(_)) => StartingCanonicalStateKind::UnverifiedClone,
        },
        chain_epoch_id: chain_epoch.as_ref().map(|epoch| epoch.id.value()),
        tip_height: chain_epoch
            .as_ref()
            .map(|epoch| epoch.visible_tip_height.value()),
        tip_hash_rpc_hex: chain_epoch
            .as_ref()
            .map(|epoch| encode_rpc_block_hash_hex(epoch.visible_tip_hash)),
        artifact_schema_version: chain_epoch
            .as_ref()
            .map(|epoch| epoch.artifact_schema_version.value()),
        checkpoint_manifest_sha256: checkpoint_manifest.map(|evidence| evidence.sha256.clone()),
    })
}

fn open_canonical_store(
    config: &ReplayConfig,
    network: zinder_core::Network,
) -> Result<(PrimaryChainStore, ChainStoreOptions), BenchError> {
    let mut canonical_budget = RocksDbResourceBudget::canonical_writer_defaults();
    if let Some(block_cache_bytes) = config.canonical_block_cache_bytes {
        canonical_budget.block_cache_bytes = block_cache_bytes;
    }
    let options = ChainStoreOptions {
        rocksdb_resource_budget: canonical_budget,
        ..ChainStoreOptions::for_network(network)
    };
    let store = PrimaryChainStore::open(&config.store_path, options)?;
    Ok((store, options))
}

fn canonical_writer_settings(
    options: ChainStoreOptions,
) -> CanonicalStoreRangeReplayWriterSettings {
    CanonicalStoreRangeReplayWriterSettings {
        store_schema_version: CURRENT_STORE_SCHEMA_VERSION,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        sync_writes: options.sync_writes,
        durability_mode: if options.sync_writes {
            "rocksdb-wal-fsync-per-write-batch"
        } else {
            "rocksdb-wal-without-fsync"
        },
        rocksdb_resource_budget: RocksDbResourceBudgetSummary::from(
            options.rocksdb_resource_budget,
        ),
    }
}

fn prepare_materialized_view_replay(
    config: &ReplayConfig,
    canonical_store: &PrimaryChainStore,
) -> Result<Option<StreamCursorTokenV1>, BenchError> {
    if config.materialized_view_preset.is_none() {
        return Ok(None);
    }
    let materialized_view_store_path =
        MaterializedViewStore::path_for_canonical(&config.store_path);
    if materialized_view_store_path.exists() {
        return Err(BenchError::invalid_argument(format!(
            "materialized-view replay requires a fresh materialized-view store, but {} already exists; create the throwaway canonical clone without its materialized-views subdirectory",
            materialized_view_store_path.display()
        )));
    }
    if config.materialized_view_replay_scope == MaterializedViewReplayScope::RetainedHistory {
        return Ok(None);
    }
    canonical_store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )
        .map(|position| position.cursor)
        .map_err(BenchError::from)
}

fn open_materialized_view_store(
    config: &ReplayConfig,
    fixed_range_start_cursor: Option<&StreamCursorTokenV1>,
) -> Result<Option<MaterializedViewStore>, BenchError> {
    let Some(materialized_view_preset) = config.materialized_view_preset else {
        return Ok(None);
    };
    let materialized_view_store =
        open_primary_materialized_view_store_for_canonical_with_materialized_view_preset(
            &config.store_path,
            RocksDbResourceBudget::materialized_view_writer_defaults(),
            materialized_view_preset,
        )?;
    if config.materialized_view_replay_scope == MaterializedViewReplayScope::FixedRange {
        seed_materialized_view_consumers(&materialized_view_store, fixed_range_start_cursor)?;
    }
    Ok(Some(materialized_view_store))
}

async fn measure_materialized_view_replay(
    store_path: &Path,
    canonical_store: &PrimaryChainStore,
    materialized_view_preset: Option<MaterializedViewPreset>,
    materialized_view_store: Option<MaterializedViewStore>,
) -> Result<MaterializedViewMeasurements, BenchError> {
    let (Some(materialized_view_preset), Some(materialized_view_store)) =
        (materialized_view_preset, materialized_view_store)
    else {
        return Ok(MaterializedViewMeasurements::default());
    };
    let materialized_view_build_started_at = Instant::now();
    let logical_write_bytes_before = materialized_view_store.logical_write_bytes();
    catch_up_materialized_view_store_to_canonical(
        canonical_store,
        &materialized_view_store,
        bench_materialized_view_config(),
    )
    .await?;
    if materialized_view_store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME) {
        let _ = bootstrap_transparent_address_ranking(canonical_store, &materialized_view_store)
            .await?;
    }
    let wall_clock_seconds = materialized_view_build_started_at.elapsed().as_secs_f64();
    let logical_write_bytes = materialized_view_store
        .logical_write_bytes()
        .saturating_sub(logical_write_bytes_before);
    materialized_view_store.refresh_rocksdb_resource_metrics();
    let row_count =
        materialized_view_row_count(&materialized_view_store, materialized_view_preset)?;
    let materialized_view_store_path = MaterializedViewStore::path_for_canonical(store_path);
    let store_bytes = directory_bytes(&materialized_view_store_path)?;
    drop(materialized_view_store);

    let reopen_started_at = Instant::now();
    let reopened =
        open_primary_materialized_view_store_for_canonical_with_materialized_view_preset(
            store_path,
            RocksDbResourceBudget::materialized_view_writer_defaults(),
            materialized_view_preset,
        )?;
    let reopen_seconds = reopen_started_at.elapsed().as_secs_f64();
    validate_materialized_view_reached_canonical_tip(canonical_store, &reopened)?;
    drop(reopened);

    Ok(MaterializedViewMeasurements {
        wall_clock_seconds: Some(wall_clock_seconds),
        row_count: Some(row_count),
        event_cursor_at_tip: Some(true),
        store_bytes: Some(store_bytes),
        reopen_seconds: Some(reopen_seconds),
        logical_write_bytes: Some(logical_write_bytes),
    })
}

fn validate_materialized_view_reached_canonical_tip(
    canonical_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
) -> Result<(), BenchError> {
    let expected_cursor = canonical_store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor;
    let consumer_names = materialized_view_store
        .chain_event_consumer_names()
        .chain(materialized_view_store.event_only_chain_event_consumer_names());
    for consumer_name in consumer_names {
        let consumer_cursor = materialized_view_store.get_chain_event_cursor(consumer_name)?;
        let cursor_matches = match (&expected_cursor, &consumer_cursor) {
            (None, None) => true,
            (Some(expected), Some(actual)) => expected.as_bytes() == actual,
            _ => false,
        };
        if !cursor_matches {
            return Err(BenchError::materialized_view_build_incomplete(format!(
                "materialized view {} did not reach the canonical event tip",
                consumer_name.as_str()
            )));
        }
    }
    Ok(())
}

/// Seeds every selected materialized-view consumer at the canonical store's current
/// event cursor so a fixed-range benchmark excludes earlier retained history.
///
/// The materialized-view store must be fresh. Production recovery must never call this
/// helper because it intentionally declares earlier history out of scope.
pub fn seed_materialized_view_replay_at_canonical_tip(
    canonical_store: &PrimaryChainStore,
    materialized_view_store: &zinder_materialized_views::MaterializedViewStore,
) -> Result<Option<StreamCursorTokenV1>, BenchError> {
    let cursor = canonical_store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor;
    seed_materialized_view_consumers(materialized_view_store, cursor.as_ref())?;
    Ok(cursor)
}

fn seed_materialized_view_consumers(
    materialized_view_store: &zinder_materialized_views::MaterializedViewStore,
    cursor: Option<&StreamCursorTokenV1>,
) -> Result<(), BenchError> {
    let consumer_names = materialized_view_store
        .chain_event_consumer_names()
        .filter(|name| *name != TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)
        .chain(materialized_view_store.event_only_chain_event_consumer_names())
        .collect::<Vec<_>>();
    for consumer_name in &consumer_names {
        if materialized_view_store
            .get_chain_event_cursor(*consumer_name)?
            .is_some()
        {
            return Err(BenchError::invalid_argument(
                "fixed-range materialized-view replay requires a fresh materialized-view store",
            ));
        }
    }

    if let Some(cursor) = cursor {
        for consumer_name in consumer_names {
            materialized_view_store.put_chain_event_cursor(consumer_name, cursor.as_bytes())?;
        }
    }
    Ok(())
}

fn validate_starting_tip(
    fixture_from_height: u32,
    starting_tip_height: Option<u32>,
) -> Result<(), BenchError> {
    let starts_at_empty_chain = fixture_from_height == 1 && starting_tip_height.is_none();
    let continues_from_previous_height = starting_tip_height
        .is_some_and(|tip_height| tip_height.checked_add(1) == Some(fixture_from_height));
    if starts_at_empty_chain || continues_from_previous_height {
        Ok(())
    } else {
        Err(BenchError::invalid_argument(format!(
            "fixture starts at height {fixture_from_height}, but the cloned store tip is {starting_tip_height:?}"
        )))
    }
}

fn materialized_view_row_count(
    materialized_view_store: &zinder_materialized_views::MaterializedViewStore,
    materialized_view_preset: MaterializedViewPreset,
) -> Result<u64, BenchError> {
    let mut row_count = 0_u64;
    for schema in materialized_view_preset.consumer_schemas() {
        for column_family in schema.column_families {
            row_count = row_count
                .saturating_add(materialized_view_store.consumer_row_count(column_family)?);
        }
    }
    Ok(row_count)
}

fn directory_bytes(path: &Path) -> Result<u64, BenchError> {
    let entries = std::fs::read_dir(path).map_err(|source| BenchError::io(path, source))?;
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|source| BenchError::io(path, source))?;
        let entry_path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| BenchError::io(&entry_path, source))?;
        bytes = if metadata.is_dir() {
            bytes.saturating_add(directory_bytes(&entry_path)?)
        } else {
            bytes.saturating_add(metadata.len())
        };
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        BenchError, StartingCanonicalState, StartingCanonicalStateKind,
        validate_acceptance_starting_state,
    };

    #[test]
    fn thresholded_genesis_rejects_an_unverified_height_zero_clone() -> Result<(), BenchError> {
        let starting_state = StartingCanonicalState {
            kind: StartingCanonicalStateKind::UnverifiedClone,
            chain_epoch_id: Some(1),
            tip_height: Some(0),
            tip_hash_rpc_hex: Some("00".repeat(32)),
            artifact_schema_version: Some(18),
            checkpoint_manifest_sha256: None,
        };

        let Some(error) = validate_acceptance_starting_state(true, 1, &starting_state).err() else {
            return Err(BenchError::invalid_argument(
                "unverified height-zero clone must not prove a fresh-genesis start",
            ));
        };

        assert!(
            error
                .to_string()
                .contains("requires starting state kind empty")
        );
        Ok(())
    }
}
