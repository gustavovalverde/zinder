//! Operational HTTP endpoints (`/healthz`, `/readyz`, `/metrics`) shared by services.

use std::net::SocketAddr;

use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use thiserror::Error;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{MetricsHandle, MetricsInstallError, Readiness, install_metrics_recorder};

/// Service identity surfaced by `/metrics` for build-time labeling.
#[derive(Clone, Debug)]
pub struct OpsServer {
    /// Service name (`zinder-ingest`, `zinder-query`, `zinder-compat-lightwalletd`, ...).
    pub service_name: &'static str,
    /// Service version, typically `env!("CARGO_PKG_VERSION")`.
    pub service_version: &'static str,
    /// Network this binary is operating on.
    pub network_name: &'static str,
}

/// Error returned by the operational HTTP server.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpsServerError {
    /// Failed to bind the operational listen address.
    #[error("failed to bind operational endpoint at {listen_addr}: {source}")]
    Bind {
        /// Address that failed to bind.
        listen_addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Underlying axum/hyper transport failed.
    #[error("operational endpoint server failed: {source}")]
    Transport {
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to install the metrics recorder used by `/metrics`.
    #[error(transparent)]
    Metrics(#[from] MetricsInstallError),
}

/// Handle for a spawned operational HTTP server.
///
/// Cancel and await the spawned task with [`OpsEndpointHandle::shutdown`].
pub struct OpsEndpointHandle {
    cancel: CancellationToken,
    join: JoinHandle<Result<(), OpsServerError>>,
}

impl OpsEndpointHandle {
    /// Cancels the operational server and awaits its task to completion.
    ///
    /// Errors from the task are logged at warn level rather than propagated,
    /// because shutdown is not the right time to surface late transport
    /// failures to the binary's main result.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        match self.join.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                target: "zinder::runtime",
                event = "ops_endpoint_error",
                error = %error,
                "operational endpoint exited with error"
            ),
            Err(join_error) => tracing::warn!(
                target: "zinder::runtime",
                event = "ops_endpoint_panic",
                error = %join_error,
                "operational endpoint task failed"
            ),
        }
    }
}

/// Spawns the operational HTTP server on a tokio task and returns a handle
/// that owns the cancellation token and join handle.
///
/// Use this from binary `main` paths; for tests that need direct access to
/// the future, call [`serve_ops_endpoint`] instead.
#[must_use = "the returned handle owns the spawned task; drop only on graceful shutdown"]
pub fn spawn_ops_endpoint(
    listen_addr: SocketAddr,
    server: OpsServer,
    readiness: Readiness,
) -> OpsEndpointHandle {
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let metrics = match install_metrics_recorder(&server) {
        Ok(metrics) => metrics,
        Err(error) => {
            let join = tokio::spawn(async move { Err(OpsServerError::Metrics(error)) });
            return OpsEndpointHandle { cancel, join };
        }
    };
    let join = tokio::spawn(async move {
        serve_ops_endpoint_with_metrics(listen_addr, server, readiness, cancel_for_task, metrics)
            .await
    });

    OpsEndpointHandle { cancel, join }
}

/// Serves `/healthz`, `/readyz`, and `/metrics` until `cancel` fires.
///
/// `readiness` is shared with the runtime so updates are visible to HTTP
/// handlers without copying.
pub async fn serve_ops_endpoint(
    listen_addr: SocketAddr,
    server: OpsServer,
    readiness: Readiness,
    cancel: CancellationToken,
) -> Result<(), OpsServerError> {
    let metrics = install_metrics_recorder(&server)?;
    serve_ops_endpoint_with_metrics(listen_addr, server, readiness, cancel, metrics).await
}

async fn serve_ops_endpoint_with_metrics(
    listen_addr: SocketAddr,
    server: OpsServer,
    readiness: Readiness,
    cancel: CancellationToken,
    metrics: MetricsHandle,
) -> Result<(), OpsServerError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|source| OpsServerError::Bind {
            listen_addr,
            source,
        })?;
    let app = build_router(&server, readiness, metrics);

    tracing::info!(
        target: "zinder::runtime",
        event = "ops_endpoint_started",
        listen_addr = %listen_addr,
        "operational endpoint started"
    );

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await;

    tracing::info!(
        target: "zinder::runtime",
        event = "ops_endpoint_stopped",
        "operational endpoint stopped"
    );

    serve_result.map_err(|source| OpsServerError::Transport { source })
}

