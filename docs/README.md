# Zinder Documentation

This documentation set defines Zinder's product scope, service boundaries, and vocabulary for a service-oriented Zcash indexer.

## Product and architecture

- [What Zinder is and is not](architecture/indexer-wallet-boundary.md): the first link new integrators should follow.

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
- [ADR-0003: Use epoch-bound storage access with RocksDB secondaries](adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0004: Separate node sources from protocol surfaces](adrs/0004-node-source-and-protocol-boundaries.md)
- [ADR-0005: Consumer-neutral wallet data plane](adrs/0005-consumer-neutral-wallet-data-plane.md)
- [ADR-0006: IngestControl transport security](adrs/0006-ingest-control-transport-security.md)
- [ADR-0007: Mempool topology and retention](adrs/0007-mempool-topology-and-retention.md)
- [ADR-0008: Per-network consensus parameters discovered from the running node](adrs/0008-network-parameter-discovery.md)

## Reference

Current integration references and API support material:

- [Server-side wallet pattern](reference/server-side-wallet-pattern.md): the canonical recipe for building a server-side Zcash wallet on Zinder + librustzcash.
- [Integration surfaces](reference/integration-surfaces.md): supported client and operator integration paths.
- [Error vocabulary](reference/error-vocabulary.md): every `ErrorReason` value, its gRPC `Status` code, and the retry policy clients should follow.

## Runbooks

Operational procedures for running Zinder against the workspace and external systems. Edited in place when the procedure changes; never describes architectural intent (that role belongs to the architecture docs).

- [Testing](runbooks/testing.md): T0-T3 test tiers, the default validation gate, consumer parity checks, live sweeps, native `WalletQuery` smoke tests via `grpcurl`, and a failure-interpretation reference.
- [Deploying on a VM](runbooks/deploying-on-a-vm.md): Compose + systemd for self-hosted single-VM deployments.
- [Deploying on Railway](runbooks/deploying-on-railway.md): single-container image on Railway / Fly.io / Render-style PaaS targets.

## Current Contracts

- **Transparent-address artifact surface**: [Wallet data plane §Transparent Address UTXOs](architecture/wallet-data-plane.md#transparent-address-utxos) and [§Transparent Address Tx History](architecture/wallet-data-plane.md#transparent-address-tx-history) carry the wire shapes and capability strings; [Extending artifacts](architecture/extending-artifacts.md) holds the canonical worked example for adding a new artifact family.
- **Transparent-address balance + derive-plane instantiation**: [Wallet data plane §Transparent Address Balance](architecture/wallet-data-plane.md#transparent-address-balance) defines the wallet and derive capabilities, and [Derive plane](architecture/derive-plane.md) defines the federation primitive.
- **Prevout resolution**: [Wallet data plane §Transparent Prevout Resolution](architecture/wallet-data-plane.md#transparent-prevout-resolution) defines the compute-at-read-time read path.
- **Mempool topology**: [ADR-0007](adrs/0007-mempool-topology-and-retention.md) records the durable mempool topology; [Wallet data plane §Mempool Snapshot and Subscription](architecture/wallet-data-plane.md#mempool-snapshot-and-subscription) owns the public surface.

## Vocabulary and naming rules

See [Public interfaces](architecture/public-interfaces.md) for the canonical naming rules, type conventions, error vocabulary, and config field shapes.

## Document lifecycles

Each tree under `docs/` has its own retire-on-ship rule.

- **Architecture** (`docs/architecture/`): the durable spine. Explains why each contract exists, what its invariants are, and where its boundary lives. Edited in place when contracts change. Architecture docs reference other architecture docs and at most one ADR per topic.
- **ADRs** (`docs/adrs/`): record of accepted design decisions in present tense. Edited in place when the decision rationale needs clarification; substantive design changes get a new ADR with a contiguous number. ADRs reference the owning architecture docs and only the predecessor decisions they directly build on.
- **Reference** (`docs/reference/`): current integration patterns and API support material. Reference docs may point into architecture docs, but they should not carry transition history or dated validation notes.
- **Runbooks** (`docs/runbooks/`): operational procedures with explicit prereqs, command lines, and expected outcomes. Edited in place as procedures evolve; reference architecture docs and ADRs (up) but do not describe architectural intent.
