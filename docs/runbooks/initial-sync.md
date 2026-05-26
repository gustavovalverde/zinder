# Initial Sync

`zinder-ingest` opens the canonical store, probes Zebra's tip, classifies the gap, and runs the right phase. Operators do not pick between subcommands; the binary auto-dispatches `AwaitingUpstream`, `BulkCatchup`, or `TipFollow` based on the gap between the store and the upstream tip and transitions between them as the gap changes. The architectural decision lives in [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md); the pipeline mechanics live in [Chain ingestion §Bulk catch-up and tip following](../architecture/chain-ingestion.md#bulk-catch-up-and-tip-following).

## Run it

```bash
zinder-ingest --config /etc/zinder/ingest.toml
```

That is the entire operator surface for normal operation. Every supported deployment shape (`deploy/docker-compose.yml`, `deploy/single-container/`, bare-metal systemd, Kubernetes Deployment) invokes the binary the same way. The same invocation handles cold start on a multi-million-block backlog, routine restart at the tip, restart after long downtime, and pre-seeded snapshots; the loop classifies the gap on every iteration.

## What you'll see

The binary exposes its current phase on `/readyz` and through `IngestControl.WriterStatus`. The phase field is orthogonal to the readiness cause:

```bash
curl -sS http://127.0.0.1:9105/readyz
```

Cold start on an empty store with a tip-synced Zebra:

```json
{
  "status": "not_ready",
  "phase": "bulk_catchup",
  "cause": "syncing",
  "current_height": 600000,
  "target_height": 4016331,
  "lag_blocks": 3416331
}
```

Once the gap closes through `ingest.phases.catchup_threshold_blocks` (defaults to `reorg_window_blocks`):

```json
{
  "status": "not_ready",
  "phase": "following_tip",
  "cause": "syncing",
  "current_height": 4016330,
  "target_height": 4016331,
  "lag_blocks": 1
}
```

Steady state:

```json
{
  "status": "ready",
  "phase": "following_tip",
  "cause": "ready",
  "current_height": 4016431,
  "target_height": 4016431,
  "safe_tip_height": 4016331
}
```

Phase transitions emit a structured log event with `from`, `to`, and `gap_blocks` fields:

```bash
docker logs zinder-ingest | grep ingest_phase_changed
# ts=... event=ingest_phase_changed from=bulk_catchup to=following_tip gap_blocks=95
```

Steady-state operation emits `chain_committed` events with `phase=following_tip` and single-block ranges. The reader's `/readyz` (port 9106) shows the height the reader has visible through its secondary store handle; it can be `ready` while the writer is still in `bulk_catchup`.

## Upstream sync diagnostic

`/readyz` distinguishes "the upstream is itself behind the network tip" from other not-ready causes through the `upstream_not_ready` cause. The signal is sourced from Zebra in one of two ways.

**Primary path: Zebra's `/ready` endpoint.** Enable it in `zebrad.toml`:

```toml
[health]
listen_addr = "0.0.0.0:8080"
```

Then point Zinder at it through the optional `[node.health]` sub-section:

```toml
[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://zebra:8232"

[node.health]
addr = "http://zebra:8080"
```

`zinder-ingest` polls `/ready` every `node.health.poll_interval_ms` (default 30000 ms). When Zebra is itself syncing, `/readyz` surfaces the structured diagnostic:

```json
{
  "status": "not_ready",
  "phase": "following_tip",
  "cause": "upstream_not_ready",
  "current_height": 600000,
  "upstream_committed_height": 600000,
  "upstream_estimated_height": 4016431,
  "upstream_verification_progress": 0.149,
  "upstream_health": {
    "source": "zebra_ready_endpoint",
    "reason": "syncing"
  }
}
```

**Fallback path.** When `[node.health].addr` is unset or unreachable, Zinder derives the same signal from `getblockchaininfo.verificationprogress` and `estimatedheight`. `upstream_health.source` becomes `verification_progress_fallback`; the reason field carries the predicate that triggered (e.g., `verification_progress_below_floor`). The fallback is less authoritative because both fields are wall-clock extrapolations from the local tip's timestamp rather than peer-reported headers. Operators running Zebras with the health endpoint enabled should configure `[node.health].addr` to use the precise signal.

