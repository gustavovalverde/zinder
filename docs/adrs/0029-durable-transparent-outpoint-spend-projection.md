# ADR-0029: Durable transparent-outpoint spend projection and retention release floor

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Canonical retention, materialized-view plane, wallet spend resolution |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0017](0017-materialized-view-consumer-and-key-codec.md), [ADR-0026](0026-utxo-set-commitment.md), [ADR-0028](0028-materialized-view-schema-versioning.md), [Wallet data plane §Transparent Reverse-Spend Resolution](../architecture/wallet-data-plane.md#transparent-reverse-spend-resolution) |

## Context

Canonical transparent-retention maintenance deletes a transparent spend fact and its
spent-output rows once the spend settles below the settled tip. `WalletQuery.TransparentSpendsByOutpoint`
then cannot resolve the spender of anything spent longer ago than the reorg
window, which breaks wallet offline-recovery: a wallet backend maps that RPC to
its spender-resolution path and expects an answer for arbitrarily old spends.

Spender identity must therefore outlive the canonical fact. The canonical store
cannot retain every spend fact forever without unbounded growth, and it must
stay ignorant of the materialized-view plane ([ADR-0003](0003-canonical-storage-access-boundary.md)).

## Decision

A bundled materialized-view consumer records durable spender identity from each child
transaction's intrinsic input and mined location. The canonical retention sweep
is clamped so it never deletes a spend fact before the consumer has durably
materialized the corresponding block.

### Authority split

Spent-versus-unspent is decided only by `TransparentUnspentOutputsByOutpoint`,
which is durable and LtHash16-committed ([ADR-0026](0026-utxo-set-commitment.md)).
This projection makes only *spender identity* durable; it never answers
spentness. A missing projection row means "no spender recorded here", never
"unspent". The union-routed read enforces this: it consults the projection only
for outpoints the canonical epoch-pinned read missed, and only surfaces a
projection hit whose spend settled at or below the pinned epoch's settled tip.

### The `transparent_outpoint_spend` consumer

A `BlockKeyedConsumer` ([ADR-0017](0017-materialized-view-consumer-and-key-codec.md))
keys primary rows on the spent outpoint (creating transaction id plus big-endian
output index) valued with the spending transaction id, spending block hash,
spending height, and transparent input index. A per-height index column family,
written for every applied block even when empty, drives reorg rewind and reports
the projection's durable height through `last_materialized_height_ascending`.
The consumer also persists `ConsumerProjectionState` in the same write batch as
its rows and chain-event cursor. Its coverage starts at the first committed
height actually materialized, advances only across a contiguous commit or
connected reorg replacement, and cannot be inferred from the latest index key.
Coinbase inputs carry no prevout and never produce a row. Parent-output
hydration is deliberately not an input to this projection: the child input
already identifies the spent outpoint, input index, spending transaction, and
mined block. A spend of an output below a checkpoint therefore produces the
same durable row as a spend whose parent output is retained. Missing or offline
parent facts must not cause the consumer to skip that row.

Artifact schema 18 stores every observed input and the resolved spend facts in
the canonical block-local spend index. Transparent-retention maintenance deletes the
per-outpoint serving row but retains that block record. A materialized-view consumer schema
bump may rebuild only when retained canonical events still begin at the durable
canonical history boundary; a retained suffix is not proof that every historical
row will be revisited. Finalized replay
verifies the block record's input set and producing-block identity against the
canonical transactions before advancing the durable projection height. Facts
whose parents predate a configured checkpoint remain explicitly unresolved;
they do not make the whole replay context unavailable.

### Retention release floor

The canonical store persists a `transparent_retention_release_height` marker
alongside the existing swept-through marker. A dedicated ingest maintenance
worker runs only after canonical and materialized views are caught up, independently of
chain commits, and clamps its ceiling to `min(current settled tip, release height)`, so a settled spend
above the release height stays retained. `zinder-ingest` publishes the release
height from the materialized-view tailer's verified contiguous coverage, so canonical
retention releases only what the projection proves it has recorded from the
canonical history boundary. A latest index height without that coverage holds
retention in place. A release
height below the swept marker is ignored safely; the sweep never regresses.

The release floor is a durability barrier, not just a progress signal. The
materialized-view store writes unsynced, so `zinder-ingest` fsyncs the materialized-view
write-ahead log before publishing a higher floor. Without that a host crash
could lose projection rows the floor already authorized the canonical sweep to
delete, stranding spender identities the guard cannot recover. Publication is
throttled: each floor advance costs one materialized-view fsync plus one synced canonical
write, and a floor that lags by the throttle interval only defers a sweep.

### Deleted-through marker and replay source

Deletion provenance is recorded, not inferred. Every batch that deletes a spend
fact (transparent-retention maintenance, and the in-place address-projection migration) writes
the highest deleted height into a `transparent_retention_deleted_through_height`
marker in the same batch. A checkpoint bootstrap that only advances the swept
cursor, and a migration that deletes nothing, leave the marker unset.

The marker still distinguishes an ambiguous canonical point miss from an
outpoint that was never observed, which keeps union-routed serving fail-closed
while materialized views lag. It also defines the startup recovery obligation: before any
schema reconciliation can clear rows, ingest requires preserved contiguous
projection coverage from the canonical history boundary through the deleted
height, or retained chain events that prove a destructive rebuild can replay
from that same boundary. Store schema 13 remains a hard boundary because older
stores recorded only outpoints in that index and may already have deleted the
facts needed to fill it. Such stores are refused at primary open and require a
genesis rebuild; there is no unsafe best-effort migration.

### Serving

`WalletQuery.TransparentSpendsByOutpoint` deepens without a wire change, a new
capability, or a new RPC: the response already carries spend entries, and
`wallet.read.transparent_spends_by_outpoint_v1` strengthens in place from a
reorg-window-scoped answer to a durable one. The strengthening ships in the
same release that introduces `contract_revision` 1, so revision 1 already
denotes the durable semantics and no in-place-revision bump marks it. Canonical
epoch-pinned read first; for canonical misses the union read consults the
projection and surfaces settled hits whose stored block hash still matches the
retained canonical header at that height, so a stale row from a reorged-out
branch (a reorg the tailer has not yet replayed) never surfaces as the spender.
If the materialized-view head trails the deleted-through marker the read refuses with the
existing materialized-view lag vocabulary rather than answering incompletely; a store that
never deleted a fact keeps the canonical-only absent semantics even with an empty
projection.

## Consequences

- Spender identity for a settled transparent outpoint is durable and survives
  the canonical sweep, so wallet offline-recovery resolves arbitrarily old
  spends.
- Canonical retention no longer advances on the settled tip alone; it waits for the
  durable projection, so a projection that lags (or is paused under memory
  pressure) holds canonical spend-fact storage until it catches up.
- A materialized view can be rebuilt after point-row retention only when its
  retained chain-event source still covers the full canonical history boundary;
  otherwise startup fails before destructive reconciliation.
- Deploying artifact schema 18/store schema 13 onto any older canonical volume
  fails closed and requires one genesis rebuild to establish that durable source.
- The projection's schema can rebuild from retained transaction inputs, but a
  new version must still preserve durable-commit-before-retention-release
  ordering before it replaces the active projection.
