# Derive Plane

The derive plane is the optional projection tier that consumes Zinder's canonical artifacts and committed event stream to materialize specialized views: explorer indexes, analytics aggregates, compliance projections, ecosystem-specific queries. It exists so that views with different freshness, retention, and replay characteristics do not contaminate the canonical write path.

This document defines the boundary, input and output contracts, failure model, replayability rules, and the decision procedure for "should this be canonical or derived." It is the sibling document to [Wallet Data Plane](wallet-data-plane.md) and a hard prerequisite for any future explorer or analytics feature.

## Purpose

Explorer features and wallet sync have different ownership rules. If explorer additions grow the canonical schema or create hidden dependencies in wallet sync, the store stops being a wallet correctness boundary and becomes a mixed product database. Zinder avoids that by keeping the derive plane separate from canonical state.

Concretely, the derive plane:

- **Consumes** canonical facts (`BlockHeaderArtifact`, `BlockTransactionIndexArtifact`, `TransactionFactsArtifact`, transparent-output facts) and event envelopes (`ChainEventEnvelope`, `MempoolEventEnvelope`) from the ingest writer.
- **Produces** materialized views with their own storage, schemas, retention, and gRPC surfaces.
- **Cannot affect** canonical state. A derive-plane crash does not stop ingest, does not block reads from `WalletQueryApi`, and does not corrupt canonical storage.
- **Is rebuildable**. Any derived view can be discarded and rebuilt from canonical artifacts. This is the test for whether a view belongs in the derive plane: if rebuilding requires re-validating chain data, the view is in the wrong plane.

A derive projection consumes canonical artifacts rather than upstream node RPCs
directly. The bundled projection is hosted by `zinder-ingest`, which opens the
derive store as primary and runs the derive tailer over retained canonical
events. The tailer atomically writes consumer rows plus derive cursors.
`zinder-query`, `zinder-compat-lightwalletd`, and `zinder-explorer` open the
derive store as RocksDB secondaries when they serve derive-backed reads.
`zinder-explorer` may start without an available derive store; in that state it
advertises only canonical/federated capabilities and omits derive-backed
capabilities. Derived explorer or analytics indexes must be rebuildable from
canonical artifacts and must not become a hidden dependency of wallet sync. The
`Derive*` SDK abstractions (trait, store, federation primitive) describe the
reusable pattern; see [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md).

Bundled wallet-serving derive projections retain their replayed rows for the
history they have processed unless their owning consumer schema is wiped or the
operator rebuilds the derive store. Do not add a partial pruning knob for
transparent-address history, recent transactions, or similar wallet-visible
derive reads without also adding the public retention floor, cursor-expiry
semantics, and tests proving readers fail explicitly instead of returning
silent partial history.

## When to use the derive plane

A feature belongs in the derive plane when **any** of the following are true:

- The view is an aggregation, summarization, or reorder of canonical data. Example: top-100-addresses-by-volume, fee-rate histograms, time-series counts.
- The view is consumer-specific. Example: explorer dashboards, compliance reports, analytics partner integrations.
- The view has different retention or freshness requirements than canonical state. Example: a 24-hour rolling activity feed; a permanent address-volume archive.
- The view's failure does not block wallet correctness. Example: an explorer that goes stale by an hour does not affect a wallet's ability to sync or broadcast.

A fact belongs in the canonical plane (`zinder-store` artifact families) when:

- The view is required for wallet sync correctness. Example: compact blocks, tree state, subtree roots.
- The view is required for transaction submission. Example: mempool snapshot.
- The view is required for chain-event subscription. Example: `ChainEventEnvelope` retention.
- A reusable immutable block or transaction fact is absent from retained
  canonical inputs and a deterministic derive view would otherwise have to
  call the upstream node. Final post-block note-commitment roots are the
  reference case: ingest obtains them through the typed source boundary and
  persists them canonically, while root-to-block lookup remains derived.

The decision procedure when adding a new feature:

```text
Is this an immutable source fact or a query-specific view?
├── Immutable fact
│   ├── Already retained canonically → consume the existing artifact
│   └── Missing but reusable         → extend the typed source boundary and
│                                      canonical artifacts
└── Query-specific view              → derive plane
```

