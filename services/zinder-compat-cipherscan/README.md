# Zinder Cipherscan Adapter

`zinder-compat-cipherscan` serves Cipherscan's current REST and WebSocket
contracts from Zinder's native query services. It lets an unchanged Cipherscan
application use Zinder for covered chain-data workflows.

The adapter is a stateless edge service. It does not open the Zinder store,
index blocks, or own Cipherscan product data.

```text
browser -> Cipherscan web -> zinder-compat-cipherscan
                                  |          |
                                  v          v
                           ExplorerQuery  WalletQuery
                                  \          /
                                   Zinder store
```

Zinder owns canonical facts, reusable projections, coverage, and freshness.
This crate owns Cipherscan paths, JSON shapes, query coercion, display units,
cache behavior, mining-pool metadata, and product formulas. The
[coverage matrix](../../docs/plans/cipherscan-adapter-architecture.md) records
the owner and current status of every route family.

## Requirements

Use matching revisions of these Zinder services:

- `zinder-ingest`, caught up on the intended network;
- `zinder-query`, serving `WalletQuery`;
- `zinder-explorer`, serving `ExplorerQuery`; and
- `zinder-compat-cipherscan`, configured for the same network.

The adapter connects to both gRPC readers at startup. If they require bearer
authentication, both endpoints must accept the token loaded from the adapter's
single `bearer_token_path`.

The adapter needs outbound HTTPS access only for the configured current and
historical market-price providers. A price-provider failure degrades price
routes; it does not replace missing prices with zero.

## Local testnet

Start the existing Z3 testnet and the Zinder testnet stack. This reuses the
named `zinder-testnet-data` volume; it does not recreate or replace the store.

```bash
cd ../z3
docker compose --env-file .env.testnet up -d

cd ../zinder
docker compose \
  --env-file deploy/.env.testnet \
  -f deploy/docker-compose.yml \
  up -d zinder-ingest zinder-query zinder-explorer
```

Wait for the three Zinder services to become ready, then run the adapter on the
host against the testnet reader ports:

```bash
cargo run --locked -p zinder-compat-cipherscan -- \
  --network zcash-testnet \
  --explorer-query-endpoint http://127.0.0.1:19068 \
  --wallet-query-endpoint http://127.0.0.1:19101 \
  --listen-addr 127.0.0.1:9070 \
  --ops-listen-addr 127.0.0.1:9108
```

Confirm both operational readiness and a data-bearing route:

```bash
curl --fail http://127.0.0.1:9108/healthz
curl --fail http://127.0.0.1:9108/readyz
curl --fail http://127.0.0.1:9070/api/info
curl --fail 'http://127.0.0.1:9070/api/mining/pool-distribution?period=7d'
```

`/readyz` proves that the process started and connected to its dependencies.
It does not prove that every projection covers the requested range. Keep the
data-bearing probe in deployment checks so incomplete backfill or unavailable
capabilities remain visible.

### Run the unchanged Cipherscan UI

Cipherscan currently exposes a configurable custom API URL through its
Crosslink deployment slot. Use that slot to run the source-unmodified UI
against a local adapter:

```bash
cd ../cipherscan
NEXT_PUBLIC_NETWORK=crosslink-testnet \
NEXT_PUBLIC_CROSSLINK_API_URL=http://127.0.0.1:9070 \
CIPHERSCAN_API_URL=http://127.0.0.1:9070 \
npm run dev -- --port 3003
```

Open <http://127.0.0.1:3003>. Core explorer workflows use the adapter.
Crosslink-specific, sidecar-owned, and explicitly unavailable panels may show
degraded states; they are not supplied by Zinder.

The public `NEXT_PUBLIC_CROSSLINK_API_URL` is used by browser requests.
`CIPHERSCAN_API_URL` is used by Cipherscan server routes and must be reachable
from the Next.js process. They can be different URLs in a container
deployment.

## Deployment model

Deploy the adapter with Zinder, not as another indexer inside Cipherscan:

```text
Z3 / Zebra
    |
zinder-ingest ---- persistent canonical and derive volume
    |
    +-- zinder-query --------+
    |                        |
    +-- zinder-explorer -----+--> zinder-compat-cipherscan
                                      |
                               TLS reverse proxy
                                      |
                                Cipherscan web
```

The adapter:

