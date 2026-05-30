//! Runtime memory pressure sampling for ingest scheduling.
//!
//! Canonical ingest and derive replay share one process in the standard
//! deployment. The scheduler needs cheap, lossy memory signals so rebuildable
//! derive projections can back off before they compete with canonical writes.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";
const PROC_SELF_STATUS: &str = "/proc/self/status";

/// Default cadence for runtime memory gauges exported by the ingest process.
pub const DEFAULT_RUNTIME_MEMORY_METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// Point-in-time process and cgroup memory sample.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "the byte suffix keeps memory metric units explicit at every call site"
)]
pub(crate) struct RuntimeMemorySnapshot {
    pub(crate) cgroup_current_bytes: Option<u64>,
    pub(crate) cgroup_max_bytes: Option<u64>,
    pub(crate) cgroup_high_bytes: Option<u64>,
    pub(crate) cgroup_swap_current_bytes: Option<u64>,
    pub(crate) cgroup_anon_bytes: Option<u64>,
    pub(crate) cgroup_file_bytes: Option<u64>,
    pub(crate) cgroup_inactive_file_bytes: Option<u64>,
    pub(crate) cgroup_active_file_bytes: Option<u64>,
    pub(crate) cgroup_kernel_bytes: Option<u64>,
    pub(crate) cgroup_slab_bytes: Option<u64>,
    pub(crate) process_rss_bytes: Option<u64>,
    pub(crate) process_rss_anon_bytes: Option<u64>,
}

impl RuntimeMemorySnapshot {
    pub(crate) fn sample() -> Self {
        Self::sample_from_paths(
            Path::new(CGROUP_ROOT),
            Path::new(PROC_SELF_CGROUP),
            Path::new(PROC_SELF_STATUS),
        )
    }

    fn sample_from_paths(cgroup_root: &Path, proc_cgroup: &Path, proc_status: &Path) -> Self {
        let cgroup_dir = cgroup_v2_dir(cgroup_root, proc_cgroup);
        let cgroup_memory_stat = read_cgroup_memory_stat(&cgroup_dir);
        let (process_rss_bytes, process_rss_anon_bytes) = process_rss_bytes(proc_status);
        Self {
            cgroup_current_bytes: read_cgroup_u64(&cgroup_dir, "memory.current"),
            cgroup_max_bytes: read_cgroup_u64(&cgroup_dir, "memory.max"),
            cgroup_high_bytes: read_cgroup_u64(&cgroup_dir, "memory.high"),
            cgroup_swap_current_bytes: read_cgroup_u64(&cgroup_dir, "memory.swap.current"),
            cgroup_anon_bytes: cgroup_memory_stat.anon,
            cgroup_file_bytes: cgroup_memory_stat.file,
            cgroup_inactive_file_bytes: cgroup_memory_stat.inactive_file,
            cgroup_active_file_bytes: cgroup_memory_stat.active_file,
            cgroup_kernel_bytes: cgroup_memory_stat.kernel,
            cgroup_slab_bytes: cgroup_memory_stat.slab,
            process_rss_bytes,
            process_rss_anon_bytes,
        }
    }

    pub(crate) fn pressure_ratio(self) -> Option<f64> {
        self.working_set_pressure_ratio()
            .or_else(|| self.current_pressure_ratio())
    }

    pub(crate) fn current_pressure_ratio(self) -> Option<f64> {
        let current = self.cgroup_current_bytes?;
        let limit = self.cgroup_high_bytes.or(self.cgroup_max_bytes)?;
        if limit == 0 {
            return None;
        }
        Some(u64_to_f64(current) / u64_to_f64(limit))
    }

    pub(crate) fn working_set_bytes(self) -> Option<u64> {
        let current = self.cgroup_current_bytes?;
        Some(current.saturating_sub(self.cgroup_inactive_file_bytes.unwrap_or(0)))
    }

    pub(crate) fn working_set_pressure_ratio(self) -> Option<f64> {
        let current = self.working_set_bytes()?;
        let limit = self.cgroup_high_bytes.or(self.cgroup_max_bytes)?;
        if limit == 0 {
            return None;
        }
        Some(u64_to_f64(current) / u64_to_f64(limit))
    }
}

