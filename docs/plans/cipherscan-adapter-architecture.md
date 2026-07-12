# Plan: Cipherscan adapter architecture

Cipherscan currently consumes a product-specific REST API backed by PostgreSQL,
Redis, direct Zebra RPC/gRPC calls, lightwalletd, and the separate
`cipherscan-rust` indexer. Zinder currently serves `zexplorer` through native
`ExplorerQuery` and `WalletQuery` gRPC surfaces.

Cipherscan cannot change in the near term. The required near-term architecture
is therefore a Cipherscan-compatible REST adapter that preserves Cipherscan's
current HTTP paths and JSON response shapes while sourcing chain facts from
Zinder. Changing the Cipherscan app to consume native Zinder contracts remains a
future TODO, not a dependency for the adapter.

The adapter must still avoid polluting Zinder core. Its job is translation and
composition. When the adapter discovers a missing reusable chain fact, that fact
should be added to Zinder's native explorer plane with product-neutral names and
capabilities. When the adapter needs Cipherscan-only enrichments, those remain
adapter-owned or sidecar-owned.

This is intentionally allowed to break Zinder internals and native contracts
where a cleaner architecture requires it. It is not allowed to require
Cipherscan frontend changes in the current phase.

## Executive conclusion

Build a `zinder-compat-cipherscan` adapter first, but keep it thin.

The clean near-term architecture is:

1. **REST compatibility at the edge.** `zinder-compat-cipherscan` exposes the
   Cipherscan REST contract so the existing Cipherscan app can switch its API
   base URL without changing frontend code.
2. **Native Zinder facts behind the adapter.** The adapter depends on
   `ExplorerQuery` and `WalletQuery`, not on direct Zebra RPC or a Cipherscan
   PostgreSQL clone.
3. **Reusable gaps become native Zinder surfaces.** Page-ready block details,
   rich transaction details, transaction history, value-pool flows, and
   commitment-root search belong in Zinder when they are chain-data facts.
4. **Cipherscan-only data stays outside core.** Labels, prices, bridge data,
   ZNS, mining-pool branding, Crosslink, and privacy-risk scoring remain
   adapter-side joins or disabled/degraded endpoints until separate product
   ownership exists.
5. **Cipherscan app migration is deferred.** A later project can replace the
   REST adapter with native clients or a Cipherscan BFF. This plan must not rely
   on that later migration.

## Architecture principles

These rules apply to every phase.

1. **Preserve REST only at the boundary.** The adapter may preserve Cipherscan
   paths and JSON names. Zinder proto, capabilities, projections, and storage
   must not inherit Cipherscan REST vocabulary.
2. **Make the useful abstraction, not the generic one.** Avoid a generic
   "database adapter", "API adapter", "data provider", or "service manager".
   Add narrow contracts for real planes: explorer reads, wallet reads,
   projections, and Cipherscan REST translation.
3. **One concept, one owner.** A chain fact belongs in canonical artifacts or a
   replayable projection. A product enrichment belongs in Cipherscan or the
   adapter. A wire translation belongs in the adapter. Do not let one module own
   all three.
4. **Break bad Zinder names instead of copying Cipherscan terms.** Cipherscan
   paths such as `/api/uncles` can remain adapter routes, but Zinder core should
   use `non_canonical_block`, `displaced_block_archive`, `value_pool_flow`,
   `transaction_component_summary`, `paid_fee_zat`, and
   `chain_reorg_history`.
5. **Keep read paths coherent.** The adapter should prefer one native Zinder
   snapshot identity per page response. Do not force itself to join many native
   calls with unrelated freshness envelopes when a page-ready native query is
   justified.
6. **Expose absence explicitly.** Missing paid fees, unresolved prevouts,
   unsupported enrichments, and unavailable projections must surface through
   capability gates, typed unavailable fields, or stable Cipherscan-compatible
   degraded JSON. Never return silent zeroes or empty success responses for
   missing facts.

## Displaced block archive decision

The non-canonical block requirement is a canonical-writer concern, not an
ordinary derive projection. The implemented design is a writer-owned
`DisplacedBlockArchive`, written atomically in the same canonical-store
`WriteBatch` as `ReorgWindowChange::Replace`. When a replacement displaces an
old canonical block, the writer captures that block's preimage before removing
or hiding it from the canonical view. The archive record is keyed by network,
block hash, and height, and carries hash identity plus deterministic event and
height ordering. The hash is identity, not a value derived later from a
display row or a reorg summary.

The archive is activated by the first writer commit that displaces a block.
Its coverage is therefore explicit: it covers preimages captured by this
writer from that activation point forward, plus any later records written by
the same atomic replacement path. Activation does not imply historical
completeness, and the plan must not claim that pre-activation orphan blocks or
all historical forks can be recovered. Permanent retention is the default;
there is no wipe or routine retention sweep in this decision. Retained raw
block bytes are optional and independently capability-gated. Without raw
bytes, the archive may still serve the retained header, hash, height, event
ordering, and any other captured structured preimage fields, but it must mark
full block-body detail unavailable.

Readers restart writer-first: the archive writer and canonical store must be
opened and brought to a consistent visible epoch before archive readers start.
Reader startup must observe the writer's activated capability and coverage
metadata rather than infer completeness from a non-empty table. The
writer-first restart has succeeded in deployment. The displaced archive's
current schema remains 15; the volume created at `2026-07-05T22:52:43Z` was not
wiped or replayed, and
rollback uses `backups/cipherscan-displaced-block-archive-v0-20260711T232158Z`.
The first post-activation capture is now proven. At
`2026-07-11T23:42:59Z`, reorg event `33738` displaced block
`00baeb26d461582dc82a443b15f9b08ec076f631c1892d969f9bef77ba23c135` at height
`4,160,925`; its current canonical counterpart is
`007837b4ce72a5ccbfd1ebc97315545e8ea7a0021beb623da891e03f61b4ad47`. The
`/api/uncles`, `/api/uncles/forks` comparison, `/api/uncles/stats` (archive
`1`, events `225`, observed reverted `303`), `/api/uncle/:hash`, and
`/api/block/:hash` routes all populated. Writer-first ingest followed by
query/explorer restart stayed healthy and the displaced detail persisted.
Unchanged local `/reorgs` rendered 225 events and one orphan with working table
links; the hash page rendered the `ORPHANED BLOCK` comparison/details without
console errors. Historical coverage remains activation-limited.

`ChainReorgHistory` cannot replace this archive. It records that a canonical
range was reverted or replaced and supports the observed reverted-incident
sum (`observedRevertedBlocks`) and fork summaries, while the archive occurrence
count (`totalOrphanedBlocks`) reports only captured displaced-block preimages.
Those counters are intentionally distinct:
activation-limited archive coverage must not be presented as historical
completeness. The implemented native `DisplacedBlockHistory` and
`DisplacedBlockDetail` capabilities retain the hashes and structured preimages
needed for `/api/uncles`, `/api/uncle/:hash`, `/api/block/:hash` fallback, and
captured fork-comparison rows. Cipherscan reports, monitored node
observations, and miner/pool branding remain Cipherscan-owned sidecars: they
are not canonical block preimages and must not be added to this archive.

## Displaced commitment-root boundary

The current schema-15 displaced archive does **not** retain final Sapling,
Orchard, or Ironwood commitment roots. Its block header, transaction-id list,
coinbase outputs, and optional raw bytes are therefore insufficient to answer
the anchor-root question for a displaced block. The existing archive can serve
displaced block identity and detail, but it cannot already serve non-canonical
commitment-root matches. The earlier claim that `/api/search/anchor/:root`
could obtain those matches from `DisplacedBlockHistory` was incorrect and is
replaced by this boundary.

The schema-16 design adds the displaced block's final-root family to the
writer-owned preimage captured atomically with `ReorgWindowChange::Replace`.
The native root-search response keeps canonical and displaced results separate:
`matches` remains canonical, `displaced_matches` contains only retained
displaced positives, and `displaced_coverage` carries the archive activation
boundary. The additive capability is
`explorer.commitment_root.displaced_matches_v1`. This is an activation-limited
read: coverage proves only the archive range captured since activation, never
pre-activation orphan history or global historical absence.

The adapter maps native `displaced_matches` into Cipherscan's `orphaned` array
with `chain: "orphaned"`. It does not add miner data to the native root-match
message. Orphaned `minerAddress` stays null unless the adapter performs a
bounded `DisplacedBlockDetail` coinbase lookup and decodes a standard payout
script; no canonical miner join is attempted for a displaced hash. For an
empty result, the adapter says that no retained displaced match exists in the
covered archive since activation, exposes the native activation event/epoch/
time and returned-match count, and keeps `degraded`/`unavailable` set because
pre-activation history remains unknown. A positive retained match is factual,
but it does not expand the archive's historical coverage.

Acceptance stops at focused adapter tests for canonical positives, displaced
positives with a null miner, capability/coverage gating, and activation-limited
negative diagnosis, plus native integration proof that schema-16 root fields
survive replacement, restart, and bounded lookup. No live root-search proof is
claimed until that writer/explorer path and a real post-activation displaced
root are exercised. The unchanged Cipherscan root-search client's canonical
and orphaned array union mismatch remains a future Cipherscan TODO.

## DX, UX, and AX goals

### Developer Experience

- Cipherscan can point at the adapter without changing frontend code.
- Zinder developers add reusable facts to native `ExplorerQuery` and
  `WalletQuery`, not to a Cipherscan-shaped core module.
- The adapter has a small, predictable responsibility: route HTTP, call native
  Zinder, serialize Cipherscan JSON, and join optional sidecars.
- Capability strings answer "can this adapter route return real data?" without
  probing every route.

### User Experience

- Core Cipherscan pages keep working during the backend migration.
- Block, transaction, address, mempool, and dashboard pages become faster as
  native page-ready Zinder reads replace Cipherscan Postgres and direct Zebra
  fan-out.
- Degraded pages explain what is missing: projection lagging, paid fee
  unavailable, sidecar disabled, or route outside Zinder's scope.
- Shielded addresses and viewing keys keep privacy-safe behavior even when the
  Cipherscan REST route expects a search-like response.

### Agent Experience

- Future agents can see the boundary from the file tree:
  `services/zinder-compat-cipherscan` means REST translation; `zinder-proto`
  means native contract; `zinder-derive` means reusable projection.
- Names in Zinder stay grep-friendly and product-neutral:
  `value_pool_flow`, not `privacy_stats`; `non_canonical_block`, not `uncle`;
  `transaction_history`, not `transactions/list`.
- The coverage matrix records end-state ownership, so agents know whether a
  missing field belongs in Zinder, the adapter, Cipherscan sidecars, or nowhere.

## End-state topology

Near term:

```text
                 +-----------------------+
                 |      Cipherscan UI     |
                 | unchanged REST client  |
                 +-----------+-----------+
                             |
                             v
          +--------------------------------------+
          |     zinder-compat-cipherscan         |
          | - Cipherscan REST paths and JSON     |
          | - native Zinder gRPC clients         |
          | - optional Cipherscan sidecar joins  |
          +------------------+-------------------+
                             |
                             v
        +------------------------------------------+
        |          Zinder native services           |
        | ExplorerQuery        WalletQuery          |
        | page-ready explorer  wallet primitives    |
        | projections          canonical reads      |
        +------------------+-----------------------+
                           |
                           v
                  zinder-ingest + derive plane
```

Later TODO:

```text
Cipherscan UI -> native Zinder client or Cipherscan BFF -> Zinder native services
```

The later TODO can remove the REST adapter for internal app traffic, but this
plan does not depend on that migration.

## Adapter placement and shape

Create `services/zinder-compat-cipherscan` when implementation begins.

The service owns:

- Cipherscan HTTP route matching.
- Query parameter parsing and Cipherscan defaults.
- Offset, limit, and cursor compatibility.
- JSON response serialization with current Cipherscan field names.
- Stable degraded responses for unsupported or sidecar-missing routes.
- gRPC clients for `ExplorerQuery` and `WalletQuery`.
- Optional clients for Cipherscan-owned enrichments.

The service must not own:

- Canonical chain storage.
- A Cipherscan-shaped PostgreSQL schema.
- Direct Zebra RPC as the normal source for chain facts.
- New product-neutral chain projections.
- Identity/linkability risk scoring or viewing-key scanning. Deterministic
  compatibility scores over complete native aggregate counts may remain here
  when they do not infer participants or claim adversarial privacy guarantees.

## Current implementation snapshot

This worktree now contains the first adapter implementation:

- `services/zinder-compat-cipherscan` exposes a Cipherscan-compatible HTTP
  service on `127.0.0.1:9070` by default.
- The binary participates in shared Zinder runtime configuration and ops
  conventions as `zinder-compat-cipherscan`; its ops endpoint defaults to
  `127.0.0.1:9108`.
- The adapter connects to Zinder through authenticated intra-Zinder gRPC
  channels, defaulting to `ExplorerQuery` at `http://127.0.0.1:9068` and
  `WalletQuery` at `http://127.0.0.1:9101`.
- Browser CORS is allowed for local Cipherscan smoke tests. One outer router
  middleware answers every `OPTIONS` request with `204` and the adapter's CORS
  headers before Axum performs route-specific method matching.

### Implemented route families

