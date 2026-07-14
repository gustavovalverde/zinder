# Plan: Wallet Workload Presets

Status: Complete for the initial RocksDB preset release
Date: 2026-07-12
Source PRD: [Wallet workload presets](../prd/wallet-workload-presets.md)

This plan combines the implementation sequence with the investigation findings that constrain it. The PRD owns the product contract; this document owns evidence gates, tracer-bullet phases, migration limits, and requirement traceability.

## Current Implementation Status

| Phase | Status | Evidence boundary |
| --- | --- | --- |
| Phase 0 | Complete | Deterministic recent and dense-mainnet controls use fixed bytes and cloned starting state. The dense pair and repeat satisfy R-BENCHMARK-1. |
| Phase 1 | Complete | Wallet history and spender reads plus explorer transaction history use typed projection-specific readiness and capability decisions. |
| Phase 2 | Complete; go gate | Wallet and complete execution cross registration, dispatch, startup composition, replay, retention, public reads, compatibility tests, and backup metadata. |
| Phase 3 | Complete | Fresh stores accept `wallet` or `complete`; configuration, readiness, server information, metrics, readers, and omitted capabilities agree on the persisted selection. |
| Phase 4 | Complete for recorded claim | Backup, restore admission, restart, shutdown, compact and transparent reads, broadcast, pending-input protection, and native reorg recovery passed within the evidence-scoped configurations. |
| Phase 5 | Trigger-gated | No database-adapter or worker-extraction trigger has opened this phase. |

Raw measurements and live observations are recorded in `docs/investigations/2026-07-12-wallet-workload-preset-implementation-evidence.md`. The dense control passed the gate before `ProjectionPreset` became public configuration.

## Outcome

Zinder will support a fresh-store wallet preset that materializes only wallet-required projections and a complete preset that preserves the current shared-product workload. Presets expand to stable projection identities, while canonical facts, payload retention, projection lifecycle state, and future worker placement remain independent.

The implementation does not start by adding a configuration enum. It first establishes a reproducible workload benchmark and a typed projection-read seam, then proves the wallet projection set as an internal end-to-end slice. Public configuration follows only after the measured and correctness gates pass.

## Architectural Decisions

### Product selection

- The operator interface is a closed preset, not an arbitrary projection allowlist.
- The initial presets are `wallet` and `complete`; `complete` preserves current default behavior.
- A preset expands to stable projection identities and required startup work.
- Per-projection manifests, schemas, cursors, recovery coverage, freshness, and retention authority remain the durable state.

### Projection roles

- `transparent_outpoint_spend` is wallet-correctness and retention-critical.
- `transparent_address_transaction_history` is wallet-serving but does not gate canonical retention.
- Every other current bundled projection is an optional product view and belongs only to `complete` initially.
- New projections must declare a product role and preset membership before they ship.

### Canonical and payload separation

- Both presets build the same canonical facts in the first release.
- The plan does not introduce wallet-specific canonical schemas.
- Raw payload retention remains an independent policy.
- Projection selection does not mutate payload retention. Wallet-serving coverage defaults to transaction blobs; full-block wallet deployments select block and transaction blobs.
- The complete preset does not imply raw block retention.

### Lifecycle

- Public preset selection applies only to fresh canonical-plus-projection stores in the first release.
- A mismatch fails before projection-manifest mutation.
- Existing-store expansion, reduction, and disk reclamation remain unsupported until a separate migration design proves recovery and rollback.
- The complete preset remains the compatibility path for existing stores and deployments.

### Execution topology

- The first implementation runs selected projections in process with the existing RocksDB topology.
- Preset vocabulary and projection identity do not depend on RocksDB.
- If the trigger-gated adapter track reaches worker extraction, the same identities become worker-assignment inputs.
- The plan does not prebuild a generic database adapter, a universal projection-row interface, or a standalone projector.

## Investigation Findings

### Current selection is implicit and all-or-nothing

The production composition opens the selected derive schema set, dispatches only selected consumers, and expands that same selection into startup guards, seeds, replay, snapshot bootstrap, tailing, backfills, and verification. Operators can select `wallet` or `complete` for a fresh store; omission resolves to `complete`.

This means projection selection crosses 7 concerns: primary registration, event dispatch, cursor discovery, startup jobs, secondary readers, capability decisions, and backup. Adding `if wallet` checks at each location would create a shallow configuration feature with poor locality. One orchestration module must own the effective projection set and provide it to each concern.

### Wallet minimum is 2 projections, not zero

The wallet preset cannot disable the derive plane entirely:

- Transparent-address transaction history backs native and lightwalletd-compatible history reads.
- Durable transparent outpoint-spend history preserves spender identity after canonical retention deletes settled spend facts.

