# Storage Backend

The storage backend is the contract between chain ingestion, epoch-bound readers, migrations, and operational recovery. It is not a public table schema.

Event and reorg semantics live in [Chain events](chain-events.md).

## Ownership

`zinder-ingest` owns the live canonical RocksDB database as the only primary writer. Production readers (`zinder-query`, `zinder-compat-lightwalletd`, `zinder-client::LocalChainIndex`) reach the same store through `SecondaryChainStore`, which implements `ChainEpochReadApi` over a RocksDB secondary instance. The full topology, catchup mechanism, lock semantics, and rolling-upgrade order live in [ADR-0007](../adrs/0007-multi-process-storage-access.md); this document does not restate them.

```text
zinder-ingest    -> RocksDB primary -> ChainEpochReadApi -> ChainEvent
zinder-query     -> RocksDB secondary -> ChainEpochReadApi -> WalletQueryApi
zinder-derive    -> ChainEventEnvelope -> derived storage
```

Direct embedded reads outside that contract are allowed only for `zinder dev` composition, unit and integration tests, offline repair tools, and immutable RocksDB checkpoint readers.

## Crate Responsibilities

`zinder-store` owns:

- Fixed `StoreKey` layouts.
- `ArtifactEnvelopeHeaderV1` parsing and validation.
- RocksDB column-family layout.
- `PrimaryChainStore`, `SecondaryChainStore`, `ChainEpochReader`, and domain store traits.
- Storage-control records, including the store network anchor.
- Migration planning and application.
- Checkpoint creation and fixture capture.

`zinder-store` must not expose RocksDB handles as public API. Public callers use domain contracts.

## Core Contracts

`ChainEpoch` is the visible consistency boundary. Required fields:

- `network`
- `tip_height`
- `tip_hash`
- `finalized_height`
- `finalized_hash`
- `artifact_schema_version`
- `tip_metadata`
- `created_at`

`tip_metadata` is chain-derived state at the visible tip. It carries
`sapling_commitment_tree_size` and `orchard_commitment_tree_size` so wallet hot
paths can derive completed subtree counts without decoding compact-block
payloads.

`created_at` is wall-clock diagnostic metadata. It is allowed to repeat or move
backward if system time changes; use `ChainEpochId` and chain-event sequence
values for monotonic ordering.

`ChainEpochReader` is an in-process read view pinned to one `ChainEpoch`. It must not merge data from multiple epochs. Primary readers may use RocksDB snapshots; secondary readers are snapshotless because RocksDB-secondary does not support snapshots.

`ChainEpochReadApi` is the internal service-to-service read API. It returns epoch-bound data to `zinder-query` without exposing RocksDB layout.

`commit_chain_epoch` is the only operation that makes a new epoch visible. It must write all required artifacts and the visible epoch pointer atomically.

## Storage Families

The first RocksDB layout should use separate column families only when tuning, iteration, or migration cadence justifies the split. Initial durable families:

| Family | Purpose |
| ------ | ------- |
| `storage_control` | Visible epoch pointer, event sequence pointer, cursor secret, schema version, store identity, and network anchor. Also holds `oldest_retained_chain_event_sequence` for M2 and reserves `oldest_retained_mempool_event_sequence` for M3. |
| `chain_epoch` | Epoch metadata, including `ChainTipMetadata` |
| `finalized_block` | Finalized block metadata and links |
| `compact_block` | Protobuf-compatible compact block artifact envelopes |
| `tree_state` | Sapling and Orchard tree state metadata needed by wallet APIs |
| `transaction` | Transaction lookup records required by wallet and explorer APIs |
| `reorg_window` | Visibility index for epoch-bound artifact overlays and replaceable non-finalized links |
| `chain_event` | Durable chain-event stream envelopes; retained per [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) (default 168 hours, time-windowed pruning) |
| `mempool_event` | M3 durable mempool-event log per [M3 Mempool](../specs/m3-mempool.md); retained per kind after M3 lands (default 60 minutes for `Mined`, 24 hours for `Invalidated`) |

Mempool state is split between in-memory and persistent storage in M3 per [M3 Mempool](../specs/m3-mempool.md). The live `MempoolIndex` lives in `zinder-ingest` as in-process state, not in canonical RocksDB. The `mempool_event` column family persists the typed event log for retention-dependent queries (rebroadcast detection, audit) and cursor resume on `WalletQuery.MempoolEvents`. Reads from the mempool event log go through `MempoolEventReadApi`, parallel to but distinct from `ChainEpochReadApi`; live snapshots and live stream tailing still require the ingest-owned private control surface because secondary RocksDB readers cannot observe the live in-process index. Mempool events do not participate in `commit_ingest_batch`; they are written by `zinder-ingest` as each `MempoolSourceEvent` arrives.