| Route family | Current behavior | Native source |
| --- | --- | --- |
| `/api/info`, `/api/blockchain-info` | Returns Cipherscan's height aliases and a partial `getblockchaininfo`-compatible tip response from Zinder's latest block. Adapter identity and Zinder service health stay on ops/native surfaces instead of the Cipherscan chain-info route. | `WalletQuery.LatestBlock` |
| `/api/blocks`, `/api/blocks/list`, `/api/network/blocks/recent` | Returns recent block rows in Cipherscan's JSON field names and PostgreSQL-compatible string fields where public Cipherscan exposes them. All three routes use the bounded block-production series for canonical coinbase facts and miner payout identity; the list routes also populate difficulty from the canonical compact target, and `/api/blocks/list` preserves cursor pagination. `/api/network/blocks/recent` returns public-style ZEC fee/reward units. Pool branding remains null. | `WalletQuery.LatestBlock`, `ExplorerQuery.BlockProductionSeries` |
| `/api/block/:heightOrHash` | Returns the block summary, canonical miner payout address, canonical-ordered transaction rows, and optional post-block Sapling, Orchard, and Ironwood roots. Public transaction counts, resolved transparent inputs, transparent outputs, and standard transparent addresses are included where retained facts permit decoding. Input values remain independently optional; the adapter withholds them block-wide when Cipherscan's unchanged fee arithmetic would otherwise treat an incomplete shielded transaction as fully known. Rows are withheld when a canonical transaction fact is unavailable rather than misclassifying it. | `ExplorerQuery.BlockTransactions`, `ExplorerQuery.BlockProductionSeries`, `WalletQuery.BlockHeaderBySelector`, `BlockFinalNoteCommitmentRoots` |
| `/api/tx/:txid` | Returns public transaction facts, location, ordered transparent inputs with outpoints plus independently resolved values and scripts, canonical transparent outputs with epoch-pinned spent state, signed intrinsic Sapling, Orchard, and Ironwood pool balances, and standard transparent addresses decoded at the adapter edge. Missing parent facts, nonstandard scripts, and unavailable intrinsic artifacts remain explicit unavailability. | `ExplorerQuery.TransactionDetail`, `WalletQuery.TransparentSpendsByOutpoint` |
| `/api/tx/:txid/raw`, `/api/tx/raw/batch` | Returns raw transaction hex when Zinder retained transaction bytes or the transaction is in mempool. Batch lookup preserves Cipherscan's scanner shape with per-transaction failures instead of failing the whole request. | `WalletQuery.Transaction` |
| `/api/tx/broadcast` | Broadcasts raw transaction hex and maps typed outcomes into Cipherscan success/failure JSON. | `WalletQuery.BroadcastTransaction` |
| `/api/transactions/list`, `/api/tx/shielded` | Uses canonical `TransactionHistory` pages in height-descending, transaction-index-descending order. The adapter translates Cipherscan's height/index cursors, offset pagination, transaction type, shielded protocol, fully-shielded/partial, and minimum-action filters without exporting SQL semantics into Zinder. `/api/tx/shielded` requests its materialized-row total in the same native response unless Cipherscan's `skip_count` or small-first-page rule applies; `/api/transactions/list` retains its 30-second edge cache. Signed intrinsic balances are mapped when retained. Unproven paid fees remain null, with ZIP-317 conventional fees exposed separately instead of occupying Cipherscan's paid-fee field. | `ExplorerQuery.TransactionHistory` |
| `/api/shielded/list` | Serves canonical net flow events with native filters, exact totals after complete coverage, and Cipherscan timestamp/id pagination. Transparent address attribution remains explicitly unavailable rather than inferred. | `ExplorerQuery.ValuePoolFlowHistory` |
| `/api/address/:address` | Returns one epoch-coherent transparent summary and confirmed activity page, including lifetime totals, exact count, stable page navigation, transaction facts, standard counterparties, and explicit coverage. Valid unused addresses return Cipherscan's zero-history shape; shielded and unified addresses return its privacy-safe response. | `ExplorerQuery.TransparentAddressActivity` v2, backed by `TransparentAddressRanking` and retained canonical transaction facts |
| `/api/rich-list` | Returns bounded positive-balance rankings, lifetime totals, address counts, and top-10/top-100 concentration from one active native generation. Standard transparent scripts are encoded at the adapter edge; labels remain null and sidecar-owned. | `ExplorerQuery.TransparentAddressRanking` |
| `/api/mempool`, `/api/mempool/tx/:txid` | Returns one coherent mempool count, sampled transactions with parsed component counts and transparent-output total, and Cipherscan-compatible point lookup for live mempool transactions. Missing, confirmed, or conflicting transactions return `inMempool: false` for the point lookup. | `ExplorerQuery.MempoolSnapshot`, `ExplorerQuery.TransactionDetail` |
| `/api/network/stats`, `/api/network/health` | Returns Zinder-backed chain/service health, current consensus subsidy, current chain supply, and numeric degraded defaults for crawler facts that Cipherscan renders as numbers. | `ExplorerQuery.BlockSummariesInRange`, `ExplorerQuery.ServerInfo`, `WalletQuery.ServerInfo`, Zebra consensus subsidy helpers |
| `/api/network/halving`, `/api/network/emission` | Returns the current halving countdown and subsidy split from consensus, exact current chain supply from the source-backed value-pool summary, and complete daily supply/emission history from canonical value-pool balance snapshots. The adapter owns Cipherscan period coercion and adjacent-day delta formatting. | `ExplorerQuery.ValuePoolSummary`, `ExplorerQuery.ValuePoolBalanceHistory`, Zebra consensus subsidy helpers |
| `/api/network/fees` | Returns Cipherscan-compatible ZIP-317 fee estimate tiers and an explicit observed ZIP-317 conventional-fee summary over the latest bounded block window. | `WalletQuery.LatestBlock`, `ExplorerQuery.FeeSummary` |
| `/api/network/fee-distribution` | Returns positive, non-coinbase actual paid-fee frequencies grouped by UTC day and translated to Cipherscan's rolling percentile shape. The response declares exact coverage and never substitutes conventional fees when a paid fee is unavailable. | `ExplorerQuery.PaidFeeDistribution`, backed by canonical `TransactionIntrinsicValueBalances` and resolved transparent prevouts |
| `/api/migration/overview`, `/api/migration/cohorts`, `/api/migration/denominations` | Returns one coherent Ironwood migration snapshot for all three routes. The adapter scans forward from the block before NU6.3 activation, includes every Ironwood transaction with a negative intrinsic Ironwood balance (including coinbase), and derives Cipherscan's overview, 256-block cohorts, and integer-safe denomination bins without SQL semantics in Zinder. | `ExplorerQuery.TransactionHistory` plus `explorer.transaction.intrinsic_value_balances_v1`; `ExplorerQuery.ValuePoolSummary` for current pool progress |
| `/api/network/protocol-stats`, `/api/stats/shielded-count`, `/api/stats/shielded-daily` | Returns complete cumulative, monthly, detailed, and daily component statistics with explicit contiguous coverage. Legacy Sapling/Orchard classifications preserve Cipherscan semantics; native totals retain source-correct consensus component counts. | `ExplorerQuery.TransactionComponentSummary`, `WalletQuery.LatestBlock` |
| `/api/mining/rewards` | Preserves Cipherscan's daily mining-reward series and five-minute cache. The adapter pages backward through canonical `BlockProductionSeries` rows at one pinned chain epoch, filters the exact wall-clock cutoff, and exposes explicit coverage plus coinbase and fee bases. | `WalletQuery.LatestBlock`, `ExplorerQuery.BlockProductionSeries` |
| `/api/supply`, `/api/circulating-supply`, `/api/supply/transparent-breakdown` | Returns Cipherscan-compatible value-pool rows from Zinder's upstream value-pool read, current circulating supply as plain text or JSON, and P2PKH/P2SH positive-address aggregates from the active ranking generation. | `ExplorerQuery.ValuePoolSummary`, `ExplorerQuery.TransparentAddressRanking`, Zebra consensus subsidy helpers |
| `/api/crosslink/fork-monitor`, `/api/crosslink/fork-monitor/check`, `/api/crosslink/block-hash/:height` | Returns a read-only fork-monitor snapshot from Zinder's canonical tip and block selector. cTAZ comparison and community registry routes are explicitly degraded or sidecar-owned. | `WalletQuery.LatestBlock`, `WalletQuery.LatestSafeBlock`, `WalletQuery.BlockIdBySelector` |
| Privacy, peer/node inventory, and chain-size endpoints | Return stable degraded JSON with `degraded` and `unavailable` markers. | Privacy and node inventory are sidecar-owned or unavailable; physical chain-size history is operational sidecar data. |

### Local smoke-test path

Run the local Z3 regtest stack first:

```bash
cd /Users/gustavovalverde/dev/zfnd/z3
./scripts/regtest-init.sh
docker compose --env-file .env.regtest up -d
```

Regtest's host-side Zebra/Zallet JSON-RPC path is the rpc-router at
`http://127.0.0.1:8181`, with default basic auth `zebra:zebra`. Start the
normal Zinder ingest/query/explorer stack against that Z3 endpoint, then start
the adapter:

```bash
cd /Users/gustavovalverde/dev/zfnd/zinder-cipherscan-compat-coverage
cargo run -p zinder-compat-cipherscan -- \
  --network zcash-regtest \
  --explorer-query-endpoint http://127.0.0.1:9068 \
  --wallet-query-endpoint http://127.0.0.1:9101 \
  --listen-addr 127.0.0.1:9070
```

Basic adapter probes:

```bash
curl -sf http://127.0.0.1:9070/api/info | jq .
curl -sf 'http://127.0.0.1:9070/api/blocks?limit=5' | jq .
curl -sf http://127.0.0.1:9108/readyz | jq .
```

Point the unchanged Cipherscan app at the adapter through its existing
Crosslink API override:

```bash
cd /Users/gustavovalverde/dev/zfnd/cipherscan
NEXT_PUBLIC_NETWORK=crosslink-testnet \
NEXT_PUBLIC_CROSSLINK_API_URL=http://127.0.0.1:9070 \
NEXT_PUBLIC_API_URL=http://127.0.0.1:9070 \
npm run dev
```

Open `http://localhost:3003` for browser validation. Next's development client
blocks its hot-reload resource when the page is opened through
`http://127.0.0.1:3003`; that is a local development-server origin constraint,
not an adapter response failure.

The mainnet/testnet API base URLs in Cipherscan are hard-coded today. Until
Cipherscan changes, use the `crosslink-testnet` override above or proxy one of
the hard-coded API hostnames to the local adapter. `NEXT_PUBLIC_API_URL` is
also required today because Cipherscan's address-label helper reads that
separate variable instead of `NEXT_PUBLIC_CROSSLINK_API_URL`.

### Local testnet validation snapshot

Validated on 2026-07-09 against the existing Docker `z3-testnet` and
`zinder-testnet` stacks.

The existing `zinder-testnet` project already had healthy ingest and
wallet-query containers. The updated worktree added only the missing
`zinder-explorer` reader to the same Compose project:

```bash
docker compose --env-file deploy/.env.testnet \
  -f deploy/docker-compose.yml \
  -p zinder-testnet \
  up -d --build --no-deps zinder-explorer
```

For this local Z3 compatibility stack, Zebra must also expose its existing
private indexer gRPC listener on the shared Docker network:

```yaml
services:
  zebra:
    environment:
      ZEBRA_RPC__INDEXER_LISTEN_ADDR: 0.0.0.0:18230
```

This is a local runtime prerequisite for Zinder's configured
`zinder_node.indexer_grpc_addr`, not a new Zinder persistence contract. Keep
the port un-published on the host. Enabling it changed ingest readiness from
`mempool_source_unavailable` to `ready`, without modifying Zinder data,
running a replay, or recreating any volume. Zaino gRPC is not a substitute:
Zinder consumes Zebra's `zebra.indexer.rpc` mempool and chain-tip streams.

Observed host ports:

- `zinder-explorer`: `http://127.0.0.1:19068`, ops
  `http://127.0.0.1:19069`
- `zinder-query`: `http://127.0.0.1:19101`, ops
  `http://127.0.0.1:19106`
- adapter: `http://127.0.0.1:9070`, ops `http://127.0.0.1:9108`
- Cipherscan dev server: `http://localhost:3003`

Run the adapter against Docker testnet:

```bash
cargo run -p zinder-compat-cipherscan -- \
  --network zcash-testnet \
  --listen-addr 127.0.0.1:9070 \
  --ops-listen-addr 127.0.0.1:9108 \
  --explorer-query-endpoint http://127.0.0.1:19068 \
  --wallet-query-endpoint http://127.0.0.1:19101
```

Run unchanged Cipherscan against the adapter, then open
`http://localhost:3003` in the browser:

```bash
cd /Users/gustavovalverde/dev/zfnd/cipherscan
PORT=3003 \
NEXT_PUBLIC_NETWORK=crosslink-testnet \
NEXT_PUBLIC_CROSSLINK_API_URL=http://127.0.0.1:9070 \
NEXT_PUBLIC_API_URL=http://127.0.0.1:9070 \
npm run dev -- --hostname localhost
```

Browser validation covered the home page, block detail, transaction detail,
mempool, block list, transaction list, shielded transactions, privacy dashboard,
rich list, network overview, docs, tools, learn/about pages, and the
Crosslink-specific chain, validators, bootstrap, and fork-monitor pages. The
current local smoke result is: no tested page renders Cipherscan's error
fallback and no tested page shows a visible "Unavailable" state.

Adapter fixes that came from this pass:

- block lists must not require `ExplorerFreshness.chain_view.chain_epoch`; the
  testnet explorer can be ready while that metadata is absent.
- Cipherscan renders some degraded fields as numbers, so REST compatibility
  must use numeric degraded defaults on those paths and reserve `null` for
  fields the current UI already treats as optional.
- `/api/shielded/list` is a shielded-flow contract with `flows` and cursor
  pagination; it must not share the transaction-list contract from
  `/api/tx/shielded`.
- The unchanged shielded-flow contract is one net event per canonical
  transaction, ordered newest-first by block time and a stable event
  coordinate. It emits only when the aggregate signed shielded balance is
  nonzero and the transaction crosses the transparent boundary. Positive
  aggregate balance is deshielding; negative aggregate balance is shielding;
  zero-net shielded-to-shielded transfers are not flow rows.
- `/api/shielded/list` filters `flow_type`, `pool`, and `min_zec` before
  pagination. Its REST cursor pair is Cipherscan compatibility state, not a
  native storage identity: the adapter maps block time plus a stable event
  coordinate to the native opaque cursor contract.
- `/api/pools/flows` aggregates the same canonical events into half-open UTC
  hour or day buckets. It must not reconstruct flows from historical pool
  balances, and pool-balance history must not be reconstructed from flow
  events.
- `/api/privacy-stats` must use the production direct-stats shape. Returning
  `success: true` without a `data` wrapper makes Cipherscan discard the body.
- Privacy-stats compatibility follows the current production job for all-time
  Sapling/Orchard/Ironwood predicates, score weights, rolling average, and
  adoption trend. Daily rows use exact UTC buckets instead of copying the
  production job's 1,153-block approximation or the archived backfill's
  Ironwood omission and final-second error. Cipherscan's UI still describes
  the older Sapling/Orchard and 40/40/20 formulas even though its executable
  job uses Sapling/Orchard/Ironwood and 40/30/30; changing that text remains a
  Cipherscan TODO.
- Sidecar absence must preserve each route's executable compatibility shape
  without inventing facts. Safety-sensitive labels, names, cross-chain,
  Crosslink finality, and finalizer routes return explicit `503`
  unavailable responses. Current and recent historical prices are supplied by
  a bounded external-market transport; older history remains sidecar-owned.
- Cipherscan's nav requires numeric `price` and `change24h`; `null` crashes the
  shared layout and zero is rendered as a real `$0.00` price. The adapter now
  validates both provider values before caching or returning them.
- The network page only renders halving and emission panels when
  `/api/network/stats` contains a `supply` object. Current chain supply can be
  derived without replay. The current pool composition comes from
  `ExplorerQuery.ValuePoolSummary`; emission and halving facts come from the
  consensus subsidy schedule. Standard P2PKH/P2SH classifications come from
  the transparent-address ranking generation; nonstandard and otherwise
  unattributed transparent value remains an explicit remainder.

Verified degraded routes now include:

| Route family | Current local behavior | Remaining non-degraded requirement |
| --- | --- | --- |
| `/api/network/halving`, `/api/network/emission`, `/api/network/stats` | Returns current tip-backed subsidy, halving countdown, daily emission estimate, source-backed transparent, Sprout, Sapling, Orchard, Ironwood, and Lockbox balances, plus complete 24-hour block and transaction counts, latest difficulty, average block time, hashrate, and actual-window revenue. Emission now uses the exact current value-pool sum rather than theoretical maximum issuance and maps complete canonical daily value-pool snapshots into Cipherscan's `supplyHistory` and positive adjacent-day `dailyEmission` arrays. The pool snapshot supplies its chain epoch, and the adapter pins matching wallet-header and paged `BlockProductionSeries` reads to that epoch. The 24-hour scan is capped at 20,000 blocks and fails closed on missing coverage or an unreached cutoff. A live testnet response at height 4,158,367 returned 2,322 blocks, 2,858 transactions, a 37-second average, difficulty 67.0183, and 1.81 H/s in 0.8 seconds. Public Cipherscan was one cached block behind with exactly one fewer block and transaction and the same difficulty-over-interval formula. Unchanged local `/network` renders the nonzero transaction count, 1.81 H/s, `Synced`, 17.88M circulating ZEC, 12.4% shielded supply, all shielded pool balances, Lockbox, and halving details without an application error. | Complete for current, 24-hour, and daily historical supply/emission metrics without another schema change, replay, backfill, or data wipe. The live adapter returned 8/31/91/366/3,539 supply points for 7d/30d/90d/1y/all and 7/30/90/365/3,538 daily deltas; every 30-day supply point matched the corresponding native zatoshi pool-history total. Zinder keeps consensus-correct miner, funding-stream, and Lockbox amounts even where public testnet reports every subsidy unit as miner revenue. Native `explorer.chain.economics_v1` history is needed only if historical subsidy-allocation facts are required outside the adapter. Standard transparent script classifications are now supplied by the separate ranking projection. Public testnet API routes returned cached Next.js `404` responses during the final comparison, so current public payload equality could not be refreshed. |
| `/api/network/health` | Returns Cipherscan's boolean Zebra-health compatibility fields from successful Zinder explorer and wallet service reads. Local and public testnet now both return `200` with identical `success: true`, `healthy: true`, and `ready: true` compatibility fields, and both unchanged Network pages render `Synced` without an application error. Local `healthEndpointAvailable` and `readyEndpointAvailable` remain false with `source: zinder-query-plane`; public reports both true because it probes Zebra directly. | Complete for Cipherscan's rendered health state without a schema, replay, backfill, or data wipe. Direct Zebra process health can be added only as a separately configured operational dependency; it is not a chain-data requirement and must not be implied by successful Zinder reads. |
| Root WebSocket `/` | Bridges the existing replayable `WalletQuery.ChainEvents` and `WalletQuery.MempoolEvents` streams into Cipherscan's `new_block`, `mempool_tx`, and `mempool_removed` frames. One process-wide bounded fanout owns both native cursors; browser sockets do not multiply upstream subscriptions. Native reconnects resume strictly after the last delivered cursor. Each non-empty committed range is collected in ascending order against one current chain epoch before any frame is emitted; the final hash must match the event's committed visible tip. An older `LatestBlock` epoch is reader lag and retries, while only a newer epoch whose tip is below the committed range proves supersession. A later reorg therefore skips and advances its obsolete event instead of permanently stalling the relay, without dropping events merely because the query secondary has not caught up yet. Empty commit ranges are cursor-only markers. Mempool additions are parsed from the immutable raw transaction bytes carried by the native event, so mining cannot make the add disappear before its later removal. Lagged or source-disconnected sockets receive `1013`; a send-timeout socket receives a best-effort `1013` before the adapter drops it, and clients recover through Cipherscan's HTTP polling. A tracked task set keeps upgraded sockets alive long enough to flush shutdown; a live termination probe received `1001` with `adapter shutting down`. Live testnet sockets have matched HTTP block detail exactly for blocks 4,157,249, 4,157,250, 4,157,288, and post-reader-lag-fix block 4,157,300. Two simultaneous clients received identical block 4,157,251 frames. A 257-frame test proves complete ordered delivery beyond the 256-frame channel capacity, so the producer cannot disconnect fast clients by overrunning its own fanout buffer. Three live additions (`1c384bff...ddd9d30`, `e682f3c3...0fe8448`, and `659987fb...879b3d9`) were followed by matching `mined` removals at block 4,157,296; after each removal the point lookup returned `inMempool: false`, and the canonical block retained the emitted size and transparent counts. A 25-second local/public comparison observed local `new_block` plus the adapter extension `privacy_stats`, while public emitted only `new_block`. | No native contract, schema, replay, backfill, or data wipe is required. The adapter uses existing product-neutral streams plus `ExplorerQuery.BlockProductionSeries`; mempool additions use their native `MempoolEntry` bytes and outputs without another upstream lookup. When a client is connected, committed blocks also trigger a best-effort `privacy_stats` frame built from the same truthful REST body; unavailable product scores remain null. `network_stats` is not a compatibility gap: current Cipherscan has a client branch for it but no server producer, and `/network` uses an initial plus 60-second `GET /api/network/stats` polling path. Adding the frame would be speculative behavior rather than parity. |
| `/api/blocks`, `/api/blocks/list`, `/api/network/blocks/recent` | Uses `ExplorerQuery.BlockProductionSeries` v2 to populate Cipherscan's difficulty and miner-address fields while retaining each route's list and pagination envelope. For shared testnet heights 4,158,353 through 4,158,357, local hashes, difficulty strings, and miner addresses match public Cipherscan exactly on all three routes. Native coinbase facts expose the miner output at index 0 and the distinct developer-fund output at index 1; the adapter decodes only index 0 for Cipherscan's legacy `miner_address` or `minerAddress` field. The adapter rounds only the legacy difficulty JSON string to the 15 significant digits produced by Cipherscan's JavaScript/PostgreSQL path; Zinder's native computation retains full precision. A fresh unchanged `/blocks` page renders without browser warnings or errors; its Miner column remains `—` because this checkout displays `miner_pool`, so miner-address API parity is the acceptance boundary. | Complete without a persisted-data change, schema migration, replay, backfill, or data wipe. The explorer reader and adapter were rebuilt while ingest retained container `a48f26050675` and the existing volume created at `2026-07-05T22:52:43Z`. Pool branding remains sidecar-owned. Public Cipherscan reports block sizes one byte below Zebra's serialized size; Zinder keeps the source-correct size. |
| `/api/block/:heightOrHash` | The adapter returns public-equivalent block headers, canonical transaction order, typed rows, transparent outputs, resolved standard transparent input identities, validated miner payout identity, and coinbase miner data decoded from same-epoch retained transaction bytes. `ExplorerQuery.BlockTransactions` now also carries optional canonical final note-commitment roots. For shared testnet block 4,158,544, local and public matched height, hash, coinbase hex/text, miner address, Sapling root, and Orchard root exactly; local additionally matched Zebra's Ironwood root while public returned it as null. Local also exposes the source-backed coinbase reward and protocol component counters that are absent from the public payload. Unchanged local Cipherscan rendered the coinbase text, all three roots under More Details, and the canonical fee recipient. | Complete for canonical block detail, coinbase miner data, and post-block roots. Schema 13 was required only for the reusable root artifact and migrated the existing volume in place; coinbase data reuses the existing raw-transaction retention policy and requires no schema change. The adapter still withholds transparent input values for blocks containing shielded or unclassified transactions when unchanged Cipherscan would calculate a false partial fee. Block solution, pool branding, shielded value balances, and absent canonical fact rows remain unavailable rather than fabricated. |
| `/api/tx/:txid` | For coinbase transaction `013635f7...790456` in block 4,156,556, the adapter returns public-testnet-equivalent `totalOutput: 1.37565`, both transparent output values, scripts, and addresses; both unchanged UIs render `No inputs` and the same two output rows. For shielding transaction `fe18003a...34852f2`, the corrected reader recovered all nine 125,000,000-zatoshi values and identical P2PKH scripts from retained parent transaction facts, returned `RESOLVED`, kept `paid_fee_zat` absent, and rendered the same nine `tmFU5A...gnDd` address links, 1.25-unit values, and 0.00055 fee as public Cipherscan. Native `TransactionDetail` reports `explorer.transaction.detail_v3`, combines each ordered input's outpoint with independently optional value and script fields, and returns signed intrinsic pool balances when the canonical artifact or retained blob is available at the pinned epoch. The adapter maps those balances into Cipherscan's existing ZEC `valueBalanceSapling`, `valueBalanceOrchard`, and `valueBalanceIronwood` fields, so the unchanged page classifies shielding, unshielding, mixed, and transparent transactions without a synthetic API type. It also joins each canonical output to the existing reverse-spend relation at the same complete chain epoch. For transaction `67fc642f...b1db1a`, local output 0 is correctly `spent: true` by `fe18003a...34852f2` and output 1 is `spent: false`; public testnet incorrectly reports both false. Both unchanged UIs still render `No inputs`, the same two addresses, and 1.25/0.125 values without console errors because this Cipherscan checkout does not display the spent flag. The deployment preserved the Docker volume created at `2026-07-05T22:52:43Z`; ingest remained untouched while query and explorer were recreated as reader services only. The Cipherscan route intentionally returns `404 { "error": "Transaction not found" }` for mempool or conflicting locations so unchanged Cipherscan can probe `/api/mempool/tx/:txid`. | Complete for transparent input identity, historical values, standard input-address decoding, canonical output spent state, intrinsic pool-balance availability, and fee semantics without clearing or replaying a projection, changing canonical storage, or calling Zebra from the explorer. Reverse-spend requests are deterministically chunked at 1,024 outputs, every response must match the complete transaction epoch, retention-swept misses fail closed without derive coverage, and malformed native output/spender rows are rejected rather than rendered as zero values. A 1,025-output acceptance test proves a spender in the second batch. The storage-only fee input message keeps the same wire fields, so no persistent schema upgrade is involved. Missing parent scripts and unavailable intrinsic artifacts remain independently unavailable without erasing retained values. For `2b7a2e11...ea6597`, this Cipherscan checkout still lacks the deployed public site's Ironwood-only summary branch; that client drift remains a Cipherscan TODO. |
| `/api/transactions/list`, `/api/tx/shielded` | `ExplorerQuery.TransactionHistory` serves canonical materialized history with compound native filters, optional exact totals, opaque native cursors, and stateless height/index anchors. The additive v2 response carries a read fence `(chain_epoch_id, projection_revision, projection_tip_height, projection_tip_hash)`, verified contiguous coverage, and count scope. The shielded adapter preserves Cipherscan's numeric, enum, and `skip_count` validation envelopes; it forwards the fence across offset scans, rejects fence drift, binds count-cache keys to the fence, and accepts a total only when the native response marks it `FULL_HISTORY`. Small first pages and `skip_count=true` retain Cipherscan's zero-total optimization. Retained Sapling, Orchard, and Ironwood intrinsic balances are mapped as signed ZEC values. Paid fee remains null when unproven, and the ZIP-317 conventional fee is additive. Live proof on the preserved deployment exercised exact totals, filters, offset pagination, and unchanged `/privacy` rendering without replacing the Docker volume. | Complete for the rendered transaction-list and shielded-history workflows through product-neutral `explorer.transaction.history_v2`. Consumer schema v3 preserves schema-v1/v2 rows, the stable `recent_transactions` column family, and the existing cursor in place. A resumable canonical verifier advances coverage from height 1 in bounded batches and retains the last durable boundary on interruption or mismatch. Transaction rows, projection state, and chain-event cursor advance atomically during normal dispatch. One derive-store snapshot supplies rows, projection metadata, fee joins, and exact counts; a secondary catch-up barrier prevents that snapshot from straddling a RocksDB secondary refresh. Full-history totals are exposed only when verified coverage reaches the fenced projection tip and hash. This proves native completeness and adapter pagination semantics, not pixel-level parity for every Cipherscan client version. Cipherscan's legacy `min_actions` predicate omits Ironwood and Sprout, while Zinder intentionally applies the threshold to every shielded protocol. |
| `/api/address/:address` | `ExplorerQuery.TransparentAddressActivity` v2 now combines the active ranking generation's confirmed balance and lifetime summary with offset-paged activity and epoch-pinned retained transaction facts. For `tmA4rv...BvGb`, local and public testnet match exactly on 1,387,111.6122294 ZEC balance, 1,387,616.81791593 ZEC received, 505.20568653 ZEC sent, 308,522 transactions, first/last timestamps, page totals, and every normalized row on pages 1 and 2. Receiving address `tmFrs...dPLE` and shielding address `tmYLY...4dRL` also match public row-for-row, including values, component flags, and standard counterparties. Valid unused, invalid, and shielded address responses match public status and body semantics. The unchanged local UI renders 308,522 transactions, first/last activity, page 1/12,341, working page-2 navigation, block rewards, real counterparty links and amounts, and the shielded privacy screen. A steady-state probe stayed non-degraded across the tip transition from 4,159,138 to 4,159,139; five prior reads completed in 15-55 ms, while public completed in 85 ms. | Complete without a new projection, schema migration, replay, backfill, canonical-volume replacement, or data wipe. The adapter deliberately uses the activity response's own summary and epoch instead of combining independent wallet-query and explorer reads; the discarded balance-first design produced a live 15-second `502` when the readers differed by one block. The explorer resolves the activity epoch from its local canonical secondary, rejects a ranking newer than that epoch, rechecks generation metadata around the summary read, and reports an indexed-tip mismatch as degraded. Counterparty labels remain sidecar-owned. The final deployment kept ingest container `1a531cad9327` and the volume created at `2026-07-05T22:52:43Z`; only the explorer reader and adapter were replaced. |
| `/api/mempool` | Returns Cipherscan's empty-mempool success envelope locally; unchanged Cipherscan renders the zero-state at `http://localhost:3003/mempool` without application errors. With Zebra's private indexer listener enabled, live v6 testnet transactions have produced coherent non-empty snapshots and unchanged-Cipherscan list, count-bubble, and transaction-table states. Transaction `02255e8b...a32536` was observed with `count: 1`, `showing: 1`, 2,878 bytes, six transparent inputs, two Sapling outputs, and a 40,000-zatoshi ZIP-317 conventional fee. `ExplorerQuery.MempoolSnapshot` supplies the global summary and parsed transparent/Sapling/Orchard/Ironwood rows from one `WalletQuery.MempoolSnapshot` response, so a mine cannot leave a stale count beside an empty page. Its positive projection and adapter mappings are tested. During the latest comparison, public testnet's API returned `500 { "success": false, "error": "RPC request failed: socket hang up" }`. | The current overview is complete without a chain-store schema change, data wipe, or replay. Per-transaction intrinsic transparent outputs are now covered by `TransactionDetail` v3 rather than duplicated into the overview row. |
| `/api/mempool/tx/:txid` | Returns Cipherscan's invalid-txid and absent-from-mempool shapes locally. Native `explorer.transaction.detail_v3` parses the transient payload through the same `TransactionPublicFactSet` source parser ingest uses, validates its txid, and fills the existing ordered transparent input/output messages without persistent state. While mixed v6 transaction `2a0cdebc...fc83435b` was pending, native detail returned one 618,759,900-zatoshi output and its exact P2PKH script; the adapter returned `totalOutput: 6.187599` and `tmUcufCr...c2KmSw`. Unchanged local Cipherscan rendered `Pending in Mempool`, the exact transparent value, and a linked output address, while public Cipherscan rendered `Transaction Not Found` because both public mempool endpoints were failing. The confirmed-only local `/api/tx/:txid` simultaneously returned the required `404`; Cipherscan logs that initial miss before executing its successful fallback. After the transaction mined in block 4,158,330, the same page automatically transitioned to the confirmed view. The ingest container remained `a48f26050675`, and the existing volume retained its July 5 creation timestamp. | Complete for membership, parsed counts, and intrinsic transparent output values/scripts/standard addresses without durable storage, replay, or a data wipe. Ordered mempool input outpoints are native but parent values/scripts remain absent; actual fees and shielded value balances are not inferred. Nonstandard output scripts retain their bytes and map to `address: null`. |
| `/api/tx/broadcast` | Accepts Cipherscan's documented `rawTx` request field. Missing and non-string values now preserve the live public API's Zod 4 detail text; empty and non-hex values retain Cipherscan's custom validation message. A deliberately invalid byte payload reaches Zinder and returns its typed `invalid_encoding` rejection. A signed, extractor-verified Ironwood testnet transaction (`262b2b1a...51017e0`) returned `200 { "success": true, "txid": ... }` with the exact wallet-computed id and then mined at height 4,157,365. Replaying it returns `400`, `success: false`, `duplicate: true`, no `txid`, and `reason: "duplicate"`, so unchanged Cipherscan cannot navigate to `/tx/undefined`. The live replay exposed two current Zebra duplicate spellings (`-1` with `already exists in mempool`, and `-25` with `transaction is already in state`); `zinder-source` now classifies both at the node boundary, with integration regressions. CORS preflight succeeds. The unchanged local and public Broadcast pages render the same form; local renders without console errors. Public testnet's broadcast API currently returns `400 { "success": false, "error": "RPC request failed: read ECONNRESET" }` even for the already-mined transaction, so it cannot provide a positive reference today. | Complete without a schema migration, projection replay, backfill, or data wipe. Accepted, duplicate, queued, invalid, rejected, unknown, and missing native outcomes have explicit adapter tests. A real queued result remains timing-dependent; unchanged Cipherscan must receive it as `success: false` because the native queued outcome has no transaction id. Browser automation could populate the controlled textarea visually but did not dispatch React's state update, so the success banner itself remains source-contract-covered rather than automation-proven; the CORS contract and mined transaction are proven independently. |
| `/api/tx/raw/batch` | Matches public Cipherscan's validation errors for missing, empty, and invalid `txids`, and returns Cipherscan's `transactions` plus per-item `failed` shape. With testnet ingest configured as `raw_blob_policy = "transactions"`, extractor-verified Ironwood transaction `2b7a2e11...ea6597` returned its exact 11,134-byte signed payload while pending and after it mined at height 4,157,777. The single, verbose, and batch routes all returned the same 22,268-character hex; batch reported `total: 1`, `successful: 1`, and no failures before and after confirmation. The mempool point lookup transitioned from `inMempool: true` to `false` while canonical raw lookup remained `200`. Public Cipherscan indexed the same transaction but its raw route returned `500`, and its batch route returned `successful: 0` with `RPC request failed: socket hang up`. | Complete for newly ingested transactions without a schema migration, projection replay, backfill, volume recreation, or data wipe. Compose now passes the existing `ZINDER_STORAGE__RAW_BLOB_POLICY` setting, and the testnet helper selects `transactions`; mainnet retains the `none` default. Transactions mined before the policy change remain unavailable unless operators deliberately replay or reingest them. |
| `/api/scan/orchard`, `/api/lightwalletd/scan` | `/api/scan/orchard` now returns the canonical Orchard candidate coordinates used by Cipherscan's recent client-side scan. It accepts only heights, rejects viewing-key fields, constrains the request to the unchanged client's seven-day maximum, and walks `TransactionHistory` v2 under one read fence with verified coverage. Before returning a candidate set, it proves that every candidate has retained raw bytes at the same chain epoch; otherwise it returns explicit `503 candidate_scan_unavailable` so the unchanged client cannot silently misclassify missing bytes as “not my transaction.” Live 1h, 6h, and 24h windows returned complete empty sets in 16-20 ms. The preserved deployment's 7d window correctly remains unavailable because one transaction predates raw-byte retention. `/api/lightwalletd/scan` retains Cipherscan's validation envelopes and explicit `503`. | Complete for bounded recent Orchard candidate discovery without a schema migration, replay, volume replacement, viewing-key transfer, or server-side decryption. `TransactionHistory` owns canonical Orchard filtering, ordering, the read fence, and coverage; the adapter owns the Cipherscan range and JSON contract; Cipherscan fetches retained bytes and decrypts locally. The maximum candidate set is bounded to the existing raw-batch contract. Seven-day availability warms up naturally under `raw_blob_policy = "transactions"`; historical missing bytes are never fabricated. Birthday compact-block scanning remains a wallet/lightwalletd streaming integration and does not enter explorer core or this materializing REST route. |
| `/api/tx/:txid/verbose` | Matches Cipherscan's invalid-txid and not-found shapes. For retained transaction `2b7a2e11...ea6597`, the adapter returned Cipherscan's `{ txid, hex, decoded }` envelope before and after confirmation; `hex` exactly matched the signed payload, while `decoded.degraded: true` and its `unavailable` list state that Zebra verbosity-1 JSON is not a native Zinder contract. The unchanged local Cipherscan Raw tab rendered the decoded JSON, Hex, Decoded JSON, and Copy Hex controls and reported 11,134 bytes and 22,268 hex characters. Public Cipherscan rendered the same confirmed transaction page, but its Raw tab displayed `Failed to load raw transaction data.` because its verbose API returned `500`. | Complete for retained bytes without inventing a reusable decoded-JSON contract. No schema migration or data wipe is required. A future Cipherscan-native client can decide whether individually named native transaction facts remove its need for Zebra verbose JSON. |
| `/api/shielded/list` | Renders real canonical flow rows with numeric ZEC amounts, typed direction/pool labels, exact totals, and disjoint Next/Prev pagination. The adapter maps native opaque cursors to Cipherscan's block-time plus stable numeric event-id pair. `addresses` remains an empty array with explicit unavailable metadata. Unchanged `/txs/shielded` rendered 25 rows, advanced to page 2, and produced zero console errors. The newest shared local/public rows matched on txid, height, time, direction, amount, and pool. | Complete for chain-derived flow facts and pagination. Transparent address joins remain follow-up work; public's PostgreSQL serial id is storage-specific and is not reproduced. |
| `/api/pools/overview`, `/api/network/pool-history` | Returns current Sprout, Sapling, Orchard, Ironwood, transparent, shielded, and total chain-supply balances from one hash-bound `ExplorerQuery.ValuePoolSummary` read, plus exact calendar-day history and 24h/7d/30d deltas from `ValuePoolBalanceHistory`. `chainSupply` includes Lockbox while Cipherscan's fixed composition remains limited to its five named pools. On 2026-07-11, local and public `7d` history had the same 8 inclusive UTC dates and identical point keys. Public was an older cached snapshot and omitted Ironwood deltas; local exposed the source-backed `+15.6K 7d` Ironwood change. The unchanged local Supply History chart rendered populated axes and all pool cards rendered current values and 7d changes. Zebra verbose `getblock` for local height 4,160,828 and hash `0002ad15...67498d` matched the native point's height, hash, time, chain supply, and every dynamic pool value exactly. | Complete through optional canonical artifact schema 15 and additive derive history without a wipe. The existing volume retained its `2026-07-05T22:52:43Z` creation time; the pre-open rollback checkpoint is `backups/cipherscan-value-pool-balance-schema14-20260711T223111Z`. Historical catchup resumed durably and scanned heights 1 through the settled boundary while fetching only daily candidates. A live overlap failure proved that rows moving from replaceable-tail ownership to historical ownership must be validated and adopted, not rewritten; the regression test covers ownership transfer, restart idempotence, and a later tail reorg. Adapter availability uses explicit contiguous historical-plus-tail ranges, so ordinary one-block synchronization lag does not erase complete calendar history. All prior pool, network, migration, and transaction probes remained 200. Turnstile remains separately degraded. |
| `/api/privacy-stats` | Returns exact all-time and UTC-day Sapling/Orchard/Ironwood transaction predicates, current/daily pool balances, mixed and fully-shielded counts, six-decimal percentages, two-decimal rolling average, score, and adoption trend. At local height 4,161,333 it returned 4,771,568 total, 472,197 shielded, 469,067 transparent, 4,161,333 coinbase, 57,991 mixed, and 36,395 fully shielded transactions; the public 03:00 snapshot at height 4,161,267 returned 4,771,460, 472,103, 469,066, 4,161,266, 57,991, and 36,390. Both returned score 10, 9.9% shielded adoption, and a declining trend. Cold local aggregation completed in 32 ms. Local daily rows are exact UTC buckets: public's current-day row is instead the production job's rolling 1,153-block approximation, while its historical backfill excludes Ironwood and drops `23:59:59`; those known database defects are intentionally not reproduced. The unchanged local and public Privacy pages both rendered `10/100`, 9.9%, Declining, 12.4% shielded supply, the same pool composition, populated recent activity, and all trend charts. Local console errors remained limited to separately unavailable labels and Crosslink routes. | Complete through product-neutral `TransactionComponentSummary` v2 predicates and adapter-owned product formulas. Derive schema 2 rebuilt only this consumer in place; the coordinated checkpoint is `backups/cipherscan-transaction-components-v1-20260712T025745Z`, the backfill completed through settled height 4,161,166, every service reopened healthy, and volume `zinder-testnet-data` retained its `2026-07-05T22:52:43Z` creation time. The native contract uses protocol-scoped names and `transaction_predicate_unavailable_count`; the adapter requires `explorer.transaction.component_summary_v2` before consuming scalar counters. A bounded whole-document epoch retry eliminated the observed secondary-transition race: 240 requests spanning a tip transition returned 240 HTTP 200 responses with 173 ms maximum latency. Product score, trend thresholds, display rounding, and JSON remain outside Zinder. A best-effort WebSocket `privacy_stats` frame reuses the same REST body. |
| `/api/analytics/anonymity-set` | Returns Cipherscan's exact period coercion, 16 ascending amount thresholds, cumulative shield/deshield counts, timestamp shape, and one-hour response cache. One `ExplorerQuery.ValuePoolFlowAmountThresholdSummary` scan folds the existing canonical flow projection without adapter paging or a second persisted aggregate. Initial live comparison exposed shielded coinbase issuance in legacy projection rows: local 30-day shielding was `27,421` versus public `2,566`, with an identical `24,855` excess at every threshold through 1 ZEC and no excess from 2 ZEC upward. Zinder now rejects coinbase transactions on new writes and excludes existing transaction-index-zero rows from history, daily summaries, and threshold summaries. After the fix, the same 30-day rows were `2,560` local versus public's older cached `2,566`; all 16 rows differed by at most 6 shield and 3 deshield events. Local and public daily flow rows also matched exactly on the previously affected dates: June 29 `1/2`, June 30 absent, and July 1 `15/0` shield/deshield events. The unchanged Privacy page renders the populated chart and switches between 7D and ALL without a fetch or console error. | Complete without a schema migration, projection replay, backfill, volume recreation, or wipe. Existing coinbase rows remain physically present but are structurally identifiable by their consensus-defined transaction index and ignored; future writes omit them. Seven-, 30-, 90-, and 365-day comparisons are near public cache parity. Zinder's `all` result remains larger because it has contiguous genesis-to-tip coverage, while public Cipherscan's `shielded_flows` history is incomplete and its archived backfill resumes above the current maximum height rather than repairing lower holes. Exact parity with that incomplete table is intentionally not fabricated. |
| `/api/analytics/shielding-distribution` | Returns Cipherscan's exact 10 ascending `[min,max)` amount buckets, labels, nullable final upper bound, shield/deshield counts, zatoshi volumes, period coercion, timestamp shape, and one-hour cache. The adapter requests the 10 bucket lower bounds from `ValuePoolFlowAmountThresholdSummary`; each closed bucket is the checked difference between adjacent cumulative counts and sums, while `1000+` uses the final cumulative row directly. Live 7-day and 30-day comparisons matched every public shielding bucket count and volume exactly. At 30 days both returned `2,559` shields and `5,623,027,318,924` aggregate zatoshi; local deshields differed only by four events across the moving two-minute public-cache boundary. The unchanged Privacy page renders all 10 labels, COUNT and VOLUME modes, and ALL period data; `/charts` now renders the populated Shielding Distribution preview instead of `No data`. | Complete without a new native contract, schema migration, projection replay, backfill, volume recreation, or wipe. It reuses `explorer.value_pool.flow_amount_threshold_summary_v1`, its complete-coverage check, and the same legacy-row coinbase exclusion as Anonymity Set. Unknown nonempty periods are echoed and use the 30-day range, matching Cipherscan. Longer-window and `all` differences retain the documented source distinction: Zinder has contiguous canonical history, while public Cipherscan does not. |
| `/api/blend-check`, `/api/blend-check/split` | Returns Cipherscan's exact amount parsing, validation, period-count, score, label, nearby-popular, and split-plan shapes. The adapter preserves the fixed ±10,000-zatoshi match tolerance, 0.01-ZEC nearest-quantum discovery, score boundaries, greedy split policy, 12-piece limit, and five-minute bounded cache. On 2026-07-12, local and public matched every 24-hour, 7-day, and 30-day count, all nearby rows, and all split plans for `1.25`, `10`, `7.31924`, `0.01`, `0.000001`, the positive amount that rounds to zero zatoshi, and 21 million ZEC. For 10 ZEC, both returned score 25 and the recommended four-piece 2.5-ZEC plan with minimum and weighted score 65. Unchanged local Cipherscan rendered the same score, period cards, nearby list, and split plan without a Blend request error. | Complete without a schema migration, replay, backfill, volume recreation, or wipe. `ExplorerQuery.ValuePoolFlowRoundedAmountSummary` performs one bounded read-time frequency fold over the existing canonical flow projection; `ValuePoolFlowAmountThresholdSummary` supplies exact rescoring in batches. Both require complete coverage and one coherent chain epoch. Zinder exposes only reusable counts. The adapter owns Cipherscan's product score, labels, denomination policy, split recommendations, response cache, and REST names. `all` counts intentionally reflect Zinder's complete canonical history and can exceed public Cipherscan's incomplete legacy table. |
| `/api/privacy/common-amounts` | Returns Cipherscan's no-chain ranked 0.01-ZEC denomination groups, combined shield/deshield event counts, full-window denominator, one-decimal percentage strings, bounded blend scores, period fallback/echo behavior, and request-correct limit handling. It uses the same nearest-quantum native summary as Blend Check plus one cumulative threshold row for the denominator, with complete coverage and matching chain epochs required. On 2026-07-12, local and public produced the same six Popular pills in the unchanged 7-day Privacy Risks page: 62.50, 1.25, 2.50, 3.71, 5.00, and 3.75 ZEC. The 24-hour, 7-day, 30-day, and 90-day API rows tracked public within the moving-window block delta. | Complete for the no-chain surface without a schema migration, replay, backfill, volume recreation, or wipe. The adapter owns percentages, scores, the 0.01-ZEC policy, 15-minute bounded cache, and JSON. It intentionally keys cache entries by period and limit rather than copying Cipherscan's Redis bug that returns a prior request's row count. Nonempty `chain` remains explicitly degraded because `chainSwapCount`, source amounts/tokens, and dual scores require the external cross-chain sidecar; public testnet currently returns `500` because that table is absent. |
| `/api/tx/:txid/linkability`, `/api/privacy/linkage-edges`, `/api/privacy/batch-risks`, `/api/privacy/clusters`, `/api/privacy/graph/:txid`, `/api/privacy/shield/:txid/batch`, `/api/privacy/patterns`, `/api/privacy/recommended-swap-amounts` | Matches Cipherscan's validation errors for invalid transaction IDs and missing recommendation query parameters, returns stable empty degraded graph, linkage, cluster, batch-risk, and recommendation envelopes, and renders the local Privacy Risks page's Round Trip and Batch Patterns modes without fetch errors. Public testnet currently returns `500` for several linkage routes because sidecar tables are unavailable. | Real linkage, identity risk, and swap recommendations require Cipherscan shielded-flow and cross-chain sidecars; rejected from Zinder core. |
| `/api/network/protocol-stats`, `/api/stats/shielded-count`, `/api/stats/shielded-daily`, `/api/analytics/usage-clock` | The three protocol/component routes now read `ExplorerQuery.TransactionComponentSummary` and return complete Cipherscan-compatible current, monthly, detailed, and daily shapes with additive coverage metadata. The native projection keeps exact half-open time semantics, UTC-day aggregates, and Cipherscan's explicitly named legacy Sapling/Orchard classification without treating Ironwood or Sprout as legacy shielded activity. On 2026-07-11, local and public fixed daily results matched exactly at `469`, `484`, and `442` transactions (total `1,395`); simultaneous detailed reads matched all counts and first/last timestamps exactly. The local Network page renders populated protocol trees and history with no loading or empty-data state. `/api/analytics/usage-clock` remains backed by the bounded no-schema `BlockActivityDistribution` contract. | Complete through additive derive consumer schema v1, resumable canonical backfill, cursor-neutral visible-tail startup seeding, reorg rollback, checkpoint inclusion, and explicit contiguous coverage. The preserved volume was repaired in place in 280 ms, retained its `2026-07-05T22:52:43Z` creation time, and required no canonical schema change or wipe. Public protocol history matches through January 2026; from February onward its PostgreSQL sums undercount the consensus-visible tree sizes by 24 Sapling and 129 Orchard commitments, while local totals equal `ChainEpoch` tree sizes and both sides match Sapling nullifiers and Ironwood actions. Zinder keeps the consensus-correct values. |
| `/api/search/anchor/:root` | Returns canonical Sapling, Orchard, and Ironwood post-block root matches newest-first, with explicit historical coverage and the existing Cipherscan diagnosis shape. Each canonical match is joined at the adapter edge to the existing canonical miner-payout fact. Invalid roots preserve public Cipherscan's exact `400` shape. The schema-16 deployment now exposes native displaced positives and activation coverage; the adapter maps only those positives into `orphaned`, preserves null displaced miner identity, and keeps both positive and empty displaced results degraded because pre-activation history is unknown. Live API proof covers all three canonical protocols and an unknown root. The unchanged UI renders linked Sapling and Ironwood canonical rows and the activation-limited unknown diagnosis without route errors. Its hard-coded empty-state bullets still mention Orchard backfill and remain a Cipherscan TODO. Public testnet's anchor API returned `500 { "error": "Failed to search anchor root" }` for the same valid root. No post-activation displacement has occurred, so a live displaced positive does not exist yet. | Complete for canonical search through the schema-13 artifact and `CommitmentRootSearchConsumer`, and activation-ready for displaced-root capture through schema 16. The writer reads the displaced block's retained final-root artifact and writes the archive row, reverse root indexes, and coverage counters atomically with `ReorgWindowChange::Replace`. Schema 16 was deployed writer-first in place against the preserved volume created at `2026-07-05T22:52:43Z`; the pre-open schema-15 rollback checkpoint is `backups/cipherscan-displaced-roots-schema15-20260712T053839Z`. The active capability is `explorer.commitment_root.displaced_matches_v1`. Coverage remains unactivated with zero captured blocks until the next replacement event. Full historical orphan parity is not claimed. |
| `/api/rich-list` | Returns and renders the active transparent-address ranking with exact Cipherscan pagination, decimal Unix-second strings, lifetime totals, and concentration metrics. The unchanged local page renders 100 rows and advances to ranks 101-200 and back without a rich-list fetch error. On 2026-07-11, local and public rows 1-8 and 10-12 matched every value. Public row 9 was 55.625 ZEC high; the local 130,320.385 ZEC balance matched the feeding Zebra node's canonical `getaddressbalance` exactly, so Zinder intentionally preserves the canonical value. | Complete through `ExplorerQuery.TransparentAddressRanking` and an additive derive consumer. The preserved testnet volume retained its `2026-07-05T22:52:43Z` creation time. Initial activation produced 316,883 positive addresses without rewriting canonical data; a routine restart reopened storage in 314 ms and reused the active generation without rebuilding it. |
| `/api/labels`, `/api/label/:address` | Returns explicit `503` unavailable responses so an absent label registry is not cached as authoritative empty data. | Cipherscan-owned label sidecar. |
| `/api/price`, `/api/price/at` | Current price returns real ZEC/USD price and 24-hour change with Cipherscan's exact three-field success body and millisecond timestamp. A reusable service-owned HTTP client applies a five-second timeout, 16 KiB response bound, 60-second fresh cache, coalesced refresh, and bounded 15-minute stale fallback. Historical lookup uses a separately configurable endpoint template, validates Cipherscan's lexical date shape, returns positive finite completed-day prices rounded to the legacy table's four decimals, caches at most 1,024 immutable successes, and clamps today/future requests to the latest completed UTC day with `actual_date`. The default CoinGecko public source covers only the latest 365 days; older requests fail explicitly rather than claiming absent data. | External market transport at the adapter edge. A durable sidecar or provider with deeper coverage is still required for complete pre-365-day history. |
| `/api/network/peers` | Matches public Cipherscan's empty peer-list shape while marking peer inventory as degraded sidecar/RPC state. | Direct node peer inventory is not a Zinder core chain fact. |
| `/api/network/nodes`, `/api/network/nodes/stats`, `/api/network/node-history` | Returns Cipherscan's aggregated node-location, node-statistics, and node-history envelopes with empty degraded location, trend, country, and snapshot lists. | GeoIP/node crawler data is sidecar-owned and not a Zinder core chain fact. |
| `/api/network/mining-metrics` | Returns Cipherscan's ascending rolling series for solrate, difficulty, block time, transaction fees, and transaction count from one `ExplorerQuery.BlockProductionSeries` read. The adapter preserves Cipherscan's JavaScript-style query coercion, clamps `window` to 5 through 100 and `limit` to 20 through 500, and reports requested, covered, and missing block counts. A live comparison on 2026-07-10 found 98 aligned steady-state points at heights 4,157,114 through 4,157,211: block time and transaction count matched exactly, while the largest floating-point differences were `2.84e-14` for difficulty and `3.55e-15` for solrate. Fee averages differed by at most `1.5e-9` ZEC because local rows explicitly use ZIP-317 conventional fee floors instead of claiming actual paid fees. The sampled local response covered all 120 requested blocks and remained explicitly degraded because all 120 fee samples were conventional floors. CORS preflight is proven. In an unchanged manually loaded local `/mining` page, all five cards rendered live values (`2.10 H/s`, `19.2`, `~22s`, `0.000188 ZEC`, and `1.1`), and the Solrate Trend chart exposed populated block-height and `H/s` axes without a visible failure state. | The bounded series joins existing block summaries with canonical header difficulty bits at read time, so it needs no projection, schema upgrade, replay, backfill, or data wipe. Cipherscan owns rolling-window presentation and legacy solrate semantics; pool branding remains sidecar-owned. |
| `/api/mining/pool-distribution` | Returns Cipherscan's pool-distribution envelope with empty degraded pool rows and the requested period preserved. | Pool labels and attribution are Cipherscan sidecar data and must stay out of Zinder core. |
| `/api/mining/pool-ranking` | Returns Cipherscan's pool-ranking table envelope with empty degraded ranking rows and the requested period preserved. | Pool metadata, regions, URLs, and attribution are Cipherscan sidecar data and must stay out of Zinder core. |
| `/api/mining/hashrate-share` | Returns Cipherscan's pool-share time-series envelope with empty degraded series and the requested period preserved. | Pool dominance and attribution are Cipherscan sidecar data and must stay out of Zinder core. |
| `/api/mining/miner-behavior` | Returns Cipherscan's computed-later fallback envelope with empty degraded series, null summary, and the requested period preserved. | Miner sell/hold behavior and destination classification are Cipherscan sidecar analytics and must stay out of Zinder core. |
| `/api/mining/zodl-leaderboard` | Returns Cipherscan's computed-later fallback envelope with empty degraded pool rankings, null summary, and the requested period preserved. | ZODL scoring and reward-destination classifications are Cipherscan sidecar analytics and must stay out of Zinder core. |
| `/api/mining/rewards` | Pages backward through epoch-pinned canonical block-production rows and applies Cipherscan's exact wall-clock cutoff before producing daily `blocks`, `totalFeesZat`, and `totalCoinbaseZat`. A 50,000-block bound covers the observed 24-hour, 3-day, and default 7-day testnet windows without assuming target block spacing; longer periods return their covered rows with explicit incomplete coverage. The adapter ignores the provisional `limit` extension because it is not part of Cipherscan's route, caches each period for five minutes, declares `transparent_outputs` as the coinbase basis, and marks ZIP-317 fee fallback degraded. On 2026-07-11, the local 7-day scan covered 28,299 blocks in 3.8 seconds. Full UTC days July 5-10 matched public Cipherscan exactly for block counts and transparent coinbase totals; moving cutoff/tip days differed only with request/cache time, except a declared 10,000-zatoshi transparent-coinbase difference on the first cutoff day. | Complete for bounded recent reward aggregation without a schema change, replay, backfill, or data wipe. Actual shielded transaction fees remain unavailable, so current rows truthfully use ZIP-317 conventional fee floors. `30d` through `all` remain explicitly partial when they cross 50,000 blocks. A product-neutral daily chain-reward projection is warranted only if complete long-range history becomes a supported native requirement. The current Cipherscan mining page does not consume this route; unchanged local `/mining` completes its active requests without application errors. |
| `/api/crosslink`, `/api/crosslink/bft-chain`, `/api/crosslink/bft-tip`, `/api/crosslink/fork-monitor` | Renders Crosslink pages and Fork Monitor with PoW tip, safe-tip finality approximation, local anchor hashes, and empty BFT/finalizer/cTAZ registry data. | Native Crosslink consensus/finality surface if Zinder becomes owner; Cipherscan sidecar for cTAZ comparison and community node registry. |
| `/api/crosslink/bootstrap-info` | Matches public testnet's no-snapshot state with `success: true` and `available: false`, plus explicit degraded metadata so the local `/bootstrap` page renders the friendly unavailable state. | Bootstrap archive metadata is Cipherscan deployment sidecar data and must not become Zinder core chain data. |
| `/api/crosslink/divergence-history` | Returns Cipherscan's divergence-history envelope with `count: 0`, `openEvent: null`, an empty event list, and explicit degraded metadata so the local `/chain` page can render without a missing-route fetch. Public testnet currently returns `500` because its divergence sidecar table is unavailable. | Divergence history is Cipherscan sidecar telemetry unless Zinder explicitly accepts a native Crosslink consensus/finality surface. |
| `/api/finalizers`, `/api/finalizer/:pubkey`, `/api/finalizer/:pubkey/participation`, `/api/crosslink/participation` | Returns Cipherscan's finalizer validation errors, empty finalizer roster, finalizer not-found response, and empty participation windows with explicit degraded metadata. The local `/validators` page renders an empty roster without error; `/finalizer/:pubkey` renders Cipherscan's not-found state. Public testnet currently returns 500 for finalizer list and participation because its backing finalizer tables/RPC are unavailable. | Finalizer roster, stake actions, and BFT signer participation are Crosslink product/consensus data. Keep them sidecar-owned unless Zinder explicitly accepts a native Crosslink finality surface. |
| `/api/crosschain/stats`, `/api/crosschain/inflows`, `/api/crosschain/outflows`, `/api/crosschain/status`, `/api/crosschain/db-stats`, `/api/crosschain/trends`, `/api/crosschain/history`, `/api/crosschain/volume-by-chain`, `/api/crosschain/address/:address`, `/api/crosschain/popular-pairs` | Returns Cipherscan's cross-chain analytics envelopes with zero totals, empty flow/history/pair/chain arrays, preserved `period`, `granularity`, `page`, `limit`, and `address` fields, and explicit degraded metadata. The local address page can probe cross-chain activity without a missing-route failure. | Cross-chain swap data belongs to Cipherscan's NEAR Intents and bridge sidecars; rejected from Zinder core. |
| `/api/name/:name`, `/api/name/:name/events` | Returns Cipherscan's unregistered-name pricing envelope and empty event-history envelope with explicit degraded metadata. Testnet/mainnet pricing tiers mirror the public ZNS unavailable-name shape, but real registration, listing, and event data remains absent. | ZNS registration and listing data belongs to the Cipherscan ZNS sidecar; rejected from Zinder core. |
| `/api/supply`, `/api/circulating-supply` | Return real local value-pool rows and current chain supply. Public testnet production currently returns 500 for these routes because its Zebra RPC supply read is unavailable. | Current supply is complete. Historical pool balances and daily chain-supply deltas are served by the completed `ValuePoolBalanceHistory` surface. A separate subsidy-history projection is needed only for consumers that require historical reward-allocation facts rather than observed chain supply. |
| `/api/supply/transparent-breakdown` | Joins one active ranking generation to the source-backed transparent value pool. It reports exact testnet P2PKH/P2SH counts and balances, the standard-address total, and the unattributed transparent remainder. At height 4,159,068, local `transparentTotal` exactly matched Zebra; the unchanged Network page rendered `277,785` P2PKH addresses at 76.9% and `39,098` P2SH addresses at 21.9%. Public Cipherscan returned a zero denominator and classified all testnet addresses as `other` because its implementation applies mainnet prefixes and its Zebra pool read is unavailable. | Complete for standard script classification and current transparent accounting. Category labels remain explicitly sidecar-owned, so the response retains a degraded label marker while its chain-derived fields are complete. Ranking schema 2 rebuilt only that derive consumer in 35 seconds, preserved the volume created at `2026-07-05T22:52:43Z`, and a routine restart reused the active generation without another rebuild. |
| `/api/network/fees` | Matches public Cipherscan's ZIP-317 estimate contract for `fees`, `unit`, `zip317`, and `note`, and adds `observedZip317` with Zinder's latest 256-block conventional-fee summary. | Daily conventional-fee percentiles and actual paid-fee aggregates remain separate native/projection work. |
| `/api/pools/overview` | Returns Cipherscan's pool-overview envelope with real current pool balances from the hash-bound `ValuePoolSummary` and exact 24h/7d/30d deltas from `ValuePoolBalanceHistory`. Known zero balances remain zero; absent balances remain `null`; duplicate pool identifiers and negative upstream balances fail closed. A valued future pool participates in chain supply, while an unvalued monitored pool makes aggregate supply unavailable instead of silently understating it. The hardened adapter was deployed against the preserved `zinder-testnet-data` volume created at `2026-07-05T22:52:43Z`; pool overview and migration remained available, and network stats plus transparent breakdown were stable across five consecutive same-tip probes. | Complete for current balances, calendar deltas, daily chain-supply history, and observed daily emission through the completed native history projection. Historical subsidy allocation remains a distinct chain-economics concern. |
| `/api/pools/flows` | Returns real UTC hour/day flow buckets from `ValuePoolFlowSummary`, preserving period, pool, granularity, numeric ZEC, and string-zatoshi modes. Daily compatibility includes the full UTC cutoff day like Cipherscan's `flow_daily`; hourly mode keeps the exact rolling cutoff. The unchanged `/pools` page rendered a populated 30-day chart with Shielded, Deshielded, and Net Flow series. | Complete for neutral flow aggregation. Public testnet has historical indexing gaps: local/public full-day Sapling, Orchard, Ironwood, and Mixed buckets match where public coverage exists, while Zinder also retains older Sprout flows and pre-public-backfill rows. Zinder does not reproduce those omissions. |
| `/api/network/pool-history` | Returns complete cumulative pool-balance history and calendar deltas from `ValuePoolBalanceHistory`, preserving Cipherscan chart metadata and explicit contiguous coverage. | Completed additive native projection; the adapter owns Cipherscan periods, fixed pool columns, and formatting. |
| `/api/network/chain-size-history` | Preserves Cipherscan chart metadata while returning a stable degraded response. | Physical chain-size history is Zebra operational telemetry and remains sidecar-owned. |
| `/api/network/fee-distribution` | Returns Cipherscan's rolling 7/30/90/365-day shape over positive, non-coinbase actual paid fees, grouped by UTC day with continuous p10/p25/p50/p75/p90 percentiles and rounded integer-zatoshi output. Schema 14 retains signed Sprout, Sapling, Orchard, and Ironwood intrinsic value balances as reusable canonical facts; `PaidFeeDistributionConsumer` combines them with resolved transparent prevouts and exposes exact frequency rows through `ExplorerQuery.PaidFeeDistribution`. The adapter prefers that capability and returns `feeBasis: actual_paid`, `degraded: false`, complete requested coverage, and zero unavailable transactions. The preserved testnet store migrated in place from schema 13: its volume creation time remains `2026-07-05T22:52:43Z`, the pre-migration canonical-plus-derive checkpoint is `backups/cipherscan-paid-fee-schema13-20260711T082431Z`, the live tail began at height 4,159,231, and newest-first history reached the 365-day target floor at height 3,484,835 in about six minutes. A routine writer restart reused the cursor and completed floor without replay. The unchanged Network page renders both 30D and selected 7D charts with populated date axes and all five percentile legends. Live 7D and 30D comparisons matched every public daily transaction count and mean; July 7 matched `521` transactions, mean `38,676`, and all percentiles exactly. Public returned p90 values one or two zatoshis below the canonical integer-fee result on July 4 and July 8. Public 90D/1Y populations also omit positive-fee transactions between 2026-02-22 and 2026-06-04; Zinder keeps the source-derived complete population instead of reproducing that historical index gap. | Complete through additive canonical schema 14, an independently versioned derive consumer, cursor-isolated live-tail seeding, newest-first source-validated backfill, reorg rollback, restart persistence, capability-gated native reads, and adapter translation. No canonical replay, volume recreation, or data wipe occurred. The older conventional-fee projection remains a truthful fallback only when the paid-fee capability is absent. |
| `/api/pools/turnstile` | Returns HTTP `503` with the existing rebuilding response: `{ "success": false, "error": "turnstile_daily view is rebuilding", "status": "building", "retryAfter": 60, "degraded": true, "unavailable": ["Turnstile classification is Cipherscan sidecar analytics and is not a Zinder core chain fact."] }`. | Turnstile is sidecar-owned destination classification. No Zinder schema or native chain projection is required. |
| `/api/migration/overview`, `/api/migration/cohorts`, `/api/migration/denominations` | Returns Cipherscan's Ironwood migration envelopes from one 15-second native snapshot. At shared testnet tip 4,159,389, the historical local/public comparison matched on 1,253 transactions, 2,241,622,388,327 migrated zatoshis, first/last heights 4,134,683/4,159,389, 117,745,915,374 Orchard-out zatoshis, and the Ironwood-in total. Local cohorts and denominations summed to the same 1,253 rows; public's independently cached 5-minute cohort/denomination responses were two transactions behind its 15-second overview. The adapter now deliberately diverges from two public payload mistakes: `poolSizes.ironwoodZat` is the source-backed current Ironwood pool rather than cumulative inflow, and `supplyAudit.balanced` compares the two observed audit totals instead of being hard-coded true. Unavailable audit facts remain `null`. Reference-node height and observed 100-block timing remain Cipherscan sidecar data. | Complete without a new persisted schema, derive consumer, replay, canonical write, or data wipe. Schema 14 already retains signed intrinsic balances. `TransactionHistoryEntry.intrinsic_value_balances` is capability-gated and epoch-pinned; the reader prefers the materialized artifact and bridges unsettled reconciliation lag from retained canonical transaction blobs. Missing data remains absent, never zero. The adapter initially exposed a 107-second pre-activation scan; activation-anchored newer paging reduced the cold overview to 1.52 seconds and warm cohort/denomination reads to 16 milliseconds. The reviewed host adapter passed 143 tests and five consecutive live probe rounds while preserving the volume created at `2026-07-05T22:52:43Z`. |
| `/api/uncles/stats`, `/api/uncles/forks`, `/api/uncles`, `/api/uncle/:hash`, `/api/uncle/report`, `/api/uncles/nodes` | Returns Cipherscan's reorg dashboard envelopes. `/api/uncles/stats` distinguishes archive occurrence count (`totalOrphanedBlocks`) from the `ChainReorgHistory` observed reverted-incident sum (`observedRevertedBlocks`). `/api/uncles` and `/api/uncle/:hash` use the native displaced-block capabilities, and `/api/uncles/forks` adds captured archive entries to fork comparisons. `/api/block/:hash` falls back to displaced-block detail after a canonical miss. Reports and monitored external nodes remain explicit sidecar responses. A post-activation event `33738` populated every native route and the unchanged `/reorgs` and orphan-detail UI paths; writer-first restart preserved the detail. | Complete for activation-limited non-canonical block history, detail, canonical counterparts, and captured fork comparisons. The live capture and restart proof are complete, but historical completeness is not claimed. Reports, monitored nodes, and miner/pool branding remain Cipherscan sidecars. |

