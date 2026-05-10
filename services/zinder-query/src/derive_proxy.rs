//! Federation primitive for proxying typed `WalletQuery` RPCs to derive-plane
//! consumers (`zinder-derive`).
//!
//! [`DeriveProxy`] is the federation entry point for any `WalletQuery.*`
//! method that delegates to a derive consumer. It owns the four concerns
//! each federation body would otherwise duplicate:
//!
//! - Endpoint and bearer-token configuration for the underlying gRPC channel.
//! - Lazy connection construction (one tonic channel per call today;
//!   pool-friendly later because the same `connect_authenticated_channel` is
//!   reused).
//! - A shared [`DeriveReadinessGauge`] that a background probe loop updates
//!   from the consumer's `ServerInfo` response.
//! - Capability gating: [`DeriveProxy::is_ready`] reports whether the
//!   consumer's readiness capability is currently advertised, so
//!   [`crate::WalletQueryGrpcAdapter`] can suppress the federated method's
//!   capability string when the proxy is unconfigured or unhealthy.
//!
//! Federated methods advertise their capability under
//! `derive.{consumer}.{capability}_v{N}` rather than `wallet.*`: the
//! namespace reflects data ownership, not RPC location. Capability
//! advertisement is gated on [`DeriveProxy::is_ready`]; `ServerInfo` does
//! not advertise the capability when the proxy is unconfigured.
//!
//! Each derive consumer constructs its own `DeriveProxy<Client>`
//! parameterized over the consumer's generated tonic client. The supplied
//! `construct_client` function pointer turns an [`AuthenticatedChannel`]
//! into the typed client; the rest of the federation logic is shared.
//! A binary running multiple derive consumers spawns one probe loop per
//! proxy.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{task::JoinHandle, time};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use zinder_runtime::{AuthenticatedChannel, BearerToken, connect_authenticated_channel};

/// Minimum interval between consecutive readiness probes.
///
/// Probing more often than this wastes RPC budget on a value that changes on
/// the order of seconds. Operators tune this through `derive_probe_interval`.
pub const MIN_DERIVE_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Default cadence between readiness probes when no operator override is set.
pub const DEFAULT_DERIVE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Configuration carried by every [`DeriveProxy`].
#[derive(Clone, Debug)]
pub struct DeriveProxyConfig {
    /// gRPC endpoint of the derive consumer (e.g. `http://127.0.0.1:9068`).
    pub endpoint: String,
    /// Optional shared-secret bearer token; passed through to
    /// [`connect_authenticated_channel`].
    pub bearer_token: Option<BearerToken>,
    /// Readiness capability advertised by the consumer's `ServerInfo`. The
    /// proxy reports `is_ready` only when its readiness gauge has observed
    /// this string in the consumer's most recent capability response.
    ///
    /// For example: `"derive.explorer.transparent_balance_v1"`.
    pub capability: &'static str,
}

/// Atomic flag that the readiness probe loop updates and adapter handlers
/// read. Cheap to clone; designed to be shared via `Arc`.
#[derive(Clone, Debug, Default)]
pub struct DeriveReadinessGauge {
    inner: Arc<DeriveReadinessGaugeInner>,
}

impl DeriveReadinessGauge {
    /// Creates a new readiness gauge initialized to "not ready".
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports the current readiness state.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.inner.is_ready.load(Ordering::Acquire)
    }

    /// Marks the underlying derive consumer as ready.
    pub fn mark_ready(&self) {
        self.inner.is_ready.store(true, Ordering::Release);
    }

    /// Marks the underlying derive consumer as not ready.
    pub fn mark_not_ready(&self) {
        self.inner.is_ready.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default)]
struct DeriveReadinessGaugeInner {
    is_ready: AtomicBool,
}

/// Federation proxy that forwards a typed `WalletQuery` RPC to a derive-plane
/// consumer.
///
/// Generic over `Client` so each derive consumer supplies its own generated
/// tonic client type. The shared [`forward`](Self::forward) helper opens an
/// authenticated channel, hands it to the caller-supplied closure, and maps
/// connection failures into a `Status::unavailable` carrying
/// `derive_unavailable` semantics.
#[derive(Clone, Debug)]
pub struct DeriveProxy<Client> {
    config: DeriveProxyConfig,
    readiness: DeriveReadinessGauge,
    construct_client: fn(AuthenticatedChannel) -> Client,
}

