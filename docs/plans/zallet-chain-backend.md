# Plan: zallet chain backend

Zallet reads all chain data through the backend-neutral `Chain` and `ChainView`
traits (`zallet/src/components/chain.rs`), proven by two interchangeable
implementations selected by cargo feature (`zebra-state`, `zaino`). The traits
are `pub(crate)`, so a Zinder backend lives inside the Zallet source tree. This
plan makes Zinder the third backend through a wire-only seam: Zallet vendors
the `zinder.v1` protos and regenerates stubs with its own toolchain, and no
Zinder Rust crate crosses the boundary.

Zinder `main` already serves 14 of 18 trait signatures. The remaining work is
two contract gaps (event tail-start, durable spend resolution), one internal
rework (bulk block streaming), a deployment posture, and the backend itself.
Zinder phases land first and freeze the consumed surfaces; the backend follows;
mainnet pays its storage epoch exactly once, after testnet validates everything.

## Decisions

Durable contract decisions (2, 3, 4) graduate to ADRs during execution; the
rest are plan-scoped and end at merge.

1. **Wire-only seam.** Zallet vendors five proto files (`wallet/wallet.proto`,
   `ingest/ingest.proto`, `ops/server_info.proto`, `ops/readiness.proto`,
   `ops/error.proto`) by commit pin with a CI drift job, mirroring the
   vendored-proto pattern both repos already run. Linking `zinder-client` is
   impossible and stays out of scope: `rust-librocksdb-sys 0.43`
   (`links="rocksdb"`) collides with Zallet's `librocksdb-sys 0.16` via
   `zebra-state`, and the workspace `rust-version = 1.95` exceeds Zallet's
   1.85.1 toolchain. A transport-only client crate split waits for a second
   external Rust consumer.

2. **Explicit event start position (breaking, revised in place).**
   `ChainEventsRequest` and `MempoolEventsRequest` replace the implicit
   empty-`from_cursor`-means-earliest convention with one required shared
   start oneof: `after_cursor | earliest_retained | live_tail`. `live_tail`
   resolves once at subscribe into a server-minted head cursor, reusing the
   existing stream driver and the ADR-0025 resume machinery. Start position
   resolves within the requested stream family; a cursor's encoded family is
   authoritative and a mismatch is `INVALID_ARGUMENT`.
   `MempoolSnapshotResponse` gains an opaque `events_resume_cursor` minted at
   the first page and threaded unchanged through the paging chain; the
   delivery contract is at-least-once with idempotent application.
   `snapshot_sequence` is deleted. `ServerInfo` carries a contract revision
   marker so a vendored consumer detects version skew loudly. All event and
   snapshot capability strings keep their `_v1` names. New ADR.

3. **Spend durability as a derive projection, zero new wire surface.** A new
   bundled derive consumer `transparent_outpoint_spend` (row: spending txid,
   block hash, height, input index, keyed by outpoint, with a per-height
   rewind index) is union-routed inside the existing
   `TransparentSpendsByOutpoint` handler: canonical epoch-pinned read first,
   derive lookups accepted only at or below the pinned epoch's settled tip,
   `DeriveLag` when the derive head trails the canonical swept-through marker.
   Transparent-retention maintenance is clamped by a `retention_release_height` so
   canonical never deletes spend facts a durable consumer has not consumed,
   and ingest startup refuses a derive store whose cursor is behind the swept
   marker. No new RPC, message, capability string, or config knob;
   `wallet.read.transparent_spends_by_outpoint_v1` strengthens in place.
   New ADR.

4. **Spentness authority split.** Spentness is decided by absence from
   `TransparentUnspentOutputsByOutpoint` (durable, LtHash16-committed,
   ADR-0026) for outputs the wallet knows exist. The spend projection resolves
   only the spender identity; a projection miss yields the trait's
   `SpentSpenderUnknown` retry arm, never a spent-versus-unspent verdict.
   Recorded in the same ADR as decision 3.

5. **Per-consumer derive schema versioning precedes the sweep clamp.**
   `DERIVE_SCHEMA_VERSION` is one store-global `u16`; combined with the clamp,
   any future bump would force a from-genesis canonical re-ingest. Derive
   schema versions become per-consumer with scoped wipe-and-rebuild, so
   unrelated consumer iterations never invalidate the spend projection.

6. **Bulk serving: same contract, streamed materialization.**
   `FullBlocksInRange` keeps its name, wire shape, and capability. The
   per-request cap rises from 64 to 1000 (one request per Zallet scan batch)
   and whole-window buffering is replaced by a demand-driven driver on the
   pinned epoch reader (the `chain_events` stream pattern), bounding stream
   memory near 50 to 80 MB. The failure mode observably changes from
   pre-stream `NOT_FOUND` to mid-stream status after delivered chunks; callers
   handle partial-range delivery under a still-consistent view.

