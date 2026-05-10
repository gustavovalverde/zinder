# ADR-0013: Derive-Plane Instantiation and Transparent Address Balance Read-Path

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Product | Zinder |
| Domain | Derive-plane operational topology, consumer SDK contract, transparent address balance API |
| Related | [Derive plane](../architecture/derive-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Chain events](../architecture/chain-events.md), [Public interfaces](../architecture/public-interfaces.md), [Service boundaries](../architecture/service-boundaries.md), [Extending artifacts](../architecture/extending-artifacts.md), [ADR-0008](0008-consumer-neutral-wallet-data-plane.md), [ADR-0010](0010-mempool-topology-and-retention.md), [ADR-0011](0011-derive-plane-federation-pattern.md), [ADR-0014](0014-compute-at-read-time-canonical-reads.md), [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md) |

## Context

Two prerequisites for shipping the explorer-side balance API are independent of the API itself:

1. **A real `services/zinder-derive` deployable.** Before M5, `services/zinder-derive` was named in `RFC-0001` and `derive-plane.md` but had no source files. Every derive-consumer architecture decision (separate process, separate RocksDB, capability namespace, cursor persistence, ops endpoints) was un-shipped architecture.
2. **A consumer SDK contract.** Future derive consumers (analytics, tax, future explorer views) need the same cold-start path: read canonical history via `WalletQuery.compact_block_range` for the gap before retained chain events begin, then attach to the live `WalletQuery.ChainEvents` stream. Without a named contract, every consumer would re-invent the backfill-then-attach plumbing differently.

M5 instantiates the derive plane (`zinder-derive` is now a real fourth deployable) and ships the first user-visible derive consumer (`TransparentAddressBalance` under `ExplorerQuery` and federated through `WalletQuery`). The federation primitive that lets a federated method live on `WalletQuery` is captured in [ADR-0011](0011-derive-plane-federation-pattern.md). The compute-at-read-time pattern that the balance handler uses is captured in [ADR-0014](0014-compute-at-read-time-canonical-reads.md), which lists M5 Slice B as the first worked example. This ADR locks the M5-specific decisions that are not covered by either of those: the operational topology, the consumer SDK contract, and the balance API's wire-shape choice.

