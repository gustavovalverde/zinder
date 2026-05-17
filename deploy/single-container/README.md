# Single-container Zinder deployment

This is the v1 recommended deployment shape for single-operator self-hosting (per [ADR-0003](../../docs/adrs/0003-canonical-storage-access-boundary.md)).

## What runs inside

```
┌──────────────────────────────────────────────────────────┐
│  Container (s6-overlay PID 1)                            │
│                                                          │
│  ┌────────────────────┐         ┌──────────────────────┐ │
│  │   zinder-ingest    │ ──────▶ │ /var/lib/zinder/     │ │
│  │   (writer)         │  commit │   store/             │ │
│  │   port 9100 LO     │         │   (RocksDB primary)  │ │
│  └──────────┬─────────┘         └──────────┬───────────┘ │
│             │ IngestControl                │ secondary   │
│             │ gRPC (loopback)              ▼  open       │
│  ┌──────────▼─────────┐         ┌──────────────────────┐ │
│  │   zinder-query     │ ◀────── │ /var/lib/zinder/     │ │
│  │   (reader)         │  read   │   secondary/         │ │
│  │   port 9101 → host │         │   (RocksDB secondary)│ │
│  └────────────────────┘         └──────────────────────┘ │
│                                                          │
│  ops endpoint: localhost:9106 → /healthz /readyz /metrics│
└──────────────────────────────────────────────────────────┘
        │
        │ 9101 (WalletQuery gRPC, plaintext)
        │ 9106 (ops HTTP, plaintext)
        ▼
┌──────────────────────────────────────────────────────────┐
│  Operator-supplied reverse proxy                         │
│  (Caddy / Nginx / Cloudflare / cloud load balancer)      │
│  - TLS termination                                       │
│  - rate limiting, IP allowlist, basic auth if desired    │
└──────────────────────────────────────────────────────────┘
                            │
                            ▼
                    Public consumers
                    (wallets, faucets, SDKs)
```

Zinder does not own TLS termination, authentication, or rate limiting; those are out of v1 scope. The reverse-proxy layer is operator-supplied; it sits between public consumers and the container's plaintext gRPC port.

## Required volumes

- `/var/lib/zinder/store`: canonical RocksDB primary. The writer (zinder-ingest) owns this; the reader (zinder-query) opens it as a RocksDB secondary.
- `/var/lib/zinder/secondary`: secondary catchup directory. zinder-query writes here; zinder-ingest never touches it.
- `/etc/zinder/ingest.toml` and `/etc/zinder/query.toml`: per-service configuration files. Each binary strict-parses its own TOML schema (writer and reader fields do not share a section set), so the single-container image mounts two configs. See `config.example.ingest.toml` and `config.example.query.toml` for starting points.

A single named Docker volume covering `/var/lib/zinder` is the simplest layout. The two subdirectories live on the same filesystem so the secondary's catchup performance matches the primary's commit pace.

## Required environment

Set at least the following in each per-service config file (or override via `ZINDER_*` env vars; see [Public interfaces §Environment variable mapping](../../docs/architecture/public-interfaces.md#environment-variable-mapping)):

- `network.name` (`zcash-mainnet`, `zcash-testnet`, `zcash-regtest`)
- `node.json_rpc_addr` — when attaching to a [Z3 stack](https://github.com/ZcashFoundation/z3) over its external Docker network, use `http://zebra:8232` (mainnet) or `http://zebra:18232` (testnet/regtest)
- One of:
  - `node.auth.method = "cookie"` + `node.auth.path = "/var/run/auth/.cookie"` (recommended when attaching to Z3; mount the `z3-<network>-cookie` volume read-only)
  - `node.auth.method = "cookie"` + `node.auth.cookie` (inline credentials for PaaS environments without persistent disks)
  - `node.auth.method = "basic"` + `node.auth.username` + `node.auth.password` (only when the Zebra you target doesn't support cookie auth)

The reader additionally needs `ingest_control.addr = "http://127.0.0.1:9100"` so it can reach the colocated writer.

## Build

```bash
docker build -f deploy/single-container/Dockerfile -t zinder-single-container .
```

## Run

Attached to a running Z3 stack (recommended):

```bash
docker run --rm -d \
  --name zinder \
  --network z3-testnet \
  -p 9101:9101 \
  -p 9106:9106 \
  -v zinder-data:/var/lib/zinder \
  -v z3-testnet-cookie:/var/run/auth:ro \
  -v $(pwd)/ingest.toml:/etc/zinder/ingest.toml:ro \
  -v $(pwd)/query.toml:/etc/zinder/query.toml:ro \
  -e ZINDER_NODE__JSON_RPC_ADDR=http://zebra:18232 \
  -e ZINDER_NODE__AUTH__METHOD=cookie \
  -e ZINDER_NODE__AUTH__PATH=/var/run/auth/.cookie \
  zinder-single-container
```

Standalone (legacy, against a non-Z3 Zebra):

```bash
docker run --rm -d \
  --name zinder \
  -p 9101:9101 \
  -p 9106:9106 \
  -v zinder-data:/var/lib/zinder \
  -v $(pwd)/ingest.toml:/etc/zinder/ingest.toml:ro \
  -v $(pwd)/query.toml:/etc/zinder/query.toml:ro \
  -e ZINDER_NODE__AUTH__COOKIE="${ZEBRA_COOKIE}" \
  zinder-single-container
```

## Health and readiness probes

```bash
# Liveness (process is up):
curl -f http://localhost:9106/healthz

# Readiness (accepting production traffic):
curl -f http://localhost:9106/readyz

# Metrics (Prometheus):
curl http://localhost:9106/metrics
```

The Docker `HEALTHCHECK` directive probes `/readyz` every 30s; the first probe waits 90s for the initial sync to begin. Operators on Kubernetes/Nomad can wire equivalent readiness probes against the same endpoint.

## When NOT to use this image

Use the per-service Dockerfiles in `services/<svc>/Dockerfile` when:

- you run Kubernetes/Nomad and want sidecar-style separation
- you need to scale readers independently of writers (read replicas)
- you operate the writer and the reader in different security boundaries

The per-service images are mechanically simpler (no s6-overlay) and pair cleanly with a service mesh.
