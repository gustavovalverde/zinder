# Plan: Naming and structure legibility

Status: Proposed

Scope: crate names, module file names, and vocabulary-spine documents on
`codex/fact-first-production-candidate` versus `main`. This plan does not
propose changing any service boundary, storage layout, or protocol byte: every
rename here is source-level (crate names, module paths, doc prose). It does
not re-litigate the ownership decisions in
[service boundaries](../architecture/service-boundaries.md) or
[ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md); it
makes the existing decisions easier to find and harder to misread.

Architecture authority: [Public interfaces](../architecture/public-interfaces.md)
(the vocabulary spine every rename here targets),
[Codebase structure conventions](../architecture/public-interfaces.md#avoid).

## How this was produced

Four candidate findings from a manual read were adversarially checked against
fresh grep evidence, and four independent sweeps searched for findings the
manual read missed: `zinder-runtime` crate cohesion, a full cross-crate
name-collision scan, a top-down newcomer-reading simulation of the doc set in
its natural reading order, and a naming-legibility pass over
`zinder-ingest`/`zinder-projector`'s module trees. One sweep claim (that
`tip_follow.rs` is wired live into production) was independently re-checked
against `main.rs`'s actual call sites and found to be a false positive: the
file only shares a name with a still-read config field
(`loop_config.tip_follow.poll_interval`), not with anything the binary calls.
It is correctly listed as dead in
[the simplification audit](fact-first-simplification-audit.md) §1.1 and is
excluded from the renames below.

Two proposed names were revisited after review: the first pass proposed
keeping `zinder-derive` (adding only a `-plane` suffix) and grouping
`zinder-ingest`'s canonical-writer files under a new `canonical/`
subdirectory. Both were replaced below with sharper alternatives; the
rationale for each replacement is recorded in its decision so the reasoning
is not lost if a future contributor wonders why the obvious shorter name was
not used.

A second pass checked the four sweeps' full finding lists against the plan
and closed out six items the first draft had left unresolved: two folded
into decision 10 alongside the original doc-accuracy fixes, and four that
needed their own decisions (11 through 15, one of which spans two file
renames). One of those, comparing `transaction_history_verifier.rs`'s word
order against the newly-renamed `writer/replay_verification.rs`, was
re-checked the same way the `tip_follow.rs` claim was: `grep` showed its only
caller is `projection_startup.rs`, one of [the simplification
audit](fact-first-simplification-audit.md) §1.1's already-dead files, so it
needs deletion (already scoped there), not a rename.

A third pass examined the "fact-first" vocabulary itself (decisions 16 and
17, phase 9) and re-assessed every earlier decision against it. That pass
changed two things: phase 3 gained a sequencing gate (decision 18), and
phase 6's code doc comment no longer links a plan document (plans expire at
merge; a crate doc comment must stand alone).

A fourth pass applied the project's delete-don't-keep rule to the plan
itself, asking of every phase: does this remove baggage, or relabel it? That
pass found the first drafts renaming dead code in one place (phase 3
previously updated types inside the production-dead
`wallet_projection_read.rs` instead of deleting it), found one
production-dead surface wider than any existing deletion list (decision 20),
and named the largest structural baggage in the workspace as a terminal
deletion target rather than leaving it implicit (decision 21). The stance
that came out of it is decision 19: in this plan, deletion outranks
renaming, and no rename may touch code whose deletion is already justified.

## Decisions

1. **`zinder-derive` renames to `zinder-materialized-views`, and every
   `Derive*` type renames to `MaterializedView*`.** The bare `-derive` suffix
   collides with the Rust ecosystem's near-universal convention that a
   `*-derive` crate is a proc-macro crate (`serde_derive`,
   `thiserror_derive`); this crate has none. A crate-name-only fix (adding a
   `-plane` suffix) would have resolved that collision but left the deeper
   problem: "derive" is a near-synonym of "project," which is why it
   overlaps with the newer wallet-projection vocabulary in the first place.
   Fixing the crate name and leaving `DeriveConsumer`/`DeriveStore` in place
   would still let every type signature, `impl` block, and doc comment carry
   the ambiguous word. "Materialized view" is not an invented term: it is the
   literal phrase `derive-plane.md`'s own prose already uses ("produces
   materialized views"), and it is the standard CQRS/event-sourcing name for
   this exact pattern (persistent state rebuilt by replaying an event
   stream), so this promotes existing prose vocabulary into the type system
   rather than inventing new words.
2. **The type prefix is `MaterializedView`, not the shorter `View`.** `View`
   alone would collide with `ChainView`, the already-established cross-plane
   chain-state envelope carried on every `WalletQuery`/`ExplorerQuery`/
   `IngestControl` response. Both concepts are genuinely about "a view of
   state," so a bare `View` prefix would recreate a smaller-scale version of
   the exact ambiguity this rename sets out to remove. `Index` was considered
   and rejected as too narrow: `BlockSummaryConsumer` and
   `ReorgIncidentsConsumer` do not produce indexes, they produce a summary
   record and an incident log, so `IndexConsumer` would be a specificity lie
   for half the SDK's own production consumers.
