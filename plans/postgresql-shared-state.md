# PostgreSQL Horizontal Production Topology

Source PRD: [#56](https://github.com/gustavovalverde/zinder/issues/56)

## Objective

Make `postgres-horizontal` a second production topology that operators may
choose instead of `rocksdb-single-host`. RocksDB remains supported, remains the
default until an explicit release decision changes that, and retains its own
storage, recovery, and serving mechanics.

This plan deliberately builds PostgreSQL as complete vertical slices. A
topology is not production-supported merely because a database adapter, schema,
or configuration value exists.

## Architectural constraints

- `DeploymentTopology::RocksDbSingleHost` and
  `DeploymentTopology::PostgresHorizontal` name coherent deployment shapes.
  Canonical and wallet engines cannot be mixed within one deployment.
- One PostgreSQL database represents one Zcash network. Domain schemas have
  exact identity, schema version, and network admission.
- PostgreSQL enables active/passive ordered writers and active/active readers.
  It does not make chain or wallet projection writers active/active.
- One migration composition root owns DDL. Application runtimes never migrate
  on startup and use separately provisioned least-privilege roles.
- Domain-shaped operations own SQL. Zinder does not expose a generic database
  adapter or force RocksDB and PostgreSQL through identical physical APIs.
- Durable event rows are authoritative. Notifications may wake consumers but
  never replace cursor-based recovery.
- Database-assigned sequence values do not define resumable event order.
  Cursor-visible positions commit through locked domain control records.
- The public client remains remote-first and has no PostgreSQL or RocksDB
  dependency.
- No dual writes, in-place RocksDB conversion, compatibility reader, or
  mixed-engine rollback path is planned. Topology migration is a blue-green
  source rebuild.
- Optional explorer and Cipherscan persistence is ported only for a named
  PostgreSQL production consumer.

## Vertical-slice proof policy

Every completed slice must pass its unit and disposable PostgreSQL integration
tests and then execute through a production composition root against Regtest.
The proof must cross a process boundary whenever the slice claims persistence,
takeover, portable cursors, or replica interchangeability.

Use Testnet when Regtest cannot represent the relevant evidence with comparable
confidence. This includes long historical construction, network-upgrade
activation boundaries, production-shaped chain density, long-lived reorg or
recovery behavior, and representative performance or storage growth. Record
why Regtest was sufficient or why Testnet was required for each slice.

No slice may claim behavior from a unit-only port, benchmark-only schema, mock
database, or test-only composition.

## Slice 1: One-transition canonical tracer

### Outcome

A one-shot migration creates the first production-shaped canonical schema. A
feature-gated ingest process reads one real block transition from Zebra,
commits its fact, epoch, event, and control state atomically, exits, and a fresh
process verifies the persisted database identity and tip.

### Scope

- Add the stable topology catalog while keeping `rocksdb-single-host` as the
  supported default.
- Add shared strict PostgreSQL configuration with secret-file URI loading,
  certificate and hostname verification, an explicit loopback-only plaintext
  posture, and redacted rendering.
- Add `zinder-migrate` as the only schema authority.
- Add `zinder-postgres` connection, migration, identity admission, and the
  smallest domain-shaped canonical append.
- Require the pre-provisioned `zinder_ingest` role and grant only the DML
  privileges required by the tracer.
- Serialize competing append transactions through the canonical writer fence.
- Reject wrong network, schema, topology, role, predecessor, and mixed RocksDB
  configuration before mutation.
- Keep the tracer behind a non-default feature and out of release artifacts.

### Exit criteria

- Migration is idempotent and binds the database to `postgres-horizontal` and
  one network.
- The schema owner cannot open the writer store.
- Removing any required writer privilege causes admission or mutation to fail.
- A late SQL failure leaves facts, epoch, event, control, and writer term
  unchanged.
- Replaying the same append is idempotent; a stale predecessor is rejected.
- Two connections racing the same append yield one commit and one idempotent
  outcome.
- Verified TLS and redaction are proven.
- Regtest proves migration, one live Zebra append, writer exit, and fresh-probe
  readback. Testnet is not required because this slice has no historical,
  upgrade-era, or lifecycle claim.

## Slice 2: Canonical lifecycle and writer takeover

### Outcome

The PostgreSQL ingest runtime constructs from an empty database, follows the
source continuously, replaces the visible suffix within the configured reorg
window, parks beyond that window, restarts, and transfers mutation authority
to a standby without partial or stale writes.

### Scope

- Extract persistence-neutral canonical transition planning only where both
  concrete topology implementations consume it.
- Implement bounded, resumable construction and final publication.
- Add append and replacement transitions with retained epoch-valid versions.
- Add database-time writer leases and monotonically increasing writer terms.
  Every mutating transaction validates its term while holding a compatible
  lock on the authority record.
- Add deterministic transition identities and unknown-commit resolution.
- Add event-retention floors, recovery evidence, readiness, and metrics.
- Preserve RocksDB construction, append, reorg, parking, and restart behavior.

### Exit criteria

- Shared semantic fixtures agree across RocksDB and PostgreSQL.
- Regtest proves restart, shallow reorg, terminal reorg, standby takeover,
  stale-writer rejection, and durable cursor recovery.
- Testnet proves long-history construction and activation-era decoding before
  this lifecycle is considered complete.

## Slice 3: Wallet projection

### Outcome

The PostgreSQL projector constructs wallet state, follows canonical events,
reconciles replacements, publishes exact serving evidence, restarts, and
transfers writer authority without exposing mixed source and projection state.

### Scope

- Move wallet transition planning to asynchronous, persistence-neutral change
  sets where the two topologies prove the same domain boundary.
- Add wallet identity, writer term, construction state, rows, undo evidence,
  source cursor, digest, and publication to a dedicated schema.
- Commit wallet changes, source position, readiness evidence, and publication
  atomically.
- Keep RocksDB immutable serving-pair publication intact.

### Exit criteria

- Both topologies pass the same wallet construction, follow, undo, and digest
  fixtures.
- Regtest proves process restart, shallow reorg reconciliation, publication
  invalidation, and standby takeover.
- Testnet proves a representative historical wallet build before production
  certification.

## Slice 4: Portable native and compatibility reads

### Outcome

Interchangeable query and lightwalletd replicas serve the same admitted
canonical and wallet publication without RocksDB paths, process-local
secondary generations, or sticky sessions.

### Scope

- Use short read-only Repeatable Read transactions for request snapshots.
- Internally page large reads while authenticating every page against the same
  publication and stream-history incarnation.
- Move storage-facing query contracts to asynchronous domain operations.
- Make cursor authentication fleet-portable through mounted secret material
  and persisted identity.
- Preserve existing protocol vocabulary and error mapping.

### Exit criteria

- At least three native and two compatibility replicas return the same chain
  view under randomized routing.
- Cursors resume on a different replica after process replacement.
- No public stream holds one database transaction for its network lifetime.
- Regtest proves replica interchangeability and reorg invalidation.
- Testnet proves representative historical scans and real consumer behavior.

## Slice 5: Shared mempool and source fanout

### Outcome

Every PostgreSQL reader serves the same exact-tip durable mempool generation,
indexes, transaction status, overlays, and retained events. Adding query
replicas does not multiply data-serving calls to ingest or sparse tree-state
calls to Zebra.

### Scope

- Add current entries, indexes, deltas, source-tip certification, and event
  retention to the canonical schema.
- Withdraw certification during hydration and takeover, then publish only a
  complete exact-tip generation.
- Use the same ingest writer term for canonical and mempool mutation authority.
- Keep broadcast as an explicit bounded source capability.
- Retain the existing RocksDB in-process mempool and private control path.

### Exit criteria

- Partial or old-tip hydration is never published.
- Durable scanning recovers all events after notification loss.
- Randomly routed readers return one certified generation.
- Regtest proves mining, invalidation, takeover, and cursor recovery.
- Testnet proves representative load, retention, and long-lived reconciliation.

## Slice 6: Operations, recovery, and security

### Outcome

Operators can observe, migrate, back up, restore, fail over, and recover the
PostgreSQL topology with explicit durability and security claims.

### Scope

- Complete least-privilege roles for migration, canonical writing, wallet
  writing, and reading.
- Add bounded pools, timeouts, transaction-age limits, connection-budget
  validation, application names, and pool/readiness metrics.
- Define physical base backup and continuous WAL archival.
- Require recovery finalization before traffic: validate identity, rotate
  writer terms, and rotate stream history after potentially lossy recovery.
- Certify one high-availability environment, including promotion authority and
  former-primary fencing, without moving cluster management into Zinder.
- Keep RocksDB state-bundle recovery and resource controls independent.

### Exit criteria

- Runtime roles cannot execute DDL or mutate another domain.
- Backup plus WAL restore passes application admission before readiness.
- Failover proves former-primary fencing and explicit cursor-history behavior.
- Regtest proves application recovery invariants; Testnet is required for the
  production-shaped backup, restore, failover, and maintenance drills.

## Slice 7: Scale and production certification

### Outcome

Current evidence demonstrates that `postgres-horizontal` meets Zinder’s
correctness, consumer, latency, construction, recovery, security, resource,
and horizontal-scaling contracts without weakening RocksDB support.

### Scope

- Run full-tip canonical and wallet construction through real runtimes.
- Run sparse and dense semantic digest fixtures.
- Scale reader replicas under a fixed connection and database resource budget.
- Inject crashes at authority, event, projection, publication, and recovery
  boundaries.
- Run native, lightwalletd, wallet, explorer, and Cipherscan matrices for every
  retained surface.
- Apply dependency, advisory, license, provenance, MSRV, binary, image, and
  release gates to topology-specific artifacts.

### Exit criteria

- Full-chain continuity and semantic digests match the reference.
- Reader scale increases aggregate throughput before the documented database
  bottleneck without breaching latency, connection, writer-lag, WAL, vacuum,
  storage, or cost bounds.
- A production-shaped Testnet soak includes writer takeover, reader
  replacement, database failover, backup, and restore.
- RocksDB passes its independent lifecycle, consumer, recovery, performance,
  and release gates.

## Slice 8: Publish the second topology

### Outcome

Operators can choose either `rocksdb-single-host` or `postgres-horizontal`
through separately identified artifacts and complete topology-specific
runbooks.

### Scope

- Publish topology-specific binaries and images containing exactly one engine
  dependency stack.
- Publish selection, capacity, deployment, migration, backup, failover, and
  incident guidance.
- Support optional blue-green RocksDB-to-PostgreSQL migration and a bounded
  rollback window without dual writes or row conversion.
- Remove tracer-only code and the benchmark-only PostgreSQL schema after the
  production path supersedes them.
- Make topology identity mandatory in diagnostics, support bundles, metrics,
  and issue templates.

### Exit criteria

- Clean deployments of both topologies pass configuration-to-public-traffic
  acceptance paths.
- Mixed topology identity, unavailable build capability, or engine-specific
  configuration fails before readiness.
- Dependency inspection proves each production artifact contains only its
  selected topology stack.
- Documentation continues to describe RocksDB as a valid long-term operator
  choice rather than a migration stage.

## Cross-cutting side effects

- Public Rust storage-facing APIs may become asynchronous. This is an accepted
  breaking change; all in-repository consumers move in the same slice.
- Canonical rules may move into a new domain crate once two production
  composition roots prove the boundary. The extraction must not happen ahead
  of a concrete PostgreSQL consumer.
- Feature combinations increase compile and CI cost. Release artifacts compile
  exactly one topology; all-feature CI is compile-time coverage, not a third
  supported mixed topology.
- PostgreSQL introduces connection, WAL, vacuum, lock, transaction-age, and
  failover operational surfaces. Readiness and runbooks must expose these
  directly.
- Portable cursors require fleet-shared authentication material and explicit
  history rotation after recovery.
- Blue-green migration temporarily doubles source load and infrastructure
  cost; capacity plans must account for it.
- The explorer plane may remain RocksDB-only. Capability reporting and
  deployment docs must make that limitation explicit if no PostgreSQL consumer
  justifies the port.

## Completion rule

`postgres-horizontal` becomes a supported production option only after every
required plane has a real composition root, every topology-specific operational
contract is documented and exercised, the production certification slice
passes, and release artifacts are published. Until then, documentation and
diagnostics must call it an unreleased tracer or candidate. RocksDB remains a
supported operator option throughout and after that transition.
