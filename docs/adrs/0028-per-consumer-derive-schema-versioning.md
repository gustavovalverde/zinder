# ADR-0028: Per-Consumer Derive Schema Versioning

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Derive-store schema lifecycle, consumer registration, open-time reconciliation |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md), [Derive plane](../architecture/derive-plane.md) |

## Context

Every derive projection is rebuildable from retained canonical events; that is
the plane's defining invariant. The schema gate that protects those
projections must therefore decide only *what* rebuilds, never *whether*
rebuild is possible.

A single store-global derive schema version couples that decision to the
wrong granularity. One consumer changing its key or payload layout invalidates
every consumer's data at once: the whole derive store is wiped and every
projection replays from the retention floor. Once canonical retention is
clamped to derive progress, a whole-store wipe escalates further, forcing a
from-genesis canonical re-ingest for a change that touched one column family.

Consumers already own disjoint column families and independent cursors
([ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md)). The
versioning unit should match the ownership unit.

## Decision

Each derive consumer versions its own on-disk layout. The store persists a
per-consumer schema manifest and, at open time, scopes wipe-and-rebuild to
exactly the consumers whose declared version moved. A store-global
container-format version remains, narrowed to the parts every consumer
shares.

### Registration declares name, version, and column families together

`DeriveConsumerSchema` is the registration unit: a consumer's stable
`DeriveConsumerName`, its `schema_version` (`u16`), and the column families it
owns. `DeriveStoreOptions::consumers` takes a slice of these declarations;
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
length-prefixed: the consumer's schema version (`u16` big-endian), a column
family count (`u16`), then each owned column-family name as a `u16` length
plus bytes. Encoding rejects a declaration that exceeds the count or length
fields; decoding rejects a truncated entry instead of silently reading a
partial column-family list, so a torn manifest row fails the open loudly
rather than leaking a column family past reconciliation.

The manifest records the column families the consumer owned *when the row was
written*, which is what lets a later open find and drop families the current
binary no longer declares.

### Open-time reconciliation (primary)

After validating the container-format version, the primary open compares each
declared consumer against the manifest:

- **Version matches.** The consumer's column families and cursor are
  byte-preserved.
- **Version moved.** The consumer rebuilds, in this order: reset its chain and
  mempool cursors, clear the rows of manifest-recorded column families it no
  longer declares, clear the rows of its declared column families, then write
  the manifest entry at the new version last.
- **Recorded but no longer declared.** Reset its cursors, clear the rows of its
  recorded column families, then remove its manifest entry last.
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
therefore lists the on-disk column families and includes any not covered by
the store tables or the declared consumers, so an emptied orphan keeps opening
across restarts without being re-recorded in the manifest. A column family
whose owning consumer changes is cleared for the new owner, not handed off with
its rows: the new owner starts from an empty projection and rebuilds from
retained events. Cross-consumer row migration is deliberately not an affordance,
because retained rows behind a fresh cursor would double-apply a non-idempotent
projection and never rebuild a differing key or payload layout.

Both reconciliation outcomes that destroy state emit a `tracing` warning
(`consumer_schema_rebuild`, `consumer_dropped`) naming the consumer and the
version transition, so an operator-visible rebuild is never silent.

### Secondary readers validate, never reconcile

A secondary reader cannot write
([ADR-0003](0003-canonical-storage-access-boundary.md)), so it cannot
rebuild. Secondary
open requires the persisted container version to equal
`DERIVE_STORE_FORMAT_VERSION` and every declared consumer's version to equal
its manifest entry. A divergence fails with `SchemaMismatch` or
`ConsumerSchemaMismatch` (carrying the persisted version, or `None` when the
primary has not recorded the consumer), and the caller retries until the
primary has reconciled and rewritten the manifest. A reader never decodes
rows written under a layout it does not declare.

### Container-format version, narrowed

`DERIVE_STORE_FORMAT_VERSION` gates only the shared container: the manifest
layout, the cursor encoding, and the metadata column family. Bumping it wipes
the whole derive store, because no consumer's data survives a change to the
shared format. A consumer changing its own key or payload layout bumps its
own `schema_version` and never the container version.

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

- **A consumer schema bump rebuilds only that consumer's projection.** Its
  cursor resets and its column families' rows clear and replay from the
  earliest retained canonical event; every other consumer's rows and cursor
  are untouched. The derive store as a whole is wiped only by a
  container-format change.
- **Deleting a consumer clears its rows immediately and reclaims its physical
  storage at the next container rebuild.** A consumer removed from the
  declaration set has its rows cleared, its cursor reset, and its manifest
  entry removed on the next primary open, so it no longer serves reads. Its now
  empty column family stays on disk as an orphan until a container-format
  change wipes the whole derive directory, because dropping the family in place
  would crash any attached secondary reader.
- **Consumer error variants change.** `SchemaMismatch` describes only the
  container version; `ConsumerSchemaMismatch` and `SchemaReconcile` cover
  per-consumer divergence and reconciliation failures.
- **Rebuild depth is bounded by canonical retention.** A scoped rebuild
  replays retained canonical events; a consumer that must rebuild past the
  retention floor still requires the operator cold-start path in the
  [derive plane](../architecture/derive-plane.md). Scoping does not change
  that bound; it changes how many consumers pay it at once.
- **Version numbers move forward by convention, not mechanism.** The store
  rebuilds on any inequality, so an accidental downgrade also rebuilds.
  Consumers only ever increment their version.
