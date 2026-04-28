# Service Operations

Zinder should be easy to run without hiding failure. Operators need typed states, explicit migrations, useful metrics, and production configuration that fails closed.

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
- `storage_unavailable` — canonical RocksDB cannot answer or has lost the visible epoch pointer
- `schema_mismatch` — Zinder's expected schema version differs from the persisted store's schema fingerprint
- `reorg_window_exceeded` — the selected branch requires replacing data outside the configured reorg window; operator action required
- `replica_lagging` — a `zinder-query` or `zinder-compat-lightwalletd` secondary RocksDB reader is behind the writer by more than `secondary_replica_lag_threshold_chain_epochs` (per [ADR-0007](../adrs/0007-multi-process-storage-access.md)); reads still serve from the last replayed state. Usually self-heals within one catchup interval; persistent lag indicates the writer is offline or under load
- `writer_status_unavailable` — a secondary reader cannot reach `zinder-ingest`'s private ingest-control endpoint and has no cached writer epoch to compare against; verify `storage.ingest_control_addr` and the ingest-control listener
- `cursor_at_risk` — chain-event or, after M3, mempool-event retention is approaching exhaustion under load (per [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure)); writes still commit and reads still serve, but long-running consumer cursors are at risk of expiry. Operators tune retention or drain consumers
- `shutting_down` — graceful shutdown in progress; new traffic is rejected

`cursor_at_risk` is informational, not failure: load balancers and orchestrators should treat it as "drain, do not fail." Health-check probes that flip to "unhealthy" on any non-`ready` cause will overreact to this signal. The intent is that operators see the warning before consumers are forcibly expired.

`zinder-ingest` readiness is capped by upstream node readiness. If the selected upstream node cannot answer, reports not ready, lacks a required capability, or has unreadable cookie-auth material, Zinder reports a typed not-ready cause instead of accepting traffic.

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
| `zinder_ingest_commit_duration_seconds` | histogram | `zinder-ingest` | Chain-epoch commit latency by status and error class. |
| `zinder_ingest_commit_batch_block_count` | histogram | `zinder-ingest` | Blocks per ingest commit batch by status. |
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

For local inspection and public-network baseline capture, use the host-binary
smoke harness in [`observability/README.md`](../../observability/README.md). It
starts Prometheus and Grafana through Docker Compose, runs the Zinder binaries
against the selected local node source, verifies checkpoint backup restore,
generates native and compatibility gRPC traffic, and writes readiness reports
under `.tmp/observability/reports`.

The readiness report is the durable baseline artifact. It records the selected
network, upstream node tip, checkpoint height, backfill range and duration, wallet
query p95, source RPC p95, store read p95, secondary catchup p95, RocksDB
compaction gauges, readiness lag, replica lag, and backup-restore outcome. Use
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
- Migration state and progress.

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
- Secondary catchup lag (per [ADR-0007](../adrs/0007-multi-process-storage-access.md)): current chain-epoch lag and time since last successful catchup.

`zinder-derive` should expose:

- Last consumed epoch.
- Derived-index lag.
- Replay progress.
- Sink write latency.
- Failed artifact count by cause.

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
- `phase` for startup and migration logs

Logs must not include wallet secrets, seed phrases, spending keys, viewing keys, or raw authorization material.

Redaction must be enforced in the logging layer. Call sites should emit typed fields and rely on the layer to remove or hash sensitive values consistently.

Configuration output must make redaction observable. If `--print-config` includes a secret-bearing field, it should render an explicit marker such as `[REDACTED]` rather than relying on `Debug` output, omission, or formatting side effects to hide the value.

`--print-config` writes the rendered TOML on stdout so operators can pipe it through ordinary text tools. All other operator-visible output, including failures during config load, runs through the tracing layer on stderr. Stdout therefore stays free of operational noise even when tracing is filtering at the default `info` level.

### Ingest event vocabulary

