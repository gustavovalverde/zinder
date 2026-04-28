# Derive Plane

The derive plane is the optional service tier that consumes Zinder's canonical artifacts and chain-event stream to materialize specialized views: explorer indexes, analytics aggregates, compliance projections, ecosystem-specific queries. It exists so that views with different freshness, retention, and replay characteristics do not contaminate the canonical write path.

This document defines the boundary, input and output contracts, failure model, replayability rules, and the decision procedure for "should this be canonical or derived." It is the sibling document to [Wallet Data Plane](wallet-data-plane.md) and a hard prerequisite for any future explorer or analytics feature.

## Purpose

Zaino's tracker shows what happens when explorer features and wallet sync share storage and write paths: every explorer addition either grows the canonical schema (forcing migrations) or creates a hidden dependency that breaks wallet sync ([Lessons from Zaino Pattern 4](../reference/lessons-from-zaino.md#pattern-4-storage-as-a-linear-migration-ladder)). Zinder avoids this by keeping the derive plane separate from canonical state.

Concretely, the derive plane:

- **Consumes** canonical artifacts (`BlockArtifact`, `CompactBlockArtifact`, transaction artifacts) and event envelopes (`ChainEventEnvelope`, `MempoolEventEnvelope`).
- **Produces** materialized views with their own storage, schemas, retention, and gRPC surfaces.
- **Cannot affect** canonical state. A derive-plane crash does not stop ingest, does not block reads from `WalletQueryApi`, and does not corrupt canonical storage.
- **Is rebuildable**. Any derived view can be discarded and rebuilt from canonical artifacts. This is the test for whether a view belongs in the derive plane: if rebuilding requires re-validating chain data, the view is in the wrong plane.

[PRD-0001 Implementation Decisions](../prd-0001-zinder-indexer.md) is explicit: "`zinder-derive` is optional in v1 and must consume canonical artifacts rather than upstream node RPCs directly. ... Derived explorer or analytics indexes must be rebuilt from canonical artifacts and must not become a hidden dependency of wallet sync."

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

The primary input. A `zinder-derive` consumer subscribes to `WalletQuery.ChainEvents` (defined in [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription)) using a persisted `StreamCursorTokenV1` cursor. Every `ChainCommitted` and `ChainReorged` envelope is delivered in order with replay-from-cursor on reconnect.

This channel is sufficient for views that derive from chain transitions: balance accumulators, address activity feeds, transaction count time-series, fee-rate distributions over time. The consumer's view is rebuilt by replaying the stream from `cursor = None`.

### Channel B — `MempoolEvents` subscription

For views that include unconfirmed activity after M3 lands. A `zinder-derive` consumer subscribes to `WalletQuery.MempoolEvents` (defined in [M3 Mempool](../specs/m3-mempool.md)) with a persisted `MempoolStreamCursorV1` cursor.

Combine with Channel A when the view needs both chain and mempool perspectives (e.g. explorer dashboards showing pending transactions alongside confirmed activity). The two streams have independent cursors and independent retention; consumers handle cross-stream ordering.

### Channel C — Canonical artifact replay

For views that need full block bodies, full transaction data, or cross-block aggregations that the event stream does not carry. A `zinder-derive` consumer reads canonical artifacts via `ChainEpochReadApi` (in-process) or `WalletQuery` (over gRPC).

This channel is used for one-time replay (initial backfill, full rebuild) and for occasional historical reads. Steady-state operation should use Channel A or B. A derive consumer that pulls from Channel C continuously is a smell: the data should probably be carried in `ChainEvents` instead, or the view is in the wrong plane.

### What the derive plane must not consume

- **Upstream node RPCs directly.** `zinder-derive` does not import `zinder-source`. It does not call Zebra. The upstream node is upstream of `zinder-ingest`, not of the derive plane. This rule is structural: the derive plane assumes Zinder canonical data is the source of truth.
- **Live primary store handles in production.** A derive consumer that opens `PrimaryChainStore` or bypasses `ChainEventEnvelope` / immutable snapshots is breaking [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md). M1 in-process composition is the only exception.
- **`zinder-ingest` internals.** No reaching into `IngestArtifactBuilder`, no shared `chain_event_writer`, no co-process write lock on RocksDB.

## Output contract

A derive-plane consumer produces one of three output shapes:

### Shape 1 — Independent gRPC service

The derive consumer ships its own gRPC service with its own proto schema, own listen address, own `ServerInfo` capability descriptor (per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery)), and own retention and migration policies. Example: a hypothetical `zinder-derive-explorer` service exposing `ExplorerQuery` proto with methods like `BlockSummary`, `AddressActivityFeed`, `FeeHistogram`.

The service's capability strings use the `derive.<consumer>.<capability>_v{N}` namespace, distinct from `wallet.*` capabilities. Example: `derive.explorer.address_activity_v1`.

### Shape 2 — Federated under `WalletQuery`

For derive views close enough to wallet semantics to belong in the same client surface, the derive consumer can be exposed as additional methods on `WalletQuery`, advertised under their own capability strings. The implementation lives in `services/zinder-derive` (or a dedicated derive crate) and is composed into `WalletQueryGrpcAdapter` at startup.

This shape is reserved for views that wallets and applications consume *as if* they were canonical. The `derive.*` capability prefix still applies; clients that gate on `wallet.*` capabilities never see the derive view by accident.

### Shape 3 — Sink-only (no Zinder-served queries)

The derive consumer writes to an external sink (Postgres, ClickHouse, S3, Kafka) and does not expose a Zinder-side query API. The consumer's user is the operator's own analytics stack. Zinder's only role is producing the event stream the consumer subscribes to.

