# ADR-0021: Parallel canonical fact build in bulk catchup

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

Bulk catchup splits per-block work into a parallel fact-build stage and an
ordered finalize/commit stage.

```text
ordered source segment
  -> parallel derive_block / fact build on the blocking pool
  -> ordered finalize_derived_block fold
  -> canonical commit
```

`ingest.bulk_catchup.fact_build_concurrency` bounds the number of in-flight
CPU-bound block builds. The default follows host parallelism and caps the
worker count so storage commit, source I/O, metrics, and the Tokio reactor
retain capacity.

The stage output is a typed canonical fact bundle. Commit still observes
source order, validates parent links, advances tree-size counters in order, and
writes one visible `ChainEpoch` at a time.

## Consequences

CPU-bound block parsing and artifact construction scale across cores while the
canonical visibility boundary stays serial and deterministic.

Back-pressure is explicit. When fact build is the bottleneck, the in-flight
fact-build permits fill. When source fetch or storage commit is the bottleneck,
the corresponding metrics report that pressure without hiding it behind the
worker pool.

The same concurrency vocabulary applies to derive replay only when replay is
hydrating typed canonical block contexts. Derive replay has separate
`ingest.derive.replay_*` limits because it is rebuildable projection work, not
canonical ingest work.
