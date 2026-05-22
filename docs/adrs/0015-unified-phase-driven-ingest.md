# ADR-0015: Unified Phase-Driven Ingest

## Status

Accepted.

## Context

Operators starting `zinder-ingest tip-follow` against an empty store on
a tip-synced Zebra get a working but unusably slow setup. The process is
alive, `/healthz` returns `200`, the `chain_committed` event ticks
forward at roughly half a block per second on testnet, and `/readyz`
reports `not_ready cause=syncing lag_blocks=4016431`. Nothing
distinguishes "ingest is broken" from "ingest needs bulk catch-up first."
On a multi-million-block backlog the operator-visible result is days or
weeks of progress before anyone notices the wrong subcommand is running.

An earlier iteration of this decision tried to hide the asymmetry in
the deployment layer: a one-shot `zinder-ingest-backfill` Compose
service gated by `service_completed_successfully`, an s6-overlay `run`
script that probes Zebra's tip and execs `backfill` then `tip-follow`,
a `bootstrap-backfill.sh` helper for bare-metal recipes, and a 119-line
runbook cataloguing the scenarios. Three orchestrators, three different
env-var spellings (`BACKFILL_TIP_OFFSET`,
`BOOTSTRAP_REORG_HEADROOM_BLOCKS`, `ZINDER_BOOTSTRAP_DISABLED`), and
two disagreeing default offsets: the s6 image leaves a 200-block gap,
Compose leaves a 100-block gap, and `reorg_window_blocks` defaults to
100. The drift was emerging before the orchestrators shipped.

Three properties of the existing ingest code make the deployment-layer
fix the wrong boundary.

- **The store-state probe is cheap.** `PrimaryChainStore::current_chain_epoch()`
  is cached behind an `Arc<RwLock<Option<ChainEpoch>>>`; the first call
  hits RocksDB, every subsequent call is a read-lock acquisition. The
  upstream tip is one `NodeSource::tip_id()` call that `tip-follow`
  already makes on every loop iteration. Asking "what's my gap to the
  upstream tip?" at startup costs one read and one RPC.
- **The recovery model is already mode-agnostic.** `decide_recovery`
  ([ADR-0013](0013-source-failure-recovery-topology.md)) takes an
  `IngestError` and a backoff state and never branches on which
  subcommand raised the error. `BackfillProducedNoCommit` and
  `TipFollowObservedTipBehindStore` sit in the same `Exit` arm.
  Folding the two loops into one preserves the existing recovery
  contract because the contract was never split.
- **The shared mechanics outweigh the divergence.** Block fetch,
  subtree hydration, artifact assembly, the commit primitive, and the
  commit outcome recorder are shared between `backfill` and
  `tip-follow`. The same `current_chain_height` helper is duplicated
  three times across the writer crate. The actual difference is two
  concerns: fetch shape (32-way pipelined vs serial) and commit shape
  (`FinalizeThrough` vs `Extend`/`Replace` plus a separate tip
  finalization step). Both are runtime choices the binary already has
  the inputs to make.

The industry consensus reinforces the conclusion. Subsquid, Ponder,
electrs, graph-node, lightwalletd, and the Tendermint/Mantlemint stack
all run a single ingest process with internal phase dispatch. None ask
operators to declare which phase the process is in. Ponder is the
closest match in shape: `/health` returns 200 the instant the process
is up, `/ready` returns 503 during historical sync and flips to 200
when realtime ingestion begins.

