//! Process-wide Prometheus recorder used to scrape in-process metrics.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use parking_lot::Mutex;

use crate::error::BenchError;

static RECORDER_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Installs the process-wide Prometheus recorder and returns a render handle.
///
/// The `metrics` facade allows exactly one global recorder, so repeated calls
/// return the handle installed on the first call. Rendering the handle after a
/// replay yields the exposition text the report parses.
pub fn install_recorder() -> Result<PrometheusHandle, BenchError> {
    if let Some(handle) = RECORDER_HANDLE.get() {
        return Ok(handle.clone());
    }
    let install_guard = INSTALL_LOCK.lock();
    if let Some(handle) = RECORDER_HANDLE.get() {
        drop(install_guard);
        return Ok(handle.clone());
    }
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|source| BenchError::Recorder {
            reason: source.to_string(),
        })?;
    let _ = RECORDER_HANDLE.set(handle.clone());
    drop(install_guard);
    Ok(handle)
}
