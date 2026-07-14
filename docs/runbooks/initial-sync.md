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

## Background projection work

Canonical readiness does not imply that every historical projection is complete. Use this table as the operator checklist; each worker persists progress and resumes after restart.

Initial indexing has strict priority: canonical owns `BulkCatchup`, derive replay
owns the post-canonical drain, and historical enrichment or verification starts
only when canonical is `FollowingTip` and derive covers its visible tip. The
workers resume automatically when `zinder_ingest_historical_work_gate_open` is
`1`; no operator sequencing is needed. An in-flight historical batch may finish
after the gate closes, but the worker cannot start another batch until both
canonical and derive are current again.

| Worker | Canonical boundary | Readiness effect | Capability and completion evidence | Restart behavior |
| --- | --- | --- | --- | --- |
| Commitment-root enrichment | Settled blocks from Sapling activation; live commits supply the tip | Does not gate canonical readiness | Root-search capability plus contiguous root coverage | Resumes bounded batches and revalidates block identity |
| Transaction-component history | Height 1 through the live projection tip | Does not gate canonical readiness; coordinated derive schema upgrade | Component-summary capability plus historical/live coverage | Rebuilds only its consumer, then resumes from durable coverage |
| Conventional-fee distribution | Configured history window joined to the live tail | Does not gate canonical readiness | Capability appears after materialized coverage; response reports requested-range completeness | Prepends newest-first from durable coverage |
| Paid-fee distribution | Configured history window; only transactions with provable inputs contribute fees | Does not gate canonical readiness | Paid-fee capability, coverage, and unavailable count | Prepends newest-first; unresolved facts remain explicit |
| Value-pool flow history | Configured settled history joined to the live tail | Does not gate canonical readiness | Flow capabilities plus consumer checkpoint and coverage | Resumes durable historical batches under the source-request budget |
| Value-pool balance history | Height 1 through the fenced projection tip; daily rows select canonical candidates | Does not gate canonical readiness | Balance-history capability plus contiguous scanned-height coverage | Resumes scanning and rejects candidates whose block identity changed |
| Transparent-address ranking | Matching settled output snapshot and lifetime deltas, then visible unsettled tail | Ranking capability withheld until activation | Ranking capability and active-generation coverage | Leaves the active generation untouched and resumes an inactive build |
| Transaction-history verification | Height 1 through the fenced history tip | Does not gate canonical readiness | History v2 capability requires verified contiguous coverage and matching tip hash | Resumes verification; v1 remains available for bounded partial history |

## Commitment-root enrichment

Artifact schema 14 adds typed post-block Sapling, Orchard, and Ironwood final
note-commitment roots without recreating the canonical store. Tip-follow writes
roots for newly observed blocks. Bulk catchup reuses its existing sparse
tree-state checkpoints rather than adding an RPC per block. One independent
background task fills the remaining settled history from Sapling activation
forward and updates the `CommitmentRootSearchConsumer` projection in bounded,
resumable batches.

The backfill does not gate canonical readiness or wallet serving. Until its
coverage reaches the settled tip, `ExplorerQuery.CommitmentRootSearch` returns
matches from the materialized range and marks negative results as incomplete.
On an existing derive store, startup seeds the new root-search consumer's
event cursor only when every pre-existing block consumer reports the same
cursor. This lets current events continue without replaying unrelated
consumers from genesis; historical root-search coverage still starts empty and
belongs exclusively to the backfill. A fresh, partially rebuilt, or
cursor-disagreeing store keeps the normal fail-closed replay behavior.
Operators should monitor structured events rather than infer completion from
ingest readiness:

```text
event=commitment_root_backfill_started from_height=...
event=commitment_root_backfill_progress from_height=... through_height=... fetched_roots=...
event=commitment_root_backfill_completed through_height=...
event=commitment_root_backfill_retry error=... retry_delay_seconds=5
event=commitment_root_search_cursor_seeded
```

The default bounds are explicit in resolved config and can be tuned without a
schema change:

```toml
[ingest.commitment_root_backfill]
enabled = true
batch_blocks = 256
fetch_concurrency = 8
```

Disabling the task stops historical progress but does not remove tip-follow
root collection. Re-enabling resumes from the durable contiguous coverage row;
it does not replay or advance the shared derive chain-event cursor.

## Transaction-component history

The transaction-component consumer is an additive derive schema; it does not
change the canonical artifact schema or require a data-volume replacement.
Its version-2 fixed-width rows are incompatible with version 1, so opening an
existing store clears this consumer's rows and cursor only. Ingest then takes a
unanimous cursor from the pre-existing block consumers and performs two bounded
operations before its event tailer starts:

This upgrade is a coordinated outage. Stop `zinder-ingest` and every process
holding the derive store as a secondary, take a checkpoint, deploy version-2
binaries, start the version-2 writer first, and start version-2 readers only
after primary reconciliation completes. Reader-first rolling and side-by-side
version-1/version-2 access are invalid.