Without an ADR, M5's decisions stay frozen in a spec that the project's lifecycle rule says should be deleted on ship ([docs/README.md §Document lifecycles](../README.md#document-lifecycles)). Future contributors weighing "should this new derive consumer get its own RocksDB?" or "what cap should a batched address request use?" need a named contract to read.

## Decision

`services/zinder-derive` is a real fourth deployable. Each derive consumer follows a uniform topology, a uniform consumer SDK contract, and a uniform set of capability advertisement rules. The first user-visible consumer (`TransparentAddressBalance`) ships under that contract.

### Operational topology

Each derive consumer is a separate process from `zinder-ingest` and `zinder-query`, with its own RocksDB instance, its own gRPC service surface (`ExplorerQuery` for the explorer consumer; future consumers add their own `*Query` services), and its own ops endpoints (`/healthz`, `/readyz`, `/metrics`). Configuration uses the `[derive.{consumer}]` namespace (e.g. `[derive.explorer]`).

Sensitive fields are excluded from environment variables per [Public interfaces §Configuration Conventions](../architecture/public-interfaces.md#configuration-conventions). Readiness causes follow the typed-cause convention: `Initializing`, `Backfilling`, `Attaching`, `LiveCatchingUp`, `Ready`, `ChainEventsUnavailable`. Prometheus metrics use the `zinder_derive_*` prefix.

The federated path between `zinder-query` and `zinder-derive` is owned by the [ADR-0011](0011-derive-plane-federation-pattern.md) `DeriveProxy<Client>` primitive; this ADR does not duplicate that surface.

### Consumer SDK contract: backfill-then-attach

A fresh derive consumer's input is the union of:

- **Channel C**: `WalletQuery` canonical reads (`compact_block_range`, `transactions_in_range`) for historical replay from genesis to a recent height.
- **Channel A**: `WalletQuery.ChainEvents` from `from_cursor = None` (which delivers events newer than `chain_event_retention_hours`, default 168h ≈ 8064 blocks) for steady-state.

The transition between Channel C and Channel A must not drop or duplicate events. The `backfill_then_attach` consumer-SDK helper in `services/zinder-derive/src/consumer/backfill.rs` is the canonical bootstrap path:

1. Read the persisted consumer cursor from its own RocksDB. If absent, set `last_processed_height = BlockHeight::new(0)`.
2. Open `WalletQuery.ChainEvents` with `from_cursor = None` and read the first envelope to discover `oldest_retained_height`.
3. If `last_processed_height < oldest_retained_height - reorg_window_blocks`: enter Channel C backfill mode, reading `compact_block_range(last_processed_height..=oldest_retained_height - reorg_window_blocks)` block by block, applying each as a synthetic `ChainCommitted` event to the consumer's accumulator.
4. Once `last_processed_height >= oldest_retained_height - reorg_window_blocks`: attach to the live `ChainEvents` stream, using the first envelope's cursor as the resume point.
5. Persist the consumer cursor after every applied envelope using a RocksDB `WriteBatch` that bundles cursor write + accumulator writes atomically.

The `DeriveConsumer` and `DeriveMempoolConsumer` traits in `services/zinder-derive/src/consumer/` define `apply_chain_*` / `apply_mempool_event` hooks. Reorg correctness is the consumer's responsibility: each consumer decides how to revert its derived state on `ChainReorged`. The pattern is uniform across consumers; the bookkeeping is the consumer's.

The MempoolEvents subscription helper (`services/zinder-derive/src/consumer/mempool_events.rs`) mirrors the chain-events subscriber for consumers that need live mempool delivery. Cursor persistence shares the same `cursor` column family under a different consumer name.

### Capability namespace

Derive-plane capabilities use the `derive.{consumer}.{capability}_v{N}` prefix even when the RPC method is federated under `WalletQuery`. The explorer consumer advertises:

- `derive.explorer.ready_v1`: indicates `zinder-derive` is up and the consumer SDK is alive. Advertised on `ExplorerQuery.ServerInfo`.
- `derive.explorer.transparent_balance_v1`: indicates the balance handler is reachable. Advertised on **both** `ExplorerQuery.ServerInfo` (direct) and `WalletQuery.ServerInfo` (federated, only when `zinder-derive` is reachable per the [ADR-0011](0011-derive-plane-federation-pattern.md) readiness gauge).

There is no `wallet.*` capability for balance. Native consumers gating on capability strings see `derive.explorer.transparent_balance_v1` regardless of which gRPC surface they use. The capability-coverage test (`crates/zinder-client/tests/integration/capability_coverage.rs`) asserts the `derive.*` namespace maps to `ExplorerQuery` methods AND federated `WalletQuery` methods.

### Balance is a derive consumer, not a canonical artifact

[Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family) names "precomputed totals table for explorer dashboards" as a `zinder-derive` materialized view, not a canonical artifact family. M5 honors this: no balance column family lives in `zinder-store`. The balance handler reads canonical UTXO artifacts (M4) and M3 mempool point lookups, sums at the gRPC adapter, and returns a structured response.

The decision procedure from [derive-plane.md §When to use the derive plane](../architecture/derive-plane.md#when-to-use-the-derive-plane) confirms: balance does not affect a wallet's ability to sync, scan, or broadcast (Zashi computes balance client-side from UTXOs); therefore balance lives in the derive plane.

Building balance canonically would establish "aggregations live canonically when convenient," and every future contributor would cite M5 as precedent. [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md) names this anti-pattern explicitly; M5 is the deliberate response.

### Storage and read-path

The shipped balance read path is Shape C compute-at-read-time per [ADR-0014](0014-compute-at-read-time-canonical-reads.md). The derive-plane handler in `services/zinder-derive/src/grpc/adapter.rs::compute_transparent_address_balance` walks each address in the request, calls `WalletQuery.TransparentAddressUtxos` for confirmed UTXOs, calls `WalletQuery.TransparentMempoolOutputsByAddress` for unconfirmed funding, and per-UTXO calls `WalletQuery.TransparentMempoolSpendByOutpoint` to subtract pending spends. The first non-empty `chain_epoch` from the federated calls binds the response.

A future Shape A column family (`transparent_address_balance` keyed by `(network, address_script_hash, block_height_be, chain_epoch_id)`) is reserved as a read-path optimization that does not change the public wire shape or the capability string. Promotion is governed by [ADR-0014](0014-compute-at-read-time-canonical-reads.md).

### Wire shape: structured `confirmed_zat` plus signed `unconfirmed_delta_zat`

The native balance response carries:

```proto
message TransparentAddressBalance {
  uint64 confirmed_zat = 1;
  int64 unconfirmed_delta_zat = 2;
  uint32 address_count = 3;
  ChainEpoch chain_epoch = 4;
}
```

`unconfirmed_delta_zat` is signed because pending spends can exceed pending receives (a miner address with pending outflows greater than pending inflows shows a negative delta). Saturating arithmetic at the construction site guarantees the wire never carries an overflowed value. `address_count` lets consumers distinguish "zero balance because none of these addresses received anything" from "this exact address has zero outputs."

Esplora and ElectrumX both expose confirmed and unconfirmed totals separately; the lightwalletd-era `Balance { value_zat: int64 }` cannot. The compat shim's `GetTaddressBalance` projects `confirmed_zat as int64` into `lightwalletd::Balance.value_zat` and drops `unconfirmed_delta_zat` for legacy clients; native and explorer clients see the structured shape.

The address list is hard-capped at 256 entries per request to bound the federated fanout. The cap is the convention every batched wallet-plane read uses (mirrored by M6's `MAX_TRANSPARENT_PREVOUTS_PER_REQUEST`).

### Streaming form is dropped from the native API

`GetTaddressBalanceStream` has zero call sites across the surveyed wallet ecosystem (Zashi, Android SDK, Zallet, librustzcash, surveyed third-party wallets). Native `WalletQuery` and `ExplorerQuery` do not expose a streaming variant. The compat shim implements `GetTaddressBalanceStream` as a per-address loop over the unary form for legacy lightwalletd clients only.

This refuses [Public interfaces §Contract Hygiene](../architecture/public-interfaces.md#contract-hygiene) violation: a native streaming form that no consumer calls is exactly what the rule against vestigial APIs is meant to prevent.

### Out of scope (reserved)

- **Historical balance (`balance_at_height`)**: no production indexer ships balance-at-height as a first-class endpoint (Esplora, Blockchair, ElectrumX, BlockCypher all punt). The `at_epoch` field is preserved per the standard chain-epoch pin contract; arbitrary historical-height queries are reserved for a future spec when a real consumer requires them.
- **Balance-change subscription** (`BalanceChanged` event variant): reserved with no producer. ElectrumX's `subscribe_addresses` uses status hashes (not balance values) because pushing balance values has unbounded fanout. Real-world subscription patterns are explorer pages polling on demand and wallet UIs recomputing on `ChainEvents`; neither needs a server-pushed balance event.
- **Per-address breakdown in the response**: every consumer surveyed sums across the address list and presents one number. A future explorer needing per-address breakdown adds `repeated TransparentAddressBalanceEntry per_address = 5;`.
- **Sink-only Shape 3 derive consumers** (writing to Postgres, ClickHouse, S3): the architecture supports it; M5 does not ship a reference. The cursor protocol is sufficient for an operator to build their own.
- **Standardized derive-consumer SDK as a separate crate**: helpers live in `services/zinder-derive/src/consumer/` until a second consumer beyond explorer justifies extraction.

## Consequences

- The derive plane is a real fourth deployable. Future consumers (analytics, tax) drop in via the same SDK + capability + topology contract.
- The consumer SDK contract is uniform: every consumer follows backfill-then-attach with cursor persistence + atomic write batches. No per-consumer reinvention.
- The balance API ships under `derive.explorer.transparent_balance_v1`, federated through `WalletQuery` per [ADR-0011](0011-derive-plane-federation-pattern.md). Consumers gate on the capability string regardless of which gRPC surface they use.
- The balance read-path is Shape C; Shape A is reserved per [ADR-0014](0014-compute-at-read-time-canonical-reads.md). Operational storage cost is zero today.
- The streaming `GetTaddressBalanceStream` is native-API-absent; the compat shim retains it for legacy lightwalletd clients only.

## Alternatives considered

**Build balance as a canonical artifact in `zinder-store`.** Reject. Establishes "aggregations live canonically when convenient" precedent; explicit anti-pattern per [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md). The cookbook ([Extending artifacts](../architecture/extending-artifacts.md)) names this exact case as a derive workload.

**Ship balance as a streaming RPC in the native API.** Reject. Zero ecosystem call sites; vestigial proto. Compat shim retains the streaming form for the lightwalletd contract; native API stays free of dead surfaces.

**Use a `wallet.*` capability for the federated balance method.** Reject. Conflates wallet-plane and derive-plane lifecycles. The capability string identifies the data source, not the gRPC surface; the derive-plane namespace makes operator deployment topology explicit at the consumer-gating layer.

**Pre-compute a per-block running-totals accumulator (Shape A) at ingest time.** Reject for v1; reserved per [ADR-0014](0014-compute-at-read-time-canonical-reads.md). Commits ~55-60 GB of mainnet storage today for a sub-millisecond gain on a workload no consumer has surfaced. Promotion path is documented; public contract does not change.

**Combine confirmed and unconfirmed into one signed `int64` total.** Reject. Loses information consumers need (explorer UI showing "1.5 ZEC confirmed, 0.05 ZEC pending"). The lightwalletd contract collapses; the native API does not.

## Cross-references

- [Derive plane](../architecture/derive-plane.md): the architecture doc this ADR makes durable.
- [Wallet data plane §Transparent Address Balance](../architecture/wallet-data-plane.md#transparent-address-balance): the wire-shape documentation.
- [ADR-0008](0008-consumer-neutral-wallet-data-plane.md): the consumer-neutral wallet data plane that the federation pattern preserves.
- [ADR-0010](0010-mempool-topology-and-retention.md): the M3 mempool surfaces D6 composes for the unconfirmed delta.
- [ADR-0011](0011-derive-plane-federation-pattern.md): the federation primitive (`DeriveProxy<Client>`, readiness gauge, capability gating).
- [ADR-0014](0014-compute-at-read-time-canonical-reads.md): the compute-at-read-time pattern this ADR's Slice B applies; M5 Slice B is its first worked example.
- [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md): the upstream anti-pattern this ADR avoids by keeping balance out of canonical storage.
- [Public interfaces §Capability discovery](../architecture/public-interfaces.md#capability-discovery): the `derive.*` capability namespace contract.
- [Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family): the cookbook rule that places balance in the derive plane.
