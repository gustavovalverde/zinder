# ADR-0030: Compatible canonical artifact enrichment

| Field | Value |
| --- | --- |
| Status | Accepted |
| Product | Zinder |
| Domain | Canonical storage and schema evolution |
| Related | [Storage backend](../architecture/storage-backend.md), [Extending artifacts](../architecture/extending-artifacts.md) |

## Context

Some product facts are canonical but were not retained by earlier writers. Rebuilding the whole store for every additive family is operationally expensive, while silently treating missing historical rows as zero is incorrect. Schema 13 is already owned by Orchard and Ironwood transaction facts, so later features must not reuse it.

## Decision

The canonical writer owns one monotonic artifact-schema ladder: schema 14 adds final note-commitment roots, schema 15 adds transaction-intrinsic value balances, schema 16 adds per-block value-pool balances, and schema 17 adds displaced-block final roots and indexes. New families are optional for older readable epochs, use exact block or transaction identity, and support bounded idempotent enrichment against a pinned canonical epoch. The next canonical commit stamps the current schema and older binaries fail closed.

Absence remains explicit. A read may bridge an unsettled enrichment gap only from already-retained canonical bytes at the same epoch. It must not call the source, fabricate zero, or mix chain views.

## Consequences

- Schema numbers have one owner in `zinder-store`; feature tests use named current or historical constants.
- Writer and readers upgrade as one coordinated service set with a canonical-plus-materialized-view checkpoint.
- Enrichment progress is separate from canonical readiness and is exposed through feature coverage.