Zebra itself provides the upstream-side half of the same answer.
[`zebrad`](https://zebra.zfnd.org/user/health.html) ships an opt-in
HTTP health server with two endpoints: `/healthy` for peer-count
liveness, and `/ready` for composite readiness combining peer count,
`SyncStatus::is_close_to_tip()`, lag against the estimated network
tip, and last-tip-grow age. When operators enable it, Zebra answers
the exact question Zinder needs: "is the upstream itself near network
tip?" For Zebras that run without the health endpoint, the JSON-RPC
`getblockchaininfo` response carries `verificationprogress` and
`estimatedheight` as a derived fallback, with the caveat that both
fields come from wall-clock extrapolation of the local tip's timestamp
and not from peer-reported headers. Either way, the signal exists; the
design must consume it so a Zinder that follows a syncing Zebra
reports the truth instead of serving stale data with a `ready` label.

## Decision

`zinder-ingest` is a single long-running command. Invoking the binary
with no subcommand opens the canonical store, probes Zebra's tip,
classifies the gap, runs the matching phase, transitions phase as the
gap changes, and keeps running. The `backfill` and `tip-follow`
subcommands are removed; the `backup` subcommand stays unchanged. A
new `probe` diagnostic prints `{store_tip, upstream_tip, gap_blocks,
phase_that_would_run, upstream_health}` and exits.

The loop is driven by an explicit phase classifier with three phases:

- `IngestPhase::AwaitingUpstream` when the upstream tip is below the
  catch-up floor (regtest near genesis, freshly initialized nodes).
- `IngestPhase::BulkCatchup` when
  `gap_blocks > ingest.phases.catchup_threshold_blocks` (defaults to
  `ingest.reorg_window_blocks`). Runs the pipelined fetch shape and
  commits with `FinalizeThrough { tip_height: target }` against a
  per-batch target of
  `min(upstream_tip - ingest.reorg_window_blocks,
       store_tip + ingest.bulk_catchup.commit_batch_blocks)`.
  The commit stage may close the batch earlier when out-of-batch
  transparent prevout store-lookups reach
  `ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch`.
- `IngestPhase::TipFollow` otherwise. Runs the serial fetch shape and
  commits with `Extend` or `Replace`, then advances the finalized
  boundary through the same `finalize_tip_if_ready` path that exists
  today.

`IngestPhase` serializes with `#[serde(rename_all = "snake_case")]` so
`/readyz`, structured log fields, and `WriterStatus` all expose the
phase as `awaiting_upstream` / `bulk_catchup` / `following_tip`. The
wire shape is part of the public contract; the Rust enum spelling is
an implementation detail.

Phase transitions are bidirectional. Bulk catch-up runs at startup on
cold stores, transitions to tip-follow when the gap closes through the
threshold, and bounces back to bulk catch-up if the gap re-opens
beyond the threshold (long downtime, upstream burst). The mempool
orchestrator, retention worker, and chain-tip notification stream
spawn once, on the first entry into `TipFollow`, and stay running
across subsequent bounces. The `IngestControl` gRPC server starts at
process start and runs throughout.

Phase is exposed orthogonally to `cause` in the readiness surface.
`/readyz` gains a top-level `phase` field; the existing `cause`
taxonomy stays untouched and gains one new value, `upstream_not_ready`,
emitted whenever Zebra reports it isn't ready (see Upstream sync
detection below). `IngestControl.WriterStatusResponse` gains a
`WriterPhase` enum field, `upstream_committed_height`,
`upstream_estimated_height`, `upstream_verification_progress`,
`upstream_health_source`, and `upstream_health_reason` cursors plus a
`gap_blocks` derived value. The capability `ingest.writer.phase_v1`
advertises that the writer surfaces phase and upstream-health
information. An agent that asks "is the writer alive, following tip,
currently node-unavailable?" reads two independent fields rather than
inferring from log-line shape.

Configuration restructures along concern boundaries instead of along the
deprecated subcommand split. The writer-private `[ingest]` section
carries the chain-truth invariant `reorg_window_blocks` at the top and
splits the rest into four concern-named sub-sections:

- `[ingest.phases]` carries the classifier boundary
  (`catchup_threshold_blocks`, defaults to `ingest.reorg_window_blocks`).
- `[ingest.derive]` carries shared CPU-bound derive execution knobs
  (`concurrency`).
- `[ingest.bulk_catchup]` carries the pipelined-fetch and commit-work
  knobs (`commit_batch_blocks`,
  `max_transparent_prevout_store_lookups_per_batch`,
  `fetch_concurrency`).
- `[ingest.tip_follow]` carries the serial-loop knobs
  (`poll_interval_ms`, `lag_threshold_blocks`).
- `[ingest.modifiers]` carries the optional one-shot or
  disposable-store knobs (`target_height` renamed from `to_height`,
  `checkpoint_height`, `allow_near_tip_finalize`, `coverage`).

