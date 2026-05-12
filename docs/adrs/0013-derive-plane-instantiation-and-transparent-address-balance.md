# ADR-0013: Derive-Plane Instantiation and Transparent Address Balance Read-Path

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-10) |
| Product | Zinder |
| Domain | Derive-plane operational topology, transparent address balance API |
| Related | [Derive plane](../architecture/derive-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Chain events](../architecture/chain-events.md), [Public interfaces](../architecture/public-interfaces.md), [Service boundaries](../architecture/service-boundaries.md), [Extending artifacts](../architecture/extending-artifacts.md), [ADR-0008](0008-consumer-neutral-wallet-data-plane.md), [ADR-0010](0010-mempool-topology-and-retention.md), [ADR-0011](0011-derive-plane-federation-pattern.md), [ADR-0014](0014-compute-at-read-time-canonical-reads.md), [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md) |

## Context

`services/zinder-derive` is the fourth Zinder deployable. It runs in its own process, owns its own `RocksDB` instance, and reads the canonical chain and mempool surfaces from `zinder-query` over gRPC. The first user-visible derive consumer is `TransparentAddressBalance` under `ExplorerQuery`, federated through `WalletQuery`.

Three concerns are independent of the federation primitive ([ADR-0011](0011-derive-plane-federation-pattern.md)) and the storage-shape pattern ([ADR-0014](0014-compute-at-read-time-canonical-reads.md)):

- The operational topology: per-consumer process, per-consumer `RocksDB`, capability namespace, configuration shape, ops endpoints.
- The decision to model balance as a derive consumer rather than as a canonical artifact.
- The transparent-address balance wire shape (signed unconfirmed delta, batched address cap, no streaming form).

This ADR locks those decisions.

## Decision

Each derive consumer runs as its own process under a uniform topology and capability convention. The first user-visible consumer (`TransparentAddressBalance`) ships under that contract.

### Operational topology

Each derive consumer is a separate process from `zinder-ingest` and `zinder-query`, with its own RocksDB instance, its own gRPC service surface (`ExplorerQuery` for the explorer consumer; future consumers add their own `*Query` services), and its own ops endpoints (`/healthz`, `/readyz`, `/metrics`). Configuration uses the `[derive.{consumer}]` namespace (e.g. `[derive.explorer]`).

