# ADR-0028: Per-Consumer Materialized-View Schema Versioning

## Status

Accepted.

## Context

Materialized-view consumers own disjoint rows, cursors, and recovery sources.
A payload or key change in one consumer must not invalidate every other view.
Rebuild safety also cannot be inferred from retained chain events alone: a row
may depend on canonical inputs whose retention differs from the event log.

The versioning unit therefore needs to match the ownership and recovery unit.

## Decision

Each consumer registers one `MaterializedViewConsumerSchema` containing:

- a stable `MaterializedViewConsumerName`;
- a monotonic `schema_version`;
- the exact column families it owns; and
- any older `row_compatible_versions` its reader can interpret safely.

`MaterializedViewStoreOptions::consumers` is a closed declaration set for the
opened store. Consumer names and column-family ownership must be unique. The
`consumer_metadata` family persists one manifest row per consumer with its
writer version, owned families, and all row versions still present.

### Primary reconciliation

A primary open compares every declaration with the manifest:

- Matching versions and families preserve rows and cursors.
- A declared row-compatible upgrade with unchanged families preserves rows and
  cursors, then records the new writer version and cumulative row provenance.
- An older incompatible version or a changed family set resets that consumer's
  cursors, clears its previously and currently owned rows, and writes the new
  manifest last.
- A persisted newer version fails with `ConsumerSchemaMismatch` before any
  mutation.
- A recorded but undeclared consumer fails with `ConsumerNotDeclared`.
- A declared but unrecorded consumer starts with empty owned families and a new
  manifest row.

Cursor reset precedes row clearing, and manifest publication is last. A crash
during reconciliation therefore leaves the old manifest and a reset cursor, so
the next open repeats the rebuild rather than skipping a partially cleared
range.

Reconciliation clears rows with ordinary RocksDB writes; it does not drop and
recreate column families while secondaries may be following the store. Unknown
on-disk families are opened only so the manifest can be inspected and rejected
without data loss.

### Secondary validation

Secondary readers never reconcile. Open and every catch-up require:

- the exact `MATERIALIZED_VIEW_STORE_FORMAT_VERSION`;
- every manifest consumer to be declared;
- identical column-family ownership; and
- every persisted row version to be accepted by the running reader.

Any divergence fails closed. A secondary never continues after catching up into
rows it has not declared safe.

### Container format

`MATERIALIZED_VIEW_STORE_FORMAT_VERSION` versions only the shared container:
manifest layout, cursor encoding, and shared metadata families. The current
format is 7.

Opening a primary on an older container format rebuilds the entire
`materialized-views` directory and emits `store_format_rebuild`. Opening a newer
format fails without mutation. Secondary readers reject every container-format
divergence. Consumer key or payload changes increment only that consumer's
schema version.

## Consequences

- Incompatible changes rebuild one consumer instead of the whole store.
- Row-compatible readers can preserve historical values and record exactly
  which encodings remain present.
- Removing or renaming a consumer requires an explicit container migration;
  absence is never interpreted as permission to delete.
- A rebuild is allowed only when the consumer's declared recovery source covers
  the rows being discarded.
- Rollback cannot reinterpret or erase a newer consumer or container format.

## References

- [Materialized-view plane](../architecture/materialized-view-plane.md)
- [ADR-0003: Canonical storage access boundary](0003-canonical-storage-access-boundary.md)
- [ADR-0017: Materialized-view consumer and key codec](0017-materialized-view-consumer-and-key-codec.md)
- [ADR-0029: Durable transparent outpoint-spend projection](0029-durable-transparent-outpoint-spend-projection.md)