The DX/AX corollary: if you find yourself extending `zinder-store` to support an explorer dashboard or analytics view, stop. You are likely growing the canonical surface for non-canonical reasons. The derive plane is the right boundary.

## Input contract

The bundled derive plane consumes Zinder data through three channels, in decreasing order of preference:

### Channel A - ingest-hosted chain-event tailing

The primary input. `zinder-ingest` tails retained canonical `ChainEvent`
envelopes, hydrates typed block contexts from the canonical store, and dispatches
them to bundled `BlockKeyedConsumer` implementations through
`zinder_derive::DeriveStore::write_chain_event`. Each derive write stores
consumer rows and the canonical chain-event cursor in one derive-store batch.

This channel is sufficient for views that derive from chain transitions: block summaries, recent transactions, paid-fee summaries, address activity feeds, transaction count time-series, fee-rate distributions over time. The consumer's view is rebuilt by replaying retained canonical events from the lowest persisted derive cursor, or from `cursor = None` after dropping the derive store.

The cursor contract is owned by `DeriveStore`, not by `zinder-ingest`: the dispatcher stages consumer writes and cursor advances in the same `WriteBatch`, so a crash mid-apply replays the envelope on the next startup repair.

### Channel B - ingest-hosted mempool-event dispatch

For views that include unconfirmed activity. `zinder-ingest` dispatches committed `MempoolEventEnvelope` values to bundled `DeriveMempoolConsumer` implementations through `zinder_derive::DeriveStore::write_mempool_event`.

Combine with Channel A when the view needs both chain and mempool perspectives, such as explorer dashboards showing pending transactions alongside confirmed activity. Chain and mempool cursors live in separate derive-store column families because chain cursors rewind on reorg while mempool cursors do not.

The same atomic-cursor contract applies: consumers stage writes into `DeriveConsumerCtx::batch`, and `DeriveStore` stages the mempool cursor before the batch is committed.

### Channel C - canonical artifact replay

For views that need full block bodies, full transaction data, or cross-block aggregations that the event stream does not carry. The ingest-hosted dispatcher reads canonical artifacts through `ChainEpochReadApi` when repairing startup gaps or resolving prevouts outside the current commit batch.

This channel is used for one-time replay, full rebuild, startup catch-up, and
occasional historical lookups. Steady-state operation should use Channel A or B.
A derive consumer that repeatedly scans canonical history outside the tailer is
a smell: the data should probably be carried in the chain event context, or the
view is in the wrong plane.

When a newly introduced canonical fact needs historical upstream enrichment,
`zinder-ingest` owns that source operation before the derive consumer runs. The
commitment-root backfill is the reference pattern: ingest fetches settled
`z_gettreestate` observations through `NodeSource`, idempotently enriches the
matching canonical block artifact, then feeds bounded canonical contexts to
`CommitmentRootSearchConsumer`. The consumer itself never imports
`zinder-source`, and its coverage row advances atomically with its index rows
without moving the shared chain-event cursor.

A fresh derive consumer whose persisted cursor sits below the canonical event retention floor needs a cold-start path that rebuilds from canonical artifacts and then resumes ingest-hosted event dispatch without dropping or duplicating events. Stateful derive consumers own this path; the framework provides the atomic-cursor contract, and the writer provides the gap-fill loop.

### What the derive plane must not consume

- **Upstream node RPCs directly.** A derive-pattern service does not import `zinder-source`. It does not call Zebra. The upstream node is upstream of `zinder-ingest`, not of any derive consumer. This rule is structural: the derive plane assumes Zinder canonical data is the source of truth.
- **Live primary store handles in reader gateways.** A reader gateway that opens `PrimaryChainStore`, opens the derive store as primary, or bypasses immutable snapshots breaks [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) and the service-boundary contract.
- **`zinder-ingest` internals from consumer logic.** Derive consumers receive typed block and event contexts. They do not reach into `IngestArtifactBuilder`, `chain_event_writer`, or canonical RocksDB write locks.

## Output contract

A derive-plane consumer produces one of three output shapes:

### Shape 1 — Independent gRPC service