### Usage Clock comparison note

On 2026-07-10, the public `https://testnet.cipherscan.app/usage-clock`
page rendered the same one-year values as
`https://api.mainnet.cipherscan.app/api/analytics/usage-clock?period=1y`
(418,472 blocks and 2,324,608 transactions), rather than the contemporaneous
testnet API response (673,831 blocks and 725,184 transactions). The deployed
page's server component defaults `API_BASE` to the mainnet API unless
`NEXT_PUBLIC_API_BASE_URL` overrides it. Treat the public page as a rendering
and wire-shape reference; use the testnet API directly when assessing testnet
data parity. Local Zinder validation therefore asserts the full 168-cell and
24-hour response shape plus explicit coverage, not equality with that
misconfigured public page's dataset.

## Native contract changes

These are reusable Zinder changes the adapter should trigger when existing
native RPCs cannot serve real Cipherscan pages without expensive fan-out or
incorrect semantics.

| Native surface | Proposed owner | Capability shape | Data impact | Adapter route pressure |
| --- | --- | --- | --- | --- |
| Page-ready block detail | `ExplorerQuery.BlockTransactions` | `explorer.block.transactions_v2` | Completed native contract change; no projection, schema upgrade, or replay. It joins existing materialized block ids to canonical transaction facts, unique retained parent facts, and compatible fee rows in bounded batched reads. | `/api/block/:heightOrHash` avoids per-transaction and per-input follow-up calls, exposes resolved transparent input identity and standard addresses, and preserves Cipherscan's block table shape without changing Cipherscan. |
| Rich transaction detail | `ExplorerQuery.TransactionDetail` | `explorer.transaction.detail_v3` | Completed native contract revision. Each mined transparent input combines its spent outpoint with independently optional value and script facts resolved from retained parent transactions under the same chain epoch; the fee reader reuses that parent batch and can preserve a projected value when the script is missing. Each mined transparent output combines its intrinsic value/script with an optional canonical spender from the existing durable reverse-spend relation. Mempool payloads pass through the same source parser ingest uses and expose ordered transaction-intrinsic input outpoints and output value/script rows; they do not claim parent resolution, canonical spent state, or actual paid fees. Mined detail also carries capability-gated intrinsic pool balances from the shared canonical resolver. Lookups are bounded, epoch-coherent, retention-safe, and fail closed on malformed or mismatched dependencies. The storage-only fee input record keeps its existing wire encoding, so no persistent schema upgrade, replay, or volume recreation is required. Raw-byte retention remains separate. | `/api/tx/:txid` serves mined transparent input/output details, canonical output spent state, and signed Sapling, Orchard, and Ironwood balances; `/api/mempool/tx/:txid` serves exact pending transparent output values and standard addresses. Missing facts and nonstandard scripts remain explicit. The unchanged page owns product labels such as `SHIELDING` and `UNSHIELDING`. |
| Bounded block activity distribution | `ExplorerQuery.BlockActivityDistribution` | `explorer.block.activity_distribution_v1` | Completed native contract change; it aggregates at most 20,000 existing `BlockSummaryRecord` rows at request time and reports materialized/missing coverage. No projection, schema upgrade, or replay. It observes the current chain view, not a historical epoch snapshot. | `/api/analytics/usage-clock` receives an exact bounded window without recreating Cipherscan period semantics in Zinder core. |
| Bounded block production series | `ExplorerQuery.BlockProductionSeries` | `explorer.block.production_series_v2` | Completed native contract change; it joins existing `BlockSummaryRecord` rows with one canonical header-range read and one batch of retained leading-transaction facts, returns points in ascending order, and reports covered and missing blocks. Validated coinbase facts include ordered transparent outputs and explicit shielded-output knowledge when available. The request is capped at 1,024 blocks. No projection, schema upgrade, replay, or backfill is required. | Recent-block and mining routes receive timestamp, transaction, fee, reward, difficulty, and canonical coinbase facts without moving rolling formulas, address decoding, payout-role classification, or pool attribution into Zinder core. |
| Paid-fee correction | `TransactionFeesConsumer` plus canonical parent-fact fallback | existing `paid_fee_zat` semantics | Completed row-compatible semantic upgrade. Version-1 rows and cursors are preserved; readers suppress legacy non-transparent values and reconstruct missing or partial input values from retained parent transaction facts. No projection clear, replay, canonical migration, or volume recreation is required. | Recent transaction rows and any product that consumes actual paid fees distinguish them from ZIP-317 conventional floors. Shielded actual fees remain absent because transparent values alone cannot prove them. |
| Transaction history | `ExplorerQuery.TransactionHistory` over `TransactionHistoryConsumer` | `explorer.transaction.history_v1`, dynamically promoted to `explorer.transaction.history_v2` when verified coverage is complete through the projection tip | Row-compatible consumer schema v3 reuses the stable `recent_transactions` physical column family and cursor, declares schema-v1/v2 rows readable, and derives the authoritative transaction index from the unchanged key. Normal commits and reorgs atomically write rows, projection epoch/tip/revision/coverage state, and the chain-event cursor. The non-readiness-blocking verifier compares persisted rows with canonical facts in bounded batches, resumes after its durable coverage height, and publishes progress only if the canonical epoch and projection head remain unchanged. Each request reads projection metadata, rows, fee joins, and optional exact counts from one derive-store snapshot; secondary snapshots hold a catch-up read barrier until the request-local reads finish. Cursors embed the filter and exact read fence, stale request fences and cursors fail closed, and totals are returned with `FULL_HISTORY` scope only when coverage runs from height 1 through the fenced projection tip and matching hash. No projection clear, canonical migration, replay, or volume recreation was required. | `/api/transactions/list` and `/api/tx/shielded` receive canonical ordering, filters, exact fenced totals, and navigation. The adapter requires v2, carries one fence across multi-request scans, rejects drift, and keys cached counts by that fence. |
| Transaction intrinsic balances | Optional signed pool balances joined onto `TransactionHistoryEntry` and `TransactionDetailResponse` | `explorer.transaction.intrinsic_value_balances_v1` | Requires the canonical secondary at artifact schema 14 or newer. Batched history reads and point detail reads validate transaction identity and block-local location at the response epoch. Materialized artifacts are preferred; retained canonical transaction blobs bridge the unsettled-tip reconciliation lag. Absence remains unknown. No schema upgrade, derive replay, or canonical rewrite is required. | `/api/migration/*` receives reusable pool-neutral transaction facts, and `/api/tx/:txid` maps the signed Sapling, Orchard, and Ironwood values to the unchanged Cipherscan detail fields. Product-specific flow labels stay in the Cipherscan page. |
| Commitment-root search | `ExplorerQuery.CommitmentRootSearch` plus the additive displaced-root arm | `explorer.commitment_root.search_v1`, `explorer.commitment_root.displaced_matches_v1` | The native displaced-match fields and capability are landed in the shared contract. Schema 16 owns the displaced final-root facts and activation coverage in the writer-owned archive; no pre-activation completeness claim is allowed. | `/api/search/anchor/:root` maps canonical matches immediately and maps only native displaced positives into Cipherscan's `orphaned` array. Both positive and empty displaced results remain degraded/activation-limited because the archive begins at activation. |
| Mempool transaction lookup | `IngestControl.MempoolTransaction` performs an O(1) writer-index lookup; unpinned `WalletQuery.Transaction` uses it only after a canonical miss, and `ExplorerQuery.TransactionDetail` supplies parsed public and transparent facts to the adapter | `ingest.control.mempool_transaction_v1`; `explorer.transaction.detail_v3` | Completed native contract change with no schema upgrade, replay, or backfill. Pinned reads remain canonical-only, while unpinned reads can observe transient writer state. The detail parser validates that payload identity matches the requested transaction id. | `/api/mempool/tx/:txid` has a no-scan membership path plus exact intrinsic transparent output values, scripts, and standard addresses. Parent input values, shielded value balances, and paid fees remain absent rather than inferred. |
| Completed value-pool flow history and summaries | `ExplorerQuery.ValuePoolFlowHistory`, `ExplorerQuery.ValuePoolFlowSummary`, `ExplorerQuery.ValuePoolFlowAmountThresholdSummary`, and `ExplorerQuery.ValuePoolFlowRoundedAmountSummary` | `explorer.value_pool.flow_history_v1`, `explorer.value_pool.flow_summary_v1`, `explorer.value_pool.flow_amount_threshold_summary_v1`, `explorer.value_pool.flow_rounded_amount_summary_v1` | One additive consumer stores canonical per-transaction transparent-boundary flow events with transaction id, block height/time, stable block-local coordinate, signed per-pool balances, explicit coverage, reorg rollback, checkpoint, and in-place backfill. Coinbase issuance is not a boundary flow: new writes omit it, and all native reads ignore structurally identifiable legacy coinbase rows without replay. Time-bucket, cumulative threshold, and rounded-frequency summaries aggregate the same events on the blocking pool. Threshold requests accept at most 32 increasing amounts. Rounded requests apply bounded raw filters, nearest-quantum positive half-up grouping, a minimum combined count, deterministic frequency ranking, and at most 100 rows. No second aggregate is persisted. The native contract does not store Cipherscan thresholds, labels, scores, denomination choices, numeric database ids, REST cursors, or transparent addresses. | `/api/shielded/list` and `/api/pools/flows` map history and time buckets. Anonymity Set and Shielding Distribution own their threshold and bucket policies. Blend Check owns its fixed tolerance, 0.01-ZEC discovery quantum, score, labels, split algorithm, and cache while batching exact rescoring under the native threshold limit. Deshield output and shield input addresses are optional adapter joins against epoch-pinned canonical transparent facts; encrypted receiver identity, risk scores, labels, and attribution remain unavailable or sidecar-owned. |
| Cumulative value-pool balance history | `ExplorerQuery.ValuePoolBalanceHistory` | `explorer.value_pool.balance_history_v1` | Schema 15 adds an optional exact-block artifact populated from Zebra's historical verbose `getblock` post-state. Schemas 12 through 14 remain readable and can be enriched in place. The derive projection scans every canonical height for coverage, fetches only the highest candidate per observed UTC day, and retains every replaceable live-tail block. Dynamic pool ids, monitored state, optional values, exact hash/time identity, durable restart progress, and reorg reconciliation remain native; Cipherscan periods and fixed columns do not. | `/api/network/pool-history` maps bounded newest-first daily pages to inclusive 7d/30d/90d/1y/all windows and ZEC/zatoshi response modes. `/api/pools/overview` subtracts exact calendar-day snapshots at 1, 7, and 30 days from the hash-bound current summary. No existing canonical or derive volume is wiped. |
| Transaction component summary | Completed `TransactionComponentSummaryConsumer` and `ExplorerQuery.TransactionComponentSummary` | `explorer.transaction.component_summary_v1` | Additive derive schema v1 owns per-block contributions, height lookup, UTC-day aggregates, historical coverage, and live-tail coverage. Historical backfill stops at `tail_boundary - 1`; startup seeds the already-visible unsettled range without advancing the inherited cursor. Reorgs update contributions and day extrema. Existing high boundaries can be widened and revalidated without deleting rows. | `/api/stats/shielded-count`, `/api/stats/shielded-daily`, and `/api/network/protocol-stats` receive complete component history and explicit coverage without a canonical migration or wipe. |
| Conventional fee distribution | Completed reusable native conventional-fee projection; insufficient for paid-fee parity | `explorer.fee.conventional_distribution_v1` | Additive derive schema v1 owns block-time contributions, a height rewind index, exact UTC-day frequency aggregates, unavailable counts, and historical/live-tail coverage. Startup seeds the visible tail and the background worker backfills canonical history in place. No canonical schema change, unrelated-consumer replay, volume replacement, or wipe is required. | `/api/network/fee-distribution` preserves Cipherscan's percentile, period, and cache shape but declares the conventional series degraded. A separate paid-fee fact/projection is required before this route is complete. |
| Chain economics summary | Pure adapter derivation over existing native facts; optional future native contract for historical subsidy allocation | existing value-pool summary/history capabilities; reserve `explorer.chain.economics_v1` only for a proven cross-product need | No schema upgrade is required for current or daily historical supply/emission. `ValuePoolBalanceHistory` already stores exact canonical daily post-block pool state with coverage, rollback, and restart semantics. A future native contract is justified only for reusable historical subsidy-allocation facts that cannot be derived from those balances. | `/api/supply` and `/api/network/halving` use current supply/subsidy facts. `/api/network/emission` sums daily pool snapshots and computes positive adjacent-day deltas at the adapter edge. |
| Displaced block archive | Canonical writer-owned `DisplacedBlockArchive` captured during `ReorgWindowChange::Replace` | `explorer.chain.displaced_block_history_v1`, `explorer.chain.displaced_block_detail_v1` | Implemented as canonical writer-owned retention, not an ordinary derive projection. The writer captures the displaced preimage, hash identity, height, and event ordering in the same canonical-store `WriteBatch` as `Replace`; native `ExplorerQuery.DisplacedBlockHistory` and `ExplorerQuery.DisplacedBlockDetail` expose product-neutral history/detail capabilities. Activation starts coverage at the first enabled displacement; permanent retention is the default, raw block bytes are optional, and no historical completeness claim or wipe is allowed. Schema remains 15. The preserved volume was created at `2026-07-05T22:52:43Z`, was not wiped or replayed, and has rollback checkpoint `backups/cipherscan-displaced-block-archive-v0-20260711T232158Z`. Post-activation event `33738` captured the displaced block at height `4,160,925`; writer-first ingest then query/explorer restart stayed healthy and detail persisted. | `/api/uncles`, `/api/uncle/:hash`, and canonical-miss `/api/block/:hash` use the native capabilities; `/api/uncles/forks` uses captured archive entries for comparisons. `/api/uncles/stats` keeps archive occurrence count distinct from the observed reverted-incident sum. Reports, monitored node observations, and miner/pool branding remain Cipherscan sidecars. |
| Chain-wide transparent address ranking | Completed `TransparentAddressRankingConsumer` and `ExplorerQuery.TransparentAddressRanking` | `explorer.transparent_address.ranking_v1` | Additive derive schema v2 owns immutable generations, balance ordering, lifetime summaries, concentration totals, P2PKH/P2SH aggregates, coverage, and per-height undo journals. Startup snapshots settled canonical balances, reconciles complete lifetime deltas, applies the visible tail, then atomically activates at the unanimous cursor. It preserves canonical storage and resumes interrupted inactive builds. | `/api/rich-list` receives deterministic bounded pages and complete aggregate metrics; `/api/supply/transparent-breakdown` receives standard script totals without scanning every address. SQL semantics and network-specific address strings remain outside the native contract. |