3. **`derive-plane.md` renames to `materialized-view-plane.md`, keeps the
   `-plane` suffix (matching its siblings `wallet-data-plane.md` and
   `explorer-plane.md`), and drops its "sibling document to Wallet Data
   Plane" framing.** `crates/zinder-wallet-projection/src/lib.rs` already
   calls the old `zinder-derive` "the legacy derive plane" in its own doc
   comment; no checked-in architecture doc says so. The document should state
   plainly that `zinder-wallet-projection`, `zinder-wallet-rocksdb`, and
   `zinder-projector` do not depend on this SDK, and that its live production
   consumer is `zinder-explorer`'s consumers
   (`BlockSummaryConsumer`, `ReorgIncidentsConsumer`,
   `CommitmentRootSearchConsumer`), not the wallet-serving path.
4. **`zinder-rocksdb` renames to `zinder-bulk-load`.** Its own doc comment
   disclaims owning "cross-engine storage abstractions"; its only module is
   `bulk_load`. The name should describe what is actually inside, not what
   the crate was expected to grow into.
5. **`zinder-client` keeps its name; gains one clarifying doc-comment
   sentence.** Its name is accurate for what it is (an external consumer
   SDK); the problem found was that nothing signals it is *not* the answer to
   [the simplification audit](fact-first-simplification-audit.md) §2.3's
   already-scoped internal lazy-gRPC-client duplication. A doc-comment
   pointer resolves that without a rename.
6. **The six `canonical_*.rs` writer-loop files in
   `services/zinder-ingest/src/` move into a `writer/` subdirectory, not a
   `canonical/` one.** "Canonical" describes the *data* (the settled chain
   state) and is already the correct word for that, used consistently
   everywhere else in the codebase (`zinder-store`'s `canonical_store`,
   `CanonicalBlockFacts`, `CanonicalControl`); reusing it here would not
   actually distinguish this subdirectory from its sibling `bulk_catchup/`
   (confirmed to also build canonical facts, just as a separate fixture/bench
   builder, not a rival production path — verified via `main.rs` import
   sites, none of which reference `bulk_catchup::`). What this subdirectory
   groups is not the *data*, it is the *role*: the process that writes it.
   `public-interfaces.md` and `service-boundaries.md` already use `writer`
   for exactly that role ("the only writer to canonical chain storage",
   "writer-visible `ChainEpoch`", "writer fence"). No existing module or type
   in the workspace is named `writer`/`Writer`, so it is free to use.
7. **The orchestrator file becomes `writer/mod.rs`, not a leaf file named
   `writer/writer.rs` or `canonical/writer.rs`.** `mod.rs` is the idiomatic
   Rust location for a module's entry point when the module has sub-files, so
   this needs no artificial second name.
8. **`canonical_replay_verification.rs` drops its prefix in place
   (`replay_verification.rs`) but does not nest inside `writer/`.** It is
   declared from `main.rs`, not `lib.rs` (`mod canonical_replay_verification;`
   at `main.rs:36`), as a CLI-only subcommand. Nesting a `main.rs`-declared
   module inside a `lib.rs`-declared module tree needs `#[path]` attribute
   workarounds for no legibility benefit, so it stays a crate-root sibling.
9. **`canonical_ingest_control.rs` only reclaims the plain
   `writer/ingest_control.rs` name after the legacy
   `services/zinder-ingest/src/ingest_control.rs` is deleted.** That deletion
   is already scoped in
   [the simplification audit](fact-first-simplification-audit.md) §1.1 and is
   a prerequisite for this rename, not part of it.
10. **Six documentation-accuracy fixes ride along because they were found by
    the same pass and are one-paragraph edits**: `CLAUDE.md`'s stale
    four-runtime orientation line, `public-interfaces.md`'s vocabulary table
    missing `zinder-projector`, `docs/README.md`'s ADR index stopping at
    ADR-0032 (0033-0035 exist and `README.md` already links ADR-0035), an
    explanatory line distinguishing `service-boundaries.md`'s "schema-v4"
    from `public-interfaces.md`'s "store schema 14" (both correct, different
    axes, never bridged), `fact-first-indexer.md`'s Explorer Projection
    Contract wording that reads as extending `zinder-projector` to explorer
    projections (it does not; `service-boundaries.md` scopes it to wallet
    state only), and `fact-first-indexer.md`'s Consumer and Data Matrix still
    calling Zally's read surface "safe tips" after `public-interfaces.md`
    explicitly retired that term for `settled_tip`.
11. **`WalletStoreControl` renames to `WalletStoreControlRecord`.** Confirmed
    a plain Rust struct
    (`crates/zinder-wallet-projection/src/control.rs`,
    `#[derive(Clone, Debug, Eq, PartialEq)] pub struct WalletStoreControl`),
    not a protobuf-generated or wire-serialized-by-name type, so the rename
    touches no protocol contract. This resolves one arm of the "Control" word
    cluster the cross-crate collision sweep found: `CanonicalControl` and
    `IngestControl` are gRPC service names (out of scope here; wire
    contracts), `WalletStoreControl` was a durable stored record wearing a
    bare service-shaped name, and `ProjectorControlCommand` (a local
    in-process `mpsc` enum) already carries the right disambiguating suffix
    and needs no change.
