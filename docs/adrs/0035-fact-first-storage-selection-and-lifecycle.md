# ADR-0035: Fact-first storage topologies and lifecycle targets

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Canonical storage, projection construction, deployment topology support, snapshot recovery, lifecycle benchmarks |
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
does not carry a physical schema version. Its deterministic correctness oracle
is independently versioned by `CanonicalBlockFactsDigestVersion`; version 1
commits the header and ordered `CanonicalTransactionFacts` values with explicit
tags, lengths, presence markers, vector boundaries, and fixed little-endian
integers. Raw block and transaction blobs are retention-policy artifacts, so
their bytes remain outside the aggregate, digest, and replay envelope. Typed
SHA-256 commitments to the exact serialized block and transaction bytes remain
inside the aggregate, allowing store admission to bind an optional retained
blob without reparsing consensus data. The reversible version-1 storage
envelope is separately identified by `CanonicalBlockReplayFormatVersion`. A
stored envelope is valid only when it decodes into the complete aggregate, has
canonical bytes, and reproduces its independent reference digest. Separate
canonical indexes exist only for independently queried contracts such as chain
position, optional transaction location, compact blocks, tree state, subtree
roots, chain epochs, chain events, and mempool events. A structure-of-arrays
encoding is an internal layout option only when a benchmark proves decoding or
cache locality gains. It is not another public fact model.

Every persisted contract introduced by this architecture starts at version 1:
canonical RocksDB and PostgreSQL, wallet projection, explorer projection,
canonical fact digest, replay envelope, backup manifest, fixture, benchmark
report, and physical candidate schema. This is a clean identity reset, not a
numeric rename. Persisted domain identities are `canonical`, `wallet`,
`explorer`, and `state-bundle`; each record carries its exact format version
and network before any data column family or table is created. A non-empty path
without the expected identity, or with any version other than 1, is refused
without mutation. Previous stores, backups, fixtures, and reports are
intentionally unsupported and must be rebuilt or recaptured.

The current ingest path parses a source block's serialized header prefix first
so bulk catchup can validate identity and parent links without deserializing
transactions. Parallel canonical preparation then performs exactly one full
block parse, validates the coinbase height and source identity, constructs the
semantic facts and retained raw blobs, and encodes the replay envelope.
The ordered positioning lane's only stateful computation folds
commitment-tree sizes; it stamps the result, serializes the prepared compact
block, and moves the prepared replay bytes toward atomic commit.

Epoch-bound replay reads use `BlockReplayBatchRequest`. The request starts
at one height, carries a nonzero block limit, and is rejected above 256
blocks. A batch uses one ordered visibility-index scan followed by one
`multi_get`; starts beyond the pinned visible tip return no rows, crossing
batches stop at the tip, and any missing or corrupt row fails the batch.

The following lifecycles remain separate deep modules:

- Snapshot restore verifies a physical backend snapshot and a Zinder-owned
  manifest, then follows the short event tail.
- Fresh canonical construction parses source blocks in parallel, bulk loads an
  inactive database, validates continuity and digests, builds secondary
  indexes, and publishes a baseline epoch.
- Projection construction builds an inactive read model using
  projection-owned bulk operations, validates it, catches up from its pinned
  canonical epoch, and promotes it. Promotion requires a future durable,
  expiring `ProjectionBuildLease` anchored to the pinned epoch and chain event;
  event pruning must retain that anchor while the lease remains valid. The
  current implementation has no persisted lease, renewal path, pruning floor,
  or lease-guarded promotion.
- Live following commits each visible canonical epoch or projection transition
  atomically under a durable writer generation.

Zinder retains two deployment topology contracts. The names describe
operational shape, not quality or environment:

- `rocksdb-single-host` keeps canonical, wallet, and explorer storage in RocksDB
  on one host. Services may remain separate processes or containers, but they
  share host-local storage and do not require Postgres.
- `postgres-scale-out` keeps the three planes in Postgres with role-scoped
  credentials and an independently deployable canonical writer, projectors,
  query replicas, and database replicas. It may run on one host for testing,
  but its contract permits those roles to scale across hosts.

Only `rocksdb-single-host` has a current service composition.
`postgres-scale-out` is represented by a block-granular diagnostic driver and
does not yet implement production schema ownership, TLS, writer fencing,
replica reads, failover, or readiness. It becomes a supported deployment only
after its complete lifecycle and topology-specific gates pass.
`rocksdb-single-host` does not mean development-only, and the term `embedded`
remains reserved for an indexer embedded in a consumer process, such as the
Zaino integration described by the indexer-wallet boundary.

The first storage implementation is a side-by-side validation, not a
winner-takes-all backend bake-off. The candidates are:

