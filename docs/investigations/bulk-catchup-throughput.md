# Bulk-catchup throughput: indexer slower than source node

> **Resolution:** [ADR-0021: Parallel block derivation](../adrs/0021-parallel-block-derivation.md) and [ADR-0022: Transparent prevout rows](../adrs/0022-transparent-prevout-index.md) move the bottleneck from serial artifact derivation and transaction re-reads to source fetch, subtree-root hydration, transparent-prevout lookup, and RocksDB commit work. This document keeps the original measurement, the corrected hot-path analysis, and the reproducer so future regressions are easy to identify and re-attack.

This document records a measurement-based finding: `zinder-ingest` bulk catchup runs roughly one order of magnitude slower than its design target and slower than the upstream Zebra node it reads from. The cause is structural (a single-threaded consumer pipeline), not configuration.

## Why this exists

An indexer that reads from a fully validated node is doing strictly less work than the node itself. It does not validate proofs, does not re-verify signatures, does not run consensus. It reads block bytes, computes derived state, writes to storage. On the same host the indexer should be *faster* than its source node, not slower. The current behaviour inverts that expectation: a mainnet sync that should land in single-digit hours takes 2.6 to 4.4 days, while the source Zebra (which has already done all the cryptographic work) sits at 1 % CPU watching the indexer struggle. This is the architectural smell the rest of the document explains.

## Measured rate

Sampled live on a fully synced Z3 mainnet (Zebra tip `#3,348,233`) running on macOS Docker Desktop, 10 logical cores, 19.5 GiB VM. Sample window: 2 minutes between height samples.

| Sample | dt (s) | dh (blocks) | rate (blocks/sec) | ETA at this rate |
| --- | ---: | ---: | ---: | --- |
| 71-second window | 71 | 1,000 | **14.1** | 61 h (2.6 d) |
| 123-second window | 123 | 1,000 | **8.1** | 106 h (4.4 d) |

The two samples bracket a steady-state rate of roughly **8 to 14 blocks/sec**. The variance comes from block-size variance and the periodic RocksDB flush every `flush_interval_epochs × commit_batch_blocks = 5,000` blocks ([ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md)).

## Design target (for comparison)

