# Plan: unify the public data-plane contract

The Zinder public contract expresses single concepts many ways: freshness has
three shapes, error reasons three hand-maintained mappers, capabilities three
parallel registries, and the chain-event resume contract diverges between docs
and code. Each divergence is a shallow seam a consumer must decode per surface.
This plan concentrates each concept behind one interface.

The work lands in three waves by dependency. Wave 1 defines the contracts
everything else rides on and is the scope detailed here. Waves 2 and 3 are
indexed at the end and planned in full once Wave 1 merges.

## Decisions

These hold for the whole redesign. Durable contract decisions (1, 3) graduate
to ADRs during execution; the rest are plan-scoped and end at merge.

1. **One cross-plane `ChainView` envelope.** Every read response across
   `WalletQuery`, `ExplorerQuery`, and `IngestControl` carries a `ChainView` at
   field tag 1. It folds together the three freshness shapes that exist today
   (the bare wallet `ChainEpoch` echo, the `ExplorerFreshness` wrapper, the flat
   `WriterStatus` scalars). Generalizes ADR-0011, updated in place.

2. **`{role}_tip` taxonomy.** The four chain heights get one naming axis so the
   reorg-vs-replay distinction is self-evident:
   - `visible_tip` — best visible block in the epoch (was `tip_height`/`tip_hash`).
   - `settled_tip` — reorg-window ceiling, the wallet scan ceiling (was `safe_tip_*`).
   - `indexed_tip` — derive-replay ceiling (was `IndexedHead`).
   - `upstream_tip` — the upstream node's view (was `UpstreamObservation`).
   `settled_tip` keeps the exact reorg-window semantics of `safe_tip`; only the
   name changes. "Finalized" stays forbidden (collides with NU7/Crosslink).

3. **Self-healing reorg on reconnect.** When a `ChainEvents` cursor names a
   branch reorged out of the canonical chain, the server emits a synthetic
   `ChainReorged` envelope into a real persisted `event_sequence` slot, then
   resumes. A consumer only ever handles `ChainCommitted` and `ChainReorged` and
   never reconciles hashes itself. An unretained divergence base degrades to a
   typed `CHAIN_EVENT_CURSOR_EXPIRED`. New ADR.

4. **Declarative single-source tables, guarded by CI.** Error-reason policy and
   capability advertisement each become one authored Rust table that every
   surface folds over; a CI test asserts each table agrees with the compiled
   `FileDescriptorSet`. No proto custom options, no codegen.

5. **Envelope scope is chain-state only.** `ChainView` carries epoch identity,
   the tips, upstream, and derive status. Response metadata that genuinely varies
   per call (`snapshot_age_millis`, `unavailable[]` field paths,
   `capability_version`) stays on the response, not in `ChainView`.

6. **Shapes (sub-decisions of 1 and 2).**
   - `BlockTip { uint32 height; string hash; }` is reused for `visible_tip`,
     `settled_tip`, and `indexed_tip`.
   - `upstream_tip` stays `{ optional uint32 committed_height; optional uint32
     estimated_height; }` (the upstream probe has no single hash).
   - Commitment-tree sizes stay on `ChainEpoch` as visible-tip scan aids.
   - `ChainEpoch` reshapes to `{ chain_epoch_id, network_name,
     artifact_schema_version, created_at_millis, BlockTip visible_tip, BlockTip
     settled_tip, uint32 sapling_commitment_tree_size, uint32
     orchard_commitment_tree_size }`.
   - The chain-view family (`ChainEpoch`, `BlockTip`, `IndexedTip`,
     `UpstreamTip`, `ChainView`) stays defined in `wallet.proto`, which
     `explorer.proto` and `ingest.proto` already import. Moving it to the
     plane-neutral `ops/` package is the cleaner home but forces a
     `zinder.v1.wallet.ChainEpoch -> zinder.v1.ops.ChainEpoch` namespace
     migration across every reference; deferred to avoid widening Wave 1.

