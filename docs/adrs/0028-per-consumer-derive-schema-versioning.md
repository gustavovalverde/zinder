# ADR-0028: Per-Consumer Derive Schema Versioning

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Derive-store schema lifecycle, consumer registration, open-time reconciliation |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md), [ADR-0029](0029-durable-transparent-outpoint-spend-projection.md), [Derive plane](../architecture/derive-plane.md) |

## Context

Every derive projection must have a deterministic recovery path, but retained
canonical events are not sufficient for every projection at every height.
Some consumers join event-scoped rows to canonical facts with shorter
retention. A projection row can therefore outlive one of the inputs that first
produced it. Before clearing a consumer, the schema gate must establish both
*what* rebuilds and whether the declared recovery source can reproduce the
preserved history.

`TransactionFeesConsumer` exposed the concrete failure mode. Its version-1
rows retained resolved transparent input values below the transparent-spend
retention floor. Clearing those rows and replaying retained events produced
`PARTIAL` replacements because the event hydration path no longer found the
short-lived spend facts. The immutable parent `TransactionFactsArtifact` rows
could still reconstruct those values, but the blanket clear-first migration
discarded useful data before proving that fallback.

A single store-global derive schema version couples that decision to the
wrong granularity. One consumer changing its key or payload layout invalidates
every consumer's data at once: the whole derive store is wiped and every
projection replays from the retention floor. Because canonical retention is
clamped to durable derive progress
([ADR-0029](0029-durable-transparent-outpoint-spend-projection.md)), a
whole-store wipe escalates further, forcing a from-genesis canonical re-ingest
for a change that touched one column family.

Consumers already own disjoint column families and independent cursors
([ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md)). The
versioning unit should match the ownership unit.

## Decision

Each derive consumer versions its own persisted row contract. The store
persists a per-consumer schema manifest and defaults incompatible forward
changes to a scoped rebuild. A consumer may explicitly declare older row
contracts compatible when the column-family set and payload encoding are
unchanged and the new reader safely interprets both meanings. Compatible
upgrades preserve rows and cursors while advancing the manifest. A
store-global container-format version remains, narrowed to the parts every
consumer shares.

### Registration declares name, version, and column families together

`DeriveConsumerSchema` is the registration unit: a consumer's stable
`DeriveConsumerName`, its `schema_version` (`u16`), the column families it
owns, and optional older `row_compatible_versions`.
`DeriveStoreOptions::consumers` takes a slice of these declarations;
`DeriveStore::bundled_consumers()` returns the bundled set, each starting at
version 1. The constructor requires the version, so a column family cannot be
registered without one.

Open rejects a declaration set whose column families are not disjoint, or
that reuses a store-table name or the `RocksDB` default family
(`DeriveStoreError::ConsumerColumnFamilyConflict`). A name shared by two
declarations would let one consumer's rebuild or removal clear another's rows
behind a cursor that never rewinds; rejecting at open time keeps that
impossible.

### Persisted manifest

The `consumer_metadata` column family holds one manifest row per consumer,
keyed by a reserved prefix plus the consumer name. The payload is
length-prefixed: the latest writer schema version (`u16` big-endian), a column
family count (`u16`), each owned column-family name as a `u16` length plus
bytes, then the sorted set of row schema versions still present. Legacy rows
without the final set decode as containing only their recorded writer version.
Encoding rejects declarations that exceed the count or length fields;
decoding rejects truncation, trailing bytes, an empty provenance set, or a row
version newer than the writer.

The manifest records the column families the consumer owned *when the row was
written*, which lets a later open distinguish a layout change from harmless
declaration reordering and reject an undeclared owner without touching it.

### Open-time reconciliation (primary)

After validating the container-format version, the primary open compares each
declared consumer against the manifest:

- **Version and column-family set match.** The consumer's column families and
  cursor are byte-preserved.
- **Older row-compatible version with the same column-family set.** Rows and
  cursors are byte-preserved. Write the current writer version plus the union
  of prior row versions and the current version. Compatibility must cover every
  persisted row version, not only the immediately preceding writer. The
  manifest-only promotion is idempotent and crash-safe.
- **Older incompatible version or changed column-family set.** The consumer
  rebuilds, in this order: reset its chain and
  mempool cursors, clear the rows of manifest-recorded column families it no
  longer declares, clear the rows of its declared column families, then write
  the manifest entry at the new version last.
- **Persisted version newer than the running binary.** Fail with
  `ConsumerSchemaMismatch` before reconciliation mutates the store. A rollback
  must never reinterpret a newer row contract as permission to rebuild it.
- **Recorded but no longer declared.** Fail with `ConsumerNotDeclared` before
  mutation. Absence may mean rollback or configuration drift; it is not an
  explicit destructive migration. Removal requires a separate store-format
  migration that names the retired consumer, clears its rows and cursors
  atomically, documents rollback, and is coordinated across every reader and
  writer. Until that migration exists, `recent_transactions` and every other
  recorded consumer remain declared even when a newer projection overlaps
  their product use case.
- **Declared but not recorded.** Clear the rows of its declared column
  families, then write its manifest entry at the declared version. Clearing
  starts the consumer from an empty projection, so a family that previously
  belonged to another consumer replays from the earliest retained event instead
  of serving the prior owner's rows behind a fresh cursor. Its column families
  were already created by the open itself, which opens every declared family
  with create-if-missing.