[`docs/architecture/chain-ingestion.md`](../architecture/chain-ingestion.md#bulk-catch-up-throughput-shape) commits to a goal:

> Two structural choices keep the throughput high enough for a multi-million-block sync to complete in **single-digit hours rather than weeks**:
>
> 1. **Pipelined block fetches.** … keeps up to `ingest.bulk_catchup.fetch_concurrency` (default 32) block fetches in flight via `futures_util::stream::iter(...).buffered(N)`.
> 2. **Concurrent per-block RPCs.** `getblockheader`, `getblock`, and `z_gettreestate` … via `tokio::join!`.

Item 2 is already shipped at [`crates/zinder-source/src/zebra_json_rpc.rs:638-639`](../../crates/zinder-source/src/zebra_json_rpc.rs); the three RPCs do race in a single `tokio::join!`. Item 1 is also live; the buffered fetch stream at `services/zinder-ingest/src/backfill.rs:318` does keep 32 fetches in flight. Both halves of the documented design target are in place. The throughput shortfall is downstream of them.

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

The hot loop is at [`services/zinder-ingest/src/backfill.rs:332`](../../services/zinder-ingest/src/backfill.rs):

```rust
let mut block_stream = futures_util::stream::iter(BlockHeightRange::inclusive(...))
    .map(|height| async move { fetch_block_with_retry(...).await })
    .buffered(fetch_concurrency);                            // 32 parallel fetches

while let Some(fetch_result) = block_stream.next().await {
    let source_block = fetch_result?;
    let built_artifacts = artifact_builder.build(&source_block)?;  // synchronous CPU
    // … push into `batch` …
    if batch.len() == commit_batch_blocks {
        populate_subtree_root_artifacts(...).await?;               // serial RPCs
        commit_finalized_backfill_batch(store, … &mut batch)?;     // serial RocksDB write
        if epochs_since_last_flush >= flush_interval_epochs {
            flush_primary_chain_store(store).await?;
        }
    }
}
```

`artifact_builder.build` (`services/zinder-ingest/src/artifact_builder.rs:330`) runs synchronously on the consumer task. Its per-block work is:

- `zcash_deserialize_into::<ZebraBlock>` (`artifact_builder.rs:421-427`): walk the entire raw block byte stream, allocate every transaction structure, and verify deserialized identity matches the source-supplied header.
- Per-transaction `zcash_serialize_to_vec` (`artifact_builder.rs:401`): re-emit each transaction's canonical bytes so the resulting `TransactionArtifact.payload_bytes` is the round-tripped form rather than a substring of the original block buffer.
- `compact_transactions` (`artifact_builder.rs:479-503`): iterate every transaction once more, collect `cmu`, `ephemeral_key`, and a 52-byte ciphertext prefix per Sapling output and Orchard action, plus the prevout outpoints and full `script_pub_key` bytes for transparent inputs and outputs. Tree-size deltas (counts of Sapling outputs and Orchard actions) are tallied here.
- `transparent_utxo_artifacts`, `transparent_address_tx_index_artifacts`, `transparent_address_tx_index_spend_candidates`, `derive_transaction_artifacts_from_parsed`: four more iterations over the parsed transactions, computing one `SHA-256` per transparent output via `TransparentAddressScriptHash::of_script_pub_key` (`crates/zinder-core/src/transparent_utxo.rs:32-36`) and re-serializing each transaction once.
- `compact_block.encode_to_vec`: prost-encode the assembled lightwalletd compact block to bytes for the `CompactBlockArtifact` payload.

None of these steps hash Sapling note commitments, Orchard action commitments, or any commitment-tree root. The only cross-block state in the builder is two `u32` counters (Sapling and Orchard commitment-tree positions, advanced by the per-block output and action counts; `artifact_builder.rs:319-340`). Every other artifact field is a pure function of one source block.

This matters for the fix shape: the per-block work is heavy CPU plus one trivial running offset. Splitting derivation into a parallel-safe phase (everything above) and a serial fold over the two counters is what makes parallelism correct rather than racy. The detailed restructure lives in [ADR-0021](../adrs/0021-parallel-block-derivation.md).

## Why pipelining the fetch alone does not help

`futures_util::stream::buffered(N)` keeps up to N futures in flight and yields their results in submission order. That shape preserves ordering (which the commit path requires) at the cost of head-of-line blocking: a single slow consumer drains the buffer to zero and the next 31 fetches sit waiting for one consumer thread. From the consumer's point of view this is identical to a serial stream with a small prefetch.

The fetch is not the bottleneck. The proof is in the CPU split: Zebra at 1 %, indexer at 93 %. If fetch were the limit we would see the opposite (idle indexer, busy node).

## Resolution

[ADR-0021](../adrs/0021-parallel-block-derivation.md) splits derivation into a pure `derive_block` (parallel-safe) and a serial `finalize_derived_block` (folds the two `u32` counters). The runtime topology is `Stream::buffered + spawn_blocking`, with `ingest.derive.concurrency` capping the in-flight derive count. Startup derive replay uses the same concurrency cap when hydrating retained canonical events.

The unified ingest loop runs bulk catchup one commit batch at a time so it can re-classify phase and readiness after each commit. The backfill flush state is therefore carried across those one-batch calls; otherwise `flush_interval_epochs = 5` degenerates into "flush after every batch" and introduces long, hard-to-explain pauses.

Commit-time spend indexing and derive-context prevout hydration use first-class `transparent_prevout` rows from [ADR-0022](../adrs/0022-transparent-prevout-index.md). The normal writer path resolves `value_zat`, `script_pub_key`, address script hash, and producing block identity through exact current-projection reads, not by loading and deserializing historical transaction artifacts, scanning per-outpoint history, or joining through transparent UTXO rows.

The commit path also carries the parsed `zebra-chain` block from parallel derivation into derive-context assembly. That avoids reparsing 1000 raw block payloads serially while the batch is ready to commit.

Validation signals after the fix: ingest keeps more than one core busy during bulk catchup, the source node is not saturated, and stalls attribute cleanly to fetch, derive, subtree-root hydration, transparent-prevout lookup, store commit, or flush cadence through the metrics above. A sustained rate below the single-digit-hours target is a separate performance finding, not a hidden continuation of the original single-threaded consumer diagnosis.

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

Per-block derive cost (scrape the ingest container's `/metrics`):

```bash
curl -fsS http://127.0.0.1:9105/metrics \
  | grep -E 'zinder_ingest_(derive_duration_seconds|batch_accumulator_blocks|batch_transparent_prevout_store_lookup_outpoints|backfill_stage_duration_seconds|commit_stage_duration_seconds|derive_context_stage_duration_seconds|prevout_resolution_total|prevout_store_lookup_)'
```

`zinder_ingest_derive_duration_seconds` is the per-block CPU contribution to throughput. `zinder_ingest_batch_accumulator_blocks` is the current block depth of the in-flight commit batch; it should oscillate between `0` and `commit_batch_blocks` when the block budget is the active limit. `zinder_ingest_batch_transparent_prevout_store_lookup_outpoints` shows the other batch budget: unique out-of-batch transparent prevouts that the commit must read from the store.
If either batch gauge reaches its configured budget and stays there, inspect `zinder_ingest_commit_stage_duration_seconds` by `stage` to distinguish spend-address resolution, derive-context assembly, the store commit, and derive dispatch. If `build_derive_contexts` dominates, inspect `zinder_ingest_derive_context_stage_duration_seconds` by `stage`; `hydrate_blocks` should stay small when parsed blocks are carried from parallel derivation, and `resolve_prevouts` should mostly resolve through `zinder_ingest_prevout_resolution_total{source="indexed_prevout"}`. While `zinder_ingest_prevout_store_lookup_active{stage="derive_context"}` or `zinder_ingest_prevout_store_lookup_active{stage="spend_address_index"}` is `1`, the progress gauges show whether a transparent-prevout batch is still advancing chunk by chunk. If committed height stalls while commit stages look healthy, inspect `zinder_ingest_backfill_stage_duration_seconds` by `stage`; `await_derived_block` points at parallel derive head-of-line blocking, `populate_subtree_roots` points at subtree-root hydration, and `flush_store` points at RocksDB flush cadence or storage pressure.

Startup derive replay has separate counters because it runs before the
unified ingest loop emits normal bulk-catchup metrics:

```bash
curl -fsS http://127.0.0.1:9105/metrics \
  | grep -E 'zinder_ingest_derive_replay_(blocks_total|lag_blocks|stage_duration_seconds)'
```

Use `rate(zinder_ingest_derive_replay_blocks_total{status="ok"}[5m])`
for replay throughput, and
`zinder_ingest_derive_replay_stage_duration_seconds` by `stage` to see
whether startup is losing time reading chain events, hydrating blocks,
resolving prevouts, or dispatching derive consumers.

## References

- [ADR-0001: Use RocksDB for canonical storage](../adrs/0001-rocksdb-canonical-store.md)
- [ADR-0003: Canonical storage access boundary](../adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0015: Unified phase-driven ingest](../adrs/0015-unified-phase-driven-ingest.md): the design that commits to "single-digit hours" mainnet catchup.
- [ADR-0020: Bounded RocksDB resource budget](../adrs/0020-bounded-rocksdb-resource-budget.md)
- [ADR-0021: Parallel block derivation](../adrs/0021-parallel-block-derivation.md): the structural fix this investigation motivates.
- [Chain ingestion §Bulk catch-up throughput shape](../architecture/chain-ingestion.md#bulk-catch-up-throughput-shape)
- [`services/zinder-ingest/src/backfill.rs`](../../services/zinder-ingest/src/backfill.rs): the bulk-catchup loop.
- [`services/zinder-ingest/src/artifact_builder.rs`](../../services/zinder-ingest/src/artifact_builder.rs): the `ArtifactBuilder::build` implementation.
- [Bulk-catchup OOM recovery](../runbooks/bulk-catchup-oom-recovery.md): the related WAL-replay restart loop, fixed by ADR-0020.
