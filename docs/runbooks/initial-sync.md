# Initial sync

This runbook covers the supported single-host wallet-serving topology. The
canonical writer constructs and publishes canonical RocksDB, the projector
builds wallet RocksDB from an authenticated canonical fence, and the
lightwalletd adapter begins serving only after both secondary readers form an
exact pair.

There is no in-place migration from an incompatible store identity or schema.
Use empty target paths or a certified coherent restore bundle.

## Prerequisites

- A Zebra node on the selected network with required JSON-RPC capabilities.
- Enough local storage for canonical data, wallet state, staging, compaction,
  checkpoints, and growth reserve.
- Stable canonical and wallet primary paths on one host filesystem.
- Unique secondary metadata roots for projector and compatibility.
- Stable projector build-owner identity and private control bearer tokens.
- Resource limits that leave headroom outside RocksDB caches and memtables.

For Z3-managed nodes, start and synchronize the selected Z3 network before
starting Zinder.

## Start the stack

```bash
docker compose \
  --env-file deploy/.env.testnet \
  -f deploy/docker-compose.yml \
  up -d --build
```

The dependency order is ingest, projector, then compatibility, but container
start does not imply traffic readiness. Follow all three operational endpoints:

```bash
curl -sS http://127.0.0.1:19105/readyz  # ingest
curl -sS http://127.0.0.1:19110/readyz  # projector
curl -sS http://127.0.0.1:19107/readyz  # compatibility
```

## Canonical construction

On an empty canonical path, ingest creates a sibling construction staging
directory, fixes a source range, loads and validates it, publishes a baseline
fence, installs it at the configured path, and starts continuous following.
The configured path does not become a ready store until publication succeeds.

The checked ingest configuration uses `coverage = "wallet-serving"`. The
writer derives the earliest supported wallet height from node-advertised
network activations and uses its predecessor as the construction checkpoint.
This avoids storing history that the supported lightwalletd wallet workload
cannot consume. An explicit checkpoint must carry the required predecessor
tree state and source identity.

Useful canonical metrics:

- `zinder_ingest_source_fetch_queue_requests`
- `zinder_ingest_source_fetch_queue_bytes`
- `zinder_ingest_canonical_tip_height`
- `zinder_ingest_canonical_lag_blocks`
- `zinder_ingest_canonical_historical_prevout_reads_total`
- `zinder_ingest_canonical_cross_block_wallet_reads_total`

The last two counters must remain zero.

If the process stops during construction, restart it with the same
configuration. A published staging store is installed and reopened. An
unpublished staging store is removed and reconstructed. Do not manually move
or edit the staging directory while the owner is running.

## Wallet construction

Projector opens canonical storage through its own secondary, converges on the
writer fence, and binds a wallet build plan to that source identity. It acquires
a wallet projection build lease and a writer-owned canonical retention lease,
then builds the wallet store in its owned path.

Wallet publication requires the complete build digest and source position to
match the admitted canonical fence. The projector then catches up and takes
continuous following ownership before releasing construction leases.

Restarting projector is safe. Lease generation, build state, source identity,
and ready evidence are persisted. A second projector using the same wallet path
must fail lease or writer admission rather than run concurrently.

## Exact-pair serving

Compatibility maintains generation-specific canonical and wallet secondaries.
It catches both up, validates source identity and event position, and publishes
one immutable `WalletServingReadPair`. Only then does `/readyz` report ready and
the gRPC readiness interceptor admit traffic.

Probe a data-bearing method after readiness:

```bash
grpcurl -plaintext -d '{}' 127.0.0.1:19067 \
  cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
```

An open canonical secondary plus an open wallet secondary is not sufficient.
The pair must agree on network, reorg policy, canonical source identity, event
fence, and wallet digest.

## Expected readiness states

| Runtime | Normal initial state | Ready boundary |
| --- | --- | --- |
| ingest | `starting` or `syncing` | Canonical lag is within threshold and mempool snapshot is complete |
| projector | `starting` or `syncing` | Wallet store is published and following the admitted canonical source |
| compatibility | `starting`, `replica_lagging`, or `writer_status_unavailable` | One exact canonical and wallet pair is published |

`node_unavailable` and `upstream_not_ready` are recoverable source conditions.
`schema_mismatch`, `reorg_window_exceeded`, corrupt store errors, and source
identity mismatches require operator action.

## Memory and storage pressure

Watch cgroup memory, RocksDB block cache, memtables, WAL bytes, pending
compaction, write-stop state, construction queue reservations, and disk free
space. Store size must not determine process RSS.

If construction approaches its memory boundary, stop the owner cleanly and
reduce the source-request, reserved-response, preparation, or RocksDB budgets
before restarting. Do not delete an admitted ready store merely to change a
runtime resource limit. See
[Bulk-catchup resource tuning](bulk-catchup-resource-tuning.md).

## Recovery and restart order

For ordinary process restarts, start owners before readers:

1. `zinder-ingest`
2. `zinder-projector`
3. `zinder-compat-lightwalletd`

Readers remain unready until their owner and secondary contracts recover. Do
not replace this with manual directory copies or primary-read shortcuts.

For disaster recovery, restore a coherent canonical and wallet state bundle
into fresh paths and require cold owner admission plus normal exact-pair
admission. Independently timed RocksDB copies are not a wallet-serving backup.

## Production acceptance

A successful initial sync proves only the observed construction and admission
run. Production acceptance also needs current evidence for sustained following,
bounded reorg replacement, coherent restore, capacity headroom, restart
behavior, mempool recovery, TLS routing, and the exact independent client and
network being advertised.

See [Testing](testing.md), [Service operations](../architecture/service-operations.md),
and [ADR-0035](../adrs/0035-canonical-storage-topologies.md).
