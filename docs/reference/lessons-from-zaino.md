# Lessons from Zaino

| Field    | Value                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status   | Background research                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Audience | Zinder maintainers, contributors, downstream developers                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Sources  | [zingolabs/zaino issues](https://github.com/zingolabs/zaino/issues) (open + closed, sampled through 2026-04-26), Zaino architecture commentary by maintainers                                                                                                                                                                                                                                                                                                                                                   |
| Related  | [PRD-0001](../prd-0001-zinder-indexer.md), [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md), [Service Boundaries](../architecture/service-boundaries.md), [Service Operations](../architecture/service-operations.md), [Node source boundary](../architecture/node-source-boundary.md), [Protocol boundary](../architecture/protocol-boundary.md), [ADR-0002](../adrs/0002-boundary-specific-serialization.md), [ADR-0004](../adrs/0004-node-source-and-protocol-boundaries.md) |

## Purpose

Zinder is a separate clean-slate indexer, not a fork of Zaino. It is not a judgment on Zaino and not a claim about Zaino's role in the ecosystem. Zaino is active ecosystem prior art; Zinder targets a different product shape: an epoch-consistent data plane for multiple consumers with explicit ingestion, query, compatibility, and derived-index boundaries.

This document is the audit trail. It captures:

1. The architectural pressure points Zaino's maintainers have publicly identified in tracker and design discussions.
2. The user requests and integration constraints that are relevant to Zinder's scope.
3. How Zinder's current decisions choose different product and architecture tradeoffs, and where new decisions are still owed.
4. The open seams Zinder must preserve so the next round of feature requests does not require a rewrite.

This is a research input, not a change-log. When a Zinder decision is referenced (PRD, RFC, ADR), assume that document is authoritative and this one is the rationale.

## Method

Issues were sampled from `zingolabs/zaino` (300 most recent, both open and closed). High-signal threads were read in full: tracking issues, "architecture" labelled issues, "Top Priority" labels, perf benchmarks, and issues whose bodies discussed architecture follow-up work. Each finding cites the issue number so future readers can pull the original context.

The clusters below are not exhaustive; they are the patterns that recurred at least three times under different surface symptoms. The recurrence test matters: a single bug can be a local implementation issue, while repeated stress against the same boundary is product and architecture input for Zinder.

## Pattern 1: Three Domains, One Type Pile

### What the tracker says

Zaino's clearest self-diagnosis is [#1029](https://github.com/zingolabs/zaino/issues/1029): _"there are three fundamental domains: wire, internal, and persistence whose concerns must be encapsulated. Currently types are constrained by the considerations of all three domains, this causes clashes in definitions and use. Changing an internal type might force a schema rewrite."_

The same root cause surfaces as:

- [#515](https://github.com/zingolabs/zaino/issues/515): public configuration exposes `zebra_state::Config` directly, so a Zebra internal change becomes a Zaino API break.
- [#717](https://github.com/zingolabs/zaino/issues/717): `ZcashIndexer` and `LightWalletIndexer` traits require `tonic::Status` bounds in places that have nothing to do with gRPC.
- [#538](https://github.com/zingolabs/zaino/issues/538): a custom block parser (`ParseFromSlice`) duplicates consensus-critical logic from Zebra and mixes parsing with RPC enrichment.
- [#631](https://github.com/zingolabs/zaino/issues/631): `StateService` and `FetchService` are not really services, they are partial backends pretending to share an interface.
- [#716](https://github.com/zingolabs/zaino/issues/716): `StateService` silently delegates to `FetchService` when the read state cannot answer; the fall-through is hard-coded per endpoint, untestable, and impossible to disable.
- [#964](https://github.com/zingolabs/zaino/issues/964): `ChainIndex` cannot serve full block data because the type chosen for the non-finalised path was never extended to the finalised path.

### Design pressure

Zaino's types were authored against the first consumer that needed them. When the second consumer arrived, the type either had to grow extra fields or the consumer had to construct a near-duplicate. Over time:

- Wire types leaked into business logic (because the proto type was already defined).
- Upstream-node types leaked into public configuration (because Zebra had a config struct).
- Persistence types leaked into RPC responses (because the LMDB record was already shaped right).

The cost compounds at every protocol upgrade and every Zebra release.

### Zinder design response

- [PRD-0001](../prd-0001-zinder-indexer.md): public API names must be domain-first (`ChainEpoch`, `BlockArtifact`, `WalletQueryApi`), and forbidden words include `Service`, `Manager`, `common`, `shared`, `utils`.
- [ADR-0002](../adrs/0002-boundary-specific-serialization.md): three serialization formats are reserved for three boundaries (fixed for ordered keys and envelopes, protobuf for protocol payloads, postcard for non-durable internal binary). The format split makes the type split self-enforcing.
- [Public interfaces](../architecture/public-interfaces.md): `zinder-core` types are the canonical domain vocabulary; `zinder-store` owns persistence; `zinder-proto` owns wire. Cross-crate type leakage is reviewed at PR time.
- [Service Boundaries](../architecture/service-boundaries.md): allowed coupling between services is enumerated (domain types, storage contracts, protocol definitions, test fixtures). Anything else is a boundary violation.
- [ADR-0004](../adrs/0004-node-source-and-protocol-boundaries.md): `NodeSource` and `zinder-proto` are separate boundaries. Source adapters normalize upstream node data; protocol modules own wire schemas.

### Seams to preserve

- `zinder-source` is the only crate allowed to depend on `zebra-*` and `zcashd-*` types directly. Upstream-node type leakage is treated as a build-time error in `zinder-core`, `zinder-store`, and `zinder-proto`.
- A new artifact (e.g. a transparent-outpoint index) requires three explicit decisions: domain shape (core), durable layout (store), wire shape (proto). Reviewers should ask for explicit modeling instead of deriving one shape from another.

## Pattern 2: RPC Surface With No Single Source of Truth

### What the tracker says

- [#907](https://github.com/zingolabs/zaino/issues/907): _"There's more than one source of truth for the RPC spec."_
- [#1026](https://github.com/zingolabs/zaino/issues/1026): no machine-readable description of the RPC surface; an earlier port (#410) went stale and was closed.
- [#802](https://github.com/zingolabs/zaino/issues/802) and [#804](https://github.com/zingolabs/zaino/issues/804): the plan to introduce a unified `IndexerRpcService` and transition away from direct `CompactTxStreamer` exposure is a live tracking effort created on 2026-01-19. Wallets, explorers, and operators today still need to learn two overlapping specs (JSON-RPC + gRPC `CompactTxStreamer`) with subtle method-name and semantic differences.
- [#717](https://github.com/zingolabs/zaino/issues/717): the trait that should have been the spec is bound to `tonic::Status`, which means the spec is not portable.

### Design pressure

The `CompactTxStreamer` proto came from `librustzcash`, the JSON-RPC spec came from `zcashd`, and Zaino had to keep both. There was no Zaino-owned canonical document, so:

- Wallet maintainers reverse-engineer behaviour from running servers.
- Subtle differences between the two specs hide in code, not docs.
- A method like `get_mempool_stream` cannot evolve to take an `expected_chain_tip_hash` without renegotiating the entire dual-spec contract.
- Adapter generation is impossible.

### Zinder design response

- [Public interfaces](../architecture/public-interfaces.md) names `WalletQueryApi` as the single externally visible read contract.
- [PRD-0001](../prd-0001-zinder-indexer.md): _"Compatibility tests must compare wallet-facing behavior against lightwalletd-compatible expectations for compact blocks, block ranges, latest block metadata, and transaction submission."_
- [ADR-0002](../adrs/0002-boundary-specific-serialization.md): protobuf is the canonical wire serialization, which means the `.proto` files are the spec, not a derived artifact.

### Zinder decisions

- Pick one canonical spec carrier. Because Zinder's wire is gRPC, the `.proto` files plus structured Markdown method docs in `zinder-proto` are the source of truth. OpenRPC (referenced in Zaino #1026) is a JSON-RPC-shaped format and would force a second spec carrier; defer it unless and until Zinder offers a JSON-RPC surface, in which case the proto remains primary and OpenRPC is generated.
- Compatibility test harness against `lightwalletd` and Zaino must run on every PR, not just on release branches. Zaino's [#991](https://github.com/zingolabs/zaino/issues/991) (CI fails on non-zingolabs branches) is a CI risk Zinder should account for early.
- Deprecation policy. Zaino's two-spec problem partly comes from never being able to retire either; Zinder must declare what "deprecated" means, what the timeline is, and how clients are notified.

### Seams to preserve

- A new wallet endpoint must be expressible in proto without inventing a new transport. If a feature genuinely requires a non-gRPC channel, that is a separate ADR.
- Wallet API extensions land as additive proto fields with version tags so older wallets keep working.

## Pattern 3: Status, Health, and Lifecycle as After-Thoughts

### What the tracker says

The single largest cluster of open architecture work in Zaino is lifecycle and status:

- [#1040](https://github.com/zingolabs/zaino/issues/1040): tracking issue for `/livez` and `/readyz` as external operator probes. Zaino has internal probing traits, but the user-facing HTTP readiness contract is still tracked separately.
- [#722](https://github.com/zingolabs/zaino/issues/722): _"We need proper health and ready endpoints"_ (open).
- [#643](https://github.com/zingolabs/zaino/issues/643): liveness and readiness gates, referencing Zebra's [#8830](https://github.com/ZcashFoundation/zebra/issues/8830).
- [#889](https://github.com/zingolabs/zaino/issues/889): two parallel status types (`AtomicStatus` and `NamedAtomicStatus`) that never got unified.
- [#923](https://github.com/zingolabs/zaino/issues/923): readiness is a number, not a typed cause; downstream consumers cannot tell `syncing` from `node_unavailable`.
- [#649](https://github.com/zingolabs/zaino/issues/649): aggregate status reports `Syncing` even when the cache is up-to-date because mempool readiness is folded into the same flag.
- [#1051](https://github.com/zingolabs/zaino/issues/1051), [#1052](https://github.com/zingolabs/zaino/issues/1052), [#1053](https://github.com/zingolabs/zaino/issues/1053): three separate refactors to move polling-based shutdown to `CancellationToken`.
- [#1032](https://github.com/zingolabs/zaino/issues/1032) (closed): `DbV0::shutdown` and `DbV1::shutdown` _unconditionally slept 5 seconds_ instead of awaiting the background handle.
- [#1033](https://github.com/zingolabs/zaino/issues/1033): a background task that completes may not be registered for shutdown notification, depending on a race.
- [#498](https://github.com/zingolabs/zaino/issues/498) (closed): race conditions in the chain index status/shutdown path.

### Design pressure

Zaino started with `AtomicBool` and `AtomicU8` as the lifecycle vocabulary. Every new subsystem added another flag. By the time the maintainers wanted typed readiness, every status check was a fan-out over scattered atomics, and every shutdown was a fan-out over polling loops.

Operators paid for this in three places: load balancer health checks reduced to `tcp_open` because the HTTP probe never landed; `kubectl rollout` extended by sleep(5) DB shutdowns; and incident response degraded by status booleans that did not encode `why not ready`.

### Zinder design response

- [Service Operations](../architecture/service-operations.md) makes lifecycle a first-class contract:
  - Typed startup phases (`load_config`, `validate_config`, `open_storage`, `check_schema`, `connect_node`, `recover_state`, `start_api`, `ready`).
  - `/healthz`, `/readyz`, `/metrics` required from day one.
  - Readiness response is JSON with a machine-readable cause.
- [RFC-0001 §Operations Model](../rfcs/0001-service-oriented-indexer-architecture.md) enumerates required readiness causes: `starting`, `syncing`, `ready`, `migrating`, `node_unavailable`, `storage_unavailable`, `schema_mismatch`, `reorg_window_exceeded`, `shutting_down`. These map directly to the gaps Zaino is still closing in #1040.

### Seams to preserve

- Shutdown in every long-running task uses `tokio_util::sync::CancellationToken` (or equivalent) and emits a structured shutdown event. No `sleep` in shutdown paths. No polled bool flags.
- Lifecycle events are emitted as `ChainEvent`-style envelopes for tests and external observers, not just logs. Zaino's `check_for_critical_errors` ([#980](https://github.com/zingolabs/zaino/issues/980)) is the review risk to avoid: a function that decides what is critical based on log-message inspection.

## Pattern 4: Storage as a Linear Migration Ladder

### What the tracker says

- [#859](https://github.com/zingolabs/zaino/issues/859): the migration system is a strict `(major, minor, patch)` linear ladder; adding any DB version requires editing a global match. The team is now planning a version-graph migration system (option A) or a migration-ID DAG (option B) to handle multiple coexisting major versions ([#860](https://github.com/zingolabs/zaino/issues/860), [#858](https://github.com/zingolabs/zaino/issues/858)).
- [#862](https://github.com/zingolabs/zaino/issues/862) and [#1049](https://github.com/zingolabs/zaino/issues/1049): the lifecycle scaffolding for `DbV0` and `DbV1` was never DRY'd; introducing `DbV2` requires re-implementing the same scaffolding for the third time.
- [#836](https://github.com/zingolabs/zaino/issues/836): partially written blocks could corrupt the DB.
- [#904](https://github.com/zingolabs/zaino/issues/904): the default LMDB map size was too small for testnet, so a brand-new operator on testnet would silently fail.
- [#900](https://github.com/zingolabs/zaino/issues/900): the address-history hot loop was CPU-bound through `mdb_cursor_next` and crippled initial sync.
- [#865](https://github.com/zingolabs/zaino/issues/865): the non-finalised state tried to hold the entire chain in memory before finalised sync started.
- [#538](https://github.com/zingolabs/zaino/issues/538): Zaino re-implemented consensus-critical block and transaction parsing instead of reusing Zebra's parsers.
- [#549](https://github.com/zingolabs/zaino/issues/549) and [#615](https://github.com/zingolabs/zaino/issues/615): silent `u64 → u32` truncation, unsafe `Height` arithmetic.

### Design pressure

LMDB was chosen early, the migration system was added when only one schema existed, and the parser was written before Zebra's parser surface was the obvious integration point. Each decision was reasonable in isolation; together they increased the scope of storage changes.

A user wanting "smaller on-disk footprint" ([#858](https://github.com/zingolabs/zaino/issues/858) DbV2) has to account for three structural constraints at once: the migration ladder cannot represent two coexisting majors, the lifecycle scaffolding cannot be shared, and the existing parsing code mixes consensus rules with indexing concerns.

### Zinder design response

- [ADR-0001](../adrs/0001-rocksdb-canonical-store.md): RocksDB is the canonical KV store. Column families replace per-major-version directories.
- [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md): canonical storage is reached only through `ChainEpochReadApi`. `zinder-query` does not open the live RocksDB database in production.
- [Storage Backend](../architecture/storage-backend.md): explicit fingerprint, atomic batch commits per epoch, schema version stored alongside data.
- [PRD-0001 Testing Decisions](../prd-0001-zinder-indexer.md): atomic commits, migration refusal without an explicit migration mode, snapshot consistency, schema-version checks, and crash recovery are required test categories.
- Block and transaction parsing is delegated to maintained Zcash consensus primitives behind `zinder-source` or ingestion adapters. Zinder defines indexing artifacts on top of validated parser output, not custom byte parsers.

### Zinder decisions

- Migration shape. Zaino's option-A version graph is the model Zinder adopts in [Storage backend](../architecture/storage-backend.md): each artifact family records its schema fingerprint, and `StoreMigrator` computes an explicit path before `--apply`.
- Newtype audit. `Height`, `BlockHash`, `TxId`, `ReorgWindowDepth`, `FinalizedHeight` should be domain newtypes with checked arithmetic. Zaino's silent-truncation bug ([#549](https://github.com/zingolabs/zaino/issues/549)) is a textbook case.
- LMDB map-size analogue. RocksDB has fewer hard sizing constraints, but operator-facing defaults must be calculated from network, not hard-coded for mainnet.

### Seams to preserve

- Adding a new artifact must not require a global enum edit. New artifacts are additive column families with their own schema fingerprint.
- Multiple readers, single writer, enforced at the `zinder-store` API surface, not at deployment time.

## Pattern 5: Reorg and Chain-Edge Correctness

### What the tracker says

- [#1006](https://github.com/zingolabs/zaino/issues/1006): _"Zaino assumes the genesis block is committed. We have a 10 second delay to work around this but there is of course a race condition."_
- [#1007](https://github.com/zingolabs/zaino/issues/1007): Zaino assumes there are always more than 100 blocks (originally surfaced in the Crosslink workshop).
- [#785](https://github.com/zingolabs/zaino/issues/785): hostile reorg conditions cause the non-finalised state sync to fail.
- [#786](https://github.com/zingolabs/zaino/issues/786): best chain is selected by tip height, not cumulative chainwork.
- [#679](https://github.com/zingolabs/zaino/issues/679): `coinbase_height` optionality misused as a "is on best chain" signal.
- [#903](https://github.com/zingolabs/zaino/issues/903): `ReadStateService` does not sync non-finalised state on testnet.
- [#1015](https://github.com/zingolabs/zaino/issues/1015) and [#919](https://github.com/zingolabs/zaino/issues/919): `zainod` returns the genesis block in an edge case before sync; `DatabaseSize(0)` causes startup to hang.
- [#838](https://github.com/zingolabs/zaino/issues/838): `FetchService` fails to start with empty chain in regtest.
- [#755](https://github.com/zingolabs/zaino/issues/755), [#756](https://github.com/zingolabs/zaino/issues/756), [#509](https://github.com/zingolabs/zaino/issues/509): regtest treated as a second-class network with hard-coded mainnet assumptions.
- [#865](https://github.com/zingolabs/zaino/issues/865): non-finalised state grows unbounded before finalised state catches up.

### Design pressure

Zaino encoded chain-edge invariants as constants rather than as state. Once "100 confirmations" lives in code, every test, regtest scenario, and Crosslink demonstration that violates the assumption becomes a bug.

Zaino's tip-vs-chainwork bug ([#786](https://github.com/zingolabs/zaino/issues/786)) is particularly instructive: most of the time tip-height is the right answer because chainwork agrees with height. The constant works until the day someone runs a sufficiently hostile reorg test. There is no test failure, just an undetected wrong answer.

### Zinder design response

- [RFC-0001 §Reorg Model](../rfcs/0001-service-oriented-indexer-architecture.md) defines the reorg state machine explicitly: `observe_chain_source → fetch_missing_ancestors → select_best_chain → build_chain_artifacts → commit_chain_epoch → publish_chain_event → finalize_tip_if_ready`. Each transition has tests.
- [PRD-0001 Testing Decisions](../prd-0001-zinder-indexer.md): "ingestion tests must cover initial sync, restart from partial sync, source disconnect, reorg inside the non-finalized window, reorg beyond the supported window, mempool add/remove, and duplicate block delivery."
- `ChainEpoch` carries `tip_height` and `finalized_height` separately; "finalised" is a typed boundary, not an integer threshold.

### Zinder decisions

- Best-chain selection rule. `select_best_chain` selects by cumulative chainwork, not tip height. Zaino's [#786](https://github.com/zingolabs/zaino/issues/786) is direct evidence that this must be written down.
- Empty-chain and genesis behaviour. Zinder has an explicit `ChainEpoch::empty()` state and tests that exercise startup at heights 0, 1, and 99.
- Regtest parity. Tests must run on regtest with assertion that no behaviour reads from a hard-coded network constant. Zaino's [#509](https://github.com/zingolabs/zaino/issues/509) (faucet receives one block less in regtest) is the kind of scenario Zinder should handle directly.

### Seams to preserve

- Activation heights come from the upstream node, never from Zinder constants. Zaino's [#743](https://github.com/zingolabs/zaino/issues/743) made this point precisely: "All subsequent references to activation heights should be made by querying the zebrad being tested."
- `ReorgWindow` is a configurable parameter with a documented default, not a constant. Crosslink and other future protocol shifts should change configuration, not require a code change.

## Pattern 6: Performance as a Sequential Implementation

### What the tracker says

- [#791](https://github.com/zingolabs/zaino/issues/791): `GetBlockRange` is 3x slower than `lightwalletd` on a 100-block range. Root cause: a sequential per-block fetch loop with per-block channel sends. Wallet sync of mainnet (~3.2M blocks) takes 5 minutes against Zaino vs ~1.6 minutes against `lightwalletd`.
- [#974](https://github.com/zingolabs/zaino/issues/974): `get_address_utxos` materialises the full backend result before applying `max_entries`. A user asking for 10 records loads everything.
- [#912](https://github.com/zingolabs/zaino/issues/912) (closed): slow init for very large block ranges in `get_compact_block_stream`.
- [#900](https://github.com/zingolabs/zaino/issues/900) (closed): `addr_hist_records_by_addr_and_index_blocking` is CPU-bound and slows initial sync.
- [#789](https://github.com/zingolabs/zaino/issues/789): `GetTaddressTxids` has no pagination support.

### Design pressure

Zaino's API layer was written to satisfy correctness first, then to optimise per endpoint. Streaming behaviour was bolted on per method. Database batch cursor APIs already existed at the storage layer ([#791](https://github.com/zingolabs/zaino/issues/791) confirms this), but the gRPC handlers never plumbed them through.

Result: the most common wallet-sync operation is the slowest. The endpoints Zaino is faster on (`GetLatestBlock`, `GetLightdInfo`) are the ones that do not need bulk reads.

### Zinder design response

- [Wallet Data Plane](../architecture/wallet-data-plane.md): block ranges are a streaming contract, not a per-block iteration.
- [PRD-0001](../prd-0001-zinder-indexer.md) lists "compatibility tests against lightwalletd-style clients" as required, which provides the regression gate Zinder needs for this product shape.
- The `ChainEpochReader` boundary makes batch reads cheap because it pins to a snapshot for the duration of the response.

### Zinder decisions

- Performance budgets. Zinder publishes target latency and throughput numbers per endpoint and gates them in CI. `lightwalletd` is the floor, not the goal: Zinder must beat it on `GetBlockRange` if migration is to be sold to wallet maintainers.
- Pagination policy. Every list endpoint defines a maximum return size and a pagination cursor. The Zaino `get_address_utxos` materialise-then-truncate pattern ([#974](https://github.com/zingolabs/zaino/issues/974)) must be impossible to write because the type signature requires a cursor.

### Seams to preserve

- Read paths use storage cursors, not loops over single fetches.
- Hot paths are benchmarked against `lightwalletd` as part of the test suite, not as a one-off audit.

## Pattern 7: Configuration as a God Object

### What the tracker says

A whole sub-cluster of open issues (created by Zaino's own maintainers as a self-audit) describes the configuration shape:

- [#502](https://github.com/zingolabs/zaino/issues/502): `IndexerConfig` is a god object mixing backend, JSON-RPC, gRPC, source, cache, database, and debug fields in one struct.
- [#503](https://github.com/zingolabs/zaino/issues/503): boolean blindness. `node_cookie_auth: bool` plus `node_cookie_path: Option<String>` allows `(true, None)` to compile; runtime validation tries to catch it.
- [#504](https://github.com/zingolabs/zaino/issues/504): feature envy. Config structs manipulate state outside themselves.
- [#505](https://github.com/zingolabs/zaino/issues/505): implicit invariants encoded in docs, not types.
- [#506](https://github.com/zingolabs/zaino/issues/506): long-method config validation.
- [#515](https://github.com/zingolabs/zaino/issues/515): public API tightly coupled to Zebra internal config.
- [#685](https://github.com/zingolabs/zaino/issues/685): TLS validation inline in `check_config`, not delegated to the TLS sub-config.
- [#737](https://github.com/zingolabs/zaino/issues/737): port 18232 hardcoded for testnet, preventing regtest on 8232.
- [#735](https://github.com/zingolabs/zaino/issues/735): config env-var names collide with Zebra's.
- [#645](https://github.com/zingolabs/zaino/issues/645): the meaning of `cookie_dir` is ambiguous; tests use it for the JSON server's cookie, prod for Zebra's.

### Design pressure

Configuration grew organically with features. Each new flag touched the same struct, each new validation rule touched the same `check_config` method. By the time the team wanted to split per service, the existing struct was load-bearing for tests, examples, and external integrations.

The user-visible damage is silent invalid states: a user setting `node_cookie_auth = true` and forgetting `node_cookie_path` produces a runtime error at startup, sometimes after a slow stage. Operators dislike this much more than parse errors.

### Zinder design response

- [Service Boundaries](../architecture/service-boundaries.md): each service has its own configuration. There is no shared `IndexerConfig` god object.
- [Service Operations](../architecture/service-operations.md): startup phases include `load_config` and `validate_config`. Validation is typed and runs before any side effect.
- PRD-0001: "production configuration must reject unsafe defaults".

### Zinder decisions

- Config-as-types policy. TLS, cookie auth, and upstream-node credentials must be enum variants that encode valid combinations. Zaino's bool+Option pattern ([#503](https://github.com/zingolabs/zaino/issues/503)) is a shape Zinder's review process should avoid by construction.
- Layered config: defaults → file → environment → CLI. The precedence order is documented in [Service operations](../architecture/service-operations.md).
- Network-aware defaults. Zinder must derive defaults from `network` (mainnet/testnet/regtest), not hard-code mainnet ([#737](https://github.com/zingolabs/zaino/issues/737), [#509](https://github.com/zingolabs/zaino/issues/509)).

### Seams to preserve

- Each service ships a `--print-config` mode that emits the effective configuration in the same format the file uses. Operators should never have to read code to understand what Zinder will do.
- Adding a new sub-config means adding a new typed module, not a new field on a god struct.

## Pattern 8: Upstream Node Coupling and Version Skew

### What the tracker says

- [#1034](https://github.com/zingolabs/zaino/issues/1034) (closed, release-blocker): update to Zebra 4.3.1.
- [#926](https://github.com/zingolabs/zaino/issues/926) (closed, top-priority): support Zebra 4.2.
- [#816](https://github.com/zingolabs/zaino/issues/816) (closed): support Zebra 4.
- [#561](https://github.com/zingolabs/zaino/issues/561) (closed): _"Entangling Dependencies and Version Skew Slow Zebra Update."_
- [#480](https://github.com/zingolabs/zaino/issues/480) (closed): prune build dependencies to limit impact of a `zingolib` internal dependency.
- [#982](https://github.com/zingolabs/zaino/issues/982): node connection retries are defined in `chaindex` (the consumer), not in the source connector (the owner).
- [#743](https://github.com/zingolabs/zaino/issues/743): activation heights have multiple sources of truth across `zaino` and `zebra-*`.
- [#829](https://github.com/zingolabs/zaino/issues/829): Zebra's read-state service returns only best-chain blocks, constraining which Zaino RPCs can be served from indexed state.

### Design pressure

Zaino imports `zebra-state`, `zebra-rpc`, and `zebra-chain` directly across multiple crates. Every Zebra release becomes a Zaino dependency-update issue, sometimes tied to release timing. The blast radius is wide because Zebra and source-specific types appear in `zaino-state`, `zaino-fetch`, and even `zainod`'s public configuration.

Upstream node coupling also narrows the feature space: supporting both Zebra 4.2 and 4.3 is harder when type imports collide.

### Zinder design response

- [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md) names `zinder-source` as the source-adapter crate.
- [PRD-0001](../prd-0001-zinder-indexer.md) story 16: _"As a node implementation maintainer, I want Zinder to isolate source adapters, so that Zebra ReadState, JSON-RPC, and future streaming sources can evolve without rewriting query APIs."_
- The vocabulary forbids leaking Zebra or source-specific types past `zinder-source`.

### Zinder decisions

- Capability detection. Zaino's [#1034](https://github.com/zingolabs/zaino/issues/1034) cycle (one issue per Zebra version) is answered by [Node source boundary](../architecture/node-source-boundary.md): detect source capabilities at connection time rather than gate on a hard version pin.
- Multi-source readiness. Multiple source backends (Zebra ReadState, JSON-RPC, future streaming source) report through the same `NodeCapabilities` and typed readiness model.
- Activation heights as data, not code. Zaino's [#743](https://github.com/zingolabs/zaino/issues/743) lesson must be encoded in `zinder-source`: the upstream node is the source of truth.

### Seams to preserve

- A new source backend is a new `zinder-source` module that implements the same `NodeSource` trait. No churn elsewhere.
- Retries and reconnects are owned by the source, not by the consumer ([#982](https://github.com/zingolabs/zaino/issues/982)).

## Pattern 9: Test and Dev Surface Brittleness

### What the tracker says

- [#991](https://github.com/zingolabs/zaino/issues/991): CI depends on access patterns that make non-zingolabs branches hard to validate.
- [#893](https://github.com/zingolabs/zaino/issues/893): CI is overwhelmed by noisy tests.
- [#1037](https://github.com/zingolabs/zaino/issues/1037), [#1036](https://github.com/zingolabs/zaino/issues/1036): flakiness from mempool-vs-indexer tip-hash race conditions.
- [#1039](https://github.com/zingolabs/zaino/issues/1039), [#1038](https://github.com/zingolabs/zaino/issues/1038): slow tests (10–12 seconds each, 60 seconds total).
- [#946](https://github.com/zingolabs/zaino/issues/946) (closed), [#947](https://github.com/zingolabs/zaino/issues/947) (closed): test code and production code originally lived in one workspace; test vectors were mixed into production crates.
- [#870](https://github.com/zingolabs/zaino/issues/870), [#869](https://github.com/zingolabs/zaino/issues/869), [#868](https://github.com/zingolabs/zaino/issues/868): CI re-downloads the Rust toolchain every run, installs `cargo-nextest` from source, uses an oversized base image.
- [#411](https://github.com/zingolabs/zaino/issues/411): `fmt`/`clippy` give different results locally vs CI.

### Design pressure

Test infrastructure was authored as needed and later had to absorb more contributor and CI cases. For Zinder, the contribution path should be explicit enough that a new contributor can clone, run the documented gates, and trust the same checks CI will run ([#991](https://github.com/zingolabs/zaino/issues/991)).

### Zinder design response

- `zinder-testkit` is named in [RFC-0001 §Workspace Crates](../rfcs/0001-service-oriented-indexer-architecture.md): "deterministic source, chain, reorg, and wallet API fixtures."
- [PRD-0001 Testing Decisions](../prd-0001-zinder-indexer.md): tests are categorised (ingestion, storage, query, compatibility, operations, derived) and external behaviour is the primary target.

### Zinder decisions

- CI policy that supports external contributors from day one. CI on forks is mandatory before public announcement.
- Toolchain pin lives in `rust-toolchain.toml`. CI builds use the same image as the dev container; mismatched `clippy` is a process failure.
- Unit-test budget. Zaino's 60-second unit-test runtime ([#1030](https://github.com/zingolabs/zaino/issues/1030)) is the upper bound Zinder should not exceed; if a test takes 10+ seconds it belongs in an integration suite.

### Seams to preserve

- Deterministic chain fixtures live in `zinder-testkit`. Tests must be runnable without a real upstream node.
- The dev profile (`zinder dev`) is a composition layer, not a parallel implementation. Operators and developers run the same code path.

## Pattern 10: Observability as a Discussion, Not a Contract

### What the tracker says

- [#856](https://github.com/zingolabs/zaino/issues/856): _"Zaino doesn't have any sort of metrics framework so it will be helpful to align those with whatever Zebra folks have in mind."_ Open, marked "Just discuss this!"
- [#1040](https://github.com/zingolabs/zaino/issues/1040): the readiness tracker lists health/ready as still-undelivered, two years after launch.
- [#688](https://github.com/zingolabs/zaino/issues/688): support for advertising Onion-Location, dependent on a generic header/policy story.

### Design pressure

Zaino tracks observability as follow-up work. Every operator who wanted Prometheus metrics, alerting, or SLO tracking had to instrument by side-channel: log scraping, TCP probes, shell scripts that diff `getinfo` over time. For Zinder's product guarantees, the metrics contract needs to exist before downstream consumers depend on the service.

### Zinder design response

- [Service Operations](../architecture/service-operations.md): `/metrics` is a launch requirement, not a milestone.
- [PRD-0001](../prd-0001-zinder-indexer.md) story 17 lists the required dimensions: chain lag, commit latency, reorg depth, query latency, DB health, API error classes.

### Seams to preserve

- Adding a metric is a code change in `zinder-ingest`, `zinder-query`, or `zinder-derive`, not a new dependency. Standard names should be defined in `zinder-core` so that operators can build dashboards without reading code.
- Logs and metrics are separate signals. Critical state is reported as a metric and a typed event, not by string-matching log lines.

## User Requests That Inform Zinder's Scope

This is the user-facing summary. Each row links a request from the Zaino tracker to the design pressure it creates for Zinder.

| User request                                         | Tracker                                                                                                                                                                                                                    | Zinder design input                                                                                                                                               | Pattern |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| `/livez`, `/readyz` HTTP probes                      | [#722](https://github.com/zingolabs/zaino/issues/722), [#643](https://github.com/zingolabs/zaino/issues/643), [#1040](https://github.com/zingolabs/zaino/issues/1040)                                                      | Status is `AtomicBool`s; readiness has no typed cause; cannot map sub-states cleanly                                                                              | 3       |
| Prometheus metrics                                   | [#856](https://github.com/zingolabs/zaino/issues/856)                                                                                                                                                                      | No metrics framework was chosen at launch; retrofit cost is high                                                                                                  | 10      |
| `GetBlockRange` parity with `lightwalletd`           | [#791](https://github.com/zingolabs/zaino/issues/791)                                                                                                                                                                      | gRPC handlers loop sequentially; storage batch APIs exist but never wired through                                                                                 | 6       |
| `GetTaddressTxids` pagination                        | [#789](https://github.com/zingolabs/zaino/issues/789)                                                                                                                                                                      | API was authored without a cursor type; adding one is a wire-and-storage change at once                                                                           | 1, 6    |
| Smaller-footprint DB option (DbV2)                   | [#858](https://github.com/zingolabs/zaino/issues/858), [#860](https://github.com/zingolabs/zaino/issues/860)                                                                                                               | Linear migration ladder cannot represent two coexisting majors; DbCore lifecycle never DRY'd                                                                      | 4       |
| Pass-through queries when not fully synced           | [#769](https://github.com/zingolabs/zaino/issues/769)                                                                                                                                                                      | `ChainIndex` types do not include the data needed for finalised/non-finalised passthrough; `ChainBlock` requires cumulative chainwork available only from genesis | 1, 4, 5 |
| Non-standard scriptpubkey support in CompactBlocks   | [#818](https://github.com/zingolabs/zaino/issues/818)                                                                                                                                                                      | Fixed-length 21-byte tag in finalised state cannot represent variable-length scripts without performance regression                                               | 1, 4    |
| Reliable regtest support                             | [#737](https://github.com/zingolabs/zaino/issues/737), [#755](https://github.com/zingolabs/zaino/issues/755), [#756](https://github.com/zingolabs/zaino/issues/756), [#509](https://github.com/zingolabs/zaino/issues/509) | Mainnet assumptions hard-coded across configuration, height handling, and reward calculation                                                                      | 5, 7    |
| Onion-Location header                                | [#688](https://github.com/zingolabs/zaino/issues/688)                                                                                                                                                                      | No general policy mechanism for response headers                                                                                                                  | 10      |
| External-contributor CI                              | [#991](https://github.com/zingolabs/zaino/issues/991)                                                                                                                                                                      | CI uses self-hosted runners gated on org membership                                                                                                               | 9       |
| Container image that meets common security standards | [#885](https://github.com/zingolabs/zaino/issues/885), [#866](https://github.com/zingolabs/zaino/issues/866), [#873](https://github.com/zingolabs/zaino/issues/873)                                                        | Docker tooling grew organically; non-root, XDG, image size, and base-image choice were all bolt-on                                                                | 7, 9    |
| Reorg-aware block lookup beyond best chain           | [#829](https://github.com/zingolabs/zaino/issues/829), [#550](https://github.com/zingolabs/zaino/issues/550)                                                                                                               | Zaino's storage shape and its dependency on Zebra's `ReadStateService` together exclude non-best-chain blocks                                                     | 1, 5, 8 |
| Unified RPC spec replacing dual-spec confusion       | [#802](https://github.com/zingolabs/zaino/issues/802), [#804](https://github.com/zingolabs/zaino/issues/804)                                                                                                               | Two specs (CompactTxStreamer + zcashd JSON-RPC) inherited from upstream; never owned a canonical doc                                                              | 2       |

The common thread: every entry in this table touches more than one design pressure. Zinder should treat those rows as product-contract input rather than as isolated feature requests.

## Review Risks To Catch Early

These come from the tracker. They are listed here so reviewers can name the risk quickly:

1. A configuration struct that mixes server, storage, source, and debug concerns. ([#502](https://github.com/zingolabs/zaino/issues/502))
2. A bool flag paired with an optional path or value, where `true` + `None` compiles. ([#503](https://github.com/zingolabs/zaino/issues/503))
3. A trait whose generic bound is a concrete transport library. ([#717](https://github.com/zingolabs/zaino/issues/717))
4. A "service" that silently delegates to another backend on certain inputs. ([#716](https://github.com/zingolabs/zaino/issues/716))
5. A custom parser for consensus-critical data when the upstream node or Zcash primitive crates already provide validated structures. ([#538](https://github.com/zingolabs/zaino/issues/538))
6. A shutdown path that polls a flag with `sleep`. ([#1032](https://github.com/zingolabs/zaino/issues/1032), [#1051](https://github.com/zingolabs/zaino/issues/1051))
7. A status type that proliferates because no module owns "what does ready mean." ([#889](https://github.com/zingolabs/zaino/issues/889), [#923](https://github.com/zingolabs/zaino/issues/923))
8. A list endpoint without a cursor or maximum size. ([#974](https://github.com/zingolabs/zaino/issues/974), [#789](https://github.com/zingolabs/zaino/issues/789))
9. A range read implemented as `for height in start..=end { fetch_one(height).await }`. ([#791](https://github.com/zingolabs/zaino/issues/791))
10. A constant for "100 confirmations" or "genesis exists" that lives outside test fixtures. ([#1006](https://github.com/zingolabs/zaino/issues/1006), [#1007](https://github.com/zingolabs/zaino/issues/1007))
11. A migration system whose only model of progress is a single linear ladder. ([#859](https://github.com/zingolabs/zaino/issues/859))
12. A public-API field whose type is `zebra_state::Config` or another upstream-node internal type. ([#515](https://github.com/zingolabs/zaino/issues/515))
13. CI that fails on forks because of a self-hosted runner gate. ([#991](https://github.com/zingolabs/zaino/issues/991))
14. Two parallel sources of truth for the same data (RPC spec, activation heights, schema version). ([#907](https://github.com/zingolabs/zaino/issues/907), [#743](https://github.com/zingolabs/zaino/issues/743))

## Open Seams Zinder Must Preserve

Architectural decisions are bets about which axes of change matter. The list below describes the changes Zinder should be ready to absorb without rewrite, drawn from requests and constraints visible in the Zaino tracker.

- **New artifact types.** Adding a "transparent outpoint to spending tx" index or any other materialised view is an additive change in `zinder-core` (domain), `zinder-store` (column family + schema fingerprint), and optionally `zinder-proto` (wire). No central enum to edit.
- **New source backends.** Adding a streaming Zebra source, a `zcashd` JSON-RPC source, or a Crosslink source is a new module in `zinder-source`. No callers change.
- **Multiple coexisting storage majors.** Operators who want a small-footprint variant should be able to switch between schemas without losing data, similar to Zaino's planned DbV2 ([#858](https://github.com/zingolabs/zaino/issues/858)) but without the linear migration tax.
- **Wallet API extensions.** New methods land as additive `.proto` fields with version tags. Older wallets keep working.
- **Derived index families.** Explorer, analytics, or compliance views land in `zinder-derive` and consume `ChainEventEnvelope`. They can fail or rebuild without affecting wallet sync.
- **Lifecycle observers.** A new operator integration (Kubernetes, systemd, OCI runtime) reads the existing typed readiness causes; no new endpoint required.
- **Network awareness.** Mainnet, testnet, regtest, and future networks differ only in configuration and fixtures, not in code paths.

## How To Use This Document

When a Zinder PR adds a new public type, configuration field, RPC method, or storage layout, the reviewer should ask: which design pressure from this document would this reintroduce if it were merged as written? If the answer is one of the risks above, link the relevant section in the review.

When a user files a feature request, the maintainer should check whether the request maps onto an open seam in the section above. If it does, the request should be expressible as an additive change. If it does not, the request requires an architecture decision before implementation.

## Closing Note

Zaino's tracker is valuable prior art. The point is to treat public project experience as design input. Zinder starts with the benefit of Zaino maintainer discussions, Zallet integration notes, and Zebra's newer source surfaces. The work below every Zinder ADR, RFC, and architecture doc is to choose the product guarantees deliberately instead of rediscovering those tradeoffs later.
