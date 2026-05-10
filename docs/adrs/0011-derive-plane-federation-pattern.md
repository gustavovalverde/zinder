# ADR-0011: Derive-Plane Federation Pattern

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-10) |
| Product | Zinder |
| Domain | Cross-process query federation, derive-plane consumer integration |
| Related | [Wallet data plane](../architecture/wallet-data-plane.md), [Derive plane](../architecture/derive-plane.md), [Public interfaces](../architecture/public-interfaces.md), [ADR-0008](0008-consumer-neutral-wallet-data-plane.md), [ADR-0009](0009-ingest-control-transport-security.md), [ADR-0013](0013-derive-plane-instantiation-and-transparent-address-balance.md) |

## Context

`zinder-derive` is a separate process from `zinder-query` ([ADR-0008](0008-consumer-neutral-wallet-data-plane.md), [derive-plane.md](../architecture/derive-plane.md)). Federated `WalletQuery.*` methods proxy to the derive consumer's gRPC service rather than to a colocated handler.

Without an enforced shape for federation, three drifts are likely:

- **Copy-pasted proxy bodies.** Every federated method ships its own client construction, error mapping, and capability gating. Each copy is a slightly different shape; bug fixes in one body are missed in others.
- **Capability-namespace creep.** A federated method served on `WalletQuery` looks like a wallet capability. Without a hard rule, a contributor advertises it as `wallet.*`, breaking the architecture spine that says derive consumers own the `derive.*` namespace ([public-interfaces.md §Capability Discovery](../architecture/public-interfaces.md#capability-discovery)).
- **Probe ad-hocing.** Each federated method probes its consumer's readiness on its own timer, in its own format, with its own threshold. Operators tuning probe intervals must touch every consumer.

The mempool point-lookup proxy (`WalletQuery → IngestControl`) is structurally different: the writer-side handler is colocated with the live mempool, and the proxy body is a few lines because the bearer-token + connect helper is already shared. Each derive consumer has its own readiness lifecycle (a `ChainEvents` backfill that can take minutes for new accumulators), its own capability, and its own gRPC client type, none of which the chain plane has to model.

## Decision

`zinder-query` carries one canonical federation primitive — `DeriveProxy<Client>` — that owns the four concerns each derive consumer's federation body would otherwise duplicate.

### Federation primitive

```rust
// services/zinder-query/src/derive_proxy.rs
pub struct DeriveProxy<Client> {
    config: DeriveProxyConfig,
    readiness: DeriveReadinessGauge,
    construct_client: fn(AuthenticatedChannel) -> Client,
}

impl<Client: Send> DeriveProxy<Client> {
    pub async fn forward<Req, Resp, Invoke, Fut>(
        &self,
        request: Request<Req>,
        invoke_remote: Invoke,
    ) -> Result<Response<Resp>, Status>
    where
        Invoke: FnOnce(Client, Request<Req>) -> Fut + Send,
        Fut: Future<Output = Result<Response<Resp>, Status>> + Send,
    { /* connect, invoke, map errors */ }

    pub fn capability(&self) -> &'static str;
    pub fn is_ready(&self) -> bool;
    pub fn readiness(&self) -> DeriveReadinessGauge;
}
```

Each derive consumer's federation method on `WalletQueryGrpcAdapter` is then one closure invocation:

```rust
async fn transparent_address_balance(&self, request: Request<...>) -> Result<...> {
    self.explorer_proxy
        .as_ref()
        .ok_or_else(|| Status::unavailable("explorer proxy not configured"))?
        .forward(request, |mut client, req| async move {
            client.transparent_address_balance(req).await
        })
        .await
}
```

Adding new consumers means adding one `DeriveProxy<C>` field per consumer (parameterized over the consumer's generated client type) and one closure body per federated method. There is no `proxy_to_analytics`, `proxy_to_tax`, etc.; the federation primitive is generic.

### Capability namespace rule

Every federated `WalletQuery.*` method that calls `DeriveProxy::forward` advertises its capability under `derive.{consumer}.{capability}_v{N}`, never `wallet.*`. The federated consumer-facing RPC is on `WalletQuery` for consumer neutrality ([ADR-0008](0008-consumer-neutral-wallet-data-plane.md)) but the data lives in a derive consumer; the capability namespace must reflect data ownership, not RPC location.

A capability advertised by `DeriveProxy::capability` is included in `WalletQuery.ServerInfo` only when:

1. The proxy is configured (`Some(DeriveProxy<_>)` on the adapter), AND
2. The proxy's [`DeriveReadinessGauge`] reports `is_ready` (`true`).

When either condition is false, the capability is silently omitted; clients that gate features on capability strings will skip the federated method.

### Readiness gauge + probe loop

[`DeriveReadinessGauge`] is a cheap atomic-bool wrapper that the federation proxy reads on every `forward` call and the readiness probe writes on every probe tick. It is shared across the proxy and the probe via `Arc`, so probes update the value the proxy observes.

[`spawn_derive_readiness_probe`] is the canonical probe loop. It takes a closure that reports whether the consumer's most recent `ServerInfo` advertises the readiness capability and updates the gauge accordingly. The closure is supplied by the consumer's wiring code, not by `DeriveProxy`, because each generated `*QueryClient::server_info` method has a different return type and `DeriveProxy` does not bind a specific consumer's protobuf.

Probe cadence is operator-tunable through `derive_probe_interval` (default 5s, clamped to `MIN_DERIVE_PROBE_INTERVAL = 1s`).

## Consequences

### Operational

- The compat shim and any non-Rust client see the federated capability string only when the proxy probe has succeeded recently; deployments without the configured `zinder-derive` reachable do not advertise the federated method.
- Operators tuning probe intervals or readiness thresholds touch one config block per derive consumer, not one per federated method.
- A `Status::unavailable` returned by `DeriveProxy::forward` carries the proxy's capability string in the message, so error logs identify which consumer is unhealthy.

### Implementation

- `services/zinder-query/src/derive_proxy.rs` is the canonical home for `DeriveProxy`, `DeriveReadinessGauge`, `DeriveReadinessProbeConfig`, and `spawn_derive_readiness_probe`.
- The federation primitive does not import any consumer's protobuf. Each derive consumer's wiring code constructs its own `DeriveProxy<*QueryClient<AuthenticatedChannel>>` and supplies the readiness probe closure.
- Capability-coverage tests assert that any `WalletQuery.*` method whose body calls `DeriveProxy::forward` advertises a capability starting with `derive.`. Adding a `wallet.*` capability for a federated method fails CI.

### Testing

- `DeriveProxy::forward` returns `Status::unavailable` when the readiness gauge reports not-ready; this is exercised by `derive_proxy::tests::forward_returns_unavailable_when_proxy_not_ready` in `services/zinder-query/src/derive_proxy.rs`.
- The probe loop drives the gauge on every tick; covered by `derive_proxy::tests::probe_loop_updates_gauge_on_each_tick`.
- The federated `WalletQuery.TransparentAddressBalance` integration test asserts capability gating end-to-end: the proxy is configured, the probe is scripted to flip ready/not-ready, and the federated `ServerInfo` response is asserted before and after each transition.

## Alternatives Considered

### A free-function `proxy_to_derive::<Req, Resp>(method, request)` on the adapter

Rejected. A free function does not own probe state or capability gating; each call site would have to read those from somewhere. Either every adapter method gains the same boilerplate (capability checks + gauge reads) or the function takes the gauge as a parameter — at which point a struct holding both is the right shape.

### One adapter method per derive consumer (`proxy_to_explorer`, `proxy_to_analytics`, ...)

Rejected. Three near-duplicate methods on the adapter compound the entropy each one was supposed to prevent. Generic over `Client` is the same code; one function, parameterized.

### Probe each method on every call rather than via a background loop

Rejected. Per-call probing turns every federated method into two RPC round-trips. The readiness signal changes on the order of seconds; a 5s background probe loop is cheaper and gives operators a tunable knob.

### Bundle multiple consumers into one `DeriveProxy`

Rejected. Each derive consumer has its own client type, its own capability, and its own readiness lifecycle; bundling forces the proxy to hold a tagged union or trait object. A separate `DeriveProxy<C>` per consumer keeps the type system honest.

## Out of Scope

- TLS-terminated derive plane. ADR-0009 covers transport security for the writer-side ingest control plane; the derive proxy reuses `connect_authenticated_channel` and inherits the same posture. Public-internet TLS for derive consumers is a separate ADR if a deployment ever requires it.
- A standardized derive-consumer SDK as a separate crate. Derive helpers live in `services/zinder-derive/src/consumer/` until a second consumer beyond explorer justifies extraction.
