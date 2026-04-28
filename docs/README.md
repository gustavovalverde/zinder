# Zinder Documentation

This documentation set defines Zinder's product scope, service boundaries, and vocabulary for a service-oriented Zcash indexer.

## Product and architecture

- [Product requirements](prd-0001-zinder-indexer.md)
- [RFC-0001: Service-Oriented Indexer Architecture](rfcs/0001-service-oriented-indexer-architecture.md)

## Architecture

- [Service boundaries](architecture/service-boundaries.md)
- [Storage backend](architecture/storage-backend.md)
- [Chain ingestion](architecture/chain-ingestion.md)
- [Chain events](architecture/chain-events.md)
- [Node source boundary](architecture/node-source-boundary.md)
- [Protocol boundary](architecture/protocol-boundary.md)
- [Wallet data plane](architecture/wallet-data-plane.md)
- [Derive plane](architecture/derive-plane.md)
- [Service operations](architecture/service-operations.md)
- [Public interfaces](architecture/public-interfaces.md)
- [Extending artifacts](architecture/extending-artifacts.md)

## ADRs

- [ADR-0001: Use RocksDB for canonical storage](adrs/0001-rocksdb-canonical-store.md)
- [ADR-0002: Use boundary-specific serialization](adrs/0002-boundary-specific-serialization.md)
- [ADR-0003: Use an epoch read API for canonical storage access](adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0004: Separate node sources from protocol surfaces](adrs/0004-node-source-and-protocol-boundaries.md)
- [ADR-0005: Use event sequence in chain event cursors](adrs/0005-chain-event-cursor-sequence.md)
- [ADR-0006: Test tiers and unified live-test config](adrs/0006-test-tiers-and-live-config.md)
- [ADR-0007: Multi-process storage access](adrs/0007-multi-process-storage-access.md)
- [ADR-0008: Consumer-neutral wallet data plane](adrs/0008-consumer-neutral-wallet-data-plane.md)

## Reference

Living external references that constrain Zinder's design. Refreshed as the upstream world changes:

- [Lessons from Zaino](reference/lessons-from-zaino.md): prior-art lessons from Zaino's public tracker and how they inform Zinder's product guarantees.
- [Serving Zebra and Zallet](reference/serving-zebra-and-zallet.md): the integration audit between the upstream node and full-node wallet.
- [Findings from Android wallet integration](reference/android-wallet-integration-findings.md): observed behavior of `zcash-android-wallet-sdk` against `zinder-compat-lightwalletd`.

## Specs (in flight)

Mutable working documents for un-shipped multi-PR work. When a spec's work lands, its locked decisions promote to one or more ADRs and the spec is deleted.

- [M3 mempool](specs/m3-mempool.md): mempool source hydration, live index, event log, native wallet API, lightwalletd compatibility, typed Rust client, and product validation gates.

## Vocabulary and naming rules

See [Public interfaces](architecture/public-interfaces.md) for the canonical naming rules, type conventions, error vocabulary, and config field shapes.

## Document lifecycles

Each tree under `docs/` has its own retire-on-ship rule.

- **Architecture** (`docs/architecture/`): the durable spine. Explains why each contract exists, what its invariants are, and where its boundary lives. Edited in place when contracts change. Architecture docs reference other architecture docs and at most one ADR per topic.
- **ADRs** (`docs/adrs/`): record of accepted design decisions in present tense. Edited in place when the decision rationale needs clarification; substantive design changes get a new ADR with a contiguous number. ADRs reference architecture docs (up); they do not reference each other to "explain context" (that role belongs to the architecture doc).
- **Specs** (`docs/specs/`): mutable working documents for un-shipped multi-PR work. Pre-decision drafts go here; ADRs do not. When a spec's work lands, decisions promote to one or more ADRs and the spec is deleted.
- **Reference** (`docs/reference/`): living external constraints. Anti-pattern catalogs, integration requirements, upstream surface audits. Refreshed as the upstream world changes; never describes Zinder's own contracts.

Removed by design (not used in this repo):

- **`docs/plans/`**: working drafts go in `docs/specs/` instead.
- **`docs/research/`**: pre-decision rationale lives in the resulting ADR's Context section. Living external references go in `docs/reference/`.
