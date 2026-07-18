# Plan: Fact-First implementation simplification audit

Status: Proposed

Scope: source under `crates/` and `services/` on `codex/fact-first-production-candidate` versus `main` (130 files, approximately 44,000 changed lines): the canonical store, wallet-rocksdb, wallet-projection, `zinder-ingest`, the new `zinder-projector` service, `zinder-query`, and `zinder-compat-lightwalletd`. Deploy manifests, docs, and scripts are out of scope.

Architecture authority: [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md), [Service boundaries](../architecture/service-boundaries.md), [ADR-0014](../adrs/0014-shared-configuration-sections.md).

This audit identifies simplification opportunities in the fact-first rewrite. It does not propose changing the service split, the two-topology storage model, or any other decision ADR-0035 already settled; every finding below operates inside those boundaries. None of it blocks the critical path in [Fact-first wallet-serving cutover](fact-first-wallet-serving-cutover.md) — it is hygiene work that can proceed alongside or after that plan's phases.

## How this was produced

Eleven independent reviews, each scoped to a coherent subsystem, applied four lenses (reuse, simplification, efficiency, altitude) tagged by three perspectives (operator, developer, architecture). Reviewers were instructed not to re-litigate settled architecture: the three-service split, the two-topology storage model, and the project's policy of deleting legacy code outright rather than keeping compatibility shims. The headline finding (§1.1) was independently verified against the working tree with direct `grep`/`wc` checks rather than taken on a reviewer's word.

## 1. Dead code ready for deletion

No design work is required for this section; every item is either unreferenced from any production entry point or a no-op.

### 1.1 Orphaned legacy derive-plane ingest subsystem (~16,600 lines)

`services/zinder-ingest/src/main.rs` now wires only `run_canonical_runtime_with_control`, `run_fact_first_mempool_owner`, `run_fact_first_mempool_retention`, and the two `Canonical*` gRPC adapters. Sixteen modules that predate this cutover are still declared in `lib.rs`, still compiled into the binary, and still `pub use`-exported, but have zero callers outside their own file and `lib.rs`'s re-export (verified by `grep` across the whole workspace):

