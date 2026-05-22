# Zinder

Zinder is a self-hosted Zcash chain indexer for wallets, explorers, and application backends. It runs on top of a [Zebra](https://github.com/ZcashFoundation/zebra) full node and exposes a stable gRPC data plane that pins every read to a single chain epoch, so a sync batch never mixes data across competing tips.

It speaks two protocols on the same data: a native `WalletQuery` API for new clients, and a `CompactTxStreamer` compatibility surface for existing [lightwalletd](https://github.com/zcash/lightwalletd) wallets like [Zodl](https://github.com/zodl-inc/zodl-android). [Zallet](https://github.com/zcash/wallet) and other Rust consumers integrate through a typed client crate.

## Quickstart

Zinder reads chain data from a Zebra node managed by the [Z3 platform stack](https://github.com/ZcashFoundation/z3). Bring up Z3 first, then attach zinder. Both can run on a laptop; there is no separate Z3 instance to provision.

**Prerequisites.** Docker (or Docker Desktop) with Docker Compose v2, plus a clone of this repo.

**1. Bring up the Z3 stack** (one-time, from your Z3 checkout):

```bash
# testnet (fast; ~10 GB chain)
git clone https://github.com/ZcashFoundation/z3.git && cd z3
Z3_NETWORK=testnet docker compose up -d

# or mainnet (~256 GB chain; expect a long initial sync)
Z3_NETWORK=mainnet docker compose up -d
```

This creates the `z3-<network>` Docker network and the `z3-<network>-cookie` volume that zinder attaches to.

**2. Bring up zinder** (from this repo):

```bash
# testnet
docker compose --env-file deploy/.env.testnet -f deploy/docker-compose.yml up -d

# or mainnet
docker compose --env-file deploy/.env.mainnet -f deploy/docker-compose.yml up -d
```

The first `up -d` does two things in sequence:

1. **Builds** `zinder-ingest:latest`, `zinder-query:latest`, and `zinder-explorer:latest` locally from the shared `deploy/Dockerfile` builder. The first target compiles every shipped runtime binary once; later targets reuse the same BuildKit Cargo cache.
2. **Starts** `zinder-ingest`, `zinder-query`, and `zinder-explorer`. The writer's unified ingest loop probes Zebra's tip, classifies the gap, and dispatches the right phase: `BulkCatchup` at 32-way pipelined fetches and up to 1,000 blocks per epoch on a cold store (about 30 to 60 minutes on testnet, several hours on mainnet), then transitions to `TipFollow` automatically once the gap closes through the reorg window. No bootstrap step, no separate one-shot service.

From another terminal:

```bash
docker logs -f zinder-ingest                              # full event log
docker logs zinder-ingest | grep ingest_phase_changed     # phase transitions
```

Subsequent `up -d` calls reuse the existing store. The loop picks up from wherever the writer left off: seconds of work in `TipFollow` after a brief restart; a return to `BulkCatchup` after a long absence until the gap closes.

For the phase model, the upstream-sync diagnostic, alternative deployment shapes (bare-metal, Kubernetes), and forked-store recovery, see [Initial sync](docs/runbooks/initial-sync.md). For the architectural relationship between Zinder and Z3 (or any future upstream platform), and how observability federation works across the two stacks, see [Upstream platform binding](docs/architecture/upstream-platform-binding.md).

**Observability is on by default.** Every `up -d` brings a `zinder-prometheus` container that scrapes the writer (`zinder-ingest:9105/metrics`) and the reader (`zinder-query:9106/metrics`) over a Zinder-owned Docker network. Metrics survive sync runs and restarts. Add `--profile observability` to also bring `zinder-grafana` (port 3002) with the bundled dashboards; skip the flag if you'd rather feed metrics into a sibling Grafana (Z3's, Grafana Cloud, etc.).

**3. Verify both planes:**

```bash
# Writer: "ready" once the loop's BulkCatchup phase has filled the store and
# the TipFollow phase has closed the last ~100 blocks of lag to Zebra's tip
curl -sS http://127.0.0.1:9105/readyz
# {"status":"ready","cause":"ready","current_height":4016431,"target_height":4016431}

# Reader: always "ready" as soon as the writer has committed anything
curl -sS http://127.0.0.1:9106/readyz
# {"status":"ready","cause":"ready","current_height":4016431,"target_height":4016431}

# Tail the writer as it follows the chain tip
docker logs -f zinder-ingest
```

Point a wallet at `localhost:9101` (native `WalletQuery` gRPC) as soon as the reader is ready. The lightwalletd-compatible surface is a separate service shipped in the same workspace; see [single-container deployment](deploy/single-container/README.md) for the bundled topology that adds it.

**Multiple networks side-by-side.** Each `--env-file` produces an independently named stack (`zinder-mainnet`, `zinder-testnet`, `zinder-regtest`) with its own volumes and host ports. Mainnet uses the canonical ports above; testnet adds `+10000` (`19101`, `19106`, ...); regtest adds `+20000`. Bringing up another network is `docker compose --env-file deploy/.env.<network> -f deploy/docker-compose.yml up -d` — the existing stack keeps running.

**Beyond Docker.** See the [VM runbook](docs/runbooks/deploying-on-a-vm.md) for systemd-managed deployments, the [Railway runbook](docs/runbooks/deploying-on-railway.md) for hosted topologies, and the [single-container](deploy/single-container/README.md) image when you want one process tree instead of two containers.

## Where Zinder fits

- **Wallet developers** get a native `WalletQuery` gRPC API alongside lightwalletd `CompactTxStreamer` compatibility. The wallet surface covers compact blocks, tree state, subtree roots, transaction lookup, typed broadcast, chain events, mempool views, and transparent-address artifacts. Capability strings advertise each feature so clients can negotiate explicitly rather than infer from version pins.
- **Application backends** get epoch-consistent reads through a typed query API and a dedicated explorer plane (`zinder-explorer`) serving block summaries, transaction details, mempool views, and typed search. Materialized views consume canonical artifacts and can be rebuilt without affecting wallet sync.
- **Rust consumers** depend on `zinder-client`, a typed crate exposing `ChainIndex` with two implementations selected by topology: `LocalChainIndex` for colocated readers (RocksDB-secondary, no tonic round-trip) and `RemoteChainIndex` for cross-host (gRPC). Zallet uses the same trait.
- **Operators** get explicit `/healthz`, `/readyz`, and `/metrics` endpoints with a `phase` field (`bulk_catchup`, `following_tip`, `awaiting_upstream`) orthogonal to typed readiness causes (`syncing`, `node_unavailable`, `upstream_not_ready`, `reorg_window_exceeded`, others), so load balancers and incident response act on machine-readable state. Production configuration refuses to start with placeholder credentials, unsafe binds, or missing storage.

The deployment target is single-operator self-hosting backed by one Zebra node. Public endpoints sit behind operator-controlled TLS termination, authentication, rate limiting, and quota accounting.

## Further reading

- [What Zinder is and is not](docs/architecture/indexer-wallet-boundary.md): the first link new integrators should follow.
- [Service boundaries](docs/architecture/service-boundaries.md): the boundary contract Zinder is built against.
- [Integration surfaces](docs/reference/integration-surfaces.md): how wallet, application, explorer, and operator clients connect to Zinder.
- [Architecture index](docs/README.md): full documentation index.

## Architecture at a glance

Zinder is one product split into independently scalable planes. One process writes canonical chain state; every other plane reads from it through a typed contract. The boundary rule is enforced by review, not by code: ingest is the only writer, and reads pin to a single `ChainEpoch` so a sync batch never mixes data across competing tips.

```mermaid
flowchart LR
    Zebra["Zebra node"]

    subgraph zinder["Zinder"]
        direction TB
        Source["zinder-source<br/>(source adapters)"]
        Ingest["zinder-ingest<br/>(sole writer)"]
        Store[("zinder-store<br/>canonical RocksDB")]
        Query["zinder-query<br/>WalletQuery API"]
        Compat["zinder-compat-lightwalletd<br/>CompactTxStreamer"]
        Explorer["zinder-explorer<br/>(optional)"]

        Source --> Ingest
        Ingest --> Store
        Store --> Query
        Compat --> Query
        Ingest -. "ChainEvents subscription" .-> Query
        Ingest -. "ChainEvents subscription" .-> Explorer
    end

    Zebra --> Source
    Query --> NativeClients["Zallet, native wallets, Rust SDKs"]
    Compat --> LightwalletdClients["Zodl, Android SDK, lightwalletd clients"]
    Explorer --> Explorers["explorers, analytics"]
```

### Planes

- **Node source boundary** (`zinder-source`). All Zebra node coupling is isolated here. Adapters normalize upstream node observations into `NodeSource` values; no other crate imports Zebra or source-specific types, so a new source backend is a new module here rather than a workspace-wide refactor. See [node source boundary](docs/architecture/node-source-boundary.md).
- **Chain ingestion plane** (`zinder-ingest`). The only writer to canonical storage. Owns the unified ingest loop (bulk catch-up and tip-follow phases), reorg handling, artifact builders, and the atomic chain-epoch commit (`commit_ingest_batch`) that makes a new epoch visible. See [chain ingestion](docs/architecture/chain-ingestion.md), [chain events](docs/architecture/chain-events.md), and [ADR-0015](docs/adrs/0015-unified-phase-driven-ingest.md).
- **Canonical storage** (`zinder-store`). RocksDB-backed `PrimaryChainStore` and `SecondaryChainStore` role handles exposed to services through the domain-shaped `ChainEpochReadApi`. RocksDB types are private; the public read API is epoch-bound, so callers always resolve one `ChainEpoch` before reading any artifact. See [storage backend](docs/architecture/storage-backend.md) and [ADR-0003](docs/adrs/0003-canonical-storage-access-boundary.md).
- **Wallet data plane** (`zinder-query`). Read-only wallet and application API over `WalletQueryApi`, served as the native `WalletQuery` gRPC service. Owns compact block ranges, tree state, subtree roots, transaction lookup, transaction broadcast, the public `ChainEvents` proxy, mempool snapshots/events, and transparent-address reads. Never calls upstream nodes, never writes storage, never custodies keys. See [wallet data plane](docs/architecture/wallet-data-plane.md).
- **Compatibility plane** (`zinder-compat-lightwalletd`). Translates the vendored lightwalletd `CompactTxStreamer` calls onto `WalletQueryApi`. A pure translation layer: it has no source client, no canonical storage handle, and no parallel artifact construction. New compatibility adapters must use the same shape. See [protocol boundary](docs/architecture/protocol-boundary.md).
- **Explorer plane** (`zinder-explorer`, optional). Serves block summaries, transaction details, typed search, mempool dashboards, and other explorer-shaped reads through `ExplorerQuery`. Owns materialized views (consuming canonical artifacts and the `ChainEvents` stream) that cannot affect canonical state; any derived view can be discarded and rebuilt from canonical artifacts. See [explorer plane](docs/architecture/explorer-plane.md) and [derive plane](docs/architecture/derive-plane.md) (the reusable SDK pattern this service exercises).

Two foundation crates are shared across every plane: `zinder-core` (chain vocabulary: `ChainEpoch`, `BlockArtifact`, `Network`) and `zinder-proto` (`.proto` files and generated wire modules, including the pinned vendored lightwalletd schemas). Every binary also exposes an operational HTTP surface (`/healthz`, `/readyz`, `/metrics`) with typed readiness causes; that contract is owned by `zinder-runtime`. See [service operations](docs/architecture/service-operations.md).

For the full boundary contract, read [service boundaries](docs/architecture/service-boundaries.md).

## Workspace

Domain crates under `crates/` define stable contracts with no service runtime:

- `zinder-core`: chain vocabulary (`ChainEpoch`, `BlockArtifact`, `CompactBlockArtifact`, `Network`).
- `zinder-store`: RocksDB-backed canonical storage. Owns `PrimaryChainStore`, `SecondaryChainStore`, `ChainEpochReader`, `ChainEpochReadApi`, `ChainEvent`, `StreamCursorTokenV1`. RocksDB types are private; the public API is domain-shaped.
- `zinder-source`: upstream source adapters. Owns `NodeSource`, `NodeAuth`, `NodeCapabilities`, `ZebraJsonRpcSource`, `TransactionBroadcaster`.
- `zinder-proto`: protocol ownership. Owns `.proto` files and tonic-generated modules under `v1::wallet`, private `v1::ingest`, and vendored `compat::lightwalletd`.

Deployable services under `services/`:

- `zinder-ingest`: the only writer to canonical RocksDB. Owns the unified ingest loop (bulk-catchup + tip-follow phases, see [ADR-0015](docs/adrs/0015-unified-phase-driven-ingest.md)), backup checkpoints, artifact builders, the private writer-status endpoint, and the upstream-source config CLI.
- `zinder-query`: read-only wallet and application API over `ChainEpochReadApi`. Provides `WalletQueryApi` (Rust) and `WalletQueryGrpcAdapter` (tonic).
- `zinder-compat-lightwalletd`: translates the vendored lightwalletd `CompactTxStreamer` gRPC service to `WalletQueryApi`.

## Validation Gate

Run before considering any change complete:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
cargo nextest run --profile=ci-perf
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
git diff --check
```

`cargo nextest run` is the canonical workspace runner. Tests are tiered by directory as documented in the [Testing Runbook](docs/runbooks/testing.md): T0 unit, T1 integration, T2 perf, T3 live, and consumer certification. The `default`/`ci` profile runs T0 and T1; `ci-perf` runs T2; `ci-live` runs upstream-node T3; `ci-zallet-live` runs the real Zallet binary gate when a Zinder-native Zallet build is available. `cargo test` continues to work as a libtest fallback (and is what `cargo mutants` shells), but is not the documented gate.

Heavier probes for trust-sensitive storage or parser changes:

```bash
cargo llvm-cov --workspace --all-features --no-report
cargo mutants --workspace --all-features \
  --file crates/zinder-store/src/chain_store.rs \
  --file crates/zinder-store/src/chain_store/validation.rs \
  --file crates/zinder-source/src/source_block.rs \
  --re 'chain_event_history|finalized_only_commit_without_artifacts|validate_reorg_window_change|from_raw_block_bytes'
```

T3 live tests use the same env-var schema as production binaries and are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` plus a runtime `require_live()` check. Mainnet is rejected unless tests opt in by name. To run T3 against a local Zebra:

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live --run-ignored=all
```

For testnet, swap `ZINDER_NETWORK=zcash-testnet` and use Zebra cookie auth (the cookie file's `user:pass` split feeds `ZINDER_NODE__AUTH__USERNAME` and `ZINDER_NODE__AUTH__PASSWORD`). Mainnet live tests require an explicit mainnet opt-in as documented in the [Testing Runbook](docs/runbooks/testing.md).

## Local Observability Smoke

Use the local observability smoke when a change needs visible runtime evidence
rather than only test output:

```bash
scripts/observability-smoke.sh run
```

It starts Prometheus and Grafana, runs the Zinder host binaries against the
selected local node source, verifies checkpoint backup restore, generates native
and lightwalletd-compatible gRPC traffic, prints the scraped metric samples, and
writes readiness reports under `.tmp/observability/reports`. See
[observability/README.md](observability/README.md) for public-network commands,
calibration runs, ports, tunables, and the stop command.