1. Historical backfill reads canonical block and transaction artifacts from
   height 1 through the height immediately before the durable live-tail
   boundary.
2. Startup seeding writes the already-visible unsettled range into the live
   tail without advancing the inherited cursor. Normal reorg events own those
   rows after startup.

The two ranges must touch before `TransactionComponentSummary` reports
complete coverage. A restart can widen and revalidate a previously seeded tail
without deleting its contribution rows. The existing canonical and unrelated
derive families remain untouched.

```text
event=transaction_component_tail_boundary_initialized cursor_seeded=... tail_boundary=...
event=transaction_component_backfill_started from_height=... batch_blocks=...
event=transaction_component_backfill_progress from_height=... through_height=... transaction_count=...
event=transaction_component_backfill_completed through_height=...
event=transaction_component_backfill_retry error=... retry_delay_seconds=5
```

```toml
[ingest.transaction_component_backfill]
enabled = true
batch_blocks = 256
```

This worker does not gate canonical readiness. It remains deferred while
canonical bulk catchup or readiness-critical derive replay owns the storage
budget, then advances after `zinder_ingest_historical_work_gate_open` becomes
`1`. Consumers must inspect the RPC coverage envelope until historical and
live-tail ranges are contiguous.

## Conventional-fee distribution history

The conventional-fee distribution is another additive derive consumer. An
existing volume upgrades in place: startup registers four new derive column
families, seeds the visible live tail at the unanimous existing block-consumer
cursor, and starts a cursor-neutral historical backfill from height 1. It does
not change canonical artifact schema, replace the volume, clear unrelated
derive rows, or require the chain to be reingested.

```text
event=conventional_fee_distribution_tail_boundary_initialized cursor_seeded=... tail_boundary=...
event=conventional_fee_distribution_backfill_started from_height=1 batch_blocks=...
event=conventional_fee_distribution_backfill_progress from_height=... through_height=... transaction_count=...
event=conventional_fee_distribution_backfill_completed through_height=...
event=conventional_fee_distribution_backfill_retry error=... retry_delay_seconds=5
```

```toml
[ingest.conventional_fee_distribution_backfill]
enabled = true
batch_blocks = 256
```

The worker does not gate canonical readiness. The explorer omits
`explorer.fee.conventional_distribution_v1` until coverage exists, and clients
must keep honoring `requested_range_complete` while historical coverage is
still joining the live tail. Before deploying the writer, take a normal Zinder
checkpoint backup; rollback restores that checkpoint rather than deleting the
new column families from a running RocksDB instance.

Deploy every binary that opens the derive store from the same release before
starting the upgraded writer. This includes `zinder-query`, even when it does
not serve the new explorer method: secondary readers validate the complete
bundled-consumer manifest and fail closed on an undeclared consumer. A safe
rolling sequence is therefore: build all services, stop derive-store readers,
replace the readers and writer, start ingest, then start readers after manifest
reconciliation completes. Mixing an older reader with a writer that has
registered `conventional_fee_distribution` makes that reader unavailable; it
is not a signal to wipe or recreate the volume.

## Actual paid-fee history

Artifact schema 15 adds a separate canonical
`TransactionIntrinsicValueBalances` family. Each row retains signed Sprout,
Sapling, Orchard, and Ironwood balances parsed from the transaction bytes. The
`PaidFeeDistributionConsumer` combines those values with resolved transparent
prevouts and outputs, excludes coinbase and proven zero-fee transactions, and
stores exact positive paid-fee frequencies. Missing source facts increase an
explicit unavailable count; ZIP-317 conventional fees are never substituted.

Existing volumes upgrade in place. Startup seeds the visible paid-fee tail at
the unanimous event cursor, then the background task prepends settled history
newest-first. This makes recent periods usable before the full 365-day window
finishes and allows a later `history_days` increase to move the durable floor
backward without clearing existing rows. Pre-upgrade tail blocks are enriched
canonically as they settle.

```toml
[ingest.paid_fee_distribution_backfill]
enabled = true
batch_blocks = 256
fetch_concurrency = 8
history_days = 365
timestamp_safety_seconds = 7200
```

Monitor explicit progress and coverage rather than ingest readiness:

```text
event=paid_fee_distribution_tail_boundary_initialized
event=paid_fee_distribution_backfill_started direction=newest_first
event=paid_fee_distribution_backfill_progress direction=newest_first
event=paid_fee_distribution_settled_tail_reconciled
event=paid_fee_distribution_backfill_completed
event=paid_fee_distribution_backfill_retry
```

Consumers may prefer `explorer.fee.paid_distribution_v1` as soon as it is
advertised. A native response is complete only when the requested time range is
covered and its unavailable-transaction count is zero; otherwise it preserves
the exact partial rows and explains the gap. Conventional-fee distribution is
a separate native projection and must never be labeled as paid-fee history.

