# ADR-0003: Use Epoch-Bound Storage Access With RocksDB Secondaries

| Field | Value |
| ----- | ----- |
| Status | Superseded in part by [ADR-0035](0035-fact-first-storage-selection-and-lifecycle.md) |
| Product | Zinder |
| Domain | Storage access, service topology, reader freshness |
| Related | [Storage backend](../architecture/storage-backend.md), [Service boundaries](../architecture/service-boundaries.md), [Service operations](../architecture/service-operations.md) |

ADR-0035 supersedes this ADR's bundled projection, native `zinder-query`
runtime, and legacy backup decisions. The current topology uses independent
canonical and wallet stores, `zinder-projector` for wallet construction and
following, and `zinder-compat-lightwalletd` for request-scoped exact-pair
serving. The epoch-bound secondary and single-writer principles remain in
force.

## Context

`zinder-ingest` owns canonical RocksDB writes per [ADR-0001](0001-rocksdb-canonical-store.md), and storage byte contracts live behind [ADR-0002](0002-boundary-specific-serialization.md). Production readers still need a concrete way to read current canonical state without becoming writers, hiding schema upgrades, or assembling responses from mixed epochs.

The tempting default is to let `zinder-query` open the live database directly as a normal read-only store. That pushes RocksDB layout, schema timing, reorg retention, compaction behavior, and snapshot semantics into the read-serving plane. The service boundary would then be a storage-engine side effect instead of a Zinder-owned contract.

Production Zinder runs as one writer plus colocated readers: `zinder-ingest`, `zinder-query`, `zinder-compat-lightwalletd`, and optionally `zinder-client::LocalChainIndex` consumers. The architecture needs one answer for visibility, freshness, subscriptions, schema compatibility, and backup.

## Decision

Zinder uses **epoch-bound storage access** backed by **RocksDB secondary instances**.

1. `zinder-ingest` is the only production process that opens the canonical RocksDB database as primary.
2. `zinder-query`, `zinder-compat-lightwalletd`, and colocated `LocalChainIndex` consumers open the same primary store path as RocksDB secondaries through `SecondaryChainStore`.
3. Every chain-dependent query starts by resolving one `ChainEpoch` and reads every artifact through a `ChainEpochReadApi` view bound to that epoch.
4. Subscriptions do not rely on RocksDB secondary tailing. Chain and mempool subscriptions travel over the private `IngestControl` gRPC plane and are proxied by reader services where needed.
5. Direct embedded primary reads are allowed only for local development composition, tests, offline repair tools, and immutable checkpoint readers.

## Runtime Topology

```text
zinder-ingest
  -> PrimaryChainStore
  -> canonical RocksDB primary
  -> IngestControl.WriterStatus / ChainEvents / MempoolSnapshot / MempoolEvents

zinder-query
  -> SecondaryChainStore
  -> ChainEpochReadApi
  -> WalletQuery
  -> IngestControl proxy for writer-owned live streams

zinder-compat-lightwalletd
  -> SecondaryChainStore
  -> WalletQueryApi
  -> CompactTxStreamer translation

LocalChainIndex
  -> SecondaryChainStore for colocated reads
  -> explicit subscription endpoint for chain and mempool events
```

Each secondary uses a distinct `secondary_path`; sharing one secondary directory across processes is invalid. Secondary readers replay the writer's WAL and manifest by calling `try_catch_up_with_primary` on a configurable interval. The default catchup interval is 1,000 ms.

## Visibility Contract

`ChainEpoch` is the visibility boundary. A reader either sees the old epoch or the new epoch; it never sees a half-committed batch.

Primary readers may use RocksDB snapshots. Secondary readers are snapshotless because RocksDB secondary mode does not support snapshots. That constraint is acceptable because Zinder pins every request to a `ChainEpoch` and retains visibility rows needed by pinned epochs until retention can safely remove them.

The storage-access boundary uses these names:

- `PrimaryChainStore`: the only handle that may write canonical state.
- `SecondaryChainStore`: a read replica handle backed by RocksDB secondary mode.
- `ChainEpochReadApi`: the internal API exposing epoch-bound canonical reads.
- `ChainEpochReader`: an in-process reader bound to one `ChainEpoch`.
- `commit_chain_epoch`: the atomic publish operation that makes a new epoch visible.

