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

- `BlockArtifact`: canonical block metadata, transaction references, and block links.
- `CompactBlockArtifact`: wallet-oriented compact block representation.
- `TreeStateArtifact`: tree state data required by wallet sync APIs.
- `TransactionArtifact`: transaction lookup material needed by APIs.
- `MempoolIndex` / `MempoolEventLog`: non-canonical mempool view and event stream, implemented outside `commit_ingest_batch`. The live index is in-memory; the event log persists through the `mempool_event` column family per [ADR-0010](../adrs/0010-mempool-topology-and-retention.md). Both are owned by `zinder-ingest` alongside `tip-follow`. The mempool orchestrator (`run_mempool_orchestrator`) is a sibling of `tip-follow` in the writer process: it consumes a `MempoolSource` stream, hydrates each observation through `build_mempool_entry`, and writes typed `Added`/`Invalidated`/`Mined` envelopes to `MempoolEventLog`. A separate retention worker (`spawn_mempool_event_retention_task`) prunes per-variant windows and emits `MempoolCursorAtRisk` readiness when the oldest retained sequence approaches the configured floor; this is the mempool-side equivalent of [`spawn_chain_event_retention_task`](chain-events.md#retention-and-backpressure).

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
and stateful tree-size metadata for contiguous backfills. Subtree roots and
latest tree state remain separate artifacts, not fields to reconstruct at query
time.

Commitment-tree sizes must be chain-global. A fresh backfill may start at
height 1, an existing store may append immediately after its current tip, and a
checkpoint-bounded backfill may start at `SourceChainCheckpoint.height + 1` after
seeding the builder from the checkpoint's `ChainTipMetadata`. Arbitrary
non-genesis or non-contiguous backfills still fail closed unless they are backed
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

## Backfill and Tip Following

Backfill and tip following should share the same artifact builders and commit path.

The source adapter may differ:

- Historical backfill can use polling or batch reads.
- Tip following can use polling, source notifications, or future streaming sources.

The processing model must stay the same. This follows the same principle as modern checkpoint-driven indexers: source transport can change, but checkpoint or block processing remains deterministic.

Source capability detection happens before processing starts. If the selected source cannot provide required data such as finalized height, chainwork, non-finalized blocks, or transaction broadcast support for the configured mode, ingestion fails closed with a typed startup or readiness cause.

### Backfill throughput shape

Historical backfill is bounded by JSON-RPC round-trip latency, not by upstream-node CPU. Two structural choices keep the throughput high enough for a multi-million-block sync to complete in single-digit hours rather than weeks:

1. **Pipelined block fetches.** `backfill_from_source_with_store` keeps up to `BACKFILL_FETCH_CONCURRENCY` (32) `fetch_block_by_height` calls in flight via `futures_util::stream::iter(...).buffered(N)`. The stream preserves submission order, so artifact assembly and RocksDB writes stay strictly ordered; only the network-bound fetch path is concurrent.
2. **Concurrent per-block RPCs.** `ZebraJsonRpcSource::fetch_block_by_height` resolves the anchor hash with one `getblockhash` call, then drives `getblockheader`, `getblock`, and `z_gettreestate` concurrently via `tokio::join!`. Per-block round-trip count drops from four sequential RTTs to one plus a parallel triple.

[ADR-0023](../adrs/0023-pipelined-backfill-and-concurrent-block-fetch.md) records the rationale and the operational tuning advice. Tip-follow stays serial: it commits one block per poll because by definition it is following the tip, where pipelining offers no headroom.

### Tip-follow wakeups

Tip-follow's default wake-up signal is a polling interval, but when the operator sets `ZINDER_NODE__INDEXER_GRPC_ADDR=http://<zebra>:8155` the loop also subscribes to Zebra's `Indexer.ChainTipChange` gRPC stream. Each push notification wakes the loop and triggers an immediate `tip_follow_once` against the JSON-RPC source for block bytes and tree state. The polling interval stays in the `tokio::select!` as a safety net: a transient stream failure or a missed reconnect cannot stall ingest beyond `poll_interval_ms`. Block fetching does not move to gRPC because `z_gettreestate` is JSON-RPC-only. [ADR-0024](../adrs/0024-grpc-chain-tip-notifications.md) records the rationale.

Historical backfill also fetches newly completed shielded subtree roots through
the source boundary. The source adapter returns `z_getsubtreesbyindex`
data without a completing block hash, so `zinder-ingest` binds each returned
root to the block artifact that completed it before committing
`SubtreeRootArtifact` values. Query and compatibility code must not repair
missing subtree roots by calling the upstream node.

Checkpoint bootstrap must initialize the running shielded tree-size observer
from the upstream-node-supplied checkpoint tree state before validating the first
post-checkpoint block. Assuming zero after Sapling or Orchard activation makes
deep wallet-serving backfill fail even when the checkpoint metadata is correct.
The observed failure and reproduction live in
[Android wallet integration findings](../reference/android-wallet-integration-findings.md#backfill-from-a-non-zero-shielded-tree-size-checkpoint-must-seed-the-builder);
this page owns the durable ingestion requirement.

Wallet-serving backfill is an explicit coverage mode, not an operator folklore
recipe. `zinder-ingest backfill --wallet-serving` derives the historical floor
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
[ADR-0015](../adrs/0015-network-parameter-discovery.md).

The derived floor does not relax the finality bound on the backfill end height.
Serving-store backfills should stop at the latest height outside the configured
reorg window, then let `tip-follow` ingest the replaceable near-tip suffix.
Per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md),
`--allow-near-tip-finalize` is invalid with `--wallet-serving`; use it only
with explicit local or disposable stores.

Backfill retries retryable source failures with exponential backoff,
a per-block source deadline, and a per-run retryable failure budget. Retryable
failures are transport/readiness shaped, such as source unavailable,
connection reset, timeout, HTTP 503, or Zebra's loading-state JSON-RPC error.
Protocol mismatches, invalid block bytes, parse failures, and schema errors are
fatal because retrying would hide a contract violation.

Historical backfill may finalize each committed batch through its tip only when the configured range is known to be outside the live reorg window. That commit must use the same finality transition the live store understands, for example `FinalizeThrough { height: tip_height }`. It must not encode a finalized-height change as `Unchanged`.

`zinder-ingest backfill` therefore checks the upstream node tip before opening storage and rejects ranges whose `to_height` is inside the configured reorg window. The only bypass is the explicit `backfill.allow_near_tip_finalize` / `--allow-near-tip-finalize` override, intended for local regtest or disposable stores where the operator accepts that future reorgs may require recreating the store.

Near-tip ingestion needs a separate finalized-boundary policy. Reusing historical backfill for near-tip ranges without that policy would mark replaceable blocks as finalized and hide the actual reorg risk from readers and event consumers.

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