`zinder-ingest` emits one structured tracing event for every successful chain-epoch commit, keyed on the `event` field. Operators can filter the stream by `event` without parsing the human-readable message:

| `event`                       | Level  | Triggered by                                     |
| ----------------------------- | ------ | ------------------------------------------------ |
| `chain_committed`             | INFO   | Pure append, finalization advance, or any other transition that does not invalidate visible blocks |
| `chain_reorged`               | WARN   | A non-finalized range is replaced by a new committed range inside the reorg window |
| `tip_follow_started`          | INFO   | Tip-follow begins polling the upstream node tip   |
| `tip_follow_stopped`          | INFO   | Tip-follow exits because the cancellation token fired |
| `backfill_already_complete`   | INFO   | Requested backfill range is already covered by the current chain epoch |
| `ingest_run_failed`           | ERROR  | A subcommand returned an error before successful exit |

`chain_committed` carries `chain_epoch_id`, `network`, `tip_height`, `tip_hash`, `finalized_height`, `block_range_start`, `block_range_end`, and `event_sequence`. `chain_reorged` extends that schema with `committed_block_range_start`, `committed_block_range_end`, `reverted_block_range_start`, and `reverted_block_range_end`. `event_sequence` matches the monotonic chain-event sequence persisted by the store, so operators can correlate logs with `chain_event_history` cursor positions.

The two chain-transition event names (`chain_committed`, `chain_reorged`) match the `ChainEvent` variants defined in [chain events](chain-events.md). Future variants must extend this table before code emits them.

## Production Configuration

Production config should reject:

- Missing persistent storage.
- Placeholder upstream node credentials.
- Migration-on-start without explicit migration mode.
- Unknown network names.
- Canonical storage already anchored to a different network.
- Zero reorg-window or ingest commit-batch sizes.
- Unsafe debug endpoints.
- Incompatible service and storage schema versions.
- A secondary reader binary whose `MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION` is lower than the persisted store version (per [ADR-0007](../adrs/0007-multi-process-storage-access.md)).
- A `wallet-serving` backfill configuration that also enables `allow_near_tip_finalize` (per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md)).

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
6. Run `validate_config` before storage, source, network bind, or migration side effects.