Do not introduce these as one large Cipherscan API abstraction. Each surface
must earn its own capability, freshness contract, projection cost, reorg story,
and tests.

## Compatibility status vocabulary

| Status | Meaning |
| --- | --- |
| Adapter now | Existing Zinder RPCs can support the route with HTTP/JSON translation only. |
| Native contract change | The adapter needs a product-neutral Zinder query or proto change, but no persistent projection is implied. This does not require a schema upgrade. |
| Native derive projection | The route needs durable indexed data. It requires an additive schema migration, reorg handling, checkpointing, and backfill or replay planning; it is outside the no-schema/no-replay compatibility sequence. The migration must preserve the existing canonical volume rather than require a wipe. |
| Native derive data correction | Existing projection semantics or records are incorrect. First determine whether the new reader can safely interpret old rows. A row-compatible semantic upgrade preserves rows and cursors; an incompatible change may rebuild only after tests prove its recovery source covers the persisted history. Neither path may assume that retained events still contain every joined fact. |
| Adapter plus native change | Legacy matrix wording only. Resolve it to either `Native contract change` or `Native derive projection` using the data-impact column before implementation. |
| Adapter plus sidecar | The route can exist, but data comes from Cipherscan-owned sidecars, not Zinder core. |
| Degraded adapter response | The route should return a stable unsupported or unavailable response until a sidecar or native primitive exists. |
| Reject from Zinder core | The route may be adapter-owned, but the behavior must not move into Zinder core. |

