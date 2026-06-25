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

- **Consumes** the writer-owned derive store as a RocksDB secondary under `storage.path`, plus `WalletQuery` over gRPC for federated read paths.
- **Owns** no primary RocksDB. `storage.path` is the canonical store path; the derive store lives at its `derive` subdirectory and is written by `zinder-ingest`.
- **Produces** the `ExplorerQuery` gRPC service.
- **Does not** open any primary store; does not call upstream Zcash node RPCs; does not custody any wallet secret; does not serve any balance RPC.

The boundary rules:

- A `zinder-explorer` crash does not stop ingest or wallet sync. The wallet plane keeps serving every `WalletQuery` primitive, including `WalletQuery.TransparentAddressBalance`.
- The explorer plane never extends canonical artifact schemas. When a view needs a fact the canonical surface does not carry, the source boundary extends first, the canonical artifact or event gains the field, the explorer subscribes.
- Server-side shielded address scanning, persisted viewing keys, and memo decryption are out of scope by product invariant.

## Wire surface

The native gRPC service is `ExplorerQuery` in `zinder.v1.explorer`. The service includes:

```proto
service ExplorerQuery {
  rpc ServerInfo(ServerInfoRequest) returns (ServerInfoResponse);

  rpc TransactionDetail(TransactionDetailRequest)
      returns (TransactionDetailResponse);
}
```

