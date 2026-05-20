# Bulk-catchup throughput: indexer slower than source node

This document records a measurement-based finding: `zinder-ingest` bulk catchup runs roughly one order of magnitude slower than its design target and slower than the upstream Zebra node it reads from. The cause is structural (a single-threaded consumer pipeline), not configuration. A fix is sketched but not yet implemented; this investigation motivates a future ADR.

## Why this exists

An indexer that reads from a fully validated node is doing strictly less work than the node itself. It does not validate proofs, does not re-verify signatures, does not run consensus. It reads block bytes, computes derived state, writes to storage. On the same host the indexer should be *faster* than its source node, not slower. The current behaviour inverts that expectation: a mainnet sync that should land in single-digit hours takes 2.6 to 4.4 days, while the source Zebra (which has already done all the cryptographic work) sits at 1 % CPU watching the indexer struggle. This is the architectural smell the rest of the document explains.

## Measured rate

Sampled live on a fully synced Z3 mainnet (Zebra tip `#3,348,233`) running on macOS Docker Desktop, 10 logical cores, 19.5 GiB VM. Sample window: 2 minutes between height samples.

| Sample | dt (s) | dh (blocks) | rate (blocks/sec) | ETA at this rate |
| --- | ---: | ---: | ---: | --- |
| 71-second window  | 71  | 1,000 | **14.1** | 61 h (2.6 d) |
| 123-second window | 123 | 1,000 | **8.1**  | 106 h (4.4 d) |

The two samples bracket a steady-state rate of roughly **8 to 14 blocks/sec**. The variance comes from block-size variance and the periodic RocksDB flush every `flush_every_n_epochs × commit_batch_blocks = 5,000` blocks ([ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md)).

## Design target (for comparison)

