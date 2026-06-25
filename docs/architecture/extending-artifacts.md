# Extending Artifacts

This document is a cookbook. When you (a contributor or an LLM agent) need to add a new artifact family to Zinder — a new chain-derived data shape that wallets, explorers, or derived-index consumers will query — follow this checklist. The steps are concrete, the examples are real, and the patterns named here are the ones you should copy.

The goal is unambiguity. After reading this document, an agent should be able to add a transparent-address artifact family without inventing new conventions, asking clarifying questions, or making decisions that conflict with the naming spine in [Public Interfaces](public-interfaces.md).

See also [Extending the wallet data plane](extending-the-wallet-data-plane.md) for adding a typed read method on existing artifacts (no new storage), and [Derive plane](derive-plane.md) for adding a new derive consumer.

## When to add an artifact family

You are adding a new artifact family when **all** of the following are true:

- The data is chain-derived. It comes from canonical chain state (blocks, transactions, mempool source events). It is not upstream-node-supplied configuration, not operator state, not derived analytics.
- The data has a stable identity that can be looked up by a key. The key fits into one of the variants of `ArtifactKey` (or extends it: `BlockHeight`, `TransactionId`, `SubtreeRootIndex`, `TransparentAddress`, `OutPoint`, future variants).
- The data is shaped enough to deserve its own schema. If the data fits into an existing artifact's payload as a new field, prefer extending that payload.
- The data is canonical, not derived in the [zinder-explorer](derive-plane.md) sense. Derived materialized views go through `zinder-explorer`, not through new canonical artifact families.

If any of these are false, you may not be adding an artifact family. Common alternatives:

| Situation | Right shape | Wrong shape |
|-----------|-------------|-------------|
| Adding a precomputed totals table for explorer dashboards | `zinder-explorer` materialized view consumed via `ChainEvents` | Canonical artifact family |
| Adding fee-rate ordering to mempool transactions | `zinder-explorer` view over `MempoolEvents` | Canonical mempool artifact |
| Adding a new field to per-block metadata | Extend `BlockHeaderArtifact` with the new typed field, bump artifact schema version | New artifact family |
| Adding transparent address transaction history | Derive-owned projection over transaction, transparent output, and transparent spend facts | Canonical ingest index, compat-shim-only feature, ad-hoc index |
| Adding transparent address output lookup for lightwalletd compatibility | New canonical transparent output artifact family with bounded address reads | On-demand compact-block scans, upstream node proxy calls, unbounded materialize-then-truncate reads |

When in doubt, the [Wallet data plane](wallet-data-plane.md) and [derive plane](derive-plane.md) docs distinguish the two boundaries.

## Response enrichment is not an artifact family

Some public responses need fields that are useful to consumers but are not
canonical artifacts themselves. Do not add a storage family just because a client
wants a more convenient response.

Use response/read-model enrichment when all of the following are true:

- The value can be computed from existing canonical artifacts, the response's
  `ChainEpoch`, or network-upgrade metadata.
- The value has no independent retention policy or lookup key.
- The value is only meaningful when bound to the same chain view as the response.

Current examples:

- `confirmations` is derived from the response `ChainEpoch` and the mined block
  height. It must not be computed from a second latest-tip read.
- `consensus_branch_id` is derived from the network upgrade active at the mined
  block height. It is not a transaction payload field.
- `block_time` can be read from the retained block or compact-block header. If
  repeated header reads become too expensive, improve the typed block-header
  read model or artifact; do not widen `TransactionFactsArtifact` to cache a duplicate copy.
- `MempoolMinedEvent.block_hash` is event enrichment from the mined block
  identity. It belongs on the event payload so consumers do not issue a racy
  follow-up read after receiving `Mined`.

The rule for enrichment is epoch binding. A response builder may synthesize
fields only from the same `ChainEpoch` or the same source event it is already
serving. It must not call the upstream node, read an unpinned latest tip, or mix
two visible epochs to fill a convenience field. If the value is added to a public
wire response, add or update the capability string in the same change.

