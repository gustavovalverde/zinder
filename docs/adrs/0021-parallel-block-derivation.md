# ADR-0021: Parallel canonical block prepare in bulk catchup

Status: Accepted
Date: 2026-05-20

## Context

Bulk catchup does CPU-heavy per-block work before canonical commit: block
deserialization, transaction fact extraction, compact-block construction,
transparent-output indexing, and logical-action counting. The work is mostly
independent per block, while final tree-size folding and storage commits must
remain ordered.

Running every block through one async task leaves available CPU idle and makes
source prefetch less useful. Running the whole commit path concurrently would
break deterministic epoch ordering and reorg invariants.

## Decision

Bulk catchup splits per-block work into a parallel block-prepare stage and an
ordered finalize/commit stage.

```text
ordered source segment
  -> parallel derive_block / block prepare on the blocking pool
  -> ordered windowed transparent-prevout resolution
  -> ordered finalize_derived_block fold
  -> canonical commit
```

`ingest.bulk_catchup.block_prepare_concurrency` bounds the number of in-flight
CPU-bound block builds. The default follows host parallelism and caps the
worker count so storage commit, source I/O, metrics, and the Tokio reactor
retain capacity.

The stage output is a typed canonical fact bundle. Commit still observes
source order, validates parent links, advances tree-size counters in order, and
writes one visible `ChainEpoch` at a time.

Transparent-prevout resolution runs after ordered reassembly, not inside each
parallel block task. The resolver briefly coalesces contiguous completed blocks,
resolves outputs created earlier in that window, then checks a recent-output
cache carried by the bulk-catchup stream. It issues one sorted, deduplicated
RocksDB multi-get for the remaining cold outpoints and distributes resolved
artifacts back to their consuming blocks. The recent-output cache shares the
block-prepare byte watermark, evicts oldest entries to admit new prepare work,
and is discarded with the stream on completion, restart, or error. Commit keeps
the authoritative fallback lookup.

## Consequences

CPU-bound block parsing and artifact construction scale across cores while the
canonical visibility boundary stays serial and deterministic.

Back-pressure is explicit. When block prepare is the bottleneck, the in-flight
block-prepare permits fill. When source fetch or storage commit is the bottleneck,
the corresponding metrics report that pressure without hiding it behind the
worker pool.

Dense transparent history no longer creates one independent RocksDB request per
block or guaranteed misses for outputs produced earlier in the active catchup
window. Light blocks use a 2 ms coalescing deadline; a head block with at least
128 transparent inputs raises that deadline to 20 ms, and the resolver closes
after including the block that reaches 2,048 inputs, or at the
prepare-concurrency width. This bounds added latency without turning a dense
two-block RocksDB request back into two single-block requests.

The same concurrency vocabulary applies to derive replay only when replay is
hydrating typed canonical block contexts. Derive replay has separate
`ingest.derive.replay_*` limits because it is rebuildable projection work, not
canonical ingest work.
