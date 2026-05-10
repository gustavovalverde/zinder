# ADR-0014: Compute-At-Read-Time Read-Path Pattern for Canonical Reads

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Product | Zinder |
| Domain | Wallet data plane read-path implementation strategy |
| Related | [Wallet data plane](../architecture/wallet-data-plane.md), [Extending artifacts](../architecture/extending-artifacts.md), [Extending the wallet data plane](../architecture/extending-the-wallet-data-plane.md), [Public interfaces](../architecture/public-interfaces.md), [Closing the Zaino surface gap §G18](../reference/closing-the-zaino-surface-gap.md), [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md), [ADR-0001](0001-rocksdb-canonical-store.md), [ADR-0002](0002-boundary-specific-serialization.md), [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0013](0013-derive-plane-instantiation-and-transparent-address-balance.md) |

## Context

Three storage shapes can serve a typed canonical-chain read whose underlying data is already represented somewhere in the canonical store:

- **Shape A: dedicated column family.** A new RocksDB column family keyed by the read's primary key, carrying a pre-projected payload. Reads are single seeks. Writes happen at ingest time. Every Shape A column family is a commitment of operator storage budget that scales with chain growth.
- **Shape B: extend an existing artifact.** Augment a shipped artifact with new fields (offset tables, per-output projections, side payloads) so the read can slice into it efficiently. Touches every existing artifact write path; forces a storage migration on every shipped store; ties future evolution to the existing artifact's schema.
- **Shape C: compute at read time.** Read an existing canonical artifact's payload bytes and synthesize the typed read-model on the fly. Zero new storage. Read latency is bounded by the parse cost of the underlying artifact (microseconds for typical transactions; low milliseconds for batched reads at the per-request cap).