The second projection is a correctness dependency. Its durable position authorizes canonical deletion, so it must keep the existing flush-before-acknowledgement ordering and startup guard.

### Projection fan-out has measurable cost

The 2026-07-11 and 2026-07-12 rebuild evidence recorded derive compaction and write stalls that materially exceeded canonical-store values in dense history. Lagging derive replay also delayed the transparent retention floor, which increased temporary disk use and contributed to full volumes. Restart recovery later held public readers unavailable while ingest performed startup replay and synchronous projection seeds.

The same evidence also showed that chain era dominates throughput. Sparse testnet rebuilt quickly even with the larger projection set, while dense mainnet ranges remained memory and replay constrained. The wallet preset therefore needs a fixed-input benchmark; wall-clock comparisons across different ranges are not sufficient.

### Existing-store transitions are unsafe

The projection upgrade experiment reproduced 3 failure directions:

1. Forward upgrade failed after the new derive manifest was written because the older canonical store lacked required historical facts.
2. Rollback failed because the older binary rejected the newly recorded projection identity.
3. Deleting only the derive store failed because canonical retention had already removed facts needed to rebuild durable outpoint-spend history.

The safe first lifecycle is a fresh canonical-plus-projection store plus read-only preflight validation. The preflight rejects an absent projection store beside canonical history, projection data without canonical history, a retention-authoritative projection behind an irreversible canonical sweep, and foreign or mismatched projection identities before the writer opens the projection store. Any later transition protocol must validate desired projection identities, canonical recovery coverage, schema compatibility, and retention acknowledgements before it mutates durable state.

### Database work changes placement, not product meaning

The database-adapter implementation plan gives sync-time work priority and keeps adapter execution trigger-gated. Its enduring projection direction is per-projection workers, typed persistence, independent cursors, and explicit worker selection. A preset over projection identities survives that topology; a RocksDB `DeriveStore` mode or global persisted profile enum does not.

The projection-read seam is useful now regardless of whether the adapter track activates. It prevents query behavior and capability logic from depending on column-family presence, and it gives presets one typed place to report projection availability and freshness.

## Alignment Invariants

Every implementation phase must preserve these invariants:

1. Canonical epoch publication remains atomic and independent of optional projection success.
2. Only the durable outpoint-spend projection may advance the transparent retention release floor.
3. A projection capability requires that projection's own readiness and verified coverage.
4. Omitted projections are disabled deliberately; they are not treated as failed or partially complete.
5. The wallet preset preserves compact sync, tree state, subtree roots, transparent UTXOs, transparent history, durable spend resolution, broadcast, chain events, and mempool behavior.
6. Raw block availability follows payload retention, not projection preset.
7. Preset validation completes before any manifest, schema, cursor, or projection row mutation.
8. The complete preset stays behaviorally compatible with the current shared deployment.
9. Benchmarks compare identical source input, starting store state, resource limits, and binary configuration except for the selected projection set.
10. Support claims distinguish method coverage, configuration, adapter implementation, historical evidence, and current certification.

## Projection Dependency Matrix

| Product behavior | Canonical facts or live state | Required projection | Payload requirement | Preset |
| --- | --- | --- | --- | --- |
| Compact shielded sync | Compact blocks, tree state, subtree roots | None | None | Wallet and complete |
| Transparent UTXO discovery | Canonical transparent output indexes | None | None | Wallet and complete |
| Transparent transaction history | Transaction facts and locations | `transparent_address_transaction_history` | Transaction blobs for the complete compatibility contract | Wallet and complete |
| Durable spender resolution | Canonical spend facts and retention markers | `transparent_outpoint_spend` | None | Wallet and complete |
| Full-block scanning | Canonical block blobs | None | Block and transaction blobs | Wallet and complete when configured |
| Broadcast and live mempool | Writer-owned live state and event history | None of the selected derive projections | Submitted transaction bytes are request data | Wallet and complete |
| Explorer summaries and histories | Canonical facts and events | Product-specific explorer projections | Usually none unless the method returns raw bytes | Complete |
| Analytics distributions and rankings | Canonical facts and events | Product-specific analytics projections | None by default | Complete |

The matrix is normative for the first implementation. A code change that introduces another dependency updates the PRD and this matrix before changing preset membership.

## Phase 0: Reproducible Control and Contract Gate

**User stories**: 12, 13, 16

### What to build

Complete the deterministic fixed-range replay harness and establish the complete-preset control on one dense historical range and one recent range. Finish the higher-priority ordered prevout resolver, canonical-first scheduling, and shared memory-ownership work before using the harness to judge projection selection. Add or confirm executable canonical invariants for writer exclusion, epoch atomicity, reorg repair, retention, and event history.

This phase does not add public preset configuration. It creates the evidence and correctness floor every later slice uses.

### Acceptance criteria

