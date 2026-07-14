# Recovering from a bulk-catchup OOM trap

`zinder-ingest` can enter a self-perpetuating restart loop during initial sync on hosts that under-allocate RAM for RocksDB's WAL replay. The trap is closed by construction in the current code via the bounded RocksDB resource budget specified in [ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md); this runbook records the symptom, the recovery for any operator who still hits the trap on a pre-ADR-0020 store, and the operator-tunable knobs that govern the bound.

## Symptom

`zinder-ingest` restarts every 14 to 17 seconds. Logs show the startup sequence reaching `open_storage` and then going silent until the container is reaped:

```
INFO zinder::startup: startup phase entered phase="connect_node" phase_state="entry"
INFO zinder::startup: startup phase exited phase="connect_node" phase_state="exit" outcome="ok"
INFO zinder::startup: startup phase entered phase="check_schema" phase_state="entry"
INFO zinder::startup: startup phase exited phase="check_schema" phase_state="exit" outcome="ok"
INFO zinder::startup: startup phase entered phase="recover_state" phase_state="entry"
INFO zinder::startup: startup phase exited phase="recover_state" phase_state="exit" outcome="ok"
INFO zinder::startup: startup phase entered phase="open_storage" phase_state="entry"
[no further log lines; container exits and restarts ~15 s later]
```

There is no panic, no error log, no `open_storage` exit line. `docker inspect` reports `OOMKilled: false` and `ExitCode: 0`, which together would normally suggest a clean shutdown — but those fields are misleading for this failure mode. The authoritative signal is `docker events`:

```bash
docker events --since '30s' --filter container=zinder-mainnet-zinder-ingest-1
# … oom (no attrs)
# … die  exitCode=137  execDuration=15
# … start
```

`exitCode=137` is `128 + SIGKILL`. On current Compose deployments, `OOMKilled: true` means the service crossed its configured cgroup memory ceiling. On older or custom deployments without a `mem_limit`, the host kernel can reap the process while Docker reports `OOMKilled: false`; in that case the Docker event stream is the authoritative signal. The `restart: on-failure` policy then restarts on the non-zero kernel exit.

## Root cause

During `BulkCatchup`, `zinder-ingest` commits chain artifacts in large `WriteBatch`es. Those writes land in RocksDB's WAL first and flush to SST during the next memtable rotation. The defaults shipped in [`primary_db_options`](../../crates/zinder-store/src/kv/rocksdb.rs) impose no upper bound on the live WAL:

```rust
fn primary_db_options() -> Options {
    let mut db_options = Options::default();
    db_options.create_if_missing(true);
    db_options.create_missing_column_families(true);
    db_options.enable_statistics();
    db_options
}
```

If the process is killed during bulk catchup before a flush completes (any reason: oncall restart, host reboot, prior OOM, deploy), the WAL keeps every uncommitted write. Observed in one mainnet incident:

| | On disk | WAL |
| --- | --- | --- |
| `/data/store` total | 11.9 GiB | |
| 181 SSTs | 9.5 GiB | |
| `000653.log` | | **2.7 GiB** |