Forbidden names: `StorageService`, `StoreManager`, `DbHelper`, `ReadOnlyStore`, or "canonical storage, read-only" as a deployment boundary.

## Freshness And Readiness

Readers compute lag from writer status, not from local inference. `zinder-ingest` exposes `IngestControl.WriterStatus` on a private endpoint. Each reader compares the writer's latest chain epoch with its current secondary-visible epoch after catchup.

Readiness behavior:

- Lag at or below `secondary_replica_lag_threshold_chain_epochs` is acceptable.
- Lag above the threshold reports `replica_lagging`.
- Writer status unavailable before any cached writer observation reports `writer_status_unavailable`.
- If the primary is offline, readers continue serving their last replayed epoch until lag exceeds the configured threshold.

The catchup loop is lazy on idle: when no WAL state changed and a fresh writer-status snapshot proves the reader is already current, the reader skips readiness recomputation until the heartbeat interval.

## Schema Compatibility

The store records `artifact_schema_version` in `storage_control`. Reader binaries compile in the highest artifact schema they can read.

- Reader max schema >= persisted schema: open succeeds.
- Reader max schema < persisted schema: open fails with `SchemaTooNew`, and the service reports `schema_mismatch`.

Rolling upgrade order across schema bumps:

1. Upgrade read replicas first.
2. Stop the primary.
3. Upgrade and restart the primary.
4. Readers catch up to the new schema after the primary publishes it.

Same-schema upgrades tolerate any process order.

## Subscription Transport

RocksDB secondaries do not deliver live subscription semantics. The private ingest-control gRPC plane owns stream delivery:

- `IngestControl.ChainEvents` carries canonical chain-event envelopes.
- `IngestControl.MempoolSnapshot` and `IngestControl.MempoolEvents` carry writer-owned mempool state.
- The library-only native query adapter may proxy subscription RPCs in embedded
  and test composition; the first fact-first production release exposes only
  the compatibility surface.
- `zinder-compat-lightwalletd` only exposes subscription-like behavior that exists in the vendored lightwalletd protocol.

Read-only RPCs are served directly from the local secondary store. Live writer-owned state crosses `IngestControl`.

## Backup (Superseded)

This ADR originally coupled canonical and derive checkpoints. That command is
deleted: it cannot authenticate the independent canonical schema-v4 and wallet
schema-v1 stores at one fence. ADR-0035 now requires a coherent bundle and
verified 10,000-block tail before any artifact can be admitted as a production
backup.

## Consequences

Positive:

- Canonical RocksDB has exactly one writer.
- Readers replay the writer's byte-level state instead of rebuilding projections.
- Query services can scale locally without owning schema migrations or upstream-node fallbacks.
- The default reader staleness budget is small and observable.
- Read paths and subscription paths are separated explicitly.
- Frozen RocksDB checkpoints remain useful for fixtures and diagnostics; the
  production backup decision is superseded by ADR-0035.

Tradeoffs:

- Readers remain coupled to the RocksDB layout. Schema-version checks and rolling-upgrade order bound that coupling.
- Read replicas are colocated by default. Cross-host replicas need a future design.
- Secondary readers cannot depend on RocksDB snapshots. Epoch-bound visibility is the contract instead.
- The writer's private control plane is an operational surface and must be secured for non-loopback deployments.

## Out of Scope

- Cross-host read replicas.
- gRPC-fronted `ChainEpochReadApi`.
- Query-owned projection stores fed by chain events.
- Standalone storage service.
- Online restore.
- Cross-network secondary access.

## Alternatives Considered

### Direct Read-Only RocksDB in Query

Rejected. It makes RocksDB schema details part of the query boundary and hides migration ownership.

### `ChainEpochReadApi` Over gRPC

Reserved. It is the scale escape valve for reader fleets that outgrow colocated RocksDB secondaries, but v1 does not pre-pay the extra process and network hop.

### Query-Owned Store Fed By Events

Reserved. It is useful for specialized read models, but it duplicates canonical state and makes reorg replay a reader concern.

### Standalone Storage Service

Rejected for v1. It makes ownership explicit but adds a new production process before the simpler writer-owned store model has been stressed.

## References

- RocksDB read-only and secondary instances: <https://github.com/facebook/rocksdb/wiki/Read-only-and-Secondary-instances>
- RocksDB checkpoints: <https://rocksdb.org/blog/2015/11/10/use-checkpoints-for-efficient-snapshots.html>