The ordering is the crash-safety argument. The cursor reset comes first and
the manifest write comes last, so a crash at any intermediate point leaves
the manifest recording the old version: the next open re-runs the same
rebuild from the top. Because the cursor is already `None`, replay resumes
from the earliest retained event and overwrites whatever partial state the
interrupted rebuild left, rather than skipping the gap behind a stale cursor.

Reconciliation clears rows; it never drops a column family in place. A row
clear is a range tombstone over the full key span plus a point-delete sweep of
any residue above the range's exclusive upper bound, which leaves the family
indistinguishable from a freshly created one. An in-place `drop_cf`/`create_cf`
records a column-family edit in the `RocksDB` manifest, and a secondary reader
replaying that edit during catch-up crashes; range tombstones and point deletes
replay as ordinary data writes. A cleared family that no declared consumer owns
becomes an empty orphan: its rows are gone immediately, and its physical
column family is reclaimed only when a container-format change wipes the whole
derive directory.

Every on-disk column family must be listed to open the database: `RocksDB`
refuses to open while leaving an existing column family unlisted. Open
therefore includes unknown on-disk families long enough to read the manifest
and fail closed without clearing them. Transferring a family between consumer
names is rejected for the same reason. Cross-consumer migration is deliberately
not inferred from declarations.

An incompatible reconciliation emits `consumer_schema_rebuild`, naming the
consumer and version transition, so an operator-visible rebuild is never
silent. A compatible promotion emits `consumer_schema_rows_preserved` at info
level and persists cumulative row provenance.

### Secondary readers validate, never reconcile

A secondary reader cannot write
([ADR-0003](0003-canonical-storage-access-boundary.md)), so it cannot rebuild.
Open and every later `try_catch_up` require the persisted container version to
equal `DERIVE_STORE_FORMAT_VERSION`. Each declared consumer must accept every
row version in its manifest and own the same column-family set. Unknown
manifest consumers and every other divergence fail with `SchemaMismatch`,
`ConsumerNotDeclared`, or `ConsumerSchemaMismatch`. A reader never continues
after catching up into rows it did not explicitly declare safe.

The first deployment of this rule remains reader-first because binaries from
before this ADR revision do not revalidate after catch-up. Once all readers run
this contract, future incompatible changes fail on the catch-up call itself.

### Container-format version, narrowed

`DERIVE_STORE_FORMAT_VERSION` gates only the shared container: the manifest
layout, the cursor encoding, and the metadata column family. Bumping it wipes
the whole derive store, because no consumer's data survives a change to the
shared format. A consumer changing its own key or payload layout bumps its
own `schema_version` and never the container version.

The cumulative row-version trailer is an optional extension to the existing
manifest row: the new decoder accepts both legacy rows and the extended form,
so introducing it does not invalidate any consumer payload or require a
container rebuild. New writes include the trailer. This is why the first
deployment must update readers before the writer rather than bumping the
container version and deleting the derive store.

The primary open performs that wipe itself, and only forward. A persisted
container version older than the running one deletes the derive directory and
reopens it fresh, emitting a `store_format_rebuild` warning naming the version
transition. The directory is deleted rather than dropped column family by
column family in place: an in-place drop records column-family drop edits in
the `RocksDB` manifest, and a secondary reader replaying those edits during
catch-up crashes. Deleting the directory yields a fresh manifest with no drop
history for the secondary to replay. The tailer then replays from the
retention floor because every cursor is gone. A persisted version *newer* than
the running one is left on disk and rejected with `SchemaMismatch`, so rolling
a binary back never destroys a store it cannot read; the older binary
crash-loops until the store-matching binary runs. Secondary readers never
wipe: they reject any container-version divergence and retry until the primary
has rebuilt.

Introducing the manifest is itself a container change, so the constant takes
a one-time bump to 7. That bump forces one final whole-store wipe; from
version 7 onward, per-consumer scoping applies.

## Consequences

- **An incompatible consumer schema bump rebuilds only that consumer's projection.** Its
  cursor resets and its column families' rows clear and replay from the
  earliest retained canonical event; every other consumer's rows and cursor
  are untouched. The derive store as a whole is wiped only by a
  container-format change.
- **Removing or renaming a consumer fails closed.** The running declaration
  must account for every manifest entry. A future removal feature must be an
  explicit migration rather than interpreting absence as permission to delete.
- **Consumer error variants change.** `SchemaMismatch` describes only the
  container version; `ConsumerSchemaMismatch` and `SchemaReconcile` cover
  per-consumer divergence and reconciliation failures.
- **A row-compatible semantic upgrade preserves useful historical rows and
  their provenance.** The new reader must sanitize or translate every row
  version recorded in the manifest before values cross a public boundary.
  Compatibility is cumulative and is never inferred from equal protobuf wire
  layout alone.
- **Rebuild depth is bounded by the consumer's proven recovery source.** A
  scoped rebuild may use retained events, canonical artifacts, or an explicit
  checkpoint. If the source does not cover the projection's persisted history,
  the migration must preserve compatible rows or require the operator cold-start
  path in the [derive plane](../architecture/derive-plane.md).
- **Consumer versions are monotonic in mechanism.** A persisted newer version
  fails closed; an accidental downgrade does not clear rows or cursors.
