# ADR-0035: Fact-first storage selection and lifecycle targets

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Canonical storage, projection construction, production database selection, snapshot recovery, lifecycle benchmarks |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0015](0015-unified-phase-driven-ingest.md), [ADR-0022](0022-resource-budgeted-bulk-catchup.md), [ADR-0034](0034-exclusive-index-build-stages-and-block-local-spend-replay.md), [Canonical-first indexer](../architecture/fact-first-indexer.md) |

## Context

The canonical writer currently publishes immutable chain facts and several
cross-block wallet read models in the same foreground commit. Transparent
output lookup, address-output indexing, spend resolution, reorg repair, and
retention therefore make canonical throughput depend on the full historical
wallet workload. A wallet-only mainnet canary held projection replay at height
zero and kept historical work closed, but canonical progress still remained
dominated by RocksDB commit and historical prevout I/O. Source transport, CPU,
memory pressure, swap, pending compaction, and write stalls were not the active
limits.

The accepted fact-first architecture removes those read models from canonical
ingest. That change invalidates a previous comparison that priced Postgres as a
row-for-row port of the present canonical schema. A compact, block-local fact
model has a different write shape and needs a new comparison against an
equivalent fact-first RocksDB control.

Provisioning from a verified snapshot and reconstructing the chain from source
are also different operator products. A single replay loop cannot optimize both
without accumulating branches and backend-specific exceptions. Initial
projection construction has a third shape: it can use set operations and bulk
loading, while live projection following must preserve ordered atomic state
transitions.

## Decision

Zinder implements three durable data planes:

1. Canonical ingest publishes immutable, block-local facts and chain visibility
   events. Its commit path performs no cross-block reads and no projection
   reads.
2. One wallet projection owns live transparent outputs, spent outputs, address
   history, balances, bounded reorg undo, its commitment, and one authenticated
   projection position.
3. Explorer projections own independently rebuildable public analytics. They
   consume wallet-owned facts through the wallet contract when the wallet plane
   already owns the state.

The canonical physical model centers on one `CanonicalBlockFacts` value per
block with ordered transaction facts. The Rust aggregate is backend-neutral and
does not carry a physical schema version. Its deterministic reference encoding
is independently versioned by `CanonicalBlockFactsDigestVersion`; version 1
commits the header, optional raw block bytes, and ordered
`CanonicalTransactionFacts` values with explicit tags, lengths, presence
markers, vector boundaries, and fixed little-endian integers. Separate
canonical indexes exist
only for independently queried contracts such as chain position, transaction
location, compact blocks, tree state, subtree roots, chain epochs, chain
events, and mempool events. A structure-of-arrays encoding is an internal
layout option only when a benchmark proves decoding or cache locality gains. It
is not another public fact model.

The following lifecycles remain separate deep modules:

- Snapshot restore verifies a physical backend snapshot and a Zinder-owned
  manifest, then follows the short event tail.
- Fresh canonical construction parses source blocks in parallel, bulk loads an
  inactive database, validates continuity and digests, builds secondary
  indexes, and publishes a baseline epoch.
- Projection construction builds an inactive read model using
  projection-owned bulk operations, validates it, catches up from its pinned
  canonical epoch, and promotes it.
- Live following commits each visible canonical epoch or projection transition
  atomically under a durable writer generation.

The first production database choice is a bake-off, not a permanent adapter
matrix. The candidates are:

- a fact-first RocksDB control that can build sorted external SST files; and
- a block-granular Postgres candidate that can use binary `COPY` and deferred
  secondary-index construction while the database is not serving reads.

Both candidates consume the same `CanonicalBlockFacts`, captured source corpus,
durability contract, reference hardware, and validation digest. The selected
runtime backend must meet every hard lifecycle gate, every universal
correctness gate, and every gate for its advertised topology. The losing
canonical runtime, configuration, metrics, documentation, examples, and tests
are deleted before general availability.

Postgres is the preferred production outcome if it passes. That topology uses
one database per Zcash network with `canonical`, `wallet`, and `explorer`
schemas and role-scoped credentials. It gives operators one high-availability,
backup, replica, and observability model. If Postgres misses the canonical hard
gate, Zinder keeps RocksDB only for canonical truth and uses Postgres for wallet
and explorer projections. That hybrid is a measured performance compromise,
not the default assumption. It is eligible only after the Postgres wallet and
explorer planes independently pass their applicable construction, readiness,
correctness, recovery, and query gates. If either projection plane misses its
gates, the storage decision reopens rather than silently retaining an
unmeasured topology.

