# Closing the Zaino Surface Gap: Implementation Plan

| Field | Value |
| ----- | ----- |
| Status | In progress (Phases 0–2 shipped; Phases 3–6 pending) |
| Audience | Zinder maintainers picking up the gap-closing work in a new session |
| Source | Phased plan extracted from the 2026-05-09 analysis session that produced the Phase 0 gap-doc refresh and shipped Primitives A and B |
| Related | [Closing the Zaino Surface Gap](../reference/closing-the-zaino-surface-gap.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Public interfaces](../architecture/public-interfaces.md), [Extending artifacts](../architecture/extending-artifacts.md), [Lessons from Zaino](../reference/lessons-from-zaino.md), [ADR-0006](../adrs/0006-test-tiers-and-live-config.md), [ADR-0007](../adrs/0007-multi-process-storage-access.md), [ADR-0009](../adrs/0009-ingest-control-transport-security.md), [ADR-0010](../adrs/0010-mempool-topology-and-retention.md), [ADR-0013](../adrs/0013-derive-plane-instantiation-and-transparent-address-balance.md) |

## Purpose

The [gap inventory](../reference/closing-the-zaino-surface-gap.md) catalogs 21 surfaces (G1–G21) where consumers of Zaino still need a Zinder shape. Rather than treating each gap as an independent decision, the analysis grouped them into **four architectural primitives** that close 11 of 12 originally-numbered gaps and address 7 of 8 newly-named Zallet frictions. This document captures the phased plan that delivers those primitives, with explicit pointers to what shipped and what remains.

The gap doc is the inventory. This document is the implementation plan that operationalizes it.

## Phase status

| Phase | Primitive / Scope | Closes | Status |
| ----- | ----------------- | ------ | ------ |
| **0** | Gap-doc refresh | n/a (doc-only) | ✓ Shipped 2026-05-09 |
| **1** | Primitive A — `BlockSelector` resolver + `BlockHeaderInfo` read model | G2, G4 | ✓ Shipped 2026-05-09 |
| **2** | Primitive B — `TxStatus` wire envelope + `MinedDetails` enrichment | G3, G7, G13, partly G5 | ✓ Shipped 2026-05-09 |
| **3** | Primitive C — Mempool point lookups on gRPC + `MempoolMinedEvent.block_hash` | G6, G7-on-wire (already covered), Decision 8 | Pending |
| **4** | Primitive D — Federation generic + M5 Slice B | G1, future M6+ derive consumers | Pending |
| **5** | Newly-named gap cleanup | G14, G15, G16, G17, G19, G20, G21 | Pending |
| **6** | Polish | G8, G9, G10 (open ADR), G11 (parity tier) | Pending |

`cargo nextest run --profile=ci` currently runs **393 tests** plus 2 perf tests, all green. The default validation gate (`cargo fmt --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo nextest run --profile=ci`, `cargo nextest run --profile=ci-perf`, `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`) passes after Phases 0–2.

## Architectural primitives (the spine of the work)

Phases 3–6 build on the same vocabulary established by Phases 1 and 2. The four primitives are:

- **Primitive A — `BlockSelector` resolver.** Typed `Height(BlockHeight) | Hash(BlockHash)` selector backed by a canonical `block_hash_index` column family. Compat hash-only callers route through it; native callers use the same shape for the typed block-header read model. Closes G2 and G4.
- **Primitive B — `TxStatus` wire envelope + `MinedDetails`.** Replaces mined-only `TransactionResponse` with `TransactionStatusResponse` carrying a typed oneof. Mined details (`consensus_branch_id`, `block_time`, `confirmations`) are response-bound and constructed only via `MinedDetails::from_response_epoch`, which is the entropy gate against racy confirmations. Closes G3, G7, G13.
- **Primitive C — Mempool point lookups on gRPC.** Mirrors the existing Rust trait methods (`transparent_mempool_outputs_by_address`, `transparent_mempool_spend_by_outpoint`) onto `WalletQuery` so non-Rust consumers don't need to scan `MempoolSnapshot`. Adds `block_hash` to `MempoolMinedEvent`. Pending.
- **Primitive D — Federation generic + M5 Slice B.** `WalletQueryGrpcAdapter::proxy_to_derive` helper plus the M5 `TransparentBalanceAccumulator` and federated balance RPC. The generic is the entropy gate against M5+M6+M7 each shipping a copy-pasted proxy body. Pending.

