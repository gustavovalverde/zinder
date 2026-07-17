# Storage Backend

The storage backend is the contract between chain ingestion, epoch-bound readers, migrations, and operational recovery. It is not a public table schema.

This document describes the current `rocksdb-single-host` implementation.
[ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md) also
accepts a `postgres-scale-out` topology, which remains a migration target until
its concrete storage and lifecycle gates pass.

Event and reorg semantics live in [Chain events](chain-events.md).

## Ownership

`zinder-ingest` owns the live canonical RocksDB database as the only primary writer. Production readers (`zinder-query`, `zinder-compat-lightwalletd`, `zinder-client::LocalChainIndex`) reach the same store through `SecondaryChainStore`, which implements `ChainEpochReadApi` over a RocksDB secondary instance. The full topology, catchup mechanism, lock semantics, and rolling-upgrade order live in [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md).

```text
zinder-ingest    -> RocksDB primary -> ChainEpochReadApi -> ChainEvent
zinder-query     -> RocksDB secondary -> ChainEpochReadApi -> WalletQueryApi
zinder-explorer    -> ChainEventEnvelope -> derived storage
```

Direct embedded reads outside that contract are allowed only for `zinder dev` composition, unit and integration tests, offline repair tools, and immutable RocksDB checkpoint readers.

## Crate Responsibilities

`zinder-store` owns:

- Fixed `StoreKey` layouts.
- `ArtifactEnvelopeHeaderV1` parsing and validation.
- RocksDB column-family layout.
- `PrimaryChainStore`, `SecondaryChainStore`, `ChainEpochReader`, and domain store traits.
- Storage-control records, including the store network anchor.
- Schema-version validation on open.
- Checkpoint creation and fixture capture.
- The bounded RocksDB open path, resource budget, and shared block-table factory. [ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md) records the architectural invariants and the operator-tunable surface; both the canonical store and the derive store route through the bounded open path so the bulk-catchup-OOM trap can only be reopened by changing that path.

`zinder-store` must not expose RocksDB handles as public API. Public callers use domain contracts.

## Core Contracts

`ChainEpoch` is the visible consistency boundary. Required fields:

- `network`
- `tip_height`
- `tip_hash`
- `safe_tip_height`
- `safe_tip_hash`
- `artifact_schema_version`
- `tip_metadata`
- `created_at`

`tip_metadata` is chain-derived state at the visible tip. It carries
`sapling_commitment_tree_size`, `orchard_commitment_tree_size`, and
`ironwood_commitment_tree_size` so wallet hot paths can derive completed subtree
counts without decoding compact-block payloads.

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
| `storage_control` | Visible epoch pointer, event sequence pointer, cursor secret, schema version, store identity, and network anchor. Also holds `oldest_retained_chain_event_sequence` and `oldest_retained_mempool_event_sequence`. |
| `chain_epoch` | Epoch metadata, including `ChainTipMetadata` |
| `block_header` | Canonical block-header facts and links |
| `block_replay` | Versioned, reversible, block-ordered semantic `CanonicalBlockFacts` envelopes. One envelope is required for every committed header and shares the safe-block visibility index. Retention-dependent raw block and transaction blobs remain outside this family. |
| `compact_block` | Protobuf-compatible compact block artifact envelopes |
| `tree_state` | Sapling and Orchard tree state metadata needed by wallet APIs |
| `transaction` | Transaction lookup records required by wallet and explorer APIs |
| `transaction_intrinsic_value_balances` | Signed Sprout, Sapling, Orchard, and Ironwood balances parsed from one canonical transaction. Artifact schema 19 requires one row for every transaction represented by the replay envelope; a store below that floor is refused at open and rebuilt from genesis. |
| `final_note_commitment_roots` | Optional post-block Sapling, Orchard, and Ironwood roots with explicit historical enrichment coverage; introduced by artifact schema 14 |
| `block_value_pool_balances` | Optional post-block cumulative value-pool balances bound to exact block identity and time; introduced by artifact schema 16 |
| `displaced_block` and indexes | Writer-owned archive keyed by displaced block hash and observation order; capture begins at the schema-17 activation record and is retained permanently |
| `address_output_index` | Reorg-safe current projection of unspent transparent outputs keyed `(network, address_script_hash, height, outpoint)`. Rows derive from `transparent_outputs_by_outpoint` at commit; spends hide rows at read time inside the reorg window, and transparent-retention maintenance deletes finalized-spent rows |
| `transparent_output` | Exact canonical `(network, outpoint)` projection for transparent-output resolution hot paths. Unspent rows are retained forever for prevout resolution; finalized-spent rows are deleted by transparent-retention maintenance |
| `transparent_output_block_index` | Block-local transparent outpoint lists used to bound current-projection repair during reorg replacement |
| `transparent_spend_fact` | Exact canonical `(network, spent_outpoint)` projection for resolved transparent spend facts. Finalized rows are deleted by transparent-retention maintenance |
| `transparent_spend_fact_block_index` | Durable block-local input set and resolved spend facts used for finalized derive replay, current spend-projection repair during reorg replacement, and transparent-retention maintenance. Point rows may be deleted; these replay records are retained |
| `block_hash_index` | Best-chain `(network, block_hash) -> (height, source_chain_epoch_id)` resolver written on every safe-tip block commit. Monotonic: reorged-out hashes are filtered at read time and never deleted, so the family grows roughly one row per finalized block (~50K rows per year on mainnet). A future retention pass may prune rows older than the reorg window once an active-reader proof exists. |
| `reorg_window` | Visibility index for epoch-bound artifact overlays and replaceable links within the reorg window |
| `chain_event` | Durable chain-event stream envelopes; retained per [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) (default 168 hours, time-windowed pruning) |
| `mempool_event` | Durable mempool-event log per [ADR-0007](../adrs/0007-mempool-topology-and-retention.md); retained per kind (default 60 minutes for `Mined`, 24 hours for `Invalidated`, derived shorter window for `Added`) |