## Coverage matrix

| Cipherscan area | Current endpoints | Near-term adapter behavior | Zinder native surface | Decision |
| --- | --- | --- | --- | --- |
| Server and chain info | `/api/info`, `/api/blockchain-info` | Adapter now | `WalletQuery.LatestBlock`, future `OverviewSnapshot` | Preserve Cipherscan's chain-info shape. Use native/ops surfaces for Zinder capabilities, service metadata, and freshness. |
| Recent blocks | `/api/blocks`, `/api/blocks/list`, `/api/network/blocks/recent` | Completed adapter surface with Cipherscan-compatible rows, cursor pagination, exact compact-target-derived difficulty strings on list routes, recent-block ZEC fee/reward units, and exact standard miner payout addresses | `BlockProductionSeries`, future `OverviewSnapshot` | Version 2 reuses retained canonical coinbase facts with no storage impact. The adapter decodes output index 0 for Cipherscan compatibility; Zinder exposes ordered outputs without assigning payout roles. Pool branding remains sidecar-owned. Local and public API values match for shared testnet heights 4,158,353 through 4,158,357 except public Cipherscan's one-byte-low size. |
| Block page | `/api/block/:heightOrHash` | Completed adapter surface plus completed native contract and root-artifact changes | `ExplorerQuery.BlockTransactions`, `ExplorerQuery.BlockProductionSeries`, `WalletQuery.Transaction`, `BlockFinalNoteCommitmentRoots` | Page-ready transaction rows use bounded canonical batches to resolve retained transparent prevouts, and a same-epoch one-height production point supplies validated coinbase output 0 for miner payout identity. The adapter reads the coinbase transaction at the same epoch and decodes its retained miner-data bytes into Cipherscan's hex/text fields. Standard transparent addresses are decoded at the adapter edge. Optional post-block Sapling, Orchard, and Ironwood roots come from the schema-13 canonical artifact; unchanged local Cipherscan rendered the coinbase data and all three roots for block 4,158,544. Input values remain withheld block-wide when unchanged Cipherscan could miscompute fees for shielded rows; shielded value balances remain separate persisted-fact work. |
| Transaction page | `/api/tx/:txid` | Adapter plus completed `TransactionDetail` v3 contract and paid-fee correction | `ExplorerQuery.TransactionDetail` | Mined canonical rows provide transparent outputs with canonical spent state and ordered inputs with outpoint, independently optional value, and script. The adapter decodes standard input/output addresses. Safe fee reads preserve compatible historical rows and recover missing values from the same parent-fact batch without a projection replay. Shielded value balances remain distinct work. |
| Raw transaction | `/api/tx/:txid/raw`, `/api/tx/raw/batch` | Completed adapter surface with live pending-to-mined byte-retention proof and unchanged-UI proof | `WalletQuery.Transaction`, existing transaction-blob retention policy | Testnet retains transaction blobs for new blocks through `raw_blob_policy = "transactions"`; no schema upgrade, replay, backfill, volume recreation, or data wipe is required. Historical bytes from before the policy change remain absent. |
| Verbose transaction JSON | `/api/tx/:txid/verbose` | Completed degraded adapter response for retained bytes, with live unchanged-UI proof | none | Preserve Cipherscan's `{ txid, hex, decoded }` envelope while marking `decoded` degraded. Do not promise Zebra's decoded JSON as a native Zinder contract; reusable decoded transaction facts must be named individually on native Zinder surfaces. |
| Broadcast | `/api/tx/broadcast` | Completed adapter route with live accepted/mined and duplicate evidence | `WalletQuery.BroadcastTransaction` | Map typed outcomes into Cipherscan JSON. Only accepted is `success: true`; duplicate and queued remain explicit failures for the unchanged client because neither may fabricate a transaction id. No durable data change is required. |
| Commitment-root search | `/api/search/anchor/:root` | Canonical compatibility complete; schema-16 displaced-root mapping deployed with activation-limited coverage | `BlockFinalNoteCommitmentRoots`, `ExplorerQuery.CommitmentRootSearch`, `CommitmentRootSearchConsumer`, schema-16 displaced-root fields and reverse indexes | Canonical roots come from the schema-13 artifact and bounded miner joins. Schema 16 retains displaced roots atomically with replacement and exposes positives plus independent activation coverage. The adapter maps retained positives to `orphaned`, leaves displaced miner identity null, and keeps empty results degraded/indeterminate because pre-activation history is unknown. The migration preserved the existing volume and has a schema-15 rollback checkpoint. All three canonical protocols and the unactivated zero-capture state have live API proof; no live displaced positive can be proven until a post-deployment replacement occurs. |
| Transaction browsing | `/api/transactions/list`, `/api/tx/shielded` | Rendered compatibility complete with fenced native full-history proof | `TransactionHistory` v2, transaction component filters | Cipherscan cursors and offsets stop at the adapter. Native pages use filter-bound, fence-bound opaque cursors or canonical anchors. Exact totals are accepted only with `FULL_HISTORY` scope under the same read fence used for the page walk. Schema v3 preserved existing rows, cursors, and the Docker volume in place; the resumable verifier established contiguous height-1-through-tip coverage without replaying or wiping the projection. Live route and unchanged `/privacy` proof covers the current rendered workflow, not every historical or future Cipherscan UI variant. |
| Shielded flow list | `/api/shielded/list` | Completed adapter and additive native projection | `ValuePoolFlowHistory` | Zinder exposes product-neutral canonical net events, signed per-pool balances, typed filters, filter-bound opaque cursors, optional exact totals, and explicit coverage. The adapter owns Cipherscan labels, numeric display units, and timestamp/id pagination. Address attribution remains explicitly unavailable. |
| Mempool overview | `/api/mempool` | Adapter plus completed no-schema native extension; summary and rows derive from one `MempoolSnapshot`, so a mine between separate upstream reads cannot show a stale count beside an empty page. Live empty and non-empty API paths, the count bubble, transaction table, and transaction link are proven in unchanged Cipherscan. | `MempoolSnapshot` | Shape Zinder mempool facts into Cipherscan JSON without persisting a second mempool projection. The local Z3 stack must expose Zebra's private indexer gRPC listener to feed current mempool state. |
| Mempool transaction | `/api/mempool/tx/:txid` | Adapter plus completed no-schema native lookup for membership, parsed counts, version, size, locktime, first-seen time, and ordered intrinsic transparent outputs. Live pending-output rendering and the transition to a mined page are proven. | `WalletQuery.Transaction`, private `IngestControl.MempoolTransaction`, `ExplorerQuery.TransactionDetail` | Return `inMempool: false` for missing, confirmed, or conflicting txids. Keep `/api/tx/:txid` confirmed-only so unchanged Cipherscan reaches its mempool fallback. Exact transparent output totals and standard addresses come from the transient payload; nonstandard scripts produce `address: null`. Parent input enrichment and shielded value balances remain unavailable. No durable schema is required. |
| Realtime WebSocket | root `/` WebSocket upgrade | Completed no-schema adapter bridge with live-proven block and mempool lifecycles, reorg-safe cursor progress, reader-lag retries, ordered multi-block fanout, immutable mempool-add mapping, and one shared native subscription per stream | `WalletQuery.ChainEvents`, `WalletQuery.MempoolEvents`, `ExplorerQuery.BlockProductionSeries`, native `MempoolEntry` bytes and outputs | Emit only Cipherscan's existing `new_block`, `mempool_tx`, and `mempool_removed` wire shapes. Keep opaque cursors and reconnect semantics internal. The unchanged homepage consumes `new_block`; the homepage and `/mempool` consume mempool events. `/blocks` remains HTTP-only. Do not emit `network_stats` or `privacy_stats` until those facts are truthful. |
| Transparent address page | `/api/address/:address` | Completed adapter and no-schema native contract with exact public API comparison, block-transition stability, and unchanged-UI proof | `ExplorerQuery.TransparentAddressActivity` v2, `TransparentAddressRanking`, retained canonical transaction facts | Zinder owns one epoch-coherent confirmed summary, complete lifetime coverage, bounded offset/cursor paging, source transaction facts, and raw counterparty scripts. The adapter owns page-number coercion, testnet address encoding, Cipherscan field names, privacy responses, and typed degradation. It does not join a second wallet-reader epoch. Counterparty labels remain sidecar-owned. No replay, backfill, schema migration, volume replacement, or data wipe is required. |
| Address labels | `/api/labels`, `/api/label/:address` | Degraded adapter response now; adapter plus sidecar for real labels | none | Moderation/tag data does not enter Zinder core. Testnet production currently returns an empty label list. |
| Rich list | `/api/rich-list` | Completed adapter and additive native derive surface with live API, Zebra, restart, and unchanged-UI proof | `ExplorerQuery.TransparentAddressRanking` | The adapter owns Cipherscan query coercion, address encoding, ZEC conversion, decimal timestamp strings, labels, and pagination names. Zinder owns canonical ranking, lifetime summaries, concentration totals, explicit coverage, and freshness. The additive projection preserves the existing canonical volume and never claims public Cipherscan's non-canonical rank-9 overcount. |
| Supply, activity, and transparent totals | `/api/network/stats`, `/api/supply`, `/api/circulating-supply`, `/api/supply/transparent-breakdown` | Completed current value-pool composition, circulating supply, bounded 24-hour activity, and standard transparent script breakdown | `ValuePoolSummary`, `BlockProductionSeries`, `TransparentAddressRanking`, `UtxoSetSummary`, chain economics summary | The transparent breakdown validates P2PKH/P2SH totals against the active ranking generation and uses the source-backed transparent pool as its denominator. It exposes nonstandard/unattributed value rather than forcing it into `other`. Category labels stay sidecar-owned. The additive ranking migration preserves canonical data and requires no volume wipe. |
| Fees | `/api/network/fees`, `/api/network/fee-distribution` | Complete ZIP-317 estimate/summary and actual paid-fee distribution routes | `FeeSummary`, conventional-fee distribution, intrinsic value balances, paid-fee distribution | Keep conventional and paid fees as distinct native facts. The paid-fee projection combines signed pool balances with resolved transparent values, supports per-height rollback, and backfills in place without wiping canonical data. Conventional fees remain an explicitly degraded fallback only when paid-fee coverage is unavailable. |
| Network health | `/api/network/health` | Completed adapter compatibility surface with exact rendered-field API comparison and unchanged-UI proof | ops `/readyz`, `ExplorerQuery.ServerInfo`, `WalletQuery.ServerInfo` | `healthy` and `ready` describe successful Zinder service reads. Direct Zebra endpoint availability remains false and explicitly sourced as `zinder-query-plane`; adapter health and native capability details remain separate response arms. No schema, replay, backfill, or data wipe is required. |
| Peer and node inventory | `/api/network/peers`, `/api/network/nodes`, `/api/network/nodes/stats`, `/api/network/node-history` | Adapter plus sidecar or degraded response | none | Node-crawler data is not Zinder core. |
| Chain economics | `/api/network/halving`, `/api/network/emission` | Completed adapter surface over current consensus facts and reusable native daily value-pool history | `ValuePoolSummary`, `ValuePoolBalanceHistory`; future chain economics summary only for proven cross-product subsidy-allocation demand | Name as chain economics, not Cipherscan network analytics. The adapter computes tip-backed subsidy values from Zebra consensus helpers, exact current supply from all valued pools, and observed daily issuance from adjacent canonical supply snapshots. |
| Value-pool history | `/api/network/pool-history`, `/api/pools/overview`, `/api/pools/flows` | Completed native and adapter surfaces with in-place schema-15 deployment, Zebra source validation, public API comparison, restart/overlap regression coverage, and unchanged-UI proof | `ValuePoolSummary`, `ValuePoolFlowHistory`, `ValuePoolFlowSummary`, `ValuePoolBalanceHistory` | `ValuePoolSummary` remains the hash-bound source-backed current tip. Flow events and UTC summaries remain a separate transaction-movement projection. Cumulative history uses authoritative Zebra block post-state, an optional schema-15 canonical artifact, sparse daily historical sampling, exact live-tail retention, dynamic future-pool ids, and height-domain coverage. The adapter alone owns inclusive Cipherscan periods, fixed known-pool fields, ZEC/zatoshi formatting, and 1/7/30 calendar deltas. The existing testnet volume migrated in place after checkpoint `backups/cipherscan-value-pool-balance-schema14-20260711T223111Z`; it was not wiped or recreated. |
| Mining rewards | `/api/mining/rewards` | Completed adapter surface for bounded 24-hour, 3-day, and default 7-day windows; explicit partial coverage for longer windows | `BlockProductionSeries`; future daily chain-reward projection only for complete long-range history | The adapter uses exact timestamp cutoffs, pinned canonical pages, contiguous-height validation, and Cipherscan's five-minute cache. Legacy series fields are preserved; additive metadata declares transparent-output coinbase semantics and ZIP-317 fee fallback. No schema migration, replay, backfill, or data wipe is required. Pool attribution stays sidecar-owned. |
| Mining production metrics | `/api/network/mining-metrics` | Adapter plus completed no-schema native contract; live API comparison, CORS preflight, five populated metric cards, and the Solrate Trend chart are proven in unchanged local Cipherscan | `BlockProductionSeries` | Keep canonical block-production inputs and explicit coverage in Zinder. Keep rolling averages, the 75-second first-sample fallback, and legacy Cipherscan solrate units in the adapter. Actual paid fee history remains distinct from ZIP-317 conventional fee floors. |
| Mining attribution and behavior | `/api/mining/pool-distribution`, `/api/mining/pool-ranking`, `/api/mining/hashrate-share`, `/api/mining/miner-behavior`, `/api/mining/zodl-leaderboard` | Adapter plus sidecar or degraded response | none | Do not move branding, pool attribution, destination classification, or behavior scoring into core. |
| Protocol/component stats | `/api/network/protocol-stats`, `/api/stats/shielded-count`, `/api/stats/shielded-daily`, `/api/analytics/usage-clock` | Completed adapter and additive native derive surface for protocol and shielded history; completed no-schema bounded Usage Clock contract | `TransactionComponentSummary`, `WalletQuery.LatestBlock`, `BlockActivityDistribution` | Exact fixed daily and simultaneous detailed responses match public testnet. Protocol totals remain source-correct when public PostgreSQL aggregation diverges from consensus commitment-tree sizes. All responses expose contiguous coverage; no canonical schema change or data wipe is required. |
| Privacy/pool analytics | `/api/privacy-stats`, `/api/analytics/anonymity-set`, `/api/analytics/shielding-distribution` | All three routes are complete. Privacy Stats now owns exact all-time and daily predicate counts, current/historical pool values, adapter score/trend/rounding, and WebSocket refresh. Anonymity Set and Shielding Distribution share the native cumulative amount-threshold summary while retaining separate adapter-owned thresholds/buckets, cache entries, and JSON contracts. | `TransactionComponentSummary` v2, `ValuePoolSummary`, `ValuePoolBalanceHistory`, `ValuePoolFlowHistory`, and `ValuePoolFlowAmountThresholdSummary` | Keep raw predicate totals, explicit unavailable counts, and coverage in Zinder; keep scores and product analytics in the adapter. The exact UTC-day contract intentionally differs from Cipherscan's inconsistent live-job and archived-backfill approximations. The two amount views are read-time folds over one canonical projection; measured cold full-history aggregation remains below 100 ms on the local testnet projection, so no second aggregate or analytics sink is justified. |
| Turnstile | `/api/pools/turnstile` | Sidecar-owned; current adapter response is the exact rebuilding `503` above | none | Destination classification is not a Zinder chain fact. No Zinder schema is required. |
| Blend Check | `/api/blend-check`, `/api/blend-check/split` | Completed adapter scoring and split policy over reusable no-schema native flow summaries, with local/public API and unchanged-UI proof | `ValuePoolFlowAmountThresholdSummary`, `ValuePoolFlowRoundedAmountSummary` | Exact flow counts and complete coverage remain native. Scores, labels, candidate ranges, denomination thresholds, greedy split policy, and five-minute cache remain compatibility-product behavior. |
| Common amounts | `/api/privacy/common-amounts` without `chain` | Completed no-schema adapter surface with local/public API and unchanged Privacy Risks Popular-row proof | `ValuePoolFlowAmountThresholdSummary`, `ValuePoolFlowRoundedAmountSummary` | Exact ranked groups and the complete denominator remain native. Percentage, score, period/limit coercion, cache, and JSON stay in the adapter. Cross-chain enrichment remains sidecar-owned. |
| Linkability and risk scoring | `/api/tx/:txid/linkability`, `/api/privacy/*` risk and graph endpoints | Degraded adapter response now; sidecar required for real linkage, graph, batch-risk, and swap-recommendation results | none | Reject inferred identities and safety verdicts from Zinder core. These require product-side analysis and can mislead users when incomplete. |
| Cross-chain swaps | `/api/crosschain/*` | Explicit `503` unavailable response now; adapter plus sidecar for real swap analytics | none | Bridge semantics are external to Zcash chain indexing. |
| Names | `/api/name/:name`, `/api/name/:name/events` | Name lookup returns explicit `503` unavailable rather than invented availability/pricing; event history remains degraded until a sidecar exists | none | Name-service data belongs to a separate integration. The unchanged name page maps non-2xx to not-found, so a distinct unavailable UI requires the deferred Cipherscan change. |
| Prices | `/api/price`, `/api/price/at` | Current and latest-365-day historical prices complete through a bounded adapter-owned external transport; deeper historical coverage remains sidecar-owned | none | Market data is not Zinder chain data. Local `2026-07-01` returned exact `$398.5574`; `2026-07-12` returned `$498.9317` from completed day `2026-07-11` with `exact: false`. The unchanged transaction page rendered a `$0.07` fee and `$6,174.28` output with no browser errors, while public testnet returned `500` for all valid historical dates because its table was unavailable. Provider failure never gates Zinder readiness. |
| Reorg and non-canonical blocks | `/api/uncles`, `/api/uncles/forks`, `/api/uncle/:hash`, `/api/uncles/stats`, canonical-miss `/api/block/:hash` fallback | Completed adapter/native surface with activation-limited coverage and post-activation live proof | `ChainReorgHistory`, `ExplorerQuery.DisplacedBlockHistory`, `ExplorerQuery.DisplacedBlockDetail` | Do not use `uncle` as core vocabulary. The writer captures displaced preimages atomically with replacement. Event `33738` displaced `00baeb26...c135` at height `4,160,925`, with canonical counterpart `007837b4...ad47`; `/api/uncles`, `/api/uncles/forks`, `/api/uncles/stats`, `/api/uncle/:hash`, and `/api/block/:hash` populated. Stats showed archive `1`, events `225`, and observed reverted `303`; unchanged local `/reorgs` and orphan-detail pages rendered without console errors. Writer-first ingest then query/explorer restart stayed healthy and detail persisted. Historical coverage remains activation-limited. Reports, monitored nodes, and miner/pool branding remain sidecars. |
| Public fork reports | `/api/uncle/report`, `/api/uncles/nodes` | Degraded adapter response now; adapter plus sidecar for real registry data | none | Public report ingestion and monitored external node status are fork-monitor product behavior. |
| Crosslink/finalizers | `/api/crosslink/*`, `/api/finalizers`, `/api/finalizer/*` | Consensus, BFT, and finalizer routes return explicit `503` unavailable instead of invented finalized height, zero gap, stake, or roster state. Bootstrap no-snapshot, divergence history, and read-only fork-monitor anchors retain their executable degraded shapes. | `BlockIdBySelector` for local anchors; none for BFT/cTAZ/registry today | Separate product surface until Zinder explicitly owns NU7/Crosslink finality. A PoW safe-tip approximation must not be labeled as Crosslink finality. |
| Migration dashboards | `/api/migration/*` | Complete chain-derived overview, cohorts, denominations, and supply audit; degraded only for reference-node and observed timing sidecars | `ValuePoolSummary`, NU6.3 activation height, `TransactionHistory` intrinsic balances | Keep the Ironwood dashboard shape at the adapter boundary. Zinder owns signed pool-neutral transaction facts and current pools; the adapter owns migration predicates, cohort boundaries, denominations, caching, and Cipherscan JSON. No new persisted schema or wipe is required. |
| Client-side scanning helpers | `/api/scan/orchard`, `/api/lightwalletd/scan` | Recent Orchard candidate discovery complete; birthday compact-block scan remains an explicit degraded rejection | `TransactionHistory` v2 and retained transaction bytes for recent candidate discovery; `zinder-compat-lightwalletd` for streamed compact chain data | Keep viewing keys and decryption in Cipherscan. The adapter may translate a bounded canonical Orchard candidate query, but it rejects viewing-key fields and fails closed unless every candidate has same-epoch retained bytes. Do not materialize birthday compact-block ranges as one REST JSON document; that workflow remains on the bounded wallet/lightwalletd streaming surface. |