- a fact-first RocksDB control that currently builds one sorted external SST file; and
- a block-granular Postgres candidate that can use binary `COPY` and deferred
  secondary-index construction while the database is not serving reads.

Both candidates consume the same `CanonicalBlockFacts`, captured source corpus,
durability contract, reference hardware, and validation digest. Each topology
must meet every hard lifecycle gate, every universal correctness gate, and its
topology-specific gates before Zinder advertises it as ready. A failed result
blocks that topology from release until it is corrected; it does not demote the
other topology or create a partial hybrid.

The initial single-SST and single-load-transaction implementations are a
correctness baseline, not a claim of maximum engine throughput. A later
fastest-sync sweep may compare segmented or parallel SST generation,
PostgreSQL COPY pipelining, and resource partitions as named benchmark arms
without changing the topology contracts or acceptance oracle.

Once both composition roots exist, a deployment selects
`rocksdb-single-host` or `postgres-scale-out` as one application-level
contract. Per-plane mixing, such as RocksDB canonical storage with Postgres
projections, will not become a third supported topology. This keeps
configuration, backup, recovery, readiness, and failure semantics bounded to 2
deliberate shapes.

Supporting both topologies does not introduce `DatabaseAdapter`, a generic row
transaction, or a lowest-common-denominator key-value interface. Canonical,
wallet, and explorer modules expose focused domain operations. Engine-specific
modules own concrete SQL or RocksDB mechanics. Shared implementation support is
extracted only after the two retained implementations prove stable duplication.

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
The current replay drivers do not execute this full lifecycle and therefore do
not certify any target in this table.

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
directly drives. The complete topology-certification evidence set additionally
includes dense-range throughput, rows, logical bytes, WAL or SST bytes, peak
resident memory, disk high-water mark, index construction time, validation
digests, snapshot restore time, and representative query latency where those
drivers apply. A narrow driver does not emit placeholder fields for lifecycles
or measurements it did not execute.

## Correctness gates

A topology cannot be certified unless all of these pass:

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

The current diagnostic drivers satisfy only their scoped replay round trip.
They do not provide crash-injection, snapshot-tail, projection construction,
API-parity, or topology-certification evidence.

The `postgres-scale-out` topology must additionally prove durable
writer-generation fencing, automated failover promotion, stale-writer
rejection, standby lag reporting, and request-scoped replica read fences. The
`rocksdb-single-host` topology must instead prove exclusive primary ownership,
crash-safe primary restart, same-volume secondary catch-up, and a coherent
checkpoint bundle across its RocksDB stores. It does not advertise cross-host
writer failover or database-replica reads.

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
   boundary. The fixture, report, fact digest, replay envelope, and diagnostic
   physical schemas all begin at version 1; abandoned pre-release numbers are
   not compatibility contracts.
2. Introduce the pure `CanonicalBlockFacts` value, its version-1 reference
   digest contract, independently versioned version-1 replay format, and
   ordered sequence digest while the current store remains a temporary oracle.
3. Implement the smallest fact-first RocksDB and Postgres vertical slices and
   run both from the same Docker Compose benchmark topology. Every row must
   decode back into complete semantic facts and reproduce the fixture digest.
   The diagnostic round-trip slices now exist; they do not yet implement the
   complete canonical lifecycle or certify either topology.
4. Persist one production RocksDB replay envelope per block in the atomic
   canonical commit, prove append/reorg/reopen/secondary/corruption behavior,
   and add a pure block-local `BlockSummary` projector over decoded replay. The
   replay persistence at store schema 14 and artifact schema 19 and the pure
   equivalence seam exist, but production `BlockSummaryConsumer` dispatch still
   uses `BlockCommitContext`. The replay projector preserves the existing
   schema-1 meaning of `total_size_bytes`: the complete serialized block size
   recorded in `BlockHeaderArtifact`. Neither projector equivalence nor replay
   persistence provides fact-first throughput evidence.
5. Freeze the consumer-data matrix, then cut a fresh canonical schema with a
   distinct `canonical` identity and exact version 1. Remove global transparent
   output, address, spend, repair, and retention state from canonical commits.
   Keep compact scan payloads, tree metadata, roots, and displaced-branch facts
   canonical until version-1 replay can reproduce them without source access.
6. Implement RocksDB fresh canonical construction and live following over that
   schema, then prove that canonical commits perform no cross-block wallet
   lookup across the representative mainnet workload anchors. The store-side
   atomic append boundary now exists with authenticated consecutive reopen;
   shallow reorg, continuous source following, and the ingest cutover remain
   part of this step.
7. Add the durable projection-build lease and anchor-aware event-pruning floor
   before any inactive builder can be promoted.
