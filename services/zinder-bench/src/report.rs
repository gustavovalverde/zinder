//! Machine-readable benchmark report and its assembly from scraped metrics.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{
    fixture::WorkloadDensity,
    metrics_scrape::{MetricSample, parse_prometheus_samples, sum_by_name},
    rss::PeakRss,
};

const READ_DURATION_COUNT: &str = "zinder_store_read_duration_seconds_count";
const READ_DURATION_SUM: &str = "zinder_store_read_duration_seconds_sum";
const MULTI_GET_KEYS_TOTAL: &str = "zinder_store_multi_get_keys_total";
const MULTI_GET_RESOLVED_TOTAL: &str = "zinder_store_multi_get_resolved_total";
const ROCKSDB_TICKER: &str = "zinder_store_rocksdb_ticker";
const ROCKSDB_COMPACT_READ_BYTES: &str = "rocksdb.compact.read.bytes";
const ROCKSDB_COMPACT_WRITE_BYTES: &str = "rocksdb.compact.write.bytes";
const DERIVE_PRIMARY_STORE_ROLE: &str = "derive_primary";
const COMMIT_DURATION_COUNT: &str = "zinder_ingest_commit_duration_seconds_count";
const COMMIT_FALLBACK_CALLER: &str = "commit_fallback";
const HEAD_OF_LINE_WAIT_SUM: &str = "zinder_ingest_bulk_pipeline_head_of_line_wait_seconds_sum";
const HEAD_OF_LINE_WAIT_COUNT: &str = "zinder_ingest_bulk_pipeline_head_of_line_wait_seconds_count";
const BLOCK_PREPARE_STAGE_DURATION_COUNT: &str =
    "zinder_ingest_block_prepare_stage_duration_seconds_count";
const BLOCK_PREPARE_STAGE_DURATION_SUM: &str =
    "zinder_ingest_block_prepare_stage_duration_seconds_sum";
const BLOCK_DERIVE_STAGE_DURATION_COUNT: &str =
    "zinder_ingest_block_derive_stage_duration_seconds_count";
const BLOCK_DERIVE_STAGE_DURATION_SUM: &str =
    "zinder_ingest_block_derive_stage_duration_seconds_sum";

