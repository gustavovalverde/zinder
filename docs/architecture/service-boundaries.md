# Service Boundaries

Zinder is one product with three deployable services in the first fact-first
release. The boundary rule is simple: ingest alone writes canonical facts,
projector alone writes wallet state, and compatibility serves only a
request-scoped exact pair of read-only canonical and wallet generations.

## Current Boundary Map

| Boundary | Owns | Must not own |
| --- | --- | --- |
| `zinder-ingest` | Upstream connections, chain selection, canonical construction and following, reorg handling, canonical publication, durable chain and mempool event history | Wallet projection writes, public wallet traffic, or projection promotion |
| `zinder-projector` | Fixed-fence wallet construction, continuous canonical-event following, settlement and bounded reorg reconciliation, wallet-store primary ownership | Chain selection, canonical writes, public traffic, or source-node RPCs |
| `zinder-compat-lightwalletd` | Canonical and wallet secondaries, exact-pair admission and atomic generation replacement, vendored lightwalletd behavior, compatibility error mapping | Either primary store, source-node reads, migrations, or mixed-generation responses |
| `zinder-query` library | `WalletQueryApi`, request types, native adapter code, and shared query error mapping | A standalone production listener, storage ownership, or lifecycle orchestration |

`zinder-explorer` and `zinder-compat-cipherscan` remain post-wallet-cutover
work. They are workspace code but are not built into, configured by, or started
by the first production release.

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

The current implementation and only release target is the
`rocksdb-single-host` topology. `postgres-scale-out` remains diagnostic-only;
its benchmarks preserve these ownership rules but do not create a production
migration commitment. [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
owns that boundary.

`zinder-ingest` opens the canonical schema-v4 RocksDB store as its only primary.
`zinder-projector` opens a canonical secondary and the wallet schema-v1 store
as its only primary. `zinder-compat-lightwalletd` opens both stores as
generation-specific secondaries, catches them up, validates their authenticated
fence and wallet digest, and atomically publishes the pair. Requests retain one
pair generation for their full lifetime, so refresh cannot mix stores beneath
an in-flight response.

Canonical and wallet paths are siblings, not nested aliases. Each secondary
path is process- and generation-specific. No release process opens the legacy
derive store, and no compatibility fallback may reconstruct wallet rows or
serve directly from a primary.

## Development Profile

Local composition starts the same ingest, projector, and compatibility
binaries with regtest paths. It must preserve distinct store ownership,
secondaries, exact-pair admission, readiness, and reorg behavior. Do not
introduce a generic `zinder-serve` process or a local primary-read shortcut.

## Deployment Topologies

### `rocksdb-single-host`

The production service set is:

```text
zinder-ingest              -> canonical RocksDB (primary)
                           -> IngestControl.WriterStatus / ChainEvents -> [ingest_control] gRPC
zinder-projector           -> canonical RocksDB (secondary)
                           -> wallet RocksDB (primary)
                           -> IngestControl.WriterStatus / ChainEvents
zinder-compat-lightwalletd -> canonical RocksDB (generation secondary)
                           -> wallet RocksDB (generation secondary)
                           -> exact pair -> WalletQueryApi -> CompactTxStreamer
```

Read replicas are colocated with the writer on one shared-filesystem host.
Cross-host RocksDB replicas are out of scope; see
[ADR-0003 §Out of Scope](../adrs/0003-canonical-storage-access-boundary.md#out-of-scope).
This topology is production-supported and has no Postgres dependency.

### `postgres-scale-out` diagnostic candidate

PostgreSQL is not an accepted production target until `rocksdb-single-host`
passes its lifecycle and performance certification. Benchmarking may continue
against the same fact-first schema, but it must not add runtime abstraction,
compatibility branches, or deployment claims to this release.

```text
zinder-ingest       -> Postgres canonical schema (one fenced active writer)
zinder-projector    -> Postgres wallet or explorer schema (fenced per projection)
compatibility edge  -> epoch-bound canonical + wallet read session -> WalletQueryApi
```

Role-scoped credentials, writer-generation fencing, replica-lag reporting,
request-scoped epoch reads, failover, and stale-writer rejection are part of
this topology's certification boundary. It becomes production-supported only
after those gates and the shared lifecycle targets pass.

## Anti-Patterns

- A single production daemon where query handlers call upstream node RPC directly.
- A query service that writes missing blocks on demand.
- A query service that opens the live canonical RocksDB database as **primary** in production. Secondary access is the production contract per ADR-0003.
- A compatibility adapter that opens either primary, mixes secondary
  generations, reconstructs missing wallet state, or calls upstream nodes.
- A generic `zinder-serve` boundary that hides which service owns ingestion,
  query, or compatibility behavior.
- A derived explorer index that is required for wallet sync.
- A migration that runs because a query process booted.
- A `common` crate that silently becomes the real application.
- A `wallet service` that does not actually implement a wallet.
