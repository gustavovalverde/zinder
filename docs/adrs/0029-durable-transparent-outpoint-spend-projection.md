# ADR-0029: Durable transparent-outpoint spend projection and retention release floor

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Canonical retention, derive plane, wallet spend resolution |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md), [ADR-0026](0026-utxo-set-commitment.md), [ADR-0028](0028-per-consumer-derive-schema-versioning.md), [Wallet data plane §Transparent Reverse-Spend Resolution](../architecture/wallet-data-plane.md#transparent-reverse-spend-resolution) |

## Context

The canonical safe-tip retention sweep deletes a transparent spend fact and its
spent-output rows once the spend settles below the safe tip. `WalletQuery.TransparentSpendsByOutpoint`
then cannot resolve the spender of anything spent longer ago than the reorg
window, which breaks wallet offline-recovery: a wallet backend maps that RPC to
its spender-resolution path and expects an answer for arbitrarily old spends.

Spender identity must therefore outlive the canonical fact. The canonical store
cannot retain every spend fact forever without unbounded growth, and it must
stay ignorant of the derive plane ([ADR-0003](0003-canonical-storage-access-boundary.md)).

## Decision

A bundled derive consumer records durable spender identity, and the canonical
retention sweep is clamped so it never deletes a fact the consumer has not yet
recorded.

### Authority split

Spent-versus-unspent is decided only by `TransparentUnspentOutputsByOutpoint`,
which is durable and LtHash16-committed ([ADR-0026](0026-utxo-set-commitment.md)).
This projection makes only *spender identity* durable; it never answers
spentness. A missing projection row means "no spender recorded here", never
"unspent". The union-routed read enforces this: it consults the projection only
for outpoints the canonical epoch-pinned read missed, and only surfaces a
projection hit whose spend settled at or below the pinned epoch's settled tip.

### The `transparent_outpoint_spend` consumer

A `BlockKeyedConsumer` ([ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md))
keys primary rows on the spent outpoint (creating transaction id plus big-endian
output index) valued with the spending transaction id, spending block hash,
spending height, and transparent input index. A per-height index column family,
written for every applied block even when empty, drives reorg rewind and reports
the projection's durable height through `last_materialized_height_ascending`.
Coinbase inputs carry no prevout and never produce a row. A block whose spend
facts are unavailable at replay time is a hard error, never a skip: the durable
height gates irreversible canonical deletion, so it must not advance past a
block whose spenders the consumer could not observe.

The projection sources its facts from the same canonical spend facts the sweep
deletes, so a schema-version bump that rebuilds it replays from retained
canonical events; a rebuild floor above already-swept heights forces a full
canonical re-ingest. This is the one exception to the per-consumer rebuild
economics of [ADR-0028](0028-per-consumer-derive-schema-versioning.md): every
other consumer's schema bump costs a scoped wipe plus a replay from retained
events, while this consumer's bump can escalate to a from-genesis canonical
re-ingest because the sweep destroyed its source rows. The row format is
deliberately conservative to keep the version stable.

### Retention release floor

The canonical store persists a `transparent_retention_release_height` marker
alongside the existing swept-through marker. The safe-tip sweep clamps its
ceiling to `min(new safe tip, previous tip, release height)`, so a settled spend
above the release height stays retained. `zinder-ingest` publishes the release
height from the derive tailer as the durable projection advances, so canonical
retention releases only what the projection has already recorded. A release
height below the swept marker is ignored safely; the sweep never regresses.

The release floor is a durability barrier, not just a progress signal. The
derive store writes unsynced, so `zinder-ingest` fsyncs the derive
write-ahead log before publishing a higher floor. Without that a host crash
could lose projection rows the floor already authorized the canonical sweep to
delete, stranding spender identities the guard cannot recover. Publication is
throttled: each floor advance costs one derive fsync plus one synced canonical
write, and a floor that lags by the throttle interval only defers a sweep.

### Deleted-through marker and startup guard

Deletion provenance is recorded, not inferred. Every batch that deletes a spend
fact (the safe-tip sweep, and the in-place address-projection migration) writes
the highest deleted height into a `transparent_retention_deleted_through_height`
marker in the same batch. A checkpoint bootstrap that only advances the swept
cursor, and a migration that deletes nothing, leave the marker unset.

`zinder-ingest` refuses to start when the projection's durable height falls below
that marker: those spender identities can never be re-derived, and the only
remedy is a full canonical re-ingest, so the process crashes with a typed error
naming exactly that. A fresh store and a checkpoint-bootstrap store both pass
because the marker stays unset until a real deletion runs. A store that migrated
its address projection in place and deleted finalized-spent facts seeds the
marker at its safe tip, so it fails the guard rather than serving absent spenders
for genuinely spent outpoints.

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
If the derive head trails the deleted-through marker the read refuses with the
existing derive-lag vocabulary rather than answering incompletely; a store that
never deleted a fact keeps the canonical-only absent semantics even with an empty
projection.

## Consequences

- Spender identity for a settled transparent outpoint is durable and survives
  the canonical sweep, so wallet offline-recovery resolves arbitrarily old
  spends.
- Canonical retention no longer advances on the safe tip alone; it waits for the
  durable projection, so a projection that lags (or is paused under memory
  pressure) holds canonical spend-fact storage until it catches up.
- Deploying onto a store that already swept past an absent projection crashes on
  purpose and requires a canonical re-ingest.
- The projection's schema version is expensive to move; a bump rebuilds from
  retained events and can escalate to a canonical re-ingest.
