# ADR-0007: Mempool Topology and Retention

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Mempool surface, write-side topology, read-side proxy, retention |
| Related | [Chain events](../architecture/chain-events.md), [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0006](0006-ingest-control-transport-security.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Storage backend](../architecture/storage-backend.md), [Chain ingestion](../architecture/chain-ingestion.md), [Service operations](../architecture/service-operations.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

[ADR-0003](0003-canonical-storage-access-boundary.md) settles the writer / secondary-reader topology for canonical chain state. The mempool surface is structurally similar (writer-owned live state, reader-owned secondary access) but differs in three ways the chain-state design did not have to answer:

- A mempool transaction's lifetime is bounded by network behavior, not by chain commits. Retention is per-variant: a `Mined` event is interesting only until the wallet has resynced past the height; an `Invalidated` event is interesting until the wallet has decided whether to rebroadcast; an `Added` event is only interesting while the transaction is still in the mempool.
- The live `MempoolIndex` cannot be a column-family read. It is in-process state computed from a streaming source, and a secondary RocksDB reader cannot observe it without a control-plane handoff. Chain state has the inverse property: the visible epoch is precisely what the canonical store records.
- The lightwalletd compatibility contract requires `GetMempoolStream` to close on chain-tip change. The native `MempoolEvents` stream does not. The compat shim therefore needs both a mempool-event channel and a chain-tip-change channel from the same writer process.

The decisions to record here are:

1. The split between in-memory `MempoolIndex` (live state) and durable mempool event history (cursor-resumable history) and why the canonical store carries the latter but not the former.
2. The `IngestControlMempoolSurface` topology: secondary readers and the lightwalletd compat shim consume the writer's mempool through the same `IngestControl` endpoint that already serves chain events, instead of opening a second source connection.
3. The two-tier retention windows, their defaults, and the readiness causes they emit when retention is approaching exhaustion.
4. The `TxStatus::InMempool(MempoolEntry)` typed shape that carries the hydrated mempool entry, the chain epoch at first observation, the transparent overlay, and the precomputed compact-tx bytes; clients receive the typed status directly rather than parsing error strings.

## Decision

### Mempool live state is in-memory; mempool history is in RocksDB

`MempoolIndex` is owned by `zinder-ingest` as in-process state. It is a concurrent map keyed by `TransactionId` plus transparent overlays (output / spend lookups). It is not a column family. Crashes drop the live index; the next restart rebuilds it from the source's snapshot.

The canonical store persists durable mempool event history in its `mempool_event` column family. Every typed `Added` / `Invalidated` / `Mined` envelope is committed there before consumers see it. The retention floor is stored with the canonical control records, so cursor expiration is a single durable read rather than a column-family scan.

The split exists because the live index needs to answer transparent lookups in microseconds without a RocksDB round-trip, and because it can be reconstructed deterministically from a source snapshot at startup. The event log needs to answer cursor resume and rebroadcast detection with millisecond latency over hours of history, which RocksDB does well and an in-process ring buffer does not.

### Compatibility and native adapters reach the writer through `IngestControl`, not a second source connection

`zinder-compat-lightwalletd` and any native adapter that embeds `zinder-query` are secondary RocksDB readers per ADR-0003. The mempool live state is not in RocksDB, so the secondary readers cannot observe it directly. Two options were considered:

1. Each reader opens its own `MempoolSource` connection to the upstream node and maintains its own `MempoolIndex`.
2. Each reader proxies `MempoolSnapshot` and `MempoolEvents` through the writer's `IngestControl` gRPC endpoint, which also serves `WriterStatus` and the narrow `VisibleChainEvents` stream.

Zinder picks option 2. The decisive constraints are:

- Two `MempoolIndex` instances diverge under load. A wallet that hits compat for `GetMempoolStream` and `WalletQuery` for `transaction_by_id` must see a single mempool, not a per-process one. Source-of-truth duplication would make pending-transaction visibility depend on which read service handled the request.
- Operators already wire one privileged path from secondary readers to the writer (writer status). Adding mempool to that path costs no new transport, no new auth surface, no new firewall rules. ADR-0006's bearer token already covers `MempoolSnapshot` and `MempoolEvents`.
- One source connection upstream uses one mempool-stream slot on Zebra. A reader-per-process design multiplies that load by the number of read services.

The concrete bindings are:

- `IngestControlMempoolSurface` (in `services/zinder-compat-lightwalletd`) implements `MempoolSurface` over the `IngestControl.MempoolSnapshot` / `MempoolEvents` proxy methods.
- `WalletQueryGrpcAdapter::with_ingest_control_proxy` makes the same proxy available to native `WalletQuery` consumers.
- `spawn_ingest_control_tip_change_publisher` runs in the compat process and subscribes to `IngestControl.VisibleChainEvents`. It feeds a `TipChangeWatcher` so `LightwalletdGrpcAdapter::with_tip_change_watcher` can race the mempool-event stream against tip changes and close the gRPC stream on each best-block change. This restores the lightwalletd Go contract that Zodl's `sync` loop relies on without making the compat process open its own upstream node connection.

### Retention is two-tier with separately tunable windows

`MempoolEventRetentionConfig` carries:

- `mined_window`: how long a `Mined` envelope stays before pruning. Default: 60 minutes.
- `invalidated_window`: how long an `Invalidated` envelope stays. Default: 24 hours.
- An `Added` window is derived as `min(mined_window, invalidated_window)` because once an `Added` event ages past the shorter window, neither of its possible terminal events would still be retained, so the `Added` is no longer reachable through cursor resume.

The defaults are calibrated for two distinct consumer expectations:

- A wallet that comes back online after a brief disconnect (minutes) must be able to resume the stream and see the `Mined` event for the transaction it broadcast. 60 minutes covers normal mobile-wallet reconnect behavior plus margin.
- A wallet that wants to detect "my submitted transaction was rejected by the network" needs `Invalidated` events to outlive a typical user-driven retry cadence. 24 hours covers the case where a user submits, closes the app, returns the next morning, and expects to see whether the transaction is still pending or has been evicted.

`run_mempool_retention` runs the pruner. Its retention state feeds three readiness causes:

- `MempoolCursorAtRisk { oldest_retained_age_minutes, retention_minutes }` when the oldest retained event is approaching its window. The threshold is configurable; the default is to flip when 80 % of the shorter window has elapsed.
- `MempoolSourceUnavailable` when the source stream emits `MempoolStreamUnavailable { is_retryable: false }` or when the source has been down longer than the source-availability threshold.
- `MempoolHydrationLagging { recent_hydration_failures }` when `getrawtransaction` hydration falls behind the source's `Added` emission rate beyond the lag threshold.

These three causes are documented in [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness). The corresponding metrics (`zinder_mempool_events_pruned_total`, `zinder_mempool_event_retention_oldest_sequence`, `zinder_mempool_snapshot_age_seconds`) are documented in the [Metrics table](../architecture/service-operations.md#metrics).

Cursor expiration on read is a hard stop, not a warning. A consumer whose `from_cursor` is below `oldest_retained_mempool_event_sequence` receives `MempoolCursorExpired` carrying the current floor, mapped to gRPC `FailedPrecondition` with a `PreconditionFailure` detail. The consumer must resnapshot, not retry the same cursor.

### `TxStatus::InMempool` carries the hydrated entry

```rust
pub enum TxStatus {
    Mined { /* ... */ },
    InMempool(MempoolEntry),
    NotFound,
}
```

The `MempoolEntry` carries the hydrated transaction, the chain epoch at first observation, the transparent overlay, and the precomputed compact-tx bytes. The typed enum is the contract; `transaction_by_id` reports mempool presence by returning `InMempool`, not by an error string consumers parse. Typed transaction status is a product-correctness requirement.

## Consequences

### Operational

- One privileged path (`IngestControl`) serves writer status, visible chain events, mempool snapshots, and mempool events. Bearer-token auth from ADR-0006 covers every method.
- Operators tune mempool retention separately from chain-event retention. Default `mined_window = 60 minutes` and `invalidated_window = 24 hours` are shipped values; production deployments with longer wallet reconnect SLAs raise them and accept the larger `mempool_event` column-family footprint.
- A `mempool_*` readiness cause never fails a load balancer probe. They are drain-not-fail signals, identical posture to the existing `cursor_at_risk` cause.

## Alternatives Considered

### Each reader runs its own `MempoolSource`

Rejected. Two `MempoolIndex` instances diverge under load and reintroduce the multi-cache class of bug Zinder is structurally avoiding. It also multiplies upstream-node mempool-stream slot usage by the number of read processes.

### One global retention window

Rejected. `Mined` and `Invalidated` events have different shelf lives because they answer different consumer questions. A single window forces the operator to choose between under-serving rebroadcast detection (short window) and bloating the column family (long window).

### Persist the live `MempoolIndex` as a column family

Rejected. The live index needs microsecond-latency transparent-overlay lookups. RocksDB cannot match that. Persisting also means every transparent index change causes a commit, multiplying write amplification on a hot path that has no recovery requirement (the source snapshot reseeds it).

### Skip `TipChangeWatcher`; let consumers handle stream-end semantics

Rejected. The Android SDK tolerates arbitrary stream end and reconnects, but the lightwalletd Go contract specifically requires close-on-tip-change. Diverging from the published contract is a compatibility regression even if a particular consumer happens to tolerate it.

## Out of Scope

- Native gRPC TLS for `IngestControl.MempoolSnapshot` / `MempoolEvents`. ADR-0006 covers transport security; this ADR does not modify it.
- A second mempool source backend type. The two existing backends (`JsonRpcMempoolSource` polling, `ZebraIndexerMempoolSource` streaming) are the supported set; new backends would be a separate ADR.
- Shielded-by-address mempool queries. The privacy boundary forbids them; transparent-only is intentional and already encoded in [Wallet data plane](../architecture/wallet-data-plane.md).
- Mempool-broadcast endpoints beyond the existing `SendTransaction` path. Broadcast remains a `TransactionBroadcaster` boundary owned by `zinder-source`.