## Visibility Index Lifecycle

The `reorg_window` family is a visibility index, not a guarantee that storage remains bounded to the configured reorg depth. Commits retain historical `(network, height, epoch)` and `(network, transaction, epoch)` visibility rows after those rows stop being the latest visible branch. This is required for snapshotless secondary readers pinned to an older `ChainEpoch`.

Artifact families and the visibility index use disjoint `StoreKey` kind bytes
even though RocksDB column families already isolate them. This keeps key dumps,
repair tools, future column-family migrations, and agent-assisted debugging from
depending on implicit column-family context to disambiguate byte prefixes.

The `reorg_window` column family is opened with a visibility-prefix extractor so
height, transaction, and subtree-root visibility seeks can use prefix bloom
filtering. Other column families stay on default options until a measured access
pattern justifies table-specific tuning.

Readers must revalidate branch identity before returning data from a visibility lookup. For example, transaction lookup can find an older same-transaction-id row from a reorged branch, so the reader checks that the artifact's block hash still matches the visible block at that height before returning it.

Production storage needs an explicit lifecycle for stale visibility rows before mainnet-scale ingest. Reorg replacement must not synchronously delete visibility rows for the replaced non-finalized range, because a secondary reader may have pinned the previous `ChainEpoch` without a RocksDB snapshot. Stale visibility rows are pruned only by an explicit retention pass that can prove no active reader, retained event cursor, or configured replay window can still need them.

## Commit Protocol