Shape 3 is the mode that aligns with [Goldsky Mirror-style](https://goldsky.com) and [Subsquid SQD-style](https://blog.sqd.dev/) integrations: Zinder is the upstream, the operator's analytics tooling is downstream. Zinder does not constrain the sink; the cursor protocol is sufficient for the sink to track its own progress.

### Output naming

A derive service follows the same naming spine as canonical services ([Public Interfaces](public-interfaces.md)):

- Crate name: `zinder-derive` (the umbrella) or `zinder-derive-{consumer}` (per concrete consumer).
- Service name: `{Consumer}Query` for read-only views (no `Service`/`Manager`/`Handler` suffix).
- Capability prefix: `derive.{consumer}.{capability}_v{N}`.
- Storage path: independent from canonical RocksDB; never colocated.

## Failure isolation

The derive plane fails independently. The boundary rules:

- **A derive consumer crash does not stop `zinder-ingest`.** Ingest writes ChainEvents to its own retention buffer; consumers fall behind, eventually expire, recover via cursor replay or full rebuild.
- **A derive consumer crash does not stop `zinder-query`.** `WalletQueryApi` reads from `ChainEpochReadApi`, not from any derive consumer.
- **A derive view becoming inconsistent does not corrupt canonical state.** Canonical artifacts are written by `zinder-ingest` in atomic batches. A derive consumer that misinterprets an event produces a wrong derived view, not a wrong canonical view.
- **Operators can drop and rebuild a derive view.** The derive consumer's storage is independent. `rm -rf /var/lib/zinder-derive-explorer && systemctl restart zinder-derive-explorer` produces a full rebuild from `ChainEvents` at `cursor = None`.

The metrics surface reflects this: `zinder-derive` consumers emit their own readiness state, their own oldest-retained-cursor metric, their own backlog size. A derive view that is "not ready" does not propagate to `zinder-query` or `zinder-ingest`.

## Replayability

Every derive view is rebuildable. The contract:

- The view's state is a deterministic function of `ChainEvents` plus canonical artifacts up to some cursor. After M3, views that include unconfirmed activity may also include `MempoolEvents`.
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

A derive consumer ships its own ops endpoints (`/healthz`, `/readyz`, `/metrics`) on a dedicated listener, separate from `zinder-query`'s. The conventions in [Service Operations](service-operations.md) apply: typed readiness causes, structured `/readyz` body, Prometheus metrics with the `zinder_derive_*` prefix.

Configuration follows the canonical TOML conventions ([Public Interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions)):

```toml
[derive.explorer]
listen_addr = "127.0.0.1:9068"
storage_path = "/var/lib/zinder-derive-explorer"
chain_events_endpoint = "https://zinder.example:9067"  # zinder-query gRPC
chain_events_cursor_persist_path = "/var/lib/zinder-derive-explorer/cursor"

[derive.explorer.retention]
view_retention_days = 365
```

Sensitive upstream node credentials never reach the derive plane. The derive plane authenticates against `zinder-query`, not against Zebra.

## Cross-cutting rules

- A derive consumer **must** advertise a `ServerCapabilities` descriptor including its own capability strings and its event-cursor retention windows. Operators and clients discover the consumer's surface the same way they discover canonical surfaces.
- A derive consumer **must** preserve the privacy boundary. By-address shielded queries are forbidden in the derive plane, the same way they are forbidden in `WalletQueryApi`. Server-side scanning is forbidden; viewing keys never reach the derive plane.
- A derive consumer **may** be deployed independently from `zinder-ingest` and `zinder-query`. The cursor protocol is the integration contract; the derive consumer can be on a different host, in a different region, or in a different organization (subject to network access to `WalletQuery.ChainEvents`).
- A derive consumer's **schema is its own**. `zinder-derive-explorer` defines `ExplorerQuery` proto; `zinder-derive-analytics` defines `AnalyticsQuery` proto. There is no shared "derive proto" file beyond the cursor and event-envelope types from `zinder-proto`.

## Out of scope (for now)

- **A reference derive consumer implementation.** This document defines the contract; implementations land per-consumer when concrete needs appear. The first likely consumer is an explorer view (paginated address history, block summaries, fee histograms) when wallet-side sync against M1 is validated.
- **Federation across multiple derive consumers.** A single client query that joins data across `derive.explorer` and `derive.analytics` is not supported. Clients call each consumer separately and reconcile if needed.
- **A standardized derive-consumer SDK.** Consumers are free-form Rust code that consumes `ChainEvents`. A future "consumer SDK" might land if the boilerplate becomes recurrent; until then, the contract is the cursor protocol.
- **Derive consumers that require upstream node data not in canonical artifacts.** If a use case appears (e.g. mempool fee rates that Zinder does not currently surface), the canonical artifact extends first; derive consumers do not bypass.

## Cross-references

- [PRD-0001](../prd-0001-zinder-indexer.md) — defines the derive plane as optional in v1 and replayable.
- [RFC-0001 §zinder-derive](../rfcs/0001-service-oriented-indexer-architecture.md) — names the derive plane in the workspace inventory.
- [Wallet Data Plane](wallet-data-plane.md) — the sibling boundary; canonical wallet/application read surface.
- [Chain Events](chain-events.md) — the event vocabulary the derive plane consumes.
- [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) — the subscription contract.
- [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) — retention windows that bound derive-consumer downtime tolerance.
- [M3 Mempool](../specs/m3-mempool.md) — the second event stream available to the derive plane after M3.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — the capability protocol derive consumers must implement.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions that derive consumers inherit.