12. **`services/zinder-projector/src/bin/zinder-projector/canonical_control.rs`
    renames to `canonical_lease_client.rs`.** It wraps `CanonicalControlClient`,
    "the authenticated client for canonical retained-event lease ownership"
    per its own doc comment. It currently shares a filename with
    `services/zinder-ingest/src/canonical_control.rs` (the unrelated
    server-side handle, renamed to `writer/control.rs` in phase 5) despite
    the two files being opposite ends of the same RPC exchange in different
    crates. The new name states what the file actually is and stops
    colliding with its own directory sibling `projector_control.rs` (a
    distinct, unrelated local command enum that keeps its name — see
    decision 11).
13. **The `zinder-store::kv` code-location question is recorded, not fixed,
    here.** The cross-crate sweep found that the RocksDB primitive every
    storage engine actually shares (`open_bounded_rocksdb`,
    `RocksDbResourceBudget`, `build_block_based_table_factory`) lives in
    `zinder-store::kv`, not in `zinder-rocksdb` (renamed to `zinder-bulk-load`
    in decision 4 precisely because its own doc comment disclaims owning
    that). Renaming `zinder-rocksdb` makes the crate's name honest about what
    is inside it; it does not move code between crates. Whether the shared
    primitive should move out of `zinder-store` into a crate positioned below
    both `zinder-store` and `zinder-wallet-rocksdb` is a dependency-graph
    decision, not a naming decision, and needs its own investigation into why
    it ended up there before anyone proposes moving it.
14. **`bulk_catchup/`'s member files gain `//!` doc comments; no rename.**
    All four files (`mod.rs`, `block_prepare.rs`, `commit_reassembly.rs`,
    `flush.rs`) currently have none, unlike every sibling subdirectory in the
    same crate (`mempool/`, `cli/`, and `writer/construction/` once phase 5
    lands). Checked: the directory's own name is already accurate (it is
    `zinder-explorer`'s and `zinder-bench`'s shared fixture/bench builder, not
    a rival to the writer's fresh-construction path — confirmed via `main.rs`
    import sites in decision 6). This is a documentation gap, not a naming
    one.
15. **`transaction_history_verifier.rs` needs no rename.** The cross-crate
    sweep flagged its word order (`{noun}_verifier`) as inconsistent with the
    newly-renamed `writer/replay_verification.rs` (`{noun}_verification`).
    Checked: it is one of [the simplification
    audit](fact-first-simplification-audit.md) §1.1's eight already-dead
    backfill/verifier modules — its only caller, `projection_startup.rs`, is
    itself on that same dead list. It is deleted in phase 4, which resolves
    the apparent inconsistency by removing one side of the comparison rather
    than renaming it.
16. **"Fact-first" is a migration label, not domain vocabulary; every
    `FactFirst*` code identifier retires once its legacy counterpart is
    deleted.** The term's semantic content — persist immutable block-local
    facts, derive everything else later — is real and durable, but the
    codebase already owns precise words for both halves: `canonical` (the
    facts) and `projection` (the derived state). What the compound
    "fact-first" adds is only *contrast with the legacy architecture*: it
    answers "as opposed to what?", and the answer is code scheduled for
    deletion. Each of the nine `FactFirst*`/`run_fact_first_*` identifiers
    confirms this by grep: `FactFirstWalletQuery` is the *only*
    `WalletQueryApi` implementation (the prefix distinguishes it from
    nothing that will survive phase 4); `FactFirstMempoolOwner` contrasts
    only with the dead `run_mempool_orchestrator`; the mempool is live state,
    not facts, so its prefix never described the code at all. Once legacy is
    gone these names mark the era the code was written in, not what it does
    — the same dead-context pattern as a `unified-model.ts` whose merge
    nobody remembers. Two boundaries on this decision: the noun **"fact" by
    itself stays** — `CanonicalBlockFacts`, `TransactionPublicFacts`, and
    `zinder-bench`'s `canonical_fact_round_trip` name real domain content
    and are untouched; and the renames are **gated on phase 4**, because
    while the legacy counterparts still compile, the prefix performs real
    disambiguation and removing it early would create genuine ambiguity.
