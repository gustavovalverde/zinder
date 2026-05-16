# Explorer Plane

The explorer plane is the Zinder product surface for block explorers, dashboards, and analytics consumers. It serves UI-ready, API-ready, and agent-ready views over canonical chain artifacts and replayable event streams, with explicit freshness, typed unavailability, and capability-gated panels. It is owned by `zinder-explorer`.

This document defines the boundary, wire vocabulary, capability namespace, freshness contract, and the rule that distinguishes explorer views from wallet views. It is the sibling document to [Wallet data plane](wallet-data-plane.md) and [Derive plane](derive-plane.md). The explorer plane exercises the derive-plane SDK; the derive plane defines that SDK.

## Purpose

Wallets and explorers ask different questions. A wallet asks "did anything I care about happen?" and follows the chain by height. An explorer asks "what happened in this block, what is the status of this transaction, what is happening in the mempool right now, is this indexer fresh enough to trust." Those questions need typed responses with freshness metadata, capability strings, and stable pagination. They do not need raw compact blocks.

`ExplorerQuery` is the read surface for those questions. It owns block summaries, transaction details, typed search, mempool summaries, transparent address activity, fee summaries, value-pool summaries, and explorer freshness. `WalletQuery` keeps the wallet-correctness primitives.

The decisions that govern this plane:

- [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) — service topology, capability namespace, dual-capability federation rule.
- [ADR-0010](../adrs/0010-transaction-public-facts.md) — single transaction parser feeding ingest, mempool, and explorer consumers.
- [ADR-0011](../adrs/0011-explorer-freshness-envelope.md) — freshness envelope embedded on every explorer response.
- [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md) — typed search and structured privacy refusal.

## Boundary

`zinder-explorer` is a fourth deployable alongside `zinder-ingest`, `zinder-query`, and `zinder-compat-lightwalletd`. It:

- **Consumes** canonical artifacts (via `WalletQuery` over gRPC) and replayable events (`WalletQuery.ChainEvents`, `WalletQuery.MempoolEvents`).
- **Owns** its own RocksDB at `explorer.storage_path`, distinct from canonical storage and from any other consumer's store.
- **Produces** the `ExplorerQuery` gRPC service plus federated additions to `WalletQuery` (currently `TransparentAddressBalance`'s mempool overlay).
- **Does not** open the canonical RocksDB primary or secondary; does not call upstream Zcash node RPCs; does not custody any wallet secret.

The boundary rules:

- A `zinder-explorer` crash does not stop ingest or wallet sync. `WalletQuery.TransparentAddressBalance` falls back to the canonical-confirmed compute path.
- The explorer plane never extends canonical artifact schemas. When a view needs a fact the canonical surface does not carry, the source boundary extends first, the canonical artifact or event gains the field, the explorer subscribes.
- Server-side shielded address scanning, persisted viewing keys, and memo decryption are out of scope by product invariant.

## Wire surface

The native gRPC service is `ExplorerQuery` in `zinder.v1.explorer`. The 2026-05 surface (post the explorer-plane initial slice) is:

```proto
service ExplorerQuery {
  rpc ServerInfo(ServerInfoRequest) returns (ServerInfoResponse);

  rpc TransactionDetail(TransactionDetailRequest)
      returns (TransactionDetailResponse);

  rpc TransparentAddressBalance(TransparentAddressBalanceRequest)
      returns (TransparentAddressBalanceResponse);
}
```

Additional methods land incrementally per [the slicing plan](#slicing-plan-deferred): `BlockSummariesInRange`, `BlockDetail`, `TransparentAddressActivity`, `MempoolSummary`, `MempoolActivity`, `FeeSummary`, `ValuePoolSummary`, `Search`. Every method follows the same shape rules:

- Response message field tag 1 is `ExplorerFreshness freshness` ([ADR-0011](../adrs/0011-explorer-freshness-envelope.md)).
- Streaming responses are chunked; each chunk carries its own `ExplorerFreshness` and an opaque `cursor: bytes`.
- Paginated requests accept `from_cursor: bytes` plus `max_entries: uint32`.
- Fields use unit suffixes per [Public interfaces §Method Naming Conventions](public-interfaces.md#method-naming-conventions): `_zat`, `_zec`, `_height`, `_count`, `_bytes`, `_millis`, `_seconds`.

## Capability namespace

The explorer plane uses the `explorer.*` capability prefix. The full namespace structure:

| Capability | Owner method | Always-on? |
| ---------- | ------------ | ---------- |
| `explorer.server_info_v1` | `ExplorerQuery.ServerInfo` | Yes |
| `explorer.transaction.detail_v1` | `ExplorerQuery.TransactionDetail` | When the wallet endpoint is configured |
| `explorer.transparent_address.balance_v1` | `ExplorerQuery.TransparentAddressBalance` (and federated `WalletQuery.TransparentAddressBalance`) | When the wallet endpoint is configured |
| `explorer.block.summary_v1` | `ExplorerQuery.BlockSummariesInRange` + `BlockDetail` summary part | When the block-summary consumer is built and caught up |
| `explorer.block.detail_v1` | `ExplorerQuery.BlockDetail` per-tx rows | When the block-detail consumer is built and caught up |
| `explorer.transparent_address.activity_v1` | `ExplorerQuery.TransparentAddressActivity` | Yes once shipped |
| `explorer.mempool.summary_v1` | `ExplorerQuery.MempoolSummary` | When the mempool-summary consumer is built |
| `explorer.mempool.activity_v1` | `ExplorerQuery.MempoolActivity` | Yes once shipped |
| `explorer.fee.summary_v1` | `ExplorerQuery.FeeSummary` | Yes once shipped |
| `explorer.value_pool.summary_v1` | `ExplorerQuery.ValuePoolSummary` | When the source boundary supports chain value pools |
| `explorer.search.v1` | `ExplorerQuery.Search` | Yes once shipped |

The naming follows `explorer.<noun>.<capability>_v{N}`. The noun is a domain category; the capability is the operation. New methods add new capability strings; wire-shape changes ship as `_vN` increments.

Two capabilities cross planes (dual-capability federation rule per [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md)):

- `wallet.address.transparent_balance_v1` — always-on canonical-confirmed-balance path.
- `explorer.transparent_address.balance_v1` — same RPC carries the live-mempool overlay when the explorer proxy is ready.

Future federated methods follow the same dual-capability pattern: `wallet.<surface>.<noun>_v{N}` always-on, `explorer.<surface>.<noun>_v{N}` for the richer derive-enriched shape.

## Freshness envelope

Every explorer response embeds `ExplorerFreshness` at field tag 1. The shape and rationale live in [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). The key fields:

- `chain_epoch` — wallet-plane primitive, identifies the snapshot the response was produced from.
- `snapshot_age_millis` — age of the mempool snapshot, when the response touches mempool state.
- `derive_cursor_lag_blocks` and `derive_cursor_lag_millis` — how far behind canonical the explorer derive view is.
- `capability_version` — exact capability string that produced the response.
- `unavailable` — repeated `UnavailableField` entries declaring specific field paths absent with structured reasons.

`UnavailableField` carries a `field_path` (dotted-path matching the response shape), a structured `reason` (enum), and a `human_reason` string from the canonical registry in `crates/zinder-core/src/explorer_reasons.rs`. Frontends can branch on `reason` or render `human_reason` verbatim; both come from the same source so the words match across surfaces.

## Privacy boundary

The explorer plane is a privacy surface. The non-negotiable rules:

- Search for a shielded address, viewing key, or unified-address shielded receiver returns the typed `NotPubliclyIndexable` arm per [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). The arm carries a canonical reason string. Empty match lists for shielded inputs are forbidden.
- The classifier never reaches storage for shielded inputs. A privacy regression test enforces this with a mock that records storage call counts.
- The explorer plane never receives viewing keys, spending keys, or seed phrases over any RPC. Search inputs that classify as viewing keys are echoed back only in their typed `NotPubliclyIndexable` form; the `canonical_form` field is omitted for viewing keys to avoid logging-layer leaks.
- Server-side shielded scanning is out of scope. The explorer plane does not implement, persist, or expose any shielded-address indexing.

The wallet plane's privacy invariants ([ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md)) apply identically to the explorer plane; the explorer adds the typed refusal vocabulary so a refusal is a structured response, not an error.

## Operator surface

`zinder-explorer` ships standard ops endpoints (`/healthz`, `/readyz`, `/metrics`) on a dedicated listener at `explorer.ops_listen_addr` (default `127.0.0.1:9069`). Prometheus metrics use the `zinder_explorer_*` prefix.

Configuration follows the canonical TOML conventions:

```toml
[explorer]
listen_addr = "127.0.0.1:9068"
storage_path = "/var/lib/zinder-explorer"
bearer_token_path = "/run/secrets/zinder-explorer-token"
wallet_query_endpoint = "https://zinder.example:9101"   # zinder-query gRPC
ops_listen_addr = "127.0.0.1:9069"

[explorer.freshness]
max_lag_blocks = 16              # response carries UNAVAILABLE_STALE beyond this
warn_lag_blocks = 4              # readiness cause flips at this threshold
```

When `explorer.bearer_token_path` is set, the `ExplorerQuery` gRPC endpoint enforces the same shared-secret bearer-token interceptor as `IngestControl` per [ADR-0006](../adrs/0006-ingest-control-transport-security.md). The matching `zinder-query` process points its `[explorer]` config at the same secret before advertising the federated `explorer.transparent_address.balance_v1` capability.

Environment-variable mapping uses the `ZINDER_EXPLORER__*` prefix:

- `ZINDER_EXPLORER__LISTEN_ADDR`
- `ZINDER_EXPLORER__STORAGE_PATH`
- `ZINDER_EXPLORER__BEARER_TOKEN_PATH`
- `ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT`
- `ZINDER_EXPLORER__OPS_LISTEN_ADDR`

## Failure isolation

The explorer plane fails independently from canonical state.

- An explorer service crash does not stop `zinder-ingest`. Ingest continues writing canonical artifacts and ChainEvents.
- An explorer service crash does not stop `zinder-query`. `WalletQuery` continues serving wallet primitives. `WalletQuery.TransparentAddressBalance` falls back to the canonical-confirmed compute path; the `explorer.transparent_address.balance_v1` capability disappears from `WalletQuery.ServerInfo` until the explorer proxy returns to readiness.
- An explorer derive view becoming inconsistent does not corrupt canonical state. Operators drop the explorer store and rebuild from `WalletQuery.ChainEvents` at `cursor = None`.
- Explorer readiness causes flow through the `/readyz` endpoint and `WalletQuery.ServerInfo` capability gating; they never propagate to the wallet plane's readiness.

## Source-boundary extensions

The explorer plane never calls upstream Zcash node RPCs. When a view needs a fact that is not in canonical artifacts or replayable events, the source boundary extends first:

1. New `NodeSource` method on `zinder-source` (e.g. `fetch_chain_value_pools`).
2. New `NodeCapability` variant identifying the surface.
3. The fact lands in `SourceBlock`, a typed `Source*` value, or a new canonical artifact family per [Extending artifacts](extending-artifacts.md).
4. The explorer consumer subscribes to the new event or artifact and materializes its view.

Chain value pools (the `ValuePoolSummary` view) is the first scheduled source-boundary extension. The upstream `getblockchaininfo.valuePools` field is already in scope (Zinder calls `getblockchaininfo` for network upgrade activations); extending the existing deserializer is mostly mechanical.

## Slicing plan (deferred)

The explorer plane lands incrementally. Each slice ships testable, capability-advertised, doc-updated value:

| Slice | Scope | Capabilities lit |
| ----- | ----- | ---------------- |
| ~~**0 (rename)**~~ | _Shipped._ Rebrand `zinder-derive` to `zinder-explorer`. Two existing capabilities become `explorer.server_info_v1` and `explorer.transparent_address.balance_v1`. | (renames) |
| ~~**1 (tracer bullet)**~~ | _Shipped._ `TransactionPublicFacts` parser per [ADR-0010](../adrs/0010-transaction-public-facts.md) is the single source of truth in `zinder_source::parse_transaction_public_facts`. `ExplorerQuery.TransactionDetail` returns the typed shape plus the cross-cutting `ExplorerFreshness` envelope per [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). Both mined and mempool transactions are covered; conflicting-chain returns `FAILED_PRECONDITION`. | `explorer.transaction.detail_v1` |
| **2** | `BlockSummary` and `BlockDetail` via the first real `BlockSummaryConsumer` derive view. Reorg-rewind test. | `explorer.block.summary_v1`, `explorer.block.detail_v1` |
| **3** | Typed `Search` with `SearchIndexConsumer` derive view. Privacy refusal for shielded inputs per [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). | `explorer.search.v1` |
| **4** | `MempoolSummary`, `MempoolActivity`, `TransparentAddressActivity`. Page-oriented aggregations of existing primitives. | `explorer.mempool.summary_v1`, `explorer.mempool.activity_v1`, `explorer.transparent_address.activity_v1` |
| **5** | `FeeSummary` (Shape C, no consumer). `ValuePoolSummary` requires the source-boundary extension. ZIP-317 conventional-fee vocabulary. | `explorer.fee.summary_v1`, `explorer.value_pool.summary_v1` |

Slices 2-5 can land in any order or in parallel once Slice 1 establishes the parser, freshness envelope, and federation patterns.

## Cross-references

- [Service boundaries](service-boundaries.md) — names `zinder-explorer` in the workspace inventory.
- [Derive plane](derive-plane.md) — the reusable SDK pattern the explorer plane exercises.
- [Wallet data plane](wallet-data-plane.md) — sibling boundary; canonical wallet read surface and federated dual-capability methods.
- [Public interfaces](public-interfaces.md) — naming spine, capability discovery, error vocabulary, configuration conventions.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions the explorer service inherits.
- [Reference: error vocabulary](../reference/error-vocabulary.md) — explorer-specific `ErrorReason` variants and retry semantics.
- [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md), [ADR-0010](../adrs/0010-transaction-public-facts.md), [ADR-0011](../adrs/0011-explorer-freshness-envelope.md), [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md) — the four decisions that govern this plane.