Use `ZINDER_` with `__` for nesting, for example `ZINDER_NODE__JSON_RPC_ADDR` and `ZINDER_QUERY__LISTEN_ADDR`. Service code should not read production configuration directly from `std::env`; test-only gates use the explicit `ZINDER_TEST_*` namespace (`ZINDER_TEST_LIVE`, `ZINDER_STORE_CRASH_*`) which is stripped from production reads in `zinder_runtime::zinder_environment_source`. There is no parallel `ZINDER_Z3_*` namespace; live tests reuse the production `ZINDER_NETWORK` and `ZINDER_NODE__*` schema (see [§Validation Tiers](#validation-tiers)).

Reject production environment overrides whose leaf keys contain sensitive terms such as `password`, `secret`, `token`, `cookie`, or `private_key`. Those values belong in the config file or an operator secret-management layer, not process environment.

Do not expose secret-bearing CLI overrides. Command-line flags are for non-secret selectors and operational knobs; password, token, cookie, key, and secret material must come from the accepted config source or the operator secret-management layer.

Each production binary exposes `--config`, `--print-config`, and command-specific CLI overrides. `--print-config` emits the effective post-merge configuration in the same TOML field shape accepted by the file loader, with secret-bearing values visibly redacted. Rendering should use a TOML serializer or equivalent structured emitter, not hand-built escaping.

Use typed configuration for valid combinations. For example, source authentication is an enum (`None`, `Cookie`, `Basic`) rather than a bool paired with optional credentials.

`zinder-ingest tip-follow` uses the same source, storage, and ingest sections
as backfill, plus a mode-specific `[tip_follow]` section:

```toml
[ingest]
reorg_window_blocks = 100
commit_batch_blocks = 1

[ingest.control]
listen_addr = "127.0.0.1:9100"

[tip_follow]
poll_interval_ms = 1000
```

`poll_interval_ms` must be non-zero. Shutdown is driven by a
`CancellationToken`; the CLI root token is cancelled on `ctrl-c`, and the loop
checks the token through `tokio::select!` instead of polling a boolean flag.

`zinder-ingest backup --to <path>` uses `[network]` and `[storage]` plus a
mode-specific `[backup] to_path` field when invoked through config. It opens the
store as `PrimaryChainStore` and creates a RocksDB checkpoint; it does not
connect to the upstream node.

## Recovery

Expected recovery behavior:

- If `zinder-query` fails, restart it without affecting ingestion.
- If `zinder-derive` fails, mark derived indexes stale and rebuild or resume later.
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
M x zinder-derive
```

Only one ingest writer should own a canonical storage namespace unless leader election and write fencing are explicitly designed.

The canonical store directory is a security boundary. Zinder stores cursor authentication material inside the store so cursors fail closed when tampered with or replayed against another store. An actor with read access to the RocksDB directory can forge local cursor tokens, so production deployments must restrict filesystem permissions to the service operator account and backup system.

Wallet-serving history requirements are owned by
[Wallet data plane §External Wallet Compatibility Claims](wallet-data-plane.md#external-wallet-compatibility-claims).
This page owns the transport part of that claim: Zinder binaries expose
plaintext gRPC and HTTP, so production Zashi compatibility requires TLS
termination in front of `zinder-compat-lightwalletd`. A reverse proxy such as
Caddy, nginx, or traefik terminates HTTPS and forwards h2c to the local compat
process. Plaintext LAN endpoints are development-only for patched SDK demo apps
and protocol debugging.

Writer-status clients must make wrong-endpoint failures diagnosable. A
secondary reader that reaches the wrong service on `storage.ingest_control_addr`
reports `writer_status_unavailable`, and logs should name the configured target
and expected `zinder.v1.ingest.IngestControl/WriterStatus` RPC method rather
than repeating a generic "unimplemented" warning without context.

`created_at` fields in chain epochs are diagnostic wall-clock timestamps. Clock
steps are benign for chain ordering because ordering comes from `ChainEpochId`
and chain-event sequence, but operators may see repeated or backward-moving
timestamps in logs after an NTP adjustment.

## Validation Tiers

Tests are organized into four tiers by **runtime mechanism** ([ADR-0006](../adrs/0006-test-tiers-and-live-config.md)). Network choice (regtest, testnet, mainnet) is a parameter on T3, not a separate tier.

| Tier | Mechanism | Module path | Default cadence |
| ---- | --------- | ----------- | --------------- |
| T0 unit | in-process pure logic | `#[cfg(test)] mod tests` in `src/` | every PR |
| T1 integration | fixture-driven, no external state | `tests/integration/` | every PR |
| T2 perf | time-budgeted, no external state | `tests/perf/` | every PR (separate job) |
| T3 live | real upstream node | `tests/live/` | nightly (regtest), weekly (testnet), `workflow_dispatch` (mainnet) |

A test's tier is its directory. The directory listing is the tier inventory; filenames cannot lie.

T3 tests carry two gates: `#[ignore = LIVE_TEST_IGNORE_REASON]` plus a first-line `zinder_testkit::live::require_live()` call. `require_live()` rejects mainnet by default; mainnet-targeted tests opt in via `require_live_for(...)` or `require_live_mainnet()`.

Test functions under `tests/live/` use plain `snake_case_describing_behavior` names. Do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in the function name; the directory and runtime parameterization handle that.

`cargo nextest run` is the canonical runner. The four profiles (`default`, `ci`, `ci-perf`, `ci-live`) live in `.config/nextest.toml`. Live tests and production binaries read the same env-var schema (`ZINDER_NETWORK`, `ZINDER_NODE__*`); the full schema, gating contract, runner profiles, `node-mutating` group, and CI cadence are owned by [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) and the canonical TOML in [Public interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions).
