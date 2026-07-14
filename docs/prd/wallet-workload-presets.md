# Wallet Workload Presets

Status: Draft
Date: 2026-07-12
Author: Gustavo Valverde
Reference consumers: ZODL, Vizor, Zally, Zallet, and lightwalletd-compatible wallets
Implementation plan: [Wallet workload presets](../plans/wallet-workload-presets.md)

Zinder should let an operator build a wallet-serving store without also materializing every explorer and analytics projection. The product contract is a small set of named presets over stable projection identities, not a database-specific mode or a public projection allowlist.

## Problem Statement

Zinder currently indexes one shared canonical chain view and materializes every bundled projection in the same ingest deployment. This topology is appropriate for a shared installation that serves wallets, explorers, analytics, and future products, but a dedicated wallet deployment pays for projections that its clients never query. The extra work increases derive-store writes, compaction, disk use, replay lag, startup recovery, and the chance that optional projection work competes with canonical catchup.

The existing `wallet-serving` coverage profile does not solve this problem. It selects the historical artifact floor and a safe raw-transaction retention default, but it does not select projection work. A new product concept must remain distinct from coverage and payload retention so operators can answer 3 separate questions: how far back the store is complete, which read models it materializes, and which raw payloads it retains.

The change must also survive Zinder's planned storage evolution. RocksDB remains the current canonical backend, while a future adapter track may introduce independently fenced projection workers and projection-specific stores. Presets therefore select durable projection identities; they do not encode RocksDB column families, process placement, or a store-global backend mode.

## Evidence Summary

The investigation established 4 constraints:

1. Wallets share a small common chain-data requirement, but full-block wallets require more payload retention than compact-block wallets.
2. Two derive projections belong in the wallet contract: transparent-address transaction history and durable transparent outpoint-spend history.
3. Only durable outpoint-spend history may gate canonical transparent-spend retention. Explorer and analytics projections must not gain that authority.
4. Existing-store preset changes are unsafe without a dedicated migration protocol. A forward projection upgrade, rollback, and derive-only rebuild can each fail when canonical recovery inputs or manifest compatibility are missing.

The active rebuild evidence also shows that projection fan-out is operationally significant. Derive replay has produced materially more compaction and write stalls than canonical ingest in dense historical ranges, while projection lag can delay canonical retention and increase temporary disk pressure. These findings justify a controlled benchmark of a wallet preset; they do not justify skipping the fixed-range benchmark or the higher-priority sync-time work.

## Product Principles

### Presets express intent

An operator chooses a supported workload, and Zinder owns the projection dependencies required to serve it. Operators do not select individual internal consumers or reason about cursor groups, schema recovery, retention floors, and capability gates.

### Projection state stays independent

Every projection keeps its own identity, schema, cursor, recovery coverage, freshness, and retention authority. A preset expands to those identities, but it never replaces their individual lifecycle state with one global status.

### Canonical truth stays shared

The first release does not create wallet-specific canonical schemas or skip canonical transparent facts. Canonical facts remain the common source for wallet reads, complete deployments, future projection workers, and rebuilds.

### Payload retention stays orthogonal

Projection selection does not determine raw block or transaction retention. Compact and full-block wallets may use the same wallet preset while requiring different payload policies.

### Missing work fails explicitly

Zinder must never serve a partial projection as complete, infer that a missing spend means unspent after canonical retention, or advertise a capability from store-wide readiness alone. Availability and freshness are evaluated per projection.

## Actors

- **Wallet operator**: runs Zinder for one or more wallet clients and wants the smallest correct deployment.
- **Shared deployment operator**: runs one Zinder installation for wallets, explorers, analytics, and other products.
- **Wallet integrator**: connects through the native wallet interface or the lightwalletd-compatible interface and depends on truthful capability and readiness reporting.
- **Zinder maintainer**: adds or changes projections without silently expanding the wallet workload.
- **Projection operator**: runs future independently deployed projection workers without changing the meaning of a product preset.

## User Stories

