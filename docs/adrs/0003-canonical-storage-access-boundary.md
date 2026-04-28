# ADR-0003: Use an Epoch Read API for Canonical Storage Access

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Storage access and service boundaries |
| Related | [Storage backend](../architecture/storage-backend.md), [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md) |

## Context

`zinder-ingest` owns canonical RocksDB writes per [ADR-0001](0001-rocksdb-canonical-store.md), and storage byte contracts live behind the boundaries in [ADR-0002](0002-boundary-specific-serialization.md). That still leaves how production readers access canonical chain state.

The wrong default would let `zinder-query` open the live RocksDB database directly because it seems simpler. That would push schema knowledge, migration timing, reorg safety, compaction behavior, and snapshot semantics into the read-serving plane and would make the phrase "read-only canonical storage" sound like a service boundary when it is only a storage-engine capability.

The Zcash ecosystem and adjacent indexing systems point the same way: lightwalletd proves wallets should talk to a dedicated light-client service rather than open upstream-node or indexer storage directly. Reth models committed, reorged, and reverted chains explicitly. Substreams models undo signals for reversible chain segments. Sui separates source transport from deterministic checkpoint processing.

## Decision

Production Zinder uses a storage-owner access boundary:

1. `zinder-ingest` is the only production process that opens the live canonical RocksDB database as primary and owns every canonical write.
2. `zinder-query` reads canonical chain state through `ChainEpochReadApi`, backed by `SecondaryChainStore` per [ADR-0007](0007-multi-process-storage-access.md).
3. `zinder-query` must not open the live canonical database as primary or bypass `ChainEpochReadApi`.
4. Direct embedded reads are allowed only for local development composition, tests, offline repair tools, and immutable checkpoint readers.
5. `zinder-derive` consumes `ChainEventEnvelope` values or immutable snapshots and must not read live canonical storage.

The minimum production topology is:

```text
zinder-ingest
  -> canonical RocksDB primary
  -> ChainEpochReadApi
  -> ChainEventEnvelope

zinder-query
  -> canonical RocksDB secondary
  -> ChainEpochReadApi
  -> query-owned caches

zinder-derive
  -> ChainEventEnvelope or immutable snapshots
  -> derived storage
```

The local developer profile may run these in one process:

```text
zinder dev
  -> zinder-ingest
  -> zinder-query
  -> shared in-process ChainEpochReader
```

That local profile is composition over the same contracts. It does not introduce a second commit path, hidden migrations, query-time upstream-node fallback, or a local-only storage layout.

## Boundary Names

The storage-access boundary uses these names:

- `ChainEpochReadApi`: the internal service-to-service API exposing epoch-bound reads.
- `ChainEpochReader`: the in-process reader bound to one `ChainEpoch`. Primary reads may use a RocksDB snapshot; secondary reads are snapshotless and rely on epoch-bound visibility retention.
- `ChainEpochCommitted`: the event emitted after a new epoch becomes visible.
- `ChainRangeReverted`: the event emitted when a previously visible non-finalized range is invalidated.
- `commit_chain_epoch`: the write operation that atomically publishes a new visible epoch.

Post-commit values wrap into `ChainEvent` and `ChainEventEnvelope` per [Chain events](../architecture/chain-events.md).

Forbidden names: `StorageService`, `StoreManager`, `DbHelper`, `ReadOnlyStore`, "internal API" without naming `ChainEpochReadApi`, "canonical storage, read-only" in deployment diagrams.

## Rationale

The boundary keeps the database engine behind Zinder-owned contracts. That preserves the option to change the RocksDB layout, add derived read stores, add checkpoint exports, or introduce a fallback store without changing the wallet and explorer API surface.

Reorg handling is easier to reason about. A query starts from one `ChainEpoch`, receives a `ChainEpochReader`, and either finishes from that epoch or restarts. Readers do not assemble responses from a mix of old compact blocks, new tree state, and a newer tip pointer.

Migrations stay protected. Only `zinder-ingest migrate --plan` and `zinder-ingest migrate --apply` change canonical storage. Query startup rejects schema mismatches with a typed readiness cause instead of attempting repair.

Wallet privacy stays auditable. Wallet-facing APIs are reviewable as a single data plane. Direct table access by multiple services would make it harder to prevent shielded wallet scanning, leaked query interests, and accidental exposure of derived wallet state.

## Consequences

Positive:

- RocksDB details stay inside `zinder-store` and the storage owner.
- `zinder-query` scales by adding caches, query-owned read stores, or immutable checkpoint readers without becoming a second canonical writer.
- Migration ownership is unambiguous.
- Reorg events become explicit inputs to query and derived stores.
- Downstream developers integrate with domain APIs, not column families.

Negative:

- Production readers are coupled to the canonical RocksDB layout through `SecondaryChainStore`.
- Secondary readers require manual catchup, unique secondary paths, and snapshotless read invariants.
- Internal API and storage schema versioning are part of the operational surface.
- Tests cover service-to-service freshness, backpressure, and epoch consistency.

Neutral:

- RocksDB secondary instances are the production read transport (see [ADR-0007](0007-multi-process-storage-access.md)). They do not change ownership: only `zinder-ingest` opens primary.
- RocksDB checkpoints remain the preferred primitive for immutable fixtures, backup, and offline read replicas.
- A future high-scale deployment can replace `ChainEpochReadApi` with a query-owned read store without changing wallet APIs.

## Alternatives Considered

### Direct Read-Only RocksDB in `zinder-query`

Tempting because it avoids an internal read API, but it makes RocksDB layout, migration timing, and snapshot caveats part of the query service boundary. The accepted secondary path keeps the public boundary as `ChainEpochReadApi`, uses role-specific `SecondaryChainStore`, and accepts the RocksDB coupling explicitly.

### Standalone Storage Service

A dedicated storage service makes ownership explicit but adds another production process before the simpler `zinder-ingest` ownership model has been stressed. Reserved for the case where `ChainEpochReadApi` becomes a scaling or operational bottleneck.

### Checkpoint-Only Query Replicas

Checkpoints are excellent for fixtures, backups, and offline read replicas. They are not enough for the lowest-latency wallet path unless paired with a freshness model and near-tip event delivery.

## References

- RocksDB read-only and secondary instances: <https://github.com/facebook/rocksdb/wiki/Read-only-and-Secondary-instances>
- RocksDB checkpoints: <https://rocksdb.org/blog/2015/11/10/use-checkpoints-for-efficient-snapshots.html>
- Reth `ExExNotification`: <https://reth.rs/docs/reth_exex/enum.ExExNotification.html>
- Substreams reorg handling: <https://docs.substreams.dev/reference-material/sql/sql/reorg-handling>
- Sui gRPC streaming indexers: <https://blog.sui.io/grpc-real-time-streaming-indexing-sui/>
- Zcash lightwalletd: <https://github.com/zcash/lightwalletd>
- Zcash wallet threat model: <https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html>
