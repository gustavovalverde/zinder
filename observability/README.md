# Local Observability Smoke

This directory contains the local Prometheus and Grafana stack used to inspect
Zinder metrics while the binaries run on the host. The compose file intentionally
does not containerize `zinder-ingest`, `zinder-query`, or
`zinder-compat-lightwalletd`: the smoke path should exercise the same Cargo-built
binaries that developers use during T3 live testing.

## What It Proves

`scripts/observability-smoke.sh run` performs an end-to-end checkpoint smoke
against the selected local upstream node:

1. Reads the selected Zebra node tip.
2. Backfills a fresh store from a checkpoint.
3. Creates a RocksDB checkpoint backup and verifies that a restored
   `zinder-query` process can serve the checkpointed tip.
4. Starts `zinder-ingest tip-follow`, `zinder-query`, and
   `zinder-compat-lightwalletd` with `/metrics` endpoints.
5. Attempts to mine one regtest block so the live ingest writer path records a
   commit after startup.
6. Calls the native `WalletQuery` API and the lightwalletd-compatible
   `CompactTxStreamer` API with `grpcurl`.
7. Waits for Prometheus scrapes, prints metric samples, and writes JSON and
   Markdown readiness reports under `.tmp/observability/reports`.

This is a local observability smoke, not a benchmark. It proves the metrics are
emitted, scrapeable, and usable for bottleneck investigation. Use the T2 perf
tests and `scripts/observability-smoke.sh calibrate` for latency budgets.

## Prerequisites

The defaults match the Z3 local regtest sidecar:

```bash
ZINDER_OBSERVABILITY_NODE_ADDR=http://127.0.0.1:39232
ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME=zebra
ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD=zebra
```

Required commands:

- `cargo`
- `curl`
- `docker`
- `grpcurl`
- `jq`
- `python3`

## Run

```bash
scripts/observability-smoke.sh run
```

The script leaves the services running so dashboards remain inspectable:

- Prometheus: <http://127.0.0.1:9095>
- Grafana: <http://127.0.0.1:3002>
- Ingest metrics: <http://127.0.0.1:9190/metrics>
- Query metrics: <http://127.0.0.1:9191/metrics>
- Compat metrics: <http://127.0.0.1:9192/metrics>

Grafana uses `admin/admin` by default. Override with
`ZINDER_GRAFANA_ADMIN_PASSWORD` when needed.

To print the current evidence again:

```bash
scripts/observability-smoke.sh snapshot
```

To stop the local services and compose stack:

```bash
scripts/observability-smoke.sh stop
```

The legacy `scripts/observability-regtest-smoke.sh` filename remains as a
compatibility wrapper. New docs and automation should use the network-neutral
`scripts/observability-smoke.sh` name.

## Public-Network Smokes

Use public-network smokes when a synced local node is available. Set
`ZINDER_OBSERVABILITY_GENERATE_BLOCKS=0` because public networks cannot mine
ad-hoc blocks:

```bash
AUTH="$(docker exec z3_zebra sh -lc 'cat /var/run/auth/.cookie')"
ZINDER_OBSERVABILITY_NETWORK=zcash-testnet \
ZINDER_OBSERVABILITY_NODE_ADDR=http://127.0.0.1:18232 \
ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME="${AUTH%%:*}" \
ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD="${AUTH#*:}" \
ZINDER_OBSERVABILITY_GENERATE_BLOCKS=0 \
ZINDER_OBSERVABILITY_BACKFILL_BLOCKS=100 \
scripts/observability-smoke.sh run
```

```bash
AUTH="$(docker exec z3_mainnet_observability_zebra sh -lc 'cat /var/run/auth/.cookie')"
ZINDER_OBSERVABILITY_NETWORK=zcash-mainnet \
ZINDER_OBSERVABILITY_NODE_ADDR=http://127.0.0.1:29232 \
ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME="${AUTH%%:*}" \
ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD="${AUTH#*:}" \
ZINDER_OBSERVABILITY_GENERATE_BLOCKS=0 \
ZINDER_OBSERVABILITY_BACKFILL_BLOCKS=1000 \
scripts/observability-smoke.sh run
```

## Calibration

`calibrate` repeats the full smoke and writes an aggregate baseline with P50,
P95, P99, and worst-case values:

```bash
ZINDER_OBSERVABILITY_NETWORK=zcash-mainnet \
ZINDER_OBSERVABILITY_NODE_ADDR=http://127.0.0.1:29232 \
ZINDER_OBSERVABILITY_NODE_AUTH_USERNAME="${AUTH%%:*}" \
ZINDER_OBSERVABILITY_NODE_AUTH_PASSWORD="${AUTH#*:}" \
ZINDER_OBSERVABILITY_GENERATE_BLOCKS=0 \
ZINDER_OBSERVABILITY_BACKFILL_BLOCKS=1000 \
ZINDER_OBSERVABILITY_RUNS=6 \
scripts/observability-smoke.sh calibrate
```