The bake-off does not introduce `DatabaseAdapter`, a generic row transaction,
or a lowest-common-denominator key-value interface. Canonical, wallet, and
explorer modules expose focused domain operations. Backend crates own concrete
SQL or RocksDB mechanics. Shared backend support is extracted only after two
retained implementations prove stable duplication.

## Lifecycle targets

The reference hardware, source corpus digest, chain height, database settings,
and durability settings are recorded with every result. At the approximately
3.413 million block tip used to establish this decision, the targets are:

| Lifecycle | Target | Hard gate |
| --------- | ------ | --------- |
| Fresh canonical construction | 2 hours | 3 hours |
| Wallet projection construction after canonical | 1 hour | 2 hours |
| Fresh wallet-ready lifecycle | 3 hours | 4 hours |
| Verified snapshot restore and tail | 15 minutes | 15 minutes |
| Healthy-tip canonical lag | At most 2 blocks | At most 2 blocks |
| Healthy wallet projection lag | At most 2 canonical epochs | At most 2 canonical epochs |

Chain growth does not weaken these targets silently. Benchmark reports record
the exact source height and calculate the achieved average block rate. New
production releases re-run the full lifecycle against the then-current tip.

The restore gate uses a certified, immutable snapshot fixture for each
candidate. Its Zinder-owned manifest records the network, backend and schema
revisions, snapshot epoch, height and hash, archive byte length and SHA-256,
canonical fact-sequence digest, and the pinned source-corpus tip exactly 10,000
blocks later. The archive and source corpus are pre-staged on the measured
storage class; WAN transfer is excluded from this storage lifecycle and is
reported separately by deployment tests.

Restore timing starts before archive checksum verification and extraction or
database restore. It includes store open, network and schema admission, replay
of the 10,000-block tail, continuity validation, comparison of the manifest's
certified fact digest with the snapshot's atomically stored digest marker, a
fresh digest over the replayed tail, and the canonical query-readiness fence at
the pinned epoch. Full pre-snapshot fact recomputation belongs to snapshot
certification and is not hidden inside the 15-minute restore timer. The timer
stops only after the restore checks pass. A tip-adjacent snapshot, a shorter
tail, an already-extracted database, or a readiness probe without digest
validation cannot satisfy the gate. Wallet and explorer restore readiness
receive separate measured stages when those snapshot contracts exist.

Every threshold-bearing candidate report includes the commit, applicable
schema revisions, fixture digest, hardware and storage class, effective
database and durability settings, and the wall time for the boundary it
directly drives. The complete production-selection evidence set additionally
includes dense-range throughput, rows, logical bytes, WAL or SST bytes, peak
resident memory, disk high-water mark, index construction time, validation
digests, snapshot restore time, and representative query latency where those
drivers apply. A narrow driver does not emit placeholder fields for lifecycles
or measurements it did not execute.

## Correctness gates

Performance cannot select a backend unless all of these pass:

- Full-chain continuity and canonical digest equality against a serial
  reference.
- Instrumented proof that canonical construction performs no cross-block
  lookup.
- Wallet UTXO count, total value, commitment, spent map, balances, and address
  history equality against a serial reference.
- Same-height replacement and maximum-depth reorg behavior.
- Crash injection before and after each publication boundary.
- Snapshot restore followed by ordered tailing.
- Native API parity, the complete lightwalletd and Zallet compatibility suite,
  and the covered Zally, Zexplorer, and Cipherscan route matrices.

Postgres must additionally prove durable writer-generation fencing, automated
failover promotion, stale-writer rejection, standby lag reporting, and
request-scoped replica read fences. A RocksDB canonical fallback must instead
prove exclusive primary ownership, crash-safe primary restart, and same-volume
secondary catch-up. That fallback does not advertise cross-host writer failover
or database-replica reads; adding those capabilities requires a new topology
decision and its corresponding gates.

Unavailable or behind state remains explicit. A query returns a typed
`ProjectionBehind` or `ReplicaBehind` failure rather than an empty list, zero
balance, missing row, or unqualified unavailable error.

## Implementation order