## The seven steps

Adding a canonical artifact family touches seven layers, in this order:

1. **Domain type** in `crates/zinder-core/`.
2. **Storage shape and schema fingerprint** in `crates/zinder-store/`.
3. **Ingest path** in `services/zinder-ingest/`.
4. **Query method** on `WalletQueryApi` in `services/zinder-query/`.
5. **Wire shape** in `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`.
6. **Adapter wiring** in `services/zinder-query/src/grpc/adapter.rs` and (optionally) `services/zinder-compat-lightwalletd/src/grpc.rs`.
7. **Tests, capability advertisement, and docs.**

Each step has a concrete naming decision and a concrete file to edit. Follow them in order; they have dependencies.

### Step 1 — Domain type in `zinder-core`

A new file in `crates/zinder-core/src/`, named after the artifact family in snake case. Examples:

- `block_artifact.rs` (existing)
- `compact_block_artifact.rs` (existing)
- `address_output_index.rs` (existing)

The file exports one `*Artifact` struct or one `*Artifact` enum. The struct is named `{Family}Artifact`. Its fields are:

- An identifier appropriate to the artifact (`pub fn block_height(&self) -> BlockHeight`, `pub fn address(&self) -> &TransparentAddress`, etc.).
- A payload field that carries the artifact-specific data.
- Constructors (`new`, `with_*`) that validate invariants. No public field exposure unless the struct is a passive data record.

The file also exports any newtypes specific to the family. Examples: `SubtreeRootIndex` lives in `subtree_root.rs` because it is the family's identifier and is reused.

Re-export the type from `crates/zinder-core/src/lib.rs`:

```rust
mod transparent_output;
pub use transparent_output::TransparentUnspentOutput;
```

Naming check:

- Type name: `{Family}Artifact`. Not `{Family}Record`, `{Family}Entry`, `{Family}Data`, `{Family}Info`.
- Module name: snake_case version of the type, in the same directory.
- Identifier method: noun-phrase only. `block_height()`, `address()`, `outpoint()`. Never `get_block_height()`.
- Constructor: `new(...)`. Use `try_new(...) -> Result<Self, _>` only when construction can fail.

### Step 2 — Storage shape and schema fingerprint in `zinder-store`

A new file in `crates/zinder-store/src/` with the matching name. Inside, three things land:

1. The on-disk byte shape, expressed as `*Key` and `*Payload` types that implement the boundary serialization rules from [ADR-0002](../adrs/0002-boundary-specific-serialization.md). Fixed layout for keys (so RocksDB ordering is correct), protobuf or postcard for payloads.

2. A new column family is registered. Add it to the `ColumnFamilyName` enum (private to `zinder-store`):

```rust
enum ColumnFamilyName {
    // existing variants
    AddressOutputIndex,
}
```

The enum's `as_str()` method returns the literal column-family name written to RocksDB (`"address_output_index"`). Renaming this string later is a migration; pick carefully.

3. A schema fingerprint is added. The `SchemaFingerprint` table in `crates/zinder-store/src/storage_control.rs` gains a new entry:

```rust
SchemaFingerprintEntry {
    artifact_family: ArtifactFamily::AddressOutputIndex,
    schema_version: 1,
    payload_format: PayloadFormat::Protobuf,
    description: "Transparent address output index",
}
```

`schema_version` increments only when the on-disk shape changes in a way readers must understand. Adding new fields to the protobuf payload is additive (no version bump). Removing or repurposing fields is a version bump and a migration.

Two read paths are exposed:

- `read_{family}_artifact(...)` on `ChainEpochReader` (per the rule: every new artifact family is reachable via `ChainEpochReader` for in-process reads).
- `{family}_at(...)` or `{family}_by_{key_noun}(...)` on `ChainEpochReadApi` (the public read trait).

