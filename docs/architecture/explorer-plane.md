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

Additional methods land incrementally per [the slicing plan](#slicing-plan-deferred): `BlockSummariesInRange`, `BlockDetail`, `Search`, `TransparentAddressActivity`, `MempoolSummary`, `MempoolActivity`, `FeeSummary`, `ValuePoolSummary`. Slices 1-4 and `FeeSummary` are shipped; only `ValuePoolSummary` remains (deferred behind the source-boundary extension for chain value pools). Every method follows the same shape rules:

- Response message field tag 1 is `ExplorerFreshness freshness` ([ADR-0011](../adrs/0011-explorer-freshness-envelope.md)).
- Streaming responses are chunked; each chunk carries its own `ExplorerFreshness` and an opaque `cursor: bytes`.
- Paginated requests accept `from_cursor: bytes` plus `max_entries: uint32`.
- Fields use unit suffixes per [Public interfaces §Method Naming Conventions](public-interfaces.md#method-naming-conventions): `_zat`, `_zec`, `_height`, `_count`, `_bytes`, `_millis`, `_seconds`.

## Block view shape

The block listing and block-detail surfaces share one materialized view: a `BlockSummaryRecord` per canonical block, keyed by big-endian block height. The wire shape splits read concerns across two RPCs without duplicating storage.

`BlockSummary` carries the per-block facts a listing page renders:

```proto
message BlockSummary {
  uint32 block_height = 1;
  bytes block_hash = 2;                  // internal byte order, 32 bytes
  int64 block_time_unix_seconds = 3;
  uint32 transaction_count = 4;          // includes coinbase
  bytes previous_block_hash = 5;         // internal byte order, 32 bytes
}
```

`ExplorerQuery.BlockSummariesInRange` returns a range of `BlockSummary` rows ordered by ascending height. The handler reads the materialized record from the consumer store, projects the summary fields, and skips the transaction-id payload so the wire response stays cheap on long ranges.

`ExplorerQuery.BlockDetail` resolves either a height or a hash to one `BlockSummary` plus the canonical-ordered list of transaction ids. Clients drill into per-transaction facts by calling `ExplorerQuery.TransactionDetail` with each id from the list. The first slice keeps the per-tx surface as ids only; richer block-detail rows (per-tx component counts, fees, privacy shape) require new derive-time aggregation and ship in a later `_v2` increment.

The materialized record covers both reads so a `BlockDetail` request is one RocksDB get, and a `BlockSummariesInRange` request is one range scan. Storage cost is dominated by the transaction-id list: `~32 bytes × tx_count` per block, plus ~80 bytes for the summary fields.

Reorg rewind deletes every record in the reverted height range and re-fetches the replacement range before committing the cursor advance, so the view never advertises a stale BlockSummary for a height that no longer maps to the canonical chain.

## Search shape

`ExplorerQuery.Search` accepts a raw user input string and returns a typed `SearchResponse` carrying zero or more `SearchCandidate` arms in confidence order. The classifier lives in `crates/zinder-core/src/explorer_search.rs` and is a pure function of `(query, network)`; it never touches the canonical store, the wallet plane, or the network. The handler in `services/zinder-explorer/src/grpc/search.rs` composes the classifier output with optional `WalletQuery` confirmations:

- Numeric input routes through `WalletQuery.BlockIdBySelector(height)` to confirm the block exists at that height before emitting `BlockMatch`.
- 64-character hex input routes through both `BlockIdBySelector(hash)` and `Transaction(hash)`; whichever resolves emits a candidate. If both resolve, each candidate carries `confidence = 0.5`; otherwise the single winner carries `confidence = 1.0`.
- Transparent (`t*/tm*`, `t3*/t2*`), ZIP-320 TEX (`tex*/textest*`), and ZIP-316 unified (`u*/utest*/uregtest*`) addresses decode locally via `zcash_address`; transparent and unified-with-transparent-receivers candidates emit at full confidence without storage probes.
- Sapling and Sprout shielded inputs (`zs*/ztestsapling*/zc*`) route to the typed `NotPubliclyIndexable` arm with `reason = NOT_PUBLICLY_INDEXABLE_REASON_SHIELDED_ADDRESS`; viewing keys (`uivk*/uview*/zxviews*/zviews*`) route to the typed `NotPubliclyIndexable` arm with `reason = NOT_PUBLICLY_INDEXABLE_REASON_VIEWING_KEY` and the `canonical_form` field omitted.
- Unknown ZIP-316 receiver typecodes inside a unified address surface as `UnifiedAddressReceiverKind::UNKNOWN` with a typed `NotPubliclyIndexable` body; the enclosing unified-address arm still routes any recognized transparent receivers normally.
- Empty, oversized, and unparseable inputs route to `UnclassifiedMatch` with an operator-readable hint.

The classifier short-circuits shielded forms before the handler issues any `WalletQuery` call, which is the structural invariant required by [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). The handler's only universal wallet call is `WalletQuery.LatestBlock`, issued once at the end to build the `ExplorerFreshness` envelope (a property of every explorer response, not the search candidate).

A `SearchIndexConsumer` derive view that pre-builds sublinear address-prefix lookups for autocomplete is deferred to a later slice; the classifier alone gates the `explorer.search_v1` capability.

## Mempool views

The mempool surface is two RPCs that aggregate `WalletQuery.MempoolSnapshot` at request time. Neither requires a derive consumer: snapshot reads are bounded by the per-request cap (`max_entries` on the request, hard cap 4 096 on the explorer handler), and every entry is parsed once via `zinder_source::parse_transaction_public_facts` so the privacy-shape and version classifiers stay in lockstep with `TransactionDetail`.

`ExplorerQuery.MempoolSummary` returns one aggregated page:

```proto
message MempoolSummaryResponse {
  ExplorerFreshness freshness = 1;
  uint32 transaction_count = 2;
  uint64 total_size_bytes = 3;
  repeated PrivacyShapeCount privacy_shape_distribution = 4;
  repeated TransactionVersionCount version_distribution = 5;
  uint64 oldest_entry_age_millis = 6;
  uint64 newest_entry_age_millis = 7;
}
```

The age fields are wall-clock deltas computed at response time against the entry's `first_seen_unix_millis`; they are zero when the snapshot is empty.

`ExplorerQuery.MempoolActivity` paginates the same snapshot into typed entry rows sorted by newest-first observation time. The cursor is opaque: 12 bytes packing `(first_seen_unix_millis, transaction_id_tail_4_bytes)` big-endian. Mempool state is transient, so subsequent pages may interleave with new arrivals; clients that need a consistent paged read should treat any single response as a snapshot and re-pin if needed.

## Transparent-address activity

`ExplorerQuery.TransparentAddressActivity` is the single RPC explorer pages call to render the per-address activity feed. It composes two existing wallet primitives at request time:

1. `WalletQuery.TransparentAddressTxIdsInRange` for the confirmed slice (server-streamed; the explorer handler consumes up to `max_entries` rows and forwards the wallet cursor as the page's `next_cursor`).
2. `WalletQuery.TransparentMempoolOutputsByAddress` plus `WalletQuery.MempoolSnapshot` for the mempool overlay (the snapshot join hydrates `first_seen_unix_millis` per mempool entry).

The mempool overlay is emitted only on the first page (`from_cursor` empty and `include_mempool=true`) so subsequent pages stay deterministic. Mempool entries lead when `descending=true`; the confirmed slice leads when `descending=false`. Each row carries either the confirmed fields (`block_height`, `block_hash`, `tx_index_in_block`) or the mempool fields (`in_mempool=true`, `first_seen_unix_millis`); the two arms never coexist on one entry.

The handler dedupes against the mempool overlay's transaction ids when streaming the confirmed slice so a transaction that mines between the two reads never appears twice in one response.

## Fee summary

`ExplorerQuery.FeeSummary` aggregates per-transaction [ZIP-317](https://zips.z.cash/zip-0317) conventional fee floors across an inclusive block range. The fee fields are the ZIP-317 floor `MARGINAL_FEE × max(logical_actions, GRACE_ACTIONS)`, not miner-collected fees: computing actual fees requires resolving every transparent input via `WalletQuery.TransparentPrevouts`, and that fan-out is intentionally out of scope for `v1`. The conventional-fee floor is the minimum a wallet should attach to a transaction with the given shape; aggregates over many blocks give an explorer page a useful approximation of fee floors without prevout resolution.

`logical_actions = max(transparent_input_count, transparent_output_count, max(sapling_spend_count, sapling_output_count), orchard_action_count)`. The handler reads each block via `WalletQuery.FullBlock`, parses with `zebra-chain`, re-serializes each non-coinbase transaction, and calls `zinder_source::parse_transaction_public_facts` to extract the component counts. The fee helper lives on `zinder_core::TransactionComponentCounts::zip317_conventional_fee_zat` so the same formula is reusable from any handler that builds the count shape. The range cap is 256 blocks per request; coinbase transactions are excluded because they have no fee.

## Capability namespace

The explorer plane uses the `explorer.*` capability prefix. The full namespace structure:

| Capability | Owner method | Always-on? |
| ---------- | ------------ | ---------- |
| `explorer.server_info_v1` | `ExplorerQuery.ServerInfo` | Yes |
| `explorer.transaction.detail_v1` | `ExplorerQuery.TransactionDetail` | When the wallet endpoint is configured |
| `explorer.transparent_address.balance_v1` | `ExplorerQuery.TransparentAddressBalance` (and federated `WalletQuery.TransparentAddressBalance`) | When the wallet endpoint is configured |
| `explorer.block.summary_v1` | `ExplorerQuery.BlockSummariesInRange` + `BlockDetail` summary part | When the block-summary consumer is built and caught up |
| `explorer.block.detail_v1` | `ExplorerQuery.BlockDetail` per-tx rows | When the block-detail consumer is built and caught up |
| `explorer.transparent_address.activity_v1` | `ExplorerQuery.TransparentAddressActivity` | When the wallet endpoint is configured |
| `explorer.mempool.summary_v1` | `ExplorerQuery.MempoolSummary` | When the wallet endpoint is configured |
| `explorer.mempool.activity_v1` | `ExplorerQuery.MempoolActivity` | When the wallet endpoint is configured |
| `explorer.fee.summary_v1` | `ExplorerQuery.FeeSummary` | When the wallet endpoint is configured |
| `explorer.value_pool.summary_v1` | `ExplorerQuery.ValuePoolSummary` | When the source boundary supports chain value pools |
| `explorer.search_v1` | `ExplorerQuery.Search` | When the wallet endpoint is configured |

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
| ~~**3**~~ | _Shipped._ Typed `Search` with the local classifier in `crates/zinder-core/src/explorer_search.rs`. Privacy refusal for shielded inputs per [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). `SearchIndexConsumer` derive view for autocomplete is deferred. | `explorer.search_v1` |
| ~~**4**~~ | _Shipped._ `MempoolSummary`, `MempoolActivity`, `TransparentAddressActivity`. All three compose existing `WalletQuery` primitives at request time; no derive consumer required. | `explorer.mempool.summary_v1`, `explorer.mempool.activity_v1`, `explorer.transparent_address.activity_v1` |
| **5a** | _Shipped._ `FeeSummary` (Shape C, no consumer). Aggregates ZIP-317 conventional fee floors over a block range. Actual miner-collected fees are out of scope for v1 (would require per-input prevout resolution). | `explorer.fee.summary_v1` |
| **5b** | `ValuePoolSummary` requires the source-boundary extension for `getblockchaininfo.valuePools` plus a new `WalletQuery` primitive before the explorer handler can compose it. | `explorer.value_pool.summary_v1` |

Slices 2-5 can land in any order or in parallel once Slice 1 establishes the parser, freshness envelope, and federation patterns.

## Cross-references

- [Service boundaries](service-boundaries.md) — names `zinder-explorer` in the workspace inventory.
- [Derive plane](derive-plane.md) — the reusable SDK pattern the explorer plane exercises.
- [Wallet data plane](wallet-data-plane.md) — sibling boundary; canonical wallet read surface and federated dual-capability methods.
- [Public interfaces](public-interfaces.md) — naming spine, capability discovery, error vocabulary, configuration conventions.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions the explorer service inherits.
- [Reference: error vocabulary](../reference/error-vocabulary.md) — explorer-specific `ErrorReason` variants and retry semantics.
- [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md), [ADR-0010](../adrs/0010-transaction-public-facts.md), [ADR-0011](../adrs/0011-explorer-freshness-envelope.md), [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md) — the four decisions that govern this plane.
