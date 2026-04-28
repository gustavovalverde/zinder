# ADR-0007: Multi-Process Storage Access

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Storage runtime topology |
| Related | [Storage backend](../architecture/storage-backend.md), [Service boundaries](../architecture/service-boundaries.md), [Service operations](../architecture/service-operations.md), [Public interfaces](../architecture/public-interfaces.md), [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md) |

## Context

Production Zinder runs as a multi-process deployment: one `zinder-ingest` writer plus one or more colocated read replicas (`zinder-query`, `zinder-compat-lightwalletd`, embedded `LocalChainIndex` consumers). [ADR-0003](0003-canonical-storage-access-boundary.md) settles that `zinder-ingest` is the only process that opens the canonical RocksDB database for writes and that read replicas reach canonical state through `ChainEpochReadApi`. This ADR specifies the concrete mechanism.

Four candidates were considered:

1. **RocksDB secondary instances.** Readers open the same files as the primary in secondary mode and replay the WAL plus manifest on a configurable interval.
2. **`ChainEpochReadApi` over gRPC.** The writer exposes its `ChainEpochReadApi` as a gRPC service; readers query up.
3. **Query-owned store fed by canonical artifacts.** Each reader maintains its own RocksDB, fed from `ChainEvent` envelopes streamed by the writer.
4. **Standalone storage service.** A dedicated process between the writer and readers serves canonical reads.

