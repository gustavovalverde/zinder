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
  -> CanonicalFactBuildStage
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

- `source_segment_max_blocks = 16`
- `source_segment_target_response_bytes = 33554432`
- `source_fetch_max_in_flight_requests = 12`
- `source_fetch_max_in_flight_bytes = 402653184`
- `fact_build_concurrency = min(available_parallelism, 16)`
- `fact_build_max_in_flight_artifact_bytes = 536870912`
- `commit_reassembly_max_queued_artifact_bytes = 536870912`
- `canonical_batch_max_blocks = 1000`
- `canonical_batch_max_artifact_bytes = 536870912`
- `flush_interval_epochs = 5`

The segment sizer uses observed response bytes per block, p95 density, overshoot
memory after split attempts, and network-upgrade resets. The JSON-RPC response
default is 64 MiB, so the default segment target is 32 MiB. Source fetch and
fact build may complete out of order, but ordered reassembly is the only place
that releases blocks to the serial finalization boundary. Completed out-of-order
source segments keep their byte reservation until emitted. Each request reserves
`node.max_response_bytes` before it is sent, then shrinks to the measured
response size after the segment is decoded. `source_fetch_max_in_flight_bytes`
therefore bounds both worst-case active responses and completed reassembly
bytes; config validation requires it to be at least `node.max_response_bytes`.

The durable writer remains serial, but subtree-root attachment, checkpoint
tree-state fetch, canonical commit, and flush run as one in-flight commit
future while source fetch and fact build continue filling the next batch.
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
`fact_build_concurrency`. `derive_replay_*` names are reserved for the async
derive replay plane.

Bulk-catchup observability uses stage labels from this ADR:
`source_fetch`, `canonical_fact_build`, `canonical_finalize`,
`subtree_root_attachment`, `checkpoint_tree_state`, `commit_reassembly`,
`canonical_commit`, and `canonical_flush`.