1. As a wallet operator, I want a supported wallet preset, so that I do not materialize explorer and analytics projections that my deployment never serves.
2. As a shared deployment operator, I want a complete preset, so that one installation continues to expose every shipped product projection.
3. As an existing operator, I want the complete preset to preserve current behavior by default, so that an upgrade does not silently remove capabilities.
4. As a lightwalletd-compatible wallet integrator, I want transparent-address transaction history to remain available under the wallet preset, so that transaction enhancement and transparent history remain complete.
5. As a native full-block wallet integrator, I want durable transparent spender resolution under the wallet preset, so that offline recovery can identify old spends after canonical retention.
6. As a compact-block wallet operator, I want transaction payload retention without full block payload retention, so that the deployment supports wallet history without paying for unused full blocks.
7. As a full-block wallet operator, I want to retain full blocks with the same wallet projection preset, so that a Zallet-style backend can scan epoch-pinned blocks.
8. As an operator, I want Zinder to reject incompatible preset and store combinations before mutation, so that a failed start does not leave the store impossible to upgrade or roll back.
9. As an operator, I want a clear rebuild instruction when a requested preset cannot use an existing store, so that recovery is deliberate and predictable.
10. As a wallet integrator, I want capabilities to reflect the required projection and payload availability, so that the client does not discover missing data halfway through sync.
11. As an operator, I want readiness to identify the lagging or unavailable projection, so that I can distinguish wallet correctness from optional product lag.
12. As a Zinder maintainer, I want every new projection to declare its product role, recovery source, preset membership, and retention authority, so that preset behavior cannot drift accidentally.
13. As a Zinder maintainer, I want the wallet and complete workloads measured with the same deterministic replay input, so that the product decision rests on comparable storage and sync evidence.
14. As an operator, I want backup metadata to state which projections were captured and whether they were current, so that restore does not overstate the recovered capability set.
15. As a projection operator, I want future workers to reuse the same projection identities and preset membership, so that moving work out of ingest does not change the product contract.
16. As a developer, I want default local tests to remain embedded and fast, so that optional future databases do not become prerequisites for wallet-preset development.
17. As a release owner, I want compatibility claims separated from method coverage, implemented adapters, historical evidence, and current certification, so that a preset is not mistaken for proof that every named wallet works.
18. As an operator, I want `--print-config`, readiness, and metrics to state the selected preset and effective projection set without exposing secrets, so that the running workload is auditable.

## Scope

In scope:

- Named presets that expand to stable projection identities.
- A wallet preset and a complete preset.
- Explicit projection roles and retention authority.
- Fresh-store selection and fail-closed lifecycle validation.
- Per-projection capability, freshness, and readiness decisions.
- Independent raw payload retention with preset-aware defaults and validation.
- Deterministic workload benchmarks and acceptance evidence.
- Compact in-process RocksDB execution today and compatibility with future projection workers.
- Backup, restore, configuration discovery, metrics, and testing requirements for the selected projection set.

Out of scope:

- A public arbitrary projection allowlist.
- Changing presets on an existing store.
- Automatically deleting disabled projection rows.
- Wallet-specific canonical artifact schemas.
- Skipping canonical transparent outputs or spend facts.
- Lazy projection construction triggered by a query.
- Shipping Postgres, ClickHouse, SQLite, Turso, or a standalone projector as part of the first preset release.
- Treating the preset as wallet certification.

## Projection Roles

| Projection class | Current projection | Product requirement | Canonical retention authority |
| --- | --- | --- | --- |
| Wallet correctness | `transparent_outpoint_spend` | Required by the wallet preset and complete preset | May publish the transparent-spend retention release floor after durable commit |
| Wallet serving | `transparent_address_transaction_history` | Required by the wallet preset and complete preset | None |
| Optional product view | Every other current bundled projection | Required by the complete preset only | None |

New projections must enter one of these classes before they join a preset. A projection does not become wallet-critical because a wallet might display its output; it becomes wallet-critical only when wallet correctness or an advertised compatibility contract fails without it.

