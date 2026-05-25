# Bulk-catchup throughput: indexer slower than source node

> **Historical scope:** This document records the original bulk-catchup
> bottleneck investigation. [Fact-first indexer](../architecture/fact-first-indexer.md)
> owns the current canonical indexing architecture.

The original measurement-based finding was that `zinder-ingest` bulk catchup ran roughly one order of magnitude slower than its design target and slower than the upstream Zebra node it read from. In that run, the cause was structural single-threaded consumption, not configuration. Later fact-first work removed that bottleneck and exposed new bottlenecks; do not read the sections below as the active May 24 diagnosis.

## Why this exists

An indexer that reads from a fully validated node is doing strictly less work than the node itself. It does not validate proofs, does not re-verify signatures, does not run consensus. It reads block bytes, computes derived state, writes to storage. On the same host the indexer should be *faster* than its source node, not slower. The original run inverted that expectation: a mainnet sync that should have landed in single-digit hours took 2.6 to 4.4 days, while the source Zebra sat at 1 % CPU watching the indexer struggle. This was the architectural smell the rest of the historical document explains.

## Measured rate

Sampled live on a fully synced Z3 mainnet (Zebra tip `#3,348,233`) running on macOS Docker Desktop, 10 logical cores, 19.5 GiB VM. Sample window: 2 minutes between height samples.

| Sample | dt (s) | dh (blocks) | rate (blocks/sec) | ETA at this rate |
| --- | ---: | ---: | ---: | --- |
| 71-second window | 71 | 1,000 | **14.1** | 61 h (2.6 d) |
| 123-second window | 123 | 1,000 | **8.1** | 106 h (4.4 d) |

The two samples bracket a steady-state rate of roughly **8 to 14 blocks/sec**. The variance comes from block-size variance and the periodic RocksDB flush every `flush_interval_epochs × canonical_batch_max_blocks = 5,000` blocks ([ADR-0020](../adrs/0020-bounded-rocksdb-resource-budget.md)).

## Design target (for comparison)