## Phase 3 — Primitive C: Mempool point lookups (G6, Decision 8)

**Goal.** Promote the existing Rust trait methods to typed gRPC RPCs and enrich `MempoolMinedEvent` with the mined block hash. The Rust trait already has `transparent_mempool_outputs_by_address`, `transparent_mempool_spend_by_outpoint`, and `is_in_mempool`; `RemoteChainIndex` currently works around the gRPC asymmetry by paging `MempoolSnapshot` client-side. Phase 3 removes the workaround.

**New vocabulary.**

```protobuf
// wallet.proto
message TransparentMempoolOutputsByAddressRequest {
  AddressLookup address = 1;
  optional ChainEpoch at_epoch = 2;
}

message TransparentMempoolOutputsByAddressResponse {
  ChainEpoch chain_epoch = 1;
  repeated TransparentMempoolOutput outputs = 2;
}

message TransparentMempoolSpendByOutpointRequest {
  bytes transaction_id = 1;
  uint32 output_index = 2;
  optional ChainEpoch at_epoch = 3;
}

message TransparentMempoolSpendByOutpointResponse {
  ChainEpoch chain_epoch = 1;
  optional TransparentMempoolSpend spend = 2;  // None when not spent in mempool
}

rpc TransparentMempoolOutputsByAddress(TransparentMempoolOutputsByAddressRequest)
    returns (TransparentMempoolOutputsByAddressResponse);

rpc TransparentMempoolSpendByOutpoint(TransparentMempoolSpendByOutpointRequest)
    returns (TransparentMempoolSpendByOutpointResponse);

// Add to MempoolMinedEvent:
message MempoolMinedEvent {
  bytes transaction_id = 1;
  uint32 block_height = 2;
  bytes block_hash = 3;  // NEW (Decision 8)
}
```

**Capabilities.** Add `wallet.mempool.transparent_outputs_by_address_v1` and `wallet.mempool.transparent_spend_by_outpoint_v1`. The `wallet.mempool.*` namespace is new; the choice over `wallet.address.*` is deliberate (different storage tier, different lifecycle).

**Files to add/modify.**

- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` — two new RPCs, two new request/response message pairs, `block_hash` on `MempoolMinedEvent`.
- `crates/zinder-proto/src/capabilities.rs` — add the two capability strings.
- `services/zinder-query/src/lib.rs` — `WalletQueryApi` extension; the methods proxy to `IngestControl` per ADR-0010 (writer owns the live mempool).
- `services/zinder-query/src/grpc/native.rs` — adapters + response builders.
- `services/zinder-query/src/grpc/adapter.rs` — handler methods on `WalletQueryGrpcAdapter`.
- `crates/zinder-client/src/remote.rs` — replace the snapshot-scan workaround in `transparent_mempool_outputs_by_address` and `transparent_mempool_spend_by_outpoint` with the new gRPC calls.
- `services/zinder-ingest/src/ingest_control.rs` — `IngestControlClient` methods that proxy to the writer-owned `MempoolIndex`.
- `crates/zinder-client/tests/integration/capability_coverage.rs` — add the two capabilities.
- `docs/architecture/wallet-data-plane.md` §Mempool Point Lookups — replace pending prose with shipped citations.
- `docs/adrs/0010-mempool-topology-and-retention.md` — append §D8 confirmation that `MempoolMinedEvent.block_hash` is shipped.
- `docs/reference/closing-the-zaino-surface-gap.md` — mark G6 closed; update matrix.

**Tests.**

- `services/zinder-ingest/tests/integration/mempool_point_lookups.rs` — exercise both RPCs against a regtest mempool fixture.
- `crates/zinder-client/tests/integration/mempool_point_lookup_parity.rs` — `Local` vs `Remote` parity.
- `services/zinder-ingest/tests/integration/mempool_mined_event_block_hash.rs` — verify the new field is populated.

**Acceptance gate.** Default validation gate green. `RemoteChainIndex::transparent_mempool_outputs_by_address` no longer scans `MempoolSnapshot` client-side. `MempoolMinedEvent.block_hash` is populated for every mined event.

**Rough size.** 200–400 lines across ~8 files.

## Phase 4 — Primitive D: Federation generic + M5 Slice B (G1)

**Goal.** Land `WalletQueryGrpcAdapter::proxy_to_derive::<Req, Resp>(method, request)` in the same change as M5 Slice B's `TransparentAddressBalance`. The generic codifies the federation pattern so M6+ derive consumers don't each ship their own proxy body. Capture the pattern in ADR-0013.

**New vocabulary.**

```rust
// services/zinder-query/src/federation.rs (new module)
impl WalletQueryGrpcAdapter<QueryApi> {
    pub(crate) async fn proxy_to_derive<Req, Resp, F, Fut>(
        &self,
        method: F,
        request: tonic::Request<Req>,
    ) -> Result<tonic::Response<Resp>, tonic::Status>
    where
        F: FnOnce(ExplorerQueryClient<tonic::transport::Channel>, tonic::Request<Req>) -> Fut,
        Fut: Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
    { /* opens client, invokes method, maps errors */ }
}
```

**M5 Slice B work** (now retired to [ADR-0013](../adrs/0013-derive-plane-instantiation-and-transparent-address-balance.md) + [ADR-0014](../adrs/0014-compute-at-read-time-canonical-reads.md)):

- `services/zinder-derive/src/balance_accumulator.rs` — running totals with dynamic-filter visibility, mempool overlay path.
- `crates/zinder-proto/proto/zinder/v1/derive/explorer.proto` — `ExplorerQuery.TransparentAddressBalance` RPC.
- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` — federated `WalletQuery.TransparentAddressBalance` RPC.
- `services/zinder-query/src/grpc/native.rs` — single-line method body: `self.proxy_to_derive(|client, req| client.transparent_address_balance(req), request).await`.
- `services/zinder-compat-lightwalletd/src/grpc.rs` — `get_taddress_balance` and `get_taddress_balance_stream` rewired through the federated native RPC; the existing `Status::unimplemented` removed.
- `crates/zinder-client/src/chain_index.rs` — `transparent_address_balance(addresses, at_epoch)` Rust method.
- `crates/zinder-proto/src/capabilities.rs` — add `derive.explorer.transparent_balance_v1`. **Note:** the namespace is `derive.*`, not `wallet.*`, even though the consumer-facing RPC is on `WalletQuery`.
- `crates/zinder-client/tests/integration/capability_coverage.rs` — add the federation rule: any `WalletQuery` method whose adapter calls `proxy_to_derive` must advertise `derive.*`, not `wallet.*`. The CI assertion is the entropy gate against future namespace creep.
- `docs/adrs/0013-derive-plane-pattern.md` (new) — capture the federation pattern, the `proxy_to_derive` helper, and the `derive.*` capability namespace rule. Durable rule for every M6+ derive consumer.

**Tests.**

- `services/zinder-derive/tests/integration/transparent_balance_accumulator.rs` — accumulator correctness across the chain edge.
- `services/zinder-query/tests/integration/federated_balance_grpc.rs` — exercise the federated RPC.
- `services/zinder-compat-lightwalletd/tests/integration/get_taddress_balance.rs` — close the existing `Status::unimplemented` and verify Esplora-style `confirmed`/`unconfirmed` split where applicable.

**Doc updates.**

