# Derive Plane

The derive plane is the optional projection tier that consumes Zinder's canonical artifacts and committed event stream to materialize specialized views: explorer indexes, analytics aggregates, compliance projections, ecosystem-specific queries. It exists so that views with different freshness, retention, and replay characteristics do not contaminate the canonical write path.

This document defines the boundary, input and output contracts, failure model, replayability rules, and the decision procedure for "should this be canonical or derived." It is the sibling document to [Wallet Data Plane](wallet-data-plane.md) and a hard prerequisite for any future explorer or analytics feature.

## Purpose

Explorer features and wallet sync have different ownership rules. If explorer additions grow the canonical schema or create hidden dependencies in wallet sync, the store stops being a wallet correctness boundary and becomes a mixed product database. Zinder avoids that by keeping the derive plane separate from canonical state.

Concretely, the derive plane:

- **Consumes** canonical artifacts (`BlockArtifact`, `CompactBlockArtifact`, transaction artifacts) and event envelopes (`ChainEventEnvelope`, `MempoolEventEnvelope`) from the ingest writer.
- **Produces** materialized views with their own storage, schemas, retention, and gRPC surfaces.
- **Cannot affect** canonical state. A derive-plane crash does not stop ingest, does not block reads from `WalletQueryApi`, and does not corrupt canonical storage.
- **Is rebuildable**. Any derived view can be discarded and rebuilt from canonical artifacts. This is the test for whether a view belongs in the derive plane: if rebuilding requires re-validating chain data, the view is in the wrong plane.

A derive projection is optional in v1 and consumes canonical artifacts rather than upstream node RPCs directly. The bundled projection is hosted by `zinder-ingest`, which opens the derive store as primary and atomically writes consumer rows plus derive cursors after canonical commits. `zinder-explorer` is the stateless reader gateway: it opens the same derive store as a secondary and serves `ExplorerQuery`. Derived explorer or analytics indexes must be rebuildable from canonical artifacts and must not become a hidden dependency of wallet sync. The `Derive*` SDK abstractions (trait, store, federation primitive) describe the reusable pattern; see [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) and [ADR-0023](../adrs/0023-derive-plane-hosted-by-ingest.md).

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

The bundled derive plane consumes Zinder data through three channels, in decreasing order of preference:

### Channel A - ingest-hosted chain-event dispatch

The primary input. After `zinder-ingest` commits a canonical chain epoch, it dispatches the committed `ChainEvent` plus parsed block contexts to bundled `BlockKeyedConsumer` implementations through `zinder_derive::DeriveStore::write_chain_event`. Each derive write stores consumer rows and the canonical chain-event cursor in one derive-store batch.

This channel is sufficient for views that derive from chain transitions: block summaries, recent transactions, paid-fee summaries, address activity feeds, transaction count time-series, fee-rate distributions over time. The consumer's view is rebuilt by replaying retained canonical events from the lowest persisted derive cursor, or from `cursor = None` after dropping the derive store.

The cursor contract is owned by `DeriveStore`, not by `zinder-ingest`: the dispatcher stages consumer writes and cursor advances in the same `WriteBatch`, so a crash mid-apply replays the envelope on the next startup repair.

### Channel B - ingest-hosted mempool-event dispatch

For views that include unconfirmed activity. `zinder-ingest` dispatches committed `MempoolEventEnvelope` values to bundled `DeriveMempoolConsumer` implementations through `zinder_derive::DeriveStore::write_mempool_event`.

Combine with Channel A when the view needs both chain and mempool perspectives, such as explorer dashboards showing pending transactions alongside confirmed activity. Chain and mempool cursors live in separate derive-store column families because chain cursors rewind on reorg while mempool cursors do not.

The same atomic-cursor contract applies: consumers stage writes into `DeriveConsumerCtx::batch`, and `DeriveStore` stages the mempool cursor before the batch is committed.

### Channel C - canonical artifact replay

For views that need full block bodies, full transaction data, or cross-block aggregations that the event stream does not carry. The ingest-hosted dispatcher reads canonical artifacts through `ChainEpochReadApi` when repairing startup gaps or resolving prevouts outside the current commit batch.

This channel is used for one-time replay, full rebuild, startup catch-up, and occasional historical lookups. Steady-state operation should use Channel A or B. A derive consumer that repeatedly scans canonical history outside commit dispatch is a smell: the data should probably be carried in the chain event context, or the view is in the wrong plane.

A fresh derive consumer whose persisted cursor sits below the canonical event retention floor needs a cold-start path that rebuilds from canonical artifacts and then resumes ingest-hosted event dispatch without dropping or duplicating events. Stateful derive consumers own this path; the framework provides the atomic-cursor contract, and the writer provides the gap-fill loop.

### What the derive plane must not consume

