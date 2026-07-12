# Native Indexer Projections Merge Plan

Status: Ready for review
Base: `main` at `03d3a76`
Branch: `feat/native-indexer-projections`

## Scope

This branch reconstructs reusable Zinder indexing work on top of current `main`. It contains native facts, projections, APIs, operations, tests, and documentation only. It intentionally excludes product-specific adapters, REST contracts, serializers, adapter deployment wiring, external pricing transport, and compatibility plans.

Existing native surfaces remain available, including `RecentTransactions`, Ironwood migration RPCs, `NetworkUpgradeStatus`, `BlockTransactions`, mempool snapshots, reorg history, source fallback, bounded catchup, RocksDB instrumentation, and service-exit propagation.

## Coverage

| Slice | Canonical facts | Derive projection | Native API | Operational lifecycle |
| --- | --- | --- | --- | --- |
| Commitment roots | Schema 14 final Sapling, Orchard, and Ironwood roots | Root reverse index with coverage | Commitment-root search; roots on block transactions | Resumable settled-history enrichment |
| Intrinsic balances and paid fees | Schema 15 transaction-intrinsic balances | Paid-fee frequencies | Intrinsic balances on transaction detail/history; paid-fee distribution | Newest-first bounded backfill with unavailable counts |
| Value-pool history | Schema 16 per-block balances | Daily balances and transaction flow history | Balance history, flow history, and aggregate summaries | Separate resumable coverage for balances and flows |
| Transaction analytics | Existing transaction facts | Generic history, component summary, conventional-fee distribution | Filtered history with read fences; component and fee summaries | Split live tail and historical verification/backfill |
| Transparent analytics | Existing output, spend, and address facts | Ranking generation and coherent activity | Address ranking and activity v2 | Snapshot activation plus incremental tail |
| Displaced blocks | Schema 17 writer-owned archive and root index | None | Displaced history/detail and displaced-root search | Atomic capture from activation; permanent retention |

## Merge Rules

1. Merge by vertical slice or as one coordinated schema release; do not cherry-pick shared store/proto files independently.
2. Preserve the artifact-schema ladder: 13 is Orchard/Ironwood transaction facts, 14 roots, 15 intrinsic balances, 16 block value pools, and 17 displaced archive roots/indexes.
3. Deploy every canonical/derive reader and writer from the same release. Stop readers, checkpoint canonical plus derive storage, start ingest first, then start readers.
4. Treat per-projection checkpoint and coverage as the completeness authority. Canonical readiness and the block-summary tip are not substitutes.
5. Keep `recent_transactions` and `transaction_history` as distinct consumers and column families. Removing an established consumer requires a separate storage migration.

## Review Sequence

1. Canonical schemas, enrichment identity checks, reorg behavior, and archive retention.
2. Projection state, read snapshots, cursor fences, coverage joins, and source-request budgets.
3. Proto methods, capability gates, and preservation of existing public methods.
4. Backfill restart/cancellation behavior and deployment configuration.
5. Documentation consistency and the [Zinder information catalog](../reference/zinder-value-by-use-case.md).

## Validation Baseline

The reconstructed worktree passes formatting, workspace check, strict clippy, the 1,421-test CI profile, rustdoc warnings, `cargo deny`, `cargo machete`, runbook lint, capability-document drift checks, and a native-only vocabulary scan. Live regtest/testnet validation remains the acceptance boundary after review and merge preparation.
