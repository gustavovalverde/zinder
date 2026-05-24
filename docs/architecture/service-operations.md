# Service Operations

Zinder should be easy to run without hiding failure. Operators need typed states, useful metrics, and production configuration that fails closed.

## Startup Phases

Each service should expose a startup phase:

- `load_config`
- `validate_config`
- `open_storage`
- `check_schema`
- `connect_node`
- `recover_state`
- `start_api`
- `ready`

The exact phases can differ by service, but the principle should not: startup progress is typed and visible.

## Health and Readiness

Every production service must expose:

- `/healthz`: process is alive and the runtime can answer.
- `/readyz`: process can safely receive its intended production traffic.
- `/metrics`: Prometheus-compatible metrics.

These endpoints are served by `zinder-runtime::serve_ops_endpoint` over a separate HTTP listener configured by `--ops-listen-addr` (or the matching TOML field). Wiring the operational HTTP listener to a different socket than the gRPC service prevents accidental coupling between operator probes and wallet traffic.

gRPC services also expose the equivalent tonic health service where useful for infrastructure probes. HTTP and gRPC health surfaces must read the same typed readiness state.

Readiness should include a machine-readable cause:

```json
{
  "status": "not_ready",
  "cause": "syncing",
  "currentHeight": 1200000,
  "targetHeight": 1200500
}
```

Required readiness causes:

- `starting` — the service has begun startup; storage, source, and API are not yet wired
- `syncing` — ingestion is catching up or waiting for a replacement tip after an upstream node rewind; reads return data only when the visible epoch is past the requested height
- `ready` — every required capability is wired and the visible epoch is current within tolerance
- `node_unavailable` — the configured upstream node cannot answer or reports its own not-ready state
- `node_capability_missing` — the upstream node is reachable but lacks a required capability (see [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) and [Node source boundary §Capability Model](node-source-boundary.md#capability-model))
- `upstream_not_ready` — the configured upstream node is reachable but reports itself as not at the network tip; sourced from Zebra's `/ready` endpoint when `[node.health].addr` is configured, or from `getblockchaininfo.verificationprogress` + `estimatedheight` as the fallback. Carries the structured payload `{ upstream_committed_height, upstream_estimated_height, upstream_verification_progress, upstream_health.source, upstream_health.reason }`. See [ADR-0015 §Upstream sync detection](../adrs/0015-unified-phase-driven-ingest.md#upstream-sync-detection)
- `storage_unavailable` — canonical RocksDB cannot answer or has lost the visible epoch pointer
- `schema_mismatch` — Zinder's expected schema version differs from the persisted store's schema fingerprint
- `reorg_window_exceeded` — the selected branch requires replacing data outside the configured reorg window; operator action required
- `replica_lagging` — a `zinder-query` or `zinder-compat-lightwalletd` secondary RocksDB reader is behind the writer by more than `secondary_replica_lag_threshold_chain_epochs` (per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md)); reads still serve from the last replayed state. Usually self-heals within one catchup interval; persistent lag indicates the writer is offline or under load
- `writer_status_unavailable` — a secondary reader cannot reach `zinder-ingest`'s private ingest-control endpoint and has no cached writer epoch to compare against; verify `ingest_control.addr` and the ingest-control listener
- `cursor_at_risk` — chain-event retention is approaching exhaustion under load (per [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure)); writes still commit and reads still serve, but long-running consumer cursors are at risk of expiry. Operators tune retention or drain consumers
- `mempool_cursor_at_risk` — mempool-event retention is approaching exhaustion (per [ADR-0007 §Retention windows](../adrs/0007-mempool-topology-and-retention.md)); same posture as `cursor_at_risk` but on the `mempool_event` column family, with separate Mined/Invalidated/Added windows
- `mempool_source_unavailable` — the mempool source stream cannot be opened or has emitted `MempoolStreamUnavailable`; the live `MempoolIndex` keeps the last known state but no new `Added`/`Invalidated`/`Mined` events are arriving. Operators check upstream node health and the indexer port (`ZEBRA_RPC__INDEXER_LISTEN_ADDR` for streaming, `getrawmempool` reachability for polling)
- `mempool_hydration_lagging` — hydration of `MempoolChange::ADDED` notifications via `getrawtransaction` is falling behind the source's emission rate; the index and event log will skip events older than the lag threshold and surface them as missing rather than out-of-order. Operators check upstream JSON-RPC latency and the `zinder_node_request_duration_seconds{operation="get_raw_transaction"}` series
- `shutting_down` — graceful shutdown in progress; new traffic is rejected

`cursor_at_risk` and `mempool_*` causes are informational warnings, not traffic-blocking failures: load balancers and orchestrators should treat them as "drain or investigate, do not fail." `/readyz` still returns HTTP 200 and `"status": "ready"` for these causes, while the structured `cause` and `zinder_readiness_state` metric keep the operator-actionable signal visible. Health-check probes that flip to "unhealthy" on any non-`ready` cause will overreact to this signal. The intent is that operators see the warning before consumers are forcibly expired or mempool UX degrades.

`zinder-ingest` readiness is capped by upstream node readiness. If the selected upstream node cannot answer, reports not ready, lacks a required capability, or has unreadable cookie-auth material, Zinder reports a typed not-ready cause instead of accepting traffic.

Every source-shaped failure inside the long-running writer is a readiness transition, not a process exit. The writer loop and its siblings (the unified ingest loop, the mempool orchestrator, the chain-tip re-subscriber) consult `services/zinder-ingest/src/source_recovery.rs::decide_recovery` on every iteration; source errors recover with a backoff selected by failure class, storage and reorg-window failures exit. The `node_unavailable` readiness cause carries a structured `NodeUnavailableDetail` payload (`failure_class`, `last_reason`, `consecutive_failures`, `outage_seconds`) so operators can triage from `/readyz` without consulting logs. The full failure-class operator table lives in [Node source boundary](node-source-boundary.md#capability-model); the architectural decision is recorded in [ADR-0013](../adrs/0013-source-failure-recovery-topology.md).

## Shutdown

Long-running Rust tasks use `tokio_util::sync::CancellationToken`.

The shutdown sequence is:

```text
receive signal or internal fatal event
  -> cancel root token
  -> stop accepting new public traffic
  -> await child tasks with bounded deadlines
  -> flush storage and metrics
  -> emit shutdown result
```

Do not implement shutdown by polling atomics with `sleep`. Every task that owns a network listener, source stream, write batch, event publisher, or compatibility stream must either receive the root token or a child token.

## Metrics

Zinder uses the `metrics` facade with a process-wide Prometheus recorder
installed by `zinder-runtime`. The shared `/metrics` endpoint renders that
recorder directly; services and domain crates own their own measurements and do
not start private metrics servers.

Metric labels must stay bounded and operational: `operation`, `status`,
`error_class`, `table`, `artifact_family`, `source`, `method`, `service`,
`version`, `network`, and `cause` are acceptable. Do not label by block height,
block hash, transaction id, file path, peer address, or request payload value.

Implemented baseline metrics:

| Metric | Type | Owner | Purpose |
| ------ | ---- | ----- | ------- |
| `zinder_build_info` | gauge | `zinder-runtime` | Process identity by service, version, and network. |
| `zinder_readiness_state` | gauge | `zinder-runtime` | Current readiness cause by service and network; active cause is `1`, inactive causes are `0`. |
| `zinder_readiness_sync_lag_blocks` | gauge | `zinder-runtime` | Block lag carried by `ReadinessCause::Syncing`, or `0` when not syncing. |
| `zinder_readiness_replica_lag_chain_epochs` | gauge | `zinder-runtime` | Chain-epoch lag carried by `ReadinessCause::ReplicaLagging`, or `0` otherwise. |
| `zinder_node_request_duration_seconds` | histogram | `zinder-source` | Upstream node JSON-RPC request latency by source, method, status, and error class. |
| `zinder_node_request_total` | counter | `zinder-source` | Upstream node JSON-RPC request count by source, method, status, and error class. |
| `zinder_ingest_source_request_duration_seconds` | histogram | `zinder-ingest` | Ingest source fetch latency by operation, status, and error class. |
| `zinder_ingest_source_request_total` | counter | `zinder-ingest` | Ingest source fetch count by operation, status, and error class. |
| `zinder_ingest_source_retry_total` | counter | `zinder-ingest` | Retryable source failures by ingest operation. |
| `zinder_ingest_fact_build_duration_seconds` | histogram | `zinder-ingest` | Per-block bulk-catchup derive latency by status and error class. |
| `zinder_ingest_fact_build_total` | counter | `zinder-ingest` | Per-block bulk-catchup derive count by status and error class. |
| `zinder_ingest_derive_tailer_tick_duration_seconds` | histogram | `zinder-ingest` | Derive tailer catch-up pass latency by status and error class. |
| `zinder_ingest_derive_tailer_ticks_total` | counter | `zinder-ingest` | Derive tailer catch-up pass count by status and error class. |
| `zinder_ingest_derive_replay_stage_duration_seconds` | histogram | `zinder-ingest` | Derive tailer replay stage latency by stage, status, and error class. |
| `zinder_ingest_derive_replay_events_total` | counter | `zinder-ingest` | Derive tailer replay event count by status and error class. |
| `zinder_ingest_derive_replay_blocks_total` | counter | `zinder-ingest` | Derive tailer replay block count by status and error class. |
| `zinder_ingest_derive_replay_tip_height` | gauge | `zinder-ingest` | Canonical tip height observed before a derive tailer catch-up pass. |
| `zinder_ingest_derive_replay_height` | gauge | `zinder-ingest` | Latest canonical height replayed into the derive store by the tailer. |
| `zinder_ingest_derive_replay_lag_blocks` | gauge | `zinder-ingest` | Derive lag between replay progress and canonical tip. |
| `zinder_ingest_transparent_spend_fact_resolution_total` | counter | `zinder-ingest` | Transparent spend facts resolved during canonical ingest by status: `resolved` or `unresolved`. |
| `zinder_ingest_transparent_spend_fact_read_total` | counter | `zinder-ingest` | Transparent spend facts read while building derive contexts by status: `resolved` or `unresolved`. |
| `zinder_ingest_transparent_spend_fact_requested_outpoint_count` | histogram | `zinder-ingest` | Unique transparent outpoints requested while building one derive context batch. |
| `zinder_ingest_raw_blob_disabled_total` | counter | `zinder-ingest` | Raw block or transaction blob rows intentionally skipped by `storage.raw_blob_policy`, labeled by table. |
| `zinder_ingest_backfill_stage_duration_seconds` | histogram | `zinder-ingest` | Bulk-catchup/backfill stage latency by stage, status, and error class; stages include `await_fact_build`, `populate_subtree_roots`, and `flush_store`. |
| `zinder_ingest_commit_duration_seconds` | histogram | `zinder-ingest` | Chain-epoch commit latency by status and error class. |
| `zinder_ingest_commit_stage_duration_seconds` | histogram | `zinder-ingest` | Chain-epoch commit substage latency by stage, status, and error class. |
| `zinder_ingest_commit_batch_block_count` | histogram | `zinder-ingest` | Blocks per ingest commit batch by status. |
| `zinder_ingest_commit_batch_transaction_count` | histogram | `zinder-ingest` | Transactions per ingest commit batch by status. |
| `zinder_ingest_commit_batch_transparent_output_count` | histogram | `zinder-ingest` | Transparent outputs per ingest commit batch by status. |
| `zinder_ingest_commit_batch_transparent_spend_reference_count` | histogram | `zinder-ingest` | Transparent spend references per ingest commit batch by status. |
| `zinder_ingest_batch_accumulator_blocks` | gauge | `zinder-ingest` | Blocks currently accumulated in the in-flight ingest batch. |
| `zinder_ingest_batch_accumulator_transactions` | gauge | `zinder-ingest` | Transactions currently accumulated in the in-flight ingest batch. |
| `zinder_ingest_batch_accumulator_transparent_outputs` | gauge | `zinder-ingest` | Transparent outputs currently accumulated in the in-flight ingest batch. |
| `zinder_ingest_batch_accumulator_transparent_spend_references` | gauge | `zinder-ingest` | Transparent spend references currently accumulated in the in-flight ingest batch. |
| `zinder_ingest_batch_commit_trigger_total` | counter | `zinder-ingest` | Bulk-catchup batch commits by trigger: `block_count`, `transactions`, `transparent_outputs`, or `transparent_spend_references`. |
| `zinder_ingest_writer_has_chain_epoch` | gauge | `zinder-ingest` | Whether the ingest writer currently has a visible chain epoch. |
| `zinder_ingest_writer_chain_epoch_id` | gauge | `zinder-ingest` | Latest visible chain-epoch id published by the ingest writer. |
| `zinder_ingest_writer_tip_height` | gauge | `zinder-ingest` | Latest visible tip height published by the ingest writer. |
| `zinder_ingest_writer_finalized_height` | gauge | `zinder-ingest` | Latest visible finalized height published by the ingest writer. |
| `zinder_ingest_writer_status_request_duration_seconds` | histogram | `zinder-ingest` | Private writer-status RPC latency by status and error class. |
| `zinder_ingest_writer_status_request_total` | counter | `zinder-ingest` | Private writer-status RPC count by status and error class. |
| `zinder_ingest_writer_status_available` | gauge | `zinder-ingest` | Whether the latest writer-status RPC served successfully. |
| `zinder_ingest_backup_duration_seconds` | histogram | `zinder-ingest` | RocksDB checkpoint creation latency by network, status, and error class. |
| `zinder_ingest_backup_total` | counter | `zinder-ingest` | RocksDB checkpoint creation count by network, status, and error class. |
| `zinder_ingest_backup_last_success_unix_seconds` | gauge | `zinder-ingest` | Unix timestamp of the latest successful checkpoint creation by network. |
| `zinder_query_request_duration_seconds` | histogram | `zinder-query` | Wallet-query operation latency by operation, status, and error class. |
| `zinder_query_request_total` | counter | `zinder-query` | Wallet-query operation count by operation, status, and error class. |
| `zinder_query_compact_block_range_block_count` | histogram | `zinder-query` | Compact-block range size by status. |
| `zinder_query_secondary_catchup_duration_seconds` | histogram | `zinder-query` | RocksDB secondary catchup pass latency by status and error class. |
| `zinder_query_secondary_catchup_total` | counter | `zinder-query` | RocksDB secondary catchup pass count by status and error class. |
| `zinder_query_secondary_has_visible_epoch` | gauge | `zinder-query` | Whether the secondary reader has replayed a visible chain epoch. |
| `zinder_query_secondary_chain_epoch_id` | gauge | `zinder-query` | Latest chain-epoch id visible to the secondary reader, or `0` when none is visible. |
| `zinder_query_secondary_tip_height` | gauge | `zinder-query` | Latest tip height visible to the secondary reader, or `0` when none is visible. |
| `zinder_query_secondary_replica_lag_chain_epochs` | gauge | `zinder-query` | Chain-epoch distance between the writer status and the secondary reader. |
| `zinder_query_writer_status_request_duration_seconds` | histogram | `zinder-query` | Client-side writer-status RPC latency by status and error class. |
| `zinder_query_writer_status_request_total` | counter | `zinder-query` | Client-side writer-status RPC count by status and error class. |
| `zinder_query_writer_status_available` | gauge | `zinder-query` | Whether the latest writer-status fetch succeeded. |
| `zinder_query_writer_status_has_chain_epoch` | gauge | `zinder-query` | Whether the latest writer-status response carried a writer chain epoch. |
| `zinder_query_writer_status_chain_epoch_id` | gauge | `zinder-query` | Latest writer chain-epoch id observed through writer status. |
| `zinder_query_writer_status_tip_height` | gauge | `zinder-query` | Latest writer tip height observed through writer status. |
| `zinder_query_writer_status_finalized_height` | gauge | `zinder-query` | Latest writer finalized height observed through writer status. |
| `zinder_store_read_duration_seconds` | histogram | `zinder-store` | RocksDB read latency by operation, column family, and status. |
| `zinder_store_read_bytes_total` | counter | `zinder-store` | Bytes returned from successful RocksDB reads. |
| `zinder_store_multi_get_key_count` | histogram | `zinder-store` | Key fanout for `multi_get` reads. |
| `zinder_store_write_batch_duration_seconds` | histogram | `zinder-store` | RocksDB write-batch latency by status. |
| `zinder_store_write_batch_rows_total` | counter | `zinder-store` | Write-batch row count by put/delete kind and column family. |
| `zinder_store_write_batch_bytes_total` | counter | `zinder-store` | Write-batch payload bytes by put/delete kind and column family. |
| `zinder_store_visibility_seek_total` | counter | `zinder-store` | Visibility-index reverse seeks by artifact family. |
| `zinder_store_rocksdb_property` | gauge | `zinder-store` | Curated RocksDB integer properties by column family and property name. |
| `zinder_mempool_hydration_failures_total` | counter | `zinder-source` | Mempool `Added` observations the source could not hydrate by reason (transient JSON-RPC failure, payload too large, unknown txid races). |
| `zinder_mempool_source_errors_total` | counter | `zinder-source` | Mempool source error items by kind (`stream_item`, `connect`); a non-zero rate is the input signal for `mempool_source_unavailable` readiness. |
| `zinder_mempool_events_pruned_total` | counter | `zinder-store` | Mempool events pruned by the retention worker by kind (`added`, `invalidated`, `mined`); the cumulative health signal for two-tier retention. |
| `zinder_mempool_event_retention_oldest_age_seconds` | gauge | `zinder-store` | Age of the oldest retained mempool event in seconds; together with the per-variant retention windows in [ADR-0007](../adrs/0007-mempool-topology-and-retention.md), drives `mempool_cursor_at_risk` readiness. |
| `zinder_mempool_event_retention_oldest_sequence` | gauge | `zinder-store` | Oldest retained mempool-event sequence number; cursor consumers below this floor receive `MempoolCursorExpired`. |
| `zinder_mempool_snapshot_age_seconds` | gauge | `zinder-ingest` | Wall-clock age of the most recent `WalletQuery.MempoolSnapshot` response; published by the ingest control adapter for clients deciding whether to fall through to `MempoolEvents`. |

For local inspection and public-network baseline capture, use the host-binary
smoke harness in [`observability/README.md`](../../observability/README.md). It
starts Prometheus and Grafana through Docker Compose, runs the Zinder binaries
against the selected local node source, verifies checkpoint backup restore,
generates native and compatibility gRPC traffic, and writes readiness reports
under `.tmp/observability/reports`.

The readiness report is the durable baseline artifact. It records the selected
network, upstream node tip, checkpoint height, bulk-catchup range and duration,
wallet query p95, source RPC p95, store read p95, secondary catchup p95,
RocksDB compaction gauges, readiness lag, replica lag, and backup-restore
outcome. Use
`scripts/observability-smoke.sh calibrate` for repeated runs that aggregate P50,
P95, P99, and worst-case values before updating performance-budget tables.

`zinder-ingest` should also expose:

- Current chain height.
- Current finalized height.
- Source height.
- Chain lag.
- Blocks processed per second.
- Artifact commit latency. The baseline metric is
  `zinder_ingest_commit_duration_seconds`.
- Reorg count and max observed depth.
- Storage commit failures.
- Node request latency and error class. The baseline metrics are
  `zinder_node_request_duration_seconds` and
  `zinder_node_request_total`.
- Mempool source health and hydration outcome. The baseline metrics are
  `zinder_mempool_source_errors_total`,
  `zinder_mempool_hydration_failures_total`,
  `zinder_mempool_events_pruned_total`,
  `zinder_mempool_event_retention_oldest_age_seconds`,
  `zinder_mempool_event_retention_oldest_sequence`, and
  `zinder_mempool_snapshot_age_seconds`. These drive the
  `mempool_source_unavailable`, `mempool_hydration_lagging`, and
  `mempool_cursor_at_risk` readiness causes.

`zinder-query` should expose:

- Request count by endpoint and status. The baseline metric is
  `zinder_query_request_total`.
- Request latency by endpoint. The baseline metric is
  `zinder_query_request_duration_seconds`.
- Response epoch age.
- Compact block cache hit ratio if a cache exists.
- Transaction broadcast result class.
- Storage read latency and error class. The baseline metric is
  `zinder_store_read_duration_seconds`.
- Secondary catchup lag (per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md)): current chain-epoch lag and time since last successful catchup.

`zinder-explorer` should expose:

- Last consumed epoch.
- Derived-index lag.
- Replay progress.
- Sink write latency.
- Failed artifact count by cause.

## Observability Federation

Zinder ships its own observability stack and never assumes the
upstream platform's observability is present. Federation across the
two stacks is operator-driven, not contract-driven.

### Ownership boundary

The boundary mirrors the code boundary. Zinder owns Zinder's
metrics; the upstream platform (see [Node source boundary §Upstream Platform Bindings](node-source-boundary.md#upstream-platform-bindings))
owns the platform's metrics. Both ship standalone-functional
observability.

### What Zinder ships

`deploy/docker-compose.yml` includes a `zinder-prometheus` service
that scrapes `zinder-ingest:9105`, `zinder-query:9106`, and
`zinder-explorer:9069` over a project-scoped Docker network
(`zinder-<network>-observability`). One Prometheus runs per zinder
stack, so mainnet and testnet stay isolated and a single host can
hold both. The service is always on: it comes up with every
`docker compose up -d`. It does not depend on whether the platform's
observability is enabled, on whether the platform exists at all, or
on which binding the operator chose. Metrics collection is
continuous for the life of the deployment.

A `zinder-grafana` service ships behind `--profile observability`.
It is opt-in because many operators feed Zinder's metrics into a
sibling Grafana (the platform's, the company-wide one, Grafana
Cloud) and do not want a second one. The minimum guarantee is
"metrics are always collected"; the bonus is "and visualised if
you want."

The Grafana provisioning under `observability/grafana/` is shared
between the deploy-mode observability and the smoke-mode
observability documented in
[`observability/README.md`](../../observability/README.md). The
dashboards are agnostic to whether the metrics come from
local-binary smoke or compose-attached deploy.

### Resource Guardrails

`deploy/docker-compose.yml` sets per-service memory ceilings through
`mem_limit`, with network-specific defaults in `deploy/.env.<network>`.
These limits are cgroup guardrails, not throughput controls. The
application still owns bounded work units through canonical commit size,
derive replay batch size, RocksDB WAL/cache budgets, and explicit
backpressure. A memory limit should catch a regression that escapes those
bounds; it should not be the mechanism that makes normal bulk catchup fit.

Mainnet ingest gets the largest default ceiling because historical replay
and transparent-output hydration are the only local stack paths expected to
reach multi-GiB RSS during catchup. Readers and observability services stay
under smaller limits so one runaway sidecar cannot starve the writer or the
upstream Zebra process.

`zinder-ingest` samples its cgroup and process RSS on a dedicated periodic
task. The exported memory gauges are runtime health signals, not derive-replay
progress signals, so they remain fresh while canonical catchup or derive replay
spends a long time inside a single work pass. The derive replay budget gauges
use the same current memory cadence, so `canonical-first` pressure state is
observable even when the tailer is still finishing a retained-event pass.

### Federation patterns

When operators want a single Grafana for everything, they have
three choices, none of which require code changes in Zinder:

1. **Cross-data-source Grafana.** Add Zinder's `zinder-prometheus`
   as an additional data source in the platform's Grafana. Both
   data sources live side by side; dashboards pick whichever one
   carries the series. Lowest-friction; operator action is a
   single Grafana datasource config.
2. **Prometheus federation.** Configure the platform's Prometheus
   to scrape Zinder's Prometheus via `/federate`. All Zinder
   metrics land in the platform's TSDB. Suitable for operators who
   want a single Prometheus to query against.
3. **Remote-write to an external sink.** Configure either
   Prometheus instance to remote-write to Grafana Cloud, Cortex,
   Mimir, or Thanos. Suitable for multi-environment fleets.

The architectural commitment is that Zinder's observability stays
standalone-functional regardless of which (if any) federation
pattern the operator picks.

### What the upstream platform must not assume

A platform binding must not assume Zinder's metrics will appear
in the platform's observability stack. A platform dashboard that
depends on Zinder metric series will break for operators who do
not federate. Zinder reserves the right to evolve its metric
label vocabulary (subject to
[Public interfaces](public-interfaces.md) review), and federation
makes that evolution visible to the platform stack. Cross-stack
coupling is operator-owned, not contract-owned.

## Logs

Logs should be structured. Production binaries use the `tracing` ecosystem with a `tracing-subscriber` layer that writes to stderr. The default level is `info`, overridable through `RUST_LOG` (the standard `EnvFilter` directive grammar).

Required structured fields:

- `service`
- `version`
- `network`
- `chain_epoch` when available
- `tip_height` when available
- `tip_hash` when available
- `request_id` for API requests
- `phase` for startup logs

Logs must not include wallet secrets, seed phrases, spending keys, viewing keys, or raw authorization material.

Redaction must be enforced in the logging layer. Call sites should emit typed fields and rely on the layer to remove or hash sensitive values consistently.

Configuration output must make redaction observable. If `--print-config` includes a secret-bearing field, it should render an explicit marker such as `[REDACTED]` rather than relying on `Debug` output, omission, or formatting side effects to hide the value.

`--print-config` writes the rendered TOML on stdout so operators can pipe it through ordinary text tools. All other operator-visible output, including failures during config load, runs through the tracing layer on stderr. Stdout therefore stays free of operational noise even when tracing is filtering at the default `info` level.

### Ingest event vocabulary

`zinder-ingest` emits one structured tracing event for every successful chain-epoch commit and every phase transition, keyed on the `event` field. Operators can filter the stream by `event` without parsing the human-readable message:

| `event`                       | Level  | Triggered by                                     |
| ----------------------------- | ------ | ------------------------------------------------ |
| `chain_committed`             | INFO   | Pure append, finalization advance, or any other transition that does not invalidate visible blocks |
| `chain_reorged`               | WARN   | A non-finalized range is replaced by a new committed range inside the reorg window |
| `ingest_started`              | INFO   | The unified ingest loop begins (after store open and upstream probe) |
| `ingest_phase_changed`        | INFO   | The loop's classifier moves between `awaiting_upstream`, `bulk_catchup`, and `following_tip`; carries `from`, `to`, `gap_blocks` |
| `ingest_source_unavailable`   | WARN   | The loop observed an upstream source failure and moved readiness to `node_unavailable` (tagged with `phase`, `failure_class`) |
| `ingest_source_recovered`     | INFO   | The loop recovered from `node_unavailable` and resumed normal readiness calculation |
| `ingest_upstream_not_ready`   | WARN   | Zebra's `/ready` probe (or the `verificationprogress` fallback) reports the upstream is itself syncing or stale; tagged with `source` (`zebra_ready_endpoint` or `verification_progress_fallback`) and `reason` |
| `ingest_upstream_ready`       | INFO   | Upstream recovers from `upstream_not_ready` |
| `ingest_stopped`              | INFO   | The loop exits because the cancellation token fired or a fatal error escaped |
| `ingest_run_failed`           | ERROR  | The process returned an error before clean shutdown |

`chain_committed` carries `chain_epoch_id`, `network`, `tip_height`, `tip_hash`, `finalized_height`, `block_range_start`, `block_range_end`, `event_sequence`, and `phase` (`bulk_catchup` or `following_tip`). `chain_reorged` extends that schema with `committed_block_range_start`, `committed_block_range_end`, `reverted_block_range_start`, and `reverted_block_range_end`. `event_sequence` matches the monotonic chain-event sequence persisted by the store, so operators can correlate logs with `chain_event_history` cursor positions.

The two chain-transition event names (`chain_committed`, `chain_reorged`) match the `ChainEvent` variants defined in [chain events](chain-events.md). Future variants must extend this table before code emits them.

## Production Configuration

Production config should reject:

- Missing persistent storage.
- Placeholder upstream node credentials.
- Unknown network names.
- Canonical storage already anchored to a different network.
- Zero reorg-window or ingest commit-batch sizes.
- Unsafe debug endpoints.
- Incompatible service and storage schema versions.
- A secondary reader binary whose `MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION` is lower than the persisted store version (per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md)).
- A `wallet-serving` coverage configuration that also enables `allow_near_tip_finalize` (per [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md)).

Configuration precedence is:

```text
defaults -> config file -> ZINDER_* environment variables -> CLI flags
```

All production binaries use `config-rs` for source layering. TOML is the canonical file-source format, loaded through `config-rs` with the `toml` feature; it is not a separate hand-written parser or a prototype shortcut. Pin the file source format explicitly instead of relying on extension guessing, so `--config ./zinder-ingest.toml` is an operator contract.

The loader shape is:

1. Start from typed defaults.
2. Merge an optional TOML config file.
3. Merge `ZINDER_` environment variables with `__` nesting.
4. Apply CLI overrides.
5. Deserialize into the service-specific config type.
6. Run `validate_config` before storage, source, or network-bind side effects.

Use `ZINDER_` with `__` for nesting, for example `ZINDER_NODE__JSON_RPC_ADDR` and `ZINDER_QUERY__LISTEN_ADDR`. Service code should not read production configuration directly from `std::env`; test-only gates use the explicit `ZINDER_TEST_*` namespace (`ZINDER_TEST_LIVE`, `ZINDER_STORE_CRASH_*`) which is stripped from production reads in `zinder_runtime::zinder_environment_source`. There is no parallel `ZINDER_Z3_*` namespace; live tests reuse the production `ZINDER_NETWORK` and `ZINDER_NODE__*` schema (see [§Validation Tiers](#validation-tiers)).

Secrets pass through the env-var loader unchanged. Secret hygiene lives at the emit boundary: `--print-config`, structured logs, and `Debug` impls redact every secret regardless of how it was supplied. The ingest-control bearer token remains file-only ([ADR-0006](../adrs/0006-ingest-control-transport-security.md)).

Do not expose secret-bearing CLI overrides. Command-line flags are for non-secret selectors and operational knobs; password, token, cookie, key, and secret material must come from the accepted config source or the operator secret-management layer.

Each production binary exposes `--config`, `--print-config`, and command-specific CLI overrides. `--print-config` emits the effective post-merge configuration in the same TOML field shape accepted by the file loader, with secret-bearing values visibly redacted. Rendering should use a TOML serializer or equivalent structured emitter, not hand-built escaping.

Use typed configuration for valid combinations. For example, source authentication is an enum (`None`, `Cookie`, `Basic`) rather than a bool paired with optional credentials.

`zinder-ingest` reads `[network]`, `[node]` (with the optional
`[node.health]` sub-section), `[storage]`, `[ingest_control]`,
`[retention]`, and the `[ingest]` section with its four concern-named
sub-sections. Per [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md)
the binary picks its phase from the gap between Zinder's store and
Zebra's tip, so there is no per-subcommand split:

```toml
[ingest]
reorg_window_blocks = 100        # chain-truth invariant

[ingest.phases]
catchup_threshold_blocks = 100   # defaults to ingest.reorg_window_blocks

[ingest.derive]
replay_concurrency = 16          # derive replay context width
replay_batch_blocks = 100        # bounded derive replay write chunk
replay_policy = "canonical-first"

[ingest.bulk_catchup]
canonical_batch_max_blocks = 1000
canonical_batch_max_artifact_bytes = 536870912
source_segment_max_blocks = 128
source_segment_target_response_bytes = 50331648
source_fetch_max_in_flight_requests = 8
source_fetch_max_in_flight_bytes = 268435456
fact_build_concurrency = 16

[ingest.tip_follow]
poll_interval_ms = 1000
lag_threshold_blocks = 1

[ingest.modifiers]
# target_height = ...
# checkpoint_height = ...
# allow_near_tip_finalize = false
# coverage = "explicit"

[node.health]
# addr = "http://zebra:8080"
# poll_interval_ms = 30000
# verification_progress_floor = 0.999
# estimated_gap_floor_blocks = 10

[ingest_control]
listen_addr = "127.0.0.1:9100"
```

`source_segment_max_blocks` is a hard ceiling, not the steady-state request size.
Bulk catch-up targets 75% of `node.max_response_bytes`, records source-segment
payload bytes, shrinks the next request after oversized responses or dense
payload samples, grows back after sustained success, carries learned density
across bulk commit batches, and resets density when the node-advertised
consensus branch changes. The source stage keeps a bounded ordered prefetch
queue so several source segments can be awaiting Zebra at once without changing
the serial commit contract. The Zebra JSON-RPC adapter still splits an oversized
in-flight segment and emits
`zinder_node_source_segment_split_total{reason="response_too_large"}`; repeated
splits should drive `zinder_ingest_source_segment_next_blocks` down without an
operator retune.

`ingest.tip_follow.poll_interval_ms` and `node.health.poll_interval_ms`
must be non-zero. Shutdown is driven by a `CancellationToken`; the
CLI root token is cancelled on `ctrl-c`, and the loop checks the
token through `tokio::select!` instead of polling a boolean flag.

`zinder-ingest backup --to <path>` uses `[network]` and `[storage]` plus a
subcommand-specific `[backup] to_path` field when invoked through config. It
opens the store as `PrimaryChainStore` and creates a RocksDB checkpoint; it
does not connect to the upstream node.

## Recovery

Expected recovery behavior:

- If `zinder-query` fails, restart it without affecting ingestion.
- If `zinder-explorer` fails, mark derived indexes stale and rebuild or resume later.
- If `zinder-ingest` fails during an epoch commit, restart from the last committed epoch or fail with `storage_unavailable` or `schema_mismatch`.
- If a reorg exceeds the configured window, fail closed and require operator action.

## Deployment Guidance

Minimum production deployment:

```text
1 x zinder-ingest
N x zinder-query
0..N x zinder-compat-lightwalletd
```

Optional derived deployment:

```text
1 x zinder-ingest
N x zinder-query
0..N x zinder-compat-lightwalletd
M x zinder-explorer
```

Only one ingest writer should own a canonical storage namespace unless leader election and write fencing are explicitly designed.

### Zallet with Zinder

The default Zallet deployment uses `zinder-client::RemoteChainIndex` over a
separately run `zinder-query` process:

```text
1 x zinder-ingest
1 x zinder-query
1 x Zallet using RemoteChainIndex -> zinder-query
```

This is the recommended operator recipe because the wallet and the indexer do
not share a store path. `zinder-query` owns RocksDB secondary catchup,
writer-status checks, public `WalletQuery` gRPC, chain-event subscriptions,
mempool proxying, `ServerInfo`, and broadcast forwarding. Zallet only needs the
query endpoint and the configured network. The same readiness, metrics, TLS, and
reverse-proxy guidance that applies to other wallet clients applies to the
Zallet endpoint.

`LocalChainIndex` is an advanced colocated optimization, not the baseline
recipe. It is appropriate only when the operator intentionally runs Zallet on
the same host as the canonical store and accepts these responsibilities:

- Zallet opens a RocksDB secondary against the canonical store path and owns a
  separate secondary path.
- The service account running Zallet has read access to the canonical store but
  must not obtain writer permissions.
- The operator monitors secondary catchup and schema-version compatibility as
  part of wallet readiness.
- Zallet still needs a subscription endpoint for chain and mempool events; use a
  colocated `zinder-query` proxy unless the deployment is explicitly event-only
  and wired to a private ingest subscription surface.
- `ServerInfo`, transaction broadcast, and any future query-plane federation
  still come from `zinder-query`.

If a deployment starts on `RemoteChainIndex`, it can later move selected reads to
`LocalChainIndex` without changing Zinder's public contract. Treat that as an
operator optimization after the Zallet integration is already correct, not as
the first production recipe.

The canonical store directory is a security boundary. Zinder stores cursor authentication material inside the store so cursors fail closed when tampered with or replayed against another store. An actor with read access to the RocksDB directory can forge local cursor tokens, so production deployments must restrict filesystem permissions to the service operator account and backup system.

Wallet-serving history requirements are owned by
[Wallet data plane §External Wallet Compatibility Claims](wallet-data-plane.md#external-wallet-compatibility-claims).
This page owns the transport part of that claim: Zinder binaries expose
plaintext gRPC and HTTP, so production Zashi compatibility requires TLS
termination in front of `zinder-compat-lightwalletd`. A reverse proxy such as
Caddy, nginx, or traefik terminates HTTPS and forwards h2c to the local compat
process. Plaintext LAN endpoints are development-only for patched SDK demo apps
and protocol debugging.

The public wallet plane (`WalletQuery` on `zinder-query` and
`CompactTxStreamer` on `zinder-compat-lightwalletd`) has no built-in
authentication; operators terminate TLS and apply auth, rate-limiting, and
per-tenant quotas at the reverse proxy.

The private `IngestControl` gRPC plane that ties `zinder-ingest`,
`zinder-query`, and `zinder-compat-lightwalletd` together is plaintext h2c.
Zinder does not offer native TLS on this port. Per
[ADR-0006](../adrs/0006-ingest-control-transport-security.md), the operator
chooses one of three deployment patterns:

1. **Localhost only.** All processes share a host and bind to `127.0.0.1`.
   No bearer token, no TLS. The OS process boundary is the only thing
   guarding the writer's storage handle.
2. **VPN or private network.** Readers run on different hosts but reach the
   writer through Wireguard, Tailscale, or a private VLAN. Configure the
   shared-secret bearer token on every process so a leaked endpoint URL
   cannot be exploited from the trust boundary's edge.
3. **Reverse proxy with TLS.** A proxy (Caddy, nginx, traefik) terminates
   HTTPS in front of the writer and forwards h2c to the local control port.
   Readers connect to the proxy's HTTPS endpoint; the bearer token still
   travels in the proxied request. This is the same pattern
   `zinder-compat-lightwalletd` uses for public wallet traffic.

The bearer token is configured by `[ingest_control] bearer_token_path` on
every process; the writer reads the file to verify and every reader reads
the same file to present. The token is loaded at startup, validated
server-side with constant-time comparison, redacted from all logs and
`--print-config` output, and never sourced from environment variables (env
vars leak into process listings and debugger snapshots). Rotation requires
updating the file on every host and restarting each process. With no token
configured, the writer accepts every request, which is correct for pattern
1 and an explicit operator choice for the others.

Writer-status clients must make wrong-endpoint failures diagnosable. A
secondary reader that reaches the wrong service on `ingest_control.addr`
reports `writer_status_unavailable`, and logs should name the configured target
and expected `zinder.v1.ingest.IngestControl/WriterStatus` RPC method rather
than repeating a generic "unimplemented" warning without context.

`created_at` fields in chain epochs are diagnostic wall-clock timestamps. Clock
steps are benign for chain ordering because ordering comes from `ChainEpochId`
and chain-event sequence, but operators may see repeated or backward-moving
timestamps in logs after an NTP adjustment.

## Validation Tiers

Tests are organized into four tiers by **runtime mechanism**. Network choice (regtest, testnet, mainnet) is a parameter on T3, not a separate tier. The detailed commands and runner profiles live in the [Testing Runbook](../runbooks/testing.md).

| Tier | Mechanism | Module path | Default cadence |
| ---- | --------- | ----------- | --------------- |
| T0 unit | in-process pure logic | `#[cfg(test)] mod tests` in `src/` | every PR |
| T1 integration | fixture-driven, no external state | `tests/integration/` | every PR |
| T2 perf | time-budgeted, no external state | `tests/perf/` | every PR (separate job) |
| T3 live | real upstream node | `tests/live/` | nightly (regtest), weekly (testnet); mainnet runs against an operator-hosted Zebra |
| T3 Zallet live | real Zallet binary against Zinder's native contract | `crates/zinder-client/tests/live/zallet.rs` | release / integration certification |

A test's tier is its directory. The directory listing is the tier inventory; filenames cannot lie.

T3 tests carry two gates: `#[ignore = LIVE_TEST_IGNORE_REASON]` plus a first-line `zinder_testkit::live::require_live()` call. `require_live()` rejects mainnet by default; mainnet-targeted tests opt in via `require_live_for(...)` or `require_live_mainnet()`.

Test functions under `tests/live/` use plain `snake_case_describing_behavior` names. Do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in the function name; the directory and runtime parameterization handle that.

`cargo nextest run` is the canonical runner. The profiles (`default`, `ci`, `ci-perf`, `ci-live`, `ci-zallet-live`, `ci-parity`) live in `.config/nextest.toml`. Live tests and production binaries read the same env-var schema (`ZINDER_NETWORK`, `ZINDER_NODE__*`); the full schema, gating contract, runner profiles, `node-mutating` group, and CI cadence are owned by the [Testing Runbook](../runbooks/testing.md) and the canonical TOML in [Public interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions).
