//! Fixed-range replay: run the real bulk-catchup pipeline over a captured
//! fixture and a cloned canonical store, then assemble a report.

use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use metrics_exporter_prometheus::PrometheusHandle;
use zinder_derive::{DeriveStore, ProjectionPreset, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME};
use zinder_ingest::{
    RawBlobPolicy,
    bench_support::{BenchBulkCatchupParams, bench_bulk_catchup_run_config, bench_derive_config},
    bootstrap_transparent_address_ranking, catch_up_derive_store_to_canonical,
    open_primary_derive_store_for_canonical_with_projection_preset, run_bulk_catchup_with_store,
};
use zinder_store::{
    ChainEventStreamFamily, ChainStoreOptions, EventStreamStartPosition, PrimaryChainStore,
    RocksDbResourceBudget, StreamCursorTokenV1,
};

use crate::{
    error::BenchError,
    fixture::{FixtureManifest, FixtureNodeSource},
    report::{FixtureSummary, ReplayMeasurements, Report, build_report},
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
    /// Optional canonical block-cache override in bytes (the cache-size knob).
    pub canonical_block_cache_bytes: Option<u64>,
    /// Projection preset to replay after canonical ingest, or `None` for a
    /// canonical-only run.
    pub projection_preset: Option<ProjectionPreset>,
    /// Portion of canonical event history presented to the selected
    /// projections.
    pub projection_replay_scope: ProjectionReplayScope,
}

/// Canonical event-history scope used for a projection benchmark arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProjectionReplayScope {
    /// Seed fresh projection cursors at the cloned store tip, then measure only
    /// events produced by the captured range.
    #[default]
    FixedRange,
    /// Start fresh projections without cursors and rebuild all retained
    /// canonical event history.
    RetainedHistory,
}

impl ProjectionReplayScope {
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
struct ProjectionMeasurements {
    wall_clock_seconds: Option<f64>,
    row_count: Option<u64>,
    lag_blocks: Option<u64>,
    store_bytes: Option<u64>,
    reopen_seconds: Option<f64>,
    write_bytes: Option<u64>,
}

/// Replays the captured range and returns a machine-readable report.
///
/// When `metrics_handle` is `Some`, the report includes per-caller store reads
/// and `RocksDB` tickers scraped from the in-process recorder; when `None`, the
/// report carries only the directly measured scalars.
pub async fn replay_fixture(
    config: ReplayConfig,
    metrics_handle: Option<PrometheusHandle>,
) -> Result<Report, BenchError> {
    let manifest = FixtureManifest::read(&config.fixture_directory)?;
    let network = manifest.network_typed()?;
    let activations = Arc::new(manifest.activations_typed()?);
    let source = FixtureNodeSource::open(&config.fixture_directory, &manifest)?;

    let mut canonical_budget = RocksDbResourceBudget::canonical_writer_defaults();
    if let Some(block_cache_bytes) = config.canonical_block_cache_bytes {
        canonical_budget.block_cache_bytes = block_cache_bytes;
    }
    let store = PrimaryChainStore::open(
        &config.store_path,
        ChainStoreOptions {
            rocksdb_resource_budget: canonical_budget,
            ..ChainStoreOptions::for_network(network)
        },
    )?;

    let tip_height_before = current_tip_height(&store)?;
    validate_starting_tip(manifest.from_height, tip_height_before)?;
    let derive_store = open_projection_store(&config, &store)?;

    let run_config = bench_bulk_catchup_run_config(BenchBulkCatchupParams {
        network,
        storage_path: config.store_path.clone(),
        from_height: zinder_core::BlockHeight::new(manifest.from_height),
        to_height: zinder_core::BlockHeight::new(manifest.to_height),
        block_prepare_concurrency: config.block_prepare_concurrency,
        canonical_rocksdb_budget: canonical_budget,
        raw_blob_policy: RawBlobPolicy::None,
        network_upgrade_activations: activations,
    });

    let started_at = Instant::now();
    run_bulk_catchup_with_store(&run_config, &source, &store).await?;
    let wall_clock_seconds = started_at.elapsed().as_secs_f64();
    let tip_height_after = current_tip_height(&store)?;

    let projection_measurements = measure_projection_replay(
        &config.store_path,
        &store,
        config.projection_preset,
        derive_store,
    )
    .await?;

    let exposition = metrics_handle.map(|handle| handle.render());
    let fixture = FixtureSummary {
        network: manifest.network.clone(),
        from_height: manifest.from_height,
        to_height: manifest.to_height,
        block_count: manifest.block_count,
        workload_density: manifest.workload_density,
        segment_count: manifest.segments.len(),
    };
    let measurements = ReplayMeasurements {
        block_prepare_concurrency: config.block_prepare_concurrency.get(),
        projection_preset: config.projection_preset.map(ProjectionPreset::as_str),
        projection_replay_scope: config
            .projection_preset
            .map(|_| config.projection_replay_scope.as_str()),
        wall_clock_seconds,
        tip_height_before,
        tip_height_after,
        derive_wall_clock_seconds: projection_measurements.wall_clock_seconds,
        projection_row_count: projection_measurements.row_count,
        projection_lag_blocks: projection_measurements.lag_blocks,
        derive_store_bytes: projection_measurements.store_bytes,
        derive_reopen_seconds: projection_measurements.reopen_seconds,
        derive_bytes_written: projection_measurements.write_bytes,
        peak_rss: peak_rss(),
    };
    Ok(build_report(fixture, measurements, exposition.as_deref()))
}

fn open_projection_store(
    config: &ReplayConfig,
    canonical_store: &PrimaryChainStore,
) -> Result<Option<DeriveStore>, BenchError> {
    let Some(projection_preset) = config.projection_preset else {
        return Ok(None);
    };
    let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &config.store_path,
        RocksDbResourceBudget::derive_writer_defaults(),
        projection_preset,
    )?;
    if config.projection_replay_scope == ProjectionReplayScope::FixedRange {
        seed_projection_replay_at_canonical_tip(canonical_store, &derive_store)?;
    }
    Ok(Some(derive_store))
}