7. **Backend build arm and error mapping.** The Zallet `zinder` feature
   enables `spend-index`. Epoch-expired and pin-unavailable statuses,
   `DeriveLag`, and transport failures map to `ChainError::Unavailable`
   (Zallet's only retry-and-re-pin signal); malformed payloads map to
   `InvalidData`; remaining `FAILED_PRECONDITION` reasons (deployment gaps)
   map to `Backend`. Above-view-tip reads translate to `Ok(None)`; below-tip
   `NOT_FOUND` is a hard error, never `None`.

8. **Connect-time preflight.** The backend hard-fails at startup on: network
   string mismatch against `ServerInfo`, missing required capabilities
   (probing `wallet.events.chain_v1` for the event surface; the mempool
   capabilities are advertised independently of the ingest-control proxy),
   and a contract revision floor.

9. **Co-located topology.** `WalletQuery` has no auth interceptor and no TLS
   (ADR-0006 reverse-proxy posture). The supported v1 deployment is Zallet
   co-located with zinder-query over loopback or LAN. Remote deployments
   require an operator proxy plus Zallet-side TLS and auth-header channel
   support, out of scope.

10. **Wallet-serving deployment posture.** `storage.raw_blob_policy = "all"`
    from genesis (blobs are commit-time only, no backfill), ingest floor at or
    below the minimum wallet birthday served, broadcaster configured,
    ingest-control proxy wired on zinder-query, tree-state upstream wired for
    non-checkpoint heights.

11. **Environment ladder.** Every change validates on the local z3 regtest
    stack first (fast loop), then on the local z3 testnet stack (full
    validation, including the only storage-epoch rehearsal), and reaches the
    Railway mainnet deployment last, blue/green, via the `v*` tag pipeline
    only. Mainnet never pays a re-ingest to validate something testnet could
    have caught.

12. **Upstream engagement waits for a settled contract.** The backend builds
    in a fork of `zcash/wallet` shaped as today's tree (third feature beside
    `zaino` and `zebra-state`). The upstream proposal issue is filed after
    zcash/wallet#540 (independent backend resolution graphs) resolves and the
    consumed Zinder surfaces are frozen, and it carries an explicit
    contract-stability commitment for those surfaces. Commits destined for
    `zcash/wallet` follow its AI-disclosure policy (`Co-Authored-By`
    identifying the AI system); Zinder and zally repos keep their own commit
    conventions.

13. **zally migrates immediately after the event break.** `zally-chain`
    consumes `ChainEvents` over the wire; its `Option<ChainEventCursor>` start
    argument maps `None` to an explicit start variant. The migration lands
    before any shared deployment serves the new contract.

## Guardrails

- Never couple a reversible contract change to an irreversible storage epoch;
  the event redesign and the derive schema bump land in separate phases.
- No `_v2` capability sprawl: `_v1` semantics revise in place; skew detection
  rides the `ServerInfo` revision marker, not string renames.
- Spentness is never answered from the spend projection alone (decision 4).
- Single-pinned-epoch streaming stays: no per-element `at_epoch`, client
  cursors, or page sizes on current-projection streams.
- Storage-vs-wire byte order (ADR-0024): hash material routes through
  `crates/zinder-core/src/wire/`; the lightwalletd-compat proto stays frozen.
- Rejected shapes stay rejected: canonical spend-fact retention knob,
  `not_indexed` response arm, composed snapshot-plus-deltas wallet stream
  (session lifecycle stays at the compat shim), height-floor blob retention,
  stream concurrency knobs, buf.build publishing, conformance suite.

## Environments

- **regtest (z3, local).** Fast loop for every Zinder phase and the backend's
  first end-to-end sync. Bring-up and quirks: Zebra host RPC 29232, no
  Zebra-indexer (z3 uses Zaino), `--network` is a binary CLI flag, ingest
  auto-builds the derive store.
- **testnet (z3, local).** Full validation and the storage-epoch rehearsal.
  Zebra RPC 18232, health 18080; state persists across compose runs. Start:
  `docker compose --env-file .env.testnet -f docker-compose.yml -f
  docker-compose.testnet.override.yml up -d` (the override pins Zebra to
  native arm64).
- **mainnet (Railway).** Production deployment, blue/green on a second
  service and volume, cut over via the `v*` tag pipeline. Sync time and disk
  (~230 to 280 GB with blobs) make local mainnet impractical.

## Phase index

