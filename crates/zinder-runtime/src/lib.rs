//! Operational HTTP surface and shared configuration loader for Zinder service binaries.
//!
//! This crate owns:
//!
//! - The typed [`Readiness`] state machine and [`ReadinessCause`]
//!   values defined in `docs/architecture/service-operations.md`.
//! - The `/healthz`, `/readyz`, and `/metrics` HTTP endpoints
//!   ([`spawn_ops_endpoint`], [`serve_ops_endpoint`]).
//! - The shared configuration error type, fluent layered loader, and shared
//!   schema/redacted-render mirrors ([`ConfigError`], [`ConfigLoader`],
//!   [`NetworkSection`], [`NetworkToml`], [`NodeAuthToml`], [`NodeToml`],
//!   [`zinder_environment_source`], [`require_field`], [`duration_as_millis_u64`])
//!   that every service binary uses to honor the
//!   `defaults -> file -> ZINDER_* env -> CLI overrides` precedence.
//! - Two thin lifecycle helpers used by every binary entry point:
//!   [`cancel_on_ctrl_c`] and [`install_tracing_subscriber`].
//! - The process-wide Prometheus metrics recorder
//!   ([`install_metrics_recorder`]).
//!
//! It deliberately exposes no domain types.

mod auth;
mod config;
mod env_var_docs;
mod metrics;
mod ops_endpoint;
mod readiness;
mod startup_phase;

pub use auth::{
    AuthenticatedChannel, BearerToken, BearerTokenClientInterceptor, BearerTokenConnectError,
    BearerTokenError, BearerTokenServerInterceptor, connect_authenticated_channel,
};
pub use config::{
    ConfigError, ConfigLoader, NetworkSection, NetworkToml, NodeAuthToml, NodeToml,
    duration_as_millis_u64, require_field, zinder_environment_source,
};
pub use env_var_docs::{
    ENVIRONMENT_VARIABLES, EnvVarDoc, Requirement as EnvVarRequirement,
    render_environment_variable_table,
};
pub use metrics::{MetricsHandle, MetricsInstallError, install_metrics_recorder};
pub use ops_endpoint::{
    OpsEndpointHandle, OpsServer, OpsServerError, serve_ops_endpoint, spawn_ops_endpoint,
};
pub use readiness::{Readiness, ReadinessCause, ReadinessReport, ReadinessState};
pub use startup_phase::{StartupPhase, StartupPhaseGuard};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Spawns a task that cancels `cancel` when the process receives `SIGINT`
/// (Ctrl-C). Returns the join handle for the spawned task; callers usually
/// drop it.
///
/// Used by every Zinder binary so the same shutdown semantics apply
/// regardless of which service is running.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn cancel_on_ctrl_c(cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

/// Installs the standard Zinder tracing subscriber as the global default.
///
/// Reads `RUST_LOG` if present, defaults to `info`, writes to stderr, and
/// includes target labels. Idempotent in practice because tracing rejects
/// repeated `init()` calls; binaries should call it once at startup.
pub fn install_tracing_subscriber() {
    use tracing_subscriber::{EnvFilter, fmt};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();
}
