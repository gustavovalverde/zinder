# Block Explorer Consumer Requirements

Status: Shipped
Date: 2026-05-18
Last updated: 2026-07-12
Owner: ZFND
Reference consumer: zexplorer (M0+)

This PRD records the original explorer requirements. The current normative wire, capability, and projection semantics live in [Public interfaces](../architecture/public-interfaces.md) and [Explorer plane](../architecture/explorer-plane.md). Later additive projections do not retroactively replace shipped requirements.

## Implementation Summary (2026-05-19)

All R-* requirements landed in one branch:

- R-BLOCK-1 + R-BLOCK-2: extended `BlockSummary` proto (size, conventional + paid
  fee aggregates, coinbase reward, sapling/orchard counts, request-time
  `confirmations` and `is_canonical`).
- R-TX-1 + R-MEMPOOL-1: new `TransactionFeesConsumer` materializes per-txid fee
  rows; `TransactionDetail` and `MempoolActivity` enrich responses when
  prevout resolution is online.
- R-MEMPOOL-2: new `MempoolEventCountsConsumer` (first
  `DeriveMempoolConsumer`) + new `MempoolEventCounts` RPC.
- R-ADDR-1: rebuilt `TransparentAddressActivity` on top of a derive-backed
  consumer; the entry shape changed (drops mempool overlay fields, adds
  `net_value_zat`, `transparent_input_count`, `transparent_output_count`).
- R-TX-2: `RecentTransactionsConsumer` + bounded `RecentTransactions` RPC.
- R-SEARCH-1: extended `NotPubliclyIndexableReason` with mainnet/testnet
  shielded variants and the unified-without-transparent variant; classifier
  emits the right reason per network.
- R-POOL-1: confirmed `ChainValuePool.monitored` already shipped Slice 5b.
- R-OBS-1: shared `record_rpc_request` helper in `zinder-runtime` applied to
  both `zinder-explorer` and `zinder-query`.
- R-OBS-2: `/healthz` returns the capability list alongside status.
- R-OPS-1: `docs/runbooks/explorer-only-deployment.md` written.
- R-OPS-2: cursor expiry contract documented in `docs/architecture/explorer-plane.md`;
  `CursorExpiredError` proto shipped.
- R-PD-1: wire shape + capability constant shipped; RPC returns
  `Status::unimplemented` until a Rust ZIP-311 verifier ships.

Two new ADRs document the patterns that get copied:

- [ADR-0017](../adrs/0017-derive-consumer-template-and-key-codec-convention.md)
- [ADR-0018](../adrs/0018-capability-gated-optional-payload-fields.md)

## Open Questions Resolved

1. `MempoolEventCounts` window: client-clamped to `[60, 3600]` with a
   server-default of 300 seconds when the request passes zero.
2. `paid_fee_zat` for shielded inputs: never. The field is absent for
   transactions with any shielded input by privacy invariant.
3. `BlockSummary` pre-NU8/post-NU8 marker: skipped. Derivable from height
   plus network parameters; consumers compute their own chip.
4. Cursor expiry: hard error `Status::FailedPrecondition` with a typed
   `CursorExpiredError` payload. The server never silently jumps past a
   rewound range.
5. `RecentTransactions` per-network filter: skipped. Single-network per
   service, matching every other explorer RPC.

## Problem Statement

The first block-explorer consumer of the explorer plane (zexplorer M0) is feature-complete against the surface zinder exposes today, but several public panels carry visible "unavailable" labels, render weakened approximations, or force the consumer to embed protocol logic that belongs in zinder. Every gap below was observed end-to-end against the synced testnet stack during the 2026-05 integration pass; each one either degrades a public surface today or pushes work across the boundary in the wrong direction.

This document enumerates those gaps, ranks them by impact on the explorer product, and proposes the proto fields, derive-store columns, capability namespace additions, and observability work that close them. It treats the explorer plane as a single product contract rather than a stack of bugs.

## Source References

