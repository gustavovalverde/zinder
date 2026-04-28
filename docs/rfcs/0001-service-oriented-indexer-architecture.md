# RFC-0001: Service-Oriented Indexer Architecture

| Field | Value |
|-------|-------|
| Status | Accepted |
| Created | 2026-04-25 |
| Accepted | 2026-04-28 |
| Product | Zinder |
| Related | PRD-0001: Zinder Indexer |

## Summary

Zinder should be built as one product with separate production services for chain ingestion and query serving. The core architectural decision is to keep the write-owning indexing plane out of the wallet-facing query plane.

The required split is:

- `zinder-ingest`: follows upstream node sources, constructs canonical artifacts, handles reorgs, and writes durable chain state.
- `zinder-query`: serves the native `WalletQuery` wallet and application API from indexed state without following upstream nodes directly.
- `zinder-compat-lightwalletd`: optional adapter that serves vendored lightwalletd-compatible gRPC by translating `WalletQueryApi`.
- `zinder-derive`: optional downstream derived indexes for explorer, analytics, and application-specific materialized views.

A local developer profile may run multiple services in one process. That profile must be a composition layer over the same service boundaries, not a different architecture.

## Context

Zcash indexing has two distinct jobs that often get coupled:

1. Converting upstream node state into durable, queryable chain artifacts.
2. Serving wallets and applications with stable APIs and privacy-aware behavior.

These jobs have different failure modes. Ingestion needs deterministic sync, reorg handling, atomic commits, migrations, and upstream-node recovery. Query serving needs latency, compatibility, privacy boundaries, and independent scale-out. Coupling them in one production binary makes operations simpler at the start but expensive later: read load can interfere with chain commits, migrations become user-visible outages, and derived explorer features can drift into the wallet path.

Other ecosystems point toward the same separation:

- Blockscout supports separate indexer, web, and API modes and keeps automatic chain writes in its indexer application.
- Sui's indexing stack separates checkpoint processing from ingestion sources and combines streaming with polling fallback for correctness.
- Reth Execution Extensions model committed and reverted chains explicitly, which is the right shape for reorg-aware derived data.
- Substreams treats indexing as deterministic transformations with replayable sinks.
- Zcash's wallet threat model treats the lightwalletd server as a privacy boundary, so wallet-facing APIs must be designed as a constrained data plane.

## Goals

- Keep canonical chain writes in one service boundary.
- Serve wallet and application APIs from consistent chain epochs.
- Make reorg handling explicit and testable.
- Support lightwalletd-compatible wallet behavior where required.
- Allow derived indexes without coupling them to wallet sync.
- Let operators scale ingestion and query load independently.
- Make service status observable through health, readiness, and metrics.
- Preserve a clear Zcash node, indexer, and wallet trust boundary.
- Use names and module boundaries that are stable enough for downstream developers to copy.

## Non-Goals

- No wallet custody in the indexer.
- No server-side shielded wallet scanning as a core feature.
- No generic multi-chain framework in v1.
- No explorer UI in v1.
- No production all-in-one binary as the primary architecture.
- No derived-index dependency in the wallet data plane.
- No hidden migrations on service startup.

## Decision

Zinder will use a service-oriented architecture with one write-owning ingestion plane and one read-serving query plane.

```text
                +----------------+
                | Zebra/zcashd   |
                | NodeSource     |
                +-------+--------+
                        |
                        v
                +----------------+
                | zinder-ingest  |
                | chain commits  |
                +-------+--------+
                        |
          +-------------+-------------+
          |                           |
          v                           v
 ChainEpochReadApi          ChainEventEnvelope
          |                           |
          v                           v
+----------------+          +----------------+
| zinder-query   |          | zinder-derive  |
| wallet/API     |          | replayed views |
+----------------+          +----------------+
          |
          v
 zinder-compat-lightwalletd
          |
          v
 CompactTxStreamer
```

`zinder-ingest` owns the canonical commit path. It builds full block artifacts, compact block artifacts, chain metadata, tree state metadata, transaction submission receipts if required, and reorg-window snapshots. The event vocabulary for source observations, committed transitions, and replay streams is defined in [Chain events](../architecture/chain-events.md).

`zinder-query` owns external read APIs. It serves compact block ranges, subtree roots, tree state, latest block metadata, transaction fetches, fee or mempool views, and transaction broadcast endpoints through `ChainEpochReadApi` or through an explicit query-owned store fed by canonical artifacts. In the ADR-0013 topology, `ChainEpochReadApi` is backed by `SecondaryChainStore`. Query must not write canonical chain state, open primary storage, or bypass the epoch-bound read contract.

`zinder-compat-lightwalletd` owns lightwalletd-compatible gRPC behavior. It consumes `WalletQueryApi` and translates requests, responses, streams, and error classes to the vendored `CompactTxStreamer` contract. It must not call upstream nodes, open primary storage, run migrations, or build missing artifacts.

`zinder-derive` owns optional materialized views. It should consume `ChainEventEnvelope` values or immutable snapshots. It must be possible to delete and rebuild derived state without changing canonical ingest state.

## Service Contracts

