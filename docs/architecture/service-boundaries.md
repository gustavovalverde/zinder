# Service Boundaries

Zinder is one product with multiple deployable services. The boundary rule is simple: the service that follows the chain writes canonical state, and the service that serves wallets reads epoch-bound state through a Zinder-owned read contract.

## Boundary Map

| Boundary                     | Owns                                                                                                                    | Must Not Own                                                                    |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `zinder-ingest`              | Upstream node connections, unified ingest loop (bulk catch-up and tip-follow phases), reorg handling, canonical artifact commits, ingest-hosted derive commits, migrations | Public wallet traffic, user wallet secrets, explorer query serving              |
| `zinder-query`               | Wallet-facing APIs, explorer read APIs, transaction broadcast facade, response consistency                              | Chain selection, canonical writes, migrations, derived-index repair             |
| `zinder-compat-lightwalletd` | Vendored lightwalletd-compatible gRPC behavior, compatibility error mapping, protocol translation over `WalletQueryApi` | Upstream node calls, primary canonical storage, migrations, compact block construction |
| `zinder-explorer`            | Explorer query serving, secondary derive-store reads, explorer-specific APIs and capability advertising                 | Wallet sync, canonical chain state, source truth, derive-store primary writes   |

## Why This Split Exists

Zcash indexing has two distinct jobs that often get coupled: converting upstream node state into durable, queryable chain artifacts, and serving wallets and applications with stable APIs and privacy-aware behavior. The jobs have different failure modes. Ingestion needs deterministic sync, reorg handling, atomic commits, schema migration, recoverability, and source-failure handling. Query serving needs latency, compatibility, privacy boundaries, and independent scale-out.

Coupling them in one runtime hides operational costs: read load can interfere with chain commits, migrations become user-visible outages, and derived explorer features can drift into the wallet path. One runtime can do both during local development. Neither supported deployment topology may let read traffic share the same ownership boundary as chain commits.

The same shape appears across the indexer ecosystem: Blockscout separates indexer, web, and API modes; Sui separates checkpoint processing from ingestion sources; Reth Execution Extensions model committed and reverted chains explicitly; Substreams treats indexing as deterministic transformations with replayable sinks.

## Allowed Coupling

The services may share:

- Domain types from `zinder-core`.
- Storage contracts from `zinder-store`.
- Protocol definitions from `zinder-proto`.
- Deterministic test fixtures from `zinder-testkit`.

The services must not share:

- Mutable in-memory chain state.
- Migration ownership.
- Node client loops.
- Derived-index write access to canonical tables.
- Compatibility adapters that bypass `WalletQueryApi`.

## Storage Ownership

The current implementation is the `rocksdb-single-host` topology. The accepted
`postgres-scale-out` migration target preserves the ownership rules below while
replacing RocksDB primary/secondary mechanics with fenced writers and
request-scoped Postgres read sessions. [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
owns the two-topology contract.

`zinder-ingest` is the only writer to canonical chain storage; it opens `PrimaryChainStore` per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md). It also owns the derive-store primary for bundled explorer projections and runs the derive tailer over retained canonical events. The derive store remains separate from canonical storage and is rebuildable from canonical artifacts and retained events.

`zinder-query` and `zinder-compat-lightwalletd` open the writer's canonical store path through `SecondaryChainStore`, using a process-unique `secondary_path` and replaying the writer's WAL on a configurable catchup interval. They also open the bundled derive store as a secondary when serving derive-backed wallet reads such as transparent-address transaction history. They may own separate operational caches. Those caches must be reconstructable and must not become a second source of chain truth.

