# Derive Plane

The derive plane is the optional service tier that consumes Zinder's canonical artifacts and chain-event stream to materialize specialized views: explorer indexes, analytics aggregates, compliance projections, ecosystem-specific queries. It exists so that views with different freshness, retention, and replay characteristics do not contaminate the canonical write path.

This document defines the boundary, input and output contracts, failure model, replayability rules, and the decision procedure for "should this be canonical or derived." It is the sibling document to [Wallet Data Plane](wallet-data-plane.md) and a hard prerequisite for any future explorer or analytics feature.

## Purpose

Explorer features and wallet sync have different ownership rules. If explorer additions grow the canonical schema or create hidden dependencies in wallet sync, the store stops being a wallet correctness boundary and becomes a mixed product database. Zinder avoids that by keeping the derive plane separate from canonical state.

Concretely, the derive plane:

- **Consumes** canonical artifacts (`BlockArtifact`, `CompactBlockArtifact`, transaction artifacts) and event envelopes (`ChainEventEnvelope`, `MempoolEventEnvelope`).
- **Produces** materialized views with their own storage, schemas, retention, and gRPC surfaces.
- **Cannot affect** canonical state. A derive-plane crash does not stop ingest, does not block reads from `WalletQueryApi`, and does not corrupt canonical storage.
- **Is rebuildable**. Any derived view can be discarded and rebuilt from canonical artifacts. This is the test for whether a view belongs in the derive plane: if rebuilding requires re-validating chain data, the view is in the wrong plane.

A derive-pattern service is optional in v1 and consumes canonical artifacts rather than upstream node RPCs directly. Today's only such service is `zinder-explorer`. Derived explorer or analytics indexes must be rebuildable from canonical artifacts and must not become a hidden dependency of wallet sync. The `Derive*` SDK abstractions (trait, store, federation primitive) describe the reusable pattern; see [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) for why the SDK keeps the derive-shaped names even though the service binary rebranded to `zinder-explorer`.

## When to use the derive plane

A feature belongs in the derive plane when **any** of the following are true:

- The view is an aggregation, summarization, or reorder of canonical data. Example: top-100-addresses-by-volume, fee-rate histograms, time-series counts.
- The view is consumer-specific. Example: explorer dashboards, compliance reports, analytics partner integrations.
- The view has different retention or freshness requirements than canonical state. Example: a 24-hour rolling activity feed; a permanent address-volume archive.
- The view's failure does not block wallet correctness. Example: an explorer that goes stale by an hour does not affect a wallet's ability to sync or broadcast.

A feature belongs in the canonical plane (`zinder-store` artifact families, served via `WalletQueryApi`) when:

- The view is required for wallet sync correctness. Example: compact blocks, tree state, subtree roots.
- The view is required for transaction submission. Example: mempool snapshot.
- The view is required for chain-event subscription. Example: `ChainEventEnvelope` retention.

The decision procedure when adding a new feature:

```text
Does the feature affect a wallet's ability to sync, scan, or broadcast?
├── Yes → canonical (extend an existing artifact family, or add one per
│         the extending-artifacts cookbook)
└── No  → derive plane
```

The DX/AX corollary: if you find yourself extending `zinder-store` to support an explorer dashboard or analytics view, stop. You are likely growing the canonical surface for non-canonical reasons. The derive plane is the right boundary.

## Input contract

The derive plane consumes Zinder data through three channels, in decreasing order of preference:

### Channel A — `ChainEvents` subscription

The primary input. A derive-pattern consumer (`zinder-explorer` is the first) subscribes to `WalletQuery.ChainEvents` (defined in [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription)) using a persisted `StreamCursorTokenV1` cursor. Every `ChainCommitted` and `ChainReorged` envelope is delivered in order with replay-from-cursor on reconnect.

This channel is sufficient for views that derive from chain transitions: balance accumulators, address activity feeds, transaction count time-series, fee-rate distributions over time. The consumer's view is rebuilt by replaying the stream from `cursor = None`.

The shipped consumer-side helper is `zinder_explorer::run_chain_events_subscriber`. It implements the `DeriveConsumer` trait dispatch loop, decodes wire envelopes into typed `ChainCommittedEvent` / `ChainReorgedEvent` shapes, and persists the cursor atomically with each consumer's `WriteBatch` so a crash mid-apply replays the envelope on next start.

### Channel B — `MempoolEvents` subscription