Reports are written to:

- `.tmp/observability/reports/latest-readiness.json`
- `.tmp/observability/reports/latest-readiness.md`
- `.tmp/observability/reports/latest-calibration.json`
- `.tmp/observability/reports/latest-calibration.md`

## Tunables

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `ZINDER_OBSERVABILITY_NETWORK` | `zcash-regtest` | Network written to service configs. |
| `ZINDER_OBSERVABILITY_BACKFILL_BLOCKS` | `50` | Blocks backfilled after the checkpoint. |
| `ZINDER_OBSERVABILITY_COMMIT_BATCH_BLOCKS` | `25` | Ingest commit batch size. |
| `ZINDER_OBSERVABILITY_GENERATE_BLOCKS` | `1` | Regtest blocks to mine after tip-follow starts. Set `0` to skip. |
| `ZINDER_OBSERVABILITY_RESET` | `1` | Reset `.tmp/observability` before a run. |
| `ZINDER_OBSERVABILITY_BACKUP_RESTORE` | `1` | Create a checkpoint backup and verify it through a restored query process. |
| `ZINDER_OBSERVABILITY_RUNS` | `5` | Number of smoke repetitions for `calibrate`. |
| `ZINDER_PROMETHEUS_PORT` | `9095` | Host Prometheus port. |
| `ZINDER_GRAFANA_PORT` | `3002` | Host Grafana port. |
| `ZINDER_OBSERVABILITY_INGEST_OPS_ADDR` | `0.0.0.0:9190` | Ingest `/metrics` bind address. |
| `ZINDER_OBSERVABILITY_QUERY_OPS_ADDR` | `0.0.0.0:9191` | Query `/metrics` bind address. |
| `ZINDER_OBSERVABILITY_COMPAT_OPS_ADDR` | `0.0.0.0:9192` | Compat `/metrics` bind address. |

The script writes generated configs and logs under `.tmp/observability`, which
is ignored by Git. It strips `ZINDER_OBSERVABILITY_*` variables before launching
Zinder binaries, so harness-control values cannot be mistaken for production
configuration by the shared `ZINDER_*` config loader.

## Expected Signals

The smoke should produce samples for:

- `zinder_node_request_total`
- `zinder_readiness_state`
- `zinder_readiness_sync_lag_blocks`
- `zinder_readiness_replica_lag_chain_epochs`
- `zinder_ingest_commit_duration_seconds_count`
- `zinder_ingest_writer_chain_epoch_id`
- `zinder_ingest_writer_status_request_total`
- `zinder_query_request_total`
- `zinder_query_secondary_catchup_total`
- `zinder_query_secondary_replica_lag_chain_epochs`
- `zinder_query_writer_status_request_total`
- `zinder_store_read_duration_seconds_count`
- `zinder_store_visibility_seek_total`
- `zinder_store_rocksdb_property`

If the local node cannot mine regtest blocks, the ingest process still
runs and node-poll metrics remain visible, but the live
`zinder_ingest_commit_duration_seconds_count` sample may stay absent until a new
block arrives.

The default smoke run now executes the one-shot `backup` command and verifies
the restored checkpoint by serving `WalletQuery/LatestBlock` from it. Backup
metrics may still be absent from Prometheus because the backup process exits
before a long-running scrape path exists; the readiness report records the
backup-restore outcome directly.

## Alert Rules

Prometheus loads `observability/prometheus/rules/zinder-readiness.yml`. The
local rules cover:

- scrape targets down
- non-ready readiness causes
- secondary replica lag
- node RPC errors
- wallet-query p95 above 250ms
- store-read p95 above 50ms
- RocksDB pending compaction bytes above 256MiB
- more than four RocksDB compactions running for 15 minutes

## Grafana Overview

The bundled dashboard highlights multi-process storage-access signals that
should make writer, secondary-reader, and replica-lag failures easy to spot:

- `Non-Ready Services`: red when any service reports a non-ready readiness cause.
- `Storage Access Availability`: red when writer-status serving, writer-status
  fetching, or secondary visibility is unavailable over the last five minutes.
- `Replica Lag`: chain-epoch lag from readiness and secondary catchup.
- `Writer vs Reader Chain Epoch`: writer progress versus query-visible progress.
- `Secondary Catchup P95`: catchup latency for the secondary reader.
- `Storage Access Error Rate`: writer-status and secondary-catchup error rates.
  A flat zero line is the healthy state.
- `Backup Outcomes`: optional backup command outcomes. The default smoke run
  shows `not_exercised_by_smoke` because backup is a one-shot command and is
  not part of the long-running scrape path.
