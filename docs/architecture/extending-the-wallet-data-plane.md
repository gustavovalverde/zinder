# Extending the Wallet Data Plane

This document is a cookbook. When you (a contributor or an LLM agent) need to add a new typed read method to Zinder's wallet data plane (a new `WalletQuery` RPC, a new `ChainIndex` trait method, or a new federated derive-plane handler), follow this checklist. The 14 steps are concrete, the examples are real, and the patterns named here are the ones to copy.

The goal is unambiguity. After reading this document, an agent should be able to add a new typed read method without inventing new conventions, asking clarifying questions, or making decisions that conflict with the naming spine in [Public Interfaces](public-interfaces.md).

## Purpose and scope

This document covers three related but distinct extension shapes. Pick the right doc before starting:

| Adding | Doc | When |
|---|---|---|
| A new typed read method on existing artifacts | This doc | The data is already in storage; you need a new way to access it |
| A new artifact family (new storage) | [Extending artifacts](extending-artifacts.md) | The data is chain-derived but not yet persisted |
| A new derive consumer (federated method) | This doc §Federation extension + [Derive plane](derive-plane.md) | The data is materialized in `zinder-explorer` and surfaced through `WalletQuery` |

Each wire shape pairs with one capability string. Changing the shape of an `_v1` response requires landing a new `_v2` capability; the `_vN` suffix is part of the identity, not a version field decoded by clients (per [Public interfaces §Capability discovery](public-interfaces.md#capability-discovery)).

## The 14-step canonical file list

Adding a new typed read method touches 14 files, in this order. Skipping a step usually means a downstream layer cannot reach what you added.

### Step 1 — Proto definition

File: `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`

Add a request message, a response message, and the `rpc` entry on `service WalletQuery`.

- Every response message carries `ChainEpoch chain_epoch = 1` as its first field (the chain epoch used to answer the query).
- Discriminated shapes use `oneof status` / `oneof selector`. Never use sentinel integer fields (see anti-pattern A4).
- Reserved-for-future-fields shapes (e.g. `ConflictingChainTransaction`) use empty messages with explicit comments.

Worked example: `BlockSelector` oneof in `wallet.proto` for the `BlockIdBySelector` and `BlockHeaderBySelector` RPCs (Primitive A).

### Step 2 — Core type

File: `crates/zinder-core/src/<module>.rs` (one file per domain concept)

Add the typed Rust shape that the trait surface uses.

- Newtypes wrapping `[u8; 32]` get `from_bytes` / `as_bytes` constructors and `#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]`.
- Read-model structs (enriched at response time, not persisted) get `Clone, Copy, Debug, Eq, PartialEq`.
- Enums that future-proof against unknown server variants get `#[non_exhaustive]`.
- Add `mod <module>;` and `pub use self::<module>::*;` in `lib.rs`. No tree-shaking by feature.

Worked examples: `BlockSelector`, `TxStatus`, `MinedDetails`, `BlockHeaderInfo`, `TransparentAddressBalance`.

### Step 3 — ChainIndex trait method

File: `crates/zinder-client/src/chain_index.rs`

Add `async fn method_name(&self, ...) -> Result<ReturnType, IndexerError>` to the `ChainIndex` trait.

- Epoch-pinnable reads take `at_epoch: Option<ChainEpoch>` as the last argument. `None` resolves to the live tip; `Some(epoch)` pins.
- Mempool reads that cannot honor epoch pinning omit `at_epoch` (mempool is live state, not pinnable).
- Methods that return paged data define the view struct and cursor newtype in this same file.
- Default-body methods (e.g. `chain_events` calling `chain_events_for_family`) live here.

### Step 4 — LocalChainIndex implementation

File: `crates/zinder-client/src/local.rs`

Implement the new method on `LocalChainIndex`.

- Reads backed by RocksDB go through `self.read_at_epoch(at_epoch, move |reader| { ... })`. The closure runs inside `spawn_blocking`.
- Reads requiring live writer state (mempool, broadcast, chain events) delegate to `self.remote("operation_name")?` since the canonical writer is not in this process.

### Step 5 — RemoteChainIndex implementation

File: `crates/zinder-client/src/remote.rs`

Implement the new method on `RemoteChainIndex`.

- Build the proto request inline; call `self.client().await.rpc_name(Request::new(...))`.
- Decode the response via `from_message` free functions.
- Fixed-32-byte fields use `fixed_32_bytes(field, bytes)?`.
- Map errors with `IndexerError::from_status`.

### Step 6 — WalletQueryApi trait method

File: `services/zinder-query/src/lib.rs`

Add the method to the `WalletQueryApi` trait and a concrete impl on `WalletQuery<ReadApi, Broadcaster>`.

- Each impl wraps a `spawn_blocking` closure that opens a `ChainEpochReader` via `open_chain_epoch_reader` and reads from it.
- Every method calls `record_wallet_query_outcome("method_name", ...)` at its end for metrics.
- Define the response value struct (e.g. `BlockHeaderResponseValue`) in `lib.rs` alongside the trait.

### Step 7 — Native encoder

File: `services/zinder-query/src/grpc/native.rs`

Add `pub async fn method_name_response<Q: WalletQueryApi + ?Sized>(query_api: &Q, ...) -> Result<wallet::ResponseType, QueryError>`. The function calls `query_api.method_name(...)`, then translates the core type to the proto message via a `build_*` free function. Use `build_chain_epoch_message` from `zinder_store` to convert the `ChainEpoch`.

### Step 8 — gRPC adapter handler

File: `services/zinder-query/src/grpc/adapter.rs`

Add the `async fn method_name` arm on `impl<QueryApi> wallet_query_server::WalletQuery for WalletQueryGrpcAdapter<QueryApi>`. Pattern:

```rust
async fn method_name(
    &self,
    request: Request<RequestType>,
) -> Result<Response<ResponseType>, Status> {
    let (selector, epoch) = parse_inputs(request.into_inner())?;
    method_name_response(&self.query_api, selector, epoch)
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
}
```

Input-side translation helpers (proto-to-core) live as free functions at the bottom of `adapter.rs`.

### Step 9 — Capability string

File: `crates/zinder-proto/src/capabilities.rs`

Append the capability string to `ZINDER_CAPABILITIES`. Format: `domain.subdomain.capability_name_v{N}`.

| Namespace | Used for |
|---|---|
| `wallet.read.*` | Canonical block / transaction / tree-state reads |
| `wallet.mempool.*` | Live mempool point lookups (writer-owned, no epoch pinning) |
| `wallet.address.*` | Transparent-address reads |
| `wallet.events.*` | Streaming event families (chain, mempool) |
| `wallet.snapshot.*` | Bounded snapshot reads |
| `wallet.broadcast.*` | Write paths |
| `derive.{consumer}.*` | Federated derive-plane methods (one consumer per namespace) |

Wallet-plane RPCs own `wallet.*`; derive-backed methods own `derive.*`. Mixing namespaces fails capability-coverage tests.

### Step 10 — Capability-coverage row

File: `crates/zinder-client/tests/integration/capability_coverage.rs`

Add a row to `EXPECTED_METHOD_NAMES`: `("wallet.<subdomain>.<op>_v1", "method_name_on_chain_index_trait")`. Add `let _ = T::method_name;` in `assert_wallet_chain_index_methods_compile<T: ChainIndex>()`. Both must land in the same commit as the new capability string.

### Step 11 — Capability-docs mirrors

Files: `docs/architecture/public-interfaces.md` and `docs/runbooks/testing.md`

Both files carry a `<!-- capability-list:*:start/end -->` block that mirrors `ZINDER_CAPABILITIES`. The test `crates/zinder-proto/tests/integration/capability_docs.rs` asserts set-equality. Failing to update both docs makes CI fail.

### Step 12 — Compat shim handler (if applicable)

File: `services/zinder-compat-lightwalletd/src/grpc.rs`

Add the arm only if the lightwalletd `CompactTxStreamer` proto names a corresponding method.

- The compat shim reads only through `self.query_api.method_name(...)` on `WalletQueryApi`; it never touches RocksDB or gRPC directly.
- The compat shim builds lightwalletd-shaped types directly, projecting confirmed/unconfirmed splits onto single-field lightwalletd shapes (e.g. `Balance { value_zat }` is the confirmed total; `unconfirmed_delta_zat` is silently dropped at the lightwalletd boundary).
- Inventing surfaces absent from the vendored `CompactTxStreamer` proto is forbidden per [Service boundaries](service-boundaries.md).

### Step 13 — Integration tests

File: `services/zinder-query/tests/integration/<area>.rs`

One test module per new operation. Pattern: `StoreFixture::open()` writes synthetic artifacts, constructs `WalletQuery::new(store, ())`, calls the method, asserts on response fields. Error paths ("returns unavailable for missing X", "reports not_found") are separate test functions.

### Step 14 — Perf smoke test (range reads only)

File: `services/zinder-query/tests/perf/perf_smoke.rs`

Required for range reads where N-block reads could regress. Not required for point lookups. Budget constants are deliberately loose for CI workers; tight numbers live in the architecture docs.

## Federation extension

When the new RPC delegates to `zinder-explorer`'s `ExplorerQuery` or another derive consumer, add everything above plus seven sub-steps. The design rationale and boundary rules live in [Derive plane §Shape 2](derive-plane.md#shape-2--federated-under-walletquery).

### F1. ExplorerQuery proto

File: `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto`

Add the RPC mirroring the WalletQuery shape.

### F2. DeriveProxy<C> field on the adapter

File: `services/zinder-query/src/grpc/adapter.rs`

Add an `Option<DeriveProxy<ConsumerQueryClient<AuthenticatedChannel>>>` field to `WalletQueryGrpcAdapter`. Add a `with_<consumer>_proxy(mut self, proxy: DeriveProxy<...>) -> Self` builder and a `<consumer>_proxy_readiness(&self) -> Option<DeriveReadinessGauge>` getter.

### F3. Federated handler

In the WalletQuery server impl arm:

```rust
async fn rpc_name(
    &self,
    request: Request<RequestType>,
) -> Result<Response<ResponseType>, Status> {
    let proxy = self
        .<consumer>_proxy
        .as_ref()
        .ok_or_else(|| Status::unavailable("derive consumer not configured"))?;
    proxy
        .forward(request, |mut client, request| async move {
            client.rpc_name(request).await
        })
        .await
}
```

No translation logic: proto messages flow through unchanged. `DeriveProxy::forward` gates on `is_ready()` before opening the channel.

### F4. ServerInfo capability gating

In the `server_info` handler on `WalletQueryGrpcAdapter`: include the federated capability only when the proxy is configured and the readiness gauge reports `is_ready()`. If the proxy is unconfigured or unhealthy, actively remove the string from the advertised list.

### F5. Readiness probe

In the binary's startup (`services/zinder-query/src/bin/zinder-query/main.rs`): construct `DeriveProxy::new(DeriveProxyConfig { endpoint, bearer_token, capability }, ConsumerQueryClient::new)`, extract its readiness gauge via `.readiness()`, and pass both to `spawn_derive_readiness_probe(gauge, probe_fn, config, cancel)`.

### F6. Two capability strings per consumer

File: `crates/zinder-proto/src/capabilities.rs`

- `derive.<consumer>.server_info_v1`: advertised by the derive service's own `ServerInfo`; the readiness probe polls for this string.
- `derive.<consumer>.<method>_v1`: advertised by `zinder-query` when the readiness gauge is `true`.

### F7. Compat shim derive wiring (if applicable)

File: `services/zinder-compat-lightwalletd/src/grpc.rs`

`LightwalletdGrpcAdapter` has its own `<consumer>_proxy` field, wired via `.with_<consumer>_proxy(proxy)`. The compat shim calls `proxy.forward(request, |mut client, request| ...)` directly; it does not go through `WalletQueryApi`.

## Response enrichment rule

Some response fields are useful to consumers but are not canonical artifacts. The rule is `chain_epoch` binding: a response builder may synthesize fields only from the same `ChainEpoch` it is already serving. It must not call the upstream node, read an unpinned latest tip, or mix two visible epochs.

`MinedDetails::from_response_epoch(epoch, mined_height, consensus_branch_id, block_time)` in `crates/zinder-core/src/transaction.rs` is the canonical exemplar. The constructor computes `confirmations` from `epoch.tip_height`. This is an entropy gate: the caller must already hold the response's `ChainEpoch` in scope when computing confirmations, so it is structurally impossible to accidentally use a re-read tip.

Any future enrichment field that depends on tip state takes the response's `ChainEpoch` as a parameter and computes deterministically. Cross-link: [`extending-artifacts.md` §Response enrichment is not an artifact family](extending-artifacts.md#response-enrichment-is-not-an-artifact-family) covers the related rule that response enrichment must not promote to a new artifact family.

## Capability namespace conventions

(Repeated from Step 9 for visibility.)

| Namespace | Used for | Example |
|---|---|---|
| `wallet.read.*` | Canonical block / transaction / tree-state reads | `wallet.read.transaction_by_id_v1` |
| `wallet.mempool.*` | Live mempool point lookups | `wallet.mempool.transparent_outputs_by_address_v1` |
| `wallet.address.*` | Transparent-address reads | `wallet.address.transparent_unspent_outputs_v1` |
| `wallet.events.*` | Streaming event families | `wallet.events.chain_v1` |
| `wallet.snapshot.*` | Bounded snapshot reads | `wallet.snapshot.mempool_v1` |
| `wallet.broadcast.*` | Write paths | `wallet.broadcast.transaction_v1` |
| `<product>.<noun>.*` | Federated explorer/analytics-plane methods | `explorer.transparent_address.balance_v1` |

Storage tier and lifecycle drive the namespace; do not mix. Putting a derive-backed method under `wallet.*` fails capability-coverage tests.

## Two discipline gates

CI tests that catch the most common close-out mistakes:

1. **Capability coverage**: `crates/zinder-client/tests/integration/capability_coverage.rs`. Compile-time and runtime: every string in `ZINDER_CAPABILITIES` has a row in `EXPECTED_METHOD_NAMES`; `assert_wallet_chain_index_methods_compile<T>` references each method by function-item, so renaming a method breaks the build.
2. **Capability docs**: `crates/zinder-proto/tests/integration/capability_docs.rs`. Parses the `<!-- capability-list:*:start/end -->` blocks in `public-interfaces.md` and `testing.md` and asserts set-equality with `ZINDER_CAPABILITIES`.

A close-out PR that fails any of these has not landed correctly.

## Anti-patterns to refuse

The wallet data plane refuses these shapes because they couple the native API to
implementation-specific transport habits. A PR proposing any of them must
explain why the case is different from the documented refusal.

| Anti-pattern | Refusal in code |
|---|---|
| Verbosity integer | `transaction_by_id` returns typed `TxStatus`, no `verbose: u64` parameter |
| Verbose boolean | `block_header_by_selector` returns typed `BlockHeaderInfo`, no `verbose: bool` |
| String-keyed pool | `ShieldedProtocol` enum at every layer |
| Sentinel-overloaded `BlockId` | `BlockSelector` oneof; `BlockId` is return-only |
| External proto types on Rust API | `ChainIndex` trait takes / returns `zinder-core` types only |

## Worked examples

### Example 1 — `BlockHeaderResponse` (Primitive A)

A non-federated typed read with a new core type.

- Step 1: `wallet.proto` adds `BlockHeaderBySelectorRequest` (selector + chain_epoch_id) and `BlockHeaderResponse` (chain_epoch + BlockHeaderInfo).
- Step 2: `crates/zinder-core/src/block_artifact.rs` adds or extends the `BlockHeaderInfo` read-model struct backed by `BlockHeaderArtifact`.
- Step 3: `ChainIndex::block_header_by_selector(selector, at_epoch)` on the trait.
- Steps 4-5: `LocalChainIndex` resolves selector to height via the `block_hash_index` column family then reads `BlockHeaderArtifact`; `RemoteChainIndex` calls the gRPC method.
- Steps 6-8: `WalletQueryApi::block_header_by_selector`, native encoder, adapter handler.
- Step 9: capability `wallet.read.block_header_by_selector_v1`.
- Steps 10-11: capability_coverage row + capability-docs mirrors.
- Step 12: compat shim `GetTreeState` and `GetBlock` hash-only paths rewired through the resolver.
- Steps 13-14: integration test for valid selector + missing selector; no perf gate (point lookup).

### Example 2 — `TransactionStatusResponse` (Primitive B)

Wire envelope plus response enrichment. The path adds the `chain_epoch`-bound `MinedDetails::from_response_epoch` constructor (the entropy gate) and a `oneof status` discriminated wire shape (mined / in_mempool / conflicting). Capability `wallet.read.transaction_by_id_v1` advertises this shape.

### Example 3 — `TransparentAddressBalance` (Primitive D)

A federated derive-plane method. Adds the 14 baseline steps plus the 7 federation sub-steps:

- F1: `ExplorerQuery.TransparentAddressBalance` proto.
- F2-F4: `DeriveProxy<ExplorerQueryClient<...>>` field on `WalletQueryGrpcAdapter`, builder, ServerInfo capability gating.
- F5: `spawn_derive_readiness_probe` in `zinder-query` startup.
- F6: capabilities `explorer.server_info_v1` (probe target) and `explorer.transparent_address.balance_v1` (federated method).
- F7: compat shim `GetTaddressBalance` + per-address-loop `GetTaddressBalanceStream` over the federated path.

The compute shape is compute at read time: canonical confirmed totals are summed from transparent outputs, and the derive plane adds the live mempool overlay. An accumulator-backed read optimization may land later without changing the public wire shape or capability strings.

### Example 4 — `TransparentOutputsByOutpoint` and `TransparentMempoolOutputsByOutpoint`

A pair of canonical wallet-plane reads that resolve outpoints to their referenced outputs. Both share the new wire-level `OutPoint` message, the `TransparentOutput` payload, and a `repeated TransparentOutputEntry` response shape with `optional TransparentOutput prevout` per entry.

- The canonical method reads first-class `transparent_output` rows from `zinder-store`. Shape B canonical row lookup; pinned reads verify the row's producing-block identity against the requested epoch.
- The mempool method reads `MempoolEntry.transparent_outputs` through `MempoolIndex::transparent_outputs_by_outpoints`. Proxied through `IngestControl` because secondary readers cannot observe live writer state.
- Both surfaces share the same per-request cap (`MAX_TRANSPARENT_OUTPUTS_PER_REQUEST = 1024`) and reject the coinbase sentinel outpoint at the wallet adapter.
- Capability strings: `wallet.read.transparent_outputs_by_outpoint_v1` (canonical) and `wallet.mempool.transparent_outputs_by_outpoint_v1` (mempool).
- `ChainIndex` exposes three methods: `transparent_outputs_by_outpoint`, `transparent_outputs_by_outpoint_at_epoch`, `transparent_mempool_outputs_by_outpoint`.
- No compat-shim counterpart: `CompactTxStreamer` has no prevout endpoint, and the cookbook forbids inventing one.

The public wire shape and capability string do not expose the storage layout. Dedicated `transparent_output` rows are the canonical storage shape for mined outputs; the store also maintains a block-local index for bounded reorg repair. The response remains a list of per-request entries so consumers can decode canonical and mempool prevout resolution through the same path.

## Common mistakes

Each entry references the discipline gate that catches it.

- **Adding a public method without a capability string.** Fails capability-coverage; capability_docs both. Add the string in the same commit.
- **Adding a capability string without updating the docs mirrors.** Fails capability_docs. Update `public-interfaces.md` and `runbooks/testing.md`.
- **Putting a federated method under `wallet.*` instead of `derive.{consumer}.*`.** Fails capability-coverage assertions.
- **Computing `confirmations` or `block_time` from a re-read latest tip.** Violates the response enrichment rule. Use `MinedDetails::from_response_epoch` or analogous epoch-bound constructor.
- **Returning a `tonic::Status` or `zinder_proto::*` type from `ChainIndex`.** Violates the rule that the trait takes / returns only `zinder-core` types. Add a `from_status` mapping at the adapter boundary.

## Cross-references

- [Public interfaces](public-interfaces.md): the canonical naming, error vocabulary, capability discovery rules.
- [Wallet data plane](wallet-data-plane.md): owning venue for most read shapes.
- [Service boundaries](service-boundaries.md): which runtime owns what.
- [Extending artifacts](extending-artifacts.md): companion cookbook for new artifact families.
- [Derive plane](derive-plane.md): federation overview.
