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
- [Extending the wallet data plane](architecture/extending-the-wallet-data-plane.md)

## ADRs

- [ADR-0001: Use RocksDB for canonical storage](adrs/0001-rocksdb-canonical-store.md)
- [ADR-0002: Use boundary-specific serialization](adrs/0002-boundary-specific-serialization.md)
- [ADR-0003: Use an epoch read API for canonical storage access](adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0004: Separate node sources from protocol surfaces](adrs/0004-node-source-and-protocol-boundaries.md)
- [ADR-0005: Use event sequence in chain event cursors](adrs/0005-chain-event-cursor-sequence.md)
- [ADR-0006: Test tiers and unified live-test config](adrs/0006-test-tiers-and-live-config.md)
- [ADR-0007: Multi-process storage access](adrs/0007-multi-process-storage-access.md)
- [ADR-0008: Consumer-neutral wallet data plane](adrs/0008-consumer-neutral-wallet-data-plane.md)
- [ADR-0009: IngestControl transport security](adrs/0009-ingest-control-transport-security.md)
- [ADR-0010: Mempool topology and retention](adrs/0010-mempool-topology-and-retention.md)
- [ADR-0011: Derive-plane federation pattern](adrs/0011-derive-plane-federation-pattern.md)
- [ADR-0012: Consumer-release certification tier](adrs/0012-consumer-release-certification.md)
- [ADR-0013: Derive-plane instantiation and transparent address balance read-path](adrs/0013-derive-plane-instantiation-and-transparent-address-balance.md)
- [ADR-0014: Compute-at-read-time read-path pattern for canonical reads](adrs/0014-compute-at-read-time-canonical-reads.md)

## Reference

Living external references that constrain Zinder's design. Refreshed as the upstream world changes:

- [Lessons from Zaino](reference/lessons-from-zaino.md): prior-art lessons from Zaino's public tracker and how they inform Zinder's product guarantees.
- [Serving Zebra and Zallet](reference/serving-zebra-and-zallet.md): the integration audit between the upstream node and full-node wallet.
- [Findings from Android wallet integration](reference/android-wallet-integration-findings.md): observed behavior of `zcash-android-wallet-sdk` against `zinder-compat-lightwalletd`.
- [Serving public lightwalletd clients](reference/serving-public-lightwalletd-clients.md): operator gap analysis vs. community-run servers like `zec.rocks` and the deployment recipe to match them.
- [Closing the Zaino surface gap](reference/closing-the-zaino-surface-gap.md): cross-consumer gap inventory of what Zinder still needs to ship before Zaino consumers (Zallet, Zashi/Zodl, public lightwalletd, explorers) can replace Zaino without a parity regression.

## Runbooks

Operational procedures for running Zinder against the workspace and external systems. Edited in place when the procedure changes; never describes architectural intent (that role belongs to the architecture docs).

- [Testing](runbooks/testing.md): T0–T3 test tiers, the default validation gate, live regtest/testnet/mainnet sweeps, end-to-end runs against Zallet and the Android SDK through the lightwalletd compat shim, native `WalletQuery` smoke tests via `grpcurl`, and a failure-interpretation reference.

## Specs (in flight)

Mutable working documents for un-shipped multi-PR work. When a spec's work lands, its locked decisions promote to one or more ADRs and the spec is deleted.

- [M4: Transparent-address artifact surface](specs/m4-transparent-address.md): native `WalletQuery.TransparentAddressUtxos[Stream]` and `TransparentAddressTxIdsInRange`, the matching `ChainIndex` methods, lightwalletd `GetTaddressTxids` / `GetTaddressTransactions`, and the new tx-history canonical artifact family.

Recently retired specs and where their decisions live:

- **M5 transparent-address balance + derive-plane instantiation**: split across [ADR-0011](adrs/0011-derive-plane-federation-pattern.md) (federation primitive), [ADR-0013](adrs/0013-derive-plane-instantiation-and-transparent-address-balance.md) (operational topology, consumer SDK contract, balance wire shape), and [ADR-0014](adrs/0014-compute-at-read-time-canonical-reads.md) (the compute-at-read-time pattern the balance handler uses).
- **M6 prevout resolution**: locked into [ADR-0014: Compute-at-read-time read-path pattern for canonical reads](adrs/0014-compute-at-read-time-canonical-reads.md).
- **M3 mempool**: locked into [ADR-0010: Mempool topology and retention](adrs/0010-mempool-topology-and-retention.md), with cursor-format coverage in [ADR-0005](adrs/0005-chain-event-cursor-sequence.md) and ingest-control transport security in [ADR-0009](adrs/0009-ingest-control-transport-security.md).

## Vocabulary and naming rules

See [Public interfaces](architecture/public-interfaces.md) for the canonical naming rules, type conventions, error vocabulary, and config field shapes.

## Document lifecycles

Each tree under `docs/` has its own retire-on-ship rule.

- **Architecture** (`docs/architecture/`): the durable spine. Explains why each contract exists, what its invariants are, and where its boundary lives. Edited in place when contracts change. Architecture docs reference other architecture docs and at most one ADR per topic.
- **ADRs** (`docs/adrs/`): record of accepted design decisions in present tense. Edited in place when the decision rationale needs clarification; substantive design changes get a new ADR with a contiguous number. ADRs reference architecture docs (up); they do not reference each other to "explain context" (that role belongs to the architecture doc).
- **Specs** (`docs/specs/`): mutable working documents for un-shipped multi-PR work. Pre-decision drafts go here; ADRs do not. When a spec's work lands, decisions promote to one or more ADRs and the spec is deleted.
- **Reference** (`docs/reference/`): living external constraints. Anti-pattern catalogs, integration requirements, upstream surface audits. Refreshed as the upstream world changes; never describes Zinder's own contracts.
- **Runbooks** (`docs/runbooks/`): operational procedures with explicit prereqs, command lines, and expected outcomes. Edited in place as procedures evolve; reference architecture docs and ADRs (up) but do not describe architectural intent.

Removed by design (not used in this repo):

- **`docs/plans/`**: working drafts go in `docs/specs/` instead.
- **`docs/research/`**: pre-decision rationale lives in the resulting ADR's Context section. Living external references go in `docs/reference/`.