## Remaining-surface classification

This is the residual ownership list after the completed native and adapter work.
Current privacy statistics are complete only for the explicitly provable fields
listed above; null fields remain residual work or intentionally sidecar-owned.

| Classification | Remaining surfaces | Boundary |
| --- | --- | --- |
| Feasible with existing facts | Current chain economics, health, recent activity, and compatibility envelopes that need only bounded reads and translation | Adapter over existing `ExplorerQuery`, `WalletQuery`, or ops facts |
| Additive projection | Historical subsidy-allocation series and any future neutral analytics that require durable coverage, rollback, and restart state beyond the completed daily supply history | New product-neutral projection only after an existing native fact cannot satisfy a proven cross-product contract |
| Sidecar-owned | Turnstile, labels, historical prices beyond configured provider coverage, names, cross-chain data, mining attribution, Crosslink/finalizer data, identity/linkability scores, and graph analytics | Cipherscan or external product sidecar; no Zinder schema |
| Unavailable source facts | Historical transaction bytes outside retention, uncaptured pre-activation displaced block bodies, server-side viewing-key scan results, and physical chain-size history | Keep explicit `unavailable` or degraded responses; do not infer or fabricate values. Recent client-side Orchard candidate discovery is available only where every candidate has retained bytes. |

## Naming rules for new work