M5 Slice B introduced Shape C as a one-off: federated `TransparentAddressBalance` reads canonical UTXO artifacts (M4) and M3 mempool point lookups, summing at the gRPC adapter without committing a balance accumulator column family ([ADR-0013 §Storage and read-path](0013-derive-plane-instantiation-and-transparent-address-balance.md#storage-and-read-path)). The decision was framed as a milestone-specific trade-off: the operational cost of a balance-specific column family was unmotivated by the available consumer evidence.

M6 Slice 2 introduces the same pattern for a different surface: `WalletQuery.TransparentPrevouts` deserializes `TransactionArtifact.payload_bytes` via `zinder_source::transparent_prevout_from_raw_transaction_bytes` and indexes into the transaction's `vout` list. The same reasoning applies (no consumer evidence justifies the operational cost of a dedicated `OutPoint`-keyed column family today; the realistic workload is bounded by the per-request cap and the typical transaction size).

Two instances is the threshold the project's lifecycle rule names for ADR promotion: "ADRs lock contracts that have been proven in code" ([docs/README.md §Document lifecycles](../README.md#document-lifecycles)). Without an ADR, every future contributor weighing canonical-vs-derive-vs-compute-at-read-time will rediscover the trade-offs from scratch and risk a different answer for analogous cases. Shipping the pattern as a *named contract* prevents drift and gives future contributors a single place to reason about when to apply it.

The pattern also has a non-obvious *durability property* that needs to be locked: a future promotion from Shape C to Shape A must not change the public contract. If a Shape A landing required a capability-string bump or a wire-shape change, every consumer would have to re-gate or re-deserialize, forcing operator-side coordination that has nothing to do with the storage decision. The contract surface and the storage shape must be independently evolvable.

## Decision

The compute-at-read-time pattern is the default starting shape for new typed wallet-plane reads whose data is already in the canonical store. A dedicated column family (Shape A) is reserved as a future read-path optimization, promoted only when telemetry or a specific consumer proves Shape C cannot meet the latency budget. The public wire shape and the capability string never depend on the storage shape.

### When the pattern applies

The pattern applies when *every* condition holds:

1. The read's primary input is already a canonical artifact (a row already written by `zinder-ingest`). New artifact families are out of scope; those follow [Extending artifacts](../architecture/extending-artifacts.md).
2. The read can be expressed as a deterministic function of one or a bounded number of canonical artifacts plus typed request parameters. The function's runtime is bounded by parse cost or by mempool-index lookup cost; it is not an aggregation that scans an unbounded number of rows.
3. The response binds to a single `ChainEpoch` (canonical) or to the writer's chain epoch visible at lookup time (mempool); cross-epoch synthesis is forbidden ([Wallet data plane §Query Consistency](../architecture/wallet-data-plane.md#query-consistency)).

The pattern does not apply to:

- Aggregations that fan out across many addresses or transactions per request without a bounded request shape. Those belong on the derive plane ([Derive plane](../architecture/derive-plane.md)).
- Reads whose underlying data has not been written by ingest. Those need a new artifact family first ([Extending artifacts](../architecture/extending-artifacts.md)).
- Cross-chain or cross-epoch synthesis that mixes data from two visible epochs. The response-enrichment rule forbids this ([extending-the-wallet-data-plane.md §Response enrichment rule](../architecture/extending-the-wallet-data-plane.md#response-enrichment-rule)).

### Storage-shape ladder

A new method authored under this pattern starts at Shape C and may, when telemetry justifies it, promote to Shape A without breaking the public contract.

#### Shape C: compute at read time (the default)

The handler reads existing canonical artifacts, parses their payload bytes if needed, and assembles the typed response in process. The parse helper lives in `zinder-source` (the boundary that owns Zebra-type translation) so the rest of the read path stays in `zinder-core` vocabulary. Examples:

- M5 Slice B: `services/zinder-derive/src/grpc/adapter.rs::compute_transparent_address_balance` reads canonical UTXO artifacts and M3 mempool point lookups via `WalletQuery`.
- M6 Slice 2: `WalletQueryApi::transparent_prevouts` reads `TransactionArtifact` and calls `zinder_source::transparent_prevout_from_raw_transaction_bytes`.

#### Shape B: extend an existing artifact

Reserved for cases where Shape C cannot meet the latency budget *and* a column family addition would commit substantial mainnet storage. Adding fields to an existing artifact is forbidden when those fields can be derived from the artifact's payload at read time; the only legitimate Shape B case is when the per-read parse cost dominates and a lightweight side projection (e.g. an offset table inside the existing artifact) closes the gap. No shipped Zinder method uses Shape B today.

#### Shape A: dedicated column family

A new column family keyed by the read's primary key, carrying a typed payload, written at ingest time. Promoted only when:

- Telemetry shows Shape C parse cost crossing the deployment's latency budget (sub-millisecond for typical reads, single-digit milliseconds for batched reads at the per-request cap), AND
- A real consumer's workload is materially affected (not a synthetic benchmark or a hypothetical workload), AND
- The storage cost of the column family is justified against the gain.

When Shape A lands, the public wire shape and the capability string are unchanged. The Shape C path becomes a fallback for cache misses (or is removed once the column family is fully populated). The migration is operator-side only: existing stores re-bootstrap to populate the new column family per [M4 §D11](../specs/m4-transparent-address.md) precedent.

### Public-contract invariance

A Shape A promotion *must not*:

- Bump the capability string. The capability identifies the surface, not the implementation. Bumping it would force every consumer to re-gate and re-test for a change that does not affect them.
- Change the response message. Wire-shape changes require their own ADR and follow the deprecation contract in [Public interfaces §Capability discovery](../architecture/public-interfaces.md#capability-discovery).
- Change the request message. Same reasoning.
- Change the `ChainIndex` trait method signature. Same reasoning.

The CI gate is the capability-coverage test (`crates/zinder-client/tests/integration/capability_coverage.rs`) plus the proto round-trip tests in `crates/zinder-proto/tests/integration/`. A Shape A landing whose diff touches `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` for the affected RPC's request or response message is automatically rejected as out-of-scope.

### Source-layer parsing helpers

When a method needs to parse Zebra-type bytes at read time, the parse helper lives in `zinder-source` (not in `zinder-core`, not inline in the handler). The pattern is the same across instances:

- `zinder_source::block_header_info_from_raw_block_bytes` (block headers)
- `zinder_source::transparent_prevout_from_raw_transaction_bytes` (transparent outputs at a given index)

Future helpers follow the same shape: take raw bytes plus a typed selector (height, output index, etc.), return `Result<TypedZinderShape, SourceError>`. The `SourceError` variant gains a new `Raw{Thing}ParseFailed { reason }` arm. The handler maps the error to `QueryError::ArtifactCorrupt { family, reason }` for canonical reads or to `IndexerError::malformed` for the `LocalChainIndex` Rust path. This keeps Zebra-type vocabulary out of `zinder-core` and out of the wallet API surface.

### Per-request batching cap

Methods that batch under this pattern share a per-request cap of `MAX_TRANSPARENT_PREVOUTS_PER_REQUEST = 256` outpoints (or the analog for the method's primary input). Requests above the cap are silently truncated to the first N entries. The cap mirrors the M5 balance address cap so DX is uniform across batched wallet-plane reads.

### Anti-pattern refusals

Methods authored under this pattern refuse the same anti-patterns the rest of the wallet data plane refuses:

- **A1 (verbosity integer)** and **A2 (verbosity boolean)**: the typed return shape is the canonical shape; there is no flag to switch between compact and full responses.
- **A4 (sentinel-overloaded inputs)**: the request validates inputs at the wallet adapter boundary. The coinbase sentinel outpoint is rejected with `INVALID_ARGUMENT` at the wallet adapter; consumers filter sentinels at the request boundary.
- **A5 (`zaino_proto::*` types on the Rust API)**: the `ChainIndex` trait takes and returns only `zinder-core` types. The parse helper in `zinder-source` is the boundary where Zebra-type vocabulary terminates.

## Consequences

- New typed wallet-plane reads start with zero storage cost and can ship in one pass through the cookbook.
- Future contributors weighing storage-vs-compute trade-offs have a named contract to reference, not an ad-hoc decision per method.
- Shape A column families are still available when consumer evidence justifies them; the durability invariant ensures the upgrade is operator-side only.
- The `zinder-source` parse-helper layer continues to grow as new methods land; each helper follows the same shape, which keeps the boundary clean.
- The pattern does not apply to derive-plane reads or to reads whose underlying data is not yet a canonical artifact; those routes remain governed by [Extending artifacts](../architecture/extending-artifacts.md) and [Derive plane](../architecture/derive-plane.md).

## Alternatives considered

**Always ship Shape A.** Reject. Commits operator storage budget for every new method, regardless of consumer evidence. Re-introduces [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md) ("Storage as a Linear Migration Ladder") under a new name.

**Always ship Shape C.** Reject. Some future workload (a high-traffic public Zinder serving an explorer's per-tx page) may require sub-microsecond per-call. Without a documented promotion path, a Shape A landing would look like a wire-shape change instead of a read-path optimization.

**Make storage shape part of the capability string.** Reject. Couples consumer code to implementation; every storage migration becomes a wire-shape migration. Defeats the entire point of capability strings as exact-match contracts.

**Add Shape B (extend existing artifact) as the default for parse-bound reads.** Reject. Storage migrations on shipped artifacts are operationally expensive; the typical parse cost (microseconds) is well within the deployment's latency budget for the realistic workload (admin-style RPC, batched at the per-request cap). Reserved as a narrow fallback when Shape C and Shape A both fail.

## Worked examples

- **M5 Slice B**: `WalletQuery.TransparentAddressBalance` (federated). Shape C reads canonical UTXOs and M3 mempool point lookups at request time. Shape A (per-block accumulator column family) is reserved.
- **M6 Slice 2**: `WalletQuery.TransparentPrevouts` (canonical, direct). Shape C reads `TransactionArtifact.payload_bytes` and parses via `zinder_source::transparent_prevout_from_raw_transaction_bytes`. Shape A (dedicated `OutPoint`-keyed column family) is reserved.
- **M6 Slice 3**: `WalletQuery.TransparentMempoolPrevouts` (live mempool). Reads `MempoolEntry.transparent_outputs` directly; no parsing because the mempool ingest path pre-extracts transparent outputs at admission time. This is a degenerate Shape C (the work is done at ingest, not at read).

## Cross-references

- [Wallet data plane](../architecture/wallet-data-plane.md): the architecture doc this ADR makes durable.
- [Extending the wallet data plane](../architecture/extending-the-wallet-data-plane.md): the cookbook for new typed reads. Worked Example 4 is the M6 prevout pair under this pattern.
- [Extending artifacts](../architecture/extending-artifacts.md): the companion cookbook for new artifact families (the case this ADR is *not* about).
- [Public interfaces §Capability discovery](../architecture/public-interfaces.md#capability-discovery): the capability-string contract this ADR's invariance rule depends on.
- [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md): the upstream anti-pattern this ADR avoids.
- [ADR-0001](0001-rocksdb-canonical-store.md): the canonical RocksDB store this ADR's Shape A column families would extend.
- [ADR-0002](0002-boundary-specific-serialization.md): the boundary-specific serialization rule the parse helpers in `zinder-source` honor.
- [ADR-0003](0003-canonical-storage-access-boundary.md): the epoch read API every Shape C handler uses.
- [ADR-0013 §Storage and read-path](0013-derive-plane-instantiation-and-transparent-address-balance.md#storage-and-read-path): the first instance of the pattern; this ADR generalizes the M5 design note into a workspace-wide contract.