The same service also owns `BlockSummariesInRange`, `BlockDetail`, `Search`, `TransparentAddressActivity`, `MempoolSummary`, `MempoolActivity`, `FeeSummary`, and `ValuePoolSummary`. Every method follows the same shape rules:

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
  string block_hash = 2;                 // RPC byte order, 64 lowercase hex chars
  int64 block_time_unix_seconds = 3;
  uint32 transaction_count = 4;          // includes coinbase
  string previous_block_hash = 5;        // RPC byte order, 64 lowercase hex chars
}
```

`ExplorerQuery.BlockSummariesInRange` returns a range of `BlockSummary` rows ordered by ascending height. The handler reads the materialized record from the consumer store, projects the summary fields, and skips the transaction-id payload so the wire response stays cheap on long ranges.

`ExplorerQuery.BlockDetail` resolves either a height or a hash to one `BlockSummary` plus the canonical-ordered list of transaction ids. Clients drill into per-transaction facts by calling `ExplorerQuery.TransactionDetail` with each id from the list. Block detail intentionally keeps the per-transaction surface as ids; richer per-transaction facts belong in `TransactionDetail` or in a separately versioned aggregate view so block-detail rows stay bounded.

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

Autocomplete indexes are separate materialized views and do not gate the `explorer.search_v1` capability; the local classifier is the capability's correctness boundary.

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

## Transparent-address deltas

`ExplorerQuery.TransparentAddressDeltas` is the per-event counterpart of the activity feed. Where the activity surface returns one net row per transaction, the deltas surface returns one row per received output and per resolved spend, ordered ascending by height for `getaddressdeltas` parity. Each entry carries `transaction_id`, `block_height`, `block_time_unix_seconds`, an `index` (the output index for a received output, the input index for a spent prevout), a signed `value_zat` (positive for a receive, negative for a spend), and an explicit `kind` (`RECEIVED` or `SPENT`) so a reader never infers direction from the sign alone.

Both surfaces fold the same per-event attribution. `TransparentAddressDeltasConsumer` and `TransparentAddressActivityConsumer` each call one shared decomposition that turns a block into per-address value events; the deltas consumer persists every event while the activity consumer sums the events for one `(address, transaction)` into `net_value_zat`. The net activity for any address and range therefore equals the sum of the deltas over the same range.

Received-output events are always exact. Spend events carry `spent_value_zat` from the canonical spend fact, so they need no prevout re-resolution at read time. A spend whose prevout cannot be resolved (or when transparent-spend hydration is off) produces no delta event rather than a wrong number; the activity surface's `prevout_resolution_status` reports the same partial state for that transaction. The range read is paged with the standard opaque cursor and capped at 256 rows per page.

## Fee summary

`ExplorerQuery.FeeSummary` aggregates per-transaction [ZIP-317](https://zips.z.cash/zip-0317) conventional fee floors across an inclusive block range. The fee fields are the ZIP-317 floor `MARGINAL_FEE × max(logical_actions, GRACE_ACTIONS)`, not miner-collected fees: computing actual fees requires resolving every transparent input via `WalletQuery.TransparentOutputsByOutpoint`, and that fan-out is intentionally out of scope for `v1`. The conventional-fee floor is the minimum a wallet should attach to a transaction with the given shape; aggregates over many blocks give an explorer page a useful approximation of fee floors without prevout resolution.

`logical_actions = max(transparent_input_count, transparent_output_count, max(sapling_spend_count, sapling_output_count), orchard_action_count)`. The derive plane computes component counts from `TransactionFactsArtifact` once and materializes per-block fee aggregates in `BlockSummaryRecord`. The handler scans those typed records; it does not request raw blocks or parse `zebra-chain` bytes on the read path. The fee helper lives on `zinder_core::TransactionComponentCounts::zip317_conventional_fee_zat` so the same formula is reusable from any handler that builds the count shape. The range cap is 256 blocks per request; coinbase transactions are excluded because they have no fee.

## Value pool summary

`ExplorerQuery.ValuePoolSummary` wraps `WalletQuery.ChainValuePoolsAtTip` in the standard `ExplorerFreshness` envelope. It does not call upstream nodes directly and it does not project pool ids into fixed response fields. The response carries `repeated ChainValuePool pools` so existing UI can render known ids while future consensus pools remain visible without a new explorer wire shape.

## Capability namespace

The explorer plane uses the `explorer.*` capability prefix. The full namespace structure:

| Capability | Owner method | Always-on? |
| ---------- | ------------ | ---------- |
| `explorer.server_info_v1` | `ExplorerQuery.ServerInfo` | Yes |
| `explorer.transaction.detail_v1` | `ExplorerQuery.TransactionDetail` | When the wallet endpoint is configured |
| `explorer.block.summary_v1` | `ExplorerQuery.BlockSummariesInRange` + `BlockDetail` summary part | When the block-summary consumer is built and caught up |
| `explorer.block.detail_v1` | `ExplorerQuery.BlockDetail` per-tx rows | When the block-detail consumer is built and caught up |
| `explorer.transparent_address.activity_v1` | `ExplorerQuery.TransparentAddressActivity` | When the wallet endpoint is configured |
| `explorer.transparent_address.deltas_v1` | `ExplorerQuery.TransparentAddressDeltas` | When the wallet endpoint is configured |
| `explorer.mempool.summary_v1` | `ExplorerQuery.MempoolSummary` | When the wallet endpoint is configured |
| `explorer.mempool.activity_v1` | `ExplorerQuery.MempoolActivity` | When the wallet endpoint is configured |
| `explorer.fee.summary_v1` | `ExplorerQuery.FeeSummary` | When the wallet endpoint is configured |
| `explorer.value_pool.summary_v1` | `ExplorerQuery.ValuePoolSummary` | When the wallet endpoint is configured and `WalletQuery.ChainValuePoolsAtTip` is available |
| `explorer.search_v1` | `ExplorerQuery.Search` | When the wallet endpoint is configured |

The naming follows `explorer.<noun>.<capability>_v{N}`. The noun is a domain category; the capability is the operation. New methods add new capability strings; wire-shape changes ship as `_vN` increments.

Every `explorer.*` capability is served by the explorer plane itself; clients reach these methods on `ExplorerQuery`, never through `WalletQuery`. A future derive consumer that wants its view surfaced on the wallet client follows the federation rule in [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md): the capability lives in the consumer's product namespace, and the wallet plane advertises it only while the consumer's proxy is ready.

## Freshness envelope

Every explorer response embeds `ExplorerFreshness` at field tag 1. The shape and rationale live in [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). The key fields:

- `chain_view`: the cross-plane chain-state envelope (chain epoch, the `{role}_tip` axes, derive status). Identifies the snapshot the response was produced from. The upstream tip rides on `chain_view.upstream_tip`; the derive-replay ceiling on `chain_view.indexed_tip`. Index lag is `chain_view.chain_epoch.visible_tip.height - chain_view.indexed_tip.tip.height`.
- `snapshot_age_millis`: age of the mempool snapshot, when the response touches mempool state.
- `capability_version`: exact capability string that produced the response.
- `unavailable`: repeated `UnavailableField` entries declaring specific field paths absent with structured reasons.

`UnavailableField` carries a `field_path` (dotted-path matching the response shape), a structured `reason` (enum), and a `human_reason` string from the canonical registry in `crates/zinder-core/src/explorer_reasons.rs`. Frontends can branch on `reason` or render `human_reason` verbatim; both come from the same source so the words match across surfaces.

## Privacy boundary

The explorer plane is a privacy surface. The non-negotiable rules:

- Search for a shielded address, viewing key, or unified-address shielded receiver returns the typed `NotPubliclyIndexable` arm per [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). The arm carries a canonical reason string. Empty match lists for shielded inputs are forbidden.
- The classifier never reaches storage for shielded inputs. A privacy regression test enforces this with a mock that records storage call counts.
- The explorer plane never receives viewing keys, spending keys, or seed phrases over any RPC. Search inputs that classify as viewing keys are echoed back only in their typed `NotPubliclyIndexable` form; the `canonical_form` field is omitted for viewing keys to avoid logging-layer leaks.
- Server-side shielded scanning is out of scope. The explorer plane does not implement, persist, or expose any shielded-address indexing.

The wallet plane's privacy invariants ([ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md)) apply identically to the explorer plane; the explorer adds the typed refusal vocabulary so a refusal is a structured response, not an error.

## Operator surface

`zinder-explorer` ships standard ops endpoints (`/healthz`, `/readyz`, `/metrics`) on the shared `[ops]` listener (default `127.0.0.1:9069` for the explorer). Prometheus metrics use the `zinder_explorer_*` prefix.

Configuration follows the canonical TOML conventions:

```toml
[ops]
listen_addr = "127.0.0.1:9069"   # shared section; "" disables the endpoint