For views that include unconfirmed activity. A derive-pattern consumer (`zinder-explorer` is the first) subscribes to `WalletQuery.MempoolEvents` (defined in [Wallet data plane §Mempool Snapshot and Subscription](wallet-data-plane.md#mempool-snapshot-and-subscription) and [ADR-0007](../adrs/0007-mempool-topology-and-retention.md)) with a persisted `MempoolStreamCursorV1` cursor.

Combine with Channel A when the view needs both chain and mempool perspectives (e.g. explorer dashboards showing pending transactions alongside confirmed activity). The two streams have independent cursors and independent retention; consumers handle cross-stream ordering.

The shipped consumer-side helper is `zinder_explorer::run_mempool_events_subscriber`. The same atomic-cursor contract applies; consumers implement `DeriveMempoolConsumer` and stage writes into `DeriveConsumerCtx::batch` per `MempoolConsumerEvent`.

### Channel C — Canonical artifact replay

For views that need full block bodies, full transaction data, or cross-block aggregations that the event stream does not carry. A derive-pattern consumer (`zinder-explorer` is the first) reads canonical artifacts via `ChainEpochReadApi` (in-process) or `WalletQuery` (over gRPC).

This channel is used for one-time replay (initial backfill, full rebuild) and for occasional historical reads. Steady-state operation should use Channel A or B. A derive consumer that pulls from Channel C continuously is a smell: the data should probably be carried in `ChainEvents` instead, or the view is in the wrong plane.

A fresh derive consumer whose persisted cursor sits below the upstream's retention floor needs a cold-start path that drains `WalletQuery.compact_block_range` for the gap and then attaches to the live `WalletQuery.ChainEvents` stream without dropping or duplicating events. Stateful derive consumers own this path; the framework provides Channel A's atomic-cursor contract, the consumer provides the gap-fill loop and the seam that joins it to the live stream. The shipped explorer balance handler is stateless and so does not need it.

### What the derive plane must not consume

- **Upstream node RPCs directly.** A derive-pattern service does not import `zinder-source`. It does not call Zebra. The upstream node is upstream of `zinder-ingest`, not of any derive consumer. This rule is structural: the derive plane assumes Zinder canonical data is the source of truth.
- **Live primary store handles in production.** A derive consumer that opens `PrimaryChainStore` or bypasses `ChainEventEnvelope` / immutable snapshots is breaking [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md). In-process composition during local development is the only exception.
- **`zinder-ingest` internals.** No reaching into `IngestArtifactBuilder`, no shared `chain_event_writer`, no co-process write lock on RocksDB.

## Output contract

A derive-plane consumer produces one of three output shapes:

### Shape 1 — Independent gRPC service

The derive consumer ships its own gRPC service with its own proto schema, own listen address, own `ServerInfo` capability descriptor (per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery)), and own retention and migration policies. The shipping example today is `zinder-explorer` serving `ExplorerQuery` with methods like `TransactionDetail`, `BlockSummary`, `AddressActivityFeed`, `FeeHistogram` ([Explorer plane](explorer-plane.md) documents the surface).

Capability strings use the consumer's product namespace: `<product>.<noun>.<capability>_v{N}`, distinct from `wallet.*` capabilities. Example: `explorer.transparent_address.activity_v1`. The PRD-era `derive.<consumer>.*` namespace was retired in [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md); the product name is what operators grep for.

### Shape 2 — Federated under `WalletQuery`

For derive views close enough to wallet semantics to belong in the same client surface, the derive consumer can be exposed as additional methods on `WalletQuery`, advertised under their own capability strings. The implementation lives in the consumer's service crate (`services/zinder-explorer/` today) and is composed into `WalletQueryGrpcAdapter` at startup. The federation primitive (`DeriveProxy<Client>`, the readiness gauge, and the readiness probe loop) lives in `services/zinder-query/src/derive_proxy.rs`; every federated method on `WalletQueryGrpcAdapter` is one closure passed to `DeriveProxy::forward`, so the four concerns each consumer would otherwise duplicate (client construction, error mapping, capability gating, readiness probing) stay in one place.

The first shipped consumer of Shape 2 is `WalletQuery.TransparentAddressBalance`, which proxies to `ExplorerQuery.TransparentAddressBalance` in `services/zinder-explorer/` when the explorer plane is configured and ready. When the explorer plane is unavailable, the native adapter falls back to the always-on canonical-confirmed compute path on the same RPC; `WalletQuery.ServerInfo` advertises `wallet.address.transparent_balance_v1` unconditionally and `explorer.transparent_address.balance_v1` only when the proxy is configured AND the most recent `ExplorerQuery.ServerInfo` probe reported `explorer.server_info_v1` ready inside the configured window. The explorer capability signals that the response additionally carries the live mempool overlay in `unconfirmed_delta_zat`; the wallet capability signals confirmed totals from canonical UTXOs.

This shape is reserved for views that wallets and applications consume *as if* they were canonical. The consumer's product capability prefix still applies; clients that gate on `wallet.*` capabilities never see the derive view by accident, and a CI assertion in `services/zinder-query/tests/integration/` enforces the namespace rule against any future federated method.

The step-by-step file list for adding a new Shape 2 consumer (the `DeriveProxy<C>` field, the readiness probe wiring, the two capability strings, and the compat-shim federation) is documented in [Extending the wallet data plane §Federation extension](extending-the-wallet-data-plane.md#federation-extension).

### Shape 3 — Sink-only (no Zinder-served queries)

The derive consumer writes to an external sink (Postgres, ClickHouse, S3, Kafka) and does not expose a Zinder-side query API. The consumer's user is the operator's own analytics stack. Zinder's only role is producing the event stream the consumer subscribes to.

Shape 3 is the mode that aligns with [Goldsky Mirror-style](https://goldsky.com) and [Subsquid SQD-style](https://blog.sqd.dev/) integrations: Zinder is the upstream, the operator's analytics tooling is downstream. Zinder does not constrain the sink; the cursor protocol is sufficient for the sink to track its own progress.

### Output naming

A derive service follows the same naming spine as canonical services ([Public Interfaces](public-interfaces.md)):

- Crate name: `zinder-{product}` (e.g. `zinder-explorer`). The product is the deployable name.
- Service name: `{Product}Query` for read-only views (no `Service`/`Manager`/`Handler` suffix). Today: `ExplorerQuery`.
- Capability prefix: `{product}.{noun}.{capability}_v{N}` (e.g. `explorer.transaction.detail_v1`). Per [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md), capability namespaces match the deployable name.
- Storage path: independent from canonical RocksDB; never colocated.

## Failure isolation

The derive plane fails independently. The boundary rules:

- **A derive consumer crash does not stop `zinder-ingest`.** Ingest writes ChainEvents to its own retention buffer; consumers fall behind, eventually expire, recover via cursor replay or full rebuild.
- **A derive consumer crash does not stop `zinder-query`.** `WalletQueryApi` reads from `ChainEpochReadApi`, not from any derive consumer.
- **A derive view becoming inconsistent does not corrupt canonical state.** Canonical artifacts are written by `zinder-ingest` in atomic batches. A derive consumer that misinterprets an event produces a wrong derived view, not a wrong canonical view.
- **Operators can drop and rebuild a derive view.** The consumer's storage is independent. `rm -rf /var/lib/zinder-explorer && systemctl restart zinder-explorer` produces a full rebuild from `ChainEvents` at `cursor = None`.

The metrics surface reflects this: `zinder-explorer` consumers emit their own readiness state, their own oldest-retained-cursor metric, their own backlog size. A derive view that is "not ready" does not propagate to `zinder-query` or `zinder-ingest`.

## Replayability

Every derive view is rebuildable. The contract:

- The view's state is a deterministic function of `ChainEvents` plus canonical artifacts up to some cursor. Views that include unconfirmed activity may also include `MempoolEvents`.
- Given the same input stream, the view produces the same output. No wall-clock dependence, no entropy, no non-determinism.
- The view's storage carries its own schema fingerprint. Schema changes in the derive view are independent of canonical schema changes.

This contract is what distinguishes derive from canonical. A canonical artifact, once written, is the source of truth. A derive view, once written, is just one possible projection; if the projection logic changes, the operator drops and rebuilds.

The replayability rule has a corollary for testing: every derive consumer ships a test that exercises full rebuild from `cursor = None` against a deterministic event stream. The test is the contract assertion; it fails if the view becomes accidentally non-deterministic.

## Schema versioning

Derive views version their schemas independently from canonical artifacts. A derive consumer's schema-version field has nothing to do with `ChainEpoch::artifact_schema_version`.

When a derive consumer changes its schema:

- Increment the consumer's own schema version.
- On startup, compare the persisted version against the expected version.
- On mismatch, the consumer either runs its own migration or drops and rebuilds. The rebuild path is always available; it is the failsafe.

This lets explorers iterate on dashboards without touching canonical storage and without coordinating with wallet sync.

## Operator surface

A derive consumer ships its own ops endpoints (`/healthz`, `/readyz`, `/metrics`) on a dedicated listener, separate from `zinder-query`'s. The conventions in [Service Operations](service-operations.md) apply: typed readiness causes, structured `/readyz` body, Prometheus metrics with the consumer's product prefix (e.g. `zinder_explorer_*`).

Configuration follows the canonical TOML conventions ([Public Interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions)):

```toml
[explorer]
listen_addr = "127.0.0.1:9068"
storage_path = "/var/lib/zinder-explorer"
bearer_token_path = "/run/secrets/zinder-explorer-token"
wallet_query_endpoint = "https://zinder.example:9101"   # zinder-query gRPC
ops_listen_addr = "127.0.0.1:9069"

[explorer.retention]
view_retention_days = 365
```

When `explorer.bearer_token_path` is set, `zinder-explorer` enforces the same shared-secret bearer-token interceptor used by private Zinder control planes ([ADR-0006](../adrs/0006-ingest-control-transport-security.md)). The matching `zinder-query` process must point its `[explorer] bearer_token_path` at the same secret before it advertises federated explorer capabilities.

Sensitive upstream node credentials never reach the derive plane. The derive plane authenticates against `zinder-query`, not against Zebra.

## Cross-cutting rules

- A derive consumer **must** advertise a `ServerCapabilities` descriptor including its own capability strings and its event-cursor retention windows. Operators and clients discover the consumer's surface the same way they discover canonical surfaces.
- A derive consumer **must** preserve the privacy boundary. By-address shielded queries are forbidden in the derive plane, the same way they are forbidden in `WalletQueryApi`. Server-side scanning is forbidden; viewing keys never reach the derive plane.
- A derive consumer **may** be deployed independently from `zinder-ingest` and `zinder-query`. The cursor protocol is the integration contract; the derive consumer can be on a different host, in a different region, or in a different organization (subject to network access to `WalletQuery.ChainEvents`).
- A derive consumer's **schema is its own**. `zinder-explorer` defines `ExplorerQuery` proto; a hypothetical future `zinder-analytics` would define `AnalyticsQuery` proto. There is no shared "derive proto" file beyond the cursor and event-envelope types from `zinder-proto`.

## Out of scope (for now)

- **Federation across multiple derive consumers.** A single client query that joins data across `explorer.*` and a hypothetical second product's namespace is not supported. Clients call each consumer separately and reconcile if needed.
- **Extraction of the consumer SDK into a separate crate.** Backfill-then-attach, the cursor-persisting `ChainEvents` subscriber, and the mempool-events subscriber are shipped today as in-process helpers under `services/zinder-explorer/src/consumer/` (entry points: `run_chain_events_subscriber`, `run_mempool_events_subscriber`, `backfill_then_attach`). Extraction to a dedicated `zinder-derive-sdk` crate waits until a second derive consumer justifies the split.
- **Derive consumers that require upstream node data not in canonical artifacts.** If a use case appears (e.g. mempool fee rates that Zinder does not currently surface), the canonical artifact extends first; derive consumers do not bypass.

## Cross-references

- [Service boundaries](service-boundaries.md) — names the derive plane in the workspace inventory and locks its optional, replayable shape.
- [Wallet Data Plane](wallet-data-plane.md) — the sibling boundary; canonical wallet/application read surface.
- [Chain Events](chain-events.md) — the event vocabulary the derive plane consumes.
- [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) — the subscription contract.
- [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) — retention windows that bound derive-consumer downtime tolerance.
- [ADR-0007](../adrs/0007-mempool-topology-and-retention.md) — the second event stream available to the derive plane.
- [Extending the wallet data plane §Federation extension](extending-the-wallet-data-plane.md#federation-extension) — the concrete extension checklist for federated derive methods.
- [Wallet data plane §Transparent Address Balance](wallet-data-plane.md#transparent-address-balance) — the first shipped federated derive consumer and its capability contract.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — the capability protocol derive consumers must implement.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions that derive consumers inherit.