fn build_router(server: &OpsServer, readiness: Readiness, metrics: MetricsHandle) -> Router {
    let metrics_state = MetricsState {
        service_name: server.service_name,
        service_version: server.service_version,
        network_name: server.network_name,
        readiness: readiness.clone(),
        metrics,
    };

    Router::new()
        .route("/healthz", get(healthz_handler))
        .route(
            "/readyz",
            get(move || {
                let readiness = readiness.clone();
                async move { readyz_handler(&readiness) }
            }),
        )
        .route(
            "/metrics",
            get(move || {
                let metrics_state = metrics_state.clone();
                async move { metrics_handler(&metrics_state) }
            }),
        )
}

#[derive(Clone)]
struct MetricsState {
    service_name: &'static str,
    service_version: &'static str,
    network_name: &'static str,
    readiness: Readiness,
    metrics: MetricsHandle,
}

const READINESS_CAUSE_LABELS: [&str; 12] = [
    "starting",
    "syncing",
    "ready",
    "node_unavailable",
    "node_capability_missing",
    "storage_unavailable",
    "schema_mismatch",
    "reorg_window_exceeded",
    "replica_lagging",
    "writer_status_unavailable",
    "cursor_at_risk",
    "shutting_down",
];

async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "alive"})))
}

#[derive(Serialize)]
struct ReadinessResponseBody {
    status: &'static str,
    cause: crate::ReadinessCause,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_height: Option<u32>,
}

fn readyz_handler(readiness: &Readiness) -> (StatusCode, Json<ReadinessResponseBody>) {
    let report = readiness.report();
    let status_code = if report.is_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = ReadinessResponseBody {
        status: if report.is_ready {
            "ready"
        } else {
            "not_ready"
        },
        cause: report.cause,
        current_height: report.current_height,
        target_height: report.target_height,
    };

    (status_code, Json(body))
}

fn metrics_handler(state: &MetricsState) -> (StatusCode, String) {
    metrics::gauge!(
        "zinder_build_info",
        "service" => state.service_name,
        "version" => state.service_version,
        "network" => state.network_name
    )
    .set(1.0);
    record_readiness_metrics(state);
    (StatusCode::OK, state.metrics.render())
}

fn record_readiness_metrics(state: &MetricsState) {
    let report = state.readiness.report();
    let active_cause = report.cause.metric_label();
    for cause in READINESS_CAUSE_LABELS {
        metrics::gauge!(
            "zinder_readiness_state",
            "service" => state.service_name,
            "network" => state.network_name,
            "cause" => cause
        )
        .set(if cause == active_cause { 1.0 } else { 0.0 });
    }

    metrics::gauge!(
        "zinder_readiness_sync_lag_blocks",
        "service" => state.service_name,
        "network" => state.network_name
    )
    .set(readiness_sync_lag_blocks(report.cause));

    metrics::gauge!(
        "zinder_readiness_replica_lag_chain_epochs",
        "service" => state.service_name,
        "network" => state.network_name
    )
    .set(readiness_replica_lag_chain_epochs(report.cause));
}

fn readiness_sync_lag_blocks(cause: crate::ReadinessCause) -> f64 {
    match cause {
        crate::ReadinessCause::Syncing {
            lag_blocks: Some(lag_blocks),
        } => u64_to_f64(lag_blocks),
        crate::ReadinessCause::Starting
        | crate::ReadinessCause::Syncing { lag_blocks: None }
        | crate::ReadinessCause::Ready
        | crate::ReadinessCause::NodeUnavailable
        | crate::ReadinessCause::NodeCapabilityMissing { .. }
        | crate::ReadinessCause::StorageUnavailable
        | crate::ReadinessCause::SchemaMismatch
        | crate::ReadinessCause::ReorgWindowExceeded { .. }
        | crate::ReadinessCause::ReplicaLagging { .. }
        | crate::ReadinessCause::WriterStatusUnavailable
        | crate::ReadinessCause::CursorAtRisk { .. }
        | crate::ReadinessCause::ShuttingDown => 0.0,
    }
}

fn readiness_replica_lag_chain_epochs(cause: crate::ReadinessCause) -> f64 {
    match cause {
        crate::ReadinessCause::ReplicaLagging { lag_chain_epochs } => u64_to_f64(lag_chain_epochs),
        crate::ReadinessCause::Starting
        | crate::ReadinessCause::Syncing { .. }
        | crate::ReadinessCause::Ready
        | crate::ReadinessCause::NodeUnavailable
        | crate::ReadinessCause::NodeCapabilityMissing { .. }
        | crate::ReadinessCause::StorageUnavailable
        | crate::ReadinessCause::SchemaMismatch
        | crate::ReadinessCause::ReorgWindowExceeded { .. }
        | crate::ReadinessCause::WriterStatusUnavailable
        | crate::ReadinessCause::CursorAtRisk { .. }
        | crate::ReadinessCause::ShuttingDown => 0.0,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; readiness lag is diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}