7. **Wave 1 is response-side for `ChainView`.** Requests keep
   `at_epoch: Option<ChainEpoch>` until Wave 2 collapses the pin to a bare
   `at_epoch_id`. Wave 1 changes only what responses carry.

## Guardrails

Load-bearing decisions the redesign must not violate.

- Storage-vs-wire byte-order split (ADR-0024): hash material routes through
  `crates/zinder-core/src/wire/`; the lightwalletd-compat proto stays frozen in
  internal byte order.
- Single-pinned-epoch streaming (commits 23d1f40, af04d4e): streams pin one
  epoch server-side; do not add per-element `at_epoch`, client cursors, or page
  sizes to current-projection streams.
- `settled_tip` keeps `safe_tip` reorg-window semantics; the rename does not
  change the scan-ceiling contract.
- The derive lag signal must survive proto3 zero-vs-absent: an absent
  `indexed_tip` means "unknown", never "at tip".
- `BroadcastRejectionReason` stays a payload verdict separate from `ErrorReason`
  (ADR-0023); cross-reference, do not fold.
- Tip/Safe reorg exposure stays a cursor flag, not separate RPCs; the Safe
  family never emits `ChainReorged`.
- Two RocksDB-secondary reader topology and epoch-bound reads (ADR-0003); local
  reads stay snapshotless.
- No types named `*Service/*Manager/*Handler/*Helper`; no `utils/common/helpers`
  modules. New seams use concept names.

## Stream A — ChainView envelope and the `{role}_tip` taxonomy

Breaking. Response-side. The foundational stream; B, C, and D can run alongside
it but the dependent waves wait on it.

Shape:

```
message BlockTip { uint32 height = 1; string hash = 2; }

message ChainEpoch {
  uint64 chain_epoch_id = 1;
  string network_name = 2;
  uint32 artifact_schema_version = 3;
  uint64 created_at_millis = 4;
  BlockTip visible_tip = 5;
  BlockTip settled_tip = 6;
  uint32 sapling_commitment_tree_size = 7;
  uint32 orchard_commitment_tree_size = 8;
}

message IndexedTip { BlockTip tip = 1; int64 block_time_unix_seconds = 2; }
message UpstreamTip { optional uint32 committed_height = 1; optional uint32 estimated_height = 2; }

message ChainView {
  ChainEpoch chain_epoch = 1;
  optional IndexedTip indexed_tip = 2;   // absent = unknown, never "at tip"
  optional UpstreamTip upstream_tip = 3;
  optional DeriveStatus derive = 4;       // absent = not a derive-backed read
}
```

Work:

- Define the chain-view family in `wallet.proto`; carry `ChainView chain_view = 1`
  on every `WalletQuery`/`ExplorerQuery`/`IngestControl` response message.
- Replace `WriterStatusResponse`'s flat scalars
  (`latest_writer_tip_height`/`latest_writer_safe_tip_height`) with `ChainView`.
- Fold `ExplorerFreshness`'s chain-state fields into `ChainView`; keep
  `snapshot_age_millis`, `unavailable[]`, `capability_version` on the explorer
  response.
- Update native<->wire helpers in `crates/zinder-core/src/wire/` for the reshaped
  `ChainEpoch` and the new tip messages.
- Rewrite response builders: `services/zinder-query/src/grpc/native.rs`,
  `services/zinder-explorer/src/grpc/freshness.rs`, the ingest `WriterStatus`
  builder.
- Sweep every `safe_tip_height`/`safe_tip_hash`/`tip_height`/`indexed_head`
  reference across `crates/` and `services/` to the new names (grep-driven).

Side effects:

- `crates/zinder-client` response types and decode path follow the reshape.
- zally consumes `safe_tip`; the rename is a breaking change for it. Coordinate
  the field rename with the zally repo.
- ADR-0011 updated in place from "explorer freshness envelope" to "cross-plane
  chain-view envelope" with a revision-history entry.
