# Single-container Zinder deployment

This is the v1 recommended deployment shape for single-operator self-hosting (per [ADR-0003](../../docs/adrs/0003-canonical-storage-access-boundary.md)).

## What runs inside

```
┌──────────────────────────────────────────────────────────────┐
│  Container (s6-overlay PID 1)                                │
│                                                              │
│  ┌────────────────────┐         ┌──────────────────────────┐ │
│  │   zinder-ingest    │ ──────▶ │ /var/lib/zinder/         │ │
│  │   (writer)         │  commit │   store/                 │ │
│  │   port 9100 LO     │         │   (RocksDB primary)      │ │
│  └──────────┬─────────┘         └──────────┬───────────────┘ │
│             │ IngestControl                │ secondary open  │
│             │ gRPC (loopback)              ▼                 │
│  ┌──────────▼─────────┐ ┌────────────────────┐               │
│  │   zinder-query     │ │ /var/lib/zinder/   │               │
│  │   (wallet reader)  │ │   secondary/       │               │
│  │   port 9101 → host │ │ (RocksDB secondary)│               │
│  └────────────────────┘ └────────────────────┘               │
│  ┌──────────▼─────────┐ ┌──────────────────────────┐         │
│  │   zinder-explorer  │ │ /var/lib/zinder/         │         │
│  │   (explorer reader)│ │   explorer-secondary/    │         │
│  │   port 9068 → host │ │ (RocksDB secondary)      │         │
│  └────────────────────┘ └──────────────────────────┘         │
│                                                              │
│  ops endpoints: localhost:9106 (query)   /healthz/readyz/    │
│                 localhost:9069 (explorer)  metrics           │
└──────────────────────────────────────────────────────────────┘
        │
        │ 9101 (WalletQuery   gRPC, plaintext)
        │ 9068 (ExplorerQuery gRPC, plaintext)
        │ 9106, 9069 (ops HTTP, plaintext)
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

- `/var/lib/zinder/store`: canonical RocksDB primary. The writer (zinder-ingest) owns this; the two readers (zinder-query, zinder-explorer) open it as RocksDB secondaries.
- `/var/lib/zinder/secondary`: secondary catchup directory used by zinder-query. zinder-ingest never touches it.
- `/var/lib/zinder/explorer-secondary`: secondary catchup directory used by zinder-explorer. Distinct from `secondary/` so the two readers never race inside the same container.
- `/etc/zinder/ingest.toml`, `/etc/zinder/query.toml`, and `/etc/zinder/explorer.toml`: per-service configuration files. Each binary strict-parses its own TOML schema, so the single-container image mounts three configs. See `config.example.ingest.toml`, `config.example.query.toml`, and `config.example.explorer.toml` for starting points.

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
docker build -f deploy/Dockerfile --target zinder-single-container -t zinder .
```

All runtime images are targets in `deploy/Dockerfile`. The shared
`zinder-binaries` stage compiles every shipped runtime binary once with
BuildKit cache mounts for Cargo registry, Cargo git, and `CARGO_TARGET_DIR`;
the per-service and single-container targets only package those artifacts. New
image shapes should add a target to this Dockerfile so local and CI builds
reuse the same toolchain, native RocksDB build, and Cargo cache policy.

## Run

Attached to a running Z3 stack (recommended):

Use a network-scoped volume name (`zinder-<network>-data`) and matching host ports so a single host can run mainnet and testnet single-container deployments side-by-side. The example below uses testnet (offset `+10000`); for mainnet drop the offset and substitute the `z3-mainnet` network + cookie volume.

```bash
docker run --rm -d \
  --name zinder-testnet \
  --network z3-testnet \
  -p 19101:9101 \
  -p 19068:9068 \
  -p 19106:9106 \
  -p 19069:9069 \
  -v zinder-testnet-data:/var/lib/zinder \
  -v z3-testnet-cookie:/var/run/auth:ro \
  -v $(pwd)/ingest.toml:/etc/zinder/ingest.toml:ro \
  -v $(pwd)/query.toml:/etc/zinder/query.toml:ro \
  -v $(pwd)/explorer.toml:/etc/zinder/explorer.toml:ro \
  -e ZINDER_NODE__JSON_RPC_ADDR=http://zebra:18232 \
  -e ZINDER_NODE__AUTH__METHOD=cookie \
  -e ZINDER_NODE__AUTH__PATH=/var/run/auth/.cookie \
  zinder
```

Standalone (legacy, against a non-Z3 Zebra):

```bash
docker run --rm -d \
  --name zinder-testnet \
  -p 19101:9101 \
  -p 19068:9068 \
  -p 19106:9106 \
  -p 19069:9069 \
  -v zinder-testnet-data:/var/lib/zinder \
  -v $(pwd)/ingest.toml:/etc/zinder/ingest.toml:ro \
  -v $(pwd)/query.toml:/etc/zinder/query.toml:ro \
  -v $(pwd)/explorer.toml:/etc/zinder/explorer.toml:ro \
  -e ZINDER_NODE__AUTH__COOKIE="${ZEBRA_COOKIE}" \
  zinder
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

Use the per-service targets in `deploy/Dockerfile` when:

- you run Kubernetes/Nomad and want sidecar-style separation
- you need to scale readers independently of writers (read replicas)
- you operate the writer and the reader in different security boundaries

The per-service images are mechanically simpler (no s6-overlay) and pair cleanly with a service mesh.