impl<Client> DeriveProxy<Client>
where
    Client: Send,
{
    /// Creates a derive proxy with a fresh readiness gauge.
    ///
    /// `construct_client` is the generated tonic constructor for the derive
    /// consumer's client (e.g. `ExplorerQueryClient::new`).
    #[must_use]
    pub fn new(
        config: DeriveProxyConfig,
        construct_client: fn(AuthenticatedChannel) -> Client,
    ) -> Self {
        Self::with_readiness(config, DeriveReadinessGauge::new(), construct_client)
    }

    /// Creates a derive proxy that shares an existing readiness gauge.
    ///
    /// Used when the binary spawns one readiness probe loop and threads its
    /// gauge through to multiple call sites.
    #[must_use]
    pub fn with_readiness(
        config: DeriveProxyConfig,
        readiness: DeriveReadinessGauge,
        construct_client: fn(AuthenticatedChannel) -> Client,
    ) -> Self {
        Self {
            config,
            readiness,
            construct_client,
        }
    }

    /// Returns the readiness gauge handed to background probe loops.
    #[must_use]
    pub fn readiness(&self) -> DeriveReadinessGauge {
        self.readiness.clone()
    }

    /// Returns the capability string the consumer advertises when ready.
    #[must_use]
    pub const fn capability(&self) -> &'static str {
        self.config.capability
    }

    /// Returns whether the derive consumer is currently advertising its
    /// readiness capability.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness.is_ready()
    }

    /// Forwards `request` to the derive consumer through `invoke_remote`,
    /// mapping channel-connection failures into `Status::unavailable`.
    ///
    /// `invoke_remote` receives a freshly-constructed client and the original
    /// request envelope, then performs whatever RPC the federated method
    /// needs. Any [`Status`] returned by the closure flows through to the
    /// public caller verbatim, preserving structured error details emitted
    /// by the derive consumer.
    pub async fn forward<Req, Resp, Invoke, Fut>(
        &self,
        request: Request<Req>,
        invoke_remote: Invoke,
    ) -> Result<Response<Resp>, Status>
    where
        Invoke: FnOnce(Client, Request<Req>) -> Fut + Send,
        Fut: Future<Output = Result<Response<Resp>, Status>> + Send,
    {
        if !self.is_ready() {
            return Err(derive_unavailable_status(self.config.capability));
        }
        let channel =
            connect_authenticated_channel(&self.config.endpoint, self.config.bearer_token.as_ref())
                .await
                .map_err(|error| {
                    Status::unavailable(format!(
                        "derive consumer at {} unreachable: {error}",
                        self.config.endpoint
                    ))
                })?;
        let client = (self.construct_client)(channel);
        invoke_remote(client, request).await
    }
}

fn derive_unavailable_status(capability: &str) -> Status {
    Status::unavailable(format!("derive consumer for {capability} is not ready",))
}

/// Configuration for [`spawn_derive_readiness_probe`].
#[derive(Clone, Copy, Debug)]
pub struct DeriveReadinessProbeConfig {
    /// Cadence between consecutive readiness probes. Clamped to
    /// [`MIN_DERIVE_PROBE_INTERVAL`].
    pub probe_interval: Duration,
}

impl Default for DeriveReadinessProbeConfig {
    fn default() -> Self {
        Self {
            probe_interval: DEFAULT_DERIVE_PROBE_INTERVAL,
        }
    }
}

/// Spawns a background task that probes the derive consumer's readiness
/// capability and updates the supplied [`DeriveReadinessGauge`].
///
/// `probe_capability` is invoked on every tick; it returns `true` when the
/// consumer's most recent `ServerInfo` response advertises the proxy's
/// readiness capability and `false` when it does not (or the probe failed).
///
/// The closure is decoupled from the underlying gRPC client so derive
/// consumers can supply their own generated `*QueryClient::server_info` call
/// without forcing this module to import every consumer's proto.
///
/// The returned [`JoinHandle`] resolves when `cancel` is triggered.
pub fn spawn_derive_readiness_probe<Probe, Fut>(
    gauge: DeriveReadinessGauge,
    probe_capability: Probe,
    config: DeriveReadinessProbeConfig,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    Probe: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send,
{
    let interval = config.probe_interval.max(MIN_DERIVE_PROBE_INTERVAL);
    tokio::spawn(async move {
        loop {
            let ready = probe_capability().await;
            if ready {
                gauge.mark_ready();
            } else {
                gauge.mark_not_ready();
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = time::sleep(interval) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[tokio::test]
    async fn forward_returns_unavailable_when_proxy_not_ready() -> Result<(), &'static str> {
        let proxy: DeriveProxy<()> = DeriveProxy::new(
            DeriveProxyConfig {
                endpoint: "http://127.0.0.1:0".into(),
                bearer_token: None,
                capability: "derive.test.cap_v1",
            },
            |_| (),
        );

        let outcome = proxy
            .forward(Request::new(()), |_client, _request| async move {
                Ok::<Response<()>, Status>(Response::new(()))
            })
            .await;

        let Err(status) = outcome else {
            return Err("not-ready proxy must surface unavailable");
        };
        if status.code() != tonic::Code::Unavailable {
            return Err("expected unavailable status code");
        }
        if !status.message().contains("derive.test.cap_v1") {
            return Err("status message must reference the capability");
        }
        Ok(())
    }

    #[test]
    fn readiness_gauge_round_trips_through_mark_methods() {
        let gauge = DeriveReadinessGauge::new();
        assert!(!gauge.is_ready());
        gauge.mark_ready();
        assert!(gauge.is_ready());
        gauge.mark_not_ready();
        assert!(!gauge.is_ready());
    }

    #[tokio::test]
    async fn probe_loop_updates_gauge_on_each_tick() {
        let gauge = DeriveReadinessGauge::new();
        let cancel = CancellationToken::new();
        let probed = Arc::new(Mutex::new(0u32));
        let probed_clone = Arc::clone(&probed);

        let handle = spawn_derive_readiness_probe(
            gauge.clone(),
            move || {
                let probed = Arc::clone(&probed_clone);
                async move {
                    let mut count = probed.lock();
                    *count = count.saturating_add(1);
                    *count >= 2
                }
            },
            DeriveReadinessProbeConfig {
                probe_interval: MIN_DERIVE_PROBE_INTERVAL,
            },
            cancel.clone(),
        );

        // Wait long enough for two probe ticks (interval is 1s by clamp).
        tokio::time::sleep(Duration::from_millis(2_300)).await;
        cancel.cancel();
        let _ = handle.await;

        assert!(gauge.is_ready(), "second probe must mark gauge ready");
        let probe_count = *probed.lock();
        assert!(probe_count >= 2, "loop must run probe on each tick");
    }
}