- [Zexplorer product requirements](../../../zexplorer/docs/product-requirements.md) (cited by section below).
- [ADR-0009: Explorer plane as product surface](../adrs/0009-explorer-plane-as-product-surface.md).
- [ADR-0010: Transaction public facts](../adrs/0010-transaction-public-facts.md).
- [ADR-0011: Explorer freshness envelope](../adrs/0011-explorer-freshness-envelope.md).
- [ADR-0012: Typed explorer search and privacy refusal](../adrs/0012-typed-explorer-search-and-privacy-refusal.md).
- [Explorer plane architecture](../architecture/explorer-plane.md).
- Observations recorded during the zexplorer live-stack integration session, 2026-05-15 through 2026-05-18.

## Scope

In scope:

- The `ExplorerQuery` gRPC surface served by `zinder-explorer`.
- The federated additions to `WalletQuery` that carry explorer-derived enrichment.
- The derive consumer schema and column families backing those reads.
- The capability namespace under `explorer.*`.
- The freshness envelope shape on every response.
- Observability for the explorer service (Prometheus metrics, healthz, capability advertisement).

Out of scope:

- The wallet plane primitives (`WalletQuery.TransparentAddressBalance`, snapshot APIs, etc.), except where federation with the explorer plane is explicitly affected.
- The ingest plane and source boundary internals (`zinder-source`, `zinder-ingest`, canonical artifact format) beyond what a specific explorer surface needs.
- The light-wallet compatibility surface (`zinder-compat-lightwalletd`).
- Analytics consumers other than the block explorer.
- The browser application stack that the consumer uses (Next.js, SSR, caching strategy).

## Reference Architecture

The reference consumer (zexplorer) is a Next.js BFF that composes three explorer-plane primitives per request:

1. A typed gRPC call into `ExplorerQuery` (block summary, transaction detail, search, value pool, fee summary, mempool snapshot).
2. A subscription onto `WalletQuery.ChainEvents` and `WalletQuery.MempoolEvents` bridged through SSE to the browser.
3. A capability gate (`ExplorerQuery.ServerInfo.capabilities`) checked at startup and refreshed on every chain event.

Every gap in this document corresponds to a concrete failure mode in that composition: a panel labelled "unavailable" because the capability is absent, a number labelled "approximate" because the underlying fact is computed downstream, or a round trip the consumer issues because no aggregated RPC exists.

## Requirements By Surface

### Block view

Public surfaces: `/blocks` list, `/blocks/:selector` detail, dashboard "recent blocks" panel.

#### R-BLOCK-1. `BlockSummary` must carry `size_bytes`, `fees_collected_zat`, `coinbase_reward_zat`

Now: `BlockSummary` returns `block_height`, `block_hash`, `block_time_unix_seconds`, `transaction_count`, `previous_block_hash`. The recent-blocks panel ([zexplorer PRD §Dashboard Requirements](../../../zexplorer/docs/product-requirements.md)) needs total block size, total fees collected, and coinbase reward to render a useful row; today it shows the height, time, and tx count and nothing else.

Why this belongs upstream: each of these facts is derivable from canonical artifacts at block-write time but requires walking the full transaction list per block. Computing them per-request on the consumer requires `O(transaction_count)` `TransactionDetail` calls per `BlockSummary`. On a 200-block listing the consumer would need ~600 round trips for a single page render.

Proposed change: extend `BlockSummary` (additively, as new field tags) with:

```proto
message BlockSummary {
  // ... existing fields ...
  uint64 total_size_bytes = 6;
  uint64 fees_collected_zat = 7;       // sum of (input_zat - output_zat) over non-coinbase txs
  uint64 coinbase_reward_zat = 8;      // value paid to the coinbase output
  uint32 sapling_output_count = 9;     // sum across block
  uint32 orchard_action_count = 10;    // sum across block
}
```

The aggregation runs once at consumer-derive time and the values land in the existing `block_summary` column family alongside the row already written by `BlockSummariesConsumer`. The block listing panel becomes a single range scan instead of a tree of round trips.

Capability: extend `explorer.block.summary_v1` payload coverage in `_v2`; advertise `explorer.block.summary_v2` and keep `_v1` for at least one release.

#### R-BLOCK-2. Reorg context on every block row

Now: there is no `is_canonical` or `confirmations_height` field on `BlockSummary`. The consumer cannot show a "stale" / "orphaned" badge for a row that has been rewound.

