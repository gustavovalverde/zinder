# ADR-0011: Cross-plane chain view and explorer freshness

| Field | Value |
| --- | --- |
| Status | Accepted |
| Domain | Response freshness, chain-state identity, optional explorer fields |
| Related | [Public interfaces](../architecture/public-interfaces.md), [Explorer plane](../architecture/explorer-plane.md), [Error vocabulary](../reference/error-vocabulary.md) |

## Context

Wallet, explorer, and ingest-control responses need one interpretation of the
chain state they describe. A visible chain epoch, materialized-view indexed tip,
upstream observation, and replay health are independent axes; collapsing them
into a single height or lag number makes absence indistinguishable from “at
tip.” Explorer responses also need per-call metadata such as mempool snapshot
age and structured reasons for optional fields that could not be populated.

## Decision

`zinder.v1.wallet.ChainView` is the shared chain-state envelope. Read responses
carry it at their surface's field tag 1, either directly or through
`ExplorerFreshness`:

```proto
message ChainView {
  ChainEpoch chain_epoch = 1;
  optional IndexedTip indexed_tip = 2;
  optional UpstreamTip upstream_tip = 3;
  optional MaterializedViewStatus materialized_views = 4;
}
```

Each field has one meaning:

- `chain_epoch` identifies the canonical snapshot and its visible and settled
  tips.
- `indexed_tip` identifies the highest block reflected by materialized-view
  rows. Absence means unknown or not applicable, never caught up.
- `upstream_tip` reports the asynchronously observed upstream committed and
  estimated heights. Absence means no observation is available.
- `materialized_views` reports replay health and observation time. It is absent
  when a response does not depend on materialized views.

Wallet reads populate the canonical axis and any materialized-view axes they
actually read. Explorer reads construct all axes from the same pinned canonical
and materialized-view snapshots used for the response. Ingest-control status
populates the axes owned by the writer. A response must not combine an epoch
from one snapshot with an indexed tip from another.

`ExplorerFreshness` adds only metadata that varies by explorer call:

```proto
message ExplorerFreshness {
  zinder.v1.wallet.ChainView chain_view = 1;
  uint64 snapshot_age_millis = 2;
  string capability_version = 5;
  repeated UnavailableField unavailable = 6;
}
```

`snapshot_age_millis` measures time since the current mempool source generation
was certified, including for a certified empty mempool, and is zero for
responses that do not touch mempool state. `capability_version` is the exact advertised capability that
produced the response. `UnavailableField` carries a proto field path, a typed
`UnavailableReason`, and a stable human-readable explanation. Optional fields
stay absent when their source fact, parser support, privacy boundary, or
materialized-view coverage is unavailable; zero is never used as an absence
sentinel.

Materialized-view coverage is explicit in `chain_view.indexed_tip`. Requests
whose required materialized view is unavailable use
`MATERIALIZED_VIEW_UNAVAILABLE`; missing storage or wiring uses
`DEPENDENCY_NOT_CONFIGURED` as defined by the
[error vocabulary](../reference/error-vocabulary.md).

## Consequences

- Every consumer reads canonical, upstream, and materialized-view freshness
  through the same field names and absence semantics.
- Explorer handlers use one shared freshness builder so response metadata
  cannot drift by RPC.
- Clients can distinguish privacy-preserving absence, incomplete source facts,
  unsupported parsing, and lag without parsing human text.
- Adding an axis or unavailable reason is an additive wire change; existing
  protobuf tags and enum values remain stable.

## Rejected alternatives

- A single “current height” cannot distinguish canonical visibility, upstream
  progress, and indexed coverage.
- A scalar lag without the indexed block identity makes absent and zero
  ambiguous and cannot prove which branch was indexed.
- Nullable payload fields without structured reasons force each client to
  invent incompatible error and privacy language.