Option 1 wins on three properties: storage cost stays at approximately 1x (secondaries share SST files), reader-side reorg-replay logic is unnecessary (the writer's atomic commit batches replay byte-for-byte through the WAL), and operational surface area is minimal (no event-bus middleware, no projection store, no upstream gRPC dependency for reads).

The two classical concerns about secondaries are handled as design constraints, not wished away: secondary instances do not support RocksDB snapshots, so epoch-bound reads must be correct without storage-engine snapshots; secondary instances also do not support tailing iterators, so subscription delivery travels over gRPC.

Options 2, 3, and 4 remain available for future special cases (offline tooling, derived plane, horizontally-scaled reader fleets) without blocking v1.

## Decision

### Storage transport: RocksDB secondary instances

`zinder-ingest` opens the canonical RocksDB store as **primary** through `PrimaryChainStore`. `zinder-query`, `zinder-compat-lightwalletd`, and `zinder-client::LocalChainIndex` open the same primary store path as **secondary** through `SecondaryChainStore`. Each secondary uses a distinct `secondary_path`; sharing one secondary directory across processes is invalid. Secondary readers replay the writer's WAL and manifest by calling `try_catch_up_with_primary` on a configurable interval. After each successful replay, all artifacts and the visible-epoch pointer for newly committed chain epochs are visible to the reader.

Multiple secondary readers against the same primary store path are allowed when each process has its own secondary path. Secondaries share the underlying SST files at the filesystem level, but each process consumes file descriptors and local metadata under its secondary path.

Cross-host secondary access (NFS, replicated block storage) is mechanically possible but is out of scope for v1.

### Lock and concurrency semantics

Exactly one primary may open a given store path at a time. The RocksDB `LOCK` file enforces this; a second primary open fails with a typed `StoreError::PrimaryAlreadyOpen { lock_path }` mapped to a process-startup error. The condition is unrecoverable until the holding primary releases the lock through clean shutdown or process termination.

Secondary readers do not acquire the primary lock and do not conflict with each other or the primary. If a secondary process crashes, its secondary path can be reused by that same process after restart.

If the primary process dies without releasing the lock cleanly, operators restart the primary and let the operating system and RocksDB lock handling determine whether the lock is still held. Operators do not manually remove `LOCK` unless a documented recovery procedure proves that no primary process still owns the database.

### Schema-version compatibility

The store's `storage_control` family records `artifact_schema_version`. Each reader binary compiles in `MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION`: the highest schema this binary understands. Open semantics:

- Reader's max ≥ store's persisted version: open succeeds.
- Reader's max < store's persisted version: open fails with `StoreError::SchemaTooNew { persisted, supported }` mapped to `ReadinessCause::SchemaMismatch`. The reader refuses to serve.

Rolling upgrade sequence (across schema bumps):

1. Upgrade and restart all read replicas first. They serve under the existing schema; their compiled-in max is now greater than or equal to the persisted version.
2. Stop the primary.
3. Upgrade the primary binary.
4. Start the primary. If the upgrade requires a migration (`zinder-ingest migrate --apply`), it runs explicitly per [Storage backend §Migrations](../architecture/storage-backend.md#migrations).
5. Once migration completes, the persisted schema may be greater. Readers (already upgraded) keep serving without restart.

Same-schema upgrades (binary changes without a schema bump) tolerate any process order.

### Catchup mechanism

Each reader process runs a background task that calls `db.try_catch_up_with_primary()` every `secondary_catchup_interval_ms`. The default is 250 ms. The interval is configurable per binary under `[storage]`.

The catchup loop is lazy on idle. When `try_catch_up_with_primary` returns `before == after` (no new WAL state replayed) **and** the previous `WriterStatus` snapshot's `latest_writer_chain_epoch_id` equals the reader's current `chain_epoch_id` **and** that snapshot was fetched within `WRITER_STATUS_HEARTBEAT_INTERVAL` (5 s), the reader skips the writer-status RPC and the readiness recompute. When any of those conditions fails, the reader runs the full path. The 5 s heartbeat ensures gauges, `_available` signals, and stale-snapshot recovery still happen even when nothing has changed. Idle reader cost is one writer-status gRPC every 5 s; on commit, catchup advances and the reader picks up the change on the next tick (within 250 ms).

Lag is measured as `(latest_writer_chain_epoch_id - this_reader_visible_chain_epoch_id)` after a catchup attempt. The `latest_writer_chain_epoch_id` comes from the private ingest-control endpoint (`[storage] ingest_control_addr` on readers); a secondary cannot infer writer progress it has not replayed yet. Lag is surfaced through readiness:

- `lag ≤ secondary_replica_lag_threshold_chain_epochs`: readiness reflects the reader's state otherwise.
- `lag > threshold`: `ReadinessCause::ReplicaLagging { lag_chain_epochs }`.
- writer-status unavailable before the reader has ever observed writer progress: `ReadinessCause::WriterStatusUnavailable`.

Default threshold: 4 chain epochs. At mainnet block rate with default commit batching, that is roughly five minutes of staleness. The threshold is configurable.

If the primary is offline or unreachable (for example, ingest is stopped for upgrade), `try_catch_up_with_primary` returns success with no advance. Lag accumulates. Readers continue serving from their last-replayed state until lag exceeds the threshold and `ReplicaLagging` flips. Once the primary returns, catchup resumes within one interval and lag drops below the threshold.

### Subscription transport: gRPC, with read-replica relay

RocksDB secondary readers cannot observe the writer's live commit loop directly. Subscriptions therefore travel over gRPC and read from the durable chain-event table.

Two endpoint families exist in a v1 deployment:

- **`zinder-ingest` exposes a private ingest-control gRPC port** (`[ingest.control] listen_addr`, default `127.0.0.1:9100`). It serves `IngestControl.WriterStatus`, the small RPC secondaries use to compute readiness lag, and `IngestControl.ChainEvents`, the writer-owned subscription stream. The endpoint is intended for colocated consumers (read replicas, `LocalChainIndex`-backed applications) and is not designed for public exposure.
- **Native chain-event subscriptions use the ingest private gRPC plane.** Mempool-event subscriptions use the same topology in M3.
- **`zinder-query` proxies native subscription RPCs** to the ingest subscription port. Native wallet clients connect to the query process; subscription bytes flow `ingest -> query -> wallet` over loopback.
- **`zinder-compat-lightwalletd` only proxies subscription-like RPCs that exist in the vendored lightwalletd protocol.** It does not expose `WalletQuery.ChainEvents`, because `CompactTxStreamer` has no equivalent method. Compatibility clients continue to poll latest-block metadata for chain changes.

Read-only RPCs (`LatestBlock`, `CompactBlockRange`, `Transaction`, `TreeState`, etc.) are served by the reader process directly from its secondary RocksDB.

`zinder-client::LocalChainIndex` follows the same pattern. Reads are served from the secondary RocksDB on the colocated host. Subscriptions travel over a gRPC connection to either the ingest subscription endpoint (skipping the proxy hop) or the colocated query process (taking the proxy hop). The consumer chooses through `LocalOpenOptions::subscription_endpoint`.

### Backup mechanism: RocksDB Checkpoint API

`zinder-ingest backup --to <path>` calls the RocksDB Checkpoint API (`rust_rocksdb::checkpoint::Checkpoint::new(&db)?.create_checkpoint(path)`). The result is a hardlinked, point-in-time checkpoint of the canonical store. Backup runs while the writer is live; secondaries are unaffected.

Restore is a documented operator procedure: stop all processes, replace the storage path with a checkpoint, start the primary. `zinder-ingest restore` is not part of v1; the operator copies checkpoint files manually.

Checkpoint readers (offline tooling, fixture capture) are a separate use case from the production read path. They open a checkpoint as primary or read-only and serve frozen state.

## Storage Layout for Multi-Process Reads

`zinder-store` exposes role-specific public handles so role mismatches fail at compile time:

- `PrimaryChainStore`: opened by `zinder-ingest`; the only handle that may write canonical state.
- `SecondaryChainStore`: opened by reader binaries; calls `try_catch_up` on a background loop and exposes snapshotless `ChainEpochReader` views.

Constructors:

- `PrimaryChainStore::open(path, options)`.
- `SecondaryChainStore::open(primary_path, secondary_path, options)`.

Errors at this boundary:

- `StoreError::PrimaryAlreadyOpen { lock_path: PathBuf }`.
- `StoreError::SchemaTooNew { persisted: u32, supported: u32 }`.
- `StoreError::SecondaryCatchupFailed { source: rocksdb::Error }` (private; mapped to `StorageUnavailable` at the boundary).

`SecondaryChainStore::try_catch_up` returns `SecondaryCatchupOutcome` carrying the visible epoch before and after catchup; service readiness combines that with ingest writer-status to compute lag.

`PrimaryChainStore::create_checkpoint(&self, path: &Path)` powers `zinder-ingest backup`.

`ChainEpochReader` is explicit about read posture. Primary readers may use RocksDB snapshots; secondary readers are snapshotless because RocksDB-secondary does not support snapshots. Visibility retention is epoch-bound; reorg replacement does not synchronously delete visibility rows that a previously pinned reader may still use.

## Configuration Schema

Reader-only knobs live under `[storage]`; the writer-only ingest-control listener is under `[ingest.control]`:

```toml
# zinder-ingest
[ingest.control]
listen_addr = "127.0.0.1:9100"

# zinder-query and zinder-compat-lightwalletd
[storage]
secondary_path = "/var/lib/zinder/query-secondary"
secondary_catchup_interval_ms = 250
secondary_replica_lag_threshold_chain_epochs = 4
ingest_control_addr = "http://127.0.0.1:9100"
```

Readiness gains two causes:

- `ReadinessCause::ReplicaLagging { lag_chain_epochs: u64 }`.
- `ReadinessCause::WriterStatusUnavailable`.

## Consequences

Positive:

- Canonical RocksDB has exactly one writer; readers replay the same byte-level state. No reorg-replay logic in readers, no schema duplication, no projection bugs.
- The default 250 ms catchup interval keeps reader staleness well under wallet UX thresholds. Lag is observable through typed readiness and Prometheus metrics.
- Idle reader cost is one writer-status gRPC every 5 s. Active-path lag is unchanged.
- Storage cost is approximately 1x. Secondary readers share SST files with the primary on the same filesystem.
- Operators run N processes (one writer plus N-1 readers) against one storage path.
- The schema-version compatibility contract makes the rolling upgrade order explicit and mechanical.
- Read and subscription paths are cleanly separated. Reads bypass gRPC entirely; subscriptions cross gRPC explicitly.
- `LocalChainIndex` is honestly named: reads are local (RocksDB-secondary on the same host), subscriptions are explicitly over the wire (loopback gRPC).

Negative:

- Readers are coupled to the writer's RocksDB layout. A change to column-family naming, key encoding, or envelope format affects both writer and reader binaries. The schema-version compatibility contract bounds the coupling.
- Read replicas are colocated by default. Cross-host replicas need shared storage and are out of scope for v1.
- The subscription path adds one proxy hop when wallets connect to a query or compat process. Roughly 1 ms of loopback overhead per envelope; negligible at mainnet block rate.
- Secondary readers cannot rely on RocksDB snapshots. Mitigation: every chain-dependent query pins to one `ChainEpoch` at the start of the request, and the storage layout retains visibility rows needed by pinned epochs until an explicit retention pass proves they are no longer needed.

Tradeoffs:

- The writer's private status/subscription plane is a network surface, not a process-internal channel. Operationally simple (loopback only, no public exposure, no TLS) but adds a port to the operator's mental model. The alternative (only `zinder-query` exposes subscriptions, never ingest) was rejected because it forces a query process to be running for any subscription consumer, including direct `LocalChainIndex` users that may want to skip the proxy hop.
- `ChainEpochReadApi`-over-gRPC remains a future scale escape valve if a deployment outgrows secondary instances (extreme write throughput, very large reader fleets, cross-host scale). v1 does not pre-pay that complexity.

## Out of Scope

- **Cross-host read replicas.** Mechanically possible with shared storage; deferred to a follow-up ADR when the deployment shape demands it.
- **gRPC-fronted `ChainEpochReadApi`.** Reserved as a future scale escape valve.
- **Query-owned event-fed projection store.** Reserved as a future scale escape valve.
- **Standalone storage service.**
- **Online restore.** v1 restore is "stop, replace, start"; operator-driven.
- **Cross-network secondary access.** Secondaries that serve a different network than the primary's are rejected by the per-network store anchor.

## References

- RocksDB read-only and secondary instances: <https://github.com/facebook/rocksdb/wiki/Read-only-and-Secondary-instances>
- RocksDB Checkpoint API: <https://github.com/facebook/rocksdb/wiki/Checkpoints>
