# Cipherscan Adapter Verification

Use this runbook to verify the existing Cipherscan application against
`zinder-compat-cipherscan`. It records acceptance boundaries, not a particular
environment run.

## Preconditions

- Zinder ingest, wallet query, explorer query, and the Cipherscan adapter are
  healthy on the same network.
- The block-production time projection reports complete coverage through its
  projection tip.
- Cipherscan uses the adapter as its API base URL without source changes.
- The preserved Zinder store has been migrated and backfilled in place.

## Pool Distribution

Probe `/api/mining/pool-distribution` with no query, an empty `period`, each of
`24h`, `3d`, `7d`, `30d`, `90d`, `6m`, `1y`, and `all`, and one unknown token.
For every response, verify:

1. The status is `200` and the object has exactly `generatedAt`, `period`,
   `pools`, and `totalBlocks`.
2. Every pool has exactly `address`, `blocks`, `name`, `share`, and
   `totalFeesZat`, with `totalFeesZat` encoded as a decimal string.
3. The default and empty period echo `7d`. A known period echoes itself. An
   unknown period echoes the supplied token while using the seven-day window.
4. `totalBlocks` equals the sum of pool block counts, shares are finite and in
   `[0, 1]`, and each fee total is the exact sum of retained paid-fee facts.
5. Two requests with the same effective raw period string within five minutes
   return the same `generatedAt`. Distinct raw strings use distinct cache keys.
6. A multi-page request remains valid while the chain extends. A reorg that
   displaces the frozen projection tip invalidates the continuation.

Compare each response with the public testnet Cipherscan API by structure,
types, period behavior, grouping semantics, and cache behavior. Moving windows
may differ by blocks added between requests or by cache age; do not require
identical totals. Historical source anomalies are not a compatibility target.

Open the unchanged local `/mining` page and exercise each period control it
renders. The distribution must leave its loading state and show the returned
total and pool groups. Failures in independently owned mining ranking,
hashrate-share, miner-behavior, or ZODL panels do not invalidate this route.

## Focused Regression

After projection or adapter changes, smoke-test the actively rendered workflows
that already have complete coverage:

- home and chain status;
- blocks and one block detail;
- transactions and one transaction detail;
- mempool;
- one transparent address and the rich list;
- network and supply;
- value pools;
- privacy;
- migration;
- reorg history; and
- mining production.

For each workflow, require a successful primary request, rendered non-placeholder
content, and no new application error caused by the change. Degraded panels
listed as sidecar-owned or unavailable in the
[coverage matrix](../plans/cipherscan-adapter-architecture.md) remain outside
this regression boundary.
