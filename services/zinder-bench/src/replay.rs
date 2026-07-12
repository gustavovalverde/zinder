//! Fixed-range replay: run the real bulk-catchup pipeline over a captured
//! fixture and a cloned canonical store, then assemble a report.

use std::{num::NonZeroU32, path::PathBuf, sync::Arc, time::Instant};

use metrics_exporter_prometheus::PrometheusHandle;
use zinder_ingest::{
    RawBlobPolicy,
    bench_support::{BenchBulkCatchupParams, bench_bulk_catchup_run_config, bench_derive_config},
    catch_up_derive_store_to_canonical, open_primary_derive_store_for_canonical,
    run_bulk_catchup_with_store,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore, RocksDbResourceBudget};

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
    /// Whether to drive derive replay in the same run.
    pub run_derive: bool,
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

    let derive_wall_clock_seconds = if config.run_derive {
        let derive_store = open_primary_derive_store_for_canonical(
            &config.store_path,
            RocksDbResourceBudget::derive_writer_defaults(),
        )?;
        let derive_started_at = Instant::now();
        catch_up_derive_store_to_canonical(&store, &derive_store, bench_derive_config()).await?;
        Some(derive_started_at.elapsed().as_secs_f64())
    } else {
        None
    };

    let exposition = metrics_handle.map(|handle| handle.render());
    let fixture = FixtureSummary {
        network: manifest.network.clone(),
        from_height: manifest.from_height,
        to_height: manifest.to_height,
        block_count: manifest.block_count,
        segment_count: manifest.segments.len(),
    };
    let measurements = ReplayMeasurements {
        block_prepare_concurrency: config.block_prepare_concurrency.get(),
        derive_enabled: config.run_derive,
        wall_clock_seconds,
        tip_height_before,
        tip_height_after,
        derive_wall_clock_seconds,
        peak_rss: peak_rss(),
    };
    Ok(build_report(fixture, measurements, exposition.as_deref()))
}

fn current_tip_height(store: &PrimaryChainStore) -> Result<Option<u32>, BenchError> {
    Ok(store
        .current_chain_epoch()?
        .map(|chain_epoch| chain_epoch.visible_tip_height.value()))
}