`zinder-explorer` opens the ingest-owned derive store as a `DeriveStore` secondary when it is available and serves explorer reads from that snapshot. If the derive store is absent, the explorer process still starts and advertises only capabilities that do not require derive storage. Derived storage is downstream materialized state, not canonical state. It may be stale, rebuilding, or disabled without making `zinder-query` unsafe for wallet sync. The `Derive*` SDK abstractions (`DeriveConsumer`, `DeriveStore`) describe the reusable pattern and stay derive-shaped so future consumers can link the same SDK; the product-facing binary, config namespace, capability prefix, and Prometheus prefix use the explorer namespace. See [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md).

## Development Profile

Zinder may provide a local command that runs ingest and query together:

```text
zinder dev
```

That command should be a composition layer. It should instantiate `zinder-ingest` and `zinder-query` through their production interfaces. It must not create a special local-only path that bypasses storage contracts, epochs, readiness checks, or reorg handling.

Do not introduce a generic `zinder-serve` crate or service for this profile. If
one process hosts multiple services locally, that process is composition glue;
the product boundaries remain `zinder-ingest`, `zinder-query`, and
`zinder-compat-lightwalletd`.

## Deployment Topologies

### `rocksdb-single-host`

Minimum service set (per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md)):

```text
zinder-ingest              -> canonical RocksDB (primary)
                           -> IngestControl.WriterStatus / ChainEvents -> [ingest_control] gRPC
zinder-query               -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi
                           -> replica lag via ingest_control.addr
                           -> proxy subscriptions to the private ingest-control endpoint
zinder-compat-lightwalletd -> canonical RocksDB (secondary, unique secondary_path)
                           -> derive RocksDB (secondary, secondary_path/derive)
                           -> WalletQueryApi
                           -> replica lag via ingest_control.addr
                           -> proxy only subscription-like RPCs present in CompactTxStreamer
                           -> CompactTxStreamer
```

Extended service set (adds the current derived plane):

```text
zinder-ingest              -> canonical RocksDB (primary)
                           -> derive RocksDB (primary, nested under canonical storage path)
zinder-query               -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi
zinder-compat-lightwalletd -> canonical RocksDB (secondary, unique secondary_path)
                           -> derive RocksDB (secondary, secondary_path/derive) -> WalletQueryApi -> CompactTxStreamer
zinder-explorer            -> derive RocksDB (secondary when available, secondary_path/derive) -> ExplorerQuery
```

Read replicas are colocated with the writer on one shared-filesystem host.
Cross-host RocksDB replicas are out of scope; see
[ADR-0003 §Out of Scope](../adrs/0003-canonical-storage-access-boundary.md#out-of-scope).
This topology is production-supported and has no Postgres dependency.

### `postgres-scale-out` migration target

This topology is not implemented or certified yet. Its accepted service shape
keeps one fenced canonical writer while allowing projectors, query services,
and Postgres replicas to deploy and scale independently:

```text
zinder-ingest       -> Postgres canonical schema (one fenced active writer)
zinder-projector    -> Postgres wallet or explorer schema (fenced per projection)
zinder-query        -> epoch-bound canonical + wallet read sessions -> WalletQueryApi
zinder-explorer     -> epoch-bound canonical + wallet + explorer reads -> ExplorerQuery
compatibility edges -> WalletQueryApi / ExplorerQuery
```

Role-scoped credentials, writer-generation fencing, replica-lag reporting,
request-scoped epoch reads, failover, and stale-writer rejection are part of
this topology's certification boundary. It becomes production-supported only
after those gates and the shared lifecycle targets pass.

## Anti-Patterns

- A single production daemon where query handlers call upstream node RPC directly.
- A query service that writes missing blocks on demand.
- A query service that opens the live canonical RocksDB database as **primary** in production. Secondary access is the production contract per ADR-0003.
- A compatibility adapter that opens storage or calls upstream nodes instead of translating `WalletQueryApi`.
- A generic `zinder-serve` boundary that hides which service owns ingestion,
  query, or compatibility behavior.
- A derived explorer index that is required for wallet sync.
- A migration that runs because a query process booted.
- A `common` crate that silently becomes the real application.
- A `wallet service` that does not actually implement a wallet.
