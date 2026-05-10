# ADR-0010: Mempool Topology and Retention

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Mempool surface, write-side topology, read-side proxy, retention |
| Related | [ADR-0005](0005-chain-event-cursor-sequence.md), [ADR-0007](0007-multi-process-storage-access.md), [ADR-0009](0009-ingest-control-transport-security.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Storage backend](../architecture/storage-backend.md), [Chain ingestion](../architecture/chain-ingestion.md), [Service operations](../architecture/service-operations.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

[ADR-0007](0007-multi-process-storage-access.md) settles the writer / secondary-reader topology for canonical chain state. The mempool surface is structurally similar (writer-owned live state, reader-owned secondary access) but differs in three ways the chain-state design did not have to answer:

- A mempool transaction's lifetime is bounded by network behavior, not by chain commits. Retention is per-variant: a `Mined` event is interesting only until the wallet has resynced past the height; an `Invalidated` event is interesting until the wallet has decided whether to rebroadcast; an `Added` event is only interesting while the transaction is still in the mempool.
- The live `MempoolIndex` cannot be a column-family read. It is in-process state computed from a streaming source, and a secondary RocksDB reader cannot observe it without a control-plane handoff. Chain state has the inverse property: the visible epoch is precisely what the canonical store records.
- The lightwalletd compatibility contract requires `GetMempoolStream` to close on chain-tip change. The native `MempoolEvents` stream does not. The compat shim therefore needs both a mempool-event channel and a chain-tip-change channel from the same writer process.

The decisions to record here are:

1. The split between in-memory `MempoolIndex` (live state) and persistent `MempoolEventLog` (history + cursor resume) and why the canonical store carries the latter but not the former.
2. The `IngestControlMempoolSurface` topology: secondary readers and the lightwalletd compat shim consume the writer's mempool through the same `IngestControl` endpoint that already serves chain events, instead of opening a second source connection.
3. The two-tier retention windows, their defaults, and the readiness causes they emit when retention is approaching exhaustion.
4. The `TxStatus::InMempool(MempoolEntry)` shape change that retires the prior string-matching workaround.

## Decision

### Mempool live state is in-memory; mempool history is in RocksDB

`MempoolIndex` is owned by `zinder-ingest` as in-process state. It is a concurrent map keyed by `TransactionId` plus transparent overlays (output / spend lookups). It is not a column family. Crashes drop the live index; the next restart rebuilds it from the source's snapshot.

`MempoolEventLog` is a `PrimaryChainStore` facade backed by the `mempool_event` column family. Every typed `Added` / `Invalidated` / `Mined` envelope passes through it before consumers see it, including the orchestrator's own writes. `oldest_retained_mempool_event_sequence` lives in `storage_control` so cursor expiration is a single durable read, not a column-family scan.

The split exists because the live index needs to answer transparent lookups in microseconds without a RocksDB round-trip, and because it can be reconstructed deterministically from a source snapshot at startup. The event log needs to answer cursor resume and rebroadcast detection with millisecond latency over hours of history, which RocksDB does well and an in-process ring buffer does not.

### Compat and query reach the writer through `IngestControl`, not a second source connection

`zinder-query` and `zinder-compat-lightwalletd` are secondary RocksDB readers per ADR-0007. The mempool live state is not in RocksDB, so the secondary readers cannot observe it directly. Two options were considered:

1. Each reader opens its own `MempoolSource` connection to the upstream node and maintains its own `MempoolIndex`.
2. Each reader proxies `MempoolSnapshot` and `MempoolEvents` through the writer's `IngestControl` gRPC endpoint, which already terminates `WriterStatus` and `ChainEvents`.

Zinder picks option 2. The decisive constraints are:

- Two `MempoolIndex` instances diverge under load. A wallet that hits compat for `GetMempoolStream` and `WalletQuery` for `transaction_by_id` must see a single mempool, not a per-process one. Source-of-truth duplication would recreate the multi-cache class of bug documented in the Zaino reference.
- Operators already wire one privileged path from secondary readers to the writer (writer status). Adding mempool to that path costs no new transport, no new auth surface, no new firewall rules. ADR-0009's bearer token already covers `MempoolSnapshot` and `MempoolEvents`.
- One source connection upstream uses one mempool-stream slot on Zebra. A reader-per-process design multiplies that load by the number of read services.

The concrete bindings are:

- `IngestControlMempoolSurface` (in `services/zinder-compat-lightwalletd`) implements `MempoolSurface` over the `IngestControl.MempoolSnapshot` / `MempoolEvents` proxy methods.
- `WalletQueryGrpcAdapter::with_ingest_control_proxy` makes the same proxy available to native `WalletQuery` consumers.
- `spawn_ingest_control_tip_change_publisher` runs in the compat process and subscribes to `IngestControl.ChainEvents`. It feeds a `TipChangeWatcher` so `LightwalletdGrpcAdapter::with_tip_change_watcher` can race the mempool-event stream against tip changes and close the gRPC stream on each best-block change. This restores the lightwalletd Go contract that Zashi's `sync` loop relies on without making the compat process open its own upstream node connection.

### Retention is two-tier with separately tunable windows

`MempoolEventRetentionConfig` carries:

- `mined_window`: how long a `Mined` envelope stays before pruning. Default: 60 minutes.
- `invalidated_window`: how long an `Invalidated` envelope stays. Default: 24 hours.
- An `Added` window is derived as `min(mined_window, invalidated_window)` because once an `Added` event ages past the shorter window, neither of its possible terminal events would still be retained, so the `Added` is no longer reachable through cursor resume.

The defaults are calibrated for two distinct consumer expectations:

- A wallet that comes back online after a brief disconnect (minutes) must be able to resume the stream and see the `Mined` event for the transaction it broadcast. 60 minutes covers normal mobile-wallet reconnect behavior plus margin.
- A wallet that wants to detect "my submitted transaction was rejected by the network" needs `Invalidated` events to outlive a typical user-driven retry cadence. 24 hours covers the case where a user submits, closes the app, returns the next morning, and expects to see whether the transaction is still pending or has been evicted.

`spawn_mempool_event_retention_task` runs the pruner. It emits three readiness causes:

- `MempoolCursorAtRisk { oldest_age_seconds, oldest_sequence }` when the oldest retained sequence is approaching its window. The threshold is configurable; the default is to flip when 80 % of the shorter window has elapsed.
- `MempoolSourceUnavailable` when the source stream emits `MempoolStreamUnavailable { is_retryable: false }` or when the source has been down longer than the source-availability threshold.
- `MempoolHydrationLagging { hydration_lag_seconds }` when `getrawtransaction` hydration falls behind the source's `Added` emission rate beyond the lag threshold.

These three causes are documented in [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness). The corresponding metrics (`zinder_mempool_events_pruned_total`, `zinder_mempool_event_retention_oldest_age_seconds`, `zinder_mempool_event_retention_oldest_sequence`, `zinder_mempool_snapshot_age_seconds`) are documented in the [Metrics table](../architecture/service-operations.md#metrics).

Cursor expiration on read is a hard stop, not a warning. A consumer whose `from_cursor` is below `oldest_retained_mempool_event_sequence` receives `MempoolCursorExpired` carrying the current floor, mapped to gRPC `FailedPrecondition` with a `PreconditionFailure` detail. The consumer must resnapshot, not retry the same cursor.

### `TxStatus::InMempool` carries the hydrated entry, not a string

Earlier consumers (notably Zallet) detected "this transaction is in the mempool" by string-matching the human-readable error returned for `transaction_by_id` when the canonical chain had no record. The current shape retires that workaround:

```rust
pub enum TxStatus {
    Mined { /* ... */ },
    InMempool(MempoolEntry),
    NotFound,
}
```

The `MempoolEntry` carries the hydrated transaction, the chain epoch at first observation, the transparent overlay, and the precomputed compact-tx bytes. Consumers no longer parse error strings; the type tells them everything `transaction_by_id` saw.

String-matching against `transaction_by_id` error text is not a stable contract. Zinder treats the typed `TransactionStatus` as required for product correctness, not deferrable to a major version.

## Consequences

### Operational

- One privileged path (`IngestControl`) terminates four streams: `WriterStatus`, `ChainEvents`, `MempoolSnapshot`, `MempoolEvents`. Bearer-token auth from ADR-0009 covers all four.
- Operators tune mempool retention separately from chain-event retention. Default `mined_window = 60 minutes` and `invalidated_window = 24 hours` are shipped values; production deployments with longer wallet reconnect SLAs raise them and accept the larger `mempool_event` column-family footprint.
- A `mempool_*` readiness cause never fails a load balancer probe. They are drain-not-fail signals, identical posture to the existing `cursor_at_risk` cause.

### Implementation

- `services/zinder-compat-lightwalletd/src/main.rs` (or its config layer) now owns one extra subscription: `spawn_ingest_control_tip_change_publisher`. It is mandatory for production lightwalletd compat behavior; tests may inject a `ScriptedTipChangeWatcher`.
- `crates/zinder-store/src/mempool_event.rs` and the retention worker share a contract: every prune call updates `oldest_retained_mempool_event_sequence` atomically with the column-family delete batch, otherwise a reader can race past the new floor and observe a partially-pruned tail.
- `MempoolIndex` rebuild on writer restart must finish before the writer signals `ready`. A reader that connects to a writer mid-rebuild sees an empty live mempool plus the durable `mempool_event` history; this is acceptable because the next source snapshot reseeds the index within seconds. Readers do not see inconsistent mempool state; they see "rebuilding."

### Testing

- Persistent pipeline tests live in `services/zinder-ingest/tests/integration/mempool_pipeline.rs`. They cover the snapshot+events delivery path, cursor resume, restart durability (`mempool_event_log_resumes_after_writer_restart`), and time-window pruning surfacing `MempoolCursorExpired` (`mempool_event_log_prunes_mined_under_short_retention`).
- Compat surface tests in `services/zinder-compat-lightwalletd/tests/integration/mempool_compat.rs` cover `lightwalletd_get_mempool_stream_closes_on_tip_change` against a `ScriptedTipChangeWatcher`.
- Live broadcast cycle tests in `services/zinder-ingest/tests/live/mempool_broadcast_cycle.rs` use `zinder_testkit::TransparentTestKey` to sign + broadcast a real v5 transparent transaction and observe it through the polling source. The reorg-out gate uses Zebra's `invalidateblock` JSON-RPC.
- Mainnet streaming-source soak runs against an operator-hosted Zebra; the CI matrix shape is pending per [ADR-0006 §Open mainnet infrastructure questions](0006-test-tiers-and-live-config.md#open-mainnet-infrastructure-questions). Local invocation is supported today via `require_live_mainnet()` plus the standard `ZINDER_NETWORK=zcash-mainnet` schema.

## Alternatives Considered

### Each reader runs its own `MempoolSource`

Rejected. Two `MempoolIndex` instances diverge under load and reintroduce the multi-cache class of bug Zinder is structurally avoiding. It also multiplies upstream-node mempool-stream slot usage by the number of read processes.

### One global retention window

Rejected. `Mined` and `Invalidated` events have different shelf lives because they answer different consumer questions. A single window forces the operator to choose between under-serving rebroadcast detection (short window) and bloating the column family (long window).

### Persist the live `MempoolIndex` as a column family

Rejected. The live index needs microsecond-latency transparent-overlay lookups. RocksDB cannot match that. Persisting also means every transparent index change causes a commit, multiplying write amplification on a hot path that has no recovery requirement (the source snapshot reseeds it).

### Skip `TipChangeWatcher`; let consumers handle stream-end semantics

Rejected. The Android SDK tolerates arbitrary stream end and reconnects, but the lightwalletd Go contract specifically requires close-on-tip-change. Diverging from the published contract is a compatibility regression even if a particular consumer happens to tolerate it.

## Shipped Surfaces

- `MempoolMinedEvent` carries `transaction_id`, `mined_height`, and `block_hash`
  on the wire and on the canonical `MempoolEvent::Mined` variant. The `block_hash`
  is source-driven enrichment: `MempoolSourceEvent::Mined` carries the block hash
  observed by the source (`UpstreamTransactionLookup::Mined { mined_height,
  block_hash }`), and the orchestrator passes the value through without a chain-
  store fallback lookup. Lifecycle consumers receive the full mined block identity
  in one cursor delivery.
- `WalletQuery.TransparentMempoolOutputsByAddress` and
  `WalletQuery.TransparentMempoolSpendByOutpoint` mirror the typed
  `ChainIndex` mempool point lookups onto the gRPC wire. Both proxy through
  `IngestControl` so secondary readers continue to share one mempool source.
  Capabilities `wallet.mempool.transparent_outputs_by_address_v1` and
  `wallet.mempool.transparent_spend_by_outpoint_v1` advertise the surface.

## Out of Scope

- Native gRPC TLS for `IngestControl.MempoolSnapshot` / `MempoolEvents`. ADR-0009 covers transport security; this ADR does not modify it.
- A second mempool source backend type. The two existing backends (`JsonRpcMempoolSource` polling, `ZebraIndexerMempoolSource` streaming) are the supported set; new backends would be a separate ADR.
- Shielded-by-address mempool queries. The privacy boundary forbids them; transparent-only is intentional and already encoded in [Wallet data plane](../architecture/wallet-data-plane.md).
- Mempool-broadcast endpoints beyond the existing `SendTransaction` path. Broadcast remains a `TransactionBroadcaster` boundary owned by `zinder-source`.
