# Wallet Lifecycle Test

This setup is the acceptance boundary for a clean version-1 Wallet deployment.
It certifies both retained topologies against an already-synchronized Zebra:
`rocksdb-single-host` and `postgres-scale-out`. The harness starts with fresh,
project-scoped Zinder storage, but preserves and reuses Zebra's chain state.

The harness is intentionally blocked until the image contains
`/usr/local/bin/zinder-wallet-lifecycle` contract version 1. A benchmark driver,
a `BUILDING` canonical store, a healthy process, or a report missing any
required phase cannot produce a passing result.

## Isolation contract

The Compose file joins `z3-testnet` and mounts `z3-testnet-cookie` read-only. It
does not declare or mount `z3-testnet-chain`. The runner inspects the rendered
Compose model before cleanup and refuses to continue if the configured Zebra
chain volume appears in any service. Cleanup is also restricted to project
names beginning with `zinder-wallet-lifecycle-`.

RocksDB uses separate disposable `canonical` and `wallet` volumes. PostgreSQL
uses one disposable database volume and separate `canonical` and `wallet`
schemas. PostgreSQL is restarted between initial certification and restart
certification; the RocksDB restart check always runs in a fresh container with
both stores mounted read-only.

## Required evidence

The certifier must write one certification report and one restart report per
topology. [`validate-wallet-lifecycle-report.sh`](../scripts/validate-wallet-lifecycle-report.sh)
rejects a report unless it proves all of the following:

- canonical and wallet identities use schema version 1;
- canonical construction is pinned to one Zebra tip and performs zero
  historical prevout reads;
- the final commitment-tree checkpoint matches Zebra, checkpoint gaps never
  exceed 100 blocks, and every active pool has a complete subtree-root range;
- one three-operation atomic publication updates READY and creates epoch 1 and
  chain-event sequence 1;
- wallet derivation covers that exact epoch and tip;
- a fresh reader passes latest-block, compact-block, transaction, tree-state,
  and subtree-root probes through the public `WalletQuery` surface; and
- a new process reopens the same READY canonical and wallet state after restart.

Every phase records elapsed seconds and its domain counts, including canonical
blocks and bytes, commitment-tree checkpoints, active pools, subtree roots,
publication records, wallet transactions, and query requests. The report also
binds the fixed tip, ordered sequence digest, software revision, and image
reference. The validator accepts no partial or degraded result.

## Run

Build an immutable lifecycle image from the revision under test, then run one or
both topologies:

```bash
export ZINDER_WALLET_LIFECYCLE_IMAGE='zinder-wallet-lifecycle@sha256:<digest>'
scripts/run-wallet-lifecycle-test.sh rocksdb
scripts/run-wallet-lifecycle-test.sh postgres
# Or run both from one clean harness project:
scripts/run-wallet-lifecycle-test.sh all
```

The default source is the existing local testnet Zebra at `http://zebra:18232`.
Override `ZINDER_SOURCE_NETWORK_NAME`, `ZINDER_SOURCE_COOKIE_VOLUME_NAME`, and
`ZINDER_NODE__JSON_RPC_ADDR` together when targeting another read-only testnet
Zebra topology. The current report contract is deliberately testnet-only.
`ZINDER_WALLET_LIFECYCLE_START_HEIGHT` defaults to zero so the certification
covers a complete fresh sync.

Reports are written to `.tmp/wallet-lifecycle-evidence` by default. Successful
reports are evidence for the exact image, source tip, and topology tested; they
do not certify another revision, network, or deployment shape.

To delete only the harness state after retaining any needed reports:

```bash
docker compose \
  --project-name zinder-wallet-lifecycle-test \
  --file deploy/docker-compose.wallet-lifecycle-test.yml \
  --profile rocksdb --profile postgres \
  down --volumes --remove-orphans
```

## Current implementation blockers

The current repository cannot yet create a valid report. Canonical construction
now loads exact subtree ranges and authenticates its fixed-tip frontier, but the
missing production boundaries are fresh-reader validation and atomic READY
publication, the wallet version-1 materializer and query cutover, concrete
PostgreSQL canonical and wallet stores, and the certifier binary that composes
those paths. The existing PostgreSQL code is a diagnostic round-trip driver,
not the `postgres-scale-out` lifecycle. These are hard gates: the runner checks
for the certifier contract before deleting project state, and the report
validator independently rejects any omitted phase.
