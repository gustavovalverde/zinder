# Extending Canonical Artifacts

Canonical artifacts hold chain-derived facts whose identity, retention, and
reorg semantics belong to the canonical store. They are not a place for
presentation aggregates, node observations, or a convenience cache.

## Choose the right boundary

Add a canonical artifact only when the fact is authoritative chain data, has a
stable lookup identity, and must be retained independently of a particular
consumer. Extend an existing artifact when the fact shares its identity and
lifecycle. Put rebuildable aggregation in a materialized view; use a
response-level read model when the value can be derived from the pinned
`ChainEpoch` and existing canonical facts.

Source-derived facts enter through `zinder-source` and become canonical only
through ingest. A query handler must not fetch an upstream value, reread an
unpinned tip, or combine epochs to fill a canonical response.

## Invariants

- Canonical writes are owned by `zinder-ingest` and are committed atomically
  with the epoch they describe.
- Every canonical read is bound to a `ChainEpoch`; unpinned reads resolve one
  visible epoch before reading.
- The key shape determines reorg handling. Per-block rows are removed with a
  replacement. High-fanout rows require an epoch and canonical block-identity
  visibility check on every read.
- On-disk layout changes are admitted by the owning store schema. Unsupported
  directories fail closed; compatible data is never silently reinterpreted.
- A public wire surface is typed, capability-advertised, and documented with
  its availability and retention semantics. Compatibility adapters translate
  native facts but do not define them.

## Related boundaries

- [Wallet data plane](wallet-data-plane.md) defines wallet-facing reads.
- [Materialized-view plane](materialized-view-plane.md) defines rebuildable
  consumer state.
- [Storage backend](storage-backend.md) defines ownership, admission, and
  recovery.
- [Public interfaces](public-interfaces.md) defines names, epochs, cursors,
  errors, and capabilities.
