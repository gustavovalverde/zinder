# Chain Ingestion

Chain ingestion turns upstream node state into canonical Zinder artifacts. It must be deterministic, restartable, and reorg-aware.

The source-event and post-commit event vocabulary is defined in [Chain events](chain-events.md). This document owns the ingestion responsibilities and invariants.

Source adapter ownership is defined in [Node source boundary](node-source-boundary.md). Protocol ownership is defined in [Protocol boundary](protocol-boundary.md).

## Operation Shape

```text
NodeSource
  -> observe_chain_source
  -> fetch_missing_ancestors
  -> select_best_chain
  -> build_block_artifacts
  -> build_compact_block_artifacts
  -> commit_ingest_batch
  -> finalize_tip_if_ready
```

These names describe operations, not required files, structs, or tasks. The implementation should prefer deep modules with small public interfaces over one shallow module for every operation in the diagram.

## Canonical Artifacts

Zinder should treat artifacts as durable products of ingestion, not incidental cache entries.

Required artifact families:

- `BlockHeaderArtifact`: canonical block-header facts and block links.
- `CompactBlockArtifact`: wallet-oriented compact block representation.
- `TreeStateArtifact`: tree state data required by wallet sync APIs.
- `BlockTransactionIndexArtifact`, `TransactionLocation`, and `TransactionFactsArtifact`: transaction ordering, location, and typed public facts needed by APIs.
- `MempoolIndex` / `MempoolEventLog`: non-canonical mempool view and event stream, implemented outside `commit_ingest_batch`. The live index is in-memory; the event log persists through the `mempool_event` column family per [ADR-0007](../adrs/0007-mempool-topology-and-retention.md). Both are owned by `zinder-ingest` and spawn the first time the unified ingest loop enters the `TipFollow` phase (see §Phase transitions below). The mempool orchestrator (`run_mempool_orchestrator`) is a sibling of the ingest loop in the writer process: it consumes a `MempoolSource` stream, hydrates each observation through `build_mempool_entry`, and writes typed `Added`/`Invalidated`/`Mined` envelopes to `MempoolEventLog`. A separate retention worker (`spawn_mempool_event_retention_task`) prunes per-variant windows and emits `MempoolCursorAtRisk` readiness when the oldest retained sequence approaches the configured floor; this is the mempool-side equivalent of [`spawn_chain_event_retention_task`](chain-events.md#retention-and-backpressure).

Each artifact must include:

- Network.
- Source block hash and height.
- Artifact schema version.
- Commit epoch.
- Source metadata when available.

## Artifact Byte Contracts

Artifact bytes follow [ADR-0002](../adrs/0002-boundary-specific-serialization.md):

- Ordered storage keys use fixed big-endian layouts owned by `zinder-store`.
- Artifact values use a fixed `ArtifactEnvelopeHeaderV1` followed directly by payload bytes.
- Compact block payloads use protobuf bytes compatible with vendored Zcash wallet protos.
- Durable storage-control records use storage-specific protobuf messages, not RPC messages.
- Derived read caches may experiment with `rkyv` only after the validation gate in ADR-0002.

Artifact builders consume normalized source values. They must not hand-parse consensus-critical block headers, transaction bytes, or compact-block wire payloads. Parsing belongs behind maintained Zcash consensus primitives inside `zinder-source` or ingestion adapters; generated protocol payloads belong in `zinder-proto`. The current parser boundary is `zebra-chain`, with a root-manifest `core2` source patch only to satisfy Zebra's current transitive `equihash` resolver path.

## Real Compact Block Construction

The compact-block builder is the primary ingestion boundary because wallets
cannot sync from empty protobuf shells.

The builder must:

- Parse raw block bytes through maintained Zcash consensus primitives rather
  than local offset math or a new hand-rolled transaction parser.
- Extract the lightwalletd-compatible fields needed for shielded wallet sync:
  block identity fields, compact transaction entries, Sapling spend data,
  Sapling output data, Orchard action data, commitment-tree sizes, and any
  header field required by the pinned lightwallet protocol.
- Keep parser-specific types out of `zinder-core`,
  `zinder-store`, and public query APIs.
- Store durable `CompactBlockArtifact` payload bytes during ingestion.
- Reject source/artifact mismatches before commit, including height, hash,
  parent hash, and compact-block metadata disagreements.
- Use real regtest fixtures first, then add testnet or mainnet corpus fixtures
  before claiming public-network wallet compatibility.

`zinder-query` and `zinder-compat-lightwalletd` may decode and re-encode stored
payload bytes through generated protobuf types, but they must not build compact
blocks on demand.

Current status: `zinder-source` and `zinder-ingest` parse raw block bytes
with `zebra-chain`. The builder extracts block identity, ordered compact
transactions, transparent data, Sapling compact fields, Orchard compact fields,
and stateful tree-size metadata for contiguous bulk-catchup ranges. Subtree roots and
latest tree state remain separate artifacts, not fields to reconstruct at query
time.

Commitment-tree sizes must be chain-global. A fresh bulk-catchup run may start at
height 1, an existing store may append immediately after its current tip, and a
checkpoint-bounded bulk-catchup run may start at `SourceChainCheckpoint.height + 1` after
seeding the builder from the checkpoint's `ChainTipMetadata`. Arbitrary
non-genesis or non-contiguous bulk-catchup ranges still fail closed unless they are backed
by a resolved upstream node checkpoint.

## Chain Epochs

`ChainEpoch` is the visibility boundary between ingestion and readers.

An epoch becomes visible only after:

- All required artifacts for the epoch are written.
- Parent and child links are internally consistent.
- Compact block artifacts match their source blocks.
- Reorg-window metadata is updated.
- Finalized prefix metadata is updated.
- The commit transaction succeeds.

Readers should either see the old epoch or the new epoch. They should not see a half-committed epoch.

## Reorg State Machine

Reorgs are normal control flow, not exception paths. The pipeline is the same as in §Operation Shape; reorg-specific invariants:

- Reorgs inside the configured window apply by replacing non-finalized state.
- Reorgs beyond the configured window fail closed with `ReorgWindowExceeded` and require operator intervention.
- When a source exposes competing branches, best-chain selection uses cumulative chainwork, not tip height. The current polling source observes one upstream-node-selected best chain and validates parent-hash continuity.
- Empty-chain startup is a first-class state through `ChainEpoch::empty()`. Genesis, height 1, and short regtest chains are valid inputs, not exceptional cases.
- Derived indexes receive `ChainEvent` values with explicit reverted and committed ranges (see [Chain events](chain-events.md)).
- Query readers never observe partially reverted state.

## Bulk Catch-up and Tip Following

The unified ingest loop classifies its work into one of three phases at every iteration: `AwaitingUpstream`, `BulkCatchup`, or `TipFollow`. Phase selection is internal to `zinder-ingest`; operators run one binary and the loop dispatches based on the gap between the visible chain epoch and the upstream tip. The architectural decision lives in [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md).

All phases share the same artifact builders and commit path. The source adapter is identical across phases. What differs is fetch shape (pipelined vs serial) and commit shape (`FinalizeThrough` vs `Extend`/`Replace` plus a separate finalization step).

Source capability detection happens before processing starts. If the selected source cannot provide required data such as finalized height, chainwork, non-finalized blocks, or transaction broadcast support, ingestion fails closed with a typed startup or readiness cause.

### Phase transitions

The classifier reads two inputs each iteration: the store's `current_chain_epoch.tip_height` (cached, cheap) and the upstream tip (one `NodeSource::tip_id` call). The decision rule:

- `gap_blocks > ingest.phases.catchup_threshold_blocks` (defaults to `reorg_window_blocks`): `BulkCatchup`. The source adapter returns bounded `SourceChainSegment`s with up to `ingest.bulk_catchup.source_segment_max_blocks` connected raw blocks, while the writer adapts the requested count from observed source response bytes and consensus-branch changes. Batches are bounded by block count, artifact bytes, and canonical work cost, and the commit transition is `FinalizeThrough { tip_height: target }` against `min(upstream_tip - reorg_window, store_tip + canonical_batch_max_blocks)`.
- `gap_blocks <= ingest.phases.catchup_threshold_blocks` and upstream tip above the catch-up floor: `TipFollow`. Serial fetches, one block per commit, transition `Extend` or `Replace`. `finalize_tip_if_ready` advances the finalized boundary once a tip is older than `reorg_window_blocks`.
- Upstream tip below catch-up floor (regtest near genesis, freshly initialized node): `AwaitingUpstream`. The loop polls on the upstream-health interval and emits `cause=upstream_not_ready` until enough chain exists to commit.

Transitions are bidirectional. A long downtime or upstream burst that re-opens the gap beyond the threshold returns the loop to `BulkCatchup` until it closes. The mempool orchestrator, retention worker, and chain-tip notification stream spawn once on first entry into `TipFollow` and stay running across subsequent bounces. The `IngestControl` gRPC server starts at process start and runs throughout.

### Bulk-catch-up throughput shape

Bulk catch-up reaches single-digit-hours mainnet sync through bounded source fetch, parallel canonical block prepare, resource-bounded canonical batches, and bounded derive replay. RocksDB writes stay strictly ordered (atomic per chain epoch, per ADR-0001 and ADR-0020); artifact assembly upstream of the writer runs on a worker pool (per [ADR-0021](../adrs/0021-parallel-block-derivation.md)).

1. **Bytes-adaptive source segments.** `NodeSource::fetch_chain_segment` accepts `SourceChainSegmentLimits` and fetches raw block bytes only. Returned `SourceChainSegment` values carry advisory density stats: connected block count, response payload bytes, and split count. Bulk catchup targets `ingest.bulk_catchup.source_segment_target_response_bytes`, sizes from p95 bytes per block plus overshoot memory, grows after sustained success, and clears density samples when the consensus branch changes. The JSON-RPC adapter splits oversized ranges and retries smaller ordered ranges; a single-block oversize is a configuration error.
2. **Byte-watermarked source prefetch with ordered reassembly.** Source segment requests complete out of order through `FuturesUnordered`, then `source segment reassembly` yields blocks in canonical height order. The active-request budget is `source_fetch_max_in_flight_requests`; `source_fetch_max_in_flight_bytes` covers active worst-case response reservations plus completed out-of-order source bytes waiting for earlier heights. Each active request reserves `node.max_response_bytes` before fetch and releases or shrinks the reservation only after the response is decoded, so dense ranges apply back-pressure by shrinking segment size or pausing scheduling instead of expanding process memory without limit.
3. **Resource-bounded canonical batches.** `canonical_batch_max_blocks` is an upper bound, not the only trigger. The in-flight canonical batch also commits when it reaches `canonical_batch_max_artifact_bytes`, or when `canonical_batch_max_estimated_write_bytes` is reached after `canonical_batch_min_blocks_before_estimated_write_close`. Raw transaction, transparent-output, and transparent-spend-reference counts remain metrics for diagnosis; they are not separate batch-closing contracts. Dense ranges write smaller chain epochs before RocksDB write-batch construction can consume the cgroup memory budget, while transparent-input-heavy historical ranges avoid collapsing into tiny commits.
4. **Parallel canonical block prepare with ordered reassembly.** Each connected block from the segment is handed to a block-prepare worker. The worker derives canonical block artifacts and prefetches already-visible spent transparent outputs before the block enters ordered reassembly. Up to `ingest.bulk_catchup.block_prepare_concurrency` blocks prepare concurrently, and `block_prepare_max_in_flight_artifact_bytes` bounds active plus completed derived artifacts and prefetched output payloads. Completed prepared blocks can return out of order, but `block-prepare reassembly` emits them in canonical height order before the serial `finalize_derived_block` fold. Commit still performs the authoritative fallback lookup for same-batch outputs and outputs that became visible after an overlapped previous commit.
5. **Overlapped commit under bounded reassembly.** Subtree-root attachment, checkpoint tree-state fetch, canonical commit, and optional flush remain serial for each chain epoch, but the source and block-prepare stages can continue while one commit is in flight. `commit_reassembly_max_queued_artifact_bytes` bounds the next finalized batch while the previous batch is attaching metadata, committing, or flushing.
6. **Bounded derive replay.** Startup derive replay uses `ingest.derive.replay_batch_blocks`, `ingest.derive.replay_policy`, and memory watermarks under `[ingest.derive]`. Canonical-first replay shrinks the effective batch size at `memory_degrade_ratio`, pauses at `memory_pause_ratio`, resumes from pause as degraded work below `memory_pause_ratio`, and returns to the normal batch size below `memory_resume_ratio`; `min_replay_batch_blocks` is the lowest degraded batch size before pause.

Tip-follow stays serial: it commits one block per poll because by definition it is following the tip, where pipelining offers no headroom. The same `derive_block` and `finalize_derived_block` functions feed the tip-follow loop, just sequentially.

The loop treats every upstream-source failure as a readiness transition rather than a process lifecycle event. It consults `decide_recovery` per [ADR-0013](../adrs/0013-source-failure-recovery-topology.md), reports `node_unavailable` with a structured `NodeUnavailableDetail` payload, backs off according to the failure class, and resumes from the current visible chain epoch. Committed batches are durable, so the retry does not replay from the wallet-serving floor after every transient outage.

### Tip-follow wakeups

Tip-follow's default wake-up signal is a polling interval, but when the operator sets `ZINDER_NODE__INDEXER_GRPC_ADDR=http://<zebra>:8155` the loop also subscribes to Zebra's `Indexer.ChainTipChange` gRPC stream. Each push notification wakes the loop and triggers an immediate iteration against the JSON-RPC source for block bytes plus one checkpoint tree-state fetch for the committed tip. The polling interval stays in the `tokio::select!` as a safety net: a transient stream failure, missed reconnect, or failed re-subscription cannot stall ingest beyond `ingest.tip_follow.poll_interval_ms`.

Every upstream-source failure is a readiness event, not a process lifecycle event. If Zebra is restarting, warming up, reorging near the tip, or unreachable mid-iteration, `zinder-ingest` reports `node_unavailable` with a `failure_class` payload, keeps `/healthz` alive, returns not-ready on `/readyz`, and continues retrying. Storage errors and reorg-window violations still fail closed; protocol mismatches and missing capabilities stay alive in a typed operator-action readiness state for inspection. See [ADR-0013](../adrs/0013-source-failure-recovery-topology.md).

### Upstream sync detection

`zinder-ingest` distinguishes "we're up-to-date with Zebra" from "Zebra is itself at the real network tip" through a dual-path probe:

- **Primary**: when `[node.health].addr` is set, the loop polls Zebra's HTTP `/ready` endpoint every `node.health.poll_interval_ms` (default 30000). Zebra returns `200 OK` when it is near tip with sufficient peers and a fresh tip; otherwise `503` with a sentinel body (`syncing`, `no tip`, `tip_age=<N>s`, `lag=<N> blocks`, `insufficient peers`).
- **Fallback**: when `[node.health].addr` is unset or all probes fail, the loop derives the same signal from `getblockchaininfo.verificationprogress < node.health.verification_progress_floor` (default 0.999) or `estimated_height - blocks > node.health.estimated_gap_floor_blocks` (default 10). Less authoritative because both fields come from wall-clock extrapolation of the local tip's timestamp rather than peer-reported headers; operators running Zebras with the health endpoint enabled should configure `[node.health].addr` for the precise signal.

When upstream-not-ready, the loop still commits whatever blocks Zebra has made available. The readiness surface gates traffic: `cause=upstream_not_ready` with structured details (`upstream_committed_height`, `upstream_estimated_height`, `upstream_verification_progress`, `upstream_health.source`, `upstream_health.reason`) is emitted on `/readyz` until the upstream catches up. See [ADR-0015 §Upstream sync detection](../adrs/0015-unified-phase-driven-ingest.md#upstream-sync-detection).

### Subtree roots and checkpoint bootstrap

The bulk-catch-up phase also fetches newly completed shielded subtree roots through
the source boundary. The source adapter returns `z_getsubtreesbyindex`
data without a completing block hash, so `zinder-ingest` binds each returned
root to the block artifact that completed it before committing
`SubtreeRootArtifact` values. Query and compatibility code must not repair
missing subtree roots by calling the upstream node.

Checkpoint bootstrap initializes the running shielded tree-size observer from
the checkpoint `ChainTipMetadata`. Canonical ingest stores tree-state payloads
only at committed epoch tips and the latest tip; query and compatibility code
must not repair missing checkpoint tree state by calling the upstream node.
This page owns the durable ingestion requirement.

### Wallet-serving coverage

Wallet-serving coverage is an explicit coverage mode, not an operator folklore
recipe. `zinder-ingest --wallet-serving` derives the historical floor
from upstream-node-advertised activation heights in `getblockchaininfo`, resolves a
checkpoint at `floor - 1`, and starts canonical artifact ingestion at the floor.
The current floor is the earliest shielded-pool activation the upstream node
advertises, so fresh lightwalletd-compatible wallets can request subtree roots
from index 0 and tree states at flow-selected anchor heights without hitting a
recent-checkpoint store gap. Do not encode public-network activation constants
inside Zinder docs or config examples; the upstream node remains the source of
truth, including Regtest and custom Testnet activation schedules. The shared
[`NetworkUpgradeActivations`](public-interfaces.md#networkupgradeactivations) is the
in-process carrier: every component that needs an activation height, the
active upgrade name, or a consensus branch id (the lightwalletd
`GetLightdInfo` shim and the native `MinedDetails.consensus_branch_id` read
path included) reads it from a process-startup
`Arc<NetworkUpgradeActivations>` populated by
`ZebraJsonRpcSource::discover_network_upgrade_activations()`, never from
library-default constants. See
[ADR-0008](../adrs/0008-network-parameter-discovery.md).

The derived floor does not relax the finality bound on bulk catch-up. Wallet-serving
stores reach `upstream_tip - reorg_window_blocks` in the bulk phase, then
transition to tip-follow for the replaceable near-tip suffix. Per
[ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md),
`--allow-near-tip-finalize` is invalid with `--wallet-serving`; use it only
with explicit local or disposable stores.

The loop retries retryable source failures with exponential backoff,
a per-block source deadline, and a per-run retryable failure budget. Retryable
failures are transport/readiness shaped, such as source unavailable,
connection reset, timeout, HTTP 503, or Zebra's loading-state JSON-RPC error.
Protocol mismatches, invalid block bytes, parse failures, and schema errors are
fatal because retrying would hide a contract violation.

### Commit shape per phase

The bulk-catch-up phase finalizes each committed batch through its tip because it only operates outside the live reorg window. The commit uses the same finality transition the live store understands, for example `FinalizeThrough { height: tip_height }`. It must not encode a finalized-height change as `Unchanged`. The loop clamps every bulk-phase fetch target to `min(upstream_tip - reorg_window, store_tip + canonical_batch_max_blocks)`, then may commit earlier when accumulated canonical artifact bytes or estimated canonical write bytes reach their configured budgets. The explicit `ingest.modifiers.allow_near_tip_finalize` override is intended for local regtest or disposable stores where the operator accepts that future reorgs may require recreating the store.

Tip following performs parent-hash continuity checks before commit. If the
observed tip does not extend the visible tip, ingestion walks back to the
common ancestor, verifies the replacement stays inside the configured reorg
window, and commits through `ReorgWindowChange::Replace`.

`commit_chain_epoch` persists the chain event envelope inside the same storage
batch that advances the visible epoch pointer. The state-machine name above is
therefore descriptive: publication is a property of the commit, not a separate
post-commit write.

## Schema Compatibility

Schema validation lives in `zinder-store` per [Storage backend §Schema Compatibility](storage-backend.md#schema-compatibility). The ingest invariants this document enforces:

- The query service must not silently upgrade canonical storage or open it as its production read path.
- The ingest service must not delete old state silently.
- Schema mismatches surface as typed readiness causes.

## First Invariants

- Canonical storage is append-only for finalized data.
- Non-finalized storage is replaceable only through the reorg state machine.
- Every visible query response that depends on chain state comes from one epoch.
- Every artifact has a schema version.
- Derived indexes are replayable from canonical artifacts.
- Restarting from a crash either resumes or fails with a typed readiness cause.