Use these names unless implementation research proves a better domain term.

| Use this | Avoid | Reason |
| --- | --- | --- |
| `CipherscanRestAdapter` | `CipherscanService`, `CipherscanManager`, `CompatibilityProcessor` | Names the actual edge translation role. |
| `value_pool_flow` | `privacy_stats`, `pool_flow_stats` | Names the chain movement, not a Cipherscan product interpretation. |
| `value_pool_balance_history` | `pool_history`, `value_pool_history` | Distinguishes cumulative post-block balances from transaction flow history. |
| `transaction_component_summary` | `protocol_stats`, `shielded_daily` | One reusable aggregate serves multiple product views. |
| `block_production_series` | `mining_metrics` | Names the canonical block inputs and bounded ordering contract without importing Cipherscan's rolling formulas or pool semantics. |
| `non_canonical_block` | `uncle`, `orphan` in public Zinder API | `uncle` is wrong-domain vocabulary; `non_canonical_block` says exactly what it is. |
| `chain_economics` or specific `supply_summary` | `network_emission_stats` | Supply/subsidy are chain facts, not network health. |
| `transaction_history` | `transactions_list`, `tx_list` | Describes a canonical ordered history, not a REST endpoint. |
| `commitment_root` | `anchor` for search surfaces | The search target is the root; `anchor` can remain a protocol field inside transaction facts. |
| `paid_fee_zat` and `zip317_conventional_fee_zat` | `fee` | Avoids conflating actual paid fees with conventional fee floors. |
| `unavailable` with reason | zero or empty success | Absence is an explicit contract, not a value. |

The adapter may expose Cipherscan route names such as `/api/uncles` and JSON
fields such as `fee` because Cipherscan cannot change now. Those names must stop
at the adapter boundary.

## Breaking-change impact

### Zinder proto and capabilities

- `ExplorerQuery.BlockDetail` may need a breaking reshape or companion
  `BlockTransactions` RPC. Prefer one clean end-state over parallel v1/v2
  baggage unless an external client forces both during migration.
- `ExplorerQuery.TransactionDetail` is the page-ready transaction contract. It
  now carries existing canonical transparent rows; continue evolving that
  native shape rather than adding a Cipherscan-specific detail surface.
- Capability names must describe native guarantees, not REST paths.
- Every new projection needs freshness and typed unavailable-field behavior.

### Adapter service

- Route serializers should be grouped by domain route family, not by generic
  helpers. A future agent should find block route code by searching `block`, tx
  route code by searching `transaction`, and sidecar code by its product domain.
- Unsupported routes should be explicit and stable. A route should not silently
  return an empty Cipherscan-shaped success body because a native capability is
  missing.
- The adapter should cache capability discovery briefly, but not hide capability
  changes forever.
- The adapter should expose its own `/healthz`, `/readyz`, and `/metrics` while
  keeping Zinder service health distinct from sidecar availability.

### Derive plane

- New projections must be optional by capability and must state write cost,
  replay behavior, reorg handling, retention impact, and backup/rebuild story.
- Do not add a monolithic Cipherscan projection. Add focused projections:
  transaction history, value-pool flows, transaction component summaries, and
  fee histograms. The displaced-block archive is writer-owned canonical
  retention, not a derive projection.
- Wallet-critical projections remain separate from analytics projections.

### Cipherscan app

- No Cipherscan frontend changes are required in the current phase.
- The app should be able to switch API base URL to the adapter.
- Native client migration is a future TODO after the adapter stabilizes.

### Operations

- Deployment should make degraded ownership visible: adapter route missing,
  Zinder capability missing, projection lagging, Cipherscan sidecar unavailable,
  or route rejected from Zinder core.
- A Cipherscan deployment should be able to run core chain pages from Zinder
  without the old Postgres indexer as native projections come online.
- Optional analytics sidecars must not gate wallet or core explorer readiness.

### Documentation

- Promote accepted native surfaces into ADRs and `docs/architecture/*`.
- Keep this matrix as planning evidence, not the durable contract. Durable
  contracts belong in proto comments, capability tables, architecture docs, and
  runbooks.
- Document every rejected product surface so future agents do not reintroduce
  it under a different name.

## Implementation phases

### Phase 0: finish the route ownership audit

Goal: make every endpoint and field accountable before code starts.

Work:

- Confirm which Cipherscan routes are actively consumed by the current app.
- Split each endpoint into fields and mark each field as Zinder-native,
  adapter-owned, sidecar-owned, or rejected from Zinder core.
- Decide each route's degraded response when the native capability or sidecar is
  absent.
- Draft proposed native capability names for every Zinder-owned missing fact.

Gate:

- No adapter route proceeds to implementation without an end-state owner, a
  degraded response, and a capability or rejection decision.

### Phase 1: adapter skeleton over existing RPCs

Goal: switch Cipherscan's API base URL for core low-risk routes without
requiring new Zinder storage.

Work:

- Create `services/zinder-compat-cipherscan`.
- Add gRPC clients for `ExplorerQuery` and `WalletQuery`.
- Implement route families for server info, blockchain info, block lists,
  mempool overview, transparent address overview, raw transaction lookup, and
  broadcast.
- Add stable degraded responses for sidecar-owned and unsupported routes.
- Add adapter ops endpoints and capability discovery.

Gate:

- Adapter starts without Cipherscan PostgreSQL.
- Core low-risk routes return Cipherscan-shaped JSON from Zinder.
- Unsupported routes fail explicitly.

### Phase 2: high-traffic native gaps

Goal: make core Cipherscan pages correct and efficient through native Zinder
contracts.

Work:

- Page-ready block detail is complete without a schema upgrade.
- Rich mined transaction detail is complete for retained transparent rows
  without a schema upgrade.
- Bounded block production is complete without a schema upgrade, replay,
  backfill, or data wipe; the adapter owns Cipherscan's rolling metrics.
- Transparent-address detail is complete without a schema upgrade, replay,
  backfill, or data wipe; one native response owns summary, activity, coverage,
  and canonical epoch so the adapter never joins independently advancing readers.
- Add transaction history only to the extent needed for real routes.
- Add commitment-root search if the route is active.
- Add mempool transaction lookup only if bounded scan is unacceptable.

Gate:

- Block, transaction, address, mempool, and recent activity routes do not depend
  on the old Cipherscan Postgres indexer for chain facts.

### Phase 3: adapter coverage expansion

Goal: cover remaining active routes without moving product-specific data into
Zinder.

Work:

- Add adapter routes backed by sidecars for labels, prices, names, bridge data,
  mining attribution, Crosslink, and privacy analysis when those sidecars are
  still active requirements.
- Add stable degraded responses when sidecars are absent.
- Document which routes are intentionally unsupported.

Gate:

- Every active Cipherscan route either returns real data, returns a stable
  degraded response, or is explicitly rejected.

### Phase 4: reusable analytics

Goal: promote only multi-product analytics into Zinder.

Work:

- Operate the completed transaction component, value-pool flow, and value-pool
  balance-history projections; extend them only for separately accepted neutral
  chain facts.
- Add the ZIP-317 conventional-fee daily distribution with explicit coverage;
  keep actual paid-fee aggregates as a separate series where coverage is provable.
- Maintain the completed optional transparent-address ranking and measure its
  steady-state write cost before enabling it by default in other deployment
  profiles.
- Operate and extend the validated writer-owned `DisplacedBlockArchive`:
  preserve atomic replacement capture, activation-limited coverage,
  permanent-retention, optional raw-byte, and writer-first restart contracts.
  Keep archive occurrence counts distinct from observed reverted-incident sums,
  and do not claim historical completeness. Post-activation capture and
  writer-first restart proof are complete.

Gate:

- Each projection has a capability, freshness contract, reorg behavior, rebuild
  path, and explicit owner.

### Phase 5: Cipherscan native-client TODO

Goal: remove the REST adapter from internal Cipherscan app traffic later.

Work:

- Replace internal Cipherscan REST calls with native Zinder clients or a
  Cipherscan BFF that speaks Zinder gRPC.
- Keep the REST adapter only for external public API consumers if needed.
- Retire adapter serializers route by route once no internal or external
  consumer needs them.

Gate:

- Cipherscan can render core pages without the adapter, or the adapter is
  retained only as an intentional public API product.

## Open questions

The active-route audit is closed against the sibling Cipherscan UI and its route
source files. The remaining questions are design and ownership questions, not
route-discovery gaps.
- Which routes have external public API consumers and therefore need stable
  REST behavior beyond the current frontend?
- What degraded JSON shape should Cipherscan render correctly when sidecars are
  absent?
- Can the adapter return partial block/transaction pages in the first phase, or
  must page-ready native gaps land first?
- Which analytics are important enough to pay always-on derive write cost?
- Should value-pool flow and component summary projections be served from the
  normal derive store first, then later exported to ClickHouse as a projection
  sink if volume demands it?

## Source notes

- `/Users/gustavovalverde/dev/zfnd/cipherscan/app/docs/endpoints.ts` lists the
  public documented REST endpoints.
- `/Users/gustavovalverde/dev/zfnd/cipherscan/server/api/routes/` contains
  additional internal and frontend-consumed routes.
- `docs/architecture/explorer-plane.md` defines the native Zinder explorer
  boundary, freshness model, privacy boundary, and capability rules.
- `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` defines
  `ExplorerQuery`.
- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` defines
  `WalletQuery`.
- `crates/zinder-proto/src/capabilities.rs` defines advertised Zinder
  capabilities.
- `docs/investigations/database-adapter-architecture.md` constrains new data
  work to canonical stores, projection stores, and analytics sinks rather than a
  generic database adapter.