- [x] Identical-input reruns report variance and reproduce the deterministic density, row, and write measurements.
- [x] Each run reports canonical throughput, derive rows and bytes, compaction bytes, peak anonymous resident memory, projection lag, recovery time, and final disk use.
- [x] Dense and recent controls use cloned starting state and fixed source bytes.
- [x] Canonical invariant tests pass on the current RocksDB implementation.
- [x] Required sync-path prerequisites landed before the preset benchmark decision.
- [x] No public workload configuration was added before the benchmark gate passed.

## Phase 1: Typed Projection Readiness Slice

**User stories**: 10, 11, 12, 18

### What to build

Move wallet and explorer projection reads behind a typed projection-read seam without changing the active complete projection set. The seam reports projection identity, availability, schema compatibility, freshness, and verified coverage, and public capability decisions consume that information rather than a store-wide online flag.

The slice is complete when one existing wallet projection and one existing explorer projection travel through the seam from persisted state to public capability and error behavior. The remaining projection reads then migrate using the same contract.

### Acceptance criteria

- [x] Query behavior no longer infers every projection's availability from a generic derive-store status.
- [x] One wallet projection and one optional product projection expose independent readiness and failure states end to end.
- [x] Missing, lagging, incompatible, and incomplete projection states map to explicit existing or newly documented errors.
- [x] Complete-preset behavior and capability output remain unchanged.
- [x] Tests exercise public reads and capability decisions, not concrete column-family access.
- [x] The seam contains no generic byte-row or scan interface.

## Phase 2: Internal Wallet-Preset Tracer Bullet

**User stories**: 1, 4, 5, 6, 7, 13

### What to build

Add an internal, test-only execution path that expands the wallet preset to the 2 required projection identities on a fresh store. Carry that selection through primary registration, event dispatch, startup replay, retention acknowledgement, secondary reading, query readiness, compatibility reads, and backup capture.

Run the wallet and complete projection sets against the Phase 0 harness. This slice proves the architecture and produces the go or no-go evidence; it does not create an operator-facing configuration promise.

### Acceptance criteria

- [x] The fresh store records only the selected projection identities.
- [x] Optional product consumers, backfills, verifiers, and bootstrap jobs do not run in the wallet arm.
- [x] Transparent history and durable spender resolution work through public wallet and compatibility reads.
- [x] The retention floor advances only after durable outpoint-spend projection progress.
- [x] The wallet and complete arms run against identical fixed inputs and resource limits.
- [x] Results satisfy PRD requirement R-BENCHMARK-1 before public configuration.
- [x] The investigation report records any canonical, query, backup, or recovery dependency that the 2-projection model missed.

## Phase 3: Fresh-Store Operator Presets

**User stories**: 1, 2, 3, 8, 9, 10, 11, 18

### What to build

Expose the validated wallet and complete presets as one operator choice. Resolve the preset before storage opens, preflight it against store identity and projection manifests, and pass the effective projection set through every production composition path. Keep complete as the default.

The same slice adds printed configuration, readiness, server information, and metrics for the effective set. Payload retention remains independent: the wallet-serving coverage profile defaults to transaction blobs, full-block deployments select all blobs, and complete does not imply full blocks.

### Acceptance criteria

- [x] A fresh wallet store starts and reaches wallet-serving readiness with only the 2 required projections.
- [x] A fresh complete store preserves the current projection and capability set.
- [x] Configuration output states the selected preset and effective identities with secrets redacted.
- [x] Preset and payload-retention validation reports actionable errors.
- [x] An incompatible existing store fails before manifest mutation and points to the supported rebuild path.
- [x] Reader processes, compatibility adapters, explorer processes, and local clients interpret the same effective projection set.
- [x] Omitted optional projections remove only their dependent capabilities.
- [x] Existing configuration with no preset continues as complete.

## Phase 4: Recovery and Wallet Certification

**User stories**: 4, 5, 7, 9, 14, 17

### What to build

Complete the operator lifecycle for the supported fresh-store presets. Backup and restore record the effective projection set and per-projection position, while live validation exercises the wallet behaviors the preset claims.

Certification runs by integration method, not by wallet category. Lightwalletd-compatible clients exercise compact sync, tree and subtree state, transparent history, broadcast, mempool behavior, and recovery. Native clients exercise epoch-pinned reads, chain events, transparent spentness, and reorg handling. A full-block adapter additionally runs with block payload retention enabled.

### Acceptance criteria

- [x] Backup metadata records preset, canonical-history bounds, projection identities, schemas, positions, and omitted state.
- [x] Restore admission validates the canonical-plus-projection bundle before writer open; normal readiness and coverage checks remain the only capability authority.
- [x] Compact wallet flows pass against the wallet preset on regtest and the selected public test network.
- [x] Transparent history and old-spend recovery remain correct after safe-tip retention advances.
- [x] Broadcast, pending-input protection, restart, and native reorg flows pass.
- [x] Full-block wallet reads pass with payload retention set to all and fail explicitly without block blobs.
- [x] Release claims identify the wallet version, network, integration method, and evidence level actually tested.