Why this belongs upstream: reorg detection is the indexer's job. The consumer should not run a height-tracker against tip to compute confidence.

Proposed change: add to `BlockSummary`:

```proto
message BlockSummary {
  // ... existing fields ...
  uint32 confirmations = 11;           // tip_height - block_height + 1
  bool is_canonical = 12;              // false during a known reorg window
}
```

The consumer derives a chip vocabulary (`canonical`, `recent`, `confirmed`, `deep`) from these two fields, matching the zexplorer PRD's "freshness chip" language for blocks.

### Transaction view

Public surfaces: `/tx/:txid`, dashboard "recent transactions" panel, search-result row.

#### R-TX-1. Paid fee and prevout amounts must be resolvable

Now: `TransactionDetail` exposes ZIP-317 conventional-floor inputs (`logical_actions`, component counts) but does not carry the actual paid fee or per-input value. The consumer can compute the conventional fee floor but cannot show "fee paid". Today the transaction detail page renders the floor with the qualifier "Conventional fee floor; paid fee unavailable without prevout resolution." This is technically truthful but it is not what the user came to see.

Why this belongs upstream: prevout resolution requires a transparent-output index. The consumer cannot rebuild that index without re-implementing the indexer. The same resolution is needed by the wallet plane's balance compute path; the work is shared.

Proposed change: add a per-input `value_zat` field to the transaction-detail wire shape, populated for transparent inputs only (shielded inputs remain `unavailable` because their values are encrypted by protocol). Then expose:

```proto
message TransactionDetail {
  // ... existing fields ...
  optional uint64 paid_fee_zat = 30;   // null when any transparent prevout is unresolved
  PrevoutResolutionStatus prevout_resolution_status = 31;
}

enum PrevoutResolutionStatus {
  PREVOUT_RESOLUTION_STATUS_RESOLVED = 0;
  PREVOUT_RESOLUTION_STATUS_PARTIAL = 1;     // some transparent prevouts missing
  PREVOUT_RESOLUTION_STATUS_UNAVAILABLE = 2; // capability absent
}
```

`paid_fee_zat` is set when every transparent input is resolved and zero shielded balance is in play (the shielded-net contribution stays inside the protocol pools and the consumer must not infer it). Otherwise the consumer falls back to displaying the conventional floor with the existing qualifier and a chip set from `prevout_resolution_status`.

Capability: `explorer.transaction.detail_v3` is the current transaction-detail contract. It covers mined prevout enrichment and transaction-intrinsic mempool rows; older semantic versions are not advertised in parallel.

#### R-TX-2. `RecentTransactions(limit)` composite RPC

The shipped `RecentTransactions` method and `recent_transactions` consumer satisfy the dashboard's bounded newest-first requirement and remove public `N+1` composition. They remain part of the native contract and advertise `explorer.transaction.recent_v1`.

The later `TransactionHistory` method is additive. It owns a separate consumer, filters, opaque paging, projection read fence, verified coverage, and exact-count scope. It advertises `explorer.transaction.history_v1` and conditionally `explorer.transaction.history_v2`; it does not rename or remove `RecentTransactions`.

### Address view

Public surfaces: `/address/:t`, transparent-address activity tab.

#### R-ADDR-1. Transparent-address activity derive view

Now: `WalletQuery.TransparentAddressBalance` returns the confirmed balance and, when an ingest-control endpoint is wired, a live-mempool overlay. There is no paginated activity surface; the consumer cannot show "transactions involving this address" without a full canonical scan.

Why this belongs upstream: the workload is read-heavy and very skewed (a small set of addresses generates almost all queries). A derive consumer keyed by address can serve activity in `O(page_size)`; without it the consumer either scans canonical artifacts per request or builds the same index downstream, duplicating storage.

Proposed change:

```proto
service ExplorerQuery {
  // ... existing methods ...
  rpc TransparentAddressActivity(TransparentAddressActivityRequest)
      returns (stream TransparentAddressActivityChunk);
}

message TransparentAddressActivityRequest {
  string address = 1;               // canonical form, validated against network
  uint32 max_entries = 2;
  optional bytes from_cursor = 3;
}

message TransparentAddressActivityChunk {
  ExplorerFreshness freshness = 1;
  bytes cursor = 2;
  repeated TransparentAddressActivityEntry entries = 3;
}

message TransparentAddressActivityEntry {
  bytes transaction_id = 1;
  uint32 block_height = 2;
  int64 block_time_unix_seconds = 3;
  // Negative when this tx sends FROM the address; positive when it receives.
  // Net is summed across all inputs and outputs touching the address.
  sint64 net_value_zat = 4;
  // Inputs and outputs touching this address; for cross-reference, not summed.
  uint32 transparent_input_count = 5;
  uint32 transparent_output_count = 6;
}
```

A new `transparent_address_activity` column family keyed by `(address_bytes, reverse_height, in_block_position)` indexes each transparent input and output by the address it touches. The derive consumer writes during block-finalize and rewinds on reorg.

Capability: `explorer.transparent_address.activity_v1`.

Privacy note: this surface is the public history of a transparent address. The consumer must reject shielded inputs at the parameter boundary (Sapling `zs*`, Orchard `zo*`, unified `u*/utest*`); zinder's `Search` already does this, and the input validator on this RPC must mirror that vocabulary. Otherwise an attacker can request `/address/:zs...` and learn that the input was syntactically valid even if it returns empty.

### Mempool view

Public surfaces: `/mempool`, dashboard mempool panel, mempool live ticker.

#### R-MEMPOOL-1. Entry-level fee data on `MempoolActivity`

Now: `MempoolActivity` returns each entry's transaction id, first-seen time, size, privacy shape, and component counts. Fee data is absent. The mempool table cannot show "fee" or "fee per logical action" columns.

Why this belongs upstream: the value is computed the same way as the block-built fee (R-TX-1), against the same prevout index. Duplicating that compute downstream means duplicating the prevout index downstream.

Proposed change: add to the `MempoolActivityEntry` shape:

```proto
message MempoolActivityEntry {
  // ... existing fields ...
  uint64 zip317_conventional_fee_zat = 10;
  optional uint64 paid_fee_zat = 11;          // null until prevout resolution online
  uint32 logical_actions = 12;                // matches the per-tx field on TransactionDetail
}
```

The conventional floor is always populated; the paid fee piggybacks on R-TX-1's prevout-resolution capability.

Capability: extend `explorer.mempool.activity_v1` payload coverage and advertise `_v2` when R-TX-1 lands.

#### R-MEMPOOL-2. Mempool event counts RPC

Now: the dashboard's "+ added, − mined, ! invalid" ticker is computed by an in-memory ring buffer in the consumer (`apps/web/lib/server/subscribers/mempool-event-counts.ts`, 4 096-entry ring), which subscribes to `WalletQuery.MempoolEvents` and counts cases. The buffer dies on process restart, doesn't survive horizontal scale-out, and forces every replica to maintain its own copy of the same stream.

Why this belongs upstream: the counts are a property of the indexer's view of the mempool, not of any one consumer. A derive consumer can write the same counts to a tiny rolling column family and serve them in one RPC.

Proposed change:

```proto
service ExplorerQuery {
  // ... existing methods ...
  rpc MempoolEventCounts(MempoolEventCountsRequest)
      returns (MempoolEventCountsResponse);
}

message MempoolEventCountsRequest {
  uint32 window_seconds = 1;     // server clamps to [60, 3600]
}

message MempoolEventCountsResponse {
  ExplorerFreshness freshness = 1;
  uint32 window_seconds = 2;     // effective value after clamp
  uint32 added_count = 3;
  uint32 mined_count = 4;
  uint32 invalidated_count = 5;
  uint32 suppressed_count = 6;
}
```

A new `mempool_event_counts` column family stores one row per UTC second carrying the four counters; expired rows roll off at write time. Total storage: `< 24 * 3600 * 24 bytes` even at 24 hours retention.

Capability: `explorer.mempool.event_counts_v1`.

### Search

#### R-SEARCH-1. Privacy refusal vocabulary completeness for properly-formed shielded inputs

Now: `ExplorerQuery.Search` correctly refuses shielded prefixes (Sapling, Orchard, unified, viewing keys) with a typed `NotPubliclyIndexable` arm. The consumer's input validator was independently audited during M0 and a defense-in-depth gap was caught (transparent-address Zod schema previously accepted shielded prefixes); both layers now reject. The remaining gap is a vocabulary detail.

