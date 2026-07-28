# ADR-0031: Materialized-view checkpoints and backfill coverage

| Field | Value |
| --- | --- |
| Status | Accepted |
| Product | Zinder |
| Domain | Materialized-view plane, public reads, and backfills |
| Related | [ADR-0011](0011-explorer-freshness-envelope.md), [ADR-0028](0028-materialized-view-schema-versioning.md), [Materialized-view plane](../architecture/materialized-view-plane.md) |

## Context

Additive materialized views may seed a live tail and backfill settled history independently.
A shared dashboard tip cannot prove that one materialized view is contiguous,
complete, or from the same read view as its rows and counts.

## Decision

Every independently backfilled consumer persists its materialized-view epoch,
tip height and hash, revision, and optional contiguous coverage. Endpoint
composition derives immutable capability admission from structural evidence:
the exact installed consumer manifest and any other concrete provider required
by the method. A public request reads consumer state, rows, joins, and exact
counts from one materialized-view snapshot. Opaque cursors bind the request
filters and materialized-view fence; stale fences fail closed. Mutable
coverage never selects capabilities. A request that promises completeness
requires verified contiguous coverage through the fenced tip with a matching
hash and otherwise returns the method's typed materialization outcome.

Backfills are writer-owned, resumable, cancellation-aware, bounded by a global source-request budget, and revalidate canonical identity before publishing progress. A live-tail seed and a historical prepend join only when their boundary is contiguous and hash-consistent.

## Consequences

- Canonical schema, ingest readiness, and block-summary freshness do not substitute for materialized-view evidence.
- Restart resumes durable progress without clearing unrelated consumers.
- Agents and clients can distinguish partial, stale, and complete results mechanically.