## Phase 5: Projection-Worker Handoff, Trigger-Gated

**User stories**: 12, 15, 16

### Entry condition

This phase opens only if the database-adapter trigger fires and its prerequisites for worker extraction are accepted. It is not required to ship the compact RocksDB wallet preset.

### What to build

Reuse projection identities and preset membership as assignment inputs for independently fenced workers. Move one proven projection end to end through typed persistence operations and a second backend only when that deployment need exists. Keep the compact in-process executor as a supported topology.

### Acceptance criteria

- [ ] Product preset meaning is unchanged between in-process and worker execution.
- [ ] Projection rows, cursor, schema, and worker generation commit atomically in transactional stores.
- [ ] A stale worker cannot advance rows, cursor, capability freshness, or retention acknowledgement.
- [ ] The wallet-critical progress path preserves durable-commit-before-retention-acknowledgement ordering.
- [ ] Optional worker lag does not stop canonical ingest or wallet-critical projections.
- [ ] No universal projection row interface or detachable cursor store is introduced.

## Requirement Traceability

| PRD requirement | Primary phase | Verification |
| --- | --- | --- |
| R-PRESET-1, R-PRESET-2 | Phase 3 | Configuration, startup, and default-compatibility tests |
| R-WALLET-1, R-ROLE-1 | Phase 2 | Effective-set and public wallet-read tests |
| R-CANONICAL-1 | Phases 0 and 2 | Canonical contract suite under both benchmark arms |
| R-PAYLOAD-1 | Phases 3 and 4 | Configuration validation and full-block read tests |
| R-READ-1, R-CAPABILITY-1 | Phase 1 | Per-projection readiness and capability tests |
| R-LIFECYCLE-1, R-LIFECYCLE-2 | Phase 3 | Fresh-store and fail-before-mutation tests |
| R-OPS-1 | Phase 3 | Printed config, readiness, server information, and metrics |
| R-BACKUP-1 | Phase 4 | Backup and restore round trip |
| R-BENCHMARK-1 | Phases 0 and 2 | Fixed-range comparison report |
| R-FUTURE-1 | Phase 5 | Trigger-gated worker assignment proof |
| R-CLAIM-1 | Phase 4 | Evidence-scoped compatibility report |

## Stop Conditions

Stop and return to investigation when any of these conditions occurs:

- The wallet preset needs another projection to satisfy an existing wallet or compatibility contract.
- Omitting optional projections changes canonical artifacts or event semantics.
- The fixed-range benchmark does not show a repeatable improvement outside noise.
- The wallet arm reduces derive work but causes a canonical throughput, memory, or correctness regression.
- Preset validation cannot reject an incompatible store before durable mutation.
- Backup or restore cannot state the effective projection set truthfully.
- A proposed seam exposes RocksDB column families, generic rows, or storage keys to product-level code.

## ADR Decision Point

No new ADR is required to approve this PRD or begin Phases 0 and 1. After the Phase 2 tracer bullet produces benchmark and lifecycle evidence, decide whether the durable preset, projection-role, and fresh-store-only rules amend existing ADRs or require a new ADR. The decision must account for ADR-0005, ADR-0017, ADR-0018, ADR-0028, ADR-0029, and ADR-0031.

## Evidence Sources

Tracked contracts:

- [Consumer-neutral wallet data plane](../adrs/0005-consumer-neutral-wallet-data-plane.md)
- [Derive-consumer template](../adrs/0017-derive-consumer-template-and-key-codec-convention.md)
- [Capability-gated optional fields](../adrs/0018-capability-gated-optional-payload-fields.md)
- [Per-consumer derive schema versioning](../adrs/0028-per-consumer-derive-schema-versioning.md)
- [Durable outpoint-spend projection](../adrs/0029-durable-transparent-outpoint-spend-projection.md)
- [Projection checkpoints and coverage](../adrs/0031-projection-checkpoints-and-backfill-coverage.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
- [Derive plane](../architecture/derive-plane.md)
- [Testing runbook](../runbooks/testing.md)

Workspace investigations incorporated into this plan on 2026-07-12:

- `docs/investigations/2026-07-11-ironwood-deploy-rebuild-metrics.md`
- `docs/investigations/2026-07-11-performance-improvement-backlog.md`
- `docs/investigations/database-adapter-architecture.md`
- `docs/investigations/database-adapter-implementation-plan.md`

These workspace files remain useful for raw evidence, but this tracked plan owns the requirements and sequence needed during implementation.