8. Implement the concrete RocksDB wallet projection builder, ordered follower,
   and readiness verifier; add wallet construction and wallet-ready lifecycle
   acceptance only with that real plane.
9. Rewire query, client, compatibility, and downstream contracts in one
   coordinated breaking change.
10. Move remaining explorer consumers and backfills to explorer-owned modules,
    then delete the legacy `zinder-derive` path.
11. Replace configuration, readiness, metrics, snapshot operations, testkit
    fixtures, deployment manifests, and runbooks for the target
    `rocksdb-single-host` composition.
12. Prove the complete `rocksdb-single-host` lifecycle, including canonical
    construction, wallet and explorer construction, live following, reorg,
    restart, checkpoint, restore, and client parity.
13. Implement PostgreSQL canonical construction, live following, writer
    fencing, epoch-pinned read sessions, and the transactional event outbox.
14. Implement the PostgreSQL wallet and explorer projections.
15. Add the `postgres-scale-out` configuration, readiness, metrics, snapshot,
    testkit, deployment, and operational contracts, including TLS, role-scoped
    credentials, replica lag, and failover.
16. Prove the complete `postgres-scale-out` lifecycle, including stale-writer
    rejection, failover, replica reads, restore, and client parity.
17. Build a blue-green production stack, validate it without traffic, catch up,
    switch traffic, retain the previous stack for a bounded rollback window,
    then delete the old storage paths and remaining compatibility baggage.

Complete topology validation cannot precede the schema and projection
lifecycles it is meant to validate. The RocksDB composition closes first so it
provides a reusable lifecycle suite and measured reference evidence for the
PostgreSQL composition. Both implementations remain accountable to the
backend-neutral digest contract and serial reference. This is an execution
order, not a claim that PostgreSQL is optional or that RocksDB owns correctness.

The current PostgreSQL diagnostic uses the production-intended
`tokio-postgres` driver but connects without TLS only inside the isolated
Compose network. The `postgres-scale-out` production gate additionally requires
certificate-validated TLS, role-scoped credentials, writer fencing, replica
lag/read fences, failover validation, and transactional reorg proof.

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

`rocksdb-single-host` and `postgres-scale-out` are the stable serialized names
at deployment-manifest and evidence-report boundaries. Zinder does not add a
runtime topology selector before both composition roots exist. Once they do,
one closed discriminant selects the complete topology, rejects configuration
belonging to the other topology, and provides no aliases or per-plane backend
switches.

`zinder-store` becomes the canonical domain. Concrete RocksDB and Postgres
implementations remain engine-named modules or crates selected by topology
composition rather than hidden behind one generic database API. `zinder-derive`
is deleted after wallet and explorer ownership lands. `zinder-ingest` remains
source-to-canonical only. An independent `zinder-projector` process owns build,
verify, catch-up, follow, and promote lifecycles for one selected projection.
The `rocksdb-single-host` topology may colocate these processes in one
application group, but they retain separate readiness, restart, and resource
ownership.

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
- Operators can run `rocksdb-single-host` without Postgres. A future certified
  `postgres-scale-out` composition will be the option for independent workers,
  replicas, and database operations; the present diagnostic driver is not that
  deployment.
- Both topologies preserve the same data-plane, readiness, and public API
  contracts; only their physical persistence and topology-specific operations
  differ.
- Maintaining two concrete persistence implementations is an intentional
  product cost. Temporary benchmark scaffolding and any hybrid backend path
  still have deletion gates.
- This decision supersedes RocksDB-specific ownership and scheduling details in
  earlier ADRs when those details conflict with the fact-first topology
  contracts. Historical ADRs remain records of the decisions that produced the
  current schema.

## Rejected alternatives

- Adding Postgres behind the current `DeriveStore` preserves a RocksDB-shaped
  public batch API and mixes wallet and explorer ownership.
- Calling the RocksDB topology `embedded` collides with the existing
  consumer-embedded indexer vocabulary and hides that Zinder still runs as a
  service group.
- Calling the Postgres topology `production` falsely implies that
  `rocksdb-single-host` deployments are unsuitable for production.
- Allowing arbitrary per-plane backend combinations multiplies configuration,
  backup, failure, test, and documentation semantics without adding a third
  product topology.
- Normalizing every canonical fact into relational rows during initial
  construction recreates index and WAL amplification before a query proves the
  rows need independent lookup.
- Running the live wallet state machine from genesis wastes the set-based
  leverage available while an inactive generation is not serving traffic.
- Dual writing old and new stores makes rollback appear simpler while adding a
  second foreground consistency boundary to the hottest path.
- A generic offline projection compiler introduces a framework before more than
  one projection proves the same build algorithm.
