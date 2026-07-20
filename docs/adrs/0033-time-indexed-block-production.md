# ADR-0033: Time-indexed block production

| Field | Value |
| --- | --- |
| Status | Accepted |
| Product | Zinder |
| Domain | Materialized-view plane and explorer reads |
| Related | [ADR-0028](0028-materialized-view-schema-versioning.md), [ADR-0031](0031-materialized-view-checkpoints-and-backfill-coverage.md), [Explorer plane](../architecture/explorer-plane.md) |

## Context

Block headers permit equal and non-monotonic timestamps. A height-ordered scan can answer a bounded recent-block request, but it cannot prove that every block in an arbitrary wall-clock interval was included. Mining, activity, and operational products need reusable time-range reads without importing product labels or presentation formulas into Zinder.

Long time ranges also cross ordinary tip advancement. Requiring every continuation page to match the latest tip makes a complete scan unreliable, while accepting a changed canonical prefix could mix two histories.

## Decision

Zinder materializes one product-neutral `BlockProductionTimeConsumer` row per canonical block. The primary key is signed block timestamp, height, and block hash. A reverse height index supports deterministic reorg removal. The row value contains only an encoding version; coinbase identity, paid fees, pool labels, and other enrichments remain in their owning stores.

The consumer persists a materialized-view epoch, tip height and hash, revision, full-history backfill coverage, and live-tail coverage. Chain-event rows, reverse-index changes, materialized-view state, cursor advancement, and reorg coverage correction commit atomically. A background backfill reads existing `BlockSummaryRecord` rows in bounded batches, performs no upstream refetch, and remains available to repair coverage after a later tip-reducing reorg. Ingest readiness does not depend on historical backfill completion.

`ExplorerQuery.BlockProductionInTimeRange` exposes a half-open timestamp range ordered by timestamp, height, and hash. One materialized-view snapshot supplies index rows, block summaries, paid-fee facts, materialized-view state, and coverage. A canonical epoch reader validates headers and coinbase artifacts. Missing block, coinbase, and paid-fee facts are counted explicitly.

The first page freezes the materialized-view tip. Continuation cursors bind the request bounds, chain epoch, materialized-view revision, and frozen tip. Later pages may continue while the chain extends only when the frozen tip hash remains canonical; rows above the frozen height are excluded. A reorg at or below the frozen tip invalidates the cursor.

Pool attribution remains adapter-owned. Zinder exposes canonical payout facts and exact paid fees, while a product adapter maps payout addresses to product labels and response shapes.

## Consequences

- Arbitrary time windows have explicit full-height-domain completeness instead of a timestamp-monotonicity assumption.
- Equal timestamps and future-dated canonical blocks remain queryable without special cases.
- Long scans have stable membership across normal chain growth and fail closed across relevant reorgs.
- Adding the projection requires a fresh materialized-view store rebuilt from a certified canonical recovery source; it does not replace canonical storage.
- Products can reuse the native time-range contract without inheriting Cipherscan naming, labels, caching, or aggregation policy.
