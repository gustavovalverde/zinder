# ADR-0031: Projection checkpoints and backfill coverage

| Field | Value |
| --- | --- |
| Status | Accepted |
| Product | Zinder |
| Domain | Derive plane, public reads, and backfills |
| Related | [ADR-0011](0011-explorer-freshness-envelope.md), [ADR-0028](0028-per-consumer-derive-schema-versioning.md), [Derive plane](../architecture/derive-plane.md) |

## Context

Additive projections may seed a live tail and backfill settled history independently. A shared dashboard tip cannot prove that one projection is contiguous, complete, or from the same read view as its rows and counts.

## Decision

Every independently backfilled consumer persists its projection epoch, tip height and hash, revision, and optional contiguous coverage. A public request reads that state, rows, joins, and exact counts from one derive-store snapshot. Opaque cursors bind the request filters and projection fence; stale fences fail closed. Base capabilities may expose bounded partial data when coverage is returned. Completeness capabilities require verified contiguous coverage through the fenced tip with a matching hash.

Backfills are writer-owned, resumable, cancellation-aware, bounded by a global source-request budget, and revalidate canonical identity before publishing progress. A live-tail seed and a historical prepend join only when their boundary is contiguous and hash-consistent.

## Consequences

- Canonical schema, ingest readiness, and block-summary freshness do not substitute for projection evidence.
- Restart resumes durable progress without clearing unrelated consumers.
- Agents and clients can distinguish partial, stale, and complete results mechanically.
