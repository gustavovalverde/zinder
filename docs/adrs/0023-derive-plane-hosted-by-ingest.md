# ADR-0023: Derive plane hosted by ingest

Status: Accepted
Date: 2026-05-20
Related: [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0017](0017-derive-consumer-template-and-key-codec-convention.md), [ADR-0018](0018-capability-gated-optional-payload-fields.md), [ADR-0020](0020-bounded-rocksdb-resource-budget.md), [ADR-0022](0022-transparent-prevout-index.md)

## Decision

`zinder-ingest` hosts the derive-plane writer. It opens the canonical store as primary and opens the derive store as primary under the canonical store's `derive/` subdirectory. Reader runtimes open the derive store in RocksDB secondary mode.

The canonical commit remains the first durable boundary. After `PrimaryChainStore::commit_chain_epoch` returns a chain event, ingest dispatches the bundled chain-event consumers against already-parsed block contexts and writes all derived rows plus chain-event cursors in one derive-store batch. Startup replays retained canonical chain events when a canonical event committed but the derive-store cursor did not advance before a crash.

Mempool-derived rows follow the same ownership rule: ingest appends the canonical mempool event, then dispatches the bundled mempool consumer and stores its cursor in the derive store. Chain-event cursors and mempool-event cursors use separate column families because chain cursors rewind with reorg processing and mempool cursors do not.

Reader runtimes are stateless gateways over writer-owned stores. They open
canonical storage and derive storage as RocksDB secondaries, explicitly catch
up those secondary views, and serve gRPC APIs from the latest secondary
snapshot they have observed.

`zinder-explorer` does not run derive consumers. It serves `ExplorerQuery`
from the derive secondary plus any canonical `WalletQuery` endpoint required
by a handler. Capabilities advertise only when the required reader inputs are
wired: the derive store must be present for materialized explorer views, and
the wallet endpoint must be present for handlers that compose canonical state
at request time.

## Rationale

The previous explorer-hosted derive topology made explorer fetch canonical blocks through `WalletQuery.FullBlock`, which forced block bytes through explorer to query to RocksDB and back into explorer before consumers could derive rows. That shape multiplied CPU, memory, and disk pressure during catch-up and made query compete with ingest for the same volume.

Hosting derive consumers in ingest keeps reorg safety at the writer boundary. The writer already has the committed block bytes, the parsed block, the emitted chain event, and the canonical prevout read path in scope. Consumers no longer need a transport-level block source, and query no longer serves bulk derive catch-up traffic.

Separating writers from readers keeps production topology predictable: one
runtime mutates canonical and derive stores, while reader runtimes can restart,
lag, or be scaled without changing writer safety. RocksDB secondary mode gives
the reader a local view without granting writer posture or creating missing
column families by accident.

The gateway shape also keeps API boundaries independent from storage ownership.
Explorer views can be read from derive storage without asking query to replay
full blocks, while wallet-facing APIs continue to read canonical artifacts
through the wallet plane.

## Consequences

- `zinder-derive` owns the consumer traits, bundled consumer implementations, derive-store wrapper, cursor column families, and derive write-batch helpers.
- `zinder-ingest` owns block-context construction because it has the canonical commit batch and store reader in scope.
- `zinder-ingest` does not depend on `rust-rocksdb` for derive writes; RocksDB details stay behind `zinder-derive`.
- `zinder-explorer` reads materialized derive rows from a secondary derive store and advances its view with `try_catch_up`.
- The derive schema version changes when consumer column families, cursor families, key layouts, or stored payloads change.
- Secondary readers fail closed when the primary path or column-family layout is missing or incompatible.
- Capability strings are derived from configured reader inputs, not from whether the binary exists.
- Live and integration coverage for explorer materialized views targets the secondary derive-store read path, not the deleted gRPC derive fan-out path.
- Future consolidation of reader binaries can share the same secondary-store posture without moving write ownership out of ingest.