The reader gateway ships its own gRPC service with its own proto schema, own listen address, own `ServerInfo` capability descriptor (per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery)), and own freshness policy. The shipping example today is `zinder-explorer` serving `ExplorerQuery` from a secondary derive store with methods like `TransactionDetail`, `BlockSummary`, `AddressActivityFeed`, `FeeHistogram` ([Explorer plane](explorer-plane.md) documents the surface).

Capability strings use the consumer's product namespace:
`<product>.<noun>.<capability>_v{N}`, distinct from `wallet.*` capabilities.
Example: `explorer.transparent_address.activity_v1`. Product names are the
operator-facing namespace per
[ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md).

### Shape 2 — Federated under `WalletQuery`

For derive views close enough to wallet semantics to belong in the same client surface, a derive consumer can be exposed as additional methods on `WalletQuery`, advertised under their own capability strings. The implementation lives in the consumer's service crate (`services/zinder-explorer/` today) and is composed into `WalletQueryGrpcAdapter` at startup. `zinder-query` opens a readiness-gated client to the derive service, maps its errors, and gates the federated capability on a readiness probe so the wallet plane advertises the method only while the consumer is reachable.

Shape 2 is the reserved pattern for views that wallets and applications consume *as if* they were canonical. No method ships under it today. The consumer's product capability prefix still applies; clients that gate on `wallet.*` capabilities never see a derive view by accident, and a CI assertion in `services/zinder-query/tests/integration/` enforces the namespace rule against any future federated method.