Canonical artifact schema and derive-consumer schema are separate version domains. Canonical schemas 13 through 19 describe facts written or enriched by the single canonical writer. Schema 18 and store schema 13 make each block's observed transparent input set and resolved spend facts durable. Schema 19 and store schema 14 add a complete reversible semantic replay envelope for every committed block. Older stores are refused and require a genesis rebuild because facts omitted by their persisted schema cannot be reconstructed safely. Each derive consumer independently versions its own rows and publishes a projection checkpoint and coverage. Neither version can be used as a substitute for the other.

The displaced-block archive is intentionally permanent in this release. There is no retention knob and no pruning path. This preserves hash-addressed post-reorg evidence and makes coverage monotonic, but the archive and its indexes grow with accepted replacements. Any future bounded policy requires its own ADR covering cursor invalidation, coverage contraction, secondary-reader safety, and checkpoint restore behavior.

Mempool state is split between in-memory and persistent storage. The live `MempoolIndex` lives in `zinder-ingest` as in-process state, not in canonical RocksDB. The `mempool_event` column family persists the typed event log for retention-dependent queries (rebroadcast detection, audit) and cursor resume on `WalletQuery.MempoolEvents`. Reads from the mempool event log go through `MempoolEventReadApi`, parallel to but distinct from `ChainEpochReadApi`; live snapshots and live stream tailing still require the ingest-owned private control surface because secondary RocksDB readers cannot observe the live in-process index. Mempool events do not participate in `commit_ingest_batch`; they are written by `zinder-ingest` as each `MempoolSourceEvent` arrives.

## Visibility Index Lifecycle

The `reorg_window` family is a visibility index, not a guarantee that storage remains bounded to the configured reorg depth. Commits retain historical `(network, height, epoch)` and `(network, transaction, epoch)` visibility rows after those rows stop being the latest visible branch. This is required for snapshotless secondary readers pinned to an older `ChainEpoch`.

Artifact families and the visibility index use disjoint `StoreKey` kind bytes
even though RocksDB column families already isolate them. This keeps key dumps,
repair tools, future column-family migrations, and agent-assisted debugging from
depending on implicit column-family context to disambiguate byte prefixes.

The `reorg_window` column family is opened with a visibility-prefix extractor so
height, transaction, and subtree-root visibility seeks can use prefix bloom
filtering. The `transparent_output` column family carries a full 10-bits-per-key
whole-key Bloom filter and RocksDB's memtable batch-lookup optimization, matching
its hot access shape of sorted batched point lookups over `(network, outpoint)`
keys. Its filter blocks are charged to the same shared bounded block cache as
every other block. Bloom filters attach only to newly written SST files, so an
existing store acquires them as compaction rewrites its files; no migration or
volume wipe is required. Other column families stay on default options until a
measured access pattern justifies table-specific tuning.