The owns/must-not-own boundary for each service is the [Boundary Map in Service Boundaries](../architecture/service-boundaries.md#boundary-map). Per-service detail lives in [chain ingestion](../architecture/chain-ingestion.md), [wallet data plane](../architecture/wallet-data-plane.md), [protocol boundary](../architecture/protocol-boundary.md), and [derive plane](../architecture/derive-plane.md). This RFC names the boundaries; the architecture docs own the contracts.

## Workspace Crates

The workspace should keep cross-service code deep and named by domain:

- `zinder-core`: domain types such as `ChainEpoch`, `BlockArtifact`, `CompactBlockArtifact`, `FinalizedHeight`, `ReorgWindow`, and `TransactionId`.
- `zinder-store`: durable storage contracts, fixed key and envelope formats, storage-control protos, migrations, epoch snapshots, and `ChainEpochReader`.
- `zinder-source`: upstream node clients and source adapters.
- `zinder-proto`: generated gRPC types, vendored Zcash wallet protos, internal Zinder protos, and native Zinder protos.
- `zinder-client`: typed Rust client surface for in-process consumers (Zallet) per [M2 spec §D5](../specs/m2-push-primitive.md#d5-typed-rust-client-zinder-client-was-adr-0007). Exports the `ChainIndex` trait, typed `TxStatus`, `TransactionBroadcastResult`, `IndexerError`, and `ChainEpoch`-pinned reads. Wraps `WalletQueryApi` directly without a tonic round-trip.
- `zinder-testkit`: deterministic source, chain, reorg, and wallet API fixtures.

Do not create `zinder-common`, `zinder-utils`, or `zinder-service`. If a cross-service module cannot be named by its domain, the boundary is not understood yet.

## Chain Epoch Model

Every externally visible query that depends on chain state reads from one `ChainEpoch`. The epoch is the consistency boundary; query serving may cache by epoch but must not merge block metadata from one epoch with mempool or compact-block data from another unless the response explicitly marks those fields as independent. Field shape and visibility-pointer semantics are in [Storage backend §Core Contracts](../architecture/storage-backend.md#core-contracts).

## Reorg Model

Reorg handling is a deterministic state machine owned by `zinder-ingest`. The pipeline and invariants live in [Chain ingestion §Operation Shape](../architecture/chain-ingestion.md#operation-shape) and [Chain events §Reorg Replacement Contract](../architecture/chain-events.md#reorg-replacement-contract). Every transition requires tests, and every committed epoch must be internally closed (block links, compact-block artifacts, transaction references, and tree metadata agreeing) before the epoch becomes visible to readers.

## Mempool Model

Mempool state is not canonical chain state. M3 models it as a hybrid in-memory `MempoolIndex` plus a persistent `MempoolEventLog` per [M3 Mempool](../specs/m3-mempool.md); RFC-level invariants:

- Insert-only mempool caches are forbidden.
- Mempool entries never appear finalized without a block commit.
- Query responses must not imply mempool state is as strong as finalized state.
- Reorg interaction follows Zebra's `MempoolChange` stream directly; Zinder does not synthesize mempool entries from reverted blocks.

## Operations Model

Every service exposes typed `/healthz`, `/readyz`, and `/metrics` endpoints; readiness must be a typed cause, not a numeric flag. The full readiness vocabulary, shutdown protocol, configuration rejection rules, and metrics catalog are owned by [Service operations](../architecture/service-operations.md).

## Consequences

Positive consequences:

- Ingestion can be tested as a deterministic state machine.
- Query serving can scale independently.
- Derived indexes can fail or rebuild without corrupting wallet sync.
- Operators can reason about readiness and migrations.
- Downstream developers get stable domain names and API boundaries.

Tradeoffs:

- Local development needs composition tooling.
- Operators need to deploy at least two production processes.
- Shared storage contracts need more discipline than an in-process cache.
- Cross-service versioning must be explicit.

## Open Questions

- Should the first source backend required for v1 be Zebra JSON-RPC, Zebra indexer gRPC, Zebra ReadState, or a capability-gated combination? Capability-gated combination (per [Node source boundary §Capability Model](../architecture/node-source-boundary.md#capability-model) and [Public interfaces §Capability Discovery](../architecture/public-interfaces.md#capability-discovery)); JSON-RPC remains as the universal fallback when streaming is unavailable.
- Should transaction broadcast live in `zinder-query` only, or should it call a narrow `zinder-ingest` network facade? Resolved: broadcast is a `zinder-query` method that delegates to `zinder-source::TransactionBroadcaster`, with a typed `TransactionBroadcastDisabled` for read-only deployments.
- Which Sora/Soramitsu indexer references are available for primary-source comparison?
- Should v1 mempool serving use a snapshot-only surface or a snapshot-plus-events surface? Resolved by [M3 Mempool](../specs/m3-mempool.md): snapshot plus events, as complementary surfaces.
- When should writer fencing or leader election become part of v1 operations?
- When does multi-process query mode replace the M1 in-process `ChainStore` exception? Resolved by [ADR-0013: Multi-process storage access](../adrs/0013-multi-process-storage-access.md): production readers open `SecondaryChainStore` with a process-unique secondary path; subscriptions travel over gRPC.
