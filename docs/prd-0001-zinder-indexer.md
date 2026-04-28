# Product Requirements: Zinder

| Field | Value |
|-------|-------|
| Status | Draft |
| Created | 2026-04-25 |
| Product | Zinder |
| Audience | Zcash infrastructure maintainers, wallet developers, explorer developers, indexer operators, and contributors |
| Related | RFC-0001: Service-Oriented Indexer Architecture |

## Problem Statement

Zcash needs an indexer architecture that can serve modern wallets, explorers, and application backends without forcing the Zcash node, indexer, wallet-facing API, and derived analytics into one tightly coupled runtime. Existing systems show the required capabilities: lightwalletd provides a wallet-oriented compact block API, Zaino moves Zcash indexing into Rust and integrates with Zebra, and other ecosystems demonstrate service splits between ingestion, query APIs, and derived indexes.

The main product risk is architectural entropy. If ingestion, compact block construction, wallet APIs, mempool views, migrations, and explorer-derived data all grow inside one binary with shared mutable state, the result will be hard to operate, hard to test, and hard for downstream developers to integrate. The architecture must make the first split correctly: chain ingestion and wallet-facing query serving are separate planes.

## Solution

Zinder is one product composed of independently deployable services and workspace crates.

The minimum production architecture has two services:

- `zinder-ingest`: the write-owning chain ingestion plane. It follows one or more upstream node sources, builds canonical chain artifacts, handles reorgs, maintains the finalized store and reorg window, and publishes queryable epochs.
- `zinder-query`: the read-serving data plane. It exposes wallet-facing and application-facing APIs from epoch-bound indexed state. It does not follow upstream nodes directly, does not perform chain commits, does not own migrations, and does not open live canonical storage as primary or bypass `ChainEpochReadApi` in production.

The planned extension service is:

- `zinder-derive`: a replayable downstream derived-index plane for explorer, analytics, compliance, or ecosystem-specific views. It consumes canonical artifacts from `zinder-ingest` and can be rebuilt without changing the canonical ingest path.

The compatibility service is:

- `zinder-compat-lightwalletd`: an adapter over `WalletQueryApi` that serves the
  vendored lightwalletd-compatible `CompactTxStreamer` contract. It translates
  requests, responses, streams, and error classes; it does not call upstream nodes,
  open canonical storage directly, run migrations, or build compact blocks.

Zinder should also include workspace crates for domain types, storage contracts, protocol definitions, and upstream node clients. These crates exist to keep service boundaries small, not to create a generic framework with unclear ownership.

## User Stories

1. As a wallet developer, I want a stable light-client API, so that my wallet can sync without parsing upstream node internals.
2. As a wallet developer, I want compact blocks served from a consistent chain epoch, so that a sync batch does not mix two competing tips.
3. As a wallet developer, I want subtree roots and tree state served with the same epoch consistency as compact blocks, so that modern wallet scanning can batch work safely.
4. As a wallet developer, I want a lightwalletd-compatible read-sync surface, so that existing wallet clients can migrate before adopting native Zinder APIs.
5. As a wallet developer, I want transaction submission to be available through the same query plane, so that wallet integration has one network dependency.
6. As a privacy-focused wallet developer, I want Zinder to avoid server-side wallet scanning with user secrets, so that users keep spending keys and viewing keys out of the indexer.
7. As an explorer developer, I want finalized block and transaction data through stable APIs, so that explorer rendering does not depend on upstream node RPC quirks.
8. As an explorer developer, I want derived indexes to be replayable, so that schema mistakes can be corrected without corrupting canonical chain state.
9. As an application developer, I want predictable API names and typed responses, so that integrating Zinder does not require reading the internal pipeline.
10. As a node operator, I want ingestion and query services to scale independently, so that read load cannot starve chain indexing.
11. As a node operator, I want explicit readiness states, so that load balancers do not send users to a service that is syncing, migrating, or missing an upstream node.
12. As a node operator, I want startup to fail on invalid production configuration, so that placeholder credentials, unsafe binds, and missing storage do not ship silently.
13. As a maintainer, I want a single chain ingestion state machine, so that reorg behavior is testable and does not depend on which API started first.
14. As a maintainer, I want durable commits to be atomic, so that a crash cannot publish half of a block artifact set.
15. As a maintainer, I want migrations to be explicit and resumable, so that operators can understand downtime and recovery.
16. As a contributor, I want public types named after domain concepts, so that I can navigate the codebase with search instead of tribal knowledge.
17. As a contributor, I want service boundaries documented before code exists, so that new modules do not drift into generic `service`, `manager`, or `utils` buckets.
18. As a node implementation maintainer, I want Zinder to isolate source adapters, so that Zebra ReadState, JSON-RPC, and future streaming sources can evolve without rewriting query APIs.
19. As an SRE, I want metrics for chain lag, commit latency, reorg depth, query latency, DB health, and API error classes, so that failures can be detected before user reports.
20. As a release engineer, I want compatibility tests against lightwalletd-style clients, so that existing wallets can migrate without hidden behavior changes.
21. As a protocol engineer, I want canonical artifacts versioned, so that consensus or network-upgrade changes do not produce ambiguous stored records.
22. As a product owner, I want Zinder to be one product with multiple deployable services, so that the operator experience is coherent while the architecture remains separable.
23. As a wallet developer, I want a queryable mempool with typed `Added`, `Invalidated`, and `Mined` events, so that zero-confirmation transparent flows, rebroadcast detection, and chain-tip-change notifications work without parsing error messages or waiting for stream-close events.
24. As an integrator (developer or LLM agent), I want every Zinder gRPC service to expose a `ServerInfo` capability descriptor, so that I can detect supported features (mempool, transparent address surface, ChainEvents Tip vs Finalized varieties) at runtime instead of pinning to a Zinder version.
25. As an in-process Rust consumer (Zallet, future SDK integrations), I want a `zinder-client` crate with the typed `ChainIndex` trait, so that I integrate against typed `BlockHeight`, `ChainEpoch`, `TxStatus`, and `IndexerError` values without going through tonic-over-localhost.