/// Returns the container memory budget in bytes, derived from cgroup v2.
///
/// Prefers `memory.high` (the soft throttle where the kernel starts
/// reclaiming) over `memory.max` (the hard kill threshold) so the
/// budget describes the limit Zinder should stay under, not the limit
/// past which the kernel kills it.
///
/// Returns `None` when cgroup v2 is unavailable (dev hosts without
/// containers, macOS, older Linux) or when the limit is unset (the
/// kernel exposes the literal string `max`, which the cgroup reader
/// translates to `None`). Callers in that case fall through to their
/// hand-tuned fallback constants, matching pre-existing behavior on
/// dev hosts.
///
/// Used by the binary's config layer to size the bulk-catchup pipeline
/// queue caps so a deploy on Railway, Fly, ECS, or any cgroup-enforcing
/// container runtime inherits memory-aware defaults without per-deploy
/// env-var tuning. Existing `ZINDER_INGEST__BULK_CATCHUP__*_BYTES`
/// overrides still take precedence; this helper only changes the
/// default.
#[must_use]
pub fn container_memory_budget_bytes() -> Option<u64> {
    container_memory_budget_from_snapshot(RuntimeMemorySnapshot::sample())
}

/// Pure-function variant of [`container_memory_budget_bytes`] used by
/// tests and any caller that already holds a snapshot.
pub(crate) fn container_memory_budget_from_snapshot(
    snapshot: RuntimeMemorySnapshot,
) -> Option<u64> {
    let limit = snapshot.cgroup_high_bytes.or(snapshot.cgroup_max_bytes)?;
    if limit == 0 { None } else { Some(limit) }
}

/// Spawns the runtime memory gauge sampler for the ingest process.
///
/// Memory metrics are operational state, not derive replay state. Sampling them
/// in an independent task keeps `/metrics` fresh even while canonical catchup or
/// derive replay spends a long time inside a single work pass.
#[must_use = "drop the handle to detach the runtime memory sampler or await it for symmetric shutdown"]
pub fn spawn_runtime_memory_metrics_task(
    interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks_since_log: u32 = 0;
        loop {
            let snapshot = RuntimeMemorySnapshot::sample();
            record_runtime_memory_metrics(snapshot);
            if ticks_since_log == 0 {
                log_runtime_memory_observation(snapshot);
            }
            ticks_since_log = (ticks_since_log + 1) % RUNTIME_MEMORY_LOG_EVERY_TICKS;

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
        }
    })
}

/// Number of `interval` ticks between operational memory log lines.
///
/// The gauge sampler runs every second; logging every sample would flood the
/// deploy log, so the cgroup limit and pressure ratio surface roughly once a
/// minute. This is the denominator (`memory.high`/`memory.max`) that makes the
/// derive replay pressure ratio interpretable from logs alone.
const RUNTIME_MEMORY_LOG_EVERY_TICKS: u32 = 60;

/// Logs the cgroup memory limit, working set, and pressure ratio at INFO.
///
/// Without this the container memory budget is only on the (often unscraped)
/// metrics endpoint, so a paused or memory-throttled derive plane is hard to
/// diagnose from logs. Best-effort and lossy: unset cgroup fields log as
/// `None`.
fn log_runtime_memory_observation(snapshot: RuntimeMemorySnapshot) {
    tracing::info!(
        target: "zinder::ingest",
        event = "runtime_memory_observed",
        cgroup_limit_bytes = ?container_memory_budget_from_snapshot(snapshot),
        cgroup_current_bytes = ?snapshot.cgroup_current_bytes,
        working_set_bytes = ?snapshot.working_set_bytes(),
        inactive_file_bytes = ?snapshot.cgroup_inactive_file_bytes,
        process_rss_anon_bytes = ?snapshot.process_rss_anon_bytes,
        pressure_ratio = ?snapshot.pressure_ratio(),
        "runtime memory observation",
    );
}