/// Fixture identity echoed into the report.
#[derive(Clone, Debug, Serialize)]
pub struct FixtureSummary {
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

/// Direct measurements taken around the replay call.
#[derive(Clone, Copy, Debug)]
pub struct ReplayMeasurements {
    /// Prepare concurrency the run used.
    pub block_prepare_concurrency: u32,
    /// Projection preset replayed after canonical ingest, or `None` for a
    /// canonical-only run.
    pub projection_preset: Option<&'static str>,
    /// Projection history scope, or `None` for a canonical-only run.
    pub projection_replay_scope: Option<&'static str>,
    /// Wall-clock seconds spent in the canonical replay call.
    pub wall_clock_seconds: f64,
    /// Store tip height before replay.
    pub tip_height_before: Option<u32>,
    /// Store tip height after replay.
    pub tip_height_after: Option<u32>,
    /// Wall-clock seconds spent in derive catch-up, when driven.
    pub derive_wall_clock_seconds: Option<f64>,
    /// Total rows across the selected consumers' owned column families.
    pub projection_row_count: Option<u64>,
    /// Selected projection lag behind the canonical tip after replay.
    pub projection_lag_blocks: Option<u64>,
    /// Final on-disk bytes under the derive-store directory.
    pub derive_store_bytes: Option<u64>,
    /// Seconds required to close and reopen the populated derive store.
    pub derive_reopen_seconds: Option<f64>,
    /// Serialized bytes submitted in successful derive write batches.
    pub derive_bytes_written: Option<u64>,
    /// Peak resident-set-size reading.
    pub peak_rss: PeakRss,
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

/// Aggregated work time for one block-prepare or block-derive substage.
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
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ReplaySummary {
    /// Prepare concurrency the run used.
    pub block_prepare_concurrency: u32,
    /// Projection preset replayed after canonical ingest, or `None` for a
    /// canonical-only run.
    pub projection_preset: Option<&'static str>,
    /// Projection history scope, or `None` for a canonical-only run.
    pub projection_replay_scope: Option<&'static str>,
    /// Wall-clock seconds spent in the canonical replay call.
    pub wall_clock_seconds: f64,
    /// Store tip height before replay.
    pub tip_height_before: Option<u32>,
    /// Store tip height after replay.
    pub tip_height_after: Option<u32>,
    /// Blocks committed during replay (tip delta).
    pub blocks_committed: u64,
    /// Chain epochs committed during replay.
    pub epochs_committed: u64,
    /// Committed blocks per wall-clock second.
    pub blocks_per_second: f64,
    /// Commit-fallback read calls (near zero confirms prefetch coverage).
    pub commit_fallback_reads: u64,
    /// Wall-clock seconds spent in derive catch-up, when driven.
    pub derive_wall_clock_seconds: Option<f64>,
    /// Total rows across the selected consumers' owned column families.
    pub projection_row_count: Option<u64>,
    /// Selected projection lag behind the canonical tip after replay.
    pub projection_lag_blocks: Option<u64>,
    /// Final on-disk bytes under the derive-store directory.
    pub derive_store_bytes: Option<u64>,
    /// Bytes written to the derive `RocksDB` write-ahead log during the process.
    pub derive_bytes_written: Option<u64>,
    /// Bytes read plus written by derive-store compactions.
    pub derive_compaction_bytes: Option<u64>,
    /// Seconds required to close and reopen the populated derive store.
    pub derive_reopen_seconds: Option<f64>,
    /// Peak resident-set-size reading.
    pub peak_rss: PeakRss,
}

/// The full benchmark report.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Fixture identity.
    pub fixture: FixtureSummary,
    /// Replay-derived scalars.
    pub replay: ReplaySummary,
    /// Per-caller canonical-store read timing.
    pub store_reads: Vec<CallerReadStat>,
    /// Per-caller `multi_get` key accounting.
    pub multi_get: Vec<MultiGetStat>,
    /// Per-stage bulk-pipeline head-of-line wait totals.
    pub head_of_line_wait: Vec<StageWaitStat>,
    /// Per-substage block preparation and derivation timing.
    pub stage_durations: Vec<StageDurationStat>,
    /// Exported `RocksDB` statistics tickers.
    pub rocksdb_tickers: Vec<TickerStat>,
}

/// Builds the report from direct measurements and the scraped exposition text.
#[must_use]
pub fn build_report(
    fixture: FixtureSummary,
    measurements: ReplayMeasurements,
    exposition: Option<&str>,
) -> Report {
    let samples = exposition.map(parse_prometheus_samples).unwrap_or_default();
    let store_reads = aggregate_store_reads(&samples);
    let multi_get = aggregate_multi_get(&samples);
    let head_of_line_wait = aggregate_head_of_line_wait(&samples);
    let stage_durations = aggregate_stage_durations(&samples);
    let rocksdb_tickers = aggregate_tickers(&samples);
    let derive_compaction_bytes = measurements.projection_preset.map(|_| {
        ticker_reading(
            &rocksdb_tickers,
            ROCKSDB_COMPACT_READ_BYTES,
            DERIVE_PRIMARY_STORE_ROLE,
        )
        .saturating_add(ticker_reading(
            &rocksdb_tickers,
            ROCKSDB_COMPACT_WRITE_BYTES,
            DERIVE_PRIMARY_STORE_ROLE,
        ))
    });
    let epochs_committed = round_to_u64(sum_by_name(&samples, COMMIT_DURATION_COUNT));
    let commit_fallback_reads = store_reads
        .iter()
        .filter(|stat| stat.caller == COMMIT_FALLBACK_CALLER)
        .map(|stat| stat.call_count)
        .sum();
    let blocks_committed = blocks_committed(
        measurements.tip_height_before,
        measurements.tip_height_after,
    );
    let blocks_per_second = if measurements.wall_clock_seconds > 0.0 {
        u64_to_f64(blocks_committed) / measurements.wall_clock_seconds
    } else {
        0.0
    };
    let replay = ReplaySummary {
        block_prepare_concurrency: measurements.block_prepare_concurrency,
        projection_preset: measurements.projection_preset,
        projection_replay_scope: measurements.projection_replay_scope,
        wall_clock_seconds: measurements.wall_clock_seconds,
        tip_height_before: measurements.tip_height_before,
        tip_height_after: measurements.tip_height_after,
        blocks_committed,
        epochs_committed,
        blocks_per_second,
        commit_fallback_reads,
        derive_wall_clock_seconds: measurements.derive_wall_clock_seconds,
        projection_row_count: measurements.projection_row_count,
        projection_lag_blocks: measurements.projection_lag_blocks,
        derive_store_bytes: measurements.derive_store_bytes,
        derive_bytes_written: measurements.derive_bytes_written,
        derive_compaction_bytes,
        derive_reopen_seconds: measurements.derive_reopen_seconds,
        peak_rss: measurements.peak_rss,
    };
    Report {
        fixture,
        replay,
        store_reads,
        multi_get,
        head_of_line_wait,
        stage_durations,
        rocksdb_tickers,
    }
}

fn ticker_reading(tickers: &[TickerStat], ticker: &str, store_role: &str) -> u64 {
    tickers
        .iter()
        .find(|stat| stat.ticker == ticker && stat.store_role == store_role)
        .map_or(0, |stat| round_to_u64(stat.reading))
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
            "block_derive",
            BLOCK_DERIVE_STAGE_DURATION_COUNT,
            BLOCK_DERIVE_STAGE_DURATION_SUM,
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
    use super::{aggregate_stage_durations, parse_prometheus_samples};

    #[test]
    fn stage_duration_report_preserves_family_stage_and_status() {
        let samples = parse_prometheus_samples(
            "zinder_ingest_block_derive_stage_duration_seconds_count{stage=\"block_parse\",status=\"ok\"} 4\n\
             zinder_ingest_block_derive_stage_duration_seconds_sum{stage=\"block_parse\",status=\"ok\"} 1.5\n",
        );

        let stats = aggregate_stage_durations(&samples);

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].family, "block_derive");
        assert_eq!(stats[0].stage, "block_parse");
        assert_eq!(stats[0].status, "ok");
        assert_eq!(stats[0].call_count, 4);
        assert!((stats[0].task_seconds - 1.5).abs() < f64::EPSILON);
    }
}