## Requirements

### R-PRESET-1. Presets select projection identities

Zinder must expose a closed set of named workload presets. The first set contains `wallet` and `complete`. A preset expands to stable projection identities and their required startup work; it does not expose internal column-family names or consumer implementation types.

### R-PRESET-2. Complete preserves current behavior

`complete` must remain the default for existing configuration that does not select a preset. It includes every shipped projection and preserves the shared-product deployment posture.

### R-WALLET-1. Wallet contains the minimum supported set

`wallet` must include durable transparent outpoint-spend history and transparent-address transaction history. It must omit explorer, analytics, ranking, fee-distribution, value-pool-history, migration, and dashboard projections unless later evidence promotes one into the wallet contract.

### R-ROLE-1. Every projection declares its role

Each projection must have one product role, one recovery-source declaration, one preset-membership decision, and an explicit statement about whether it may acknowledge canonical retention. Adding or changing a role requires product and architecture review.

### R-CANONICAL-1. Presets do not change canonical facts

The first implementation must build the same canonical artifacts for `wallet` and `complete`. Projection selection may change derive writes, replay, startup jobs, and product capabilities, but not canonical epoch atomicity, reorg repair, wallet facts, or event history.

### R-PAYLOAD-1. Raw payload retention remains independent

The raw payload policy remains a separate operator choice:

- `none`: retain no raw block or transaction blobs.
- `transactions`: retain transaction blobs only.
- `all`: retain block and transaction blobs.

Projection selection does not mutate payload retention. A deployment that claims the complete wallet and lightwalletd-compatible contract also selects `ingest.modifiers.coverage = "wallet-serving"`; that coverage profile defaults to `transactions` and rejects `none`. A full-block wallet selects `all`. The complete projection preset does not imply `all`.

### R-READ-1. Reads depend on projection-specific state

Query and compatibility surfaces must evaluate the required projection's availability, schema, cursor, and verified coverage. A generic “derive store online” signal is insufficient when presets omit valid projections deliberately.

### R-CAPABILITY-1. Capabilities describe effective support

Capabilities must be advertised only when their canonical facts, payload retention, projection state, and live writer dependencies satisfy the method contract. A disabled optional projection removes only its capabilities; it does not degrade wallet capabilities that do not depend on it.

### R-LIFECYCLE-1. Initial support is fresh-store only

The first implementation must apply preset selection only when creating a fresh canonical-plus-projection store. A requested preset that conflicts with persisted projection identities, recovery coverage, canonical schema, or retention markers must fail before mutating the projection manifest.

### R-LIFECYCLE-2. Existing-store transitions require a separate design

Wallet-to-complete expansion, complete-to-wallet reduction, and automatic disk reclamation are unsupported until a migration design proves manifest preflight, historical recovery coverage, rollback behavior, and retention safety. The error must direct operators to a supported full rebuild procedure.

### R-OPS-1. The effective workload is observable

Printed configuration, readiness, server information, and metrics must expose the selected preset and effective projection identities. Secrets and raw authorization material remain redacted. Metrics must distinguish canonical ingest from each active projection's replay, writes, compaction, lag, and startup recovery.

### R-BACKUP-1. Backup records projection coverage

A backup or restore manifest must state the selected preset, included projection identities, their schema versions, their projection positions, and whether each projection was exact, behind, or omitted. A restore must not advertise a projection until its recorded state passes normal readiness checks.

### R-BENCHMARK-1. Public selection is evidence-gated

The wallet preset must run against the same captured dense and recent ranges as the complete control. The comparison reports canonical blocks per second, derive rows and bytes written, compaction bytes, peak anonymous resident memory, projection lag, startup recovery time, and final disk use.

The public preset ships only when the benchmark shows a repeatable derive-write reduction outside measured run-to-run noise, plus an improvement in at least one of disk use, compaction, memory, or recovery time. Canonical throughput and wallet correctness must not regress outside the benchmark's stated noise.

### R-FUTURE-1. Presets survive worker extraction