Canonical replay uses a separate bounded read shape. A
`BlockReplayBatchRequest` supplies `start_height` and nonzero `max_blocks`;
the store rejects limits above 256 blocks. It resolves source epochs with one
ordered `reorg_window` scan, then reads the replay payloads with one
`block_replay` `multi_get`. A start beyond the pinned visible tip returns
an empty batch, a crossing batch stops at the tip, and any missing or corrupt
row fails the entire batch.

Readers must revalidate branch identity before returning data from a visibility lookup. For example, transaction lookup can find an older same-transaction-id row from a reorged branch, so the reader checks that the artifact's block hash still matches the visible block at that height before returning it.

Production storage needs an explicit lifecycle for stale visibility rows before mainnet-scale ingest. Reorg replacement must not synchronously delete visibility rows for the replaced range within the reorg window, because a secondary reader may have pinned the previous `ChainEpoch` without a RocksDB snapshot. Stale visibility rows are pruned only by an explicit retention pass that can prove no active reader, retained event cursor, or configured replay window can still need them.

The transparent projections (`address_output_index`, `transparent_output`,
`transparent_spend_fact`) follow one retention invariant: a projection row may
be physically deleted only when no commit the store will ever accept can make
it live again. `validate_reorg_window_change` rejects any `Replace` below
`max(safe_tip + 1, tip - window + 1)`, so a spend at or below
`safe_tip_height` is irreversible: that is the deletion boundary. A dedicated
ingest maintenance worker scans `transparent_spend_fact_block_index` only
after canonical ingest and derive replay are both caught up. Canonical commits
never scan or delete a historical retention backlog, so safe-tip advancement
cannot be delayed by maintenance. The maintenance ceiling is clamped to the persisted
`transparent_retention_release_height`, the durable-consumer floor
`zinder-ingest` publishes as the transparent-outpoint-spend projection
advances, so a spend fact is deleted only after its spender identity is
durably recorded elsewhere; a sweep that deletes at least one fact also
advances the `transparent_retention_deleted_through_height` marker in the same
maintenance batch ([ADR-0029](../adrs/0029-durable-transparent-outpoint-spend-projection.md)).
One pass sweeps at most `retention_sweep_max_heights_per_pass` heights
(default 1000) and stops after the first fully-swept height that reaches
`retention_sweep_max_outpoints_per_pass` outpoints (default 10000),
whichever budget hits first. The outpoint budget bounds the delete batch held
in memory through transaction-dense eras; the height cap bounds the scan
through sparse ones. A height is never split across passes, and the swept
marker advances only to the last fully-swept height, so when the release floor
jumps far ahead of the marker (a store rebuilt with derive paused, then
un-paused at tip) the backlog drains across later passes and no pass claims
unswept ground. A sweep that advances the marker or leaves a backlog logs a
`retention_sweep_advanced` event; a zero-work sweep stays silent. The sweep
scan reads each block-local replay row once instead of looking up every
outpoint point row. It runs before the maintenance write's control-lock critical section so control-plane
reads (readiness, writer status, event history) stay responsive through a
chunk; only the marker puts and deletes ride the locked `WriteBatch`, and the
locked write re-reads the swept marker first, discarding a precomputed sweep
whose starting marker no longer matches.
In-window reverted spends need no machinery at all: their rows were never
deleted, and the existing spend-fact repair un-hides them. A reader pinned to an epoch older than the reorg window
can omit rows whose spends finalized after that epoch; this is the same
fidelity erosion the outpoint-keyed projections already accept.

## Commit Protocol