The step-by-step file list for adding a Shape 2 consumer (the readiness-gated client field, the probe wiring, the two capability strings, and the compat-shim federation) is documented in [Extending the wallet data plane §Federation extension](extending-the-wallet-data-plane.md#federation-extension).

### Shape 3 — Sink-only (no Zinder-served queries)

The derive consumer writes to an external sink (Postgres, ClickHouse, S3, Kafka) and does not expose a Zinder-side query API. The consumer's user is the operator's own analytics stack. Zinder's only role is producing the event stream the consumer subscribes to.

Shape 3 is the mode that aligns with [Goldsky Mirror-style](https://goldsky.com) and [Subsquid SQD-style](https://blog.sqd.dev/) integrations: Zinder is the upstream, the operator's analytics tooling is downstream. Zinder does not constrain the sink; the cursor protocol is sufficient for the sink to track its own progress.

### Output naming

A derive service follows the same naming spine as canonical services ([Public Interfaces](public-interfaces.md)):

- Crate name: `zinder-{product}` (e.g. `zinder-explorer`). The product is the deployable name.
- Service name: `{Product}Query` for read-only views (no `Service`/`Manager`/`Handler` suffix). Today: `ExplorerQuery`.
- Capability prefix: `{product}.{noun}.{capability}_v{N}` (e.g. `explorer.transaction.detail_v3`). Per [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md), capability namespaces match the deployable name.
- Storage path: a derive-store subdirectory under the canonical storage path for writer and secondary-reader coordination; the store itself remains a separate RocksDB instance and keyspace.

## Failure isolation

The derive plane fails independently. The boundary rules:

- **A derive tailer failure does not corrupt canonical state.** Canonical commits land before the derive tailer sees the event. Startup catch-up replays retained canonical events whose cursor has not reached the derive store.
- **A reader gateway crash does not stop `zinder-ingest`.** The derive primary is owned by ingest; readers reopen as secondaries and catch up from the primary view.
- **A reader gateway crash does not stop `zinder-query`.** `WalletQueryApi` reads from `ChainEpochReadApi`, not from any derive consumer.
- **A derive view becoming inconsistent does not corrupt canonical state.** Canonical artifacts are written by `zinder-ingest` in atomic batches. A derive consumer that misinterprets an event produces a wrong derived view, not a wrong canonical view.
- **Operators can drop and rebuild a derive view.** The derive store is independent. Dropping the derive subdirectory and restarting ingest produces a full rebuild from canonical artifacts and retained events.

The metrics surface reflects this: ingest exposes derive-tailer health for
writer-owned projections, and `zinder-explorer` exposes secondary-reader
freshness and query readiness. A derive view that is "not ready" does not
propagate to wallet sync correctness.

## Replayability

Every derive view has a declared deterministic recovery path. The contract:

- The view's state is a deterministic function of `ChainEvents` plus named canonical artifacts up to some cursor. Views that include unconfirmed activity may also include `MempoolEvents`.
- Given the same input stream, the view produces the same output. No wall-clock dependence, no entropy, no non-determinism.
- The view's storage carries its own schema fingerprint. Schema changes in the derive view are independent of canonical schema changes.
- Before an incompatible migration clears rows, tests must prove that the named recovery inputs cover the persisted history. Event retention alone is not evidence of recoverability when a consumer joins shorter-lived canonical projections.

This contract is what distinguishes derive from canonical. A canonical artifact, once written, is the source of truth. A derive view is one possible projection, but operators drop and rebuild it only when its declared recovery source still covers the rows being replaced.

The recovery rule has a corollary for testing: an incompatible schema change must exercise full rebuild from its oldest supported recovery point, while a row-compatible change must prove that predecessor rows and cursors survive and that the new reader safely interprets them. A `cursor = None` replay over only the current retention window is not proof of complete historical recovery.

This contract is what distinguishes derive from canonical. A canonical artifact, once written, is the source of truth. A derive view is one possible projection, but operators drop and rebuild it only when its declared recovery source still covers the rows being replaced.

The recovery rule has a corollary for testing: an incompatible schema change must exercise full rebuild from its oldest supported recovery point, while a row-compatible change must prove that predecessor rows and cursors survive and that the new reader safely interprets them. A `cursor = None` replay over only the current retention window is not proof of complete historical recovery.

## Projection state and read snapshots

Consumers that need to make completeness claims persist a
`ConsumerProjectionState` beside their rows. The state names the canonical epoch,
projection tip height and hash, monotonic revision, and optional contiguous
`ConsumerProjectionCoverage`. A block-keyed consumer stages this state in the
same `WriteBatch` as its projection rows and chain-event cursor, so a crash
cannot publish a new tip, revision, or coverage boundary without the rows that
justify it. Reorg handling may advance, retain, or clear coverage according to
the consumer's proof; it must not infer completeness from cursor position alone.

`DeriveStore::read_snapshot` binds projection metadata, point reads, range
scans, joins, and exact counts to one store sequence. Primary stores use a
RocksDB snapshot. Secondary stores hold a shared catch-up barrier for the
snapshot lifetime, while `try_catch_up` takes the exclusive side, so one logical
response cannot straddle a secondary refresh. A public read fence can therefore
identify the snapshot by epoch, projection revision, and projection tip. Paged
cursors and follow-up requests must reject a fence that no longer matches.

## Schema versioning

Derive views version their schemas independently from canonical artifacts and from each other. A derive consumer's schema-version field has nothing to do with `ChainEpoch::artifact_schema_version`.

Each consumer declares its schema version alongside its name, column families, and explicitly compatible older row versions in one `DeriveConsumerSchema`. The manifest records both the latest writer and every row schema still present. Exact matches and cumulative compatible predecessors with unchanged column-family ownership preserve rows and cursors; incompatible forward changes rebuild only that consumer. A persisted newer version, missing compatibility for any retained row version, or an undeclared manifest consumer fails closed. Secondary readers revalidate this contract after every catch-up. Reconciliation clears rows with range tombstones rather than dropping a column family in place, because a `drop_cf` edit crashes a secondary reader replaying it during catch-up. The manifest layout, compatibility rule, deployment ordering, crash-safe reconciliation, secondary-reader validation, and narrowed `DERIVE_STORE_FORMAT_VERSION` container gate are defined in [ADR-0028](../adrs/0028-per-consumer-derive-schema-versioning.md).

The rebuild path is conditional, not a failsafe. Scoping rebuild to one consumer keeps unrelated data intact, but the consumer must still prove its source coverage. `TransactionFeesConsumer` version 2 is row-compatible with version 1: new writes omit unprovable shielded fees, readers suppress legacy values, and missing or partial input rows recover from retained parent transaction facts without clearing the projection. The `transparent_outpoint_spend` consumer has a stricter limit: the canonical safe-tip sweep deletes source rows behind its durable height, so an incompatible schema bump can escalate to a full canonical re-ingest ([ADR-0029](../adrs/0029-durable-transparent-outpoint-spend-projection.md)).

`TransactionHistoryConsumer` version 1 is the reference additive projection
with atomic projection state. It owns the `transaction_history` column family
and consumer cursor independently from `recent_transactions`, and uses its row
key as the authoritative transaction index. A background,
non-readiness-blocking verifier compares bounded height batches with canonical
transaction facts, resumes after the last verified height, and publishes a new
coverage boundary only when the canonical epoch and projection head still match
the values observed before verification. Normal chain-event dispatch advances
rows, projection state, coverage, and cursor atomically. This establishes
height-1-through-tip completeness without replacing the canonical or derive
volume.

`CommitmentRootSearchConsumer` version 1 owns three column families: newest-first
root matches, per-height deletion keys, and contiguous historical coverage.
Normal chain-event dispatch tolerates a not-yet-enriched canonical height and
materializes an empty height row; the ingest-owned backfill later replaces it
from the canonical root artifact. Reorg rewind deletes every protocol match for
the reverted height. Negative search results are authoritative only when the
response reports complete coverage through the settled tip and contiguous
recent rows through the visible tip.

Introducing this consumer does not force unrelated block consumers to replay
from retained history. Before the tailer starts, ingest may seed only the new
consumer's event cursor from the unanimous cursor of every pre-existing bundled
block consumer. It does so only while the root consumer has no cursor of its
own. A missing or disagreeing predecessor cursor leaves the root cursor unset
and retains the generic full-replay behavior. The cursor seed establishes the
future event boundary; it never claims historical root coverage, which remains
owned by the separate contiguous backfill row.

This lets explorers iterate on dashboards without touching canonical storage and without coordinating with wallet sync.

`TransactionComponentSummaryConsumer` version 2 owns per-block component
contributions, a height index, UTC-day aggregates, historical coverage, and a
separate live-tail coverage row. It supplies exact half-open time-range totals
for transparent inputs and outputs, Sapling spends and outputs, Orchard and
Ironwood actions, Sprout JoinSplits, legacy Sapling/Orchard classifications,
and neutral transaction predicates. The protocol-scoped fields are named
`sapling_orchard_or_ironwood_transaction_count`,
`non_coinbase_without_sapling_orchard_or_ironwood_transaction_count`,
`non_coinbase_sapling_orchard_or_ironwood_with_transparent_inputs_and_outputs_transaction_count`,
and
`non_coinbase_sapling_orchard_or_ironwood_without_transparent_inputs_or_outputs_transaction_count`.
Their scope, coinbase handling, and Sprout-only behavior are therefore visible
at the call site. Transactions with unsupported sections contribute only to
`transaction_predicate_unavailable_count`; predicate totals are exact only when
that count is zero. Version 2 appends those predicate counters to fixed-width rows, so
opening a version-1 store clears this consumer's rows and cursor only; its
existing canonical-artifact backfill and live tail then rebuild it. Reorgs
remove the reverted block contribution and recompute affected day extrema
before applying replacements.

Schema version 2 is a coordinated outage: stop the writer and every derive
secondary, take a checkpoint, deploy version-2 binaries, start the version-2
writer to reconcile the primary, then start version-2 readers. Reader-first
rolling and side-by-side version-1/version-2 access are invalid.

Existing stores use a two-owner bootstrap rather than replaying every bundled
consumer. The historical worker reads canonical artifacts from height 1
through the height immediately before the durable tail boundary. Before the
chain-event tailer starts, ingest seeds the already-visible unsettled range
into the new consumer without advancing its inherited cursor. Those rows then
belong to normal reorg dispatch. Startup may widen an older tail boundary and
revalidate preserved contribution rows; it resets only tail progress metadata,
not the projection or canonical store. The historical and live ranges join
only when contiguous, so a crash or partial seed produces explicit incomplete
coverage rather than a false complete result.

`ConventionalFeeDistributionConsumer` version 1 follows the same split-tail
bootstrap contract, but stores a different reusable fact: exact frequency
counts of ZIP-317 conventional fees. It owns one block-time-keyed contribution
per canonical block, a height index for rewind, one aggregate row per UTC day,
and historical/live-tail coverage. Each non-coinbase transaction contributes
one frequency count derived from its component shape. Transactions with an
unsupported section contribute only to an explicit unavailable count; the
consumer never substitutes or labels a paid fee.

The per-block contribution is retained even after its day aggregate is built.
This makes rollback exact when a reorg crosses a UTC boundary and lets clipped
first or last query days use block-time predicates while complete middle days
use one aggregate read each. Backfill coverage advances atomically with each
canonical batch. Startup seeds the already-visible tail before inheriting the
unanimous event cursor, and the background worker fills height 1 through the
height immediately before that tail. Adding this consumer creates only its
four derive column families; it does not change canonical schema or clear any
existing consumer.

`PaidFeeDistributionConsumer` version 1 is deliberately separate from the
conventional-fee projection. It combines resolved transparent inputs and
outputs with schema-15 `TransactionIntrinsicValueBalances` artifacts, excludes
coinbase and proven zero-fee transactions, and records exact positive
miner-collected fee frequencies. Missing prevouts or intrinsic artifacts
increase an unavailable count instead of falling back to a conventional fee.

Startup seeds the visible tail before assigning the existing unanimous event
cursor. Historical coverage then prepends settled blocks newest-first, allowing
short recent periods to become complete while the configured 365-day window
continues filling. The durable target floor can move only backward when an
operator increases retention. Each batch validates source block identity,
enriches missing canonical intrinsic facts in place, and atomically advances
projection coverage; it never clears unrelated derive consumers or recreates
the canonical volume.

`TransparentAddressRankingConsumer` version 2 owns an immutable active
generation, an optional in-progress generation, balance-ordered rows, lifetime
address summaries, concentration totals, P2PKH/P2SH address and balance totals,
and per-height undo journals. Existing
stores bootstrap without replaying unrelated consumers: ingest snapshots the
settled canonical address-output projection, reconciles it against complete
transparent-address deltas through the same height, applies the visible
unsettled tail into the inactive generation, and atomically activates that
generation at the unanimous chain-event cursor. A crash resumes or discards
only the inactive generation; readers continue using the previous active
generation. Normal chain-event dispatch and undo journals own updates after
activation.

Ranking activation requires exact balance and lifetime coverage. Missing
historical delta coverage leaves the capability unavailable instead of
publishing partial lifetime totals. This is an additive derive schema: it does
not change `ChainEpoch::artifact_schema_version`, rewrite canonical rows, or
require a canonical volume wipe.

## Operator surface

A derive consumer ships its own ops endpoints (`/healthz`, `/readyz`, `/metrics`) on a dedicated listener, separate from `zinder-query`'s. The conventions in [Service Operations](service-operations.md) apply: typed readiness causes, structured `/readyz` body, Prometheus metrics with the consumer's product prefix (e.g. `zinder_explorer_*`).

Configuration follows the canonical TOML conventions ([Public Interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions)):

```toml
[ops]
listen_addr = "127.0.0.1:9069"   # shared section; "" disables the endpoint

[storage]
path = "/var/lib/zinder/store"
secondary_path = "/var/lib/zinder/explorer-secondary"

[explorer]
listen_addr = "127.0.0.1:9068"
bearer_token_path = "/run/secrets/zinder-explorer-token"
wallet_query_endpoint = "https://zinder.example:9101"   # zinder-query gRPC

[explorer.retention]
view_retention_days = 365
```

When `explorer.bearer_token_path` is set, `zinder-explorer` enforces the same shared-secret bearer-token interceptor used by private Zinder control planes ([ADR-0006](../adrs/0006-ingest-control-transport-security.md)). The explorer's `wallet_query_endpoint` points back at `zinder-query` for its wallet-composed reads.

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
- [Wallet data plane §Transparent Address Balance](wallet-data-plane.md#transparent-address-balance) — a wallet-plane read that composes a canonical sum with a live-mempool overlay.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — the capability protocol derive consumers must implement.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions that derive consumers inherit.