Today the `NotPubliclyIndexable` reason is one of `SHIELDED_ADDRESS`, `VIEWING_KEY`, `UNKNOWN_RECEIVER_TYPECODE`. The consumer cannot distinguish "fully-formed mainnet Sapling address" from "fully-formed testnet Sapling address" from "malformed Sapling-looking input", which matters for the help-text the consumer renders ("this is a private Zcash address, not a transaction id").

Proposed change: extend the reason enum:

```proto
enum NotPubliclyIndexableReason {
  // ... existing entries ...
  NOT_PUBLICLY_INDEXABLE_REASON_SHIELDED_ADDRESS_MAINNET = 4;
  NOT_PUBLICLY_INDEXABLE_REASON_SHIELDED_ADDRESS_TESTNET = 5;
  NOT_PUBLICLY_INDEXABLE_REASON_UNIFIED_ADDRESS_NO_TRANSPARENT_RECEIVER = 6;
}
```

`SHIELDED_ADDRESS` is retained for backward compatibility and continues to be returned alongside the more specific reason. Consumers that don't update fall through to the existing copy.

This is a vocabulary refinement, not a new capability. No capability gate.

### Payment disclosure

Public surfaces: `/tools/payment-disclosure`.

#### R-PD-1. Payment-disclosure verifier capability

Now: the consumer ships a payment-disclosure verifier that runs ZIP-311 proof checks locally in the BFF (with strict redaction in the route, the logger, and the OTel span attributes). When `zinder-explorer` does not advertise an upstream verifier, the panel falls back to the local path and labels the result accordingly. There is no advertised `explorer.payment_disclosure.verify_v1` capability today; the consumer has to assume "local-only".

Why this belongs upstream: hosted verification with replay-protected nonces and rate limiting is a feature operators may want to enable. The capability advertisement lets the consumer route to the hosted path when present, fall back to local when absent, and label which path produced the answer.

Proposed change: define the capability and the RPC. The wire shape mirrors the local verifier:

```proto
service ExplorerQuery {
  rpc VerifyPaymentDisclosure(VerifyPaymentDisclosureRequest)
      returns (VerifyPaymentDisclosureResponse);
}

message VerifyPaymentDisclosureRequest {
  bytes payment_disclosure_bytes = 1;
}

message VerifyPaymentDisclosureResponse {
  ExplorerFreshness freshness = 1;
  PaymentDisclosureVerdict verdict = 2;
  // Echoed only when verdict is VALID; the same redaction rules apply
  // here as the local-verifier path.
  optional PaymentDisclosurePublicFacts public_facts = 3;
}

enum PaymentDisclosureVerdict {
  PAYMENT_DISCLOSURE_VERDICT_VALID = 0;
  PAYMENT_DISCLOSURE_VERDICT_INVALID_SIGNATURE = 1;
  PAYMENT_DISCLOSURE_VERDICT_TRANSACTION_NOT_FOUND = 2;
  PAYMENT_DISCLOSURE_VERDICT_MALFORMED = 3;
}
```

Capability: `explorer.payment_disclosure.verify_v1`. The capability is operator-opt-in (disabled by default); presence is the consumer's signal to route to the hosted path.

Privacy note: the upstream verifier must apply the same redaction rules as the local path (request bytes never logged; only the verdict and the explicit `public_facts` reach span attributes).

### Value pool view

Public surfaces: dashboard value-pool panel.

#### R-POOL-1. `monitored: bool` field on each pool entry

Now: `ValuePoolSummary` returns the current totals per pool (transparent, Sprout, Sapling, Orchard). Some pool totals are reliable (canonical-derived); some require accumulating per-block deltas that the indexer cannot guarantee under all source-boundary configurations. The consumer cannot distinguish "this number is authoritative" from "this number is best-effort" and therefore presents both with the same weight.

Proposed change:

```proto
message ValuePoolEntry {
  // ... existing fields ...
  bool monitored = N;       // true when the indexer is tracking deltas
                            // continuously since genesis; false when
                            // a bulk-catchup run is in progress or the source
                            // boundary cannot guarantee continuity.
}
```