- **Upstream node RPCs directly.** A derive-pattern service does not import `zinder-source`. It does not call Zebra. The upstream node is upstream of `zinder-ingest`, not of any derive consumer. This rule is structural: the derive plane assumes Zinder canonical data is the source of truth.
- **Live primary store handles in reader gateways.** A reader gateway that opens `PrimaryChainStore`, opens the derive store as primary, or bypasses immutable snapshots is breaking [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) and [ADR-0023](../adrs/0023-derive-plane-hosted-by-ingest.md).
- **`zinder-ingest` internals from consumer logic.** Derive consumers receive typed block and event contexts. They do not reach into `IngestArtifactBuilder`, `chain_event_writer`, or canonical RocksDB write locks.

## Output contract

A derive-plane consumer produces one of three output shapes:

### Shape 1 — Independent gRPC service

The reader gateway ships its own gRPC service with its own proto schema, own listen address, own `ServerInfo` capability descriptor (per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery)), and own freshness policy. The shipping example today is `zinder-explorer` serving `ExplorerQuery` from a secondary derive store with methods like `TransactionDetail`, `BlockSummary`, `AddressActivityFeed`, `FeeHistogram` ([Explorer plane](explorer-plane.md) documents the surface).

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
- Storage path: a derive-store subdirectory under the canonical storage path for writer and secondary-reader coordination; the store itself remains a separate RocksDB instance and keyspace.

## Failure isolation

The derive plane fails independently. The boundary rules:

- **A derive dispatch failure does not corrupt canonical state.** Canonical commits land before derive dispatch. Startup catch-up replays retained canonical events whose cursor has not reached the derive store.
- **A reader gateway crash does not stop `zinder-ingest`.** The derive primary is owned by ingest; readers reopen as secondaries and catch up from the primary view.
- **A reader gateway crash does not stop `zinder-query`.** `WalletQueryApi` reads from `ChainEpochReadApi`, not from any derive consumer.
- **A derive view becoming inconsistent does not corrupt canonical state.** Canonical artifacts are written by `zinder-ingest` in atomic batches. A derive consumer that misinterprets an event produces a wrong derived view, not a wrong canonical view.
- **Operators can drop and rebuild a derive view.** The derive store is independent. Dropping the derive subdirectory and restarting ingest produces a full rebuild from canonical artifacts and retained events.

The metrics surface reflects this: ingest exposes derive-dispatch health for writer-owned projections, and `zinder-explorer` exposes secondary-reader freshness and query readiness. A derive view that is "not ready" does not propagate to wallet sync correctness.

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
[ops]
listen_addr = "127.0.0.1:9069"   # shared section; "" disables the endpoint

[explorer]
listen_addr = "127.0.0.1:9068"
storage_path = "/var/lib/zinder"          # canonical path; derive store is nested under it
bearer_token_path = "/run/secrets/zinder-explorer-token"
wallet_query_endpoint = "https://zinder.example:9101"   # zinder-query gRPC

[explorer.retention]
view_retention_days = 365
```

When `explorer.bearer_token_path` is set, `zinder-explorer` enforces the same shared-secret bearer-token interceptor used by private Zinder control planes ([ADR-0006](../adrs/0006-ingest-control-transport-security.md)). The matching `zinder-query` process must point its `[explorer] bearer_token_path` at the same secret before it advertises federated explorer capabilities.

Sensitive upstream node credentials never reach reader gateways. The derive writer runs inside `zinder-ingest`, which already owns upstream node access; derive consumers do not create their own upstream clients.

## Cross-cutting rules

- A derive reader gateway **must** advertise a `ServerCapabilities` descriptor including its own capability strings and derive freshness. Operators and clients discover the gateway surface the same way they discover canonical surfaces.
- A derive consumer **must** preserve the privacy boundary. By-address shielded queries are forbidden in the derive plane, the same way they are forbidden in `WalletQueryApi`. Server-side scanning is forbidden; viewing keys never reach the derive plane.
- A derive reader gateway **may** be deployed independently from `zinder-ingest` and `zinder-query` when it has secondary access to the derive store. Cross-host streaming sinks remain future work; the bundled writer path is ingest-hosted.
- A derive consumer's **schema is its own**. `zinder-explorer` defines `ExplorerQuery` proto; a hypothetical future `zinder-analytics` would define `AnalyticsQuery` proto. There is no shared "derive proto" file beyond the cursor and event-envelope types from `zinder-proto`.

## Out of scope (for now)

- **Federation across multiple derive consumers.** A single client query that joins data across `explorer.*` and a hypothetical second product's namespace is not supported. Clients call each consumer separately and reconcile if needed.
- **External streaming derive sinks.** The bundled consumers run in `zinder-ingest` and write through `zinder_derive::DeriveStore`. A future analytics sink may need a separate replay-and-attach runner over `WalletQuery.ChainEvents`, but that runner should be introduced when a second deployable consumer justifies the extra boundary.
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