async fn measure_projection_replay(
    store_path: &Path,
    canonical_store: &PrimaryChainStore,
    projection_preset: Option<ProjectionPreset>,
    derive_store: Option<DeriveStore>,
) -> Result<ProjectionMeasurements, BenchError> {
    let (Some(projection_preset), Some(derive_store)) = (projection_preset, derive_store) else {
        return Ok(ProjectionMeasurements::default());
    };
    let derive_started_at = Instant::now();
    let write_bytes_before = derive_store.logical_write_bytes();
    catch_up_derive_store_to_canonical(canonical_store, &derive_store, bench_derive_config())
        .await?;
    if derive_store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME) {
        let _ = bootstrap_transparent_address_ranking(canonical_store, &derive_store).await?;
    }
    let wall_clock_seconds = derive_started_at.elapsed().as_secs_f64();
    let write_bytes = derive_store
        .logical_write_bytes()
        .saturating_sub(write_bytes_before);
    derive_store.refresh_rocksdb_resource_metrics();
    let row_count = projection_row_count(&derive_store, projection_preset)?;
    // The fixture source and canonical store are immutable after the bulk
    // replay. A successful catch-up exhausts that fixed event history, so the
    // selected projections are exactly at the canonical tip.
    let lag_blocks = 0;
    let derive_path = DeriveStore::path_for_canonical(store_path);
    let store_bytes = directory_bytes(&derive_path)?;
    drop(derive_store);

    let reopen_started_at = Instant::now();
    let reopened = open_primary_derive_store_for_canonical_with_projection_preset(
        store_path,
        RocksDbResourceBudget::derive_writer_defaults(),
        projection_preset,
    )?;
    let reopen_seconds = reopen_started_at.elapsed().as_secs_f64();
    drop(reopened);

    Ok(ProjectionMeasurements {
        wall_clock_seconds: Some(wall_clock_seconds),
        row_count: Some(row_count),
        lag_blocks: Some(lag_blocks),
        store_bytes: Some(store_bytes),
        reopen_seconds: Some(reopen_seconds),
        write_bytes: Some(write_bytes),
    })
}

/// Seeds every selected projection consumer at the canonical store's current
/// event cursor so a fixed-range benchmark excludes earlier retained history.
///
/// The derive store must be fresh. Production recovery must never call this
/// helper because it intentionally declares earlier history out of scope.
pub fn seed_projection_replay_at_canonical_tip(
    canonical_store: &PrimaryChainStore,
    derive_store: &zinder_derive::DeriveStore,
) -> Result<Option<StreamCursorTokenV1>, BenchError> {
    let consumer_names = derive_store
        .chain_event_consumer_names()
        .filter(|name| *name != TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)
        .chain(derive_store.event_only_chain_event_consumer_names())
        .collect::<Vec<_>>();
    for consumer_name in &consumer_names {
        if derive_store
            .get_chain_event_cursor(*consumer_name)?
            .is_some()
        {
            return Err(BenchError::invalid_argument(
                "fixed-range projection replay requires a fresh derive store",
            ));
        }
    }

    let Some(cursor) = canonical_store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Tip,
        )?
        .cursor
    else {
        return Ok(None);
    };
    for consumer_name in consumer_names {
        derive_store.put_chain_event_cursor(consumer_name, cursor.as_bytes())?;
    }
    Ok(Some(cursor))
}

fn validate_starting_tip(
    fixture_from_height: u32,
    tip_height_before: Option<u32>,
) -> Result<(), BenchError> {
    let starts_at_empty_chain = fixture_from_height == 1 && tip_height_before.is_none();
    let continues_from_previous_height = tip_height_before
        .is_some_and(|tip_height| tip_height.checked_add(1) == Some(fixture_from_height));
    if starts_at_empty_chain || continues_from_previous_height {
        Ok(())
    } else {
        Err(BenchError::invalid_argument(format!(
            "fixture starts at height {fixture_from_height}, but the cloned store tip is {tip_height_before:?}"
        )))
    }
}

fn projection_row_count(
    derive_store: &zinder_derive::DeriveStore,
    projection_preset: ProjectionPreset,
) -> Result<u64, BenchError> {
    let mut row_count = 0_u64;
    for schema in projection_preset.consumer_schemas() {
        for column_family in schema.column_families {
            row_count = row_count.saturating_add(derive_store.consumer_row_count(column_family)?);
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

fn current_tip_height(store: &PrimaryChainStore) -> Result<Option<u32>, BenchError> {
    Ok(store
        .current_chain_epoch()?
        .map(|chain_epoch| chain_epoch.visible_tip_height.value()))
}