The next start has to replay the 2.7 GiB WAL into memtables before the store is usable. RocksDB needs roughly 2.5× the WAL size in resident memory during replay (memtable plus per-CF index/bloom pins from `max_open_files = -1`, which preloads every SST's metadata). On a 16 GiB Docker Desktop VM already hosting Zebra, Zaino, observability sidecars, and a second zinder stack, that 7 GiB peak does not fit. The kernel picks `zinder-ingest` (the largest target) and kills it. The restart re-enters the same replay, peaks at the same 7 GiB, gets killed again. The cycle never breaks because the WAL is never flushed.

Testnet does not hit this in practice because the testnet store is already past initial catchup and runs in `TipFollow`, where each commit is one block (memtables stay near 1 MiB per column family, the WAL stays under a few MiB, and a restart's replay needs less than 100 MiB total).

## Confirming you are in this state

Three checks, in order of cost.

**1. Docker events show `oom` then `die exitCode=137`** for the ingest container. This is decisive.

```bash
docker events --since 30s --filter container=zinder-<network>-zinder-ingest-1 \
  --format '{{.Time}} {{.Action}} attrs={{json .Actor.Attributes}}'
```

**2. The store has a single large WAL file** (`*.log`) sitting next to the SSTs:

```bash
docker run --rm -v zinder-<network>-data:/data alpine \
  sh -c 'find /data/store -name "*.log" -exec du -h {} \;'
# /data/store/000653.log  2.7G   ← anything over ~500 MiB is symptomatic
```

**3. Container memory peaks well above the available headroom on the Docker host.**

```bash
docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' | grep zinder-ingest
# zinder-mainnet-zinder-ingest-1   7.0GiB / 15.6GiB   ← climbing right before the kill
```

If all three line up, the trap is fired.

## Immediate recovery

The recovery has one job: give RocksDB enough RAM to finish the WAL replay **once**. After that single successful open, the memtable flushes to SST, the WAL is truncated, and subsequent opens fit in normal sizes.

**Option A — increase host RAM** (lowest operator burden):

- On a Docker Desktop host, raise the VM allocation (Settings → Resources → Memory) to a value that leaves at least `2.5 × WAL_size` of headroom above the other running containers. For a 2.7 GiB WAL on a 16 GiB host already at 12.5 GiB of container RSS, raise to 24 GiB.
- On a bare-metal Linux host, free RAM by stopping non-essential workloads or attach more memory to the VM.
- Restart Docker Desktop / the host so the new allocation takes effect, then `docker compose up -d` the zinder stack. The ingest will replay the WAL through `open_storage`, emit `open_storage phase_state="exit" outcome="ok"`, and proceed into `BulkCatchup`. Watch with `docker logs -f`.

You can drop the host RAM back down to its previous level once the ingest has reached `TipFollow`. The 7 GiB peak is a one-time bootstrap cost, not steady state.

**Option B — wipe the store and resync** (high cost, guaranteed clean):

Only when the WAL is corrupted (rare) or when growing host RAM is impossible. Bulk catchup from genesis takes hours on testnet and a day or more on mainnet.

```bash
docker compose --env-file deploy/.env.<network> -f deploy/docker-compose.yml down
docker volume rm zinder-<network>-data
docker compose --env-file deploy/.env.<network> -f deploy/docker-compose.yml up -d
```

Option B is the same path documented under [Initial sync § Forked store](initial-sync.md#forked-store) and surfaces the same trade-offs.

## Operator-tunable knobs

The current code applies role-scoped bounded resource budgets at open. Operators configure the canonical store through `[storage.canonical.rocksdb]` and the derive store through `[storage.derive.rocksdb]`; the remaining RocksDB invariants are locked in code. The full design lives in [ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md).

Writer defaults target a mainnet-sized canonical store:

```toml
[storage.canonical.rocksdb]
block_cache_bytes = 536870912   # 512 MiB
max_wal_bytes = 268435456       # 256 MiB
max_open_files = 512
write_buffer_bytes = 16777216   # 16 MiB per column family
max_write_buffer_count = 2
max_background_jobs = 2         # aggregate flush + compaction jobs
```

And a multi-column-family replay profile for the writer-owned derive store:

```toml
[storage.derive.rocksdb]
block_cache_bytes = 268435456   # 256 MiB
max_wal_bytes = 268435456       # 256 MiB
max_open_files = 512
write_buffer_bytes = 16777216   # 16 MiB per column family
max_write_buffer_count = 4
max_background_jobs = 2         # raise only with pressure-metric evidence
memtable_budget_bytes = 536870912 # 512 MiB aggregate hard bound
```

Reader secondaries default lower so query, explorer, and compat services do not
compete with the ingest writer during clean sync:

```toml
[storage]
secondary_catchup_interval_ms = 1000

[storage.canonical.rocksdb]
block_cache_bytes = 134217728   # 128 MiB
max_wal_bytes = 33554432        # 32 MiB
max_open_files = 128
write_buffer_bytes = 8388608    # 8 MiB per column family
max_write_buffer_count = 2

[storage.derive.rocksdb]
block_cache_bytes = 67108864    # 64 MiB
max_wal_bytes = 16777216        # 16 MiB
max_open_files = 64
write_buffer_bytes = 4194304    # 4 MiB per column family
max_write_buffer_count = 2
```

Reader secondaries use the same resource-budget type, whose retained
`max_background_jobs = 2` value preserves uniform parsing and validation.
`OpenAsSecondary` disables automatic flushes and compactions, so Zinder does
not apply that field to reader stores.

Plus the derive and bulk-catchup ingest knobs:

```toml
[ingest.derive]
replay_batch_blocks = 500
memory_degrade_ratio = 0.90
memory_pause_ratio = 0.99
memory_resume_ratio = 0.80
min_replay_batch_blocks = 50

[ingest.bulk_catchup]
canonical_batch_max_blocks = 1000
canonical_batch_max_artifact_bytes = 536870912
canonical_batch_max_estimated_write_bytes = 536870912
canonical_batch_min_blocks_before_estimated_write_close = 100
source_segment_max_blocks = 64      # hard ceiling; runtime size adapts by response bytes
source_segment_target_response_bytes = 33554432
source_fetch_max_in_flight_requests = 20
source_fetch_max_in_flight_bytes = 671088640
block_prepare_concurrency = 16
block_prepare_max_in_flight_artifact_bytes = 536870912
commit_reassembly_max_queued_artifact_bytes = 536870912
flush_interval_epochs = 5      # RocksDB flush cadence (epochs)
```

Do not use `source_segment_max_blocks` as the primary response-size tuning knob. The
writer targets `source_segment_target_response_bytes`, which must stay at or
below `node.max_response_bytes`, shrinks the next source segment
after oversized or dense responses, carries learned density across bulk commit
batches, and resets density when the consensus branch changes. If
`zinder_node_source_segment_split_total{reason="response_too_large"}` keeps
increasing while `zinder_ingest_source_segment_next_blocks` is already `1`,
raise `node.max_response_bytes` or switch to a source feed that does not require
large JSON payloads. If split bursts repeat immediately after every
`chain_committed` event, the source-density state is being reset too often.

With `canonical_batch_max_blocks = 1000` and `flush_interval_epochs = 5`, the writer truncates the WAL every 5,000 committed blocks. `canonical_batch_max_artifact_bytes` is a hard raw-artifact memory limit. `canonical_batch_max_estimated_write_bytes` closes dense canonical-write batches only after `canonical_batch_min_blocks_before_estimated_write_close`, except for a single oversized block. Crash-recovery RAM is bounded above by `block_cache_bytes + max_wal_bytes + active_memtables`, roughly 1 GiB total for the canonical-store defaults.

`ingest.bulk_catchup.source_fetch_max_in_flight_bytes` is the source admission watermark. The first density probe reserves `node.max_response_bytes`; later requests reserve at least the configured response target with 50% prediction headroom, capped at `node.max_response_bytes`, and then resize to measured bytes after decode. Keep the watermark at least as large as `node.max_response_bytes`; otherwise startup rejects the config because the initial probe would not fit. A response above its prediction can temporarily exceed the admission watermark, but the request-count ceiling and `node.max_response_bytes` retain an absolute bound and the scheduler admits no more work until the watermark recovers. Track `zinder_ingest_source_segment_reservation_undersized_total` when tuning this budget.

`ingest.bulk_catchup.block_prepare_concurrency` controls CPU workers; `block_prepare_max_in_flight_artifact_bytes` controls the active plus completed derived-artifact backlog. The source and commit-reassembly byte limits bound different memory pools and should be tuned separately. Startup derive replay is bounded by `ingest.derive.replay_batch_blocks` and the derive memory watermarks, so replay shrinks its effective batch before pausing. See [ADR-0021](../adrs/0021-parallel-block-derivation.md).

The four `bulk_catchup` queue byte-caps auto-derive from the container memory budget when cgroup v2 is available, so containerized deploys (Railway, Fly, ECS, k8s, plain Docker) inherit sane defaults without per-deploy tuning. The formula is `container_memory / 64` per queue, clamped to `[128 MiB, 512 MiB]`. On dev hosts without cgroup the fallback 512 MiB / 384 MiB constants apply unchanged. `ZINDER_INGEST__BULK_CATCHUP__*_BYTES` env-var overrides still win when set; the auto-derived value is only the default. See [ADR-0022 § Revision: container-aware default queue caps](../adrs/0022-resource-budgeted-bulk-catchup.md#revision-container-aware-default-queue-caps-2026-05-26).

RAM-constrained hosts drop the cache to 128 MiB and the WAL ceiling to 64 MiB; high-throughput hosts can raise the cache to 1 GiB. The architectural invariants (WAL on, point-in-time recovery, atomic cross-CF flush, ordered writes) are not exposed to operator tuning because each one is a contract of the per-`ChainEpoch` commit guarantee.

## What not to change in pursuit of "less memory"

These options would each reduce RAM but break invariants Zinder relies on:

- **`Options::set_disable_wal(true)`** would eliminate the WAL but also crash recovery. Any unflushed write at shutdown vanishes. Incompatible with the per-`ChainEpoch` atomicity guarantee in [ADR-0001](../adrs/0001-rocksdb-canonical-store.md).
- **`Options::set_wal_recovery_mode(SkipAnyCorruptedRecords)`** would let the binary truncate a partial WAL silently on startup. The store would still open, but the truncated writes are lost without an audit trail. The default `PointInTimeRecovery` is the right posture for an indexer that derives from a deterministic chain source: corruption fails closed, the operator wipes the store, and `BulkCatchup` re-derives from upstream.
- **`Options::set_unordered_write(true)`** would parallelize commits but violate the per-epoch sequence ordering the derive plane assumes.
- **`Options::set_atomic_flush(false)`** would weaken cross-CF flush ordering; the per-epoch invariant requires a single atomic flush across the artifact families that commit together.

Stay on the defaults for the four above. The improvements come from the WAL ceiling, the open-file cap, and the bounded block cache, all of which are crash-safe by construction.

## Observability

The metric set shipped alongside the bounded resource budget catches the trap before it becomes operational:

- `zinder_store_wal_bytes` — sum of `*.log` file sizes inside the store path. Scraped at every commit.
- `zinder_store_wal_bytes_limit` — the configured `max_wal_bytes`. The alert `ZinderStoreWalGrowth` fires when the ratio exceeds 75% for five minutes, evaluated per `store_role`.
- `zinder_store_block_cache_capacity_bytes` and `zinder_store_block_cache_usage_bytes` — block cache size and current usage. These are the canonical signals for cache pressure; the same numbers are not republished as `zinder_store_rocksdb_property` labels.
- `zinder_store_max_background_jobs`: the configured primary-writer aggregate flush-and-compaction job cap. The default remains two, and the gauge is emitted only for `canonical_primary` and `derive_primary`; compare it with the per-column-family queue properties before raising it.
- `zinder_store_rocksdb_property` (gauge, labels `property`, `cf`, `store_role`) includes the active and immutable memtable state, level-zero file count, flush pending/running state, compaction pending/running state, base level, and pending-compaction bytes, plus the DB-level write-controller properties `rocksdb.actual-delayed-write-rate` and `rocksdb.is-write-stopped` (reported under `cf="__db__"`) that name a write stall directly. Every resource gauge above carries a `store_role` label; the canonical and derive stores share the process, so sum across available `store_role` series for total footprint and split by it to attribute pressure. Secondary roles are intentionally absent from the primary-only background-job gauge.
- `zinder_store_rocksdb_ticker` — use `rocksdb.stall.micros` with flush, memtable payload/garbage-at-flush, and compaction byte counters to distinguish foreground stalls, flush amplification, and compaction debt.
- `zinder_startup_phase_duration_seconds` (histogram, labels `phase`, `outcome`, `service`) — the alert `ZinderStartupOpenStorageSlow` fires when `open_storage` p95 exceeds 60 seconds, the shape this trap takes during the restart loop.
- `zinder_ingest_bulk_pipeline_queue_bytes{stage}` and `zinder_ingest_bulk_pipeline_reorder_buffer_bytes{stage}` distinguish active source/fact reservations from completed out-of-order backlog.
- `zinder_ingest_derive_replay_budget_state{state}` and `zinder_ingest_derive_replay_effective_batch_blocks` show whether derive replay is normal, degraded, or paused under memory pressure.
- `zinder_ingest_derive_replay_phase_gate` is `1` until the canonical phase is positively classified as `FollowingTip`. It therefore remains engaged during unclassified startup, `AwaitingUpstream`, and `BulkCatchup`, then drops to `0` when the writer reaches tip. During a from-genesis rebuild and after any mid-rebuild restart this gauge is expected to sit at `1`.
- `zinder_ingest_derive_replay_caught_up` and `zinder_ingest_historical_work_gate_open` distinguish the post-canonical derive drain from optional history. After canonical reaches tip, the first remains `0` until derive covers that tip, and the second must remain `0`; historical workers may advance only after both become `1`.

These signals appear on the relevant service `/metrics` endpoints and feed the existing Grafana dashboards under `observability/grafana/`.

## References

- [ADR-0001: Use RocksDB for Canonical Storage](../adrs/0001-rocksdb-canonical-store.md)
- [ADR-0015: Unified Phase-Driven Ingest](../adrs/0015-unified-phase-driven-ingest.md)
- [ADR-0020: Bounded RocksDB Resource Budget](../adrs/0020-bounded-rocksdb-resource-budget.md)
- [Initial sync](initial-sync.md)
- [Storage backend](../architecture/storage-backend.md)
- [RocksDB Write Ahead Log](https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log)
- [RocksDB Setup Options and Basic Tuning](https://github.com/facebook/rocksdb/wiki/Setup-Options-and-Basic-Tuning)
- [RocksDB Block Cache](https://github.com/facebook/rocksdb/wiki/Block-Cache)
- [RocksDB Memtable](https://github.com/facebook/rocksdb/wiki/MemTable)
- [`rust-rocksdb::Options`](https://docs.rs/rust-rocksdb/latest/rust_rocksdb/struct.Options.html)
- [`rust-rocksdb::BlockBasedOptions`](https://docs.rs/rust-rocksdb/latest/rust_rocksdb/struct.BlockBasedOptions.html)