17. **"Fact-first" survives in exactly two documentation tiers: ADR-0035's
    title (it is the proper name of a decision made at a point in time, and
    the citation web across docs and plans anchors to it) and
    plans/investigations (dated genres whose scope ends at merge).** The
    steady-state tiers — architecture docs, runbooks, `README.md`,
    `CLAUDE.md` — migrate their load-bearing uses ("the first fact-first
    release", "fact-first stores") to canonical/projection vocabulary, with
    at most a citation-style mention when pointing a reader at ADR-0035.
    `fact-first-indexer.md`'s retitle is deferred to the same editorial pass
    already excluded from phase 1, since that document's genre problem and
    its title are one repair.
18. **Phase 3 waits for the explorer-projection-migration direction.** The
    [cutover plan](fact-first-wallet-serving-cutover.md)'s priority table
    schedules "explorer projection migration and deletion of its replaced
    legacy ownership" after wallet-ready. That migration decides whether the
    materialized-view SDK survives as the explorer's projection machinery or
    is reshaped onto the newer projection contracts. The rename in decision 1
    is correct either way, but executing the full type rename immediately
    before a rework would churn every consumer twice. Land phase 3 after the
    explorer-migration direction is settled — and if that direction replaces
    the SDK, phase 3 becomes a deletion instead of a rename, which under
    decision 19 is the preferred outcome.
19. **Deletion outranks renaming.** The project rule is that superseded code
    is deleted, not carried; a rename spent on code whose deletion is
    already justified is entropy-polishing. Concretely: no phase in this
    plan renames anything inside a file on a deletion list, and wherever a
    finding could be resolved by either a rename or a deletion, the deletion
    wins. Phase 4 is therefore not a mere prerequisite pointer; it is the
    highest-value legibility work in this plan, because the ~16,600 dead
    lines it removes cost every future reader more than any name here saves.
20. **`zinder-query`'s optional projection-reader surface is
    production-dead and becomes deletion scope this plan owns (phase 4b),
    because no existing deletion list covers it.** The surface is wider than
    one file: `wallet_projection_read.rs` (the
    `zinder_derive`-backed reader), `derive_store_wallet_projection_reader`,
    the `with_wallet_projection_reader`/`with_derive_store` builder setters
    on `zinder-query`'s query type and gRPC adapter, and the mirrored
    optional field and readiness branch in
    `services/zinder-compat-lightwalletd/src/grpc.rs`. Verified by grep: no
    production binary ever calls a setter, so the field is `None` in every
    deployed process; the only callers are integration tests
    (`transparent_address_tx_history.rs`, `lightwalletd_grpc.rs`,
    `zinder-client`'s `zodl.rs` parity test), which exercise a serving path
    production replaced with the wallet-store pair. Deleting the surface
    requires migrating or deleting those tests with it; that test work is
    part of the phase, not a reason to defer it. One boundary: after the
    deletion, `zinder-query`'s only remaining `zinder_derive` use is the
    `ProjectionPreset` enum in `grpc/native.rs`; whether to relocate that
    enum to a domain crate or keep the dependency until the explorer
    migration settles is decided at implementation and recorded in the
    phase.
21. **`crates/zinder-store/src/chain_store.rs` is the named terminal
    deletion target, gated on the explorer cutover.** `zinder-store`
    currently hosts two generations of the same responsibility with zero
    shared code between them: the 5,715-line legacy engine
    (`PrimaryChainStore`/`SecondaryChainStore` and its sibling modules) and
    the version-1 `canonical_store/`. The legacy engine survives only
    because `zinder-explorer` (itself post-cutover work) still opens it, and
    because test fixtures across the workspace build on it. This plan cannot
    delete it — the explorer migration owns that trigger — but it names the
    endpoint so the crate's ambiguity has a recorded resolution: when the
    explorer cuts over, `chain_store.rs` and its exclusive siblings go, the
    crate becomes single-engine, and decision 13's `kv/`-placement question
    reopens with fewer constraints. Separately, the empty untracked
    directory `crates/zinder-store/src/canonical_fact_store/` is abandoned
    local scaffolding referenced by nothing; remove it immediately (phase
    4c).
22. **`zinder-client`'s consumer story is a recorded question, not a
    decision here.** Verified: no crate in this workspace links it, and
    Zallet — its one named intended consumer — vendors the wire contract
    (`zallet/proto/zinder/`) instead of linking the SDK. Phase 6's doc
    sentence is correct regardless. Whether a Rust SDK with zero linked
    consumers earns its maintenance surface, or should be reduced to the
    versioned proto artifact its intended consumer actually adopted, is a
    product decision outside a naming plan; it is recorded here so the next
    product-scope review starts from evidence.

## Phase index

1. [Doc-accuracy fixes](#phase-1-doc-accuracy-fixes) — no code change, no
   dependency on later phases.
2. [`zinder-rocksdb` to `zinder-bulk-load`](#phase-2-zinder-rocksdb-to-zinder-bulk-load) —
   independent, lowest risk rename.
3. [`zinder-derive` to `zinder-materialized-views`](#phase-3-zinder-derive-to-zinder-materialized-views) —
   the largest single-phase diff in this plan; gated on the
   explorer-projection-migration direction (decision 18), not on any other
   phase.
4. [Legacy deletion](#phase-4-legacy-deletion) — the highest-value phase
   (decision 19): the audit's ingest cluster (4a), the `zinder-query`
   projection-reader surface this plan adds to the deletion scope (4b), and
   the empty scaffolding directory (4c).
5. [`canonical_*.rs` to `writer/`](#phase-5-canonical_rs-to-writer) —
   depends on phase 4a completing first.
6. [`zinder-client` doc clarification](#phase-6-zinder-client-doc-clarification) —
   independent, can run any time.
7. [Control-vocabulary disambiguation](#phase-7-control-vocabulary-disambiguation) —
   independent, touches `zinder-wallet-projection` and `zinder-projector` only.
8. [`bulk_catchup/` doc comments](#phase-8-bulk_catchup-doc-comments) —
   independent, documentation-only.
9. [`FactFirst*` retirement](#phase-9-factfirst-retirement) — depends on
   phase 4a (the legacy contrast must be deleted first) and should land
   after or with phase 5 (both touch the same `zinder-ingest` files).

Phases 1, 2, 6, 7, and 8 have no ordering dependency on each other and can
run in any sequence or in parallel; phases 4b and 4c are likewise
independent and can land immediately. Phase 3 waits on the
explorer-projection-migration direction (decision 18) and on phase 4b (so
the type rename never touches the dead reader). Phase 5 must not start
until phase 4a lands; phase 9 follows phases 4a and 5. The terminal
`chain_store.rs` deletion (decision 21) is outside this plan's executable
window and fires with the explorer cutover.

## Phase 1: Doc-accuracy fixes

| File | Change |
| --- | --- |
| `CLAUDE.md` | Replace "who owns what across the four runtimes (`zinder-ingest`, `zinder-query`, `zinder-compat-lightwalletd`, `zinder-explorer`)" with a description matching `service-boundaries.md`'s current shape: three deployable services (`zinder-ingest`, `zinder-projector`, `zinder-compat-lightwalletd`) plus the `zinder-query` library; `zinder-explorer` is post-cutover work, not one of the four. |
| `docs/architecture/public-interfaces.md` | Add a `zinder-projector` row to the "Product and runtimes" table (near line 33), matching the description already used in `service-boundaries.md`: "Production service that owns wallet-store construction, following, and reconciliation." |
| `docs/README.md` | Extend the ADR index through ADR-0035 (add ADR-0033, ADR-0034, ADR-0035 entries; `README.md`'s own quickstart already links ADR-0035, so the index currently can't find a page its sibling doc sends readers to). |
| `docs/architecture/service-boundaries.md` and/or `docs/architecture/public-interfaces.md` | Add one sentence next to each "schema-v4"/"store schema 14" mention noting these are different axes (product-generation label vs. RocksDB's internal migration counter) so a reader doesn't read them as contradictory. |
| `docs/architecture/fact-first-indexer.md` (Explorer Projection Contract) | Reword "An independent `zinder-projector` process will own build, verify, catch-up, follow, and promotion for one selected projection" so it cannot be read as extending `zinder-projector` to explorer projections; `service-boundaries.md` scopes it to wallet-store ownership only. |
| `docs/architecture/fact-first-indexer.md` (Consumer and Data Matrix) | Replace "safe tips" with `settled_tip`, matching the retirement `public-interfaces.md` already documents. |

Not included here: trimming `fact-first-indexer.md`'s dated evidence-log
content into `docs/investigations/`. That is a larger editorial pass (the
file mixes durable "why" content with dated benchmark narration throughout,
not in one separable section) and belongs in its own change once this plan's
renames are no longer shifting the file paths that document would reference.

## Phase 2: `zinder-rocksdb` to `zinder-bulk-load`

| Before | After |
| --- | --- |
| `crates/zinder-rocksdb/` | `crates/zinder-bulk-load/` |
| `zinder-rocksdb` (package name, `Cargo.toml`) | `zinder-bulk-load` |
| `zinder_rocksdb::*` (import path) | `zinder_bulk_load::*` |

Update the workspace `Cargo.toml` member path, every consuming crate's
`Cargo.toml` dependency line and `use zinder_rocksdb::` import (`zinder-store`,
`zinder-wallet-rocksdb`), and the crate's own doc comment (no content change
needed there; it already accurately describes bulk-load mechanics only).

Validation: `cargo check --workspace --all-targets --all-features`,
`cargo machete`, `git grep -n zinder-rocksdb` and `git grep -n zinder_rocksdb`
return no hits outside this crate's own now-renamed identity.

Not included here: moving `zinder-store::kv`'s shared RocksDB primitives
(`open_bounded_rocksdb`, `RocksDbResourceBudget`,
`build_block_based_table_factory`) into this crate so its new name is fully
accurate about what "shared RocksDB infrastructure" means workspace-wide.
That is a dependency-graph decision (see decision 13), not a naming one, and
needs its own investigation before anyone proposes it.

## Phase 3: `zinder-derive` to `zinder-materialized-views`

| Before | After |
| --- | --- |
| `crates/zinder-derive/` | `crates/zinder-materialized-views/` |
| `zinder-derive` (package name, `Cargo.toml`) | `zinder-materialized-views` |
| `zinder_derive::*` (import path) | `zinder_materialized_views::*` |
| `DeriveConsumer` (trait) | `MaterializedViewConsumer` |
| `DeriveStore` | `MaterializedViewStore` |
| `DeriveConsumerName` | `MaterializedViewConsumerName` |
| `DeriveMempoolConsumer` | `MaterializedViewMempoolConsumer` |
| `DeriveConsumerCtx` | `MaterializedViewConsumerCtx` |
| `DeriveStoreTable` | `MaterializedViewStoreTable` |
| `DeriveStoreReadSnapshot` | `MaterializedViewStoreReadSnapshot` |
| `DeriveStore::write_chain_event` (method) | `MaterializedViewStore::write_chain_event` (method name unchanged, receiver type renamed) |
| `BlockKeyedConsumer` | unchanged (already collision-free) |

Update the workspace `Cargo.toml` member path and every consumer's
`Cargo.toml`/imports: `zinder-testkit`, `zinder-client`, `zinder-query`,
`zinder-explorer`, `zinder-ingest`, `zinder-compat-lightwalletd`,
`zinder-bench`. This includes every production `impl` of the renamed traits
in `zinder-explorer` (`BlockSummaryConsumer`, `ReorgIncidentsConsumer`,
`CommitmentRootSearchConsumer`). Phase 4b runs before this phase (see
sequencing), so the rename never touches the deleted projection-reader
surface; per decision 19, if the explorer-migration direction replaces the
SDK outright, this phase converts to a deletion and the table above is
void.

Alongside the crate rename, move and rewrite
`docs/architecture/derive-plane.md` to
`docs/architecture/materialized-view-plane.md` (decision 3 above):

- Retitle from "Derive Plane" to "Materialized View Plane."
- Remove "It is the sibling document to
  [Wallet Data Plane](wallet-data-plane.md)."
- Add a paragraph stating that `zinder-wallet-projection`,
  `zinder-wallet-rocksdb`, and `zinder-projector` do not depend on this SDK,
  that `services/zinder-query/src/wallet_projection_read.rs`'s
  `zinder_materialized_views`-backed reader is unused in production, and that
  this SDK's live production consumer is `zinder-explorer`'s consumers.
- Update every `Derive*` type mention to its `MaterializedView*` name.

`ADR-0009` and `ADR-0017` reference `Derive*` vocabulary extensively;
`ADR-0017` is titled "Derive-consumer template and key-codec convention." Per
this repository's ADR lifecycle rule, this is a clarification (the
underlying design — per-consumer cursor, atomic write-batch dispatch, the
block-keyed vs. event-only trait split — does not change, only its name
does), so both are edited in place: retitle ADR-0017 to "Materialized-view
consumer template and key-codec convention," update every `Derive*`
reference in both ADRs' bodies to `MaterializedView*`, and add a
revision-history entry to each noting the rename and linking this plan.

Update `docs/README.md`'s architecture and ADR index entries for the moved
doc file and the retitled ADR.

Validation: same as phase 2, plus
`RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`
(intra-doc links inside the renamed doc and both ADRs must still resolve),
plus `git grep -n DeriveConsumer\|DeriveStore\|DeriveMempoolConsumer` across
`crates/`, `services/`, and `docs/` returning no hits outside test fixtures
and the dead reader explicitly left in place above.

## Phase 4: Legacy deletion

Per decision 19 this is the highest-value phase in the plan. It has three
independent sub-scopes.

### 4a: Audit §1.1/§1.2 ingest cluster

Execute [the simplification audit](fact-first-simplification-audit.md) §1.1
for `services/zinder-ingest/src/ingest_control.rs` and its sibling dead
files (`ingest_loop.rs`, `projection_startup.rs`, `retention.rs`,
`derive_consumers.rs`, `derive_status_reader.rs`, `tip_follow.rs`,
`mempool/orchestrator.rs`, and the eight backfill/verifier modules), plus the
matching dead config surface in §1.2. This plan does not re-derive that
scope; it depends on it landing before phase 5, because
`canonical_ingest_control.rs` cannot become `writer/ingest_control.rs`
while a same-named `ingest_control.rs` is still readable at the crate root.

This sub-scope's deletion of `transaction_history_verifier.rs` (one of the
eight backfill/verifier modules) also closes decision 15: there is no
separate rename to make once its only caller is gone.

### 4b: `zinder-query` projection-reader surface (decision 20)

New deletion scope owned by this plan:

- `services/zinder-query/src/wallet_projection_read.rs` and the
  `derive_store_wallet_projection_reader` constructor.
- The `with_wallet_projection_reader`/`with_derive_store` builder setters
  and the always-`None`-in-production optional fields on `zinder-query`'s
  query type and `WalletQueryGrpcAdapter`.
- The mirrored optional field and readiness branch in
  `services/zinder-compat-lightwalletd/src/grpc.rs`.
- The integration tests that exist only to exercise this surface
  (`services/zinder-query/tests/integration/transparent_address_tx_history.rs`,
  the reader-wired portions of
  `services/zinder-compat-lightwalletd/tests/integration/lightwalletd_grpc.rs`,
  `crates/zinder-client/tests/parity/zodl.rs`): migrate each test's
  behavioral assertions onto the pair-backed serving path where the behavior
  still exists, and delete the rest with the surface. The migration is part
  of this sub-scope, not a reason to defer it.

Implementation decision recorded per decision 20: the `zinder_derive`
dependency stays in `zinder-query`'s `Cargo.toml`. After this deletion it is
still used by `ProjectionPreset` in `grpc/native.rs` (the native explorer
query wiring) and by the `QueryError::DeriveStore` variant, which wraps
`zinder_derive::DeriveStoreError` and must stay to keep the error-reason table
stable. `ProjectionPreset` is not relocated; the explorer migration owns that
enum's future.

The `QueryError::WalletProjectionRead` variant stays: the fact-first read pair
still maps its admission failure onto it, so removing the projection reader
does not orphan the variant and the error-reason parity guards are unaffected.

Validation for 4b: the default gate plus
`git grep -n "wallet_projection_read\|WalletProjectionReadApi\|derive_store_wallet_projection_reader"`
across `crates/` and `services/` returning no hits. (`with_derive_store`
remains on `zinder-explorer`'s query adapter, a distinct production-live
method outside this scope.)

### 4c: Empty scaffolding directory

Remove the empty, untracked
`crates/zinder-store/src/canonical_fact_store/` directory (decision 21). No
build or test impact; it is referenced by nothing.

## Phase 5: `canonical_*.rs` to `writer/`

All six files are confirmed live in production (each is imported and called
from `services/zinder-ingest/src/main.rs`, not merely declared and
re-exported):

| Before | After | Confirmed live via |
| --- | --- | --- |
| `canonical_construction.rs` | `writer/construction.rs` | called from `canonical_runtime.rs`'s orchestration |
| `canonical_control.rs` | `writer/control.rs` | `main.rs` calls `canonical_control_channel()` |
| `canonical_follow.rs` | `writer/follow.rs` | `main.rs` imports `CanonicalFollowConfig` |
| `canonical_ingest_control.rs` | `writer/ingest_control.rs` (after phase 4) | `main.rs` constructs `CanonicalIngestControlGrpcAdapter` |
| `canonical_runtime.rs` | `writer/mod.rs` | `main.rs` calls `run_canonical_runtime_with_control` |
| `canonical_replay_verification.rs` | `replay_verification.rs` (crate-root sibling, not nested — see decision 8) | `main.rs`'s CLI dispatch calls `run_canonical_replay_verification` |

The corresponding submodule directory `canonical_construction/` (currently
holding `abort_on_drop.rs`, `source_fetch.rs`, `watermark.rs` with no `//!`
doc comments on any of the three) moves to `writer/construction/` and should
gain doc comments as part of the same change, since the move already touches
every import referencing it.

`run_canonical_runtime_with_control` and any other `canonical_runtime`-named
public function should rename its `runtime` segment to `writer` for the same
collision reason as the file (for example
`run_canonical_writer_with_control`); grep
`services/zinder-ingest/src/canonical_runtime.rs`'s current public export
list in `lib.rs` for the complete set before renaming so no caller is missed.

Validation: `cargo check --workspace --all-targets --all-features`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo nextest run --profile=ci`, and `git grep -n canonical_runtime` /
`git grep -n canonical_construction\.rs` etc. against `services/`, `docs/`,
and `.github/` to catch any doc or CI reference to the old paths (this
service's `main.rs` is 2,271 lines per `git diff main...HEAD --stat`, so a
manual read is not a substitute for the grep).

## Phase 6: `zinder-client` doc clarification

Add one sentence to `crates/zinder-client/src/lib.rs`'s top doc comment
stating the boundary as a standing fact: this crate is the external consumer
SDK, and Zinder's own services do not use it for service-to-service calls
(those go through each service's own authenticated channel setup). The
comment must stand alone: it does not reference the simplification audit or
any plan document, because plan documents expire at merge while the doc
comment persists. Once [the simplification
audit](fact-first-simplification-audit.md) §2.3's `LazyAuthenticatedClient`
extraction lands in `zinder-runtime`, the comment may name that type as the
internal counterpart.

## Phase 7: Control-vocabulary disambiguation

| Before | After |
| --- | --- |
| `WalletStoreControl` (`crates/zinder-wallet-projection/src/control.rs`) | `WalletStoreControlRecord` |
| `services/zinder-projector/src/bin/zinder-projector/canonical_control.rs` | `services/zinder-projector/src/bin/zinder-projector/canonical_lease_client.rs` |
| `CanonicalControlClient` (type inside the renamed file) | unchanged — it is the generated gRPC client type; only the file holding it renames |
| `projector_control.rs` / `ProjectorControlCommand` | unchanged — already correctly suffixed |

Update every `WalletStoreControl` reference across
`crates/zinder-wallet-projection`, `crates/zinder-wallet-rocksdb`, and
`services/zinder-query`/`services/zinder-compat-lightwalletd` where it is
constructed or matched. Confirm no `RUSTDOCFLAGS`/doc-test or config-schema
table references the old name by string before merging.

Validation: `cargo check --workspace --all-targets --all-features`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`git grep -n WalletStoreControl\b` (word-boundary, so
`WalletStoreControlRecord` itself does not false-positive) returning no
hits outside the renamed identity, and `git grep -n canonical_control` under
`services/zinder-projector/` returning no hits.

## Phase 8: `bulk_catchup/` doc comments

Add a `//!` doc comment to `services/zinder-ingest/src/bulk_catchup/mod.rs`,
`block_prepare.rs`, `commit_reassembly.rs`, and `flush.rs`, matching the
density and style already used in the crate's `mempool/` and `cli/`
subdirectories. No rename, no move, no behavior change.

Validation: `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`.

## Phase 9: `FactFirst*` retirement

Runs only after phase 4 (decision 16's gate: the prefix does real
disambiguation until the legacy counterparts are deleted) and after or with
phase 5 (both rewrite imports in the same `zinder-ingest` files).

Code renames:

| Before | After |
| --- | --- |
| `services/zinder-query/src/fact_first_pair.rs` | `read_pair.rs` |
| `services/zinder-query/src/fact_first.rs` | `pair_serving.rs` |
| `FactFirstReadPair` | `ExactReadPair` (adopts `service-boundaries.md`'s established "request-scoped exact pair" vocabulary) |
| `FactFirstCanonicalRead` (trait) | `PairCanonicalRead` |
| `FactFirstWalletRead` (trait) | `PairWalletRead` |
| `FactFirstPairAdmissionError` | `ReadPairAdmissionError` |
| `FactFirstWalletQuery` | `ExactPairWalletQuery` |
| `FactFirstMempoolOwner` | `LiveMempoolOwner` (matches its own file `mempool/live_owner.rs` and the established "live mempool" vocabulary) |
| `FactFirstMempoolSnapshotPage` (`pub(crate)`) | `LiveMempoolSnapshotPage` — not the bare `MempoolSnapshotPage`, which already names a private type in `services/zinder-explorer/src/grpc/mempool.rs`; the different crates would compile, but a workspace grep for the name would return two unrelated types |
| `run_fact_first_mempool_owner` | `run_live_mempool_owner` |
| `run_fact_first_mempool_retention` | `run_mempool_retention` (unambiguous once phase 4 deletes the legacy `retention.rs`) |

Consumers to update: `services/zinder-compat-lightwalletd` (`frozen_pair.rs`,
`grpc.rs`, its binary `main.rs`), `services/zinder-ingest` (`main.rs` and the
phase-5 `writer/` files), `services/zinder-bench` (`report.rs`,
`canonical_fact_round_trip/command.rs`), and `services/zinder-query`'s own
`lib.rs` re-exports.

Steady-state prose in the same phase (decision 17): replace load-bearing
"fact-first" phrasing in `README.md`, `CLAUDE.md`, `service-boundaries.md`,
`storage-backend.md`, `service-operations.md`, `protocol-boundary.md`,
`public-interfaces.md`, `wallet-data-plane.md`, `explorer-plane.md`,
`chain-ingestion.md`, and the runbooks (`initial-sync.md`, `testing.md`,
`deploying-on-a-vm.md`, `deploying-on-railway.md`,
`explorer-only-deployment.md`) with canonical/projection or version-1
vocabulary, keeping at most a citation-style mention where a sentence points
the reader at ADR-0035. Excluded: ADR-0035 itself, every file under
`docs/plans/` and `docs/investigations/`, and `fact-first-indexer.md`
(deferred to the editorial pass per decision 17).

The noun "fact" alone is untouched everywhere: `CanonicalBlockFacts`,
`TransactionPublicFacts`, `canonical_fact_round_trip`, and prose about "fact
rows" or "fact stores" name durable domain content, not the migration.

The branch name `codex/fact-first-production-candidate` is ephemeral and out
of scope.

Validation: `cargo check --workspace --all-targets --all-features`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo nextest run --profile=ci`, and `git grep -in "fact.first"` across
`crates/`, `services/`, `README.md`, `CLAUDE.md`, `docs/architecture/`, and
`docs/runbooks/` returning only the citation-style ADR-0035 mentions decision
17 permits.

## Sequencing

The execution order follows one axis: subtract first, reshape second,
relabel last. Deletions shrink every later diff and are the cheapest
reviews; intra-crate restructuring batches into one train so its
merge-conflict cost is paid once; crate-identity renames are mechanical but
workspace-wide, so each lands atomically in a quiet window; and the one
gated phase goes last because its gate may convert it into a deletion. The
order is neither top-down nor bottom-up through the crate graph — it is
ordered by churn, not by layering.

The wallet-serving cutover remains the critical path. Only wave 0 belongs
on the production-candidate branch (it improves that branch's own review);
waves 1 through 3 are follow-up branches after it merges.

### Wave 0 — with the current branch

- **Phase 1 (doc-accuracy fixes).** Zero code risk; corrects the exact
  pages every later reviewer reads to orient, so it pays for itself across
  all subsequent waves.
- **Phase 4c (empty scaffolding directory).** Trivial local removal.

### Wave 1 — deletion (first follow-up)

- **Phase 4a (audit ingest cluster)** and **phase 4b (projection-reader
  surface)**, as one or two pure-removal changes. Highest value per
  decision 19, no naming judgment for reviewers to weigh, and every later
  wave's diff shrinks. 4a unblocks phases 5 and 9; 4b unblocks phase 3.

### Wave 2 — intra-crate restructuring (one train)

- **Phase 5 (`writer/`)** then **phase 9 (`FactFirst*` retirement)**
  back-to-back: both rewrite imports in the same `zinder-ingest` files, so
  consecutive landing pays the conflict cost once. **Phase 7
  (control vocabulary)** and **phase 8 (`bulk_catchup/` doc comments)** ride
  along in this wave; **phase 6 (`zinder-client` doc sentence)** attaches to
  whichever wave is convenient.

### Wave 3 — crate-identity renames (quiet windows)

- **Phase 2 (`zinder-bulk-load`)**: one atomic commit when no other branch
  is touching `zinder-store`/`zinder-wallet-rocksdb`. Mechanical; the cost
  is merge conflicts with in-flight work, not the diff itself.
- **Phase 3 (`zinder-materialized-views`, or its deletion)**: last by
  design. It waits on the explorer-projection-migration direction (decision
  18), and that direction may convert it to a deletion (decision 19) — so
  deferring it means executing the cheapest version of it.

### Outside this plan's window

- **Terminal target (decision 21).** `chain_store.rs`'s deletion fires with
  the explorer cutover; it is recorded here so `zinder-store`'s two-engine
  ambiguity has a named endpoint.

Every wave ends with the full [default validation
gate](../../CLAUDE.md#default-validation-gate) once, amortized across the
wave's changes, plus the per-phase `git grep` sweeps.

Every phase ends with the [default validation gate](../../CLAUDE.md#default-validation-gate)
plus a `git grep` sweep for the old identifier across `docs/`, `.github/`,
and any deployment manifests, since crate and module names leak into CI job
names, Dockerfiles, and prose that `cargo check` cannot see.