- mounts no Zinder or Cipherscan data volume;
- can be restarted or replaced without backfill;
- keeps only bounded in-memory response caches;
- must use the same Zcash network as both Zinder readers;
- should reach Zinder gRPC over a private network; and
- should expose its REST listener through a TLS reverse proxy.

Do not run Cipherscan's current `docker-compose.yml` unchanged for this
topology. That file starts Cipherscan's Express API, PostgreSQL, Redis, Zebra,
and lightwalletd. For Zinder-backed covered routes, the adapter takes the place
of the Express API. Deploy any still-required Cipherscan-owned sidecars
separately; do not add their product schemas to Zinder.

### Build and run the service

Build the release binary from the same revision as the Zinder readers:

```bash
cargo build --locked --release --bin zinder-compat-cipherscan
./target/release/zinder-compat-cipherscan --help
```

Install that binary in the same way as the other per-service Zinder binaries
and supervise it with the platform's normal process manager. The service
handles termination signals and shuts down its realtime tasks before exit.

A production TOML configuration can keep both listeners on loopback when a
local reverse proxy terminates TLS:

```toml
[network]
name = "zcash-testnet"

[security]
allow_public_bind = false

[ops]
listen_addr = "127.0.0.1:9108"

[cipherscan]
listen_addr = "127.0.0.1:9070"
explorer_query_endpoint = "http://127.0.0.1:9068"
wallet_query_endpoint = "http://127.0.0.1:9101"
current_price_endpoint = "https://api.coingecko.com/api/v3/simple/price?ids=zcash&vs_currencies=usd&include_24hr_change=true"
historical_price_endpoint_template = "https://api.coingecko.com/api/v3/coins/zcash/history?localization=false&date={date}"
# bearer_token_path = "/run/secrets/zinder-reader-token"
```

Run it with:

```bash
zinder-compat-cipherscan --config /etc/zinder/cipherscan.toml
```

Configuration precedence is defaults, TOML file, `ZINDER_*` environment
variables, then CLI flags. The equivalent container-oriented environment
variables are:

```bash
ZINDER_NETWORK__NAME=zcash-testnet
ZINDER_SECURITY__ALLOW_PUBLIC_BIND=true
ZINDER_OPS__LISTEN_ADDR=0.0.0.0:9108
ZINDER_CIPHERSCAN__LISTEN_ADDR=0.0.0.0:9070
ZINDER_CIPHERSCAN__EXPLORER_QUERY_ENDPOINT=http://zinder-explorer:9068
ZINDER_CIPHERSCAN__WALLET_QUERY_ENDPOINT=http://zinder-query:9101
ZINDER_CIPHERSCAN__BEARER_TOKEN_PATH=/run/secrets/zinder-reader-token
```

Use `--print-config` to inspect the resolved configuration. Secret material is
not printed.

### Build Cipherscan for a custom endpoint

Cipherscan's `NEXT_PUBLIC_*` values are compiled into the browser bundle.
Set them while building the production application, then provide the internal
server URL when starting it:

```bash
cd ../cipherscan
NEXT_PUBLIC_NETWORK=crosslink-testnet \
NEXT_PUBLIC_CROSSLINK_API_URL=https://api.example.test \
npm run build

CIPHERSCAN_API_URL=http://zinder-compat-cipherscan:9070 \
npm start
```

Route `https://api.example.test` to the adapter. Route WebSocket upgrades on
the same origin. Keep the adapter's operational port private.

## Operations

Use these probes for distinct questions:

| Probe | Meaning |
| --- | --- |
| `GET /healthz` on the ops port | The adapter process is alive. |
| `GET /readyz` on the ops port | Startup completed and initial gRPC connections succeeded. |
| `GET /api/info` on the REST port | The adapter can serve current chain data. |
| `GET /api/mining/pool-distribution?period=7d` | The block-production projection covers the requested window. |

Treat non-success and degraded responses as operational signals. The adapter
does not fabricate zero, empty, complete, healthy, or canonical values when a
source fact is missing.

A restart clears in-memory caches. That can change `generatedAt` and refresh
provider-backed data, but it does not affect canonical or projection state.
The mining pool-distribution cache lasts five minutes and is keyed by the
effective raw period string.

For route-level acceptance and the focused UI regression, follow the
[Cipherscan adapter verification runbook](../../docs/runbooks/cipherscan-adapter-verification.md).
Projection identity, backfill, restart, pagination, and reorg behavior are
specified by
[ADR 0033](../../docs/adrs/0033-time-indexed-block-production.md).
