//! Shared per-RPC duration and outcome metric helpers.
//!
//! Every Zinder service that serves gRPC (`zinder-query`, `zinder-explorer`,
//! `zinder-ingest`, `zinder-query`) emits the same metric shape
//! per request: a duration histogram and a total counter, each labelled with
//! `operation`, `status` (`ok|error`), and `error_class`. The two metric
//! names share a service-specific prefix (`zinder_explorer_request_*`,
//! `zinder_query_request_*`, ...). Service binaries call
//! [`describe_rpc_metrics`] once at startup and [`record_rpc_request`] from
//! every handler.
//!
//! The helper is service-agnostic: callers map their own error type to a
//! short `error_class` label, since each plane has its own vocabulary
//! (tonic [`Code`](tonic::Code) names for explorer-style proxy services,
//! typed domain-error variants for `zinder-query`'s `WalletQueryApi` reads).

use std::time::Duration;

/// Outcome of a single RPC invocation.
///
/// Constructed by handlers and passed to [`record_rpc_request`]. Keep this
/// small (no error sources, no allocations) so emitting metrics never
/// allocates on the hot path.
#[derive(Clone, Copy, Debug)]
pub enum RpcOutcome {
    /// Handler returned a success response.
    Ok,
    /// Handler returned an error classified by the short label `class`.
    ///
    /// `class` must be the `snake_case` label the service's dashboards filter
    /// on (for example `"invalid_argument"`, `"artifact_unavailable"`).
    /// Callers compute it from their own domain error or from a
    /// [`tonic::Status`] via the service's `error_class` mapper.
    Error {
        /// Short `snake_case` classifier for the error.
        class: &'static str,
    },
}

impl RpcOutcome {
    /// Returns the short status label (`ok` or `error`) for this outcome.
    #[must_use]
    pub const fn status_label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error { .. } => "error",
        }
    }

    /// Returns the short error class label (`none` when the call succeeded).
    #[must_use]
    pub const fn error_class_label(self) -> &'static str {
        match self {
            Self::Ok => "none",
            Self::Error { class } => class,
        }
    }
}

/// Names the per-service metric pair emitted by [`record_rpc_request`].
///
/// Held as a single value rather than two strings so callers cannot mix
/// metric names from different services. Construct once per binary and
/// reuse across every handler.
#[derive(Clone, Copy, Debug)]
pub struct RpcMetricNames {
    duration_seconds: &'static str,
    request_total: &'static str,
}

impl RpcMetricNames {
    /// Builds the metric pair for the named service.
    ///
    /// Pass the canonical prefix (for example `"zinder_explorer"` or
    /// `"zinder_query"`); the helper appends `_request_duration_seconds`
    /// and `_request_total` to produce the two metric names. The prefix is
    /// fixed at construction time so the metric registry deduplicates
    /// consistently.
    ///
    /// Currently the function is generic over the prefix to keep the API
    /// service-agnostic. The metric registry hashes by name, so the same
    /// prefix passed from multiple call sites resolves to the same metric.
    #[must_use]
    pub const fn for_service(duration_seconds: &'static str, request_total: &'static str) -> Self {
        Self {
            duration_seconds,
            request_total,
        }
    }

    /// Returns the histogram metric name (`{prefix}_request_duration_seconds`).
    #[must_use]
    pub const fn duration_seconds(self) -> &'static str {
        self.duration_seconds
    }

    /// Returns the counter metric name (`{prefix}_request_total`).
    #[must_use]
    pub const fn request_total(self) -> &'static str {
        self.request_total
    }
}

/// Registers `# HELP` and `# TYPE` text for a service's RPC metric pair.
///
/// Call once per service at startup, after
/// [`crate::install_metrics_recorder`] returns and before the gRPC server
/// records its first request. `service_label` is the human-readable name of
/// the RPC surface (`"ExplorerQuery"`, `"WalletQuery"`) used in the metric
/// description text.
pub fn describe_rpc_metrics(metric_names: RpcMetricNames, service_label: &str) {
    metrics::describe_histogram!(
        metric_names.duration_seconds(),
        metrics::Unit::Seconds,
        format!(
            "Wall-clock duration of {service_label} RPCs handled by this service. \
             Labels: operation (`snake_case` RPC method name), status (ok|error), \
             error_class (`snake_case` error label; 'none' on ok)."
        )
    );
    metrics::describe_counter!(
        metric_names.request_total(),
        metrics::Unit::Count,
        format!(
            "Total {service_label} RPC outcomes observed by this service. Same labels \
             as the duration histogram; sum across operations gives total throughput."
        )
    );
}

/// Records one per-RPC duration sample and one total-counter increment.
///
/// `operation` is the `snake_case` RPC method name shared with the
/// service's dashboards. `elapsed` is the wall-clock duration of the
/// handler. `outcome` carries the status and error-class labels.
pub fn record_rpc_request(
    metric_names: RpcMetricNames,
    operation: &'static str,
    elapsed: Duration,
    outcome: RpcOutcome,
) {
    let status = outcome.status_label();
    let error_class = outcome.error_class_label();
    metrics::histogram!(
        metric_names.duration_seconds(),
        "operation" => operation,
        "status" => status,
        "error_class" => error_class
    )
    .record(elapsed);
    metrics::counter!(
        metric_names.request_total(),
        "operation" => operation,
        "status" => status,
        "error_class" => error_class
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_outcome_labels_match_convention() {
        assert_eq!(RpcOutcome::Ok.status_label(), "ok");
        assert_eq!(RpcOutcome::Ok.error_class_label(), "none");
    }

    #[test]
    fn error_outcome_propagates_class_label() {
        let outcome = RpcOutcome::Error {
            class: "invalid_argument",
        };
        assert_eq!(outcome.status_label(), "error");
        assert_eq!(outcome.error_class_label(), "invalid_argument");
    }

    #[test]
    fn metric_names_expose_prefixed_pair() {
        let names = RpcMetricNames::for_service(
            "zinder_explorer_request_duration_seconds",
            "zinder_explorer_request_total",
        );
        assert_eq!(
            names.duration_seconds(),
            "zinder_explorer_request_duration_seconds"
        );
        assert_eq!(names.request_total(), "zinder_explorer_request_total");
    }
}