Preset vocabulary and projection identities must remain independent of process placement and storage backend. The compact topology may execute selected projections inside ingest, while a future deployment assigns the same identities to independently fenced projection workers and projection-specific stores.

### R-CLAIM-1. Preset support is not wallet certification

Documentation and release notes must distinguish method coverage, a configured preset, an implemented wallet adapter, historical end-to-end evidence, and current certification. The wallet preset proves that Zinder built the required data products; it does not prove that every wallet release consumes them correctly.

## Testing Decisions

Tests exercise the public workload contract rather than internal consumer arrays.

- Contract tests prove that the effective projection set matches each preset and remains stable across configuration parsing, ingest startup, query startup, backup, and restore.
- Storage tests prove fresh-store initialization, manifest preflight, fail-closed mismatches, and retention-release safety.
- Query tests prove per-projection capability and readiness decisions, including deliberate omission and lag.
- Fixed-range performance tests compare identical inputs under wallet and complete presets.
- Live wallet tests exercise compact sync, transparent history, durable spend recovery, broadcast, mempool behavior, and reorg recovery.
- Full-block wallet tests run with raw payload retention set to `all`.
- Compatibility tests keep support claims scoped to the wallet release and network actually exercised.

## Acceptance Criteria

- A fresh wallet-preset store reaches wallet-serving readiness without creating optional product projection state.
- A fresh complete-preset store preserves the current capability and projection set.
- Transparent-address history and durable spender resolution remain complete under the wallet preset.
- Canonical epoch, reorg, event, and retention contract tests pass under both presets.
- Full-block reads remain unavailable under `transactions` and become available under `all`, independent of projection preset.
- An incompatible existing store fails before its projection manifest changes.
- Capability and readiness output identifies every required missing or lagging dependency.
- Backup and restore preserve or explicitly omit each projection's state.
- The fixed-range evidence satisfies R-BENCHMARK-1 before the preset becomes a supported operator feature.
- The default complete deployment and its existing clients observe no behavior change.

## Open Questions

1. Should the public name be `complete`, `shared`, or `all`? This PRD uses `complete` to avoid confusing projection coverage with full-block payload retention.
2. Which exact compatibility claim requires transaction blobs, and can a narrower native-wallet deployment safely use `none` without creating another public preset?
3. What backup manifest format should become durable before multi-backend projection stores exist?
4. Should a future wallet-to-complete transition backfill in place, or should every preset change continue to require a fresh store?
5. Which numeric improvement threshold, beyond measured run-to-run noise, should release policy require after the first fixed-range control exists?

## Source References

- [Consumer-neutral wallet data plane](../adrs/0005-consumer-neutral-wallet-data-plane.md)
- [Explorer plane as first-class product surface](../adrs/0009-explorer-plane-as-product-surface.md)
- [Derive-consumer template and key-codec convention](../adrs/0017-derive-consumer-template-and-key-codec-convention.md)
- [Capability-gated optional payload fields](../adrs/0018-capability-gated-optional-payload-fields.md)
- [Per-consumer derive schema versioning](../adrs/0028-per-consumer-derive-schema-versioning.md)
- [Durable transparent-outpoint spend projection](../adrs/0029-durable-transparent-outpoint-spend-projection.md)
- [Projection checkpoints and backfill coverage](../adrs/0031-projection-checkpoints-and-backfill-coverage.md)
- [Wallet data plane](../architecture/wallet-data-plane.md)
- [Derive plane](../architecture/derive-plane.md)
- [Integration surfaces](../reference/integration-surfaces.md)
- [Block explorer consumer requirements](block-explorer-consumer.md)

Workspace investigations consulted on 2026-07-12:

- `docs/investigations/2026-07-11-ironwood-deploy-rebuild-metrics.md`
- `docs/investigations/2026-07-11-performance-improvement-backlog.md`
- `docs/investigations/database-adapter-architecture.md`
- `docs/investigations/database-adapter-implementation-plan.md`

The implementation plan carries the durable requirements derived from these gitignored investigation notes.
