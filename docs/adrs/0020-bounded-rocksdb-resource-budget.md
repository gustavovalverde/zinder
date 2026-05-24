# ADR-0020: Bounded RocksDB resource budget

Status: Accepted
Date: 2026-05-20
Related: [ADR-0001](0001-rocksdb-canonical-store.md),
[ADR-0003](0003-canonical-storage-access-boundary.md),
[ADR-0015](0015-unified-phase-driven-ingest.md)

## Context

`zinder-ingest` enters a self-perpetuating restart loop during initial sync on hosts that under-allocate RAM for RocksDB's WAL replay. The trap is documented in [the OOM-recovery runbook](../runbooks/bulk-catchup-oom-recovery.md):

1. During `BulkCatchup`, the writer commits large `WriteBatch`es. Each batch lands in the WAL first and flushes to SST on the next memtable rotation.
2. RocksDB's `max_total_wal_size = 0` default means "never trigger a flush based on WAL size."
3. When the process is killed mid-bulk-catchup (oncall restart, host reboot, prior OOM, deploy), the WAL keeps every uncommitted write. A 2.7 GiB WAL has been observed on mainnet.
4. The next start replays the WAL into memtables before the store is usable. RocksDB needs roughly 2.5× the WAL size in resident memory during replay (memtable plus per-CF index/bloom pins from `max_open_files = -1`).
5. On a RAM-constrained host the kernel kills `zinder-ingest` before replay completes. The container restarts, replays the same WAL, gets killed again. The cycle never breaks.

[ADR-0001](0001-rocksdb-canonical-store.md) states that "RocksDB tuning is part of Zinder's operational surface" but does not say *where* the choices live. Before this ADR, every tuning choice was a hardcoded constant inside two separate option factories (one in `zinder-store/src/kv/rocksdb.rs` and one in `services/zinder-explorer/src/store.rs`). Both factories used bare `Options::default()` plus a handful of create flags. Neither set a WAL ceiling, a block cache, or an open-file cap. The two factories were not aware of each other.

Two problems compounded:

- **No tuning surface.** Operators could not raise the cache or cap the WAL without recompiling.
- **No central source of truth.** A fix applied to the canonical store would leave the derive plane vulnerable to the same OOM trap, and any future RocksDB-using component would re-invent the same defaults.

## Decision

RocksDB option choices are layered into three tiers with explicit ownership:

### Tier 1 — Architectural invariants (locked in code)

Four RocksDB options are contracts of the storage layer and are not operator-configurable:

- **WAL on** (no `set_disable_wal`). Disabling the WAL would erase every unflushed write on shutdown and break the per-`ChainEpoch` atomicity guarantee from ADR-0001 §Commit Protocol.
- **Point-in-time recovery** (RocksDB default; do not call `set_wal_recovery_mode(SkipAnyCorruptedRecords)`). Letting RocksDB silently truncate a partial WAL would lose committed writes without an audit trail.
- **Atomic cross-CF flush on** (`set_atomic_flush(true)`). The per-epoch invariant requires a single atomic flush across the artifact families that commit together.
- **Ordered writes** (no `set_unordered_write`). The derive plane assumes per-epoch sequence ordering.

These four are encoded in `zinder_store::build_primary_db_options` and `zinder_store::build_secondary_db_options`. Operators cannot toggle them through config.

### Tier 2 — Bounded resource budget (operator-configurable)

Three knobs cap the open-time RAM peak. They live under `[storage.tuning]` for the canonical store (writer + secondaries) and `[explorer.tuning]` for the derive plane:

