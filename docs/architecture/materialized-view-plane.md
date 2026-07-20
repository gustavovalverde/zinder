# Materialized-view plane

The materialized-view plane builds explorer-shaped indexes and aggregates from
canonical artifacts and retained events. It is optional, independently
versioned, and rebuildable. A materialized-view failure must not change
canonical truth or wallet projection state.

The implementation lives in `zinder-materialized-views`. The crate owns the
consumer SDK, the materialized-view RocksDB wrapper, bundled explorer
consumers, per-consumer schema manifests, cursors, coverage, and read
snapshots. `zinder-explorer` is the primary reader of those views.

This subsystem is not the wallet projection. Wallet state is owned by
`zinder-wallet-projection`, `zinder-wallet-rocksdb`, and `zinder-projector`.
Those crates do not depend on `zinder-materialized-views`.

## Deployment status

The materialized-view and explorer services compile in the workspace but are
not part of the three-runtime release composition. The checked release topology
does not start `zinder-explorer` or expose its native query service. The SDK's
public Rust surface is a current code contract; it does not imply release-image
or deployment support.

## Ownership

A materialized view is appropriate when the value is a query-specific
aggregation, ordering, summary, or index that can be reconstructed from named
canonical inputs. Examples include transaction history, address activity,
block summaries, fee distributions, reorg incidents, value-pool history, and
time-indexed block production.

A value belongs in canonical storage when it is immutable source truth needed
by more than one consumer, required for wallet correctness, or required to
rebuild a projection without contacting the node. Consumer presentation,
ranking, rolling windows, and product-specific formulas do not belong in
canonical storage.

```text
Immutable reusable source fact?
├── yes -> canonical storage
└── no
    └── deterministic query-specific view? -> materialized-view consumer
```

Materialized-view consumers do not import `zinder-source` or call Zebra. If a
rebuild needs an upstream fact that canonical storage does not retain, the
canonical source and artifact contract must be extended first.

## Consumer contract

`MaterializedViewConsumer` is the event-level interface. It applies committed
and reorged chain events through a `MaterializedViewConsumerCtx` that owns the
pending RocksDB batch.

`BlockKeyedConsumer` is the standard per-block interface. Implementations apply
and revert one `BlockCommitContext`; a blanket implementation supplies the
event range loops. `BlockCommitContext` carries the shared block identity,
time, transaction facts, and any hydrated spend facts required by the selected
consumers. The host hydrates a context once and shares it across consumers.

`MaterializedViewMempoolConsumer` handles typed mempool events for views that
include unconfirmed activity. Chain and mempool cursors remain separate because
chain state rewinds on reorg while the mempool event sequence does not.

Every consumer declares one `MaterializedViewConsumerSchema` containing:

- a stable `MaterializedViewConsumerName`;
- a monotonically increasing schema version;
- the complete set of owned column families; and
- the single row version admitted by the current reader.

Column-family ownership must be disjoint. Consumer names are persisted keys,
not display labels, so renaming one is a storage migration rather than a source
cleanup.

## Store contract

`MaterializedViewStore` is a separate RocksDB instance located under
`MATERIALIZED_VIEW_STORE_SUBDIR`, currently `materialized-views`, beneath the
configured canonical path. Writer and reader processes resolve that path
through `MaterializedViewStore::path_for_canonical`.

The primary stages consumer rows, materialized-view state, and cursor advances in one
write batch. A crash cannot publish a cursor beyond the rows it describes. A
secondary validates the container and every declared consumer after open and
after each catch-up; it never alters schemas or writes primary state.

`MaterializedViewStore::read_snapshot` binds materialized-view metadata and reads to
one store sequence. Primary stores use a RocksDB snapshot. Secondary stores
hold the shared side of the catch-up barrier for the snapshot lifetime, so a
catch-up cannot advance the underlying sequence halfway through a multi-read
response.

## Replay and coverage

The host feeds retained `ChainEventEnvelope` values in order, hydrates the
blocks named by each event, and calls
`MaterializedViewStore::write_chain_event`. Rebuild starts from the earliest
point covered by the consumer's declared recovery source, not automatically
from the oldest retained event.

Consumers that make completeness claims persist `MaterializedViewState`
beside their rows. It records the canonical epoch, materialized-view tip, revision,
and optional contiguous `MaterializedViewCoverage`. Cursor position alone is
progress evidence and must not be presented as historical completeness.

A deterministic recovery source can be retained events, canonical artifacts,
or an explicit checkpoint. Before activating a replacement store, operators
must prove that the declared source covers the history being rebuilt. This
prevents a replacement from publishing partial history behind a fresh cursor.

## Schema lifecycle

`MATERIALIZED_VIEW_STORE_FORMAT_VERSION` versions shared container state:
manifest layout, cursor encoding, and metadata families. Every opener rejects a
container mismatch without mutation. Operators create a fresh materialized-view
path and rebuild from a certified recovery source; no service deletes an
existing materialized-view directory during open.

Individual row layouts use per-consumer versions. A fresh store records the
complete manifest atomically. Every later primary or secondary open requires
the exact consumer names, versions, owned column families, and physical
column-family set. Any divergence fails without mutation and requires a fresh
store rebuilt from a certified recovery source. The full decision is recorded
in [ADR-0028](../adrs/0028-materialized-view-schema-versioning.md).

## Key codecs and reorgs

Persisted keys use codecs from `zinder-core::wire`. Heights, positions,
timestamps, address script hashes, and outpoints are not encoded ad hoc inside
consumers.

Each block-keyed consumer must be able to delete exactly the rows produced by a
reverted block. Height-prefixed layouts use bounded range deletes. Layouts
whose primary key does not begin with height maintain a per-height index of the
keys written by that block. Reorg deletion and replacement rows share the same
batch as the cursor and materialized-view-state transition.

## Query exposure

`zinder-explorer` exposes materialized views through `ExplorerQuery` and
advertises a capability only when its dependencies and coverage support the
method. An unavailable materialized-view store maps to the stable
`MATERIALIZED_VIEW_UNAVAILABLE` vocabulary. Missing data is never translated
into zero, an empty complete result, or canonical absence.

The Cipherscan adapter may translate those explorer methods into product
routes, but Cipherscan names and response shapes stop at the adapter boundary.
No materialized-view consumer may shape canonical storage around an external
product contract.

## Extension checklist

When adding a consumer:

1. identify the complete canonical recovery source and retention boundary;
2. choose a stable consumer name and owned column families;
3. add reusable key codecs to `zinder-core::wire`;
4. implement apply and revert behavior in one atomic batch;
5. persist truthful materialized-view coverage when the public method needs it;
6. define the fresh-store recovery behavior for a schema change;
7. expose the method under an explorer capability; and
8. test replay, reorg, crash recovery, secondary catch-up, and incomplete
   coverage refusal.

See [ADR-0017](../adrs/0017-materialized-view-consumer-and-key-codec.md)
for the consumer template and [Explorer plane](explorer-plane.md) for public
query behavior.