A write path is exposed only to `zinder-ingest`. `PrimaryChainStore::commit_chain_epoch` is extended to accept the new artifact in its commit batch. **The write path must participate in the atomic `WriteBatch`**; an artifact written separately from the visible-epoch pointer breaks epoch consistency.

#### Reorg model: pick by key shape

There are two reorg-handling patterns. The right one is determined by the family's key shape, not by preference.

1. **Per-block families** (compact blocks, transactions, tree states, subtree roots): use **eager-delete**. The key contains a single `BlockHeight` (no per-address dimension), so a reorged height has exactly one row to revert. The `ReorgWindow` table tracks the family's entries within the reorg window. Reorg deletes must include the new family's keys for the reverted range. Search `chain_store.rs` for `ColumnFamilyName::CompactBlock` and add a parallel branch for the new family wherever one appears.

2. **Address-keyed canonical families** (transparent address outputs and future address-shaped canonical artifacts): use **dynamic-filter visibility**. The key fans out across many addresses per height, which makes per-height eager-delete an O(addresses-touched-by-block) write amplification on every reorg. Instead, rows are written and never physically deleted. `build_reorg_window_deletes` returns empty for the family. The key carries a trailing `chain_epoch_id` (8 BE bytes) recording the source epoch. Read paths enforce visibility at scan time:
   - Reject rows whose `source_epoch > chain_epoch.id` (visibility filter).
   - Call `block_is_visible(height, expected_hash)`, which reads the `BlockHeaderArtifact` at the row's height and rejects rows whose stored `block_hash` does not match the visible chain at that height.

   Reorg-revert is logical, not physical; reorged-out rows are silently skipped on every read. The canonical example is `crates/zinder-store/src/address_output_index.rs::read_address_output_index_rows_paged` together with the key layouts in `format/store_key.rs`.

Mixing the patterns within one family is a design error. Pick eager-delete for `(network, height, ...)` families and dynamic-filter for `(network, address_script_hash, ...)` families. The trailing `chain_epoch_id` byte and the `block_is_visible` check are non-optional for dynamic-filter families.

**This step is the most common silent-failure point in either pattern.** For per-block families, omitting a delete branch silently retains stale state. For address-keyed families, omitting the trailing `chain_epoch_id` byte from the key or skipping `block_is_visible` at read time silently exposes reorged-out rows.

### Step 3 — Ingest path in `zinder-ingest`

A new artifact builder. The pattern is established by `services/zinder-ingest/src/artifact_builder.rs::IngestArtifactBuilder`.

- Add a build method: `build_{family}_artifact(&self, block: &SourceBlock) -> Result<Option<{Family}Artifact>, IngestError>`. The method returns `None` if the artifact does not apply to this block (e.g. a transparent-address artifact returns `None` for blocks with no transparent transactions).
- Wire the builder into `IngestArtifactBuilder::build` so that every committed block produces the artifact in the right order.
- The artifact's data flow follows the documented commit protocol (per [`CLAUDE.md`](../../CLAUDE.md) §Commit and Read Protocol):

```text
observe_chain_source -> fetch_missing_ancestors -> select_best_chain
  -> build_block_artifacts -> build_compact_block_artifacts
  -> commit_ingest_batch -> finalize_tip_if_ready
```

The new builder runs as part of `build_block_artifacts` or `build_compact_block_artifacts`, depending on whether the artifact is at the wallet-protocol-compact level or the canonical-block level. Most new artifacts are canonical-block-level; choose `build_block_artifacts`.