0. Environment prep (testnet Zebra sync head start)
1. Event surface redesign (Zinder, breaking)
2. zally migration
3. Bulk-serving rework (Zinder)
4. Per-consumer derive schema versioning (Zinder)
5. Spend durability and retention clamp (Zinder)
6. Testnet storage epoch and wallet-serving recipe
7. Contract packaging and tagged release
8. Backend implementation (wallet fork)
9. Mainnet storage epoch (Railway)
10. Upstream engagement

Phases 1 and 3 are independent and may overlap. Phase 2 gates deploying phase
1 to shared deployments. Phase 5 requires phase 4. Phase 8 requires phases 6
and 7. Phase 9 requires phase 8 green on testnet.

## Phase 0: environment prep

The local z3 testnet stack runs so Zebra reaches tip before phase 6 needs it.
Regtest bring-up is on demand per phase. No repository changes.

## Phase 1: event surface redesign

Decision 2. Contract-only, no on-disk schema.

Files:
- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`: start oneof on
  both event requests, `events_resume_cursor`, delete `snapshot_sequence`,
  family-resolution comments.
- `crates/zinder-proto/proto/zinder/v1/ops/server_info.proto` and
  `crates/zinder-proto/src/capabilities.rs`: contract revision marker.
- `services/zinder-ingest/src/ingest_control.rs`: start-position resolution,
  `live_tail` head-cursor minting, mempool-index last-applied event position
  (atomic with entry application), first-page resume-cursor minting threaded
  through snapshot paging.
- `services/zinder-query/src/grpc/chain_events.rs`: proxy passthrough.
- `services/zinder-compat-lightwalletd`: collapse the two hand-rolled
  tail-start sites (GetMempoolStream snapshot head-read plus client-side
  filter; tip-change publisher full-window replay) onto `live_tail` and
  `after_cursor`.
- `crates/zinder-client`: stream API takes the start position explicitly.
- Tests: generated-message round-trips in `crates/zinder-proto/tests/`,
  `wire_invariants.rs`, `capability_coverage.rs`, ingest-control integration
  tests (tail semantics, family mismatch, resume-cursor exactness across
  paging, lag-past-retention expiry), compat-shim parity.
- Docs, same change: new ADR (event start positions and snapshot-anchored
  resume), revision entry in ADR-0025, event sections of
  `docs/architecture/wallet-data-plane.md`,
  `docs/reference/error-vocabulary.md` for any new reason strings.

Gate: default validation gate, then the live regtest suite
(`ci-live`), exercising mempool snapshot-then-stream and chain-event
tail subscriptions against z3 regtest.

## Phase 2: zally migration

Decision 13. In the zally repository.

Files: `crates/zally-chain/src/zinder_source.rs` (map the `None` cursor arm to
an explicit start variant; thread the choice through the sync driver),
workspace pin bump to the phase 1 Zinder revision.

Gate: zally workspace gate plus its funded live round-trip suite against a
phase 1 regtest Zinder. Shared Railway deployments update only after this
phase merges.

## Phase 3: bulk-serving rework

Decision 6. Server-internal.

Files: `services/zinder-query/src/lib.rs` (FullBlocksInRange handler: cap
constant, demand-driven driver reusing the `grpc/chain_events.rs` bounded
channel pattern over the pinned epoch reader), perf coverage in the `ci-perf`
tier (sustained range stream, memory ceiling), wallet-data-plane doc section
on mid-stream failure semantics.

Gate: default validation gate plus `ci-perf`; regtest spot check of a
multi-window range under live ingest.

## Phase 4: per-consumer derive schema versioning

Decision 5. Prerequisite for phase 5.

Files: `crates/zinder-derive/src/store.rs` (per-consumer schema record
replacing the single `DERIVE_SCHEMA_VERSION` gate; scoped wipe-and-rebuild on
mismatch), consumer registration surface in `crates/zinder-derive/src/consumer/`,
migration of the existing consumers' declared versions, ADR documenting the
versioning contract (revision to the derive-plane ADR or a new one, whichever
the doc tree owns).

Gate: default validation gate; regtest proof that bumping one consumer's
version rebuilds only its column families while others' cursors hold.

## Phase 5: spend durability and retention clamp

Decisions 3 and 4. One derive schema change, landed together with the clamp.

Files:
- `crates/zinder-derive/src/consumer/transparent_outpoint_spend.rs` (new
  consumer: outpoint-keyed rows plus per-height rewind index).
- `crates/zinder-store/src/chain_store.rs`: `retention_release_height` clamp
  in `build_transparent_retention_sweep`; swept-through marker exposure.
- `services/zinder-ingest`: startup guard refusing a derive store behind the
  swept marker; release-height feed from the derive cursor.
- `services/zinder-query/src/lib.rs`: union-routed read inside the
  TransparentSpendsByOutpoint handler (canonical first, derive at or below
  settled tip, `DeriveLag` otherwise).
- Tests: consumer rewind property tests, sweep-clamp invariants (mutants over
  the sweep per the heavy-probe list), union-read routing, guard refusal.
- Docs, same change: new ADR (durable spend resolution, authority split,
  sweep clamp), wallet-data-plane spend sections, capability table notes.

Gate: default validation gate plus the trust-sensitive heavy probes
(`cargo mutants` over `chain_store.rs` sweep paths), then regtest: spend an
outpoint, advance past the reorg window, verify the spender resolves from the
projection and absence from the unspent set still decides spentness.

## Phase 6: testnet storage epoch and wallet-serving recipe

Decisions 10 and 11. Operational; local z3 testnet.

Steps: wipe the local testnet store; run zinder-ingest from genesis with
`storage.raw_blob_policy = "all"`, broadcaster, ingest-control proxy, and
tree-state upstream configured; let the derive plane build the spend
projection; validate (UTXO-set commitment parity, spot spend lookups deep in
history, full-block serving across the whole range, tree state at
non-checkpoint heights). Document the posture as the wallet-serving recipe in
the operations doc that owns deployment guidance.

Gate: `ci-live` testnet suite green against the rebuilt store.

## Phase 7: contract packaging and tagged release

Decision 1, Zinder side.

Files: `docs/reference/integration-surfaces.md` ("Vendoring the Protocol"
section: the five-file set, COMMIT-pin recipe, capability and error-reason
registries as the out-of-proto contract surfaces). Tag a release so the
Zallet pin references a stable commit whose images the tag pipeline publishes.

Gate: default validation gate; docs review.

## Phase 8: backend implementation

Decisions 1, 7, 8, 9, 12. In the wallet fork.

Files:
- `zallet/proto/zinder/v1/` (vendored five files, `COMMIT`, licensing note),
  CI drift job diffing against the pinned Zinder commit.
- `zallet/Cargo.toml` and `zallet/src/lib.rs`: `zinder` feature enabling
  `spend-index`, three-way `compile_error!` exclusivity guards, `ChainBackend`
  alias arm.
- `zallet/src/components/chain/zinder.rs`: `ZinderChain` and
  `ZinderChainView`. Adapter contract: epoch capture from `LatestBlock`;
  `at_epoch_id` threading; chunked `FullBlocksInRange` for `stream_blocks*`;
  header slice via `BlockHeader::read` on `FullBlock` bytes; tree-state JSON
  parsing with empty-tree versus unavailable disambiguation; mempool compose
  (snapshot pages, `MempoolEvents(after_cursor)` with idempotent application,
  `ChainEvents(live_tail)` for tip-change termination, `Ok(None)` on stale
  epoch); spend status per decision 4; subtree-root pagination; broadcast
  outcome mapping; wire conversions (hash byte-flips, range-end exclusivity,
  block-time narrowing); preflight per decision 8; error table per decision 7.
- Config: `[indexer.zinder]` with the gRPC address, following the existing
  backend config subsection pattern; generated example config update.
- Tests: unit tests over conversions and error mapping; a mid-batch
  epoch-expiry re-pin test; live sync against z3 regtest (initialize,
  recover_history, steady_state, broadcast, reorg through the synthetic-reorg
  event path); then a full testnet sync against the phase 6 store, plus
  Zinder's `ci-zallet-live` suite.

Gate: Zallet workspace checks (fmt, clippy, tests) under the `zinder`
feature; regtest end-to-end green; testnet full sync green with balances
cross-checked against the `zebra-state` backend on the same seed.

## Phase 9: mainnet storage epoch

Decision 11. Railway.

Steps: verify volume headroom for the ~230 to 280 GB target plus the
blue/green doubling window; stand up the second service and volume; re-ingest
from genesis with the wallet-serving posture via the tag pipeline; cut over;
decommission the old volume. Watch zebra-mainnet upstream load and zexplorer
freshness during the re-ingest.

Gate: production metrics steady (derive lag, memory pressure), zexplorer
parity spot checks, a mainnet Zallet smoke sync from a recent birthday.

## Phase 10: upstream engagement

Decision 12. After zcash/wallet#540 resolves and phases 1 through 8 freeze
the consumed surfaces.

Steps: file the proposal issue (scope, deployment posture, the
contract-stability commitment for the consumed surfaces, pointers to the
running backend); on acknowledgment, open the PR moving the backend into
whatever packaging #540 landed, under Zallet's toolchain, cargo-vet, and
AI-disclosure rules.

Gate: upstream CI green on the PR branch; disclosure trailers on every
commit.