The full ingest pipeline lives in [Chain ingestion §Operation Shape](chain-ingestion.md#operation-shape). The storage-level invariant: `commit_chain_epoch` writes every artifact, the event envelope, the event sequence pointer, store metadata, and the visible epoch pointer in one RocksDB `WriteBatch`. Readers observe either the previous epoch or the new epoch; a half-committed epoch is a correctness bug.

Required steps inside `commit_chain_epoch`:

1. Validate block links, compact block artifacts, transaction references, tree metadata, reorg-window metadata, and one replay envelope per header. Replay headers, transaction order, public facts, intrinsic balances, and transparent input and output facts must exactly match the other semantic rows in the commit. Raw block and transaction blobs must exactly match the store's immutable retention contract for every committed header and transaction; their identities and locations are validated separately, and their bytes are not replay fields.
2. Serialize the read-validate-write window for the visible epoch pointer and event sequence pointer, or use an equivalent compare-and-swap write fence.
3. Build the single `WriteBatch` covering artifacts (including `block_replay` and the address-output projection rows derived from `transparent_outputs_by_outpoint`), transparent current-projection repairs, event envelope, sequence pointer, store metadata, and visible-epoch pointer. Historical retention is a separate maintenance write.
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

## Schema Compatibility

Stores validate schema at open. A store written with an `artifact_schema_version` above `MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION` returns `StoreError::SchemaTooNew`; a network or layout mismatch returns the matching `SchemaMismatch` or `ChainEpochNetworkMismatch` variant. Both surface as `SchemaMismatch` at the service boundary and fail readiness with `schema_mismatch`. `zinder-query` never mutates canonical storage on schema mismatch; the operator recreates the store from a fresh ingest run or an offline checkpoint.

Store schema 13 restores the wipe-and-resync posture for every earlier layout.
Older releases could migrate metadata versions 10 and 11 in place, but those
layouts do not contain durable complete spend replay records and may have
deleted their point facts. Primary open therefore rejects every pre-13 store;
secondaries must use the freshly rebuilt volume after ingest establishes it.

Store schema 14 adds the `block_replay` column family, and artifact schema
19 requires exactly one canonical replay envelope for every committed header.
Open refuses schema-13 volumes rather than creating the new column family or
pretending that omitted source facts can be reconstructed. Primary and
secondary processes must use a fresh schema-14 volume populated from source or,
when available, a separately certified schema-19 snapshot.

Artifact schema version 12 adds the Ironwood (NU6.3) shielded pool to
`tip_metadata` and to each compact block's payload. A version-11 artifact store
carries neither and cannot be repaired in place, because the omitted Ironwood
action data was never derived from the source block; it is rejected at open with
`StoreError::SchemaTooOld` and must be rebuilt from genesis. The store metadata
migrations above can normalize the RocksDB layout, but they cannot upgrade
pre-Ironwood artifact payloads.

Artifact schema version 13 adds the signed Orchard and Ironwood value balances
and the Orchard shared anchor to `TransactionFactsArtifactRecord`. A version-12
artifact store carries none of these fields, because they were never derived
from the source block; it is rejected at open with `StoreError::SchemaTooOld`
and must be rebuilt from genesis.

Store schema version 12 removes the canonical `transparent_address_tx_index`
column family. Transparent-address transaction history is a derive-plane
projection over canonical transaction, output, and spend facts; the shared
`TransparentAddressTxIndexArtifact` row type remains the wallet/query response
shape, but canonical ingest no longer writes or serves that projection.

Artifact schema 18 and store schema 13 replace the transparent spend block
index's outpoint-only payload with the complete observed input set and the
ordered subset of resolved spend facts. Finalized derive replay reads this
retained block-local source after per-outpoint rows are swept. Recording both
sets distinguishes legitimate checkpoint-parent misses from record
truncation. A pre-13 store cannot prove that all omitted facts remain
available, so there is no in-place migration.

## Checkpoints and Backups

RocksDB checkpoints are used for backups (`zinder-ingest backup --to <path>`), fixture capture, offline repair, and immutable analytics replicas. The backup command checkpoints the canonical store and the bundled derive store together, installing the derive checkpoint under the canonical checkpoint's `derive` subdirectory. Restore is "stop, replace, start" (operator procedure, no online restore in v1).

Checkpoint readers must open a documented manifest and validate store identity, network, schema versions, and visible epoch before serving data. They serve frozen snapshots; production read replicas instead open the live store as RocksDB-secondary per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) and replay the writer's WAL.

## Multi-Process Operations

The primary/secondary contract is in [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md): one writer per store path, process-unique `secondary_path`, 1,000 ms catchup default, schema-version one-directional compatibility, gRPC-only subscription delivery. Storage code follows that ADR; this document owns the storage-family details.

## Storage Metrics

Readiness causes and operational metrics are owned by [Service operations](service-operations.md). Storage-specific metrics:

- Current `ChainEpoch` height and hash.
- Safe tip height and hash.
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
  pending compaction bytes, running compaction count, and per-CF
  active-memtable size. DB-level write-controller properties
  (`rocksdb.actual-delayed-write-rate`, `rocksdb.is-write-stopped`) are
  exported under `cf="__db__"`. Every sample carries a `store_role` label
  (`canonical_primary`, `canonical_secondary`, `derive_primary`,
  `derive_secondary`) so the canonical and derive stores, which share one
  process, stay attributable in aggregate resident-set analysis.
- WAL ceiling diagnostics through `zinder_store_wal_bytes` (live `*.log`
  bytes inside the store path) and `zinder_store_wal_bytes_limit` (the
  configured role-scoped RocksDB `max_wal_bytes`). Both feed
  `ZinderStoreWalGrowth` per [ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md).
- Block-cache capacity and usage through dedicated gauges
  `zinder_store_block_cache_capacity_bytes`,
  `zinder_store_block_cache_usage_bytes`, and
  `zinder_store_block_cache_pinned_usage_bytes`. Pinned usage exposes blocks
  held above capacity by live references, which total usage alone hides. These
  are the canonical signals; the same numbers are not republished as
  `zinder_store_rocksdb_property` labels. These and the memtable-budget, WAL,
  and I/O-mode gauges carry the same `store_role` label.
- DB-wide RocksDB statistics tickers through `zinder_store_rocksdb_ticker`,
  labeled by the upstream ticker name and `store_role`. The exported set covers
  Bloom filter useful/full-positive/full-true-positive counts, block-cache
  data/index/filter hits and misses, MultiGet call/key/byte counts, total bytes
  read and written, stall micros, and compaction bytes read and written. Each
  ticker aggregates across all column families of one store; the Bloom entries
  reflect only `transparent_output` because it is the sole filtered column
  family.
- Resource-footprint sampling cadence. The sweep that publishes the property,
  WAL, block-cache, memtable, and I/O-mode gauges runs on the write path but is
  throttled to at most once per second, so a burst of small commits (one per
  mempool event on the derive store) does not probe RocksDB properties per
  write. The interval sits below the 15s scrape so every scrape reads a fresh
  sample.
- Startup-phase duration through `zinder_startup_phase_duration_seconds`,
  labeled by `phase`, `outcome`, and `service`. Cold WAL replay durations
  feed `ZinderStartupOpenStorageSlow`.
- RocksDB read latency through `zinder_store_read_duration_seconds`, labeled by
  operation, column family, status, and `caller`. The `caller` label
  attributes each read to the pipeline stage that issued it (`query`,
  `block_prefetch`, `commit_fallback`, `retention_sweep`, `derive_hydration`),
  and `zinder_store_multi_get_keys_total` / `zinder_store_multi_get_resolved_total`
  carry the same label to size serial-seek wall time under concurrency.
- Visibility-index point-read seeks through
  `zinder_store_visibility_seek_total` and bounded batch scans through
  `zinder_store_visibility_scan_total`, both labeled by artifact family. A
  block replay batch contributes one scan followed by one payload `multi_get`,
  independent of its block count; per-height seek growth indicates a caller
  bypassed the batch boundary.
- Transparent-retention sweep size through
  `zinder_store_retention_swept_outpoints_total`, remaining height backlog
  through `zinder_store_retention_backlog_heights`, and bounded pass latency
  through `zinder_store_retention_sweep_duration_seconds`. Each swept outpoint removes
  one row from each of `address_output_index`, `transparent_output`, and
  `transparent_spend_fact`.
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

## Storage Readiness Checklist

Production storage code should not be treated as ready until these checks pass:

1. Real Zcash fixture replay from Zebra or curated mainnet artifacts.
2. `GetBlockRange` P50, P99, and P99.9 under concurrent ingest.
3. Crash recovery during artifact writes, visible pointer writes, compaction, and reorg replacement.
4. Recovery to the last complete `ChainEpoch` or a typed fail-closed error.
5. Query refusal on schema mismatch.
6. Checkpoint creation and checkpoint reader validation.
7. Compact block raw-byte serving or measured decode/re-encode fallback.
8. Deletion and rebuild of query-owned caches without canonical storage changes.

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
- `store_error`
- `store_key`
- `artifact_envelope`
- `stream_cursor`

Avoid generic modules such as `common`, `shared`, `helpers`, `manager`, or `service`. Use a RocksDB-specific module only for the private adapter layer; do not let RocksDB vocabulary leak into public contracts.