If the artifact requires source-side data not currently captured (e.g. nullifier-to-spending-tx requires Zebra's `--features indexer`), advertise the dependency as a `NodeCapabilities` flag and gate the builder behind it. Failing-soft-and-disabling-feature is preferred to failing-hard-at-startup; expose the disabled state via `ServerCapabilities` (omit the corresponding capability string).

### Step 4 — Query method on `WalletQueryApi`

Add the method to the `WalletQueryApi` trait in `services/zinder-query/src/lib.rs`. Follow [Public Interfaces §Method Naming Conventions](public-interfaces.md#method-naming-conventions):

- Single-key lookup with `BlockHeight`: `{family}_at(height: BlockHeight, at: Option<ChainEpoch>) -> Result<{Family}, QueryError>`.
- Single-key lookup with non-height key: `{family}_by_{key_noun}(key: KeyType, at: Option<ChainEpoch>) -> Result<{Family}, QueryError>`.
- Range read: `{family}s_in_range(range: Range, at: Option<ChainEpoch>) -> impl Stream<Item = Result<{Family}, QueryError>>`.

The `at: Option<ChainEpoch>` parameter is mandatory on every method that depends on chain state. Default behavior (when `at` is `None`) is "the visible epoch at call time."

The implementation in `WalletQuery<ReadApi, Broadcaster>` follows the pattern in `services/zinder-query/src/lib.rs`:

```rust
async fn {family}_by_{key_noun}(
    &self,
    key: KeyType,
    at: Option<ChainEpoch>,
) -> Result<{Family}, QueryError> {
    let read = self.read_api.clone();
    spawn_blocking(move || {
        let epoch = at.unwrap_or_else(|| read.current_epoch())?;
        read.read_{family}_artifact(&epoch, key)
            .ok_or(QueryError::ArtifactUnavailable {
                family: ArtifactFamily::{Family},
                key: ArtifactKey::{KeyVariant}(key),
            })
    })
    .await
    .map_err(|_| QueryError::StorageUnavailable {
        kind: StorageErrorKind::TaskJoin,
    })?
}
```

Error mapping follows [Public Interfaces §Error Conventions](public-interfaces.md#error-conventions). Per-artifact unavailability is **always** `ArtifactUnavailable { family, key }`, never a new top-level variant. Add the `ArtifactFamily::{Family}` and `ArtifactKey::{KeyVariant}` variants if they do not exist.

### Step 5 — Wire shape in `wallet.proto`

Add the RPC method to the `WalletQuery` service:

```proto
rpc {Family}By{KeyNoun}({Family}By{KeyNoun}Request) returns ({Family}Response);
```

(Or `{Family}At` for height-keyed lookups, `{Family}sInRange` for range reads.)

Add the request and response messages. The request mirrors the typed signature: it carries the lookup key and an optional epoch pin. The response carries the resolved epoch and the artifact payload.

Naming check:

- RPC name uses PascalCase: `AddressOutputIndexByAddress`.
- Request and response messages use the RPC name as a prefix: `AddressOutputIndexByAddressRequest`, `AddressOutputIndexByAddressResponse`.
- Field names use snake_case in proto, matching the typed Rust signature. The lookup key field is named after the key noun: `address`, `transaction_id`, `subtree_root_index`.
- Optional epoch pin: `optional ChainEpoch at_epoch = N;` (proto field name `at_epoch`, not `at`, because `at` is reserved-looking in some languages; the typed Rust API converts).

#### Address-keyed inputs use the `AddressLookup` oneof

When a request key is a transparent address, the request takes the `AddressLookup` shared message:

```proto
message AddressLookup {
  oneof selector {
    bytes script_hash = 1;
    string address = 2;
  }
}

message AddressOutputIndexByAddressRequest {
  AddressLookup address = 1;
  // ... other fields
}
```

The native gRPC adapter accepts either form on the wire, parses the string variant via `ZebraTransparentAddress::parse` plus SHA-256, and returns typed `InvalidAddress` for parse failures. The Rust `ChainIndex` API accepts only the typed `TransparentAddressScriptHash`; the dual wire shape is a convenience for clients (CLIs, debug tools, third-party SDKs) and does not relax the typed boundary on the in-process API. The compatibility shim is the only other site that converts strings to typed addresses.

Do not invent new cursor types for non-streaming methods. If the response is bounded, return a single message. If it is a range read with a server stream, follow the cursor conventions in [Public Interfaces §Cursor Conventions](public-interfaces.md#cursor-conventions).

Add a capability string. New methods require a new capability per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery). Declare the `pub const` string, then add a `CapabilitySpec` row to the `CAPABILITIES` table binding it to its surface, proto method, and advertise policy:

```rust
// crates/zinder-proto/src/capabilities.rs
pub const WALLET_ADDRESS_TRANSPARENT_HISTORY_V1: &str = "wallet.address.transparent_history_v1";

pub const CAPABILITIES: &[CapabilitySpec] = &[
    // existing rows
    CapabilitySpec::new(
        WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentAddressTxIdsInRange"),
        AdvertisePolicy::AlwaysOn,
    ),
];
```

The `capability_descriptor_drift` CI guard rejects the change if the proto method has no table row or the row's method does not match the descriptor.

For the lightwalletd compatibility slice, the transparent output family is
served natively by `WalletQuery.TransparentAddressUnspentOutputs` under
`wallet.address.transparent_unspent_outputs_v1`. The lightwalletd
`GetAddressUtxos[Stream]` adapter reads from the same stored transparent
output artifacts and can therefore set `GetLightdInfo.taddrSupport=true`.

### Step 6 — Adapter wiring

Two adapters consume `WalletQueryApi`:

**Native adapter (`services/zinder-query/src/grpc/adapter.rs::WalletQueryGrpcAdapter`)**: implement the new tonic RPC method. Use the shared `status_from_query_error` mapping function in `services/zinder-query/src/grpc/mod.rs`. Build the response via a new function in `services/zinder-query/src/grpc/native.rs::build_{family}_response`.

**Compat shim adapter (`services/zinder-compat-lightwalletd/src/grpc.rs::LightwalletdGrpcAdapter`)**: only if the artifact has a corresponding lightwalletd `CompactTxStreamer` method. If yes, translate; if no, the artifact is native-only and the compat shim is unaffected. Do not invent a parallel surface in the compat shim — that violates [ADR-0004](../adrs/0004-node-source-and-protocol-boundaries.md).

For both adapters, the error mapping function is **shared, not duplicated**. If you find yourself copying a `match` over `QueryError` variants, you have made a mistake; use `services/zinder-query/src/grpc/mod.rs::status_from_query_error` instead.

### Step 7 — Tests, capability advertisement, and docs

**Unit tests** in the artifact's source file. Cover the constructor, accessors, and any invariants.

**Storage tests** in `crates/zinder-store/tests/`. Cover commit, reorg-revert, range read, schema-fingerprint mismatch, and crash recovery for the new column family. Use the pattern in `crates/zinder-store/tests/commit_chain_epoch.rs` as a template.

**Integration tests** in `services/zinder-query/tests/`. Cover the full path: ingest commits → store reads → WalletQueryApi → gRPC adapter → response. The pattern in `services/zinder-query/tests/single_artifact_lookup.rs` is the template.

**Live tests** under `services/zinder-ingest/tests/live/`, gated per the [Testing Runbook](../runbooks/testing.md): `#[ignore = LIVE_TEST_IGNORE_REASON]` plus `require_live()`, with the unified `ZINDER_NETWORK` and `ZINDER_NODE__*` env-var schema. Add a test that exercises the artifact against a live regtest stack.

**Capability advertisement**. Add a `CapabilitySpec` row to the `CAPABILITIES` table in `crates/zinder-proto/src/capabilities.rs`. The `capability_descriptor_drift` CI guard verifies the row against the compiled `FileDescriptorSet`.

**Doc updates**. The change is incomplete until at least these are updated:

- [Public interfaces §Vocabulary](public-interfaces.md#vocabulary): the new `*Artifact`, `*Response`, `*Request`, and `ArtifactFamily` variant names are added.
- [Wallet data plane](wallet-data-plane.md): the new RPC and its capability string are listed.
- [Storage backend §Storage Families](storage-backend.md): the new column family, its schema fingerprint, and its retention behavior (if any) are documented.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery): the new capability string is listed.

If the artifact requires a new source capability (e.g. nullifier-to-spending-tx requires Zebra's `--features indexer`), [Node source boundary](node-source-boundary.md) is amended too.

If the artifact changes the on-disk shape of an existing artifact, [ADR-0002](../adrs/0002-boundary-specific-serialization.md) and [Storage backend](storage-backend.md) §Schema Versioning are amended.

## A worked example: transparent address transaction history

To make the cookbook concrete, here is the path for transparent-address transaction history, the derive-owned projection that backs paginated `GetTaddressTxids`.

The transparent output artifact follows the same seven-step path,
but its priority and capability are different: it is served under
`wallet.address.transparent_unspent_outputs_v1`, while this
transaction-history example uses `wallet.address.transparent_history_v1`.

1. **Domain type**: `crates/zinder-core/src/transparent_address_tx_index.rs` exporting `TransparentAddressTxIndexArtifact { address_script_hash, block_height, tx_index_in_block, transaction_id, block_hash }`. One row per `(address, transaction)` pair, regardless of how many transparent inputs or outputs the transaction has for that address.

2. **Storage**: derive consumer column families `transparent_address_transaction_history`, `transparent_address_transaction_history_descending`, and `transparent_address_transaction_history_index`. Primary keys are `(address_script_hash, block_height_be, tx_index_be)` in ascending or descending height order. The value is the transaction id and block hash. The per-height index stores both primary keys so reorg rollback deletes exact rows.

   Per-row keying lets pagination cursors point at exact `(height, tx_index)` boundaries without re-decoding a vec payload to skip already-returned txids; this is the canonical shape for tx-history-keyed projections.

3. **Derive**: `TransparentAddressTransactionHistoryConsumer` walks canonical transaction facts, transparent outputs, and hydrated transparent spends, deduplicates by `(address_script_hash, tx_index_in_block)`, and emits one row per matching pair. Canonical ingest does not write this projection.

4. **Query**: `WalletQueryApi::transparent_address_tx_ids_in_range(request: TransparentAddressTxIdsInRangeRequest) -> impl Stream<Item = Result<TransparentAddressTxIdsChunk, QueryError>>`. The request carries the typed `address_script_hash: TransparentAddressScriptHash`, an inclusive height range, `max_entries` bounded by `WalletQueryOptions::max_transparent_history_entries`, an opaque `from_cursor` for resume, and a `descending: bool` flag for newest-first iteration. The derive projection is current-only; each response chunk carries the `chain_epoch` used to answer the read instead of accepting an `at_epoch` pin.

5. **Wire**:

   ```proto
   rpc TransparentAddressTxIdsInRange(TransparentAddressTxIdsInRangeRequest)
       returns (stream TransparentAddressTxIdsChunk);

   message TransparentAddressTxIdsInRangeRequest {
     AddressLookup address = 1;
     uint32 start_height = 2;
     uint32 end_height = 3;
     uint32 max_entries = 4;
     bytes from_cursor = 5;
     bool descending = 6;
   }

   message TransparentAddressTxIdsChunk {
     ChainEpoch chain_epoch = 1;
     bytes transaction_id = 2;
     uint32 block_height = 3;
     uint32 tx_index_in_block = 4;
     bytes block_hash = 5;
     bytes cursor = 6;
   }
   ```

   Capability string: `wallet.address.transparent_history_v1`.

   The cursor is a `StreamCursorTokenV1` token with the `TransparentHistory` family flag (nibble `0x3`); the body encodes `(network_id, address_script_hash, last_height, last_tx_index, descending_bit)`. Decoding follows the same HMAC-verify-then-parse pattern as `decode_chain_event` and `decode_mempool_event`; mismatched family produces `StreamCursorError::StreamFamilyMismatch`.

6. **Adapters**: native gRPC adapter implements `transparent_address_tx_ids_in_range`. Compat shim adapter implements `GetTaddressTxids` and `GetTaddressTransactions` over the same native method: `GetTaddressTxids` emits each `TransactionId` as `RawTransaction { hash }`, and `GetTaddressTransactions` resolves each `TransactionId` through `WalletQueryApi::transaction` to fill `RawTransaction { data, height }`. The lightwalletd contract has no cursor; the compat shim drains the native stream within the requested height range and accepts the bounded result.

7. **Tests, capability, docs**: new tests in store, query, compat (`tests/integration/`), and a live test under `services/zinder-ingest/tests/live/`. Capability added. `wallet-data-plane.md`, `storage-backend.md`, `public-interfaces.md` amended. The capability-coverage test in `crates/zinder-client/tests/integration/capability_coverage.rs` asserts the new `ChainIndex` method exists.

## Common mistakes

The DX/AX audit and code review have surfaced a recurring set of errors. Each is preventable by following the cookbook.

| Mistake | Why it happens | Right shape |
|---------|----------------|-------------|
| Adding a top-level error variant for the new family (`TransparentAddressUnavailable`) | The old split-error precedent suggests it | Use `ArtifactUnavailable { family, key }` |
| Forgetting to extend the `ReorgWindow` delete path in `chain_store.rs` | The pattern is implicit, not enforced | Search `chain_store.rs` for `ColumnFamilyName::CompactBlock` and add a parallel branch wherever it appears (per-block families only) |
| Applying eager-delete `ReorgWindow` semantics to an address-keyed family | Two reorg patterns coexist and the right one is decided by key shape, not family | Pick by key shape per Step 2: per-block keys use eager-delete; address-keyed keys use dynamic-filter. Address-keyed families must include the trailing `chain_epoch_id` byte and call `block_is_visible` at read time |
| Taking `bytes script_pub_key` or `bytes address_script_hash` only on a public address-keyed RPC | Looks more typed than `string`; in practice it taxes every CLI/test/debug session | Use the `AddressLookup` oneof; the adapter parses the string variant. The Rust `ChainIndex` API stays typed on `TransparentAddressScriptHash` |
| Duplicating `status_from_query_error` in the new adapter | The function is named in two services and looks copy-friendly | Import from `services/zinder-query/src/grpc/mod.rs`; never copy |
| Inventing a new cursor type for a one-off paginated read | The cursor section is long; skipping seems efficient | Use `StreamCursorTokenV1` with the appropriate family tag (chain-event, mempool-event, transparent-history, address-output, or snapshot-page) |
| Naming the method `get_*` or `fetch_*` | These verbs are common in other codebases | Use the bare-noun convention: `transparent_address_tx_ids_in_range`, not `get_transparent_address_tx_ids` or `fetch_*` |
| Mixing `_secs` and `_ms` for the new feature's config knobs | Duration units become inconsistent across services | Use `_ms` for durations; see [Public Interfaces §Configuration Conventions](public-interfaces.md#configuration-conventions) |
| Skipping the capability string update | The CI job will catch it, so why be careful upfront | Add the `CapabilitySpec` row to `CAPABILITIES` in the same commit as the proto method; treat it as part of the proto change |
| Adding the artifact to the compat shim because "wallets need it" | The compat shim is a translation layer, not a feature surface | If the lightwalletd protocol does not name the method, the compat shim does not implement it; native-only is fine |

## Cross-references

- [Public Interfaces](public-interfaces.md) — the vocabulary spine and naming rules referenced throughout this cookbook.
- [Storage Backend](storage-backend.md) — column family registration, schema fingerprint conventions, retention.
- [Chain Events](chain-events.md) — for artifacts that emit events on commit.
- [Wallet Data Plane](wallet-data-plane.md) — the public protocol surface inventory the new RPC will appear in.
- [Derive Plane](derive-plane.md) — the alternative boundary if your data is derived rather than canonical.
- [ADR-0002: Boundary-specific serialization](../adrs/0002-boundary-specific-serialization.md) — the byte rules for new keys and payloads.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — capability-string conventions.