[`docs/architecture/chain-ingestion.md`](../architecture/chain-ingestion.md#bulk-catch-up-throughput-shape) commits to a goal:

> Two structural choices keep the throughput high enough for a multi-million-block sync to complete in **single-digit hours rather than weeks**:
>
> 1. **Pipelined block fetches.** The historical implementation kept up to `ingest.bulk_catchup.fetch_concurrency` block fetches in flight via `futures_util::stream::iter(...).buffered(N)`.
> 2. **Concurrent per-block RPCs.** `getblockheader`, `getblock`, and `z_gettreestate` ran via `tokio::join!`.

That was the pre-segment source shape. The current implementation uses `NodeSource::fetch_chain_segment` with `ingest.bulk_catchup.source_segment_max_blocks`, so this investigation is historical evidence, not the current implementation contract.

For mainnet (3.3 M blocks) "single-digit hours" implies a sustained throughput of roughly **90 to 350 blocks/sec**. The observed range (8 to 14 blocks/sec) is **one order of magnitude below the floor** and **two orders below the ceiling** of that target.

## Original Measurement: Where The Time Went

Container CPU snapshot during steady catchup:

| Container | CPU % (10 cores available) | Memory |
| --- | ---: | ---: |
| `zinder-mainnet-zinder-ingest-1` | **93 %** | 1.8 GiB |
| `z3-mainnet-zebra-1` | 1.1 % | 0.9 GiB |
| `zinder-mainnet-zinder-query-1` (reader) | 0.5 % | 12.3 GiB |
| `zinder-mainnet-zinder-explorer-1` (reader) | 0.0 % | 0.5 GiB |

Two facts dominated that run:

1. **The source node was idle.** Zebra served the indexer at 1 % CPU and could push data faster by orders of magnitude. In that run, the bottleneck was not upstream availability.
2. **The indexer was single-threaded.** 93 % of one core, out of 1000 % available. The container had 10 cores; nine were idle. Pipelined fetch (`buffered(32)`) was doing its job for network I/O, but every block then had to traverse a single consumer thread end-to-end.

## Code-level cause

The historical hot loop was at [`services/zinder-ingest/src/bulk_catchup/mod.rs`](../../services/zinder-ingest/src/bulk_catchup/mod.rs):

```rust
let mut block_stream = futures_util::stream::iter(BlockHeightRange::inclusive(...))
    .map(|height| async move { fetch_block_with_retry(...).await })
    .buffered(fetch_concurrency);                                // 32 parallel fetches

while let Some(fetch_result) = block_stream.next().await {
    let source_block = fetch_result?;
    let built_artifacts = artifact_builder.build(&source_block)?;  // synchronous CPU
    // … push into `batch` …
    if batch.len() == canonical_batch_max_blocks {
        populate_subtree_root_artifacts(...).await?;               // serial RPCs
        commit_finalized_bulk_catchup_batch(store, … &mut batch)?;     // serial RocksDB write
        if epochs_since_last_flush >= flush_interval_epochs {
            flush_primary_chain_store(store).await?;
        }
    }
}
```

`artifact_builder.build` (`services/zinder-ingest/src/artifact_builder.rs:330`) runs synchronously on the consumer task. Its per-block work is:

- `zcash_deserialize_into::<ZebraBlock>` (`artifact_builder.rs:421-427`): walk the entire raw block byte stream, allocate every transaction structure, and verify deserialized identity matches the source-supplied header.
- Per-transaction `zcash_serialize_to_vec` (`artifact_builder.rs:401`):
  re-emits each transaction's canonical bytes only when the raw-blob policy
  writes `transaction_blob`; the hot path stores parsed transaction facts and
  locations instead of raw transaction payloads.
- `compact_transactions` (`artifact_builder.rs:479-503`): iterate every transaction once more, collect `cmu`, `ephemeral_key`, and a 52-byte ciphertext prefix per Sapling output and Orchard action, plus the prevout outpoints and full `script_pub_key` bytes for transparent inputs and outputs. Tree-size deltas (counts of Sapling outputs and Orchard actions) are tallied here.
- `address_output_artifacts` and `derive_transaction_artifacts_from_parsed`: additional passes over the parsed transactions, computing one `SHA-256` per transparent output via `TransparentAddressScriptHash::of_script_pub_key` (`crates/zinder-core/src/transparent_output.rs`). Raw transaction bytes are serialized only when `storage.raw_blob_policy` enables transaction blobs.
- `compact_block.encode_to_vec`: prost-encode the assembled lightwalletd compact block to bytes for the `CompactBlockArtifact` payload.

None of these steps hash Sapling note commitments, Orchard action commitments, or any commitment-tree root. The only cross-block state in the builder is two `u32` counters (Sapling and Orchard commitment-tree positions, advanced by the per-block output and action counts; `artifact_builder.rs:319-340`). Every other artifact field is a pure function of one source block.

This matters for the fix shape: the per-block work is heavy CPU plus one trivial running offset. Splitting derivation into a parallel-safe phase (everything above) and a serial fold over the two counters is what makes parallelism correct rather than racy. The detailed restructure lives in [ADR-0021](../adrs/0021-parallel-block-derivation.md).

## Why Fetch-Only Pipelining Was Not Enough In The Original Run

`futures_util::stream::buffered(N)` keeps up to N futures in flight and yields their results in submission order. That shape preserves ordering (which the commit path requires) at the cost of head-of-line blocking: a single slow consumer drains the buffer to zero and the next 31 fetches sit waiting for one consumer thread. From the consumer's point of view this is identical to a serial stream with a small prefetch.

In that run, fetch was not the bottleneck. The proof was in the CPU split: Zebra at 1 %, indexer at 93 %. If fetch had been the limit, we would have seen the opposite: idle indexer and busy node.

The May 24 follow-up is different. Zebra is unhealthy, `getblock` tail latency is high, and source fetch is now part of the active bottleneck. See the current Zinder-side plan linked at the top of this document.

## Resolution

[ADR-0021](../adrs/0021-parallel-block-derivation.md) splits derivation into a pure `derive_block` (parallel-safe) and a serial `finalize_derived_block` (folds the two `u32` counters). The runtime topology is `Stream::buffered + spawn_blocking`, with `ingest.bulk_catchup.block_prepare_concurrency` capping the in-flight derive count. Startup derive replay uses the same concurrency cap and chunks retained chain events with `ingest.derive.replay_batch_blocks`.

The unified ingest loop runs bulk catchup one commit batch at a time so it can re-classify phase and readiness after each commit. The bulk-catchup flush state is therefore carried across those one-batch calls; otherwise `flush_interval_epochs = 5` degenerates into "flush after every batch" and introduces long, hard-to-explain pauses.

Commit-time spend indexing uses first-class `transparent_output` rows. The
normal writer path resolves `value_zat`, address script hash, and producing
block identity through exact current-projection reads, not by loading and
deserializing historical transaction artifacts or scanning per-outpoint history.
The raw previous-output `script_pub_key` stays on `transparent_output`; it is
not duplicated into `transparent_spend_fact`.

The commit path also carries the parsed `zebra-chain` block from parallel derivation into derive-context assembly. That avoids reparsing 1000 raw block payloads serially while the batch is ready to commit.

Validation signals after the fix: ingest keeps more than one core busy during bulk catchup, the source node is not saturated, and stalls attribute cleanly to fetch, derive, subtree-root hydration, transparent-output lookup, store commit, or flush cadence through the metrics above. A sustained rate below the single-digit-hours target is a separate performance finding, not a hidden continuation of the original single-threaded consumer diagnosis.

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
  | grep -E 'zinder_ingest_(derive_duration_seconds|batch_accumulator_(blocks|transactions|transparent_outputs|transparent_spend_references|estimated_write_bytes)|bulk_pipeline_(stage_duration_seconds|queue_bytes|reorder_buffer_bytes|watermark_blocked_total)|commit_(stage_duration_seconds|batch_(block|transaction|transparent_output|transparent_spend_reference)_count|batch_estimated_write_bytes)|derive_replay_(stage_duration_seconds|budget_state|effective_batch_blocks|lag_blocks)|derive_tailer_tick_duration_seconds|transparent_spend_fact_|raw_blob_disabled_total)'
```

`zinder_ingest_block_prepare_duration_seconds` is the per-block prepare contribution to throughput: derivation plus spent-transparent-output prefetch in bulk catchup, and sequential derivation plus finalization in tip follow. `zinder_ingest_batch_accumulator_blocks` is the current block depth of the in-flight commit batch; it should oscillate between `0` and `canonical_batch_max_blocks` when the block budget is the active limit. Dense ranges can hit the estimated-write budget first; in that case `zinder_ingest_batch_accumulator_estimated_write_bytes` rises to its budget and `zinder_ingest_batch_commit_trigger_total{trigger="estimated_write_bytes"}` increments. Raw transaction, transparent-output, and transparent-spend-reference accumulators are diagnostic density signals, not independent closing budgets.
If an accumulator reaches its configured budget and stays there, inspect `zinder_ingest_commit_stage_duration_seconds` by `stage` to distinguish canonical commit latency from upstream fetch or parser stalls. Derive work is no longer a commit stage; inspect `zinder_ingest_derive_tailer_tick_duration_seconds`, `zinder_ingest_derive_replay_stage_duration_seconds`, `zinder_ingest_derive_replay_lag_blocks`, and `zinder_ingest_transparent_spend_fact_read_total` to determine whether the asynchronous tailer is falling behind. If committed height stalls while commit stages look healthy, inspect `zinder_ingest_bulk_pipeline_stage_duration_seconds` by `stage`; `canonical_block_prepare` points at CPU-bound derivation, `subtree_root_attachment` points at subtree-root hydration, `checkpoint_tree_state` points at tree-state fetch latency, and `canonical_flush` points at RocksDB flush cadence or storage pressure.

Derive replay has separate counters because it tails durable chain events
outside the canonical commit critical path:

```bash
curl -fsS http://127.0.0.1:9105/metrics \
  | grep -E 'zinder_ingest_derive_(tailer_tick_duration_seconds|tailer_ticks_total|replay_(blocks_total|lag_blocks|stage_duration_seconds))'
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
- [`services/zinder-ingest/src/bulk_catchup/mod.rs`](../../services/zinder-ingest/src/bulk_catchup/mod.rs): the bulk-catchup loop.
- [`services/zinder-ingest/src/artifact_builder.rs`](../../services/zinder-ingest/src/artifact_builder.rs): the `ArtifactBuilder::build` implementation.
- [Bulk-catchup OOM recovery](../runbooks/bulk-catchup-oom-recovery.md): the related WAL-replay restart loop, fixed by ADR-0020.