- `docs/architecture/wallet-data-plane.md`, `explorer-plane.md`, and
  `public-interfaces.md` §Vocabulary gain the chain-view family and the
  `{role}_tip` rule.

Tests that survive or change:

- Proto round-trip test for `ChainView` (every plane embeds the subset it fills).
- `services/zinder-query/tests/integration/query_epoch_consistency.rs` adapts to
  the response field move; the mid-read stability assertions stay.
- Explorer freshness tests assert `indexed_tip` absent means unknown.

## Stream B — self-healing reorg on reconnect, with a locator cursor

Non-breaking behavior change. New ADR (0025).

A `ChainEvents` consumer that reconnects after its branch was reorged out must
recover without a full re-derive. The cursor carries a locator instead of a
single position: a bounded set of back-spaced `(height, hash)` pairs, exponentially
spaced from the cursor's tip and capped (the cap bounds the recoverable reorg
depth and the cursor size). The server builds the locator at emit time and the
client treats the cursor as opaque, so the locator stays inside the
HMAC-authenticated `StreamCursorTokenV1` envelope. Recovery resolves the fork
point against the block index, which outlives the pruned event-log window, so it
works even when the divergence point is no longer in retained history.

Resume algorithm (`chain_event_history_start_sequence`):

- Find the fork point: the most recent locator entry whose hash equals the
  canonical block hash at that height.
- Top entry on-chain: no reorg. Resume from `event_sequence + 1`.
- A lower entry is the fork point, and the real reorg events are still retained
  at or after `event_sequence`: resume from `event_sequence + 1`; the real
  `ChainReorged` replays. No synthesis.
- Fork point found but the event log at `event_sequence` is pruned: synthesize a
  `ChainReorged` (reverted from the fork point) into a real persisted
  `event_sequence` slot, then resume forward. The locator-resolved fork point
  bridges the pruned gap.
- No locator entry on the canonical chain (divergence deeper than the cap, or
  fork-point block unresolvable): `CHAIN_EVENT_CURSOR_EXPIRED` with re-derive
  guidance.
- `CHAIN_EVENT_CURSOR_INVALID` is reserved for genuine corruption or forgery
  (bad HMAC, malformed body, ahead-of-history sequence), never for a reorg.

The `Safe` family never receives `ChainReorged`; a `Safe` cursor cannot be
reorged out below `settled_tip` by definition, so a locator miss on a `Safe`
cursor is an expiry, not a synthesized reorg.

Work:

- `crates/zinder-store/src/format/stream_cursor.rs`: replace the single
  `(last_height, last_hash)` in `ChainEventCursorPayload` with a bounded locator;
  keep the `StreamCursorTokenV1` family framing and HMAC. Confirm the envelope
  supports a variable-length body without disturbing the other cursor families
  (mempool, history, address-output).
- `crates/zinder-store/src/chain_store.rs`: build the locator at emit
  (`build_chain_event` / the cursor-construction path) from retained event tips
  and the block index; implement the resume algorithm above; synthesize and
  persist the bridging `ChainReorged` idempotently.
- `services/zinder-query/src/grpc/chain_events.rs` and the ingest stream driver
  tolerate a server-injected envelope ahead of the page scan.

Side effects:

- `crates/zinder-store/tests/integration/chain_event.rs`: the tests asserting
  `ChainEventCursorInvalid` on position mismatch flip. New coverage: within-
  retention reorg reconnect replays the real `ChainReorged`; past-retention reorg
  reconnect gets a synthesized `ChainReorged` resolved via the locator;
  unresolvable fork point yields `CHAIN_EVENT_CURSOR_EXPIRED`.
- New ADR-0025 records the reconnect-reorg contract and the locator cursor (no
  ADR owns it today, which is why docs and code diverged).