| Knob | Default (canonical) | Default (derive) | Effect |
| --- | --- | --- | --- |
| `block_cache_bytes` | 512 MiB | 128 MiB | Bounded LRU cache size shared by data, index, and bloom blocks. Without it, RocksDB pins index and bloom blocks per-SST in resident memory, which scales with store size. |
| `max_wal_bytes` | 256 MiB | 64 MiB | Total live WAL ceiling. Crossing it triggers a memtable flush so the WAL truncates. The default of 0 (RocksDB's own) means "never trigger from WAL size," which is the bug. |
| `max_open_files` | 512 | 256 | Open SST file handle cap. The default of -1 (RocksDB's own) means "open every SST and pin its metadata," which scales with store size. |

The typed value `zinder_store::StorageTuning` carries these three numbers. Both the canonical store (`ChainStoreOptions::tuning`) and the derive store (`DeriveStoreOptions::tuning`) consume the same type.

`block_cache_index_and_filter_blocks = true` and `pin_l0_filter_and_index_blocks_in_cache = true` are paired with the bounded block cache: index and bloom blocks live inside the bounded cache instead of being pinned per-SST. This caps at-rest metadata budget at the cache size.

A validation gate in `validate_chain_store_options` rejects values below the minimums (`StorageTuning::MIN_BLOCK_CACHE_BYTES` = 4 MiB, `MIN_MAX_WAL_BYTES` = 4 MiB, `MIN_MAX_OPEN_FILES` = 32). `max_wal_bytes = 0` is specifically rejected because, on writer-posture stores, it disables the safety trigger; the schema is symmetric on secondaries (where the field is inert) so the same minimum applies on both sides.

### Tier 3 — Phase-aware flush policy

`BulkCatchup` forces an explicit `db.flush()` every `flush_interval_epochs` committed epochs (default 5). With the default `canonical_batch_max_blocks = 1000`, this truncates the WAL after every 5,000 committed blocks. The flush is also performed once on `BulkCatchup` exit so the phase hands off a clean WAL state to `TipFollow`.

`TipFollow` does not flush explicitly. Each commit is one block; the WAL stays under a few MiB; natural memtable rotation handles it.

### Tier 4 — Observable peaks

Metrics that would catch the trap before it became operational:

- `zinder_store_wal_bytes` (gauge) — sum of `*.log` file sizes in the store path. Scraped at every commit.
- `zinder_store_wal_bytes_limit` (gauge) — the configured `max_wal_bytes`. Lets alerts express thresholds as a percentage of the limit.
- `zinder_store_block_cache_capacity_bytes` and `zinder_store_block_cache_usage_bytes` — block cache size and current usage. Sampled from the bounded LRU directly, not duplicated as `zinder_store_rocksdb_property` labels.
- `zinder_store_rocksdb_property{property="rocksdb.cur-size-active-mem-table"}` — added to the existing per-CF property gauge so an oversized memtable shows up alongside the WAL gauge.
- `zinder_startup_phase_duration_seconds` (histogram, labels `phase`, `outcome`, `service`) — emitted by `StartupPhaseGuard` on every phase exit. Coarser-bucketed than the project's general `_duration_seconds` rule because cold WAL replay can run for several minutes.

Two alerts in `observability/prometheus/rules/zinder-readiness.yml`:

- `ZinderStoreWalGrowth` fires when `wal_bytes / wal_bytes_limit > 0.75` for five minutes.
- `ZinderStartupOpenStorageSlow` fires when `histogram_quantile(0.95, …)` of the `open_storage` phase exceeds 60 seconds.

## Consequences

- **The trap is closed by construction.** With a non-zero WAL ceiling, a non-pinning open file cap, a bounded block cache, and a periodic flush, the bulk-catchup OOM trap cannot fire on a host that satisfies the documented memory envelope.
- **The fix applies once.** Both the canonical store and the derive store route through `zinder_store::build_primary_db_options`. A future RocksDB-using component (a fourth store, a checkpoint inspector, a repair tool) uses the same factory.
- **Operators have a tunable surface.** RAM-constrained hosts can drop the cache to 128 MiB; high-throughput hosts can raise to 1 GiB. The invariants stay locked.
- **Operators cannot disable WAL or atomic flush.** This is deliberate.
- **The metric set lets SREs alert before the trap fires.** Watching `zinder_store_wal_bytes / zinder_store_wal_bytes_limit` and the `open_storage` p95 catches both the pre-trap state and the post-trap restart loop.
- **All existing tests use `StorageTuning::for_local_tests`** (32 MiB cache, 16 MiB WAL ceiling, 64 open files) so unit tests keep their tight memory footprint while exercising the bounded code path.

## Alternatives considered

- **A single hardcoded fix in `primary_db_options`.** Closes the trap for the canonical store only; leaves the derive plane and any future component vulnerable. Rejected.
- **A `[rocksdb]` config section exposing every option.** Surfaces too many knobs that operators have no signal to tune; many of them break invariants if mis-set. Rejected in favor of the curated three-knob `[storage.tuning]` surface plus the locked invariants.
- **`StorageTuning` as a serde-deserializable type directly.** Forced every consuming crate to take a hard dep on serde just to construct one. Rejected; the type is a plain struct, with serde shaping done at the runtime config boundary (`StorageTuningSection`).
- **Filesystem-scrape `du` for WAL bytes vs RocksDB property API.** `rust-rocksdb` 0.47 does not expose `DB::GetSortedWalFiles` or a per-DB WAL-size property. A filesystem scan of `*.log` is one syscall plus N stat calls per commit; the overhead is negligible. Accepted.