Sensitive fields are excluded from environment variables per [Public interfaces §Configuration Conventions](../architecture/public-interfaces.md#configuration-conventions). Readiness causes follow the typed-cause convention: `Initializing`, `Backfilling`, `Attaching`, `LiveCatchingUp`, `Ready`, `ChainEventsUnavailable`. Prometheus metrics use the `zinder_derive_*` prefix.

The federated path between `zinder-query` and `zinder-derive` is owned by the [ADR-0011](0011-derive-plane-federation-pattern.md) `DeriveProxy<Client>` primitive; this ADR does not duplicate that surface.

### Capability namespace

Derive-plane capabilities use the `derive.{consumer}.{capability}_v{N}` prefix even when the RPC method is federated under `WalletQuery`. The explorer consumer advertises:

- `derive.explorer.server_info_v1`: indicates `zinder-derive` is up. Advertised on `ExplorerQuery.ServerInfo`.
- `derive.explorer.transparent_balance_v1`: indicates the balance handler is reachable. Advertised on **both** `ExplorerQuery.ServerInfo` (direct) and `WalletQuery.ServerInfo` (federated, only when `zinder-derive` is reachable per the [ADR-0011](0011-derive-plane-federation-pattern.md) readiness gauge).

There is no `wallet.*` capability for balance. Native consumers gating on capability strings see `derive.explorer.transparent_balance_v1` regardless of which gRPC surface they use. The capability-coverage test asserts the `derive.*` namespace maps to `ExplorerQuery` methods and federated `WalletQuery` methods.

### Balance is a derive consumer, not a canonical artifact

[Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family) names "precomputed totals table for explorer dashboards" as a `zinder-derive` materialized view, not a canonical artifact family. No balance column family lives in `zinder-store`. The balance handler reads canonical UTXO artifacts and live mempool point lookups, sums at the gRPC adapter, and returns a structured response.

The decision procedure from [derive-plane.md §When to use the derive plane](../architecture/derive-plane.md#when-to-use-the-derive-plane) confirms: balance does not affect a wallet's ability to sync, scan, or broadcast (Zashi computes balance client-side from UTXOs); therefore balance lives in the derive plane.

Building balance canonically would establish "aggregations live canonically when convenient," and every future contributor would cite this as precedent. [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md) names that anti-pattern explicitly.

### Storage and read-path

The balance read path is compute-at-read-time per [ADR-0014](0014-compute-at-read-time-canonical-reads.md). The derive-plane handler in `services/zinder-derive/src/grpc/adapter.rs::compute_transparent_address_balance` walks each address in the request, calls `WalletQuery.TransparentAddressUtxos` for confirmed UTXOs, calls `WalletQuery.TransparentMempoolOutputsByAddress` for unconfirmed funding, and per-UTXO calls `WalletQuery.TransparentMempoolSpendByOutpoint` to subtract pending spends. The first non-empty `chain_epoch` from the federated calls binds the response.

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

The address list is hard-capped at 256 entries per request to bound the federated fanout. Every batched wallet-plane read uses the same cap.

### Streaming form is dropped from the native API

`GetTaddressBalanceStream` has zero call sites across the surveyed wallet ecosystem (Zashi, Android SDK, Zallet, librustzcash, surveyed third-party wallets). Native `WalletQuery` and `ExplorerQuery` do not expose a streaming variant. The compat shim implements `GetTaddressBalanceStream` as a per-address loop over the unary form for legacy lightwalletd clients only.

A native streaming form that no consumer calls is exactly the kind of vestigial API the [Public interfaces §Contract Hygiene](../architecture/public-interfaces.md#contract-hygiene) rule prohibits.

### Out of scope (reserved)

- **Historical balance (`balance_at_height`)**: no production indexer ships balance-at-height as a first-class endpoint (Esplora, Blockchair, ElectrumX, BlockCypher all punt). The `at_epoch` field is preserved per the standard chain-epoch pin contract; arbitrary historical-height queries are reserved for a future spec when a real consumer requires them.
- **Balance-change subscription** (`BalanceChanged` event variant): reserved with no producer. ElectrumX's `subscribe_addresses` uses status hashes (not balance values) because pushing balance values has unbounded fanout. Real-world subscription patterns are explorer pages polling on demand and wallet UIs recomputing on `ChainEvents`; neither needs a server-pushed balance event.
- **Per-address breakdown in the response**: every consumer surveyed sums across the address list and presents one number. A future explorer needing per-address breakdown adds `repeated TransparentAddressBalanceEntry per_address = 5;`.
- **Sink-only Shape 3 derive consumers** (writing to Postgres, ClickHouse, S3): the architecture supports it; this ADR does not ship a reference. The cursor protocol is sufficient for an operator to build their own.
- **Standardized derive-consumer SDK as a separate crate**: helpers live in `services/zinder-derive/src/consumer/` until a second consumer beyond explorer justifies extraction.

## Consequences

- The derive plane is a real fourth deployable. Future consumers (analytics, tax) drop in via the same capability + topology contract.
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
- [ADR-0010](0010-mempool-topology-and-retention.md): the mempool surfaces composed for the unconfirmed delta.
- [ADR-0011](0011-derive-plane-federation-pattern.md): the federation primitive (`DeriveProxy<Client>`, readiness gauge, capability gating).
- [ADR-0014](0014-compute-at-read-time-canonical-reads.md): the compute-at-read-time pattern the balance handler applies.
- [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md): the upstream anti-pattern this ADR avoids by keeping balance out of canonical storage.
- [Public interfaces §Capability discovery](../architecture/public-interfaces.md#capability-discovery): the `derive.*` capability namespace contract.
- [Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family): the cookbook rule that places balance in the derive plane.