## Transparent-address ranking bootstrap

The transparent-address ranking is an additive derive projection. Schema 2
adds P2PKH/P2SH aggregate counters to the same generation metadata and rebuilds
only this consumer when upgrading from schema 1. On the first
startup that introduces it, ingest catches existing consumers up to the
canonical tail, then builds an inactive generation from two matching settled
sources: the canonical address-output snapshot and complete lifetime address
deltas. It reconciles every address before writing the generation, applies the
visible unsettled tail with undo journals, and atomically activates the result
at the existing unanimous event cursor.

This bootstrap can take tens of seconds on testnet, but it does not change the
canonical artifact schema or wipe the volume. Interruption leaves the active
generation untouched and resumes only the inactive build. If lifetime source
coverage is incomplete, startup continues with the ranking capability omitted;
it never exposes partial lifetime values as complete.

```text
event=transparent_address_ranking_activated generation=... through_height=... positive_address_count=... total_positive_balance_zat=...
```

Routine restarts on the same derive schema do not rebuild an active generation. Operators should verify
that `ExplorerQuery.ServerInfo` advertises
`explorer.transparent_address.ranking_v1`, then call
`ExplorerQuery.TransparentAddressRanking` and confirm that its coverage reaches
the response's visible tip. A canonical volume replacement is neither required
nor expected.

`ExplorerQuery.TransparentAddressActivity` v2 reuses this active generation for
its per-address confirmed summary and reuses the existing activity projection
plus retained canonical transaction facts for rows. Enabling v2 does not add a
consumer schema, rebuild the ranking, replay chain events, or backfill canonical
data. Operators should verify
`explorer.transparent_address.activity_v2`; a missing capability means the
ranking has no active complete generation or the explorer lacks the required
read stores, not that the canonical volume should be wiped.

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

The default `reorg_window_blocks` is `100`. Upstream Zebra (v5 onwards) follows and serves reorgs up to `MAX_BLOCK_REORG_HEIGHT = 1000`, so a rollback deeper than the configured window lands below the settled tip and trips this fail-closed path even though Zebra itself recovered cleanly. The fail-closed posture means a deep reorg degrades to a re-sync, never to silent corruption. No reorg approaching 100 blocks has been observed on Zcash mainnet, so the default covers all observed history; operators who run against a node expecting deeper rollbacks can raise `reorg_window_blocks` (`ZINDER_INGEST__REORG_WINDOW_BLOCKS`), trading near-tip retention memory, which scales with the window, for the wider recovery range.

```bash
docker compose --env-file deploy/.env.mainnet -f deploy/docker-compose.yml down
docker volume rm zinder-mainnet-data
docker compose --env-file deploy/.env.mainnet -f deploy/docker-compose.yml up -d
```

(For testnet substitute `.env.testnet` + `zinder-testnet-data`; for regtest substitute `.env.regtest` + `zinder-regtest-data`.)

The fresh start runs `BulkCatchup` from the wallet-serving floor (or genesis, depending on `ingest.coverage`) and transitions to `TipFollow` when it catches up. No manual sequencing is needed.

## Schema v11: in-place rebuild at first open

Schema version 11 (the address-output current projection) migrates a
version-10 store in place; no wipe or resync is needed. On the first
`zinder-ingest` start with the v11 binary, the writer rebuilds the
`address_output_index` column family from `transparent_output` and
`transparent_spend_fact` before serving, emitting
`address_output_projection_rebuild_started` and
`address_output_projection_rebuild_completed` log events with row counts and
duration. Expect minutes, not hours; the rebuild streams the two source
families once and reclaims the old family's disk space immediately.

The rebuild is crash-safe: the store metadata version flips to 11 only after
the projection is complete, so a kill or crash mid-rebuild simply re-runs it
on the next start. Deploy order matters: start the v11 writer first and let
the rebuild finish, then restart every reader (`zinder-query`,
`zinder-explorer`, `zinder-compat-lightwalletd`). A reader running against a
not-yet-migrated store fails closed with `cause=schema_mismatch`, and a
secondary opened before the migration cannot replay the column-family drop,
so the restart is required, matching the rolling-upgrade order in
[ADR-0003](../adrs/0003-canonical-storage-access-boundary.md).

## Schema v12: rebuild from genesis (Ironwood/NU6.3)

Schema version 12 adds the Ironwood shielded pool to `tip_metadata` and to
each compact block's payload. Unlike v11, it is not repairable in place: the
Ironwood action data a v11 store is missing was never derived from the source
block. A v11 (or v10) store is rejected at open with `SchemaTooOld`, and the
operator must wipe the volume and resync from the wallet-serving floor (or
genesis), exactly as in the fresh-start procedure above. Deploy the v12 binary
only after wiping the store; a redeploy over an un-wiped v11 store crash-loops
on `open_storage` until the volume is cleared.

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
