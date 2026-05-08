# M5: Transparent-address balance and derive-plane instantiation

| Field | Value |
| ----- | ----- |
| Status | Decisions locked; implementation unstarted. Bootstraps `services/zinder-derive` (currently zero source files) |
| Created | 2026-05-08 |
| Product | Zinder |
| Audience | Zinder maintainers, explorer developers, wallet developers, future analytics integrators |
| Related | [PRD-0001](../prd-0001-zinder-indexer.md), [Derive plane](../architecture/derive-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Chain events](../architecture/chain-events.md), [Service boundaries](../architecture/service-boundaries.md), [Public interfaces](../architecture/public-interfaces.md), [Extending artifacts](../architecture/extending-artifacts.md), [M4 spec](m4-transparent-address.md), [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md), [ADR-0010](../adrs/0010-mempool-topology-and-retention.md) |

## Context

[M4 §D7](m4-transparent-address.md#d7-balance-and-balance-stream-surfaces-are-out-of-m4) deferred transparent-address balance to M5. M5 is the milestone that closes the explorer-side of the transparent-address surface AND instantiates `services/zinder-derive` as a real deployable. The cookbook ([Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family)) names "precomputed totals table for explorer dashboards" as a derive-plane workload, not a canonical artifact family. The derive plane is fully designed in [derive-plane.md](../architecture/derive-plane.md) but has zero implementation today: there is no `services/zinder-derive/` directory, no consumer SDK, no separate ops surface. M5 makes it real.

What real consumers ask for, locked from research:

- **`GetTaddressBalance`** is consumed by every Zaino-backed wallet path. It currently delegates to Zebra's `ReadStateService::AddressBalance` (finalized state only, no mempool view) and drops the `received` total at the lightwalletd compat boundary because the `Balance` proto only carries `value_zat`. M5 must do better than Zaino.
- **`GetTaddressBalanceStream`** has zero call sites across Zashi, Android SDK, Zallet, librustzcash, and 20+ third-party wallets. It is vestigial proto. Native API drops it; compat shim implements as a per-address loop over the unary form for legacy clients.
- **Real explorer access pattern** (`devdotbo/zcash-explorer` consuming Zaino): one balance call per address page view, no caching, sub-second response budget. No historical balance, no mempool split today. The mempool split is what M5 unlocks that Zaino cannot.
- **Esplora and ElectrumX** both expose `confirmed` and `unconfirmed` separately. Lightwalletd's `Balance { value_zat: int64 }` cannot. M5's native API mirrors Esplora's split.
- **Balance-at-height** (historical balance) is not served by any production indexer (Esplora documents you reconstruct from UTXOs at a height; Blockchair documents you self-join their outputs table). M5 defers it.

What is **not** yet shipped that M5 depends on:

- `services/zinder-derive/` does not exist. No `Cargo.toml`, no `src/main.rs`. The crate is named in `RFC-0001`, `service-boundaries.md`, and `derive-plane.md` but has never been instantiated.
- No derive-consumer SDK. The only existing consumer-facing types in `zinder-client` are `ChainIndex`-shaped client traits; nothing covers ChainEvents+ChainEpochReadApi backfill-then-attach for derive consumers.
- `WalletQuery.ChainEvents` reorg delivery has store-level test coverage but no gRPC-level integration test (the live mempool reorg flow is `#[ignore]`'d). M5 closes that gap as part of Slice A's foundational work.
- The `derive.{consumer}.{capability}_v{N}` capability namespace is reserved in [public-interfaces.md](../architecture/public-interfaces.md) but no string is yet advertised.

## Decisions

### D1. Two independently shippable slices

M5 splits into two slices that ship in order:

- **Slice A: derive-plane bootstrap.** Instantiates `services/zinder-derive` as a deployable binary with its own RocksDB, its own gRPC service `ExplorerQuery`, its own ops endpoints (`/healthz`, `/readyz`, `/metrics`), its own config block (`[derive.explorer]`), and the consumer infrastructure for ChainEvents+ChainEpochReadApi backfill-then-attach + MempoolEvents subscription. Exposes a single trivial capability `derive.explorer.ready_v1` to validate the wiring end-to-end. No user-visible product.
- **Slice B: balance accumulator and the user-visible API.** Adds `TransparentBalanceAccumulator` as the first real derive consumer, the explorer-side `ExplorerQuery.TransparentAddressBalance` RPC, the `WalletQuery.TransparentAddressBalance` federated RPC (per [derive-plane.md Shape 2](../architecture/derive-plane.md#shape-2--federated-under-walletquery)), the compat shim's `GetTaddressBalance` and `GetTaddressBalanceStream`, and the `ChainIndex::transparent_address_balance` typed method. Lights up `derive.explorer.transparent_balance_v1`.

**Why:** Slice A has no functional product but is the prerequisite for every future derive workload (M6+ explorer/analytics). Shipping it first lets reviewers focus on the cross-cutting infrastructure (cursor persistence, RocksDB wrapper, ops surface, capability advertisement) without being distracted by balance-specific accumulator logic. Slice B then becomes pure feature work on top of a tested foundation.

**How to apply:** Land Slice A end-to-end before starting Slice B's accumulator code. The capability-coverage test (created in M4 Slice A) is extended to enumerate `derive.*` capabilities in addition to `wallet.*`.

### D2. M5 promotes derive-plane.md to ADR-0012

When both slices ship, the design decisions about `services/zinder-derive` (separate process, separate RocksDB, federated under WalletQuery via Shape 2, capability namespace, cursor persistence contract, replayability rule) promote from architecture-doc status to a hard ADR. The spec is then deleted.

**Why:** ADRs lock contracts that have been proven in code. derive-plane.md has stood as un-shipped architecture for several milestones; M5 is the first time it gets exercised. The ADR captures what was decided when there was a real implementation to constrain.

**How to apply:** ADR-0011 (M4 promotion) and ADR-0012 (M5 derive-plane instantiation) land in their respective ship PRs. Numbering is contiguous; M5 reserves `0012`.

### D3. Balance is a derive consumer, not a canonical artifact

[Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family) line 19 names "precomputed totals table for explorer dashboards" as a `zinder-derive` materialized view, not a canonical artifact family. M5 honors this. The cookbook is not amended; the rule was already correct.

The decision procedure from [derive-plane.md §When to use the derive plane](../architecture/derive-plane.md#when-to-use-the-derive-plane) confirms: balance does not affect a wallet's ability to sync, scan, or broadcast (Zashi computes balance client-side from UTXOs per [M4 §D7](m4-transparent-address.md#d7-balance-and-balance-stream-surfaces-are-out-of-m4)); therefore balance lives in the derive plane.

**Why:** Building balance canonically would establish "aggregations live canonically when convenient," and every future contributor would cite M5 as precedent. The cookbook was authored with this precise anti-pattern in mind ([Lessons from Zaino Pattern 4](../reference/lessons-from-zaino.md#pattern-4-storage-as-a-linear-migration-ladder)). The derive plane was designed to absorb workloads exactly like this; M5 is the validation that the design works.

**How to apply:** No new column family lands in `crates/zinder-store`. The balance accumulator's storage lives entirely inside `services/zinder-derive`'s own RocksDB instance. `WalletQueryApi::transparent_address_balance` proxies to `zinder-derive`'s `ExplorerQuery.TransparentAddressBalance` over gRPC; `zinder-store` is unaffected.

### D4. Storage shape: per-block running totals (Shape A)

The balance accumulator stores one row per `(network, address_script_hash, block_height_be)` triple at every height the address was touched, with the trailing 8-byte `chain_epoch_id` matching M4's dynamic-filter pattern. The payload carries running totals: `confirmed_zat: u64`, `funded_count: u64`, `spent_count: u64`. Reorg semantics use the same dynamic-filter visibility model as M4 D9: rows are written and never physically deleted, visibility is enforced at read time via `source_epoch <= chain_epoch.id` plus `block_is_visible(height, expected_hash)`.

**Why:** Three storage shapes were considered (running totals, deltas, UTXO-scan-and-sum). Running totals win on every axis the explorer audience cares about:

| Shape | Storage (mainnet) | Read at tip (143K-tx miner) | Balance-at-height | Mempool overlay |
| ----- | ----------------- | --------------------------- | ----------------- | --------------- |
| **A. Running totals** | ~55-60 GB compressed | One reverse-prefix seek + visibility | Single seek; same cost | Sum mempool deltas |
| B. Per-block deltas | ~55-60 GB compressed | Sum 100K+ delta rows; tens of ms | Same as tip; tens of ms | Same |
| C. UTXO-set sum (no new storage) | 0 GB | 100ms+ on heavy addresses | Not supported | Same |

Shape A's read latency is constant in address activity; the worst-case 143K-tx miner address reads in the same sub-millisecond budget as a 5-UTXO consumer wallet. Shape B saves nothing on storage and pays 100x worse read latency. Shape C cannot serve historical balance at all.

The reorg correctness is identical to M4's UTXO surface, which is already integration-tested. The only subtle bug to guard against: the read-modify-write at commit must use the visibility-checked rev-iter (matching `read_transparent_address_utxos`), not a raw `scan_prefix` reverse, otherwise a commit during reorg recovery could base its new running total on a reorged-out previous row.

**How to apply:** Slice B's storage layout mirrors M4 §B2 with one change: the trailing `chain_epoch_id` field still applies, but the column family lives in `services/zinder-derive`'s own RocksDB instance, not in `zinder-store`. The codec, key layout discipline, and visibility-check pattern are copied from `crates/zinder-store/src/transparent_utxo.rs`.

### D5. Wire shape: structured `Balance { confirmed_zat, unconfirmed_delta_zat, address_count }`

The native API exposes a structured balance message that splits confirmed and mempool-pending. The compat shim collapses to lightwalletd's `Balance { value_zat: int64 }` for legacy clients.

```proto
message TransparentAddressBalance {
  uint64 confirmed_zat = 1;
  int64 unconfirmed_delta_zat = 2;
  uint32 address_count = 3;
  ChainEpoch chain_epoch = 4;
}

message TransparentAddressBalanceRequest {
  repeated AddressLookup addresses = 1;
  optional ChainEpoch at_epoch = 2;
}
```

`AddressLookup` is the shared message defined in [M4 §A1](m4-transparent-address.md#a1-wire-shape) (`oneof { bytes script_hash; string address }`). Per-address breakdown is deliberately out of scope: every consumer surveyed sums across the address list and presents one number; if a future explorer needs per-address breakdown it can call once per address.

**Why:** Esplora and ElectrumX both split confirmed and unconfirmed, and that split is what users actually want to see in explorer UI ("you have 1.5 ZEC confirmed, 0.05 ZEC pending"). Lightwalletd's `Balance` is one int64 because the lightwalletd-era contract was wallet-only and wallets compute mempool overlay client-side from `GetMempoolStream`. Zinder's audience includes explorers, where the mempool overlay must come from the indexer.

**How to apply:** The native proto carries the structured message. The compat shim's `GetTaddressBalance` constructs the typed native request, calls `WalletQueryApi::transparent_address_balance`, and maps `Balance.confirmed_zat as int64` into `lightwalletd::Balance.value_zat`, dropping `unconfirmed_delta_zat`. Legacy lightwalletd clients see exactly the value they expected; native and explorer clients see the structured shape.

### D6. Mempool overlay computed at the gRPC adapter, not stored

`unconfirmed_delta_zat` is computed from M3's existing surfaces:

```rust
let confirmed = derive_store.transparent_address_balance(script_hash, chain_epoch)?;
let mempool_outputs = mempool_index.transparent_mempool_outputs_by_address(script_hash)?;
let mempool_spends = mempool_index.transparent_mempool_spend_by_outpoint_for_address(script_hash)?;
let unconfirmed_delta = mempool_outputs.sum_value() as i64
    - mempool_spends.sum_value() as i64;
```

The mempool overlay is not stored in the derive consumer's RocksDB. Mempool entries are ephemeral (ADR-0010 retention windows) and live in the writer's in-memory `MempoolIndex`; persisting them in a derive view would duplicate state with no read-latency benefit.

**Why:** Mempool data has different retention semantics from confirmed data. Persisting it would force the derive consumer to subscribe to mempool events, maintain its own mempool overlay, and reconcile against the canonical mempool index on every read. Computing it at the adapter is one extra RPC call per balance read (`transparent_mempool_outputs_by_address`) which is bounded by the address's live mempool footprint (typically 0; spike to a few during heavy activity). Sub-millisecond.

**How to apply:** The `ExplorerQuery.TransparentAddressBalance` adapter calls both `derive_store.transparent_address_balance(...)` and the existing M3 mempool methods, composes them, returns the structured `Balance`. The derive consumer's `MempoolEvents` subscription is used for one reason only: to invalidate cached metadata if the derive consumer maintains any address-level cache. M5 does not maintain such a cache (RocksDB lookups are fast enough); the subscription is reserved for M6+ analytics views.

### D7. Drop `GetTaddressBalanceStream` from the native API

The lightwalletd-era streaming form has zero call sites across the surveyed ecosystem. Native `WalletQuery` and `ExplorerQuery` do not expose a streaming variant. The compat shim implements `GetTaddressBalanceStream` as a per-address loop over the unary form for legacy clients.

**Why:** Shipping a vestigial RPC violates the [Public Interfaces §Contract Hygiene](../architecture/public-interfaces.md#contract-hygiene) rule that "public event variants, error variants, API transitions, cursor fields, and proto surfaces must be produced, consumed, or explicitly reserved by the owning architecture document." A native streaming form that no consumer calls is exactly what that rule is meant to prevent.

**How to apply:** The native proto has only `TransparentAddressBalance(TransparentAddressBalanceRequest) returns (TransparentAddressBalance)`. The compat shim has both `GetTaddressBalance` (direct mapping) and `GetTaddressBalanceStream` (per-address loop, identical to Zaino's implementation).

### D8. Historical balance (`balance-at-height`) is out of M5

The `at_epoch` field on `TransparentAddressBalanceRequest` pins the read to a specific chain epoch, but it does not let a caller request the balance at an arbitrary historical height. No production indexer ships historical balance as a first-class endpoint (Esplora, Blockchair, ElectrumX, BlockCypher all punt). The storage cost is unbounded if exposed naively.

**Why:** Two cases motivate historical balance: forensic analysis ("what was the balance at the time of incident X") and time-travel UI ("balance trend over the last year"). Forensic analysis is rare and can be served offline. Time-travel UI is unbounded and properly belongs to a future running-balance-over-time derive view (a separate column family that M6+ can build atop M5's accumulator). Shipping balance-at-height in M5 would force the storage shape to commit to one of those use cases without consumer evidence.

**How to apply:** `at_epoch` is preserved per the existing read-API convention. A future derive view can add `balance_at_height(script_hash, height)` if a real consumer requires it; the underlying running-totals rows already permit it (one seek to `(script_hash, H)`). The RPC method is not yet exposed.

### D9. Balance-change subscription is out of M5

A `BalanceChanged` event variant is reserved in `ChainEventEnvelope` (the proto adds the enum value with no producer) so that future consumers can subscribe. M5 ships no producer.

**Why:** ElectrumX's `subscribe_addresses` uses status hashes (not balance values) because pushing balance values has unbounded fanout (one push per address subscriber per block). Real-world subscription patterns are: (a) explorer pages poll on demand at sub-second cadence; (b) wallet UIs update on `ChainEvents` and recompute. Neither needs a server-pushed balance event. Reserving the slot is enough; implementing it without a real consumer would create a feature nobody calls.

**How to apply:** `ChainEvent` enum gains `BalanceChanged` variant marked `// reserved for M6+`. Producer is not implemented. Consumers calling `chain_events_for_family(BalanceChanged)` receive an empty stream.

### D10. Capability strings use the `derive.{consumer}.{capability}_v{N}` namespace

Per [public-interfaces.md §Capability Discovery](../architecture/public-interfaces.md#capability-discovery) and [derive-plane.md §Output naming](../architecture/derive-plane.md#output-naming), derive-plane capabilities use the `derive.*` prefix even when the RPC method is federated under `WalletQuery`. M5 advertises:

- `derive.explorer.ready_v1` (Slice A): indicates `zinder-derive` is up and the consumer SDK is alive. Advertised on `ExplorerQuery.ServerInfo`.
- `derive.explorer.transparent_balance_v1` (Slice B): indicates the balance accumulator is caught up (lag below the readiness threshold) and serving. Advertised on **both** `ExplorerQuery.ServerInfo` (direct) and `WalletQuery.ServerInfo` (federated, only when `zinder-derive` is reachable and ready).

There is no `wallet.*` capability for balance. Native consumers gating on capability strings see `derive.explorer.transparent_balance_v1` regardless of which gRPC surface they use.

**Why:** The architecture spine is explicit ([derive-plane.md line 102](../architecture/derive-plane.md#output-naming)): "Capability prefix: `derive.{consumer}.{capability}_v{N}`." Using `wallet.*` for a derive view would muddy the namespace, causing future contributors to pick prefixes by intuition instead of by rule. The user research preview suggested `wallet.address.transparent_balance_v1`; that suggestion is incorrect against the established convention and is corrected here.

**How to apply:** `crates/zinder-proto/src/capabilities.rs` gains both strings. The capability-coverage test (created in M4 Slice A) is extended to validate `derive.*` capabilities against `ExplorerQuery` methods AND federated `WalletQuery` methods. The test must also gate `wallet.*` capabilities against `ChainIndex` methods to prevent regressions.

### D11. Federated under WalletQuery, not independent gRPC

Per [derive-plane.md Shape 2](../architecture/derive-plane.md#shape-2--federated-under-walletquery), balance is exposed both directly on `ExplorerQuery.TransparentAddressBalance` (the derive consumer's own surface) and federated on `WalletQuery.TransparentAddressBalance` (proxied by `zinder-query` to `zinder-derive`). The compat shim's `GetTaddressBalance` consumes only `WalletQueryApi`, preserving the consumer-neutral wallet data plane contract from [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md).

**Why:** The compat shim must not consume derive-plane gRPC directly: that would require the compat shim to know about two backends (`zinder-query` and `zinder-derive`), violating the "compat reads only WalletQueryApi" rule. The federation pattern keeps the compat shim simple. The direct `ExplorerQuery` surface is for explorer-shaped consumers that want to call the derive plane without going through the wallet boundary.

**How to apply:** `WalletQueryApi::transparent_address_balance` is implemented on `zinder-query`'s `WalletQuery` struct as a gRPC client call to `ExplorerQuery.TransparentAddressBalance`. The proxy honors `at_epoch`, threads errors through `status_from_query_error`, and surfaces `Code::Unavailable` with `ReasonInfo("derive_unavailable")` if `zinder-derive` is unreachable. The capability `derive.explorer.transparent_balance_v1` is advertised on `WalletQuery.ServerInfo` only when the proxy is configured AND the most recent `derive.explorer.ready_v1` probe succeeded within the readiness window.

### D12. Backfill-then-attach contract for fresh derive consumers

A fresh `zinder-derive` consumer's input is the union of:

1. **Channel C**: `WalletQuery` canonical reads (`compact_block_range`, `transactions_in_range`) for historical replay from genesis to a recent height.
2. **Channel A**: `WalletQuery.ChainEvents` from cursor=None (which delivers only events newer than `chain_event_retention_hours`, default 168h ≈ 8064 blocks) for steady-state.

The transition between Channel C and Channel A must not drop or duplicate events. The consumer SDK provides a `backfill_then_attach` helper that:

1. Reads the persisted consumer cursor from its own RocksDB. If absent, sets `last_processed_height = BlockHeight::new(0)`.
2. Calls `WalletQuery.ChainEvents` with `from_cursor = None` and reads the **first** envelope to discover `oldest_retained_height`.
3. If `last_processed_height < oldest_retained_height - reorg_window_blocks`: enters Channel C backfill mode, reading `compact_block_range(last_processed_height..=oldest_retained_height - reorg_window_blocks)` block by block, applying each as a synthetic `ChainCommitted` event to the consumer's accumulator.
4. Once `last_processed_height >= oldest_retained_height - reorg_window_blocks`: attaches to the live `ChainEvents` stream, using the first envelope's cursor as the resume point.
5. The consumer cursor is persisted after every applied envelope (not after every block) using a RocksDB transaction that bundles cursor write + accumulator writes atomically.

**Why:** This is the canonical "derive consumer cold-start" path. Without it, a fresh consumer trying to backfill from genesis via Channel A alone fails because `chain_event_retention_hours` retains only ~7 days. With it, every derive consumer has the same bootstrap shape: backfill from canonical artifacts, attach to live events, persist cursor durably. The pattern is reusable for M6+ consumers.

**How to apply:** Slice A ships `backfill_then_attach` as a free function inside `services/zinder-derive` (not yet extracted to a separate SDK crate). The function takes a generic `DeriveConsumer` trait that the balance accumulator (and future consumers) implement. Cursor persistence uses RocksDB column family `cursor` colocated with the consumer's data column family.

## Build order

Land in order. Each phase ends with `cargo nextest run --profile=ci && cargo nextest run --profile=ci-perf` plus `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`. Live tests under each phase are gated per [ADR-0006](../adrs/0006-test-tiers-and-live-config.md).

### Slice A: derive-plane bootstrap

#### A1. Crate scaffold

`services/zinder-derive/` is created with:

- `Cargo.toml` declaring binary `zinder-derive` and library `zinder_derive`.
- `src/main.rs` invoking `zinder_derive::run`.
- `src/lib.rs` exposing `run`, `Config`, `DeriveConsumer` trait, error types.
- Workspace member registration in root `Cargo.toml`.
- Dependency policy entry in `cargo deny`.

Compiles, passes `cargo check --workspace --all-targets --all-features` and `cargo clippy --workspace -- -D warnings`. Binary boots, prints config, exits.

#### A2. Configuration and ops surface

`crates/zinder-runtime` is extended with `DeriveConfig` covering:

```toml
[derive.explorer]
listen_addr = "127.0.0.1:9068"
storage_path = "/var/lib/zinder-derive-explorer"
chain_events_endpoint = "http://127.0.0.1:9101"  # zinder-query gRPC
chain_events_cursor_persist_path = "auto"  # uses storage_path/cursor
catchup_interval_ms = 250
readiness_lag_threshold_chain_epochs = 4

[derive.explorer.retention]
view_retention_days = 365
```

Sensitive fields are excluded from environment variables per [public-interfaces.md §Configuration Conventions](../architecture/public-interfaces.md#configuration-conventions). `--config` and `--print-config` work end to end. `/healthz`, `/readyz`, and `/metrics` listeners are wired on the configured port.

Readiness causes follow [Service Operations](../architecture/service-operations.md) typed-cause conventions. Initial causes:

- `Initializing`: startup before first cursor write.
- `Backfilling`: Channel C in progress; reports `backfill_remaining_blocks`.
- `Attaching`: switching from Channel C to Channel A.
- `LiveCatchingUp`: Channel A is attached but lag exceeds threshold.
- `Ready`: Channel A active, lag below threshold.
- `ChainEventsUnavailable`: upstream `zinder-query` endpoint unreachable.

Prometheus metrics follow the `zinder_derive_*` prefix.

#### A3. DeriveStore: RocksDB wrapper

`services/zinder-derive/src/store/` provides a `DeriveStore` that:

- Opens its own RocksDB at the configured `storage_path`. Never colocated with canonical RocksDB.
- Exposes typed put/get/scan operations gated by column-family enums local to `zinder-derive`.
- Carries its own `SchemaFingerprint` (independent from canonical schema fingerprint).
- Has its own boundary `thiserror` enum `DeriveStoreError`.

Initial column families:

- `cursor`: consumer-cursor persistence, keyed by consumer name.
- `consumer_metadata`: schema versions, last-applied-height, error counts.

Slice B adds a `transparent_address_balance` column family for the balance accumulator.

#### A4. ChainEvents subscription with cursor persistence

`services/zinder-derive/src/consumer/chain_events.rs` provides a generic `ChainEventsSubscriber<C: DeriveConsumer>` that:

- Connects to `WalletQuery.ChainEvents` (or `IngestControl.ChainEvents` if configured directly) with the persisted cursor.
- Receives `ChainEventEnvelope` messages and dispatches `ChainCommitted` and `ChainReorged` to `consumer.apply_event`.
- Persists the cursor after every envelope using a `WriteBatch` that bundles `cursor` column-family write with the consumer's data writes.
- Emits typed errors on cursor expiry, channel disconnect, or upstream unavailability.

Reorg correctness: the subscriber requires `consumer.apply_reorged(&ChainRangeReverted, &ChainEpochCommitted) -> Result<(), C::Error>` so each consumer decides how to revert its derived state. The balance accumulator (Slice B) reverts by un-aggregating affected addresses; future consumers may rebuild from scratch.

#### A5. ChainEpochReadApi backfill helper

`services/zinder-derive/src/consumer/backfill.rs` provides:

```rust
pub async fn backfill_then_attach<C: DeriveConsumer>(
    consumer: &mut C,
    derive_store: &DeriveStore,
    upstream: &ChainEventsUpstream,
    config: &BackfillConfig,
) -> Result<(), DeriveError>;
```

The function implements D12's contract:

1. Read persisted `last_processed_height` from `cursor` column family.
2. Discover `oldest_retained_height` via the first envelope from `ChainEvents`.
3. If a backfill gap exists, drain the gap via `WalletQuery.compact_block_range` block by block, dispatching to `consumer.apply_event(ChainCommitted)` synthetically.
4. Switch to `ChainEventsSubscriber` for live delivery.

The function is the prerequisite for any derive consumer; Slice B's balance accumulator is the first user.

#### A6. MempoolEvents subscription helper

`services/zinder-derive/src/consumer/mempool_events.rs` provides a `MempoolEventsSubscriber<C: DeriveMempoolConsumer>` that subscribes to `WalletQuery.MempoolEvents` with cursor persistence in the same `cursor` column family. Slice A ships the helper but no consumer uses it; Slice B's balance accumulator does not subscribe to mempool events (mempool overlay is computed at the gRPC adapter per D6). The helper exists so M6+ consumers have it.

#### A7. ExplorerQuery gRPC service skeleton

`crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` is created with:

```proto
service ExplorerQuery {
  rpc ServerInfo(ServerInfoRequest) returns (ServerInfoResponse);
}
```

`services/zinder-derive/src/grpc/` implements the `ExplorerQuery` server, advertising `derive.explorer.ready_v1` on `ServerInfo` once the consumer is ready. No data RPCs in Slice A.

#### A8. Local development composition

The `zinder dev` profile (mentioned in [service-boundaries.md §Development Profile](../architecture/service-boundaries.md#development-profile)) gains optional `zinder-derive` composition. The profile is documented; the actual binary launch sequence is operator-side.

#### A9. Capability advertisement

`crates/zinder-proto/src/capabilities.rs::ZINDER_CAPABILITIES` gains `derive.explorer.ready_v1`. The capability-coverage test created in M4 Slice A (`crates/zinder-client/tests/integration/capability_coverage.rs`) is extended to assert that `derive.*` capabilities map to `ExplorerQuery` methods (analogous to the existing `wallet.*` -> `ChainIndex` mapping). Slice A's only `derive.*` capability is `ready_v1`; Slice B adds `transparent_balance_v1`.

#### A10. Tests

- Unit tests for `DeriveStore` open/close, schema fingerprint validation, cursor put/get round-trip.
- Integration test in `services/zinder-derive/tests/integration/bootstrap.rs`: launch `zinder-derive` against a local `zinder-query`, assert `/readyz` transitions through `Initializing` -> `Backfilling` -> `Attaching` -> `LiveCatchingUp` -> `Ready`.
- Integration test in `services/zinder-derive/tests/integration/reorg_delivery.rs`: drives a live `ChainReorged` envelope through the subscriber and asserts `apply_reorged` is called with the correct ranges. Closes the gap M4 surfaced where reorg delivery over the gRPC stream lacked integration coverage.
- Live regtest under `services/zinder-derive/tests/live/derive_bootstrap.rs`: cold-starts `zinder-derive` against a regtest `zinder-query` synced to height 200, asserts backfill catches up to tip, asserts a mined reorg invalidates the right blocks.
- Mutation testing focused on cursor-persistence and reorg-handling paths:

  ```bash
  cargo mutants --workspace --all-features \
    --file services/zinder-derive/src/consumer/chain_events.rs \
    --file services/zinder-derive/src/consumer/backfill.rs \
    --re 'apply_event|apply_reorged|persist_cursor|backfill_then_attach'
  ```

#### A11. Documentation

- `docs/architecture/derive-plane.md` is updated:
  - "Out of scope" entry "A reference derive consumer implementation" is removed (it is no longer out of scope).
  - "A standardized derive-consumer SDK" entry is reworded to note the in-process helpers in `services/zinder-derive/src/consumer/` and that extraction to a separate crate is deferred to M6+.
- `docs/architecture/service-boundaries.md` extended-production-deployment block is updated to reflect `zinder-derive` as a real fourth deployable.
- `docs/architecture/chain-events.md` reorg-delivery integration coverage notes are updated.
- `docs/specs/m5-transparent-address-balance.md` (this file) records "Slice A complete" once shipped.

### Slice B: balance accumulator and APIs

#### B1. Domain type

`crates/zinder-core/src/transparent_address_balance.rs` exports:

```rust
pub struct TransparentAddressBalance {
    pub confirmed_zat: u64,
    pub unconfirmed_delta_zat: i64,
    pub address_count: u32,
    pub chain_epoch: ChainEpoch,
}
```

Re-exported from `lib.rs`. The accumulator's storage row is internal to `zinder-derive` and is not in `zinder-core` (it is a derive concern, not a canonical concern).

#### B2. DeriveStore schema and accumulator row

`services/zinder-derive/src/balance/store.rs` adds the `transparent_address_balance` column family with the key layout:

```text
[KEY_VERSION=1, kind=1] ++ network_id (4 BE) ++ address_script_hash (32) ++ block_height_be (4 BE) ++ chain_epoch_id (8 BE)
```

Note the `kind=1` is local to `zinder-derive`'s key namespace; it does not collide with `zinder-store`'s `kind=8` (transparent_address_tx_index, M4 Slice B) because the two stores are physically separate RocksDB instances.

Payload (prost-encoded):

```rust
#[derive(Clone, PartialEq, Message)]
struct TransparentBalanceAccumulatorRecord {
    #[prost(uint64, tag = "1")] confirmed_zat: u64,
    #[prost(uint64, tag = "2")] funded_count: u64,
    #[prost(uint64, tag = "3")] spent_count: u64,
}
```

`SchemaFingerprintEntry` for the consumer with `schema_version = 1`. Read path follows M4 D9 dynamic-filter visibility.

#### B3. Accumulator consumer

`services/zinder-derive/src/balance/consumer.rs` implements `DeriveConsumer` for `TransparentBalanceAccumulator`:

- `apply_event(ChainCommitted)` walks the committed range's transparent inputs and outputs (already extracted by `IngestArtifactBuilder`; the consumer reads them from `compact_block_range` via the wallet query API or directly from the chain-event payload). For each address-touched-at-height, it reads the previous running total via the visibility-checked rev-iter, computes the new running total, writes the new row.
- `apply_reorged(ChainRangeReverted, ChainEpochCommitted)`: no physical delete; the dynamic-filter visibility model handles reorged rows automatically. The accumulator only writes rows for the new committed range; reorged rows fail `block_is_visible` on subsequent reads.

Critical correctness check: the `previous running total` lookup at commit time uses the visibility-checked rev-iter. A naive `scan_prefix` reverse iter would return the most-recent-by-key-order row, which during reorg recovery might be a reorged-out row. A targeted regression test exercises this.

#### B4. ExplorerQuery RPC

`crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` adds:

```proto
message TransparentAddressBalanceRequest {
  repeated AddressLookup addresses = 1;
  optional ChainEpoch at_epoch = 2;
}

message TransparentAddressBalance {
  uint64 confirmed_zat = 1;
  int64 unconfirmed_delta_zat = 2;
  uint32 address_count = 3;
  ChainEpoch chain_epoch = 4;
}

service ExplorerQuery {
  rpc TransparentAddressBalance(TransparentAddressBalanceRequest)
      returns (TransparentAddressBalance);
}
```

`AddressLookup` is imported from `wallet.proto` (the M4-defined shared message). The `ExplorerQuery` adapter in `services/zinder-derive/src/grpc/balance.rs`:

1. Parses the request's `AddressLookup` list via the shared `address_lookup_to_script_hash` helper from M4 Slice A.
2. For each script hash, reads the latest visible accumulator row via `DeriveStore::transparent_address_balance(script_hash, chain_epoch)`.
3. Calls `WalletQueryApi::transparent_mempool_outputs_by_address(script_hash)` and `transparent_mempool_spend_by_outpoint_for_address(script_hash)` (M3 mempool methods) for the unconfirmed delta.
4. Sums confirmed and unconfirmed across the address list.
5. Returns the structured `Balance`.

#### B5. WalletQuery federation (Shape 2)

`services/zinder-query/src/lib.rs::WalletQueryApi` gains:

```rust
async fn transparent_address_balance(
    &self,
    request: TransparentAddressBalanceRequest,
) -> Result<TransparentAddressBalance, QueryError>;

async fn transparent_address_balance_at_epoch(
    &self,
    request: TransparentAddressBalanceRequest,
    at_epoch: Option<ChainEpoch>,
) -> Result<TransparentAddressBalance, QueryError>;
```

The implementation in `WalletQuery<ReadApi, Broadcaster, DeriveProxy>` (a new generic parameter for the proxy client) calls `ExplorerQueryClient::transparent_address_balance` over gRPC. Errors are mapped to `QueryError::DeriveUnavailable`, which surfaces as `Code::Unavailable` with `ReasonInfo("derive_unavailable")`.

`crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` gains:

```proto
service WalletQuery {
  // ... existing methods
  rpc TransparentAddressBalance(TransparentAddressBalanceRequest)
      returns (TransparentAddressBalance);
}
```

The federated method shares the request/response types with `ExplorerQuery` (per the proto import). `WalletQueryGrpcAdapter` proxies the call. Capability `derive.explorer.transparent_balance_v1` is advertised on `WalletQuery.ServerInfo` only when:

- `derive.explorer_endpoint` is configured, AND
- The most recent `ExplorerQuery.ServerInfo` probe (refreshed every `derive_probe_interval_ms`, default 5000) returned `Ready` with `derive.explorer.ready_v1`.

#### B6. ChainIndex methods

`crates/zinder-client/src/chain_index.rs` adds:

```rust
async fn transparent_address_balance(
    &self,
    addresses: &[TransparentAddressScriptHash],
) -> Result<TransparentAddressBalance, IndexerError>;

async fn transparent_address_balance_at_epoch(
    &self,
    addresses: &[TransparentAddressScriptHash],
    at_epoch: ChainEpoch,
) -> Result<TransparentAddressBalance, IndexerError>;
```

`LocalChainIndex` calls through to `WalletQueryApi::transparent_address_balance` which proxies to `zinder-derive`. `RemoteChainIndex` calls the federated `WalletQuery.TransparentAddressBalance` over tonic. Both honor `at_epoch` per the existing companion-method pattern.

#### B7. Compat shim

`services/zinder-compat-lightwalletd/src/grpc.rs` implements:

- `GetTaddressBalance(AddressList) -> Balance`: parses string addresses via the existing `transparent_address_utxos_request` helper, constructs the typed request, calls `WalletQueryApi::transparent_address_balance`, maps `confirmed_zat as int64` to `lightwalletd::Balance.value_zat`. `unconfirmed_delta_zat` is dropped (legacy proto cannot express it).
- `GetTaddressBalanceStream(stream Address) -> Balance`: per-address loop over the unary form. Returns the summed confirmed-only balance. Identical to Zaino's implementation. Existence is for legacy lightwalletd clients only.

Both replace the current `Status::unimplemented` branches.

#### B8. Tests, capability, docs

- Storage tests: accumulator row commit, dynamic-filter visibility under reorg, schema-fingerprint mismatch, crash recovery for the `transparent_address_balance` column family.
- Consumer logic tests: `apply_event(ChainCommitted)` correctness with multi-address blocks; `apply_reorged` correctness under deep reorgs (the read-modify-write at commit must use visibility-checked rev-iter, regression-tested explicitly).
- ExplorerQuery integration tests in `services/zinder-derive/tests/integration/balance.rs`: full path from `apply_event` through `TransparentAddressBalance` RPC, including mempool overlay via M3 surfaces.
- Federated WalletQuery integration tests in `services/zinder-query/tests/integration/transparent_address_balance.rs`: launches `zinder-query` with a `zinder-derive` proxy, asserts capability advertisement gating, asserts proxy returns `DeriveUnavailable` when `zinder-derive` is down.
- Compat shim tests in `services/zinder-compat-lightwalletd/tests/integration/lightwalletd_grpc.rs`: `GetTaddressBalance` and `GetTaddressBalanceStream`, both end-to-end against the federated path.
- ChainIndex parity: `crates/zinder-client/tests/integration/transparent_address_balance_parity.rs` covers `LocalChainIndex` vs `RemoteChainIndex`.
- Live regtest under `services/zinder-derive/tests/live/balance_accumulator.rs`: mines transparent transactions to multiple addresses, asserts balance reads match the sum of UTXOs, asserts a reorg correctly invalidates and re-applies running totals.
- Mutation testing extends Slice A's coverage to include the accumulator and visibility-checked rev-iter:

  ```bash
  cargo mutants --workspace --all-features \
    --file services/zinder-derive/src/balance/consumer.rs \
    --file services/zinder-derive/src/balance/store.rs \
    --re 'apply_event|apply_reorged|read_previous_visible_total'
  ```

- Capability advertisement: `derive.explorer.transparent_balance_v1` added to `ZINDER_CAPABILITIES`. The capability-coverage test asserts the federated `WalletQuery.TransparentAddressBalance` and direct `ExplorerQuery.TransparentAddressBalance` both map to it.
- Docs:
  - [Derive plane](../architecture/derive-plane.md): explorer consumer is now a real reference, not "out of scope."
  - [Wallet data plane](../architecture/wallet-data-plane.md): federated `TransparentAddressBalance` documented.
  - [Public interfaces](../architecture/public-interfaces.md): vocabulary entries for `TransparentAddressBalance`, `TransparentAddressBalanceRequest`, the `derive.*` capability namespace.
  - [Protocol boundary](../architecture/protocol-boundary.md): native API surface inventory updated to include the federated method; new `ExplorerQuery` proto family added under "Protocol Surfaces."
  - [Service operations](../architecture/service-operations.md): readiness causes for `zinder-derive` documented.

## Resolved questions

### R1. Canonical vs derive plane

Balance is a derive consumer, not a canonical artifact. The cookbook is unambiguous, the architectural separation matters for entropy reasons, and the storage shape is identical either way. M5 instantiates `services/zinder-derive` as the first real consumer.

### R2. Streaming form

Dropped from the native API. Compat shim implements as a per-address loop for legacy lightwalletd clients (zero ecosystem call sites; vestigial proto preserved only for backward compatibility of the lightwalletd contract).

### R3. Mempool integration

Computed at the gRPC adapter from M3's existing mempool surfaces. Not stored in the derive consumer's RocksDB. The native wire shape exposes `unconfirmed_delta_zat` as a signed integer (negative when pending spends exceed pending receives).

### R4. Historical balance

Out of M5. No production indexer ships balance-at-height. Reserved as a future flag (the storage shape supports it) when a real consumer requires it.

### R5. Balance-change subscription

Out of M5. `ChainEvent::BalanceChanged` enum slot is reserved with no producer. Real subscription patterns use polling at sub-second cadence (explorers) or `ChainEvents`-driven recompute (wallets); pushing per-address balance values has unbounded fanout.

### R6. Capability namespace

`derive.{consumer}.{capability}_v{N}` per derive-plane.md, including for federated methods exposed under `WalletQuery`. M5 advertises `derive.explorer.ready_v1` (Slice A) and `derive.explorer.transparent_balance_v1` (Slice B). No `wallet.*` capability for balance.

### R7. Operational topology

`zinder-derive` is a fourth deployable: separate process, separate RocksDB, separate ops endpoints. Federated under `WalletQuery` for consumer-neutrality; direct `ExplorerQuery` surface for explorer clients that prefer the derive plane directly.

## ADR promotion

When both slices ship, this spec is deleted and decisions promote to:

- **ADR-0012: Derive-plane instantiation and the Channel A/C backfill-then-attach contract**. Captures the topology, federation pattern (Shape 2), capability namespace, cursor persistence contract, and replayability rule from Slice A. References `derive-plane.md` as the architecture doc the ADR makes durable.
- **ADR-0013: Balance accumulator running-totals pattern**. Captures the storage shape A choice (per-block running totals with dynamic-filter visibility), the read-modify-write-with-visibility-checked-rev-iter correctness rule, the mempool-overlay-at-adapter pattern, and the "wire-shape splits confirmed and unconfirmed_delta" decision from D5. References M4's ADR-0011 for the dynamic-filter pattern it reuses.

## Out of scope (reserved for future)

- **Historical balance (`balance_at_height`)**. Deferred per D8. A future spec can add `balance_at_height(script_hash, height)` against the existing accumulator rows when a consumer requires it.
- **Balance-change subscription (`BalanceChanged` event variant)**. Reserved per D9; producer not implemented.
- **Per-address breakdown in the response**. The current shape sums across the address list. If a future explorer needs per-address breakdown, the wire shape gains `repeated TransparentAddressBalanceEntry per_address = 5;`.
- **Top-addresses-by-balance and analytics aggregates**. Belongs in M6+ as separate derive consumers (or extensions of `ExplorerQuery`). Not in scope for M5.
- **Fee histograms and address activity feeds**. Future derive consumers; M5 establishes the SDK pattern they will use.
- **Cross-derive-consumer queries** (a single client query that joins data across `derive.explorer` and `derive.analytics`). Per [derive-plane.md §Out of scope](../architecture/derive-plane.md#out-of-scope-for-now), unsupported. Clients call each consumer separately.
- **Sink-only Shape 3 derive consumers** (writing to Postgres, ClickHouse, S3). The architecture supports it; M5 does not ship a reference. The cursor protocol is sufficient for an operator to build their own.
- **Standardized derive-consumer SDK as a separate crate**. Per A1, helpers live in `services/zinder-derive/src/consumer/` until M6+ adds a second consumer that justifies extraction.

## Cross-references

- [PRD-0001](../prd-0001-zinder-indexer.md): names the derive plane as optional in v1 and replayable from canonical artifacts.
- [Derive plane](../architecture/derive-plane.md): the architecture doc M5 makes durable. After ship, this doc references ADR-0012 instead of being purely aspirational.
- [Wallet data plane §Federated under WalletQuery](../architecture/derive-plane.md#shape-2--federated-under-walletquery): the federation pattern this spec adopts.
- [Chain events](../architecture/chain-events.md): the input stream Slice A consumes (Channel A) and bootstrap-via-canonical (Channel C).
- [M4 spec §D7](m4-transparent-address.md#d7-balance-and-balance-stream-surfaces-are-out-of-m4): the deferral that named M5.
- [M4 spec §D9](m4-transparent-address.md#d9-address-keyed-artifact-families-use-dynamic-filter-visibility): the dynamic-filter pattern Slice B reuses.
- [Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family): the cookbook rule that places balance in the derive plane.
- [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md): the consumer-neutral wallet data plane that the federation pattern preserves.
- [ADR-0010](../adrs/0010-mempool-topology-and-retention.md): the M3 mempool surfaces D6 composes for the unconfirmed delta.
- [Public interfaces §Capability Discovery](../architecture/public-interfaces.md#capability-discovery): the `derive.*` capability namespace.
- [Lessons from Zaino §Pattern 4](../reference/lessons-from-zaino.md#pattern-4-storage-as-a-linear-migration-ladder): the anti-pattern this spec deliberately avoids by keeping balance out of canonical storage.
