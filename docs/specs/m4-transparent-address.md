# M4: Transparent-address artifact surface

| Field | Value |
| ----- | ----- |
| Status | Decisions locked; implementation in flight. Slice A infrastructure partly shipped (compat-only path), Slice B unstarted |
| Created | 2026-05-08 |
| Product | Zinder |
| Audience | Zinder maintainers, wallet developers, explorer developers |
| Related | [PRD-0001](../prd-0001-zinder-indexer.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Extending artifacts](../architecture/extending-artifacts.md), [Protocol boundary](../architecture/protocol-boundary.md), [Storage backend](../architecture/storage-backend.md), [ADR-0002](../adrs/0002-boundary-specific-serialization.md), [ADR-0007](../adrs/0007-multi-process-storage-access.md), [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md) |

## Context

[PRD-0001 §Implementation Decisions](../prd-0001-zinder-indexer.md) names M4 as "the transparent-address artifact surface following the extending-artifacts cookbook: paginated `GetTaddressTxids`-equivalent and `GetAddressUtxos`-equivalent native methods, end-to-end through the new artifact-family seam." The product purpose is twofold:

1. Close the t-address read surface that wallet, Zashi/Zodl compat, and explorer consumers need.
2. Validate that adding a canonical artifact family is mechanical under the [extending-artifacts cookbook](../architecture/extending-artifacts.md), with no central enum edits.

Some Slice A infrastructure already shipped during the M2/M3 work because the lightwalletd compat shim required transparent UTXO reads to claim Zashi compatibility:

- Domain types: `TransparentAddressUtxoArtifact`, `TransparentUtxoSpendArtifact`, `TransparentAddressScriptHash`, `TransparentOutPoint` in `crates/zinder-core/src/transparent_utxo.rs`.
- Storage: `TransparentAddressUtxo` column family, atomic commit through `commit_chain_epoch`, codec round-trip, reorg-revert path in `crates/zinder-store/src/chain_store.rs` and `transparent_utxo.rs`.
- Read trait: `ChainEpochReadApi::transparent_address_utxos`.
- Query boundary: `WalletQueryApi::transparent_address_utxos` + epoch-pin variant in `services/zinder-query/src/lib.rs:131,484`.
- Compat shim: `GetAddressUtxos`, `GetAddressUtxosStream`, `taddr_support=true` in `services/zinder-compat-lightwalletd/src/grpc.rs:507,519,879`, validated against Zashi v2.4.8 on testnet 2026-04-29 ([Findings from Android wallet integration](../reference/android-wallet-integration-findings.md)).

What is **not** shipped:

- The native `WalletQuery` proto carries no transparent-address RPC. Native consumers (Zallet, future SDK clients) cannot reach the existing `transparent_address_utxos` over the public service.
- `zinder-client::ChainIndex` exposes no transparent-address methods. The capability-coverage test will reject any attempt to advertise `wallet.address.transparent_utxos_v1` until `ChainIndex` covers it.
- No transparent-address tx-history artifact family exists. The compat shim's `GetTaddressTxids`, `GetTaddressTransactions`, `GetTaddressBalance`, and `GetTaddressBalanceStream` all return `Status::unimplemented` (`grpc.rs:340,351,360,369`).
- Capability strings `wallet.address.transparent_utxos_v1` and `wallet.address.transparent_history_v1` are reserved vocabulary only ([Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos), [Extending artifacts §Step 5](../architecture/extending-artifacts.md#step-5--wire-shape-in-walletproto)).

## Decisions

### D1. Two independently shippable slices

M4 splits into two slices that ship in order but do not share a single PR cluster:

- **Slice A: native UTXO surface.** Lifts the existing compat-only UTXO path onto the native `WalletQuery` proto and `ChainIndex` trait. No new storage, no new artifact family, no new ingest work. Lights up `wallet.address.transparent_utxos_v1`.
- **Slice B: transparent tx history.** Adds a new canonical artifact family (`TransparentAddressTxIndexArtifact`) following the extending-artifacts cookbook. Adds `GetTaddressTxids` and `GetTaddressTransactions` to the compat shim, mapped over the new family. Lights up `wallet.address.transparent_history_v1`.

**Why:** Slice A has no dependency on Slice B. Shipping them as two PR clusters lets Zallet integrate transparent UTXOs immediately, and lets the cookbook's worked example land before the explorer-facing tx-history surface introduces its larger storage footprint.

**How to apply:** Land Slice A end-to-end before starting Slice B's ingest path. Do not advertise either capability before the corresponding `ChainIndex` method exists; the capability-coverage test in `crates/zinder-client/tests/` is authoritative.

### D2. Slice B uses the extending-artifacts cookbook; corrections to the cookbook ship with M4

The cookbook's seven steps in [Extending artifacts](../architecture/extending-artifacts.md) are the work order. Slice B's worked example in §A worked example: transparent address tx index is normative.

The M4 implementation surfaces three cookbook corrections that ship as part of Slice B:

1. **Step 2 §Storage** distinguishes two reorg-handling patterns by key shape: per-block families use eager-delete via `ReorgWindow`; address-keyed families use dynamic-filter visibility (no physical delete; trailing `chain_epoch_id` byte plus `block_is_visible` enforce visibility at read time). The pre-M4 cookbook implied a single eager-delete path, contradicting the actual `TransparentAddressUtxo` implementation. See D9.
2. **Step 5** introduces the `AddressLookup` oneof as the canonical wire shape for address-keyed inputs. See D8.
3. **§A worked example** is updated to per-row keying with `(network, address_script_hash, block_height_be, tx_index_be)` keys and a `prost`-encoded payload, replacing the prior `Vec<TransactionId>` payload.

**Why:** The cookbook was written so M4 has zero surprises. The corrections are not deviations from the cookbook's spirit; they are doc bugs that the M4 implementation surfaces against the existing canonical example.

**How to apply:** When the implementer finds an apparent gap in the cookbook, treat it as a doc bug. Update the cookbook in the same change as the implementation rather than copying the workaround across files.

### D3. Tx-history artifact key is `(network, address_script_hash, block_height_be, tx_index_be)` with `(transaction_id, block_hash)` payload

The Slice B artifact is keyed by `(Network, TransparentAddressScriptHash, BlockHeight, TxIndexInBlock)` plus the trailing 8-byte `chain_epoch_id` (per D9's dynamic-filter pattern). The payload is `(transaction_id, block_hash)`, prost-encoded per [ADR-0002](../adrs/0002-boundary-specific-serialization.md). One row per `(address, transaction)` pair, regardless of how many transparent inputs or outputs the transaction has for that address. The `block_hash` in the payload is what `block_is_visible` compares against the visible chain at the row's height (per D9).

Pre-M4 cookbook drafts proposed `(network, address_script_hash, block_height_be)` with a `Vec<TransactionId>` payload. M4 commits per-row keying as the canonical pattern for tx-history-shaped families. Per-row keying lets pagination cursors point at exact `(height, tx_index)` boundaries without re-decoding a payload to skip past already-returned txids; lets the ingest path emit one row per transaction rather than buffering an entire block's address-to-txids map; and aligns with the trailing-`chain_epoch_id` discipline that the dynamic-filter pattern requires.

**Why:** Pagination correctness under reorgs is the critical property. A consumer that paged through height H and held a cursor at `(H, tx_index=5)` must resume cleanly even if the same address gains additional transactions in a sibling block at height H. Per-row keying makes the cursor an exact RocksDB seek; vec-payload keying requires the consumer to skip-decode a stale or replaced vec. Per-row keying is also what lets the dynamic-filter visibility check (D9) reject reorged-out rows individually instead of dropping a full vec on a single mismatch.

**How to apply:** Slice B updates the cookbook's §A worked example in the same change that lands the domain type. The cookbook's Step 2 §Storage no longer presents the vec-payload variant as an option for tx-history-shaped families; M4's per-row keying is canonical.

### D4. Pagination contract for tx-history reads

`TransparentAddressTxIdsInRange` is a server-streaming RPC. The request carries:

- `address`: an `AddressLookup` oneof carrying either `bytes script_hash` or `string address` (per D8).
- `start_height`, `end_height`: inclusive `BlockHeight` bounds.
- `max_entries`: server-bounded; `0` means use the server default.
- `from_cursor`: optional opaque `StreamCursorTokenV1` bytes for resume.
- `at_epoch`: optional `ChainEpoch` pin.
- `descending`: when `true`, the stream emits newest-first; the cursor body encodes the direction so resume continues in the same order.

The response stream emits `TransparentAddressTxIdsChunk { chain_epoch, transaction_id, block_height, tx_index_in_block, block_hash, cursor }` per message. Exposing `tx_index_in_block` lets explorers construct stable per-tx links without a follow-up call. Stream end means the bounded range was fully drained or `max_entries` was reached. The server enforces `max_entries <= max_transparent_history_entries` (default 1000, configurable via `[zinder.query] max_transparent_history_entries`).

**Why:** Native consumers must never hit an unbounded materialize-then-truncate read. The cursor reuses the existing `StreamCursorTokenV1` envelope (HMAC-authenticated, fixed 82 bytes, base64url over the wire), with a new family flag nibble `0x3` (`TransparentHistory`), so the cursor protocol stays uniform across `ChainEvents`, `MempoolEvents`, and now transparent history. Descending order is required by real explorer consumers (Zaino issue #789 against `explorer.zec.pro`); the per-row key shape supports either direction without storage changes.

**How to apply:** Slice B extends `StreamCursorTokenV1` family handling in `crates/zinder-store/src/format/stream_cursor.rs`: a new `TransparentHistoryStreamFamily` enum with flag nibble `0x3`, a body layout `(network_id, address_script_hash[32], last_height, last_tx_index, descending_bit)`, and a `decode_transparent_history` function paralleling `decode_chain_event` and `decode_mempool_event`. Cursor expiration is impossible for this family because the store keeps every retained block's history; visibility is enforced through the dynamic-filter pattern from D9, not through cursor expiration.

### D5. `GetLightdInfo.taddr_support` stays `true`; native capability gates separately

`taddr_support=true` is already advertised by the compat shim because Slice A's UTXO path is wired through the lightwalletd surface. The native capability `wallet.address.transparent_utxos_v1` remains gated on Slice A's `ChainIndex` method landing, per [Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos).

**Why:** The two surfaces have different consumer contracts. Lightwalletd clients (Zashi/Zodl, Android SDK) gate on `taddr_support`. Native Rust consumers (Zallet) gate on the capability string. They can advance independently.

**How to apply:** Do not couple the lightwalletd flag to Slice A capability advertisement. The compat shim has been answering `GetAddressUtxos[Stream]` since 2026-04-29; flipping it back during Slice A would be a regression.

### D6. `GetTaddressTransactions` decodes the same source rows as `GetTaddressTxids`

The compat shim implements both methods over a single artifact-family read. `GetTaddressTxids` returns each `TransactionId` as a `RawTransaction { hash }` envelope. `GetTaddressTransactions` resolves each `TransactionId` through `WalletQueryApi::transaction` to fill `RawTransaction { data, height }`.

**Why:** Two compat methods, one canonical family. Duplicating the storage path would re-introduce the exact "compat owns its own storage" anti-pattern called out in [Service boundaries §Anti-Patterns](../architecture/service-boundaries.md#anti-patterns) and [ADR-0004](../adrs/0004-node-source-and-protocol-boundaries.md).

**How to apply:** The compat shim's implementation lives in `services/zinder-compat-lightwalletd/src/grpc.rs` and reads through the same `WalletQueryApi` handle the rest of the shim already holds. The N+1 transaction lookup for `GetTaddressTransactions` is acceptable because lightwalletd's contract caps the request at a height range, which is already bounded.

### D7. Balance and balance-stream surfaces are owned by M5

`GetTaddressBalance` and `GetTaddressBalanceStream` are out of M4. They are scoped to **[M5: Transparent-address balance and derive-plane instantiation](m5-transparent-address-balance.md)**, which covers transparent-address balance as the first real `services/zinder-derive` consumer. The split between M4 and M5 is intentional: M4 closes the canonical wallet-shaped surfaces (UTXO and tx history); M5 closes the explorer-shaped balance surface AND instantiates the derive plane.

**Why:** Balance is an aggregation, not a per-event canonical fact. [Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family) names "precomputed totals table for explorer dashboards" as a derive-plane workload. Wallets compute balance client-side from `GetAddressUtxos` (Zashi today, Zallet today); the consumer that needs server-side balance is the explorer audience. Building balance canonically in M4 would set a precedent that aggregations live canonically when convenient, growing entropy across future milestones. M5 is the right home: it instantiates `services/zinder-derive` (currently zero source files), validates the derive-plane contract end to end, and lays foundation for future analytics workloads.

**How to apply:** The compat shim continues to return `Status::unimplemented` for both balance methods until M5 Slice B lands. The M4 native proto reserves no balance RPC name. M5's wire shape (`TransparentAddressBalance` with structured `confirmed_zat`/`unconfirmed_delta_zat`/`address_count`) is defined in the M5 spec; M4 implementations must not anticipate it.

### D8. `AddressLookup` oneof for native address-keyed RPCs

Every native `WalletQuery` RPC keyed by a transparent address takes the shared `AddressLookup` message:

```proto
message AddressLookup {
  oneof selector {
    bytes script_hash = 1;
    string address = 2;
  }
}
```

The native gRPC adapter accepts either form. Strings are parsed via `ZebraTransparentAddress::parse`, validated for the configured `Network`, and SHA-256-hashed to a `TransparentAddressScriptHash`. Parse failure returns the typed `InvalidAddress` error. The Rust `ChainIndex` API and `WalletQueryApi` accept only the typed `TransparentAddressScriptHash`; the dual wire shape is a convenience that does not relax the typed in-process boundary.

**Why:** Every consumer surveyed (lightwalletd, Zaino, Zashi/Android SDK, Zallet, Esplora) accepts string addresses on the wire; only Electrum uses script-hash. A bytes-only native API would tax every CLI, test, and debug session with a manual SHA-256 step. The oneof shape lets typed clients send `script_hash` for the high-throughput path while still accepting strings everywhere else. The compat shim stays the only place that converts strings on its existing path; the same parse logic is now shared with the native adapter.

**How to apply:** `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` defines `AddressLookup` once. Every transparent-address request in M4 (Slice A and Slice B) embeds it. The adapter parse helper lives in `services/zinder-query/src/grpc/native.rs::address_lookup_to_script_hash` so the compat shim and the native adapter share the same parsing path.

### D9. Address-keyed artifact families use dynamic-filter visibility

The transparent-address artifact families (`TransparentAddressUtxo`, `TransparentUtxoSpend`, the new `TransparentAddressTxIndex`) use the dynamic-filter reorg pattern from [Extending artifacts §Step 2](../architecture/extending-artifacts.md#step-2--storage-shape-and-schema-fingerprint-in-zinder-store). Rows are written and never physically deleted: `build_reorg_window_deletes` returns empty for the family, the trailing 8-byte `chain_epoch_id` records the source epoch on every key, and read paths enforce visibility through `source_epoch <= chain_epoch.id` plus `block_is_visible(height, expected_hash)`.

**Why:** Address-keyed keys fan out across many addresses per height; per-height eager-delete on reorg would amplify writes by `O(addresses-touched-by-block)` for every reverted height. The dynamic-filter pattern keeps reorg-revert at zero write amplification and concentrates the visibility logic in the read path, where it is testable in isolation. The pre-M4 cookbook implied that every new family extends the eager-delete `ReorgWindow` path; the actual `TransparentAddressUtxo` code uses dynamic-filter, and the cookbook is corrected in the same change that lands Slice B.

**How to apply:** In Slice B, `TransparentAddressTxIndex` follows `TransparentAddressUtxo` exactly: 8-byte trailing `chain_epoch_id` in the key, empty `build_reorg_window_deletes` branch, and a read path that calls `block_is_visible` via the existing `BlockArtifact` lookup. Search `chain_store.rs` for `ColumnFamilyName::TransparentAddressUtxo` and add a parallel `TransparentAddressTxIndex` branch wherever it appears (commit, schema fingerprint, secondary-open registration, metric labels). Do not extend `ReorgWindow` for the new family.

### D10. `TransparentAddressUtxosRequest` is rewritten as a typed native request

The current `TransparentAddressUtxosRequest` in `services/zinder-query/src/lib.rs` carries compat-origin debris: `address: String` (only round-tripped for the compat reply's `address` field) and `script_pub_key: Vec<u8>` (server-side hashed via SHA-256 inside `WalletQuery::transparent_address_utxos_at_epoch`). It is also inconsistent with the M3 mempool surface, which already uses typed `TransparentAddressScriptHash` on `TransparentMempoolOutputsRequest`.

Slice A rewrites the type:

```rust
pub struct TransparentAddressUtxosRequest {
    pub address_script_hash: TransparentAddressScriptHash,
    pub start_height: BlockHeight,
    pub max_entries: NonZeroU32,
    pub from_cursor: Option<TransparentUtxoCursor>,
}
```

`start_height` is preserved because Zashi and Zallet both use it as a wallet-birthday optimization that skips UTXOs older than the wallet's earliest interest. `BlockHeight::new(0)` scans from genesis (the genesis-scan default for explorers and unspecified callers). The `at_epoch` parameter stays as the existing `_at_epoch` companion-method pattern on `WalletQueryApi`. The compat shim becomes the only site that parses string addresses and computes SHA-256; it constructs the typed request before calling `WalletQueryApi::transparent_address_utxos`.

**Why:** Per CLAUDE.md, the project has no users yet and breaking changes are accepted in service of the cleanest architecture. Keeping the current shape would propagate compat-origin baggage into the native API and the `ChainIndex` trait, where the right contract is typed. Mempool and mined surfaces share the same parameter type; the address-script-hash derivation lives at the wire boundary, not inside the query trait.

**How to apply:** Slice A removes `address: String` and `script_pub_key: Vec<u8>` from the type. The reply construction site that copies `address` into `GetAddressUtxosReply.address` moves to the compat shim, which already holds the source string. The native adapter never touches a string address in the hot read path.

### D11. No storage migration story; existing stores are wiped

Slice B introduces a new column family (`transparent_address_tx_index`) and a new `SchemaFingerprintEntry`. Operators with existing stores (currently dev-only; no production deployments) wipe and re-bootstrap on upgrade. The Slice B changelog says so; the `zinder-ingest backfill --wallet-serving` workflow against a fresh store is the canonical path. There is no `--rebuild-family` flag, no online catchup, and no degraded-state handling for stores that predate Slice B.

**Why:** Per CLAUDE.md, the project has no users yet. A migration story for non-existent users is dead weight, and online-catchup logic would have to participate in the same atomic write batches, schema fingerprints, and reorg-window machinery as live ingest, adding meaningful complexity for zero benefit. The simplest correct code wins.

**How to apply:** Slice B's release notes tell operators to wipe stores. The `SchemaFingerprintEntry` mismatch on a pre-Slice-B store causes startup to refuse to open the store, which is the desired behavior. No code is written to migrate; no documentation describes a migration path.

## Build order

Land in order. Each phase ends with `cargo nextest run --profile=ci && cargo nextest run --profile=ci-perf` plus a fresh `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`. Live tests under each phase are gated per [ADR-0006](../adrs/0006-test-tiers-and-live-config.md).

### Slice A: native UTXO surface

#### A1. Wire shape

`crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` gains the shared `AddressLookup` (per D8) and the UTXO surface:

```proto
message AddressLookup {
  oneof selector {
    bytes script_hash = 1;
    string address = 2;
  }
}

message TransparentAddressUtxosRequest {
  AddressLookup address = 1;
  optional uint32 max_entries = 2;
  bytes from_cursor = 3;
  optional ChainEpoch at_epoch = 4;
  uint32 start_height = 5;  // 0 means scan from genesis
}

message TransparentAddressUtxo {
  bytes address_script_hash = 1;
  bytes script_pub_key = 2;
  bytes transaction_id = 3;
  uint32 output_index = 4;
  uint64 value_zat = 5;
  uint32 block_height = 6;
  bytes block_hash = 7;
}

message TransparentAddressUtxosResponse {
  ChainEpoch chain_epoch = 1;
  repeated TransparentAddressUtxo utxos = 2;
  bytes next_cursor = 3;
}

message TransparentAddressUtxosStreamChunk {
  ChainEpoch chain_epoch = 1;
  TransparentAddressUtxo utxo = 2;
  bytes cursor = 3;
}

service WalletQuery {
  // existing methods
  rpc TransparentAddressUtxos(TransparentAddressUtxosRequest) returns (TransparentAddressUtxosResponse);
  rpc TransparentAddressUtxosStream(TransparentAddressUtxosRequest) returns (stream TransparentAddressUtxosStreamChunk);
}
```

Round-trip test in `crates/zinder-proto/tests/` covers `AddressLookup` in both selector forms.

#### A2. Typed `WalletQueryApi` request

Per D10, `services/zinder-query/src/lib.rs` rewrites the existing `TransparentAddressUtxosRequest`:

```rust
pub struct TransparentAddressUtxosRequest {
    pub address_script_hash: TransparentAddressScriptHash,
    pub start_height: BlockHeight,
    pub max_entries: NonZeroU32,
    pub from_cursor: Option<TransparentUtxoCursor>,
}

#[async_trait]
impl WalletQueryApi for WalletQuery<...> {
    async fn transparent_address_utxos(
        &self,
        request: TransparentAddressUtxosRequest,
    ) -> Result<TransparentAddressUtxos, QueryError>;

    async fn transparent_address_utxos_at_epoch(
        &self,
        request: TransparentAddressUtxosRequest,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxos, QueryError>;

    async fn transparent_address_utxos_stream(
        &self,
        request: TransparentAddressUtxosRequest,
    ) -> Result<TransparentAddressUtxosStream, QueryError>;
}
```

The compat shim (`services/zinder-compat-lightwalletd/src/grpc.rs::transparent_address_utxos_request`) is the only caller that parses strings and computes SHA-256; it constructs the typed request before invoking the API.

#### A3. `ChainIndex` methods

`crates/zinder-client/src/chain_index.rs` adds three methods plus a `TransparentUtxoCursor` newtype that wraps `StreamCursorTokenV1`:

```rust
async fn transparent_address_utxos(
    &self,
    address_script_hash: TransparentAddressScriptHash,
    start_height: BlockHeight,
    max_entries: Option<NonZeroU32>,
    from_cursor: Option<TransparentUtxoCursor>,
) -> Result<TransparentAddressUtxos, IndexerError>;

async fn transparent_address_utxos_at_epoch(
    &self,
    address_script_hash: TransparentAddressScriptHash,
    start_height: BlockHeight,
    max_entries: Option<NonZeroU32>,
    from_cursor: Option<TransparentUtxoCursor>,
    at_epoch: ChainEpoch,
) -> Result<TransparentAddressUtxos, IndexerError>;

async fn transparent_address_utxos_stream(
    &self,
    address_script_hash: TransparentAddressScriptHash,
    start_height: BlockHeight,
    max_entries: Option<NonZeroU32>,
    from_cursor: Option<TransparentUtxoCursor>,
) -> Result<TransparentAddressUtxoStream, IndexerError>;
```

`start_height` is the wallet-birthday optimization that lets a Zashi/Zallet sync skip UTXOs older than the wallet's earliest interest. `BlockHeight::new(0)` scans from genesis. The existing storage primitive `transparent_utxo.rs::read_transparent_address_utxos` already takes this parameter; the native API surfaces it through.

`LocalChainIndex` calls through to `WalletQueryApi::transparent_address_utxos`. `RemoteChainIndex` calls the new tonic client methods. Both honor `at_epoch` per the existing companion-method pattern. `TransparentUtxoCursor` reuses `StreamCursorTokenV1` with a new `TransparentUtxoStreamFamily` flag nibble (`0x4`) added in `crates/zinder-store/src/format/stream_cursor.rs`.

#### A4. Native gRPC adapter

`services/zinder-query/src/grpc/native.rs` adds:

- `address_lookup_to_script_hash(AddressLookup, network) -> Result<TransparentAddressScriptHash, QueryError>`: shared parser used by every transparent-address adapter method (Slice A and Slice B). String selector is parsed via `ZebraTransparentAddress::parse`; bytes selector is taken verbatim. `InvalidAddress` maps to `tonic::Status::invalid_argument("invalid_address")` via `status_from_query_error`.
- `build_transparent_address_utxos_response`, `build_transparent_address_utxos_stream_chunk`.

`services/zinder-query/src/grpc/adapter.rs::WalletQueryGrpcAdapter` implements the two new RPC methods, mapping `QueryError` through `status_from_query_error` (no copy).

#### A5. Capability advertisement and capability-coverage test

`crates/zinder-proto/src/capabilities.rs::ZINDER_CAPABILITIES` gains `wallet.address.transparent_utxos_v1`.

`crates/zinder-client/tests/integration/capability_coverage.rs` is **created** in Slice A. The test enumerates `ZINDER_CAPABILITIES` and asserts every advertised string has a corresponding `ChainIndex` method (mapping is `wallet.<area>.<noun>_v1` to a documented method name list maintained alongside the capability constant). Today this test is referenced by `.github/workflows/protocol-contracts.yml::capability-coverage` but the test file does not exist; Slice A produces it. Failing the test on a future capability addition without a method blocks the PR.

#### A6. Tests

- Storage round-trip and reorg-revert: existing tests in `crates/zinder-store/tests/integration/chain_epoch_reader.rs::transparent_address_utxos_return_visible_remined_outpoint_after_reorg` already cover the artifact family. Extend if Slice A surfaces edge cases.
- Native gRPC: new test in `services/zinder-query/tests/integration/transparent_address_utxos.rs` exercises the adapter end-to-end with a regtest fixture, including `AddressLookup` in both selector forms.
- `ChainIndex` parity: `crates/zinder-client/tests/integration/transparent_address_utxos_parity.rs` asserts `LocalChainIndex` and `RemoteChainIndex` return identical results for the same input.
- Live regtest: new test under `services/zinder-ingest/tests/live/transparent_address_utxos.rs` mines a transparent transaction and asserts the address shows up via `WalletQuery.TransparentAddressUtxos`. Reuse `BROADCAST_TEST_SEED` from `zinder-testkit::transparent_signer` so the test does not require new operator setup.

#### A7. Doc updates

- [Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos): list the two new RPCs and the capability string.
- [Public interfaces §Vocabulary](../architecture/public-interfaces.md#vocabulary): add `AddressLookup`, `TransparentAddressUtxosRequest`, `TransparentAddressUtxosStreamChunk`, `TransparentUtxoCursor`. Add `TransparentUtxoStreamFamily` to the cursor section.
- [Public interfaces §Capability Discovery](../architecture/public-interfaces.md#capability-discovery): list `wallet.address.transparent_utxos_v1` as advertised.
- [Protocol boundary §Native API](../architecture/protocol-boundary.md#native-api): add the new messages to the surface inventory.

### Slice B: transparent tx history

Follows [Extending artifacts](../architecture/extending-artifacts.md) Step 1 through Step 7.

#### B1. Domain type

`crates/zinder-core/src/transparent_address_tx_index.rs` exports `TransparentAddressTxIndexArtifact`:

```rust
pub struct TransparentAddressTxIndexArtifact {
    pub address_script_hash: TransparentAddressScriptHash,
    pub block_height: BlockHeight,
    pub tx_index_in_block: u32,
    pub transaction_id: TransactionId,
    pub block_hash: BlockHash,
}
```

Re-export from `lib.rs`.

#### B2. Storage shape and schema fingerprint

`crates/zinder-store/src/transparent_address_tx_index.rs` adds the column family `transparent_address_tx_index`. Key layout (fixed, matching the `TransparentAddressUtxo` precedent):

```text
[KEY_VERSION=1, kind=9] ++ network_id (4 BE) ++ address_script_hash (32) ++ block_height_be (4 BE) ++ tx_index_be (4 BE) ++ chain_epoch_id (8 BE)
```

The trailing 8-byte `chain_epoch_id` records the source epoch and is what the dynamic-filter visibility check (per D9) uses at read time. Prefix scans use the first `2+4+32 = 38` bytes for `(network, script_hash)` lookups, or include `block_height_be` (`+4 = 42` bytes) for height-bounded scans.

Payload (`prost::Message` per [ADR-0002](../adrs/0002-boundary-specific-serialization.md)):

```rust
#[derive(Clone, PartialEq, Message)]
struct TransparentAddressTxIndexArtifactRecord {
    #[prost(bytes, tag = "1")] transaction_id: Vec<u8>,
    #[prost(bytes, tag = "2")] block_hash: Vec<u8>,
}
```

`PayloadFormat::ZinderTransparentAddressTxIndexArtifactV1` registered in `format/payload_format.rs`. `StorageTable::TransparentAddressTxIndex` registered in `kv/mod.rs`. `SchemaFingerprintEntry { artifact_family: ArtifactFamily::TransparentAddressTxIndex, schema_version: 1, payload_format: ZinderTransparentAddressTxIndexArtifactV1, description: "Transparent address tx history index" }` registered in `storage_control.rs`.

Per D9, this family uses dynamic-filter visibility: `build_reorg_window_deletes` returns empty, and `build_chain_epoch_puts` calls a new `push_transparent_address_tx_index_artifact_puts` paralleling `push_transparent_address_utxo_artifact_puts`. Search `chain_store.rs` for `ColumnFamilyName::TransparentAddressUtxo` and add a parallel `TransparentAddressTxIndex` branch wherever it appears (commit, schema fingerprint, secondary-open registration, metric labels). Do **not** extend `ReorgWindow` for this family.

Read paths follow `transparent_utxo.rs::read_transparent_address_utxos` exactly:

- `read_transparent_address_tx_ids_in_range(inner, chain_epoch, address_script_hash, range, max_entries, from_cursor, descending) -> Result<Vec<TransparentAddressTxIndexArtifact>, StoreError>`. Iterates the prefix in the requested direction, rejects rows with `source_epoch > chain_epoch.id`, calls `block_is_visible(height, expected_hash)`, decodes the payload, accumulates up to `max_entries`. The cursor (when present) seeds the iterator at the exact `(height, tx_index)` position.
- Exposed on `ChainEpochReader` as `transparent_address_tx_ids_in_range`.

#### B3. Ingest path

`services/zinder-ingest/src/artifact_builder.rs::IngestArtifactBuilder::build_transparent_address_tx_index_artifacts(&self, block: &SourceBlock)` walks each transaction's transparent inputs and outputs, deduplicates by `(address_script_hash, tx_index_in_block)`, and emits one artifact per matching pair.

Wire into `build_block_artifacts` so the artifact lands in the same atomic `WriteBatch` as the block's other artifacts.

#### B4. Query method

`services/zinder-query/src/lib.rs` adds:

```rust
pub struct TransparentAddressTxIdsInRangeRequest {
    pub address_script_hash: TransparentAddressScriptHash,
    pub height_range: RangeInclusive<BlockHeight>,
    pub max_entries: NonZeroU32,
    pub from_cursor: Option<TransparentHistoryCursor>,
    pub descending: bool,
}

async fn transparent_address_tx_ids_in_range(
    &self,
    request: TransparentAddressTxIdsInRangeRequest,
) -> Result<TransparentAddressTxIdsStream, QueryError>;

async fn transparent_address_tx_ids_in_range_at_epoch(
    &self,
    request: TransparentAddressTxIdsInRangeRequest,
    at_epoch: Option<ChainEpoch>,
) -> Result<TransparentAddressTxIdsStream, QueryError>;
```

`TransparentHistoryCursor` is a newtype over `StreamCursorTokenV1` with the `TransparentHistory` family flag (per D4). `WalletQueryOptions` gains `max_transparent_history_entries: NonZeroU32` (default 1000); `validate_transparent_history_range` parallels the existing `validate_block_range`. The stream emits `TransparentAddressTxIdsChunk` per item; each chunk carries the cursor for resume.

#### B5. Wire shape

`wallet.proto` adds:

```proto
message TransparentAddressTxIdsInRangeRequest {
  AddressLookup address = 1;
  uint32 start_height = 2;
  uint32 end_height = 3;
  uint32 max_entries = 4;
  bytes from_cursor = 5;
  optional ChainEpoch at_epoch = 6;
  bool descending = 7;
}

message TransparentAddressTxIdsChunk {
  ChainEpoch chain_epoch = 1;
  bytes transaction_id = 2;
  uint32 block_height = 3;
  uint32 tx_index_in_block = 4;
  bytes block_hash = 5;
  bytes cursor = 6;
}

service WalletQuery {
  rpc TransparentAddressTxIdsInRange(TransparentAddressTxIdsInRangeRequest)
      returns (stream TransparentAddressTxIdsChunk);
}
```

`AddressLookup` is the shared message defined in Slice A's A1. `descending` selects iteration direction; the cursor body encodes the direction so resume is consistent. `tx_index_in_block` is exposed on the chunk so explorers can construct stable per-tx links without a follow-up call.

#### B6. Adapters

Native: `WalletQueryGrpcAdapter::transparent_address_tx_ids_in_range`. The adapter parses the request's `AddressLookup` via the shared `address_lookup_to_script_hash` helper from Slice A's A4, decodes the cursor, and streams `TransparentAddressTxIdsChunk` messages.

Compat: `LightwalletdGrpcAdapter::get_taddress_txids` and `get_taddress_transactions` consume the new method:

- `GetTaddressTxids` decodes the request's `TransparentAddressBlockFilter`, parses the string address, constructs a typed `TransparentAddressTxIdsInRangeRequest` (descending=false, no cursor; the lightwalletd contract has no cursor), invokes the native method through the in-process `WalletQueryApi` handle, and emits one `RawTransaction { hash }` per chunk.
- `GetTaddressTransactions` resolves each `TransactionId` through `WalletQueryApi::transaction` to fill `RawTransaction { data, height }`.

Both replace the current `Status::unimplemented` branches. Remove the inline error messages.

#### B7. Tests, capability advertisement, docs

- Storage tests in `crates/zinder-store/tests/`: commit, range read (ascending and descending), reorg visibility (asserting that a reorged-out row is silently skipped, never returned), schema-fingerprint mismatch, crash recovery, cursor resume across direction changes.
- Ingest tests: `services/zinder-ingest/tests/` covers `build_transparent_address_tx_index_artifacts` deduplication semantics (one row per `(address, transaction)` pair regardless of input/output count for that address).
- Integration: `services/zinder-query/tests/integration/transparent_address_tx_history.rs` covers the full path (ingest commit -> store read -> `WalletQueryApi` -> gRPC adapter -> response), `AddressLookup` in both selector forms, and `descending` iteration.
- Compat: `services/zinder-compat-lightwalletd/tests/integration/lightwalletd_grpc.rs` covers `GetTaddressTxids` and `GetTaddressTransactions`. Pagination is exercised explicitly via the bounded height range.
- `ChainIndex` parity: `crates/zinder-client/tests/integration/transparent_address_tx_history_parity.rs` covers `LocalChainIndex` vs `RemoteChainIndex`. The capability-coverage test created in Slice A's A5 gains the new method.
- Live regtest under `services/zinder-ingest/tests/live/transparent_address_tx_history.rs`: mines two transparent transactions to the same address across two blocks, asserts both surface in ascending and descending order, asserts a reorg makes the rows invisible, and the replacement chain reintroduces them. Reuses `BROADCAST_TEST_SEED` from `zinder-testkit::transparent_signer`.
- Mutation testing focuses the slice's correctness-critical paths:

  ```bash
  cargo mutants --workspace --all-features \
    --file crates/zinder-store/src/transparent_address_tx_index.rs \
    --file crates/zinder-store/src/format/stream_cursor.rs \
    --file services/zinder-ingest/src/artifact_builder.rs \
    --re 'transparent_address_tx_ids|decode_transparent_history|build_transparent_address_tx_index_artifacts'
  ```

- Capability: `crates/zinder-proto/src/capabilities.rs::ZINDER_CAPABILITIES` gains `wallet.address.transparent_history_v1`.
- Docs:
  - [Wallet data plane](../architecture/wallet-data-plane.md): list both new RPCs, the `AddressLookup` shape, and both capability strings.
  - [Storage backend](../architecture/storage-backend.md): document the new column family, its schema fingerprint, the `kind=9` key layout (M3's `MempoolEvent` already holds `kind=8`), and the dynamic-filter visibility model. Add the new family to the visibility-seek metric label inventory.
  - [Public interfaces](../architecture/public-interfaces.md): vocabulary entries for `TransparentAddressTxIndexArtifact`, `TransparentAddressTxIdsInRangeRequest`, `TransparentAddressTxIdsChunk`, `TransparentHistoryCursor`, `TransparentHistoryStreamFamily`, `ArtifactFamily::TransparentAddressTxIndex`, `ArtifactKey::TransparentAddressTxIndex`.
  - [Protocol boundary §Native API](../architecture/protocol-boundary.md#native-api): add the new messages.
  - [Extending artifacts](../architecture/extending-artifacts.md): the cookbook corrections from D2 land in the same change. Worked example updated to per-row keying with the prost-encoded payload and the dynamic-filter reorg pattern.

## Resolved questions

The five questions raised in earlier drafts of this spec are resolved:

### R1. Address representation: `AddressLookup` oneof

The native proto accepts both `bytes script_hash` and `string address` via the `AddressLookup` oneof per D8. Every consumer surveyed (lightwalletd, Zaino, Zashi/Android SDK, Zallet, Esplora) uses string addresses on the wire; only Electrum uses script-hash. Forcing every CLI/test/debug session to compute SHA-256 of a scriptPubKey is an ergonomic tax with no offsetting safety benefit. The Rust `ChainIndex` API stays typed on `TransparentAddressScriptHash`; the dual wire shape is a convenience, not a relaxation of the typed boundary.

### R2. Streaming UTXO form: ship both

Slice A ships `TransparentAddressUtxos` (unary, bounded by `max_entries`) and `TransparentAddressUtxosStream` (server-streamed, page-bounded). Mining and exchange addresses (Zaino issue #789 documents 143K-tx addresses on `explorer.zec.pro`) need streaming to avoid a single-message size cap. Wallet flows use the unary form.

### R3. Reorg model for tx-history: dynamic-filter visibility, not eager-delete

Per D9, `TransparentAddressTxIndex` uses dynamic-filter visibility, matching the existing `TransparentAddressUtxo` precedent in `crates/zinder-store/src/transparent_utxo.rs`. The pre-M4 cookbook's claim that `ReorgWindow` handles every family uniformly was contradicted by the actual code; the cookbook is corrected as part of Slice B per D2.

### R4. No mempool-availability flag on the UTXO response

`ServerInfo`'s `ServerCapabilities` already advertises both surfaces (`wallet.address.transparent_utxos_v1` for mined; `wallet.snapshot.mempool_v1` for mempool snapshot, including `transparent_mempool_outputs_by_address`). Consumers compose them. Adding a per-response flag would duplicate the capability descriptor at the wrong granularity.

### R5. End-of-milestone capability snapshot

After M4 lands, `ZINDER_CAPABILITIES` adds two strings:

- `wallet.address.transparent_utxos_v1` (Slice A)
- `wallet.address.transparent_history_v1` (Slice B)

`taddr_support=true` continues to be advertised on `GetLightdInfo` (already shipped). The compat shim's `GetTaddressTxids` and `GetTaddressTransactions` move from `Status::unimplemented` to working. `GetTaddressBalance` and `GetTaddressBalanceStream` remain `Status::unimplemented` (deferred per D7 to a future M5 explorer-surface spec).

## ADR promotion

When both slices ship, this spec is deleted and decisions promote to **ADR-0011: Transparent-address artifact surfaces and the address-keyed dynamic-filter pattern**. The ADR captures:

- Per-row keying for tx-history (D3) as the canonical pattern for tx-history-shaped families.
- Dynamic-filter reorg model for address-keyed artifact families (D9), distinguished from eager-delete for per-block families.
- `AddressLookup` oneof as the canonical address-input shape (D8).
- `StreamCursorTokenV1` family-flag nibble `0x3` reserved for `TransparentHistory`; nibble `0x4` reserved for `TransparentUtxo`.
- Balance deferred to a future explorer-surface ADR, not to this one.

## Out of scope (reserved for future)

- `GetTaddressBalance` / `GetTaddressBalanceStream`, a native balance RPC, and transparent-address running totals. Owned by **[M5](m5-transparent-address-balance.md)** per D7. Wallet-side balance is computed client-side from the M4 UTXO surface until M5 Slice B lands.
- Shielded address surfaces. Out of scope by privacy boundary per [PRD-0001 §Out of Scope](../prd-0001-zinder-indexer.md#out-of-scope).
- Transparent address scriptPubKey templates beyond the standard P2PKH and P2SH that ingest already extracts. New script families would extend ingest, not this milestone.
- Top-addresses-by-volume, fee histograms, address-activity feeds, and other analytics views. Belong in M6+ as future derive consumers atop the M5-instantiated derive plane.
- Storage migration for pre-M4 stores. Per D11, existing dev stores are wiped on Slice B upgrade.

## Cross-references

- [PRD-0001 §Implementation Decisions](../prd-0001-zinder-indexer.md): names M4 as the fourth milestone.
- [Wallet data plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos): the public-protocol contract this spec implements.
- [Extending artifacts](../architecture/extending-artifacts.md): the seven-step cookbook Slice B follows.
- [Protocol boundary §Lightwalletd Compatibility](../architecture/protocol-boundary.md#lightwalletd-compatibility): the compat-shim contract for `GetTaddressTxids`, `GetTaddressTransactions`, `GetAddressUtxos`, `GetAddressUtxosStream`.
- [Storage backend](../architecture/storage-backend.md): column-family conventions, schema fingerprint discipline.
- [ADR-0002](../adrs/0002-boundary-specific-serialization.md): the byte rules for new keys and payloads.
- [ADR-0007](../adrs/0007-multi-process-storage-access.md): writer/reader topology that secondary-store reads inherit unchanged.
- [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md): consumer-neutral surface that both compat and native consume.
- [Findings from Android wallet integration](../reference/android-wallet-integration-findings.md): existing wallet-side validation for the UTXO path.
- [Lessons from Zaino Pattern 4](../reference/lessons-from-zaino.md#pattern-4-storage-as-a-linear-migration-ladder): the anti-pattern this spec deliberately avoids.