- ~~`docs/specs/m5-transparent-address-balance.md`~~ — retired per the retire-on-ship rule; M5 decisions live in ADR-0011, ADR-0013, ADR-0014.
- `docs/architecture/wallet-data-plane.md` — add §Federation section documenting the proxy pattern and namespace rule.
- `docs/reference/closing-the-zaino-surface-gap.md` — mark G1 closed.

**Acceptance gate.** Default validation gate green. The compat shim's `get_taddress_balance` `Status::unimplemented` is gone. The federation generic has zero direct callers other than `transparent_address_balance` but is documented and CI-enforced as the canonical pattern. Capability `derive.explorer.transparent_balance_v1` is advertised when the deployment includes `zinder-derive`.

**Rough size.** 600–1000 lines across ~15 files. Largest remaining phase.

## Phase 5 — Newly-named gap cleanup (G14, G15, G16, G17, G19, G20, G21)

**Goal.** Audit and close the seven Zallet-side frictions surfaced in the P0 doc refresh that the existing Zinder shape already addresses (or addresses with small additions). G18 (prevout resolution) is excluded — it needs its own spec.

| Gap | Action | Files |
| --- | ------ | ----- |
| **G14** typed errors | Audit `IndexerError` for explicit `NotFound`-like variants; document exhaustively in `public-interfaces.md` §Error vocabulary | `crates/zinder-client/src/error.rs`, doc |
| **G15** typed tip height | Confirm `ChainIndex::latest_block` returns `BlockId` (typed). Add a CI assertion that no consumer of the wire response calls `try_into().expect`. | None (verification) |
| **G16** typed subtree-root return | Audit `crates/zinder-core/src/subtree.rs` and the proto: subtree root must be `bytes` (32-byte fixed) on the wire and `[u8; 32]` typed in Rust. Confirm no hex-string return path. | `wallet.proto`, `chain_index.rs` |
| **G17** tree-state on `ChainIndex` | Confirm `tree_state_at(BlockHeight)` is on the trait and exposed by both `LocalChainIndex` and `RemoteChainIndex`. Document Zallet migration sketch. | doc |
| **G19** broadcast typed-bytes | `ChainIndex::broadcast_transaction(bytes: &[u8])` — confirm typed-bytes shape, not hex. | `chain_index.rs`, doc |
| **G20** tip-change push event | Confirm `ChainEvent::TipAdvanced` exists and is the canonical signal. Add a doc note explaining why the mempool-stream-closure heuristic is wrong. | doc |
| **G21** typed pool discriminants | Add typed `Pool` enum (`Sapling`, `Orchard`) to `zinder-core` and audit every "pool" consumer to use it. | `crates/zinder-core/src/pool.rs` (new) |

**G18** (prevout resolution) gets its own spec — `docs/specs/m6-prevout-resolution.md` — because batching strategy is non-trivial. Phase 5 names this as a follow-up but does not implement it.

**Acceptance gate.** Each of G14–G17, G19–G21 has either "✓ closed by [link]" or "verified, no work needed" in the gap doc. No `try_into().expect` on tip queries in any new tests. G18 has a draft spec.

**Rough size.** 100–300 lines across ~5–8 files.

## Phase 6 — Polish (G8, G9, G11)

**Goal.** Close lightwalletd-compat polish gaps and the parity-test scaffolding. G10 (wallet-plane authentication) stays as an open ADR question per the gap doc.

- **G8** — Wire `pool_selection_from_request` into `get_mempool_tx`. The helper exists; the call site silently drops the field today.
  - File: `services/zinder-compat-lightwalletd/src/grpc.rs:419-458`
  - Test: `services/zinder-compat-lightwalletd/tests/integration/get_mempool_tx_pool_filter.rs`
- **G9** — Decide which `LightdInfo` fields to populate (cross-link to `ServerCapabilities`). Some fields are deliberately empty (consumer-neutral); others should be filled.
  - File: `services/zinder-compat-lightwalletd/src/grpc.rs:983-1012`