pub(crate) fn record_runtime_memory_metrics(snapshot: RuntimeMemorySnapshot) {
    if let Some(bytes) = snapshot.cgroup_current_bytes {
        metrics::gauge!("zinder_ingest_memory_current_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_max_bytes {
        metrics::gauge!("zinder_ingest_memory_max_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_high_bytes {
        metrics::gauge!("zinder_ingest_memory_high_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_swap_current_bytes {
        metrics::gauge!("zinder_ingest_memory_swap_current_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_anon_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_anon_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_file_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_file_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_inactive_file_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_inactive_file_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_active_file_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_active_file_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_kernel_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_kernel_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.cgroup_slab_bytes {
        metrics::gauge!("zinder_ingest_memory_cgroup_slab_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.working_set_bytes() {
        metrics::gauge!("zinder_ingest_memory_working_set_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.process_rss_bytes {
        metrics::gauge!("zinder_ingest_process_rss_bytes").set(u64_to_f64(bytes));
    }
    if let Some(bytes) = snapshot.process_rss_anon_bytes {
        metrics::gauge!("zinder_ingest_process_rss_anon_bytes").set(u64_to_f64(bytes));
    }
    if let Some(ratio) = snapshot.pressure_ratio() {
        metrics::gauge!("zinder_ingest_memory_pressure_ratio").set(ratio);
    }
    if let Some(ratio) = snapshot.current_pressure_ratio() {
        metrics::gauge!("zinder_ingest_memory_current_pressure_ratio").set(ratio);
    }
}

fn cgroup_v2_dir(cgroup_root: &Path, proc_cgroup: &Path) -> PathBuf {
    let Ok(cgroup_text) = fs::read_to_string(proc_cgroup) else {
        return cgroup_root.to_path_buf();
    };
    for line in cgroup_text.lines() {
        let mut parts = line.splitn(3, ':');
        let Some(hierarchy_id) = parts.next() else {
            continue;
        };
        let Some(controllers) = parts.next() else {
            continue;
        };
        let Some(relative_path) = parts.next() else {
            continue;
        };
        if hierarchy_id == "0" && controllers.is_empty() {
            return cgroup_root.join(relative_path.trim_start_matches('/'));
        }
    }
    cgroup_root.to_path_buf()
}

fn read_cgroup_u64(cgroup_dir: &Path, file_name: &str) -> Option<u64> {
    let file_text = fs::read_to_string(cgroup_dir.join(file_name)).ok()?;
    let trimmed = file_text.trim();
    if trimmed == "max" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CgroupMemoryStat {
    anon: Option<u64>,
    file: Option<u64>,
    inactive_file: Option<u64>,
    active_file: Option<u64>,
    kernel: Option<u64>,
    slab: Option<u64>,
}

fn read_cgroup_memory_stat(cgroup_dir: &Path) -> CgroupMemoryStat {
    let Ok(stat_text) = fs::read_to_string(cgroup_dir.join("memory.stat")) else {
        return CgroupMemoryStat::default();
    };
    let mut stat = CgroupMemoryStat::default();
    for line in stat_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(sample_bytes) = parts
            .next()
            .and_then(|sample_text| sample_text.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "anon" => stat.anon = Some(sample_bytes),
            "file" => stat.file = Some(sample_bytes),
            "inactive_file" => stat.inactive_file = Some(sample_bytes),
            "active_file" => stat.active_file = Some(sample_bytes),
            "kernel" => stat.kernel = Some(sample_bytes),
            "slab" => stat.slab = Some(sample_bytes),
            _ => {}
        }
    }
    stat
}

fn process_rss_bytes(proc_status: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(status_text) = fs::read_to_string(proc_status) else {
        return (None, None);
    };
    let mut rss = None;
    let mut rss_anon = None;
    for line in status_text.lines() {
        if rss.is_none() {
            rss = parse_status_kib_line(line, "VmRSS:");
        }
        if rss_anon.is_none() {
            rss_anon = parse_status_kib_line(line, "RssAnon:");
        }
    }
    (rss, rss_anon)
}

fn parse_status_kib_line(line: &str, key: &str) -> Option<u64> {
    let kib_text = line.strip_prefix(key)?.split_whitespace().next()?;
    kib_text.parse::<u64>().ok()?.checked_mul(1024)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; runtime memory values are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn samples_nested_cgroup_v2_memory_files() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let cgroup_root = tempdir.path().join("sys-fs-cgroup");
        let service_dir = cgroup_root.join("docker/abc");
        fs::create_dir_all(&service_dir)?;
        fs::write(tempdir.path().join("cgroup"), "0::/docker/abc\n")?;
        fs::write(service_dir.join("memory.current"), "900\n")?;
        fs::write(service_dir.join("memory.max"), "1000\n")?;
        fs::write(service_dir.join("memory.high"), "max\n")?;
        fs::write(service_dir.join("memory.swap.current"), "10\n")?;
        fs::write(
            service_dir.join("memory.stat"),
            "anon 400\nfile 500\ninactive_file 300\nactive_file 200\nkernel 50\nslab 20\n",
        )?;
        fs::write(
            tempdir.path().join("status"),
            "Name:\tzinder\nVmRSS:\t42 kB\nRssAnon:\t40 kB\n",
        )?;

        let snapshot = RuntimeMemorySnapshot::sample_from_paths(
            &cgroup_root,
            &tempdir.path().join("cgroup"),
            &tempdir.path().join("status"),
        );

        assert_eq!(snapshot.cgroup_current_bytes, Some(900));
        assert_eq!(snapshot.cgroup_max_bytes, Some(1000));
        assert_eq!(snapshot.cgroup_high_bytes, None);
        assert_eq!(snapshot.cgroup_swap_current_bytes, Some(10));
        assert_eq!(snapshot.cgroup_anon_bytes, Some(400));
        assert_eq!(snapshot.cgroup_file_bytes, Some(500));
        assert_eq!(snapshot.cgroup_inactive_file_bytes, Some(300));
        assert_eq!(snapshot.cgroup_active_file_bytes, Some(200));
        assert_eq!(snapshot.cgroup_kernel_bytes, Some(50));
        assert_eq!(snapshot.cgroup_slab_bytes, Some(20));
        assert_eq!(snapshot.working_set_bytes(), Some(600));
        assert_eq!(snapshot.process_rss_bytes, Some(42 * 1024));
        assert_eq!(snapshot.process_rss_anon_bytes, Some(40 * 1024));
        assert_eq!(snapshot.current_pressure_ratio(), Some(0.9));
        assert_eq!(snapshot.pressure_ratio(), Some(0.6));

        Ok(())
    }

    #[test]
    fn pressure_ratio_prefers_cgroup_high_when_set() {
        let snapshot = RuntimeMemorySnapshot {
            cgroup_current_bytes: Some(900),
            cgroup_max_bytes: Some(2000),
            cgroup_high_bytes: Some(1000),
            cgroup_swap_current_bytes: None,
            cgroup_anon_bytes: None,
            cgroup_file_bytes: None,
            cgroup_inactive_file_bytes: None,
            cgroup_active_file_bytes: None,
            cgroup_kernel_bytes: None,
            cgroup_slab_bytes: None,
            process_rss_bytes: None,
            process_rss_anon_bytes: None,
        };

        assert_eq!(snapshot.pressure_ratio(), Some(0.9));
    }

    #[test]
    fn pressure_ratio_ignores_inactive_file_cache_when_available() {
        let snapshot = RuntimeMemorySnapshot {
            cgroup_current_bytes: Some(900),
            cgroup_max_bytes: Some(1000),
            cgroup_high_bytes: None,
            cgroup_swap_current_bytes: None,
            cgroup_anon_bytes: Some(200),
            cgroup_file_bytes: Some(700),
            cgroup_inactive_file_bytes: Some(500),
            cgroup_active_file_bytes: Some(200),
            cgroup_kernel_bytes: None,
            cgroup_slab_bytes: None,
            process_rss_bytes: None,
            process_rss_anon_bytes: None,
        };

        assert_eq!(snapshot.working_set_bytes(), Some(400));
        assert_eq!(snapshot.current_pressure_ratio(), Some(0.9));
        assert_eq!(snapshot.pressure_ratio(), Some(0.4));
    }

    #[test]
    fn container_budget_prefers_high_over_max() {
        let snapshot = RuntimeMemorySnapshot {
            cgroup_max_bytes: Some(24 * 1024 * 1024 * 1024),
            cgroup_high_bytes: Some(20 * 1024 * 1024 * 1024),
            ..RuntimeMemorySnapshot::default()
        };
        assert_eq!(
            container_memory_budget_from_snapshot(snapshot),
            Some(20 * 1024 * 1024 * 1024),
            "memory.high is the soft throttle Zinder should stay under"
        );
    }

    #[test]
    fn container_budget_falls_back_to_max_when_high_unset() {
        let snapshot = RuntimeMemorySnapshot {
            cgroup_max_bytes: Some(24 * 1024 * 1024 * 1024),
            cgroup_high_bytes: None,
            ..RuntimeMemorySnapshot::default()
        };
        assert_eq!(
            container_memory_budget_from_snapshot(snapshot),
            Some(24 * 1024 * 1024 * 1024),
        );
    }

    #[test]
    fn container_budget_is_none_when_cgroup_absent() {
        let snapshot = RuntimeMemorySnapshot::default();
        assert_eq!(container_memory_budget_from_snapshot(snapshot), None);
    }

    #[test]
    fn container_budget_treats_zero_limit_as_absent() {
        let snapshot = RuntimeMemorySnapshot {
            cgroup_max_bytes: Some(0),
            cgroup_high_bytes: None,
            ..RuntimeMemorySnapshot::default()
        };
        assert_eq!(container_memory_budget_from_snapshot(snapshot), None);
    }
}
