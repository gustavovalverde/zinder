# Service operations

Every Zinder runtime exposes the same operational HTTP contract through
`zinder-runtime`:

| Route | Purpose |
| --- | --- |
| `/healthz` | Process liveness. Returns success while the runtime can answer HTTP. |
| `/readyz` | Typed traffic readiness for the runtime's owned contract. |
| `/metrics` | Prometheus metrics with bounded label sets. |

Health does not imply readiness. A writer may be alive while synchronizing or
recovering its source, and a reader may be alive while its secondaries cannot
form an exact serving pair.

## Readiness vocabulary

`ReadinessCause` is shared by JSON and protobuf surfaces:

| Cause | Operator meaning |
| --- | --- |
| `starting` | Initialization has not completed. |
| `syncing` | Owned state is healthy but behind its target. |
| `ready` | The runtime can serve its contract. |
| `node_unavailable` | Zebra or its transport is temporarily unavailable. |
| `node_capability_missing` | Zebra cannot provide a required capability. |
| `upstream_not_ready` | Zebra is reachable but not sufficiently synchronized. |
| `storage_unavailable` | Required local storage cannot be opened or read. |
| `schema_mismatch` | Persisted identity or schema differs from the binary. |
| `reorg_window_exceeded` | A replacement crosses the persisted reorg policy. |
| `replica_lagging` | A RocksDB secondary exceeds the admitted epoch lag. |
| `writer_status_unavailable` | A trusted reader cannot reach the writer control API. |
| `cursor_at_risk` | Canonical event retention is approaching an active cursor. |
| `mempool_cursor_at_risk` | Mempool retention is approaching an active cursor. |
| `mempool_source_unavailable` | The live mempool source is unavailable. |
| `mempool_hydration_lagging` | Mempool transaction hydration is falling behind. |
| `shutting_down` | New traffic has been drained for termination. |

Payload-bearing causes include structured detail. `node_unavailable` carries a
bounded failure class, sanitized reason, consecutive-failure count, and outage
duration. `upstream_not_ready` carries source heights, verification progress,
and the health-probe source. Reorg, replica, and retention causes carry the
numeric boundary that failed.

Readiness detail is operational data, not a place for raw node responses,
authorization material, filesystem paths, transaction identifiers, or other
secrets.

## Runtime-specific readiness

### Ingest

Ingest is ready when its canonical fence is within the configured source lag
and the current mempool source generation has published a complete snapshot.
Fresh construction, source recovery, schema admission, and reorg refusal keep
it unready. The private control server is supervised with the writer; an
unexpected control-server exit terminates the runtime.

### Projector

Projector is ready only after it has admitted the canonical secondary,
acquired the required leases, opened or built the wallet primary, and reached
continuous following at an authenticated source position. An individually
healthy canonical or wallet store is insufficient if their source identities
do not agree.

### Lightwalletd compatibility

Compatibility is ready only while `WalletServingPairPublisher` can reach writer
status, catch canonical and wallet secondaries up, and publish a pair that
passes exact-fence admission. Traffic uses a readiness interceptor, so a
process that has drained readiness does not accept new gRPC requests.

## Startup and shutdown

Startup phases use the shared `StartupPhase` vocabulary, including load config,
connect node, check schema, recover state, open storage, start API, and ready.
Phase duration and failure are metrics and structured logs.

On termination, a runtime sets `shutting_down`, stops accepting new traffic,
cancels background tasks, waits for owned tasks and servers, and closes the
operational endpoint. Primary stores are closed only by their owner. Readers do
not attempt a final write or migration.

## Metrics

Every runtime exports build information and readiness state. Important release
metrics include:

### Canonical writer

- `zinder_ingest_canonical_tip_height`
- `zinder_ingest_canonical_lag_blocks`
- `zinder_ingest_canonical_chain_epoch`
- `zinder_ingest_canonical_chain_event_sequence`
- `zinder_ingest_canonical_live_appends_total`
- `zinder_ingest_canonical_live_replacements_total`
- `zinder_ingest_canonical_live_commit_seconds`
- source fetch queue, reservation, response-size, and reassembly gauges
- construction watermark and persistence measurements
- `zinder_ingest_canonical_historical_prevout_reads_total`
- `zinder_ingest_canonical_cross_block_wallet_reads_total`

The last two counters are expected to remain zero and protect the canonical
block-local boundary.

### Projector and wallet store

Projector metrics report build phase, lease renewal, canonical event position,
wallet source position, projection lag, transition bytes, digest validation,
and checkpoint operations. Store metrics use explicit canonical or wallet role
labels.

### Compatibility reader

Compatibility metrics report writer-status availability, catch-up duration,
pair convergence attempts, published generation, replica lag, pair admission
failure, and exact-pair replacement. gRPC request metrics remain separate from
pair-maintenance metrics.

### RocksDB

`zinder-store` exports bounded role-labelled metrics for block-cache capacity
and use, memtables, WAL bytes and limits, pending compaction, active
compactions, write stops, bytes read and written, MultiGet behavior, Bloom
filters, read latency, and startup open duration.

Metric labels must be closed enumerations. Never use paths, addresses, hashes,
transaction identifiers, exception text, or user input as labels.

## Structured logs

Logs use a stable `event` field and the owning tracing target. Current canonical
events include:

- `canonical_writer_started`
- `canonical_ready_store_reopened`
- `canonical_empty_construction_staging_removed`
- `canonical_unpublished_construction_restarted`
- `canonical_live_append_committed`
- `canonical_live_replacement_committed`
- `materialized_view_replay_caught_up`
- `materialized_view_replay_fell_behind`

Messages explain the event for humans; automation keys on structured fields.
Logs may include bounded heights, epoch numbers, sequence numbers, durations,
and enum labels. They must not include raw authorization headers, bearer tokens,
cookie contents, disclosure bytes, viewing keys, or unbounded payloads.

## Security boundary

Public listeners require explicit `security.allow_public_bind = true`. The
release service does not provide public TLS termination, authentication, rate
limits, or quota accounting. Put public compatibility traffic behind an
operator-controlled HTTP/2 proxy.

Private ingest and projector control endpoints use bearer tokens loaded from
files. Configuration output shows redaction markers, and errors identify only
the configuration field or unreadable path, never token contents.

Node JSON-RPC authentication uses `none`, `basic`, or `cookie` according to the
shared `[node.auth]` section. Cookie and bearer material must not enter metrics,
logs, reports, or test snapshots.

## Recovery

Recoverable source errors drain readiness and retry from durable state. Store
identity, corruption, schema, and reorg-policy failures fail closed and require
operator action.

Production wallet recovery is one coherent state-bundle operation:

1. coordinate canonical and wallet owners through their private control APIs;
2. capture physical checkpoints and owner admission evidence;
3. bind both checkpoints to one canonical event fence and wallet digest;
4. restore into fresh paths;
5. cold-admit each checkpoint under bounded resources; and
6. start ingest, projector, then compatibility and require normal exact-pair
   admission before traffic.

An independently timed copy of the canonical and wallet directories is not a
coherent backup. Physical checkpoint success also does not prove query serving,
continuous following, reorg recovery, or client compatibility.

## Deployment support

The supported composition is the same-host RocksDB deployment in
`deploy/docker-compose.yml` and `deploy/systemd/`. Release images are limited to
ingest, projector, and lightwalletd compatibility. Explorer, Cipherscan, native
query, PostgreSQL, and mixed single-container compositions are not release
classes.