- **G11** — Add a `parity-zaino` profile to `.config/nextest.toml` per ADR-0006. Initial scope: spin up a Zaino instance against a regtest network alongside Zinder, run a curated set of `WalletQuery` and lightwalletd compat RPCs against both, assert equivalence where the contract is byte-equal and behavioral-equal where it is not. Long-term release gate.

**G10** — Open ADR question. Defer to v2 unless multi-tenant operator scope appears.

**Rough size.** G8 + G9 are ~50 lines each. G11 is ~200 lines plus operator-doc updates.

## Cross-cutting infrastructure (lands incrementally)

These were named in the original plan but are not yet shipped. They are entropy gates against the most likely drift directions:

1. **Machine-readable gap tags on `Status::unimplemented`.** Decision 10 in the gap doc. A `///` doc comment immediately above each `Status::unimplemented` site of the form `/// gap: G{N}` with a CI assertion in `services/zinder-compat-lightwalletd/tests/integration/` that walks the source. Without this, the inventory drifts from reality silently between refreshes. Pending.
2. **`MinedDetails` literal-construction lint.** A grep-test or clippy-style lint that catches `MinedDetails { ... }` outside of `crates/zinder-core/src/transaction.rs` and fails the build. Today the type is constructed only via `from_response_epoch` by convention. Pending.
3. **Federation/derive-namespace assertion.** Per Phase 4: any `WalletQueryGrpcAdapter` method that calls `proxy_to_derive` must advertise a capability starting with `derive.`, not `wallet.`. Lands with Phase 4. Pending.
4. **`ChainIndex` method-count budget.** Test that asserts `ChainIndex` has fewer than 50 methods. When it crosses, the trait splits into `BlockReader`, `TransactionReader`, `MempoolReader`, `TransparentAddressReader`, `EventSubscriber`. Track as a milestone deliverable, not a follow-up. Pending.

## Quick-start for a new session

1. **Read the gap inventory:** `docs/reference/closing-the-zaino-surface-gap.md` (master state).
2. **Read the architectural commitments:** `docs/architecture/wallet-data-plane.md` (now reflects Phase 1 + Phase 2 shipped state), `docs/architecture/public-interfaces.md`, `docs/architecture/extending-artifacts.md`.
3. **Pick a phase from §Phase status above.** Phases 3 and 5 are independent of each other; Phase 4 depends on M5 Slice A (already shipped) and is the largest remaining feature; Phase 6 is small polish. Phase 3 is the natural next step because it reuses Phase 2's typed-status pattern and is small enough to land in one PR.
4. **Validate before any commit:** the default validation gate from `CLAUDE.md` (formatting, check, clippy, nextest `ci`+`ci-perf`, doc, deny, machete, `git diff --check`). Phases 1 and 2 hit all these gates green; new phases must too.
5. **Update this plan and the gap doc in the same change** as the code lands. The gap doc's `Last refresh` line is the durable status indicator; this plan is the durable next-steps indicator.

## Anti-pattern reminders

When picking up the work, avoid these shapes (named in the gap doc's §Anti-Patterns Zinder Refuses to Replicate):

- A1. Verbosity integers / verbosity booleans.
- A2. "Verbose" booleans on header/block calls.
- A3. String-keyed pool discriminants (relevant to G21).
- A4. Sentinel-overloaded `BlockId { height: 0, hash: bytes }` (Phase 1 replaced this with `BlockSelector`).
- A5. `zaino_proto::*` types on the Rust API surface.

Phase 2's `MinedDetails::from_response_epoch` is the canonical example of an entropy-gate constructor. Phases 3–6 should reuse the pattern: when a new value depends on a context (epoch, network, request shape), force the context into the constructor signature so it cannot be elided.

## Closing note

The architectural foundation (Phases 0–2) is the load-bearing work. Phases 3–6 extend or polish it rather than introducing new architectural shapes. The gap doc remains the single source of truth for what is shipped and what is open; this plan is the operational companion that names the work for the next session.