[storage]
path = "/var/lib/zinder/store"
secondary_path = "/var/lib/zinder/explorer-secondary"

[explorer]
listen_addr = "127.0.0.1:9068"
bearer_token_path = "/run/secrets/zinder-explorer-token"
wallet_query_endpoint = "https://zinder.example:9101"   # zinder-query gRPC

[explorer.freshness]
max_lag_blocks = 16              # response carries UNAVAILABLE_STALE beyond this
warn_lag_blocks = 4              # readiness cause flips at this threshold
```

When `explorer.bearer_token_path` is set, the `ExplorerQuery` gRPC endpoint enforces the same shared-secret bearer-token interceptor as `IngestControl` per [ADR-0006](../adrs/0006-ingest-control-transport-security.md). The explorer's own `wallet_query_endpoint` config points back at `zinder-query` for its wallet-composed reads (transaction detail, block views, search, mempool activity, value pools).

Environment-variable mapping uses the `ZINDER_EXPLORER__*` prefix for explorer-specific fields, plus the shared `ZINDER_OPS__*` prefix for the universal operational endpoint:

- `ZINDER_EXPLORER__LISTEN_ADDR`
- `ZINDER_EXPLORER__BEARER_TOKEN_PATH`
- `ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT`
- `ZINDER_STORAGE__PATH`
- `ZINDER_STORAGE__SECONDARY_PATH`
- `ZINDER_OPS__LISTEN_ADDR` (shared with every Zinder binary; default `127.0.0.1:9069` for the explorer)

## Failure isolation

The explorer plane fails independently from canonical state.

- An explorer service crash does not stop `zinder-ingest`. Ingest continues writing canonical artifacts and ChainEvents.
- An explorer service crash does not stop `zinder-query`. `WalletQuery` continues serving wallet primitives. `WalletQuery.TransparentAddressBalance` is wallet-plane and unaffected by explorer state: it sums the canonical unspent-output index in-process and overlays the live mempool through the colocated `IngestControl` endpoint.
- An explorer derive view becoming inconsistent does not corrupt canonical state. Operators drop the explorer store and rebuild from `WalletQuery.ChainEvents` at `cursor = None`.
- Explorer readiness causes flow through the `/readyz` endpoint and `WalletQuery.ServerInfo` capability gating; they never propagate to the wallet plane's readiness.

## Cursor expiry contract

Every paginated explorer RPC that emits a `next_cursor`
(`BlockSummariesInRange` does not, but the streaming `RecentTransactions`
and the address-keyed `TransparentAddressActivity` do) treats the
returned bytes as **valid for the lifetime of the column-family snapshot
at issue time**. Concretely:

- A cursor that resolves to a height range still present in the consumer
  CF resumes cleanly on the next request.
- A cursor that references a height range the consumer has dropped (a
  rolling reorg removed it, or a deliberate operator rebuild started
  fresh) is rejected with
  `Status::FailedPrecondition` carrying a typed
  `CursorExpiredError` in the gRPC status details. The error carries
  `recommended_resume_height` (the new safe starting point) and `hint`
  (a short sentence a UI may render verbatim).

Consumers handle expiry by either re-issuing the request with an empty
cursor (page from the head) or jumping to
`recommended_resume_height`. Silently jumping past the rewound range is
explicitly _not_ the server's responsibility; masking the discontinuity
would lose the reorg signal a UI needs to render a "view refreshed" hint.

## Source-boundary extensions

The explorer plane never calls upstream Zcash node RPCs. When a view needs a fact that is not in canonical artifacts or replayable events, the source boundary extends first:

1. New `NodeSource` method on `zinder-source` (e.g. `fetch_chain_value_pools`).
2. New `NodeCapability` variant identifying the surface.
3. The fact lands in `SourceBlock`, a typed `Source*` value, a source-backed control primitive, or a new canonical artifact family per [Extending artifacts](extending-artifacts.md).
4. The explorer consumer subscribes to the new event or artifact, or composes through the new `WalletQuery` primitive when the fact is intentionally live-source-backed.

Chain value pools (the `ValuePoolSummary` view) is the first source-boundary extension that stays live-source-backed. `zinder-source` parses `getblockchaininfo.valuePools`, `IngestControl` owns the writer-side source handle, `WalletQuery.ChainValuePoolsAtTip` proxies through that control plane, and `ExplorerQuery.ValuePoolSummary` wraps the wallet response in `ExplorerFreshness`.

## Derived views

Explorer-derived views use the derive-plane SDK and capability-gated optional fields. `BlockSummaryConsumer`, `TransactionFeesConsumer`, `MempoolEventCountsConsumer`, `TransparentAddressActivityConsumer`, `TransparentAddressDeltasConsumer`, and `RecentTransactionsConsumer` write product-specific rows in the derive store while the canonical store remains the wallet-correctness boundary. See [ADR-0017](../adrs/0017-derive-consumer-template-and-key-codec-convention.md) for the derive-consumer template and [ADR-0018](../adrs/0018-capability-gated-optional-payload-fields.md) for the optional-field convention.

## Cross-references

- [Service boundaries](service-boundaries.md) — names `zinder-explorer` in the workspace inventory.
- [Derive plane](derive-plane.md) — the reusable SDK pattern the explorer plane exercises.
- [Wallet data plane](wallet-data-plane.md) — sibling boundary; the canonical wallet read surface.
- [Public interfaces](public-interfaces.md) — naming spine, capability discovery, error vocabulary, configuration conventions.
- [Service operations](service-operations.md) — readiness, metrics, lifecycle conventions the explorer service inherits.
- [Reference: error vocabulary](../reference/error-vocabulary.md) — explorer-specific `ErrorReason` variants and retry semantics.
- [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md), [ADR-0010](../adrs/0010-transaction-public-facts.md), [ADR-0011](../adrs/0011-explorer-freshness-envelope.md), [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md) — the four decisions that govern this plane.