## Implementation Decisions

- Zinder should begin as a service-oriented Rust workspace unless implementation research finds a stronger reason to choose another language. Rust is the natural fit for Zcash ecosystem reuse, Zebra adjacency, memory safety, async networking, and protocol-level correctness.
- `zinder-ingest` owns all writes to canonical chain storage.
- Canonical chain storage uses RocksDB through `zinder-store`.
- Storage and cursor bytes follow boundary-specific serialization: fixed layouts for ordered keys, artifact envelopes, and cursors; protobuf for protocol payloads and durable storage-control records; `postcard` only for Rust-owned non-durable binary.
- `zinder-query` reads canonical chain state through `ChainEpochReadApi`, backed by `SecondaryChainStore` in the ADR-0007 colocated topology, or through an explicit query-owned store fed by canonical artifacts in a future scale-out topology. It must not open the live canonical RocksDB database as primary or bypass the epoch-bound read contract. Transaction broadcast is allowed only as an explicit network operation that does not mutate canonical indexed state.
- The native protobuf service is named `WalletQuery`; the Rust query boundary is
  `WalletQueryApi`. The published Rust client surface for in-process consumers
  is the `ChainIndex` trait in `zinder-client`. Avoid `WalletApi` and
  `zinder-serve`.
- `zinder-compat-lightwalletd` consumes `WalletQueryApi` and serves
  `CompactTxStreamer`. It is the migration path for existing wallet clients, not
  the owner of storage, upstream node clients, migrations, or compact-block construction.
