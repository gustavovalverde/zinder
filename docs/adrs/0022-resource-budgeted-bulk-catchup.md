# ADR-0022: Resource-budgeted bulk catchup and checkpoint tree state

## Status

Accepted.

## Context

Mainnet catchup does not have a stationary block profile. Pre-Sapling,
Sapling, NU5, and later branches differ enough that fixed block-count knobs
mis-size the real resources that bound sync speed. The failure mode is a batch
sized in blocks or transparent-spend references crossing a byte, CPU, or
memory boundary that configuration does not express.

Tree-state reads have the same issue in the storage contract. Storing
`z_gettreestate` for every block puts arbitrary per-height archival in the
canonical hot path even though the wallet-facing requirement is latest or
checkpoint tree state.

## Decision

Bulk catchup is a resource-budgeted staged pipeline:

```text
SourceFetchStage
  -> CanonicalBlockPrepareStage
  -> CanonicalPrevoutResolveStage
  -> CanonicalFinalizeStage
  -> SubtreeRootAttachmentStage
  -> CanonicalCommitStage
  -> CanonicalFlushStage
```

The source boundary is `SourceChainSegmentLimits { cursor,
max_connected_blocks, target_response_bytes, max_response_bytes }`.
`NodeSource::fetch_chain_segment` fetches raw block bytes only. Tree state is a
separate source call, `fetch_tree_state_for_block(block_id)`, and JSON-RPC
adapters reject a tree-state response whose height or hash does not match the
requested block.

Bulk catchup uses byte-watermarked source fetch config:

- `source_segment_max_blocks = 64`
- `source_segment_target_response_bytes = 33554432`
- `source_fetch_max_in_flight_requests = 12`
- `source_fetch_max_in_flight_bytes = 402653184`
- `block_prepare_concurrency = min(available_parallelism, 16)`
- `block_prepare_max_in_flight_artifact_bytes = 536870912`
- `commit_reassembly_max_queued_artifact_bytes = 536870912`
- `canonical_batch_max_blocks = 1000`
- `canonical_batch_max_artifact_bytes = 536870912`
- `canonical_batch_max_estimated_write_bytes = 536870912`
- `canonical_batch_min_blocks_before_estimated_write_close = 100`
- `flush_interval_epochs = 5`

The segment sizer uses observed response bytes per block, p95 density, overshoot
memory after split attempts, and network-upgrade resets. The JSON-RPC response
default is 64 MiB, so the default segment target is 32 MiB. Source fetch and
block prepare may complete out of order, but ordered reassembly is the only
place that releases blocks to the prevout and serial finalization boundaries.
Block prepare derives canonical artifacts. The ordered prevout resolver uses
same-window outputs and a recent-output cache before issuing one deduplicated
multi-get for the window's remaining cold outpoints; canonical commit still
performs the authoritative fallback lookup. The cache shares
`block_prepare_max_in_flight_artifact_bytes`, so cache entries yield to new
prepare work instead of creating another independent memory ceiling. Completed
out-of-order source segments keep their measured-byte reservation until emitted.
The first density probe reserves `node.max_response_bytes`. Later requests
reserve the larger of the response target or 1.5 times the density prediction,
capped at `node.max_response_bytes`, then resize to the measured response after
decode. `source_fetch_max_in_flight_bytes` is therefore an admission watermark
over conservative predictions plus completed reassembly bytes, while
`source_fetch_max_in_flight_requests * node.max_response_bytes` remains the
absolute active-response bound. A larger-than-predicted response can
temporarily take the watermark over its admission limit; no more work is
admitted until retained bytes fall below the limit. Config validation still
requires enough room for the initial maximum-sized probe.

The durable writer remains serial, but subtree-root attachment, checkpoint
tree-state fetch, canonical commit, and flush run as one in-flight commit
future while source fetch and block prepare continue filling the next batch.
`commit_reassembly_max_queued_artifact_bytes` bounds that next batch so commit
overlap cannot become unbounded memory growth.

Canonical storage writes tree state only at committed canonical epoch tips.
Tip-follow writes one checkpoint per live committed tip. Bulk catchup fetches
one checkpoint tree state for the batch tip before commit. Missing tree-state
source capability does not block canonical catchup, but the wallet tree-state
capabilities are unavailable until checkpoint rows exist.

The native wallet API exposes checkpoint-oriented reads:

- `tree_state_checkpoint_at_or_before(max_height, at_epoch)`
- `latest_tree_state_checkpoint(at_epoch)`

The advertised capabilities are `wallet.read.tree_state_checkpoint_v1` and
`wallet.read.latest_tree_state_checkpoint_v1`.

Lightwalletd compatibility stays explicit: `GetLatestTreeState` returns the
latest checkpoint, and `GetTreeState(BlockID)` returns a tree state only when
the requested height is an exact stored checkpoint. Non-checkpoint heights
return `NOT_FOUND`.

## Consequences

Operators tune budgets in the units that bound throughput and memory, not by
guessing which chain height happens to be dense. Network upgrades can change
density without requiring per-height code patches.

Native clients read latest or checkpoint tree state. The native API does not
serve arbitrary per-height tree state.

The canonical ingest vocabulary is `CanonicalBatch`, `CanonicalBatchBudget`,
`CanonicalBatchCost`, `CanonicalBatchCloseTrigger`, and
`block_prepare_concurrency`. `derive_replay_*` names are reserved for the async
derive replay plane.

Bulk-catchup observability uses stage labels from this ADR:
`source_fetch`, `canonical_block_prepare`, `canonical_prevout_resolve`, `canonical_finalize`,
`subtree_root_attachment`, `checkpoint_tree_state`, `commit_reassembly`,
`canonical_commit`, and `canonical_flush`.

## Revision: container-aware default queue caps (2026-05-26)

The four bulk-catchup queue byte-caps (`source_fetch_max_in_flight_bytes`,
`block_prepare_max_in_flight_artifact_bytes`,
`commit_reassembly_max_queued_artifact_bytes`,
`canonical_batch_max_estimated_write_bytes`) shipped as fixed constants
(~512 MiB) sized for hosts with plenty of headroom. Deployed inside a
24 GiB container (Railway, Fly, mid-tier ECS), the documented worst-case
in-flight envelope of `commit + prepare + reassembly + next-batch` runs
~2.4 GiB of watermark, which then amplifies through decoded-artifact
structures, in-flight commit futures, and RocksDB write buffers into
container memory exhaustion during dense mainnet ranges (observed on
2026-05-26 around blocks 297-298k: `estimated_write_bytes=510 MB` batch
correlated with 22.7 GiB resident memory at a 24 GiB cap, ending in a
SIGTERM from the container runtime).

The queue-cap defaults now derive from the container memory budget at
startup. `services/zinder-ingest/src/memory_pressure.rs` exposes
`container_memory_budget_bytes()`, which returns `memory.high` (preferred)
or `memory.max` from cgroup v2; `services/zinder-ingest/src/config.rs`
computes each queue cap as `container_budget / 64`, clamped to
`[128 MiB, original fallback]`. On a 24 GiB Railway container each queue
shrinks from 512 MiB to 384 MiB; on dev hosts without cgroup the
fallback constants apply unchanged.

The `ZINDER_INGEST__BULK_CATCHUP__*_BYTES` env-var overrides still take
precedence over the auto-derived default. No new env var, no new ADR.
The mechanism reuses the existing `RuntimeMemorySnapshot` sampler that
already feeds the derive-replay backpressure ratios.