The upstream-health knobs do not live on `[ingest]`. The signal is a
property of the upstream chain-source binding, not of the writer's
phase classifier, so it lands under a new `[node.health]` sub-section
on the shared `[node]` section ([ADR-0014](0014-shared-configuration-sections.md)):
`[node.health].addr` (Zebra's `/ready` URL),
`[node.health].poll_interval_ms` (default 30000),
`[node.health].verification_progress_floor` (default 0.999), and
`[node.health].estimated_gap_floor_blocks` (default 10). A future
reader binary that wants to surface "is the upstream itself ready?"
reads the same section without duplication.

The deleted shapes: `[backfill]`, `[tip_follow]`, the
`ZINDER_BACKFILL__*` and `ZINDER_TIP_FOLLOW__*` env namespaces, and
the `BACKFILL_FETCH_CONCURRENCY` compile-time constant (promoted to
`ingest.bulk_catchup.fetch_concurrency`). The shared
`[ingest_control]` section keeps its current shape because the
control endpoint always runs.

The store lock is opened once at process start. RocksDB's primary
`LOCK` file is the only writer-coordination primitive needed; the
Compose `service_completed_successfully` gate and the s6 in-process
sequencing both disappear with the orchestrators.

### Upstream sync detection

Zinder treats Zebra's HTTP `/ready` endpoint as the primary
upstream-sync signal. When the writer config sets `[node.health].addr`,
a background task polls the endpoint every
`node.health.poll_interval_ms` milliseconds (default `30000`). The
response is interpreted as a state machine:

- `200 OK` with body `ok`: upstream-ready.
- `503 Service Unavailable` with body in the sentinel set
  (`insufficient peers`, `syncing`, `no tip`, `tip_age=<N>s`,
  `lag=<N> blocks`): upstream-not-ready, with the body surfaced as
  the reason.
- Connection error: fall back to the JSON-RPC path for that interval
  (the health listener can be down while the JSON-RPC server stays
  reachable; treating that as upstream-not-ready would be a false
  positive).

When `[node.health].addr` is unset or all probes fail, Zinder uses
`getblockchaininfo.verificationprogress` and `estimatedheight` as the
fallback signal: upstream-not-ready when
`verificationprogress < node.health.verification_progress_floor`
(default `0.999`) or
`estimated_height - blocks > node.health.estimated_gap_floor_blocks`
(default `10`). The fallback is less authoritative because both
fields come from wall-clock extrapolation rather than peer-reported
headers; operators running Zebras with the health endpoint enabled
should configure `[node.health].addr` to use the precise signal.

When upstream-not-ready, Zinder still commits whatever blocks Zebra
has made available; it only gates the operator-visible readiness so
queries do not serve stale data with a `ready` label. The cause field
on `/readyz` becomes `upstream_not_ready` with structured details:
`upstream_committed_height`, `upstream_estimated_height`,
`upstream_verification_progress`, `upstream_health.source` (one of
`zebra_ready_endpoint` or `verification_progress_fallback`), and
`upstream_health.reason` (the sentinel string from `/ready` or the
fallback predicate name).

### Prerequisites this design satisfies

A unified subcommand needs four properties for the change to land
without breaking the surrounding contracts. Each is satisfied by the
design above.

- **A `/readyz` cause taxonomy that covers both phases.** Solved by
  making `phase` orthogonal to `cause`. No cause strings rename. The
  existing `syncing`, `node_unavailable`, `schema_mismatch`,
  `reorg_window_exceeded`, and `replica_lagging` causes keep their
  semantics. Phase is a second dimension that agents and operators
  read independently; `upstream_not_ready` joins as a new value in
  the existing taxonomy.
- **A `BackfillOutcome` handoff that holds the store lock across the
  transition.** Solved by removing the handoff. One process, one
  store open, one loop. `BackfillOutcome::AlreadyComplete` was only
  consumed by Compose's exit-code gate; with the gate gone, the type
  is gone.
- **A failure-recovery story compatible with ADR-0013.** Already
  compatible. `decide_recovery` does not branch on mode. Every
  existing `IngestError` keeps its existing recovery class. The
  unified loop calls `decide_recovery` from the same site the
  per-subcommand loops call it today. Upstream-health failures (the
  new `/ready` probe) are scoped to readiness reporting and never
  enter `decide_recovery`; they cannot mask a genuine source failure.
- **A migration path for the existing orchestrators.** The migration
  is deletion. The orchestrators were never published outside the
  writer workspace; the in-flight branch that introduced them is
  replaced by this design.

## Consequences

**For operators.** Every supported deployment becomes one line:
`zinder-ingest --config /etc/zinder/ingest.toml`. The Compose file has
one writer service. The s6 longrun script execs the binary directly.
The [Initial sync runbook](../runbooks/initial-sync.md) collapses from
a six-row scenario table to a one-paragraph note describing the
auto-classifying loop. Operators outside the shipped deployment shapes
do nothing special; the binary handles every gap class on its own.
Operators who configure `[node.health].addr` against Zebra's `/ready`
get the precise upstream-sync signal; those who don't get the JSON-RPC
fallback with no extra setup.

**For developers.** New ingestion features land in one place. The
triplicated `current_chain_height` helper collapses to one private
helper beside the phase classifier. `BackfillConfig` becomes
`BulkCatchupConfig` and shrinks: `from_height` and `to_height` are
both derived per iteration instead of carried as configuration. The
`BACKFILL_FETCH_CONCURRENCY` constant becomes the
`ingest.bulk_catchup.fetch_concurrency` config field, parameterized
rather than hard-coded. The bulk-catchup transparent-prevout store-read
budget is also a named config field instead of being hidden inside block
count. The CLI surface tracks one front door, not two.
The duplicated `IngestNodeConfig` schema mirror in the ingest binary
disappears in favor of consuming `NodeSection` directly through
ADR-0014's `with_node_section` helper, so adding a field to the shared
`[node]` schema does not require editing two structs in two crates.
The `NodeSource::fetch_block_by_height` method renames to
`fetch_block_at` so the source boundary matches the canonical
`{artifact}_at(height)` shape from
[Public interfaces §Method Naming Conventions](../architecture/public-interfaces.md#method-naming-conventions).

**For agents.** `phase` and `cause` are independent dimensions on
`/readyz` and `WriterStatus`. The today-only inference from log-line
shape (single-block commit vs thousand-block batch) becomes a typed
contract that the capability `ingest.writer.phase_v1` advertises in
the federation handshake (added to
`crates/zinder-client/tests/integration/capability_coverage.rs` so the
test fails if a future change drops the advertisement). The wire
shape is explicit: `phase` serializes as snake-case
(`awaiting_upstream`, `bulk_catchup`, `following_tip`) on both JSON
and proto sides, so an agent that parses one surface and writes
another does not need a casing translation table. The
`upstream_health` substructure on the `upstream_not_ready` cause lets
agents distinguish "upstream is syncing" from "upstream lost peers"
from "upstream's tip is stale" without parsing log lines; the
sentinel-string set is enumerated under §Upstream sync detection so
agents can build a closed dispatch table.

**For doc authors.** [ADR-0003](0003-canonical-storage-access-boundary.md)
and [ADR-0013](0013-source-failure-recovery-topology.md) are
unchanged. [ADR-0014](0014-shared-configuration-sections.md) gains
`[node.health]` as a new shared sub-section on the existing `[node]`
schema (the upstream-health knobs are operator-readable from any
binary that wants them).
[Chain ingestion](../architecture/chain-ingestion.md) renames its
§Backfill and Tip Following section to §Bulk catch-up and tip
following, replacing subcommand vocabulary with phase vocabulary,
adds a §Phase transitions subsection covering the classifier rule and
spawn-once semantics, and adds a §Upstream sync detection subsection
covering the dual-path probe.
[Node source boundary](../architecture/node-source-boundary.md)
renames the trait method from `fetch_block_by_height` to
`fetch_block_at` and updates the §Source Trait listing.
[Public interfaces](../architecture/public-interfaces.md) deletes
`TipFollowConfig` from the vocabulary spine, rewrites
§Method Naming Conventions Rule 6 (dropping the `_by_height` carve-out),
refreshes the canonical TOML for the sub-sectioned `[ingest.*]`
schema, and adds the new `[node.health]` field rows to the env-var
table. The [Initial sync runbook](../runbooks/initial-sync.md) shrinks
to the one-paragraph "just run the binary" runbook plus a
forked-store recovery note and an upstream-sync diagnostic note.

**For the Z3 contract.** Z3 should expose Zebra's `/ready` endpoint
on a known port so Zinder's `[node.health].addr` config has a stable
target. This is a cross-repo coordination ask, not a blocker for
this ADR: Zinder's fallback path handles Z3 versions that do not yet
advertise it.

**Removed surface.** `zinder-ingest backfill`,
`zinder-ingest tip-follow`, `BackfillOutcome`, `BackfillArgs`,
`TipFollowArgs`, `TipFollowConfig` (deleted from the vocabulary
spine), the `BACKFILL_FETCH_CONCURRENCY` constant, the `[backfill]`
and `[tip_follow]` TOML sections, the `ZINDER_BACKFILL__*` and
`ZINDER_TIP_FOLLOW__*` env namespaces, the `IngestNodeConfig` schema
mirror (replaced by direct `NodeSection` consumption per ADR-0014),
the `NodeSource::fetch_block_by_height` method (renamed to
`fetch_block_at`), the `zinder-ingest-backfill` Compose service,
`deploy/bootstrap-backfill.sh`, the s6 bootstrap branch, the
`scripts/observability-smoke.sh` backfill-subcommand invocation
(rewritten to drive the unified loop), and the env names
`BACKFILL_TIP_OFFSET`, `BOOTSTRAP_REORG_HEADROOM_BLOCKS`, and
`ZINDER_BOOTSTRAP_DISABLED`.

**Added surface.** `IngestPhase` (Rust enum, serialized snake-case),
`WriterPhase` (proto), the `phase` field on `/readyz` JSON, the
`WriterStatusResponse` fields `phase`, `upstream_committed_height`,
`upstream_estimated_height`, `upstream_verification_progress`,
`upstream_health_source`, `upstream_health_reason`, and `gap_blocks`
(coordinated with the `active_transport` field added by
[ADR-0016](0016-source-streaming-pipeline.md) so a single proto bump
covers the unified-ingest wire surface), the `ingest.writer.phase_v1`
capability (registered in `capability_coverage.rs`), the new
`cause=upstream_not_ready` readiness value, the
`NodeCapability::ReadinessProbe` advertisement (already declared but
previously unused), the `zinder-ingest probe` diagnostic subcommand,
the sub-sectioned writer-private `[ingest.phases]`,
`[ingest.derive]`, `[ingest.bulk_catchup]`, `[ingest.tip_follow]`, `[ingest.modifiers]`
TOML sections with their `catchup_threshold_blocks`,
`concurrency`, `commit_batch_blocks`,
`max_transparent_prevout_store_lookups_per_batch`, `fetch_concurrency`, `poll_interval_ms`,
`lag_threshold_blocks`, `target_height`, `checkpoint_height`,
`allow_near_tip_finalize`, and `coverage` fields, and the new
`[node.health]` sub-section on the shared `[node]` schema with
`addr`, `poll_interval_ms`, `verification_progress_floor`, and
`estimated_gap_floor_blocks` fields. `target_height` replaces
`to_height` as a per-process stop modifier on the unified loop.

## Implementation Plan

Six review-friendly phases on the same feature branch. Each phase
ends green on the full Default Validation Gate before the next opens.
Phase 0 is internal cleanup; phases 1–2 land the new wire surface and
the upstream-health probe; phase 3 is the unified loop and the
breaking CLI + config flip in one merge; phase 4 is deployment
cleanup; phase 5 is verification only.

Single-operator scope drives the merge sequencing. The original plan
landed the unified loop behind a hidden `--unified` flag in a separate
phase so the old subcommands could run side-by-side for comparison
before deletion. That side-by-side validation is the hedge external
users need; Zinder is ZFND-internal with no external consumers, so the
loop ships and the subcommands go in one PR. The testkit mock covers
phase transitions and the cold-start/long-downtime scenarios; phase 5
catches anything the mock misses against z3.

### Phase 0: Internal refactor

Pure cleanup that simplifies the surface before the new code lands.
No public API change, no operator-visible behavior change. Safe to
ship at any time, independent of phases 1–5.

- Delete the `IngestNodeConfig` schema mirror in
  `services/zinder-ingest/src/config.rs`; consume the shared
  `NodeSection` directly through ADR-0014's `with_node_section`
  helper. Adding a field to `[node]` then requires editing one struct,
  not two.
- Collapse the three definitions of `current_chain_height` (at
  `services/zinder-ingest/src/backfill.rs:242`,
  `services/zinder-ingest/src/tip_follow.rs:347`, and
  `services/zinder-ingest/src/main.rs:990`) into one private helper.
  Place it where the phase classifier will land in phase 3 so the
  later merge is a no-op for that helper.
- Tests: existing test coverage unchanged. The Default Validation
  Gate is the bar.

Dependencies: none. Breaking: no. Review surface: small.

### Phase 1: Source-boundary rename + proto + readiness vocabulary

The first wire-level commitment. Lands the rename and the new proto
surface together with [ADR-0016](0016-source-streaming-pipeline.md)'s
Phase 1 so the unified-ingest wire surface bumps once, not twice.

- Rename `NodeSource::fetch_block_by_height` to `fetch_block_at`
  across the trait (`crates/zinder-source/src/node_source.rs`), the
  Zebra adapter (`crates/zinder-source/src/zebra_json_rpc.rs`), the
  mock source (`crates/zinder-testkit/src/mock_node_source.rs`), and
  every call site. Updates the spine's
  [§Method Naming Conventions Rule 6](../architecture/public-interfaces.md#rule-6--verb-forms-inside-zinder-source)
  to drop the historical carve-out for `_by_height`.
- Add `WriterPhase` enum and the new `WriterStatusResponse` fields
  (`phase`, `upstream_committed_height`, `upstream_estimated_height`,
  `upstream_verification_progress`, `upstream_health_source`,
  `upstream_health_reason`, `gap_blocks`) in
  `crates/zinder-proto/proto/zinder/v1/ingest/ingest.proto`. Coordinate
  with [ADR-0016](0016-source-streaming-pipeline.md)'s
  `active_transport` field so one proto bump covers both.
- Add `IngestPhase` Rust enum with
  `#[serde(rename_all = "snake_case")]` and the `phase` JSON field in
  `crates/zinder-runtime/src/readiness.rs` and its companion
  `ops_endpoint.rs`. Add the `upstream_not_ready` cause variant with
  its structured-detail shape.
- Add capability `ingest.writer.phase_v1` to the always-on capability
  list in `crates/zinder-proto/src/capabilities.rs`, and register it
  in `crates/zinder-client/tests/integration/capability_coverage.rs`
  so a future change cannot drop the advertisement silently.
- Tests: proto round-trip for every `WriterPhase`; readiness JSON
  snapshot covering each `phase × cause` combination the design
  expects to emit, including the `upstream_not_ready` shape with its
  full sub-structure.

Dependencies: phase 0. Breaking: yes (trait rename + proto bump).
Review surface: medium.

### Phase 2: Source-boundary upstream health probe

Lands the `/ready` consumer and the `verificationprogress` fallback so
phase 3 has the signal it needs.

- Add the new `[node.health]` sub-section to `NodeSection` in
  `crates/zinder-source/src/node_target.rs`. Fields: `addr` (URL),
  `poll_interval_ms` (default 30000),
  `verification_progress_floor` (default 0.999),
  `estimated_gap_floor_blocks` (default 10). Validation: non-zero
  poll interval, `verification_progress_floor` in `(0.0, 1.0)`.
- Add `NodeHealthSource` (or extend `NodeSource`) with
  `poll_upstream_health()` returning a structured
  `UpstreamHealthSnapshot { ready_for_queries, source, reason,
  upstream_estimated_height, upstream_verification_progress }`.
- Implement on `ZebraJsonRpcSource`: when `[node.health].addr` is set,
  hit Zebra's `/ready` over HTTP; otherwise read
  `getblockchaininfo.verificationprogress` and `estimatedheight`. Map
  the five `/ready` sentinel strings (`ok`, `insufficient peers`,
  `syncing`, `no tip`, `tip_age=<N>s`, `lag=<N> blocks`) to the
  cause-detail enum.
- Advertise `NodeCapability::ReadinessProbe` (already declared in
  `crates/zinder-source/src/node_capabilities.rs` but never granted)
  when `[node.health].addr` resolves successfully at startup. Update
  `parse_openrpc_capabilities()` to grant it in the `auto`/probed
  path.
- Wire the probe into a background task that updates readiness by
  calling `Readiness::set(...)` on the shared
  `Readiness` (`crates/zinder-runtime/src/readiness.rs:295`) directly.
  There is no channel; the existing `parking_lot::Mutex` is the
  contract. The task is independent of the ingest loop and only
  writes the `upstream_not_ready` cause; probe failure never cancels
  the loop.
- Tests: mocked HTTP `/ready` responses for every sentinel; fallback
  path with mocked `getblockchaininfo` payloads; transition cases
  (health endpoint disappears mid-run, falls back to JSON-RPC);
  regtest opt-in coverage (`enforce_on_test_networks`).

Dependencies: phase 1. Breaking: no (additive). Review surface: medium.

### Phase 3: Unified loop + CLI flip + config restructure

Lands the unified ingest loop, deletes the two subcommands, and
restructures the config schema in one merge. Single-operator scope
makes the hedge of a hidden-flag rollout unnecessary; the testkit
mock covers phase transitions and the cold-start/long-downtime
scenarios end-to-end before merge, and phase 5 validates against z3
afterwards.

Loop module:

- New module `services/zinder-ingest/src/ingest_loop.rs` with
  `classify_phase`, the three phase handlers (`AwaitingUpstream`,
  `BulkCatchup`, `FollowingTip`), and the spawn-once gates for the
  mempool orchestrator, retention worker, and chain-tip notification
  stream. The handlers reuse the existing fetch + commit primitives
  from `backfill.rs` and `tip_follow.rs`; only the dispatch around
  them is new.
- Plumb the upstream-health probe from phase 2: the loop reads the
  shared `Readiness` to observe the upstream-health cause and stamps
  the locally-classified phase on every transition via
  `ReadinessState::with_phase(...)`. No new IPC primitive; the
  existing `parking_lot::Mutex` is the contract.

CLI:

- Make `Command` optional on the top-level `Cli` struct so
  `zinder-ingest --config X` invokes the unified loop directly.
- Delete `Command::Backfill`, `Command::TipFollow`, `BackfillArgs`,
  `TipFollowArgs`, `BackfillOutcome`, `run_backfill`, and the outer
  `run_tip_follow` (the inner mechanics survive inside the loop).
- Promote per-subcommand CLI flags (`--from-height`, `--to-height`
  renamed to `--target-height`, `--checkpoint-height`,
  `--wallet-serving`, `--allow-near-tip-finalize`,
  `--commit-batch-blocks`, `--reorg-window-blocks`,
  `--poll-interval-ms`, `--lag-threshold-blocks`,
  `--ingest-control-listen-addr`, `--ingest-control-token-path`) onto
  the top-level `Cli` struct as global flags.
- Add the `Command::Probe` diagnostic subcommand
  (`{store_tip, upstream_tip, gap_blocks, phase_that_would_run,
  upstream_health}` and exit).

Config:

- Restructure `[backfill]` and `[tip_follow]` into sub-sections of
  `[ingest]`: `[ingest.phases]` (classifier),
  `[ingest.derive]` (shared CPU-bound derive execution),
  `[ingest.bulk_catchup]` (pipelined-fetch knobs), `[ingest.tip_follow]`
  (serial-loop knobs), `[ingest.modifiers]` (one-shot CLI modifiers).
  Keep `reorg_window_blocks` at the top of `[ingest]` as a chain-truth
  invariant.
- Rename `to_height` to `target_height` and place it under
  `[ingest.modifiers]`.
- Promote the `BACKFILL_FETCH_CONCURRENCY` constant to
  `ingest.bulk_catchup.fetch_concurrency` (default 32).
- Add `ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch`
  so block count does not hide commit-time transparent-prevout store-read
  cost.
- Migrate `deploy/single-container/config.example.ingest.toml` and
  `deploy/config/ingest.toml` to the new shape.

Scripts + docs:

- Update `scripts/observability-smoke.sh`: drop the `backfill`
  subcommand invocation (the script currently runs
  `zinder-ingest ... backfill` then starts a separate
  `zinder-ingest ... tip-follow`), emit the new sub-sectioned
  `[ingest.*]` config, and drive the unified loop in one invocation.
- Delete `TipFollowConfig` from
  [Public interfaces §Configuration vocabulary](../architecture/public-interfaces.md#configuration).
  Refresh the env-var table in the same doc with the new
  `ZINDER_INGEST__PHASES__*`, `ZINDER_INGEST__BULK_CATCHUP__*`,
  `ZINDER_INGEST__TIP_FOLLOW__*`, `ZINDER_INGEST__MODIFIERS__*`, and
  `ZINDER_NODE__HEALTH__*` env vars. Mirror the changes into
  `crates/zinder-runtime/src/env_var_docs.rs::ENVIRONMENT_VARIABLES`
  so the doc-mirror integration test stays green.

Tests:

- Integration tests against the testkit mock: cold start with
  multi-million-block gap (`BulkCatchup → FollowingTip`); long-
  downtime restart (`FollowingTip → BulkCatchup → FollowingTip`);
  `AwaitingUpstream` parking before the upstream tip rises above the
  catch-up floor; `upstream_not_ready` gating with the loop still
  committing blocks; phase transitions logged with structured fields.

Dependencies: phases 1 and 2. Breaking: yes (subcommand removal +
config restructure). Review surface: large.

### Phase 4: Deployment cleanup and doc rewrite

Deletes the orchestrators that the unified loop made redundant and
edits the runbooks whose prose now matches the shipped code.

- Delete `deploy/bootstrap-backfill.sh` and its compose volume mount.
- Delete the `zinder-ingest-backfill` service from
  `deploy/docker-compose.yml`; drop the
  `service_completed_successfully` gate on `zinder-ingest`.
- Delete the bootstrap branch in
  `deploy/single-container/services/zinder-ingest/run`; collapse the
  script to chown plus exec.
- Confirm `docs/runbooks/initial-sync.md`,
  `docs/runbooks/deploying-on-a-vm.md`, and
  `docs/runbooks/deploying-on-railway.md` read correctly against the
  shipped binary. The runbooks were already pre-aligned with the
  unified-loop shape; this phase is the verification pass, not a
  rewrite.
- Touch `CLAUDE.md` and `README.md` to update the operator-facing
  command shape.

Dependencies: phase 3. Breaking: yes (file deletions). Review surface:
medium.

### Phase 5: Live verification

No code changes. Validation of the merged stack against the supported
networks.

- Run the Default Validation Gate end-to-end on a clean clone.
- Live regtest sweep against the Z3 sidecar covering cold-start,
  restart, and long-downtime scenarios.
- Live testnet sweep (cold-start from scratch; multi-hour run to
  confirm the BulkCatchup → FollowingTip transition under real
  network conditions).
- Live mainnet sweep behind opt-in (`require_live_mainnet`). Validate
  the upstream-health probe against an operator-hosted Zebra with
  `/ready` enabled and a Zebra without it, to cover both paths.
- Confirm `/readyz` JSON shape against a current consumer (the Z3
  Zinder dashboard or operator probes); surface any field-shape drift
  before any external announcement.

Dependencies: phase 4. Breaking: no. Review surface: small (test
infra and CI knobs).

### Cross-cutting dependencies

- Phases 1–2 can be developed in parallel but must merge in order so
  the wire surface lands before the probe that fills it.
- Phase 1's source-boundary trait rename is the first place an
  external integrator's `NodeSource` implementation would break. The
  Zinder workspace has only `ZebraJsonRpcSource` and the testkit
  mock; the change is internal.
- Phase 1 coordinates with [ADR-0016](0016-source-streaming-pipeline.md)'s
  Phase 1 so `WriterStatus.active_transport` and
  `WriterStatus.phase` land in one proto bump.
- Phase 4 unblocks any cross-repo announcement to Z3 operators. The
  Z3 contract addition (publishing Zebra's `/ready` port) can land in
  parallel with phase 2 and is fully decoupled from Zinder's release.

## References

- [ADR-0003: Canonical Storage Access Boundary](0003-canonical-storage-access-boundary.md)
- [ADR-0013: Source Failure Recovery Topology](0013-source-failure-recovery-topology.md)
- [ADR-0014: Shared Configuration Sections](0014-shared-configuration-sections.md): the `[node.health]` sub-section addition follows the same shared-section pattern.
- [ADR-0016: Source Streaming Pipeline](0016-source-streaming-pipeline.md): coordinated proto bump for `WriterStatus.active_transport`.
- [Chain ingestion](../architecture/chain-ingestion.md)
- [Node source boundary](../architecture/node-source-boundary.md)
- [Initial sync runbook](../runbooks/initial-sync.md)
- [`services/zinder-ingest/src/main.rs`](../../services/zinder-ingest/src/main.rs): single CLI front door
- [`crates/zinder-proto/proto/zinder/v1/ingest/ingest.proto`](../../crates/zinder-proto/proto/zinder/v1/ingest/ingest.proto): `WriterPhase`, `WriterStatusResponse`
- [`crates/zinder-runtime/src/readiness.rs`](../../crates/zinder-runtime/src/readiness.rs): `IngestPhase`, `/readyz` JSON shape
- [`crates/zinder-source/src/node_capabilities.rs`](../../crates/zinder-source/src/node_capabilities.rs): `NodeCapability::ReadinessProbe`
- Zebra health endpoints: [user guide](https://zebra.zfnd.org/user/health.html), [`zebrad/src/components/health.rs`](https://github.com/ZcashFoundation/zebra/blob/main/zebrad/src/components/health.rs)