1. Give `zinder-bench` one versioned report contract. Its current replay driver
   reports only `acceptance.canonical_fixture_replay`, timed over a captured
   range and supplied current-schema clone. It does not claim fresh canonical
   construction, snapshot restore, projection construction, following, or
   wallet readiness. Each production lifecycle gains a report field only when
   a dedicated driver owns its complete build, validate, catch-up, and publish
   boundary.
2. Introduce the pure `CanonicalBlockFacts` value, its dedicated reference
   encoding version, per-block digest, and ordered sequence digest while the
   current store remains a temporary oracle.
3. Implement the smallest fact-first RocksDB and Postgres vertical slices and
   run both from the same Docker Compose benchmark topology.
4. Select one canonical runtime and delete the losing candidate.
5. Cut a fresh canonical schema that removes global transparent output,
   address, spend, repair, and retention state from canonical commits.
6. Implement the concrete wallet projection builder, ordered follower, and
   readiness verifier; add wallet construction and wallet-ready lifecycle
   acceptance only with that real plane.
7. Rewire query, client, compatibility, and downstream contracts in one
   coordinated breaking change.
8. Move remaining explorer consumers and backfills to explorer-owned modules.
9. Replace configuration, readiness, metrics, snapshot operations, testkit
   fixtures, deployment manifests, and runbooks.
10. Build a blue-green production stack, validate it without traffic, catch up,
    switch traffic, retain the previous stack for a bounded rollback window,
    then delete the old storage paths and compatibility baggage.

There is no dual-write migration. A new database is built from captured source
facts or a verified snapshot. The previous binary and volume provide rollback
during the acceptance window. A RocksDB-to-Postgres row converter or in-place
downgrade path requires separate evidence that reconstruction cannot meet the
hard lifecycle gate.

## Module and vocabulary consequences

The target ownership names are `canonical fact`, `chain epoch commit`, `wallet
projection`, `explorer projection`, `projection position`, `projection fence`,
`projection builder`, `projection follower`, `storage checkpoint`, and
`snapshot manifest`. The word `derive` is reserved for protocol or
cryptographic derivation.

`zinder-store` becomes the canonical domain and its selected backend
implementation. `zinder-derive` is deleted after wallet and explorer ownership
lands. `zinder-ingest` remains source-to-canonical only. An independent
`zinder-projector` process owns build, verify, catch-up, follow, and promote
lifecycles for one selected projection. The default deployment may run these
processes in one application group, but they retain separate credentials,
readiness, restart, and resource ownership.

Public clients use the remote Zinder contract. `LocalChainIndex` and SDK
storage-engine dependencies are deleted rather than carried as a second client
path. lightwalletd and Cipherscan remain stateless protocol edges over
WalletQuery and ExplorerQuery.

## Consequences

- Canonical throughput no longer scales with historical wallet lookup and
  indexing work.
- Snapshot restore is the normal minutes-scale provisioning path; fresh source
  reconstruction is the hours-scale disaster and verification path.
- Initial wallet construction can use set operations instead of replaying the
  whole chain through the live row-by-row state machine.
- A successful Postgres candidate removes the host-local secondary-reader and
  multi-backend operational model.
- A failed Postgres candidate still yields a clean plane split and an explicit
  reason for retaining RocksDB canonical storage.
- Temporary candidate code has a deletion gate. It does not become a permanent
  compatibility promise.
- This decision supersedes RocksDB-specific ownership and scheduling details in
  earlier ADRs when those details conflict with the selected fact-first
  implementation. Historical ADRs remain records of the decisions that
  produced the current schema.

## Rejected alternatives

- Adding Postgres behind the current `DeriveStore` preserves a RocksDB-shaped
  public batch API and mixes wallet and explorer ownership.
- Keeping both canonical backends after the comparison doubles configuration,
  backup, failure, replica, test, and documentation semantics without a product
  requirement.
- Normalizing every canonical fact into relational rows during initial
  construction recreates index and WAL amplification before a query proves the
  rows need independent lookup.
- Running the live wallet state machine from genesis wastes the set-based
  leverage available while an inactive generation is not serving traffic.
- Dual writing old and new stores makes rollback appear simpler while adding a
  second foreground consistency boundary to the hottest path.
- A generic offline projection compiler introduces a framework before more than
  one projection proves the same build algorithm.