The full ingest pipeline lives in [Chain ingestion §Operation Shape](chain-ingestion.md#operation-shape). The storage-level invariant: `commit_chain_epoch` writes every artifact, the event envelope, the event sequence pointer, store metadata, and the visible epoch pointer in one RocksDB `WriteBatch`. Readers observe either the previous epoch or the new epoch; a half-committed epoch is a correctness bug.

Required steps inside `commit_chain_epoch`:

1. Validate block links, compact block artifacts, transaction references, tree metadata, and reorg-window metadata.
2. Serialize the read-validate-write window for the visible epoch pointer and event sequence pointer, or use an equivalent compare-and-swap write fence.
3. Build the single `WriteBatch` covering artifacts, replaced non-finalized state, event envelope, sequence pointer, store metadata, and visible-epoch pointer.
4. Commit with the configured durability policy.
5. Return the committed epoch and envelope only after the batch succeeds.
6. Leave the previous visible epoch intact if the batch fails.

## Read Protocol

Every chain-dependent query starts by resolving one `ChainEpoch`.

```text
resolve_chain_epoch
  -> create ChainEpochReader
  -> read artifacts by height, hash, or cursor
  -> return response tagged with epoch metadata
```

If an artifact required by the response is missing, the reader returns `ArtifactUnavailable`. It does not fetch from the upstream node and build a one-off response.

Long-running range reads may finish from their starting epoch even if a newer epoch becomes visible. If the request requires the latest tip, the query layer may restart from a newer epoch, but it must not mix both epochs in one response.

## Reorg Protocol

Reorg semantics and event vocabulary live in [Chain events](chain-events.md). At the storage layer:

- `commit_chain_epoch` updates the affected `reorg_window` visibility records inside the same atomic `WriteBatch`.
- The replacement event (`ChainEvent::ChainReorged`) is persisted before the visible epoch advances; readers see consistent state.
- Reorgs beyond the configured window fail closed with `ReorgWindowExceeded` and require operator intervention.

## Migrations

Migrations are explicit and owned by `zinder-ingest`.

Allowed commands:

- `zinder-ingest migrate --plan`
- `zinder-ingest migrate --apply`
- `zinder-ingest start --require-schema <version>`

`zinder-query` must not migrate canonical storage. On schema mismatch it reports `SchemaMismatch` and fails readiness.

Major migrations may use a shadow store:

```text
plan migration
  -> build shadow store
  -> replay canonical artifacts
  -> validate shadow store
  -> promote shadow store
  -> checkpoint old store
```

Promotion must be explicit and observable. Partial migration state must be visible through readiness and metrics.

Migration planning should use a version graph, not a single linear ladder. Each artifact family records its schema fingerprint, and `StoreMigrator` computes an explicit path from the current store identity to the requested target. If multiple paths exist, the plan must name the selected path before `--apply` mutates data.

This keeps small-footprint or experimental storage layouts additive. A new artifact family or storage major should not require editing one global enum that knows every historical version.

## Checkpoints and Backups

RocksDB checkpoints are used for backups (`zinder-ingest backup --to <path>`), fixture capture, offline repair, and immutable analytics replicas. Restore is "stop, replace, start" (operator procedure, no online restore in v1).

Checkpoint readers must open a documented manifest and validate store identity, network, schema versions, and visible epoch before serving data. They serve frozen snapshots; production read replicas instead open the live store as RocksDB-secondary per [ADR-0007](../adrs/0007-multi-process-storage-access.md) and replay the writer's WAL.

## Multi-Process Operations

The primary/secondary contract is in [ADR-0007](../adrs/0007-multi-process-storage-access.md): one writer per store path, process-unique `secondary_path`, 250 ms catchup default, schema-version one-directional compatibility, gRPC-only subscription delivery. Storage code follows that ADR; this document does not duplicate the operational details.

## Storage Metrics

Readiness causes and operational metrics are owned by [Service operations](service-operations.md). Storage-specific metrics:

- Current `ChainEpoch` height and hash.
- Finalized height and hash.
- Commit latency. `zinder-ingest` records
  `zinder_ingest_commit_duration_seconds`; `zinder-store` records
  `zinder_store_write_batch_duration_seconds`.
- Write batch size. The baseline metrics are
  `zinder_store_write_batch_rows_total` and
  `zinder_store_write_batch_bytes_total`.
- RocksDB compaction latency.
- RocksDB block cache usage.
- Curated RocksDB property gauges through `zinder_store_rocksdb_property`,
  including live data size, SST size, memtable size, table-reader memory,
  pending compaction bytes, and running compaction count.
- RocksDB read latency through `zinder_store_read_duration_seconds`, labeled by
  operation, column family, and status.
- Visibility-index seek count through `zinder_store_visibility_seek_total`,
  labeled by artifact family. This identifies fanout-heavy wallet scans before
  adding new indexes or caches.
- Checkpoint age.
- Migration phase.
- Reorg depth.
- `ChainEpochReadApi` request latency and error count.

## Error Surface

Use typed errors at service boundaries:

- `StorageUnavailable`
- `EntropyUnavailable`
- `SchemaMismatch`
- `EpochNotFound`
- `ArtifactUnavailable`
- `ReorgWindowExceeded`
- `InvalidChainStoreOptions`
- `MigrationRequired`
- `CheckpointUnavailable`
- `NodeUnavailable`

Internal storage errors map to service errors at the boundary:

| Internal storage error | Service/API error |
| ---------------------- | ----------------- |
| `NoVisibleChainEpoch` | `EpochNotFound` |
| `ChainEpochMissing` | `EpochNotFound` |
| `ArtifactMissing` | `ArtifactUnavailable` |
| `ArtifactCorrupt` | `ArtifactUnavailable` with corruption detail |
| `ArtifactPayloadTooLarge` | `ArtifactUnavailable` or request validation failure, depending on boundary |
| `ChainEpochConflict` | `StorageUnavailable` or `SchemaMismatch`, depending on cause |
| `ChainEpochNetworkMismatch` | `SchemaMismatch` |
| `SchemaTooNew` | `SchemaMismatch` |
| `PrimaryAlreadyOpen` | Startup validation failure |
| `SecondaryCatchupFailed` | `StorageUnavailable` or `ReplicaLagging`, depending on retry policy |
| `ReorgWindowExceeded` | `ReorgWindowExceeded` |
| `ChainEventSequenceOverflow` | `StorageUnavailable` |
| `InvalidChainStoreOptions` | Startup validation failure |
| `EntropyUnavailable` | Startup validation failure or `StorageUnavailable`, depending on boundary |
| `CheckpointUnavailable` | `CheckpointUnavailable` |

Avoid catch-all errors in public boundaries. Internal adapter errors may be wrapped, but the boundary error must preserve the operator action.

## Prototype Evidence Checklist

Production storage code should not be treated as ready until the prototype proves:

1. Real Zcash fixture replay from Zebra or curated mainnet artifacts.
2. `GetBlockRange` P50, P99, and P99.9 under concurrent ingest.
3. Crash recovery during artifact writes, visible pointer writes, compaction, and reorg replacement.
4. Recovery to the last complete `ChainEpoch` or a typed fail-closed error.
5. Query refusal on schema mismatch.
6. Migration plan/apply/resume behavior.
7. Checkpoint creation and checkpoint reader validation.
8. Compact block raw-byte serving or measured decode/re-encode fallback.
9. Deletion and rebuild of query-owned caches without canonical storage changes.

## Module Naming Guidance

Future implementation modules should be named by storage-owned concepts:

- `chain_store`
- `chain_epoch`
- `chain_epoch_reader`
- `chain_event`
- `block_artifact`
- `transaction_artifact`
- `tree_state`
- `reorg_window`
- `store_migration`
- `store_error`
- `store_key`
- `artifact_envelope`
- `stream_cursor`

Avoid generic modules such as `common`, `shared`, `helpers`, `manager`, or `service`. Use a RocksDB-specific module only for the private adapter layer; do not let RocksDB vocabulary leak into public contracts.
