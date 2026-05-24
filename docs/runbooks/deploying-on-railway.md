# Deploying Zinder on Railway

This runbook gets a fresh Zinder deployment onto Railway (or a structurally similar PaaS like Fly.io or Render) in roughly 30 minutes. Target shape: the `zinder-single-container` target in `deploy/Dockerfile`, running `zinder-ingest` + `zinder-query` together under s6-overlay against a Zebra deployment hosted somewhere reachable from Railway's network.

The same shape applies to any PaaS that:

- runs containers but does not share filesystems across services,
- injects secrets as environment variables,
- exposes one TCP port per container.

Railway specifics in this document are limited to its env-var UI and volume-binding step. Fly.io and Render operators substitute their equivalents.

## Prerequisites

- A Railway project (free tier is sufficient for testnet experimentation; mainnet sizing requires paid).
- A reachable Zebra deployment with JSON-RPC. Zebra can run on Railway too, on a peer VM, or on a managed Zcash-node service.
- The Zebra cookie value (string or file path) or a `username:password` for basic auth.

## Topology

```
┌───────────────────────────────────────────────────────────┐
│ Railway service "zinder"                                  │
│                                                           │
│   single-container image (zinder-ingest + zinder-query)   │
│   s6-overlay supervises both processes                    │
│                                                           │
│   /var/lib/zinder/store      ← Railway persistent volume  │
│   /var/lib/zinder/secondary  ← same persistent volume     │
│                                                           │
│   exposed ports:                                          │
│     9101 → WalletQuery gRPC (public)                      │
│     9106 → ops HTTP (private; for HEALTHCHECK)            │
└───────────────────────────────────────────────────────────┘
        │ Railway routing layer terminates TLS
        ▼
   Public consumers (wallets, faucets, SDKs)
                                            │
                                            ▼
                                       Zebra
                                       (peer service)
```

Railway terminates TLS at its routing layer; the container serves plaintext gRPC over the configured port and Railway maps it to the public HTTPS endpoint.

## Steps

### 1. Create the service

In the Railway UI, create a new service from a Docker image:

- **Source**: `ghcr.io/gustavovalverde/zinder:latest` (replace with a pinned tag for production: `ghcr.io/gustavovalverde/zinder:v0.1.0`).
- **Port**: `9101` (TCP). Enable HTTP/2 / gRPC routing in Railway's networking tab.

### 2. Attach a persistent volume

- Mount path: `/var/lib/zinder`
- Size: 200 GB for testnet; size mainnet according to your retention and proof requirements. The volume holds both the canonical store and the secondary catchup directory.

### 3. Inject configuration via environment variables

Railway provides a UI for environment variables. Set:

| Variable | Value | Notes |
| --- | --- | --- |
| `ZINDER_NETWORK__NAME` | `zcash-mainnet` / `zcash-testnet` / `zcash-regtest` | Required |
| `ZINDER_NODE__JSON_RPC_ADDR` | `http://zebra-host:8232` | Pointing at your Zebra |
| `ZINDER_NODE__AUTH__METHOD` | `cookie` or `basic` | Pick one |
| `ZINDER_NODE__AUTH__COOKIE` | `user:cookie-content` | When `__METHOD=cookie` and the secret is inline |
| `ZINDER_NODE__AUTH__USERNAME` | `zebra` | When `__METHOD=basic` |
| `ZINDER_NODE__AUTH__PASSWORD` | `your-zebra-password` | When `__METHOD=basic` |
| `ZINDER_INGEST_CONTROL__LISTEN_ADDR` | `127.0.0.1:9100` | Internal-only |
| `ZINDER_INGEST_CONTROL__ADDR` | `http://127.0.0.1:9100` | Reader points at colocated writer |

The single-container image accepts the cookie content inline through `ZINDER_NODE__AUTH__COOKIE`; you do not need an entrypoint shim that materializes a cookie file.

Railway's secret-handling UI marks every variable with a leaf name in `{password, secret, cookie, token, private_key}` as sensitive in its console. Treat the variables above accordingly.

### 4. Set the health check

Railway's "Healthcheck" tab:

- **Path**: `/readyz` on port `9106`.
- **Healthy threshold**: 1 success.
- **Unhealthy threshold**: 3 consecutive failures.
- **Initial delay**: 120 seconds. Initial sync can take longer than the default; Railway will mark the service unhealthy before it catches up if the delay is too short.

### 5. Deploy

Trigger the deploy. Watch the build log; the image is built upstream so the deploy is a pull + start.

After the container starts, you should see structured tracing in Railway's log viewer:

```
phase=load_config phase_state=entry
phase=load_config phase_state=exit outcome=ok elapsed_ms=N
phase=open_storage phase_state=entry
phase=open_storage phase_state=exit outcome=ok elapsed_ms=N
phase=connect_node phase_state=entry
phase=connect_node phase_state=exit outcome=ok elapsed_ms=N
phase=ready phase_state=exit outcome=ok elapsed_ms=0
```

If a phase shows `outcome=failed`, the `reason=` field has the underlying error.

### 6. Expected readiness sequence

For a fresh deployment (empty store, mainnet/testnet sync from scratch):

1. **starting** (seconds): config load, storage open; `phase=awaiting_upstream`.
2. **bulk catch-up** (minutes to hours): the unified ingest loop uses byte-watermarked source fetches and parallel canonical fact build; `phase=bulk_catchup`, `cause=syncing`, `lag_blocks` shrinks as progress accumulates.
3. **tip-follow ready** (steady state): accepting traffic; `phase=following_tip`, `cause=ready`.

`/readyz` returns 503 with `cause = "syncing"` during phase 2 and 200 with `cause = "ready"` in phase 3. If Zebra is itself behind the network tip, the cause becomes `upstream_not_ready` with the structured `upstream_health` substructure; configure `[node.health].addr` to point at Zebra's `/ready` endpoint for the precise signal. See [Initial sync](../runbooks/initial-sync.md) for the full diagnostic.

### 7. Test from outside

```bash
# Replace with your Railway public hostname.
grpcurl -tls zinder-xyz.up.railway.app:443 \
    list zinder.v1.wallet.WalletQuery
```

If you set `enable_reflection = true` in `[query.grpc]`, you can list the full method set. Otherwise call a known method directly:

```bash
grpcurl -tls zinder-xyz.up.railway.app:443 \
    zinder.v1.wallet.WalletQuery/ServerInfo
```

### 8. Rollback

Railway maintains deploy history. To roll back, click "Rollback" on a prior good deploy. The persistent volume is preserved across rollbacks; only the container image changes.

For pinned-tag deploys, change the image tag back to the prior version and redeploy.

## Cost optimization

- Railway charges by CPU + memory time + egress. Most of the cost for steady-state Zinder is egress; the deployment serves compact blocks at scale to subscribers.
- The single-container image runs the writer and reader in the same process tree; you pay for one container's resources. Splitting into separate Railway services per process is supported via the per-service targets in `deploy/Dockerfile` but doubles the fixed cost.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| Container restarts repeatedly | Health check fails during initial sync | Increase initial delay to 300s for fresh mainnet sync |
| `/readyz` stays `node_unavailable` | Zebra not reachable from Railway | Ensure the Zebra service is in the same Railway project, or that the public endpoint resolves from Railway egress IPs |
| `/readyz` is `node_capability_missing` | Zebra too old | Upgrade Zebra to the version pinned in this Zinder release |
| Logs show `outcome=aborted` (no explicit complete/fail) | A startup phase panicked | Inspect the prior log line for the panic reason; file an issue |
| Volume fills | Retention windows are tuned for high-availability | Reduce `chain_event_retention_hours` in `[retention]` |
| gRPC traffic returns "HTTP/2 not supported" | Railway routing not configured for gRPC | Enable HTTP/2 on the public endpoint in the networking tab |

## References

- [Public interfaces §Environment variable mapping](../architecture/public-interfaces.md#environment-variable-mapping)
- [`deploy/single-container/`](../../deploy/single-container/)
- [`deploy/single-container/README.md`](../../deploy/single-container/README.md)
- [Service operations](../architecture/service-operations.md)
- [Deploying on a VM](deploying-on-a-vm.md)