In both cases the loop keeps committing whatever blocks Zebra has made available; the readiness surface gates traffic so wallets do not serve stale balances. See [Chain ingestion §Upstream sync detection](../architecture/chain-ingestion.md#upstream-sync-detection) for the full design.

## Forked store

A store that has diverged from the upstream history beyond the configured `reorg_window_blocks` fails closed with `cause=reorg_window_exceeded`. The loop does not attempt to recover; the operator is expected to drop the store and restart from cold. Preserve `chain_event_history` for the divergence point first if the incident is under investigation. The store path is whatever `[storage].path` resolves to (defaults to `/var/lib/zinder/store` in the shipped deployments).

```bash
docker compose --env-file deploy/.env.mainnet -f deploy/docker-compose.yml down
docker volume rm zinder-mainnet-data
docker compose --env-file deploy/.env.mainnet -f deploy/docker-compose.yml up -d
```

(For testnet substitute `.env.testnet` + `zinder-testnet-data`; for regtest substitute `.env.regtest` + `zinder-regtest-data`.)

The fresh start runs `BulkCatchup` from the wallet-serving floor (or genesis, depending on `ingest.coverage`) and transitions to `TipFollow` when it catches up. No manual sequencing is needed.

## Migrating from the legacy `zinder-data` volumes

Earlier releases shipped three hardcoded volume names: `zinder-data` (the canonical RocksDB store), `zinder-prometheus-data` (TSDB samples), and `zinder-grafana-data` (Grafana's SQLite DB with user accounts, API keys, and any UI-created alert rules). The compose file now scopes all three per network (`zinder-<network>-data`, `zinder-<network>-prometheus`, `zinder-<network>-grafana`) so two stacks can coexist on one host. Operators with the legacy volumes should rename them before the first `up` against the new compose; otherwise each stack boots against empty volumes, and the canonical store re-syncs from cold.

What's preserved by a migration vs lost on a drop:

| Volume | Content | If dropped |
| --- | --- | --- |
| `zinder-data` | RocksDB canonical store + secondary handles | `BulkCatchup` from genesis (hours for testnet, days for mainnet) |
| `zinder-prometheus-data` | TSDB samples (retention `30d`) | Metric history restarts; mounted scrape config and alert rules are unaffected |
| `zinder-grafana-data` | Grafana SQLite DB (users, API keys, UI-created alerts, snapshots) | UI state is lost; provisioned dashboards and datasources are reapplied from the read-only mounts |

Provisioning files under `observability/grafana/provisioning/` and `observability/grafana/dashboards/` are bind-mounted read-only, so dashboards and datasource definitions always come from disk and are not in the volume. The volume holds only state that Grafana writes itself.

Volume rename is a copy (Docker has no native rename). Plan for one pass through each store size (24 GB testnet canonical, ~250 GB mainnet canonical at the time of writing; observability volumes are tens of MB).

```bash
# Stop the running stack so nothing is holding the volumes open.
docker compose -p zinder-<network> down

# Choose the destination network suffix once.
net=testnet   # or mainnet, regtest

for src in zinder-data zinder-prometheus-data zinder-grafana-data; do
  case $src in
    zinder-data)            dst=zinder-${net}-data ;;
    zinder-prometheus-data) dst=zinder-${net}-prometheus ;;
    zinder-grafana-data)    dst=zinder-${net}-grafana ;;
  esac

  docker volume create "$dst"
  docker run --rm -v "$src":/from:ro -v "$dst":/to alpine \
    sh -c 'cp -a /from/. /to/'

  # Sanity check; sizes should match within a few KB.
  docker run --rm -v "$src":/v alpine du -sh /v
  docker run --rm -v "$dst":/v alpine du -sh /v
done

# Remove the legacy volumes once each rename is verified.
docker volume rm zinder-data zinder-prometheus-data zinder-grafana-data
```

Bring the stack back up with the matching env file:

```bash
docker compose --env-file deploy/.env.${net} -f deploy/docker-compose.yml up -d
```

Operators who don't care about preserving the small observability volumes can skip them in the loop — `docker volume rm` followed by `up` lets the new `zinder-<network>-prometheus` and `zinder-<network>-grafana` start empty. Only the canonical `zinder-data` is expensive to lose.

## One-shot pre-seed

For diagnostic work (snapshot import, regtest experiments, deliberate bounded ingest), `--target-height` makes the loop exit `0` after reaching the named height instead of transitioning to tip-follow:

```bash
zinder-ingest --target-height 4000000 --config /etc/zinder/ingest.toml
```

Combine with `--checkpoint-height H` to bootstrap an empty store from an upstream-supplied checkpoint at height H, then ingest the post-checkpoint range:

```bash
zinder-ingest \
  --checkpoint-height 3999999 \
  --target-height 4000000 \
  --config /etc/zinder/ingest.toml
```

`--allow-near-tip-finalize` is a disposable-store override: it lets the bulk phase finalize blocks inside the reorg window. Invalid with `--wallet-serving`. Use it only for regtest or local-only stores where the operator accepts that future reorgs may require recreating the store.

`zinder-ingest probe` (no long-running loop) prints `{store_tip, upstream_tip, gap_blocks, phase_that_would_run, upstream_health}` and exits. Useful for one-off operator checks without spawning the full loop.

## References

- [ADR-0015: Unified Phase-Driven Ingest](../adrs/0015-unified-phase-driven-ingest.md)
- [ADR-0003: Canonical Storage Access Boundary](../adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0013: Source Failure Recovery Topology](../adrs/0013-source-failure-recovery-topology.md)
- [Chain ingestion](../architecture/chain-ingestion.md)
- [ADR-0016: Source streaming pipeline](../adrs/0016-source-streaming-pipeline.md)
- [Deploying on a VM](deploying-on-a-vm.md)
- [Deploying on Railway](deploying-on-railway.md)
- [Bulk-catchup OOM recovery](bulk-catchup-oom-recovery.md)
- [Zebra health endpoints](https://zebra.zfnd.org/user/health.html)