| File | Lines | Notes |
| --- | ---: | --- |
| `services/zinder-ingest/src/ingest_loop.rs` | 1,059 | Superseded by `canonical_runtime.rs` |
| `services/zinder-ingest/src/ingest_control.rs` | 885 | Superseded by `canonical_ingest_control.rs` |
| `services/zinder-ingest/src/projection_startup.rs` | ~900 | Superseded by `zinder-projector` |
| `services/zinder-ingest/src/retention.rs` | ~500 | Superseded by fact-first retention inside `canonical_follow.rs`/`canonical_control.rs` |
| `services/zinder-ingest/src/derive_consumers.rs` | ~1,000 | Legacy `zinder-derive` catch-up |
| `services/zinder-ingest/src/derive_status_reader.rs` | ~300 | Legacy `zinder-derive` status |
| `services/zinder-ingest/src/tip_follow.rs` | ~600 | Superseded by `canonical_follow.rs` (note: the `tip_follow` *config field* on `main.rs`'s loop config is unrelated and still live) |
| `services/zinder-ingest/src/mempool/orchestrator.rs` | ~700 | Superseded by `mempool/live_owner.rs`; already behaviorally diverged (see below) |
| 8 backfill/verifier modules (`commitment_root_backfill.rs`, `conventional_fee_distribution_backfill.rs`, `paid_fee_distribution_backfill.rs`, `transaction_component_backfill.rs`, `transaction_history_verifier.rs`, `value_pool_balance_backfill.rs`, `value_pool_flow_backfill.rs`, `block_production_time_backfill.rs`) | ~9,200 | Legacy `zinder-derive` backfills; reachable only through `derive_consumers.rs`'s `HistoricalWorkGate`, itself unreferenced |

Two dedicated integration test files exercise only this dead cluster and can be deleted with it: `services/zinder-ingest/tests/integration/ingest_loop.rs` (396 lines) and `services/zinder-ingest/tests/integration/projection_startup.rs` (545 lines). Total: approximately 15,169 lines of production source plus 941 lines of tests.

`services/zinder-ingest/src/chain_ingest.rs`, `source_recovery.rs`, and `bulk_catchup/mod.rs` are **not** part of this cluster despite superficially similar names: `chain_ingest.rs` supplies `IngestError`/retry primitives still imported by the live `canonical_construction/*` and `bulk_catchup/*` modules; `source_recovery.rs` is imported directly by the live `canonical_follow.rs`; `bulk_catchup/mod.rs` is still the shared fast-fixture builder used by `zinder-explorer`, `zinder-ingest` live tests, and `zinder-bench`.

`mempool/orchestrator.rs`'s `apply_to_index` has already drifted from its replacement in `mempool/live_owner.rs`: the dead copy's `would_be_noop` treats `MempoolEvent::Suppressed` as a no-op the index never sees, while the live copy explicitly applies it. This is evidence for deleting rather than reconciling — nothing exercises the dead copy against production behavior, so it will keep drifting.

ADR-0035 step 9 and the plan's own [Cutover and Deletion Policy](fact-first-wallet-serving-cutover.md#cutover-and-deletion-policy) already mandate deleting "obsolete derive consumers" once wallet construction and following own production traffic; this section identifies that the deletion has not yet been executed, not a new architectural decision.

### 1.2 Dead configuration surface (`services/zinder-ingest/src/config.rs`)

Because §1.1's backfill modules are unreachable, roughly 500 lines of config plumbing that exists only to configure them is equally dead: 12 default constants (lines 93-109), 7 `.with_default(...)` registrations (320-387), full validation/construction of the 7 backfill config structs (1002-1104), their `RedactedXToml` mirrors (1344-1386, 1487-1545), and ~115 lines of tests asserting these dead knobs reject invalid input (1663-1777). An operator running `--print-config` today sees 7 fully-populated TOML sections (for example `[ingest.paid_fee_distribution_backfill]`) describing work the binary can never perform.

Separately, `ingest.projection_preset` accepts `"explorer"` at both the CLI (`main.rs:83-84`) and config layer (`config.rs:750-758`), but `run_ingest` (`main.rs:259-261`) unconditionally rejects anything but `Wallet`. This is an accept-then-reject surface, not a real operator choice.

### 1.3 Smaller dead-code items

| File | Item | Fix |
| --- | --- | --- |
| `crates/zinder-wallet-projection/src/contract_error.rs:82-87` | `UnsupportedProjectionAccumulatorVersion` variant never constructed or matched anywhere | Delete |
| `crates/zinder-wallet-projection/src/contract_error.rs:101-106,179-184` | `UnsupportedWalletProjectionEventCursorVersion` and `UnsupportedProjectionBuildLeaseVersion` add no information over the existing generic `UnsupportedEncodedValue{field, encoded}` | Replace both call sites with `UnsupportedEncodedValue`; delete both variants |
| `crates/zinder-wallet-projection/src/control.rs:870-876` | `validate_build_lease`'s inner `if` has identical `Ok(())` in both arms — dead branch | Collapse to `let Some(lease) = control.build_lease else { return Ok(()); };` |
| `crates/zinder-wallet-projection/src/control.rs:586-587` | `WalletStoreControl::decode` calls `validate_build_lease` explicitly, then `encode()` (line 587) which already re-validates it; READY records additionally re-validate ready-evidence a second time inside `encode_ready_evidence` | Drop the explicit pre-check; rely on `encode()`'s own validation |
| `services/zinder-query/src/fact_first.rs` (11 call sites: 143, 245, 254, 309, 348, 359, 370, 498, 596, 607, 617) | 11 `WalletQueryApi` stub methods bind `let _pair = self.capture_pair();` — an `ArcSwap::load_full()` — and never use it | Delete the unused captures |
| `crates/zinder-runtime/src/sections/defaults.rs:53` | `DEFAULT_PROJECTOR_CONTROL_LISTEN_ADDR` declared, never referenced anywhere in the workspace | Delete, or wire it in if an opt-in default was actually intended |

## 2. Cross-cutting patterns

Each of these appears independently in multiple files; fixing the underlying gap once removes every instance rather than patching them one at a time.

### 2.1 Missing request/context struct behind `too_many_arguments` allows

`clippy.toml` caps arguments at 5. Several functions bypass this with `#[allow(clippy::too_many_arguments, reason = "...")]` where the reason describes a shape the code chose, not one it was forced into — the sibling canonical store already solved the same problem by bundling inputs into a request struct (`CanonicalLiveAppend`, `crates/zinder-store/src/canonical_store/live_commit.rs:45-71`), which needs no allow.

| File | Function | Args |
| --- | --- | --- |
| `crates/zinder-store/src/canonical_store/secondary.rs:76-87` | `RocksDbCanonicalSecondary::open_ready` | 6 (three already get folded into `CanonicalStoreAdmissionExpectation` one line into the body) |
| `crates/zinder-wallet-rocksdb/src/transition.rs:60-64,92-97,191-195,226-231` and `store.rs:1204-1300` (4 forwarding wrappers) | `insert_unspent`/family apply/reconcile entry points | 7-8, repeated 8 times across the two files |
| `services/zinder-projector/src/state_bundle.rs:870-873` | `assemble_canonical_admission` | own allow admits "the cross-process admission assembler keeps every independently decoded identity explicit" |
| `services/zinder-projector/src/bin/zinder-projector/main.rs:737-772,774-842` | `handle_projector_control_command`, `capture_state_bundle` | 6-7, for a single-variant `ProjectorControlCommand` match |

Fix direction: introduce request structs mirroring `CanonicalLiveAppend` for each cluster (a `WalletCanonicalEventApplication`/`WalletCanonicalReconciliation` pair for wallet-rocksdb, a `FollowingOwnerContext` for the projector's control-command handlers), and pass a pre-built `CanonicalStoreAdmissionExpectation` into `open_ready` instead of six positional arguments.

### 2.2 Filesystem-safety helpers reimplemented 4 times

`require_absent`/symlink-attack checks appear independently in `crates/zinder-store/src/canonical_store/rocksdb.rs:458`, `crates/zinder-wallet-rocksdb/src/store.rs:1349`, and — byte-identically to each other — in `services/zinder-projector/src/recovery_archive.rs:859-1095` and `services/zinder-projector/src/state_bundle.rs:1662-2000` (eight functions duplicated verbatim between those last two: `resolve_existing_directory`, `validate_absolute_lexical_path`, `require_directory`, `require_regular_file`, `require_absent`, `validate_lower_hex_32`, `sha256_hex`, `parse_network`, differing only in which per-file error enum they wrap). `recovery_archive.rs` also has its own `read_bounded_regular_file` that `state_bundle.rs` reimplements inline instead of calling.

Fix direction: one `fs_admission` module (generic over an error-constructor closure) inside `zinder-projector`, shared by `recovery_archive.rs` and `state_bundle.rs`; a separate, smaller shared helper (perhaps in `zinder-core`) for the `require_absent`/path-confinement idiom used by the two storage crates.

### 2.3 Lazy-authenticated-gRPC-client pattern reimplemented 4 times, diverging in the projector

`services/zinder-explorer/src/grpc/adapter.rs:1572-1587`, `services/zinder-query/src/grpc/adapter.rs:883-900`, and `services/zinder-compat-lightwalletd/src/mempool/ingest_control.rs:95-111` each independently hand-roll `Arc<OnceCell<AuthenticatedChannel>>` plus `get_or_try_init(|| connect_zinder_grpc(...))`: lazy first-use connect, cheap-clone reuse thereafter. `services/zinder-projector/src/bin/zinder-projector/canonical_control.rs:106-125`'s `CanonicalRetentionLeaseClient::connect` is a fourth reimplementation that instead connects eagerly at process startup and holds the client for the process lifetime — so unlike its three siblings, the projector fails to start outright if `zinder-ingest`'s control listener isn't up yet (a real risk on a fresh multi-service deploy or restart race). `best_effort_release_canonical_lease` (`main.rs:1367-1393`) then dials a second fresh eager connection on its failure path.

Fix direction: extract the `OnceCell<AuthenticatedChannel>` + `get_or_try_init` skeleton into `zinder-runtime` (for example `LazyAuthenticatedClient<C>`), and have the projector adopt lazy connect for parity with the other three services. The per-RPC business logic stays where it is.

### 2.4 The canonical store's binary codec primitives are reimplemented across most of its files

This is the largest single cross-cutting cluster in the audit, spanning nearly every file in `crates/zinder-store/src/canonical_store/`.

- **Two full stateful byte-cursor types.** `control.rs:368-595` (`Decoder`) and `displaced_archive.rs:1252-1361` (`ArchiveDecoder`) are independently-written types with the same method set — `read_u8`/`read_u32`/`read_u64`/`read_array::<N>`/`read_bytes`, each offset-tracked and bounds-checked — differing only in endianness and which domain error a truncation maps to.
- **The same bounds-checked `read_array<const N>` free function, 6 times.** `reader.rs:450-458`, `publication.rs:1592-1600`, `control.rs:559-566`, `displaced_archive.rs:1286-1290` and `:1363-1372`, and `block_load/codec.rs:324-332` each reimplement it, and have already drifted: `publication.rs` uses `offset + N` (panics on overflow) while `reader.rs` uses `offset.saturating_add(N)`.
- **The 93-byte `chain_epoch` row encoded once, decoded independently twice** — internally in `publication.rs:1454-1476` and again in `reader.rs:356-381` — with `reader.rs:28` hardcoding the row length (`93`) as a bare literal disconnected from `publication.rs:57`'s derived sum.
- **The same fixed-offset field-read pattern inlined roughly ten times** in `event_lifecycle.rs` (`validate_event_record_shape`/`validate_event_range` at 704-785, `encode_projection_build_lease`/`decode_projection_build_lease`/`decode_projection_build_lease_generation` at 1000-1092): `<[u8; N]>::try_from(&encoded[a..b]).map_err(...)` repeated with a hand-copied offset range at each site.

Fix direction: one shared `ByteCursor`/`FieldDecoder` type (offset-tracked, bounds-checked, call-site-specific error mapping) replacing both `Decoder` and `ArchiveDecoder` and the six standalone `read_array` copies; a small local `read_be_u64(encoded, range, on_error)` helper to collapse `event_lifecycle.rs`'s inline boilerplate to one line per field; one shared encode+decode pair for the `chain_epoch` row layout.

### 2.5 `checked_add`-with-context boilerplate, a dozen-plus copies

The two-line idiom `a.checked_add(b).ok_or_else(|| Error::variant("<domain> exceeds u64::MAX"))` repeats across `block_load.rs` (four variants: row bytes, running total, SST bytes, logical bytes), `block_replay.rs`, `subtree_load.rs`, and `bulk_load.rs`, varying only in the error-domain string.

Fix direction: one generic `checked_add_u64(a, b, context) -> Result<u64, CanonicalStoreError>`.

### 2.6 Hex encode/decode reimplemented 3 times in `zinder-projector` despite depending on `hex`

`main.rs:1893-1900` (`display_digest`) and `config.rs:706-713` (`encode_hex`) both hand-write a `write!(encoded, "{byte:02x}")` loop; `config.rs:511-531` (`parse_build_owner`) hand-writes fixed-length hex decoding. `Cargo.toml:16` already declares `hex.workspace = true`, and `state_bundle.rs`/`recovery_archive.rs` in the same crate call `hex::encode`/`hex::decode_to_slice` more than 15 times for the identical purpose, including the exact "decode into a fixed-size array with a length check" shape `parse_build_owner` reimplements.

Fix direction: replace all three with direct `hex::encode`/`hex::decode_to_slice` calls, matching the rest of the crate.

### 2.7 Storage-path-disjointness check duplicated verbatim across two service configs

`services/zinder-projector/src/bin/zinder-projector/config.rs:453-509` and `services/zinder-compat-lightwalletd/src/bin/zinder-compat-lightwalletd/config.rs:295-357` both implement `normalized_storage_path_identity` and a prefix-overlap disjointness loop — same algorithm, same `Component` matching, same three-way `starts_with` check — added in sibling commits on this branch. Everything else in both files correctly reuses `zinder_runtime` config helpers; this check is the exception in both.

Fix direction: move `normalized_storage_path_identity` and a `require_disjoint_storage_roots(&[&Path], message)` into `zinder-runtime`, next to the other shared config helpers both files already import.

### 2.8 `shielded_protocols()` duplicated 3 times, protocol-tag maps duplicated twice

The private `const fn shielded_protocols() -> [ShieldedProtocol; 3]` is defined identically in `block_load/codec.rs:239-245`, `subtree_load.rs:486-492`, and `construction_manifest.rs:1162-1168`; a second concept (protocol-tag mapping) is separately duplicated between `codec.rs` and `construction_manifest.rs` with different tag schemes.

Fix direction: add `ShieldedProtocol::ALL` (or `iter_all()`) to `zinder-core` and delete the three local copies.

### 2.9 Cancellable-sleep helper duplicated

`mempool/live_owner.rs:826-831` (`wait_or_cancel`, `tokio::select! { cancel.cancelled() => true, sleep(duration) => false }`) is the same pattern inlined in `canonical_follow.rs:124-127`. Every service that runs a cancellable retry/backoff loop (ingest, projector, compat-lightwalletd) is a plausible future third copy.

Fix direction: promote one `async fn wait_or_cancel(cancel: &CancellationToken, duration: Duration) -> bool` into `zinder-runtime`.

### 2.10 `ServiceIdentifier` has no `Projector` variant — root cause of two config-drift findings

`crates/zinder-runtime/src/sections/service.rs`'s `ServiceIdentifier` doc comment states that adding a variant "forces a compile error in every section's default table so nothing silently defaults to a placeholder" — the exact mechanism [ADR-0014](../adrs/0014-shared-configuration-sections.md) introduced after a prior port-drift incident. `zinder-projector` shipped without a variant, so its `config.rs:27` hardcodes its own `DEFAULT_OPS_LISTEN_ADDR` and bypasses `ConfigLoader::with_ops_section` entirely, and `env_var_docs.rs:282` has to hand-maintain the "9110 projector" string in prose because `defaults.rs` cannot express it.

The same gap shows up a second way: the canonical-checkpoint capability token and staging root are configured through two independently-named fields — `ingest_control.checkpoint_bearer_token_path`/`checkpoint_staging_root` versus `projector_control.bearer_token_path`/`checkpoint_staging_root` — that must be kept in sync by the operator and are validated only at RPC time (the SHA-256 root-binding check inside `CreateOwnerCheckpoint`), not at config load. Both currently default to the same literal by coincidence, not by shared schema.

Fix direction: add `ServiceIdentifier::Projector` and wire its ops/gRPC defaults through the shared table; fold the checkpoint token and staging-root fields into one name reused by both sections, or validate the binding at config-load time.

### 2.11 `CanonicalControl` duplicates `IngestControl`'s writer-status and event-history surface

`crates/zinder-proto/proto/zinder/v1/ingest/ingest.proto`'s new `CanonicalControl` service (around line 355) stands up a second `WriterStatus`-shaped RPC returning a differently-shaped `CanonicalWriterFence`, and a second bounded event-page accessor (`EventPage`), alongside the pre-existing `IngestControl.WriterStatus`/`ChainEvents` that already serve largely the same projector/compatibility consumers from the same process. Two parallel "what is the writer's authenticated state" contracts now have to stay semantically consistent (retention floor, cursor format, ordering) indefinitely.

Fix direction: extend `IngestControl` with the new lease/checkpoint RPCs and a page-based event accessor instead of a second service with its own fence and event-history shapes; confirm whether `CanonicalWriterFence` can fold into the existing `ChainView` envelope.

### 2.12 Compact-commitment extraction duplicated between `live_commit.rs` and `live_replacement.rs`

`live_replacement.rs:568-619` (`append_compact_commitments`/`commitment_bytes`, new) duplicates the compact-block-to-commitment-array extraction already inlined in `live_commit.rs:335-419`'s `validate_live_checkpoint_transition`/`compact_commitment_bytes` (pre-existing, unchanged by this diff). Both decode a `CompactBlock` and build sapling/orchard/ironwood commitment-byte vectors via the same loop shape and a 32-byte-conversion helper, differing only in error-message text. A future shielded-pool addition has two call sites to update in lockstep.

Fix direction: extract one shared `append_compact_commitments(accumulator, height, payload)` helper (the free-function shape already used in `live_replacement.rs`) and call it from both files, deleting the inline loop in `validate_live_checkpoint_transition`.

### 2.13 Three near-identical "read a row from a named column family" helpers, with a `fill_cache` policy that has already drifted

`publication.rs:1696-1710` (`read_family_row`) and `displaced_archive.rs:1072-1085` (`read_exact_row`) both set `ReadOptions::fill_cache(false)` before `get_cf_opt`, appropriate for the cold/one-shot scans they serve. `live_replacement.rs:833-851`'s new `read_family_optional`/`read_family_required` instead call plain `db.get_cf` with no `ReadOptions` — so every displaced-block capture and deletion-readback read during a reorg (exactly the kind of one-shot read the other two helpers were tuned for) warms the RocksDB block cache with rows about to be archived or deleted.

Fix direction: consolidate into one column-family row-read primitive (`Option`-returning, with a `_required` wrapper), standardized on `fill_cache(false)` for these one-shot read paths, used by all three modules.

## 3. Files that have outgrown their responsibility

The canonical store crate (`crates/zinder-store/src/canonical_store/`) already establishes the pattern this codebase uses at scale: roughly 15 small, single-concern files rather than a few large ones. Several new or heavily-grown files break from that precedent.

| File | Lines | Concerns mixed |
| --- | ---: | --- |
| `crates/zinder-wallet-rocksdb/src/transition.rs` | 2,129 | Public apply/reconcile API + validation, byte accounting, and a ~900-line row-CRUD state machine in one `impl` block |
| `crates/zinder-wallet-rocksdb/src/store.rs` | 3,443 | BUILDING-lifecycle typestate chain, query-serving reads, and an independent ~780-line cold-validation oracle |
| `services/zinder-projector/src/state_bundle.rs` | 2,569 | Capture-path management, canonical/wallet checkpoint admission, the full manifest tree, and duplicated fs-safety helpers (§2.2); roughly 700-800 lines are paired "typed struct + serializable manifest struct" definitions connected by hand-written `from_admitted()`/`validate()` methods |
| `crates/zinder-store/src/canonical_store/mod.rs` | 1,191 | Roughly half the file is `CanonicalStoreBuildPlan`/`CanonicalReorgPolicy` (~250 lines) and `CanonicalStoreError`'s 30+ variants (~380 lines) — the two pieces of logic in this module that don't have their own file, unlike every sibling concern |
| `services/zinder-ingest/src/canonical_follow.rs:348-832` | — | `follow_canonical_tip_controlled` (166 lines) interleaves outage recovery, retention pruning, control-command draining, and append/replacement dispatch; `prepare_replacement_iteration` (188 lines, 8 arguments) fuses a backward common-ancestor search with a forward suffix-preparation loop — the first functions in this file to cross the complexity ceilings this diff introduces |
| `services/zinder-projector/src/bin/zinder-projector/main.rs:201-508` | — | `run_owned_projector` (304 lines, 3.8x the 80-line ceiling) interleaves bootstrap, resume-path dispatch, fresh-build orchestration, and post-build admission; a 67-line `heartbeat` closure buried at line 359 mutably captures and moves several owned values, making lease-renewal logic untestable independent of the whole function |
| `services/zinder-projector/src/bin/zinder-projector/config.rs:107-206` | — | `ProjectorError` mixes ~6 config-loading variants with 15 runtime-only variants (control-server lifecycle, fence convergence, event-cursor expiry) produced and consumed exclusively by `main.rs`/`projector_control.rs`; `zinder-ingest`'s own `config.rs` keeps its runtime errors in `canonical_runtime.rs` via `#[from]` instead |
| `crates/zinder-store/src/canonical_store/live_replacement.rs:198-361`, `displaced_archive.rs:87-200,504-606` | — | `PreparedLiveReplacement::new` (163 lines), `PreparedDisplacedArchiveWrite::new` (113 lines), and `validate_permanent_reorg_archive` (102 lines) each carry `#[expect(clippy::too_many_lines)]` at 1.3-2x the ceiling, each mixing several independent invariants (fence validation, deletion prep, archive prep, sequence-checkpoint math, ready-evidence construction) in one function body |

Fix direction: none of these require new design work — split each along the seams the struct/impl grouping already shows (for example `transition/{api,byte_accounting,planner}.rs`, `state_bundle/{manifest,canonical_admission,wallet_admission,capture}.rs`, `canonical_store/{build_plan,error}.rs`), and separate `canonical_follow.rs`'s and `main.rs`'s fused functions into the sub-steps they already delegate to internally.

## 4. Efficiency

| File | Issue | Cost |
| --- | --- | --- |
| `crates/zinder-store/src/canonical_store/reader.rs:191-228,306-340` | `read_compact_blocks_in_range`/`read_subtree_roots` issue one `get_cf` per element instead of a bounded iterator or `multi_get_cf`; `read_compact_block_at` issues a second `get_cf` per block just for a 32-byte hash | An N-block range read costs 2N sequential RocksDB round-trips on the wallet compact-block sync path |
| `crates/zinder-wallet-rocksdb/src/transition.rs:1401-1546,1710-1747` | `insert_unspent`/`remove_unspent`/`insert_spent`/`remove_spent`/`remove_address_transaction` each read a row to validate it, then `insert_row`/`remove_row` re-reads the same family+key via a fresh `raw()` call (the overlay isn't populated by reads, only writes) | 4 point-reads per spend where 2 suffice, on the hot catch-up/reconciliation path (up to 4,096 blocks per call) that directly affects the ADR-0035 wallet-projection-lag target |
| `services/zinder-projector/src/recovery_archive.rs:245-311` | `package_recovery_archive` hashes every byte during copy, then `collect_payload_files` re-reads and re-hashes the same bytes to verify the copy, then the caller's `admit_recovery_archive_outer` does a third full re-read-and-rehash to check against the manifest | Up to 3x the disk I/O and SHA-256 work on an operator-invoked path with a minutes-scale target, using a single-threaded 64 KiB buffer against files up to 64 GiB |
| `crates/zinder-store/src/canonical_store/construction_manifest.rs:1114-1160` | `canonical_construction_families()`/`canonical_staged_sst_families()` reconstruct a `BTreeSet` from 8-15 static strings on every call, at least twice per manifest validation | Redundant allocation on the path ADR-0035 targets for the 2-hour/3-hour lifecycle gate |
| `crates/zinder-wallet-projection/src/control.rs:586-587` | `WalletStoreControl::decode` explicitly validates the build lease, then `encode()` validates it again; READY records validate ready-evidence twice | Duplicated work, and a validation change applied to only one call site silently stops matching the other |
| `crates/zinder-store/src/canonical_store/live_replacement.rs:677-803` | `capture_displaced_blocks` reads raw bytes (`read_family_required`) and fully decodes the replay row (`read_persisted_replay`) for a displaced block; `PreparedCanonicalDeletions::new` then calls `read_persisted_replay` a second time on the same row instead of reusing the already-decoded facts it was passed | 3 row fetches and 2 full replay decodes per displaced block, scaling with `reorg_window_blocks`, on the path that determines writer recovery time from a reorg |

## 5. Suspected duplication verified NOT to be duplication

Recorded so these are not re-investigated:

- **`services/zinder-query/src/fact_first_pair.rs` vs `services/zinder-compat-lightwalletd/src/bin/zinder-compat-lightwalletd/frozen_pair.rs`**: a legitimate library/consumer split. `fact_first_pair.rs` owns the only admission-validation algorithm (`FactFirstReadPair::validate_readers`); `frozen_pair.rs` calls into it and owns orchestration the library has no business owning (RocksDB secondary lifecycle, gRPC writer-status polling, `ArcSwap` publication). Confirmed by tracing every call site; `fact_first.rs`'s diff shows the admission comparison was hand-rolled inline before this refactor extracted it once.
- **`commit_live_append` vs `commit_live_replacement`** (`live_commit.rs`/`live_replacement.rs`): the structural similarity is explicitly required by the Phase 1 plan text ("Keep `commit_live_replacement` as a parallel consuming public operation... Do not generalize the proven append operation into a compatibility enum or shared mutation adapter").
- **`services/zinder-ingest/src/canonical_control.rs` vs `services/zinder-projector/src/bin/zinder-projector/canonical_control.rs`**: same filename, unrelated responsibility — one is the `CanonicalControl` gRPC server, the other a client of it. Not duplicative (the real cross-service duplication involving the projector's client is §2.3).
- **`services/zinder-ingest/src/canonical_control.rs` vs `canonical_ingest_control.rs`**: back two distinct tonic services with non-overlapping RPC sets; the size increase over the deleted `backup.rs` is explained by genuinely new capability (owner-checkpoint capture, generation-fenced leases), not overlap between the two new files.
- **`live_replacement_tests.rs` copy-pasted test setup**: initially suspected given its line count is close to `live_replacement.rs`'s. On inspection its shared fixtures (`published_store`, `open_store`, `replacement_block`, `canonical_block`, `add_checkpoint`) are already well-factored and reused, not copy-pasted per test. The file's actual issue is narrower: two test functions (`maximum_depth_replacement_is_atomic_archived_and_reopenable_then_appends`, `retained_events_resume_exactly_and_leases_bound_pruning`) each bundle 5-6 unrelated behavioral assertions into one function, so a regression in one surfaces alongside unrelated assertions in a 150-240 line test. Splitting those two functions along their existing assertion boundaries is a minor, low-priority cleanup, not a fixture-extraction task.

## 6. Suggested sequencing

1. **Delete confirmed-dead code (§1).** Zero risk, no design decisions, roughly 16,600 lines of production source and tests removed, plus the dead config surface in §1.2. This alone materially reduces the review surface this audit had to cover.
2. **Extract the cross-cutting helpers (§2.1-2.13).** Bounded, mechanical, each closes an already-observed or likely drift risk (§2.4's bounds-checked reader and §1.1's `mempool/orchestrator.rs` copy of `apply_to_index` have already diverged from their live counterparts).
3. **Add `ServiceIdentifier::Projector` (§2.10).** Small and unblocks two config-drift findings; matches ADR-0014's own compile-time-safety intent for the newest service.
4. **Split the oversized files (§3).** No behavior change; do opportunistically alongside other work planned in each area rather than as a dedicated pass.
5. **Efficiency fixes (§4).** Worth measuring before/after given ADR-0035's explicit lifecycle-time gates. The `reader.rs` range-read fix and the wallet-transition double-read fix are the two most likely to move the needle on the healthy-wallet-projection-lag target.
