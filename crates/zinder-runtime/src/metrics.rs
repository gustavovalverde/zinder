//! Prometheus metrics recorder used by Zinder operational endpoints.

use std::sync::{Mutex, OnceLock};

use metrics::{Unit, describe_gauge};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use thiserror::Error;

use crate::OpsServer;

static PROMETHEUS_HANDLE: OnceLock<MetricsHandle> = OnceLock::new();
static PROMETHEUS_INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Histogram bucket boundaries (in seconds) applied to every metric whose name
/// ends in `_duration_seconds`.
///
/// The 3x-spaced sequence covers 1µs to 10s with enough resolution for
/// `histogram_quantile()` to compute meaningful p50/p95/p99 across the full
/// observed range: store reads (~10µs), node RPCs (~1ms), commit pipeline
/// (~100ms), backups (seconds). Reconfiguring after data has been recorded
/// invalidates pre-existing series, so changes here are sticky.
const DURATION_SECONDS_BUCKETS: &[f64] = &[
    1e-6, 3e-6, 1e-5, 3e-5, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2, 3e-2, 1e-1, 3e-1, 1.0, 3.0, 10.0,
];

/// Handle used by `/metrics` to render the process-wide Prometheus snapshot.
#[derive(Clone, Debug)]
pub struct MetricsHandle {
    prometheus: PrometheusHandle,
}

impl MetricsHandle {
    /// Renders the current metric snapshot in Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        self.prometheus.render()
    }
}

/// Error returned when the process-wide metrics recorder cannot be installed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MetricsInstallError {
    /// Failed to install the Prometheus recorder as the global metrics recorder.
    #[error("failed to install metrics recorder: {source}")]
    Install {
        /// Underlying recorder installation error.
        #[source]
        source: metrics_exporter_prometheus::BuildError,
    },
}

/// Installs the global Prometheus recorder and records this service's build
/// identity.
///
/// The recorder is process-wide because the `metrics` facade has one global
/// recorder. Calling this more than once through Zinder runtime returns the
/// already-installed handle and records another `zinder_build_info` label set.
pub fn install_metrics_recorder(server: &OpsServer) -> Result<MetricsHandle, MetricsInstallError> {
    let handle = if let Some(handle) = PROMETHEUS_HANDLE.get() {
        handle.clone()
    } else {
        let _install_guard = match PROMETHEUS_INSTALL_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(handle) = PROMETHEUS_HANDLE.get() {
            return Ok(record_build_info(handle.clone(), server));
        }
        let prometheus = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Suffix("_duration_seconds".to_owned()),
                DURATION_SECONDS_BUCKETS,
            )
            .map_err(|source| MetricsInstallError::Install { source })?
            .install_recorder()
            .map_err(|source| MetricsInstallError::Install { source })?;
        let installed = MetricsHandle { prometheus };
        let _ = PROMETHEUS_HANDLE.set(installed.clone());
        installed
    };

    Ok(record_build_info(handle, server))
}

fn record_build_info(handle: MetricsHandle, server: &OpsServer) -> MetricsHandle {
    describe_gauge!(
        "zinder_build_info",
        Unit::Count,
        "Build-time identity for this Zinder service."
    );
    metrics::gauge!(
        "zinder_build_info",
        "service" => server.service_name,
        "version" => server.service_version,
        "network" => server.network_name
    )
    .set(1.0);

    handle
}