- `zinder-derive` is optional in v1 and must consume canonical artifacts rather than upstream node RPCs directly.
- A local development command may compose ingest and query in one process, but production documentation and configuration must keep them as separate services.
- Committed chain transitions and replay streams use the `ChainEvent` and `ChainEventEnvelope` vocabulary. Source-observation event types land with the streaming follower; until then, ingest drives the source synchronously.
- The canonical read boundary is a `ChainEpoch`, identified by tip hash, tip height, finalized height, and artifact schema version.
- Query responses that require chain consistency must read from one epoch. Mixing latest values from different epochs is a correctness bug.
- The reorg window must be explicit. Finalized data and non-finalized data must have separate storage contracts.
- Mempool state must be epoch-bound or event-bound. Insert-only mempool caches are not sufficient.
- The first product milestone is shielded wallet read sync: parser-backed compact
  blocks, latest block metadata, tree state, latest tree state, subtree roots,
  lightd info, and lightwalletd-compatible range serving from one chain epoch.
  Android/Zashi serving claims are stricter and are owned by
  [Wallet data plane §External Wallet Compatibility Claims](architecture/wallet-data-plane.md#external-wallet-compatibility-claims).
- The second milestone is chain-event replay, typed broadcast, and the typed Rust consumer surface: `WalletQuery.ChainEvents`, `WalletQuery.BroadcastTransaction`, the `zinder-client` Rust crate exporting `ChainIndex` (see [Wallet data plane](architecture/wallet-data-plane.md) and [Chain events](architecture/chain-events.md)). Together these close the largest cluster of Zallet integration TODOs and validate the central architectural bet (atomic snapshot plus durable chain notifications) end-to-end.
- The third milestone is the queryable mempool surface ([M3 Mempool](specs/m3-mempool.md)): `WalletQuery.MempoolSnapshot`, `WalletQuery.MempoolEvents`, in-memory `MempoolIndex`, and persistent `MempoolEventLog`. Closes Zallet `#139`, `#403`, and the chain-tip-change workaround. Capability-gated source: streaming when Zebra `--features indexer` is detected, polling otherwise. Product consumer roles for Zallet, Zashi/Zodl, lightwalletd clients, Zebra, Zaino, and explorers are canonical in [Wallet data plane §Mempool Snapshot and Subscription](architecture/wallet-data-plane.md#mempool-snapshot-and-subscription-m3).
- The fourth milestone is the transparent-address artifact surface following the [extending-artifacts cookbook](architecture/extending-artifacts.md): paginated `GetTaddressTxids`-equivalent and `GetAddressUtxos`-equivalent native methods, end-to-end through the new artifact-family seam. The minimal `GetAddressUtxos[Stream]` path may be pulled forward when Zashi compatibility is the release target; full transparent history and balance remain a broader transparent-address milestone. Proves that adding a canonical artifact family does not require central enum edits and unblocks t-address wallets and explorers.
- The compact block builder is part of ingestion because it derives wallet artifacts from chain data. The wallet data plane serves those artifacts; it does not build them on demand from arbitrary upstream node reads.
- The compact block builder uses maintained Zcash consensus parsing primitives
  rather than a new hand-rolled parser. `zebra-chain` is the Zebra-aligned
  dependency for source metadata and compact-block construction. Any temporary
  dependency workaround required by Zebra's transitive graph must be explicit in
  the root manifest and dependency policy.
- Server-side wallet scanning, viewing-key custody, spending-key custody, and address-history expansion for shielded users are out of scope for the core indexer.
- Derived explorer or analytics indexes must be rebuilt from canonical artifacts and must not become a hidden dependency of wallet sync.
- Public API names must prefer concrete nouns: `Indexer`, `NodeSource`, `ChainEpoch`, `ChainEpochReader`, `ChainEpochReadApi`, `ChainEvent`, `BlockArtifact`, `CompactBlockArtifact`, `FinalizedBlockStore`, `ReorgWindow`, `WalletQuery`, and `WalletQueryApi`.
- Avoid generic names such as `Service`, `Manager`, `Processor`, `Handler`, `common`, `shared`, and `utils` in public APIs and file names.

## Testing Decisions

- Test external behavior and invariants, not internal task layout.
- Ingestion tests must cover initial sync, restart from partial sync, source disconnect, reorg inside the non-finalized window, reorg beyond the supported window, mempool add/remove, and duplicate block delivery.
- Storage tests must verify atomic commits, migration refusal without an explicit migration mode, snapshot consistency, schema-version checks, and crash recovery.
- Query tests must verify that each response is served from one `ChainEpoch`.
- Compatibility tests must compare wallet-facing behavior against lightwalletd-compatible expectations for compact blocks, block ranges, latest block metadata, tree state, subtree roots, lightd info, and transaction submission.
- Android SDK/Zashi compatibility tests must satisfy
  [Wallet data plane §External Wallet Compatibility Claims](architecture/wallet-data-plane.md#external-wallet-compatibility-claims).
- The first shielded wallet-sync acceptance test must prove that a
  lightwalletd-compatible client can scan a regtest range from Zinder without
  sending viewing keys or spending keys to Zinder.
- Operations tests must exercise `/healthz`, `/readyz`, `/metrics`, startup phase transitions, production config rejection, and graceful shutdown.
- Derived-index tests must prove replayability from canonical artifacts and must verify that derived-index failures do not corrupt canonical chain state.

## Out of Scope

- Zinder v1 is not a wallet.
- Zinder v1 must not custody user spending keys, viewing keys, seed phrases, or wallet secrets.
- Zinder v1 must not make wallet privacy depend on server-side scanning of shielded addresses.
- Zinder v1 does not need a full explorer UI.
- Zinder v1 does not need a generic multi-chain indexing framework.
- Zinder v1 does not need a distributed query network, token incentives, or hosted marketplace.
- Zinder v1 should not implement analytics-specific derived indexes until the canonical artifact and query boundaries are stable.

## Further Notes

The Sora/Soramitsu reference remains a research input, not an architectural dependency. Public research found high-level evidence of Soramitsu blockchain products and external claims about custom indexing services, but not enough primary technical detail to copy a concrete architecture. Before implementation, maintainers should map any internal Sora indexer documentation or code examples into the research survey and explicitly decide which patterns apply to Zcash.