When `monitored == false` the consumer renders the value with a "best-effort" chip.

Capability: extend `explorer.value_pool.summary_v1` payload; no version bump required (additive scalar).

### Observability

#### R-OBS-1. Per-method duration histograms on every gRPC handler

Now: `zinder-explorer` exposes `zinder_explorer_request_duration_seconds` and `zinder_explorer_request_total` with `{operation, status, error_class}` labels (landed during the 2026-05-17 architectural pass). `zinder-query` exposes the same for some methods but coverage is uneven; some handlers report only at the channel level.

Proposed change: extend the per-handler `record_request` helper used in `zinder-explorer` to `zinder-query` and apply it to every public RPC, with the same label set. Add a Prometheus alert template under `docs/runbooks/` for "explorer p95 above 1 s for 10 minutes".

#### R-OBS-2. Capability set exposed on `/healthz`

Now: `/healthz` returns ready / not-ready. To discover what an operator's node actually serves, a consumer must call `ExplorerQuery.ServerInfo`. For dashboards, monitoring, and quick `curl` checks, exposing the capability set alongside readiness avoids the gRPC round trip.

Proposed change: extend `/healthz` JSON:

```json
{
  "status": "ready",
  "build_info": { ... },
  "capabilities": [
    "explorer.server_info_v1",
    "explorer.block.summary_v1",
    "explorer.block.detail_v1",
    "explorer.transaction.detail_v3",
    "explorer.mempool.summary_v1",
    "explorer.mempool.activity_v1",
    "explorer.search_v1",
    "explorer.fee.summary_v1",
    "explorer.value_pool.summary_v1"
  ]
}
```

The same array is returned by `ServerInfo`; this is a discoverability convenience for HTTP-only tooling.

### Operations

#### R-OPS-1. Per-network `zinder-explorer`-only deployment runbook

Now: the runbooks under `docs/runbooks/` cover the full stack (ingest, query, explorer, compat). Operators who want to run only `zinder-explorer` against an existing `zinder-query` deployment (the topology zexplorer expects in production) have no end-to-end recipe.

Proposed change: add `docs/runbooks/explorer-only-deployment.md` covering: required env vars, `zinder-query` endpoint discovery, storage path selection, capability advertisement check, sample systemd unit, sample Docker Compose service.

#### R-OPS-2. Cursor expiry policy

Now: cursors returned by `BlockSummariesInRange`, `MempoolActivity`, and (per this PRD) `RecentTransactions` and `TransparentAddressActivity` are opaque, but their lifetime semantics are undefined. The zexplorer PRD lists this as open question #5.

Proposed decision: cursors are valid for the lifetime of the column-family snapshot at issue time. The server rejects a cursor that references a height range no longer in the store (reorged out) with a typed `CURSOR_EXPIRED` error reason and a hint pointing at the new tip. Consumers re-issue from the start of the range or jump to tip per the hint.

Document this in the explorer-plane architecture doc and surface it as `CursorExpiredError` in the proto.

## Capability Namespace Additions

| Capability string | Owner method | Phase |
| ----- | ----- | ----- |
| `explorer.block.summary_v2` | `BlockSummariesInRange` (extended payload) | 1 |
| `explorer.transaction.detail_v3` | `TransactionDetail` (paid fee, prevout status) | 2 |
| `explorer.transaction.recent_v1` | `RecentTransactions` | 3 |
| `explorer.transaction.history_v1` | `TransactionHistory` | additive |
| `explorer.transparent_address.activity_v1` | `TransparentAddressActivity` | 2 |
| `explorer.mempool.activity_v2` | `MempoolActivity` (entry fees) | 2 |
| `explorer.mempool.event_counts_v1` | `MempoolEventCounts` | 2 |
| `explorer.payment_disclosure.verify_v1` | `VerifyPaymentDisclosure` | 3 |

Existing `_v1` capabilities are not retired by this PRD; consumers gate per-surface and migrate at their own pace.

## Implementation Milestones

### Phase 1 (proto quick wins, no derive changes)

