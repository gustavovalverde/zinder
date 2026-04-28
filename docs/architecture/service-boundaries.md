# Service Boundaries

Zinder is one product with multiple deployable services. The boundary rule is simple: the service that follows the chain writes canonical state, and the service that serves wallets reads epoch-bound state through a Zinder-owned read contract.

## Boundary Map

| Boundary                     | Owns                                                                                                                    | Must Not Own                                                                    |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `zinder-ingest`              | Upstream node connections, backfill, tip following, reorg handling, canonical artifact commits, migrations              | Public wallet traffic, user wallet secrets, explorer analytics                  |
| `zinder-query`               | Wallet-facing APIs, explorer read APIs, transaction broadcast facade, response consistency                              | Chain selection, canonical writes, migrations, derived-index repair             |
| `zinder-compat-lightwalletd` | Vendored lightwalletd-compatible gRPC behavior, compatibility error mapping, protocol translation over `WalletQueryApi` | Upstream node calls, primary canonical storage, migrations, compact block construction |
| `zinder-derive`              | Replayable materialized views, explorer-specific indexes, analytics-specific schemas, `ChainEvent` consumption          | Wallet sync, canonical chain state, source truth                                |

## Why This Split Exists

Ingestion and query serving optimize for different things. Ingestion needs correctness under reorgs, durable commits, schema migration, recoverability, and source-failure handling. Query serving needs latency, compatibility, privacy constraints, and read availability.

One runtime can do both during local development. Production architecture must not let read traffic share the same ownership boundary as chain commits.

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

`zinder-ingest` is the only writer to canonical chain storage; it opens `PrimaryChainStore` per [ADR-0007](../adrs/0007-multi-process-storage-access.md).

`zinder-query` and `zinder-compat-lightwalletd` open the writer's canonical store path through `SecondaryChainStore`, using a process-unique `secondary_path` and replaying the writer's WAL on a configurable catchup interval. They may own separate operational caches. Those caches must be reconstructable and must not become a second source of chain truth.

`zinder-derive` writes derived storage. Derived storage is downstream materialized state, not canonical state. It may be stale, rebuilding, or disabled without making `zinder-query` unsafe for wallet sync.

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

## Production Profiles

Minimum production deployment (per [ADR-0007](../adrs/0007-multi-process-storage-access.md)):

```text
zinder-ingest              -> canonical RocksDB (primary)
                           -> IngestControl.WriterStatus / ChainEvents -> [ingest.control] gRPC
zinder-query               -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi
                           -> replica lag via storage.ingest_control_addr
                           -> proxy subscriptions to the private ingest-control endpoint
zinder-compat-lightwalletd -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi
                           -> replica lag via storage.ingest_control_addr
                           -> proxy only subscription-like RPCs present in CompactTxStreamer
                           -> CompactTxStreamer
```

Extended production deployment (adds derived plane):

```text
zinder-ingest              -> canonical RocksDB (primary) + ChainEventEnvelope
zinder-query               -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi
zinder-compat-lightwalletd -> canonical RocksDB (secondary, unique secondary_path) -> WalletQueryApi -> CompactTxStreamer
zinder-derive              -> ChainEventEnvelope or snapshots -> derived storage
```

Read replicas are colocated with the writer in v1 (shared filesystem). Cross-host replicas are out of scope; see ADR-0007 §Out of Scope.

## Anti-Patterns

- A single production daemon where query handlers call upstream node RPC directly.
- A query service that writes missing blocks on demand.
- A query service that opens the live canonical RocksDB database as **primary** in production. Secondary access is the production contract per ADR-0013.
- A compatibility adapter that opens storage or calls upstream nodes instead of translating `WalletQueryApi`.
- A generic `zinder-serve` boundary that hides which service owns ingestion,
  query, or compatibility behavior.
- A derived explorer index that is required for wallet sync.
- A migration that runs because a query process booted.
- A `common` crate that silently becomes the real application.
- A `wallet service` that does not actually implement a wallet.