[`docs/architecture/chain-ingestion.md`](../architecture/chain-ingestion.md#bulk-catch-up-throughput-shape) commits to a goal:

> Two structural choices keep the throughput high enough for a multi-million-block sync to complete in **single-digit hours rather than weeks**:
>
> 1. **Pipelined block fetches.** … keeps up to `ingest.bulk_catchup.fetch_concurrency` (default 32) block fetches in flight via `futures_util::stream::iter(...).buffered(N)`.
> 2. **Concurrent per-block RPCs.** `getblockheader`, `getblock`, and `z_gettreestate` … via `tokio::join!`.

For mainnet (3.3 M blocks) "single-digit hours" implies a sustained throughput of roughly **90 to 350 blocks/sec**. The observed range (8 to 14 blocks/sec) is **one order of magnitude below the floor** and **two orders below the ceiling** of that target.

## Where the time actually goes

Container CPU snapshot during steady catchup:

| Container | CPU % (10 cores available) | Memory |
| --- | ---: | ---: |
| `zinder-mainnet-zinder-ingest-1` | **93 %** | 1.8 GiB |
| `z3-mainnet-zebra-1` | 1.1 % | 0.9 GiB |
| `zinder-mainnet-zinder-query-1` (reader) | 0.5 % | 12.3 GiB |
| `zinder-mainnet-zinder-explorer-1` (reader) | 0.0 % | 0.5 GiB |

Two facts dominate:

1. **The source node is idle.** Zebra is serving the indexer at 1 % CPU and could push data faster by orders of magnitude. The bottleneck is not upstream availability.
2. **The indexer is single-threaded.** 93 % of one core, out of 1000 % available. The container has 10 cores; nine are idle. Pipelined fetch (`buffered(32)`) is doing its job for network I/O, but every block then has to traverse a single consumer thread end-to-end.

## Code-level cause

The hot loop lives in [`services/zinder-ingest/src/backfill.rs`](../../services/zinder-ingest/src/backfill.rs) around line 329:

```rust
let mut block_stream = futures_util::stream::iter(BlockHeightRange::inclusive(...))
    .map(|height| async move { fetch_block_with_retry(...).await })
    .buffered(fetch_concurrency);                            // 32 parallel fetches

while let Some(fetch_result) = block_stream.next().await {
    let source_block = fetch_result?;
    let built_artifacts = artifact_builder.build(&source_block)?;   // synchronous CPU work
    // … push into `batch` …
    if batch.len() == commit_batch_blocks {
        populate_subtree_root_artifacts(...).await?;               // serial RPCs
        commit_finalized_backfill_batch(store, … &mut batch)?;     // serial RocksDB write
        if epochs_since_last_flush >= flush_every_n_epochs {
            flush_primary_chain_store(store).await?;
        }
    }
}
```

`artifact_builder.build` (`services/zinder-ingest/src/chain_ingest.rs:289`) runs synchronously on the consumer task. It does the per-block derive work:

- Decode transaction bytes into typed `TransactionArtifact` records.
- Walk the block's transparent inputs and outputs to produce `transparent_utxo_spend` and `transparent_address_utxo` entries plus the cross-index `transparent_address_tx_index` rows.
- Hash shielded note commitments into the running tree-state observer (Pedersen for Sapling, Sinsemilla for Orchard).
- Emit the `BlockArtifact`, `CompactBlockArtifact`, and `TipMetadata` records.

Cryptographic hashing for the shielded pools dominates the wall-clock cost. The current loop runs that work on the same tokio task that pulls from `block_stream`, so even though 32 fetches are in flight, the consumer side is strictly serial. The only `spawn_blocking` call in `backfill.rs` is around `flush_primary_chain_store` (line 418), confirming that the design has identified blocking I/O as worth offloading but has not done the same for the per-block CPU work.

## Why pipelining the fetch alone does not help

`futures_util::stream::buffered(N)` keeps up to N futures in flight and yields their results in submission order. That shape preserves ordering (which the commit path requires) at the cost of HOL-blocking: a single slow consumer drains the buffer to zero and the next 31 fetches sit waiting for one consumer thread. From the consumer's point of view this is identical to a serial stream with a small prefetch.

The fetch is not the bottleneck. The proof is in the CPU split: Zebra at 1 %, indexer at 93 %. If fetch were the limit we would see the opposite (idle indexer, busy node).

## Recommended changes

Three structural changes that respect the per-`ChainEpoch` ordering invariant from [ADR-0001](../adrs/0001-rocksdb-canonical-store.md) and [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md):

### 1. Move `ArtifactBuilder::build` off the consumer thread

The cheapest first cut: wrap the call in `tokio::task::spawn_blocking` so each block's derive runs on the blocking-task threadpool rather than the loop's reactor. `spawn_blocking` plus an ordered join (`futures_util::stream::iter(...).then(spawn_blocking).buffered(N)` or an explicit small queue) gets the build off the critical path and uses the cores that are currently idle.

Trade-off: the blocking pool's default size is 512 threads on tokio, but most workloads cap themselves with a custom pool of `num_cpus`. Without bounding the pool, a flapping upstream that produces a burst of blocks could fan out a build storm. Cap at `num_cpus - 1` and the worst case is balanced against the I/O threads.

Expected gain: linear in core count up to the per-block batch boundary. On a 10-core host this is roughly an 8-10× speedup over the current single-thread shape.

### 2. Make the commit pipeline a true stage pipeline

Split the loop into three tokio tasks connected by small bounded channels:

```text
fetch-stage ──(SourceBlock)──> derive-stage ──(BuiltArtifacts)──> commit-stage
   N=32                          N=cpus-1                            N=1
```

Each stage owns one phase. The commit stage stays serial (one writer is required for the per-epoch atomicity contract in ADR-0001). The derive stage parallelizes across cores. The fetch stage stays as it is today. End-to-end ordering survives because the derive stage's output channel is a single-producer ordered stream consumed by the commit stage.

Trade-off: more moving parts than option 1, and the bounded channels need careful sizing so the slowest stage does not starve. Higher engineering cost but cleaner separation of concerns.

Expected gain: same throughput ceiling as option 1, with better headroom for future optimizations to each stage independently.

### 3. Batch-parallel artifact assembly

Once a fetch fills `commit_batch_blocks = 1000` blocks, build all 1,000 artifacts in parallel (`rayon::par_iter` or `tokio::task::spawn_blocking` with futures `join_all`), then commit the resulting batch atomically. Ordering is preserved because the batch is assembled into a `Vec` indexed by height before commit.

Trade-off: peaks memory at one full batch of un-committed artifacts in RAM. With 1000 blocks at ~50 KB derived state each that is ~50 MiB per CF per batch, on the order of 700 MiB at the high water mark. Bound the batch or shrink `commit_batch_blocks` to compensate.

Expected gain: same ceiling as options 1 and 2 if the per-block derive is the dominant cost. Likely the simplest patch to the existing code.

### Recommendation

Start with option 1 (`spawn_blocking` per build). It is the smallest diff against the current loop, exercises the existing blocking-pool infrastructure, and proves the throughput hypothesis. If the measured speedup matches the prediction, option 2 becomes a follow-on refactor for clarity and future tuning; option 3 stays available as a fallback if profiling reveals the derive granularity is too small for `spawn_blocking` overhead to be worth paying per block.

## Reproducing the measurement

This works from any machine that can reach the ingest's `/api/v1/<network>/network` envelope (directly on port 4000, through the BFF, or via Prometheus).

```bash
# Two samples 2 minutes apart, compute the rate.
H1=$(curl -fsS http://127.0.0.1:4000/api/v1/mainnet/network \
       | python3 -c 'import json,sys;print(json.load(sys.stdin)["freshness"]["tip_height"])')
T1=$(date +%s)
sleep 120
H2=$(curl -fsS http://127.0.0.1:4000/api/v1/mainnet/network \
       | python3 -c 'import json,sys;print(json.load(sys.stdin)["freshness"]["tip_height"])')
T2=$(date +%s)
python3 -c "
h1, t1, h2, t2 = $H1, $T1, $H2, $T2
dt, dh = t2 - t1, h2 - h1
rate = dh / dt
print(f'{rate:.1f} blocks/sec ({dh:,} blocks in {dt}s)')
print(f'ETA to tip: {(3348233 - h2) / rate / 3600:.1f} h')
"
```

CPU profile during the same window:

```bash
docker stats --no-stream --format 'table {{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}' \
  | grep -E 'zinder-(ingest|query|explorer)|z3-(mainnet|testnet)-zebra'
```

A healthy state for the recommended fix is: ingest container CPU > 200 % (multiple cores busy), source-node CPU still under 50 %, observed rate above 50 blocks/sec, ETA under 24 hours.

## What this is not

This investigation is descriptive, not prescriptive. The numbers above are reproducible today; the proposed changes are not yet ADR-blessed. A follow-on ADR ("Parallel artifact derive for bulk catchup", or similar) should pick option 1, 2, or 3, name the chosen ordering invariant, set a target rate, and ship the change. This document exists so the next person to revisit the problem has a measured starting point and a clear hypothesis to test, not a blueprint to copy.

## References

- [ADR-0001: Use RocksDB for canonical storage](../adrs/0001-rocksdb-canonical-store.md)
- [ADR-0003: Canonical storage access boundary](../adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0015: Unified phase-driven ingest](../adrs/0015-unified-phase-driven-ingest.md): the design that commits to "single-digit hours" mainnet catchup.
- [ADR-0020: Bounded RocksDB resource budget](../adrs/0020-bounded-rocksdb-resource-budget.md)
- [Chain ingestion §Bulk catch-up throughput shape](../architecture/chain-ingestion.md#bulk-catch-up-throughput-shape)
- [`services/zinder-ingest/src/backfill.rs`](../../services/zinder-ingest/src/backfill.rs): the bulk-catchup loop.
- [`services/zinder-ingest/src/chain_ingest.rs`](../../services/zinder-ingest/src/chain_ingest.rs): the `ArtifactBuilder::build` implementation.
- [Bulk-catchup OOM recovery](../runbooks/bulk-catchup-oom-recovery.md): the related WAL-replay restart loop, fixed by ADR-0020.