- `docs/architecture/chain-events.md`, `wallet-data-plane.md`, and
  `public-interfaces.md` (Cursor Conventions) reconcile to the locator cursor and
  the implemented behavior.
- Heavy probe (per CLAUDE.md): `cargo mutants` over the chain-event resume and
  cursor paths in `chain_store.rs`.

## Stream C — error-reason single source

Non-breaking. Additive proto growth plus one authored mapping.

Work:

- Add reasons in the reserved 34+ slots: `DERIVE_PROJECTION_UNAVAILABLE`,
  `DERIVE_PROJECTION_LAGGING`, `NODE_CAPABILITY_MISSING`,
  `NO_VISIBLE_CHAIN_EPOCH` (`crates/zinder-proto/proto/zinder/v1/ops/error.proto`).
- One authored reason-policy mapping (reason -> gRPC `Code` + retry hint) that
  `services/zinder-query/src/grpc/mod.rs`, `crates/zinder-store/src/grpc_status.rs`,
  the compat shim, and the new explorer error all call. Each boundary enum keeps
  a `fn error_reason(&self) -> ErrorReason` next to its definition.
- Introduce an `ExplorerError` enum so the ~85 raw `Status::*` constructors in
  `services/zinder-explorer/src/grpc/adapter.rs` route through the seam with a
  typed reason.
- Make `ERROR_REASON_UNSPECIFIED` unreachable: every boundary variant maps to a
  real reason.
- Encode the `ARTIFACT_UNAVAILABLE` family on the wire with the existing
  `zinder_core::artifact_family` constants, deleting the client's two 17-arm
  string tables.

Side effects:

- CI drift guard: a test asserting every `ErrorReason` has a code and retry
  policy and every boundary-enum variant maps to a non-`Unspecified` reason.
- `docs/reference/error-vocabulary.md` and `public-interfaces.md` §Error merge to
  one table; the doc stops contradicting the code.

## Stream D — capability single source

Non-breaking.

Work:

- Replace the three advertise idioms and the parallel arrays in
  `crates/zinder-proto/src/capabilities.rs` with one
  `CAPABILITIES: &[CapabilitySpec]` where `CapabilitySpec` carries the capability
  string, its surface, the bound RPC method, and an `advertise(settings,
  readiness)` predicate.
- Every `ServerInfo` builder (`services/zinder-query/src/grpc/native.rs`,
  `services/zinder-explorer/src/grpc/adapter.rs`, the ingest builder) folds over
  the one table.
- Replace `capability_coverage.rs`'s hand-maintained `EXPECTED_METHOD_NAMES` with
  an assertion over the table; fix the stale `transparent_address_tx_ids_v1`
  literal referenced by the new derive error reasons.

Side effects:

- CI drift guard: a test asserting the table and the compiled
  `FileDescriptorSet` agree (every served method has a capability).
- `capability_docs.rs` marker blocks become table-derived.
- The `ALWAYS_ON` vs source-gated distinction (`chain_value_pools`) is expressed
  as a predicate, not a separate array.

## Sequencing

Wave 1 streams A, B, C, D share no dependencies and land in parallel. A defines
the response shape the dependents reshape onto; B, C, D are self-contained.

Wave 2 (rides on Wave 1): collapse the `at_epoch` pin to a bare `at_epoch_id`
(needs `ChainView` as the response-only home for `ChainEpoch`); the generic
event-stream module (needs B settled); the wallet-plane balance collapse; the
transaction-location oneof and wire-naming convergence.

Wave 3 (rides on Wave 2): the `ChainIndex` local/endpoint trait split; the
stream-header plus cursor unification; the compat redaction-plus-uniform-pinning
fix.

## Validation gate

The default gate from CLAUDE.md, plus:

- Proto round-trip tests for `ChainView` and the reshaped `ChainEpoch`.
- The two CI drift guards (error-reason table vs descriptor; capability table vs
  descriptor).
- The reorg-reconnect integration test asserting a synthetic `ChainReorged`.