- R-BLOCK-1: extend `BlockSummary` with size, fees, coinbase reward, sapling output / orchard action counts. Compute in `BlockSummariesConsumer` at block-finalize. Single-PR change to proto, consumer, and capability advertisement.
- R-BLOCK-2: extend `BlockSummary` with `confirmations` and `is_canonical`. Reuse the existing tip-height tracker.
- R-POOL-1: extend `ValuePoolEntry` with `monitored`. Populate from the source-boundary configuration.
- R-OBS-1: extend the per-handler timing helper to every `zinder-query` RPC.
- R-OBS-2: extend `/healthz` payload to include capabilities.
- R-OPS-1: runbook.
- R-OPS-2: cursor expiry decision recorded in the explorer-plane architecture doc and added to the proto.

Acceptance: the dashboard's recent-blocks panel renders size, fee, coinbase reward, and a freshness chip for every row without falling back to "unavailable". The value-pool panel shows a "best-effort" chip when the source boundary is not continuous.

### Phase 2 (derive store extensions)

- R-TX-1: prevout-resolution capability and `paid_fee_zat` field.
- R-MEMPOOL-1: entry-level fee fields on `MempoolActivity`.
- R-MEMPOOL-2: `MempoolEventCounts` RPC and its column family.
- R-ADDR-1: `TransparentAddressActivity` RPC and its column family.
- R-SEARCH-1: privacy-refusal reason enum extension.

Acceptance: the transaction detail page shows the paid fee for every transparent-input-resolved transaction; the address page renders activity history for any transparent address; the dashboard mempool ticker is sourced from the upstream RPC instead of the in-memory ring buffer.

### Phase 3 (composite RPCs and hosted verification)

- R-TX-2: `RecentTransactions` RPC and its column family.
- R-PD-1: `VerifyPaymentDisclosure` RPC and capability.

Acceptance: the dashboard's recent-transactions panel renders from a single round trip; the payment-disclosure tool routes to the hosted verifier when advertised.

## Acceptance Criteria

A block-explorer consumer can render all public dashboard panels, the transaction detail page, the block detail page, the transparent address page, the mempool table, the search page, and the payment-disclosure tool without any panel labelled "unavailable on this node" except when the operator has explicitly disabled the corresponding capability. No consumer-side aggregation walks more than one `ExplorerQuery` round trip per public page render. The capability set returned by `ServerInfo` is sufficient to drive every gate the consumer needs.

## Open Questions

1. Should `MempoolEventCounts` (R-MEMPOOL-2) expose a fixed window (default 5 minutes) or accept a client window with server-side clamping? The current proposal is the latter; the alternative simplifies the column family but loses dashboard flexibility.
2. Should `paid_fee_zat` (R-TX-1) be populated for transactions with shielded inputs by inferring from the net pool delta? The privacy answer is no (the consumer would derive a per-tx shielded value that the protocol takes pains to hide), but the question recurs every time someone asks "why is the fee blank for this transaction".
3. Should `BlockSummary` carry a pre-NU8 / post-NU8 marker to ease upgrade-window UX? The structural answer is that consumers can derive this from height plus network parameters; the convenience answer is that a chip avoids consumer-side network-parameter logic.
4. Cursor expiry (R-OPS-2): is rejecting expired cursors with `CURSOR_EXPIRED` the right contract, or should the server transparently jump to the closest valid position and signal "skipped" in the freshness envelope? The current proposal favours the explicit error; the alternative is more forgiving but masks the rewind.
5. Should `RecentTransactions` (R-TX-2) expose a per-network filter, or always read from the configured network? The current proposal is single-network per service (matching every other RPC).

## Non-Goals (Explicit)

- This PRD does not propose server-side shielded address scanning, shielded balance queries, or any RPC that takes a viewing key. The privacy invariant from [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) holds: server-side viewing-key scanning is out of scope by product invariant.
- This PRD does not propose an EVM-style "internal transaction" or "trace" surface. Zcash transactions do not have nested execution.
- This PRD does not propose a wallet-tracking surface (watch addresses, alerts, push notifications). That is wallet plane work, not explorer plane work.
- This PRD does not propose holding archival data beyond what the canonical store retains; explorer derive views are rebuildable projections of canonical state, not an independent archive.
