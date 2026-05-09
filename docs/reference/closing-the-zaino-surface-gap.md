# Closing the Zaino Surface Gap

| Field | Value |
| ----- | ----- |
| Status | Background research |
| Audience | Zinder maintainers, contributors |
| Sources | Zinder HEAD as of 2026-05-09 (post-M3 mempool, post-M4 Slice A transparent UTXOs, post-M4 Slice B transparent tx history, post-M5 Slice A foundation); `zcash/wallet` HEAD captured 2026-04-07; `zingolabs/zaino` `15b81f1` (the rev pinned by `zcash/wallet`'s workspace `Cargo.toml`); the existing reference docs in this directory; the `Zallet,api-design,ECC/Z3-Request` issue cohort on `zingolabs/zaino`. |
| Related | [PRD-0001](../prd-0001-zinder-indexer.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Public interfaces](../architecture/public-interfaces.md), [Service operations](../architecture/service-operations.md), [Extending artifacts](../architecture/extending-artifacts.md), [ADR-0006](../adrs/0006-test-tiers-and-live-config.md), [ADR-0007](../adrs/0007-multi-process-storage-access.md), [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md), [ADR-0009](../adrs/0009-ingest-control-transport-security.md), [ADR-0010](../adrs/0010-mempool-topology-and-retention.md), [M5 spec](../specs/m5-transparent-address-balance.md), [Lessons from Zaino](lessons-from-zaino.md), [Serving Zebra and Zallet](serving-zebra-and-zallet.md), [Android wallet integration findings](android-wallet-integration-findings.md), [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) |
| Last refresh | 2026-05-09 (Phase 1+2 implementation): Primitive A (`BlockSelector` resolver) shipped, closing G2 and G4. New `block_hash_index` column family; new `BlockHeaderInfo` typed read model parsed from stored `BlockArtifact` via `zinder-source`; new `WalletQuery.BlockIdBySelector` and `WalletQuery.BlockHeaderBySelector` RPCs; new capability strings `wallet.read.block_id_by_selector_v1` and `wallet.read.block_header_by_selector_v1`; compat shim hash-only paths (`GetBlock`, `GetTreeState`, `GetTransaction`-by-block) rewired through the resolver. Primitive B (`TxStatus` wire envelope + `MinedDetails` enrichment) shipped, closing G3, G7, G13. `TxStatus` moved to `zinder-core`; `MinedDetails::from_response_epoch` is the only public constructor (entropy gate); `WalletQuery.Transaction` returns `TransactionStatusResponse` with oneof; capability `wallet.read.transaction_by_id_v1` evolves in place (no consumers shipped, so no bump). Default validation gate (`cargo check`/`clippy`/`fmt`/`doc`/`nextest ci`/`nextest ci-perf`) passes; 393 tests + 2 perf tests green. Earlier 2026-05-09 (P0 refresh): G12 marked closed by [Service operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder); G3 line range corrected; G8 mechanism refined; G13 added (`TxStatus` proto projection); G14–G21 added (eight Zallet frictions surfaced by a fresh `zcash/wallet` audit); new §Anti-Patterns Zinder Refuses to Replicate section; Decisions 2 and 4 collapsed into one wire-shape decision; Decision 9 (federation generic) added; matrix updated; `MempoolMinedEvent.block_hash` decision retained. Earlier 2026-05-09 pass: initial inventory after M3 mempool, M4 Slice A/B, M5 Slice A foundation. |

## Purpose

Zaino is the prior-art Zcash indexer that Zallet, Zashi, Zodl, the `zcash-android-wallet-sdk`, public lightwalletd-compatible clients, and several block explorers consume today. Per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md), Zinder's wallet data plane is consumer-neutral; each of those consumers exercises a different public contract over the same canonical artifacts. This document is the cross-consumer audit trail of the surfaces Zinder still needs to ship before any of those consumers can replace Zaino without a parity regression.

This document is not a contract and not an implementation plan. It cites file:line evidence on both sides of each gap, names the venue (architecture doc, ADR, spec, or unowned seam) that owns or should own the decision, and links to the [Lessons from Zaino](lessons-from-zaino.md) Pattern that informs the Zinder design response. New API shapes, type names, and method signatures are owned by the linked venues, not by this page.

The previous Zallet-side integration audit lives in [Serving Zebra and Zallet §Part B](serving-zebra-and-zallet.md#part-b-zinder-as-zallets-data-plane). That document is refreshed in the same change as this one to reflect post-M3/M4 implementation status; it remains the durable evidence trail for the Zebra-side and Zallet-side integration contracts. This document is the consumer-cross-cutting view: it organizes findings by gap, then projects per-consumer impact, so reviewers can prioritize work without reading four separate consumer documents.

## Method

Two parallel codebase audits were performed on 2026-05-09:

1. **`zcash/wallet`** (clone dated 2026-04-07; no commits to `zallet/src/` after that date in the local tree). Every file under `wallet/zallet/src/`, every TODO/FIXME/HACK comment that mentions Zaino, ChainIndex, or indexer behavior, every `chain.*` call site, every `zaino_*` import, the workspace `Cargo.toml`, and the test fixtures under `wallet/zallet/tests/cmd/`.
2. **`zfnd/zinder`** (post-`5e3f50d` on `main`). The native `WalletQuery` proto in `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`, the `ChainIndex` Rust trait in `crates/zinder-client/src/chain_index.rs`, the `WalletQueryApi` server trait in `services/zinder-query/src/lib.rs`, the lightwalletd compat adapter in `services/zinder-compat-lightwalletd/src/grpc.rs`, the capability constants in `crates/zinder-proto/src/capabilities.rs`, the `IngestControl` mempool methods in `services/zinder-ingest/src/ingest_control.rs`, the `RemoteChainIndex` and `LocalChainIndex` implementations in `crates/zinder-client/src/{remote,local}.rs`, the source-side broadcaster and mempool adapters in `crates/zinder-source/`, the `zinder-runtime` config schema, every `Status::unimplemented`, every `unreachable`, and every `todo!` site under `services/` and `crates/`.

Per-consumer evidence was cross-referenced against [Android wallet integration findings](android-wallet-integration-findings.md) for Zashi/Zodl, [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) for `zec.rocks`-style operators, and [M5 spec](../specs/m5-transparent-address-balance.md) for what the next derive-plane milestone closes. Citations point at file:line in the local trees. Issue numbers are linked at first use.

A Zaino call site is treated as a "gap" only if Zinder cannot serve the same observable behavior through the public surface a consumer would migrate to (`zinder-client::ChainIndex` for Rust consumers, native `WalletQuery` for typed gRPC consumers, `zinder-compat-lightwalletd::CompactTxStreamer` for lightwalletd-compatible consumers). Internal Zinder differences from Zaino (typed errors, secondary readers, multi-process topology) are by design per [ADR-0007](../adrs/0007-multi-process-storage-access.md) and [Public interfaces](../architecture/public-interfaces.md); they are not gaps.

## Gap Inventory

Each gap below cites the exact site that proves the gap, the consumers it blocks, the [Lessons from Zaino](lessons-from-zaino.md) Pattern that informs the Zinder design response, and the venue that owns or should own the decision.

### G1. Transparent-address balance is `Status::unimplemented`

**Evidence.** The lightwalletd-compat adapter explicitly rejects `GetTaddressBalance` and `GetTaddressBalanceStream` at `services/zinder-compat-lightwalletd/src/grpc.rs:399-415`:

```rust
async fn get_taddress_balance(...) -> Result<Response<lightwalletd::Balance>, Status> {
    Err(Status::unimplemented(
        "GetTaddressBalance is outside the transparent balance surface",
    ))
}
```

Native `WalletQuery` in `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` has no `TransparentAddressBalance` RPC. `ChainIndex` in `crates/zinder-client/src/chain_index.rs` has no `transparent_address_balance` method.

**Affected consumers.**

- Zashi/Zodl: blocking for transparent balance UI per [Android wallet integration findings](android-wallet-integration-findings.md).
- Public lightwalletd operators: every `zec.rocks`-style deployment claims `taddrSupport=true` and is expected to answer the call.
- Block explorers (`devdotbo/zcash-explorer`, others) paging balance per address.
- Not blocking for Zallet, which computes balances locally from the wallet DB; Zallet calls `get_address_utxos`, never `get_address_balance`.

**Owning venue.** [M5 spec](../specs/m5-transparent-address-balance.md). Slice A foundation (`services/zinder-derive` deployable, `ExplorerQuery.ServerInfo`, `DeriveStore`, capability `derive.explorer.ready_v1`) shipped 2026-05-08. Slice B (`TransparentBalanceAccumulator`, `ExplorerQuery.TransparentAddressBalance`, federated `WalletQuery.TransparentAddressBalance`, compat shim wiring, `ChainIndex::transparent_address_balance`, capability `derive.explorer.transparent_balance_v1`) is unstarted.

**Pattern.** [Pattern 6: Performance as a Sequential Implementation](lessons-from-zaino.md). Zaino's balance comes from Zebra's `ReadStateService::AddressBalance` per call (finalized state only, no mempool view). M5's accumulator path closes the Esplora-style `confirmed`/`unconfirmed` split that the lightwalletd `Balance { value_zat: int64 }` proto cannot carry; that split is what the gap exists to enable.

### G2. Hash-only block lookups ✓ closed by Primitive A

**Evidence.** `services/zinder-compat-lightwalletd/src/grpc.rs:758-772`:

```rust
fn block_height_from_id(block_id: &lightwalletd::BlockId) -> Result<BlockHeight, Status> {
    if !block_id.hash.is_empty() && block_id.height == 0 {
        return Err(Status::unimplemented(
            "hash-only block lookups are outside the indexed block lookup surface",
        ));
    }
    // ...
}
```

Native `WalletQuery.CompactBlock` in `wallet.proto:91-98` accepts `height` only. `ChainIndex::compact_block_at` in `chain_index.rs:364-374` takes `BlockHeight`, not a `BlockHash`-or-`BlockHeight` selector.

**Affected consumers.**

- Zallet: calls `chain.get_block(BlockId { height: 0, hash: ... })` at `wallet/zallet/src/components/json_rpc/methods/view_transaction.rs:944-948` to fetch the block in which a transaction was mined. Zallet already holds the height via `block_metadata(height)`, so the call site can be patched to pass the height; the call is not architecturally required from Zallet's side.
- Some lightwalletd Go-client paths exercise the hash form when their internal cache holds the hash but not the height.

**Owning venue.** Decision recorded in [Wallet data plane §Block Identity and Hash-Keyed Reads](../architecture/wallet-data-plane.md#block-identity-and-hash-keyed-reads). Zinder will add a canonical hash-to-height resolver for best-chain block identity and use it to serve hash-only compatibility reads. Height remains the primary key for compact-block range and single-block reads. Non-best-chain `(txid, block_hash)` lookup is deferred until explorer or zcashd-compat parity becomes an explicitly named milestone.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). The lightwalletd `BlockID { height, hash }` proto is overloaded; whether Zinder accepts the same wire shape and resolves on the read path, or rejects it and forces explicit height resolution, is a deliberate API decision that the spec system has not yet made.

### G3. `TransactionArtifact` consensus branch ID, block time, confirmations ✓ closed by Primitive B

**Evidence.** `crates/zinder-core/src/transaction.rs`:

```rust
pub struct TransactionArtifact {
    pub transaction_id: TransactionId,
    pub block_height: BlockHeight,
    pub block_hash: BlockHash,
    pub payload_bytes: Vec<u8>,
}
```

Native `Transaction` proto in `wallet.proto:119-132` carries the same four fields and no more. `TxStatus::Mined(TransactionArtifact)` exposes the artifact via `ChainIndex::transaction_by_id` in `chain_index.rs:107-123`. `TxStatus` itself has four arms (`Mined`, `InMempool(MempoolEntry)`, `NotFound`, `ConflictingChain`); only `Mined` has a wire projection today (see G13).

**Affected consumers.**

- Zallet: at `wallet/zallet/src/components/sync.rs:647-660,791-804` Zallet calls `chain.get_latest_block()` after every `get_raw_transaction` to compute `consensus::BranchId::for_height(params, parse_height)`, because Zaino's response does not carry the branch ID. Zallet also reaches `expect("Zaino's API should have caught this error for us")` at the parse site. This is `#237`-class friction, not a hard blocker.
- Zashi/Zodl: not an observed blocker; they decode the version header from the raw bytes.
- Block explorers exposing `time`, `confirmations`, and `branch_id` per transaction: blocking for the API parity claim.

**Owning venue.** Decision recorded in [Wallet data plane §Transaction Status and Enrichment](../architecture/wallet-data-plane.md#transaction-status-and-enrichment) and [Extending artifacts §Response enrichment is not an artifact family](../architecture/extending-artifacts.md#response-enrichment-is-not-an-artifact-family). `TransactionArtifact` stays lean. `consensus_branch_id`, `block_time`, and `confirmations` are response/read-model enrichment bound to the response's `ChainEpoch`, not new persisted transaction fields.

**Pattern.** [Pattern 2: RPC Surface With No Single Source of Truth](lessons-from-zaino.md). Zaino's `getrawtransaction(verbose=1)` returns enriched fields by routing to Zebra's JSON-RPC and returning a `zebra_rpc::methods::GetRawTransaction` enum; Zinder's typed `TransactionArtifact` chose forward-compat purity over field-by-field parity. Whether to enrich, where to compute (storage, read path, or response builder), and how to keep the store schema small are the design questions.

### G4. `get_block_header(verbose=true)` typed equivalent ✓ closed by Primitive A

**Evidence.** No `BlockHeader` artifact, no `BlockHeader` proto message, no `WalletQuery.BlockHeader[At]` RPC, no `ChainIndex::block_header_*` method. Zallet imports `zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader` at `wallet/zallet/src/commands/migrate_zcashd_wallet.rs:14` and calls `chain_subscriber.get_block_header(block_hash.to_string(), true).await?` at line 302 to determine whether a zcashd-wallet transaction's block is in the main chain.

**Affected consumers.**

- Zallet's `migrate-zcashd-wallet` command: blocking. The compact block payload's `time` field could substitute for the block time, but the verbose form also tells the caller whether the block is in the main chain (`confirmations >= 0`), which is the actual signal Zallet wants. Replacing the call with a `LatestBlock` plus chain-event subscription pair is a non-trivial migration on Zallet's side.
- Block explorers and operators that surface raw block-header bytes.

**Owning venue.** Decision recorded in [Wallet data plane §Block Identity and Hash-Keyed Reads](../architecture/wallet-data-plane.md#block-identity-and-hash-keyed-reads). Zinder will expose a typed block-header read model over the canonical hash-to-height resolver. The implementation may derive the header from retained block artifacts or promote a dedicated `BlockHeader` artifact if storage pressure or parser cost justifies it; either shape must keep Zinder's typed vocabulary and must not re-export Zebra or Zaino header response types.

**Pattern.** [Pattern 8: Upstream Node Coupling and Version Skew](lessons-from-zaino.md). The `zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader` import leaks Zebra's JSON-RPC response shape into Zallet's command code; the "verbose" boolean is Zebra's vocabulary, not Zinder's. Whatever Zinder offers in this slot must be a typed Zinder shape, not a re-export of `zebra-rpc`.

### G5. `getrawtransaction(txid, blockhash)` form is not supported

**Evidence.** Native `WalletQuery.Transaction` in `wallet.proto:110-117` accepts `transaction_id` only. Compat `GetTransaction` in `services/zinder-compat-lightwalletd/src/grpc.rs:294-318` accepts `TxFilter.hash` (txid) or `TxFilter.block` (height plus index within block), but not the `(txid, blockhash)` shape. Zallet acknowledges the gap at `wallet/zallet/src/components/json_rpc/methods/get_raw_transaction.rs:486-491`:

```rust
// TODO: We can't support this via the current Zaino API; wait for `ChainIndex`.
//       https://github.com/zcash/wallet/issues/237
if blockhash.is_some() {
    return Err(LegacyCode::InvalidParameter.with_static(
        "blockhash argument must be unset (for now).",
    ));
}
```

**Affected consumers.**

- Zallet's `getrawtransaction` JSON-RPC method: rejected at the wallet boundary today; Zallet's own callers cannot pass the `blockhash` argument.
- zcashd-compat clients that pin transaction lookup to a specific block hash for non-best-chain reads.

**Owning venue.** Decision recorded in [Wallet data plane §Block Identity and Hash-Keyed Reads](../architecture/wallet-data-plane.md#block-identity-and-hash-keyed-reads). The wallet data-plane baseline is canonical-best-chain plus live mempool transaction status. `(txid, block_hash)` non-best-chain lookup is deferred until explorer or zcashd-compat parity is a named milestone because it changes retention, secondary-reader, and reorg semantics.

**Pattern.** [Pattern 5: Reorg and Chain-Edge Correctness](lessons-from-zaino.md). The `blockhash` form is a non-best-chain query; Zaino's lack of side-chain access is the same root cause as `zingolabs/zaino #10305`. Zinder's chain-event log retains finalized history through `reorg_window_blocks`, but transaction artifacts are best-chain only. Whether non-best-chain transaction lookup belongs in the wallet data plane or the chain-event log is a deliberate split that has not been made.

### G6. Transparent-mempool surfaces are Rust-trait-only on the gRPC boundary

**Evidence.** `ChainIndex::transparent_mempool_outputs_by_address` and `ChainIndex::transparent_mempool_spend_by_outpoint` are defined in `crates/zinder-client/src/chain_index.rs:507-521`. Native `WalletQuery` in `wallet.proto:752-803` exposes neither. `RemoteChainIndex::transparent_mempool_outputs_by_address` in `crates/zinder-client/src/remote.rs:344-362` implements the typed contract by paging `MempoolSnapshot` and filtering client-side; `transparent_mempool_spend_by_outpoint` does the same at `remote.rs:365-385`.

**Affected consumers.**

- Zashi/Zodl over the gRPC boundary: blocking for unmined transparent UTXO overlays without paging the full mempool.
- Block explorers exposing pending transaction state per address.
- Not blocking for Zallet when it migrates: `LocalChainIndex` and `RemoteChainIndex` both expose the Rust trait. The gRPC asymmetry only affects non-Rust clients.

**Owning venue.** Decision recorded in [Wallet data plane §Mempool Point Lookups](../architecture/wallet-data-plane.md#mempool-point-lookups). `MempoolSnapshot` remains the bounded scan and bootstrap surface, but it is not the long-term answer for point lookups. Native gRPC will mirror focused transparent mempool output and spend lookups so non-Rust clients do not page the full mempool to answer per-address or per-outpoint questions.

**Pattern.** [Pattern 7: Configuration as a God Object](lessons-from-zaino.md). The Rust trait surface and the gRPC surface diverging carries the same risk class as configuration drift: a "ChainIndex method that turned out not to be a WalletQuery method" is an asymmetry that compounds into per-consumer feature flags.

### G7. `IsInMempool` typed status oneof ✓ closed by Primitive B

**Evidence.** `ChainIndex::is_in_mempool` is at `crates/zinder-client/src/chain_index.rs:471`. `WalletQuery` in `wallet.proto:752-803` has no `IsInMempool` RPC. `RemoteChainIndex::is_in_mempool` at `remote.rs:283-291` defers to `transaction_by_id` and matches on `TxStatus::InMempool`; the underlying gRPC call is `WalletQuery.Transaction(TransactionRequest)`, which on its own does not return mempool transactions because `Transaction { block_height, block_hash, payload_bytes }` is mined-only.

**Affected consumers.**

- Zallet `#403` (periodic transaction rebroadcast): the rebroadcast loop wants a low-cost presence check. Rust callers get this via `is_in_mempool(txid)`. Non-Rust clients that need rebroadcast logic must call `MempoolSnapshot` and search.
- Block explorers that surface a pending state badge per transaction.

**Owning venue.** Decision recorded in [Wallet data plane §Transaction Status and Enrichment](../architecture/wallet-data-plane.md#transaction-status-and-enrichment). The primary native gRPC shape is a typed transaction-status response, not a standalone boolean. A Rust-side `is_in_mempool(txid)` convenience can remain, but the wire contract should expose a `TxStatus`-style oneof so clients receive mined, mempool, not-found, or conflicting-chain state in one call.

**Pattern.** Same as G6: [Pattern 7](lessons-from-zaino.md).

### G8. `GetMempoolTx.poolTypes` filter is parsed but ignored

**Evidence.** `services/zinder-compat-lightwalletd/src/grpc.rs:419-458`:

```rust
async fn get_mempool_tx(
    &self,
    request: Request<lightwalletd::GetMempoolTxRequest>,
) -> Result<Response<Self::GetMempoolTxStream>, Status> {
    // ...
    let exclude_txid_suffixes = request.into_inner().exclude_txid_suffixes;
    // poolTypes is silently dropped.
```

The shape of the gap is "field silently dropped," not "filter broken." The supporting machinery is fully implemented: `pool_selection_from_request` at `grpc.rs:709-733` correctly maps `PoolType::Sapling/Orchard/Transparent` to a `CompactBlockPoolSelection`, and `prune_compact_block` at `grpc.rs:670-699` correctly applies that selection. Both are wired into `get_block_range`. The defect is that `get_mempool_tx` extracts only `exclude_txid_suffixes` from `request.into_inner()` and never calls `pool_selection_from_request` or `prune_compact_block`. The proto-comment contract at `service.proto:262` ("the server must prune CompactTxs") is therefore unmet on the mempool path while it is met on the block-range path.

**Affected consumers.**

- Lightwalletd-compat clients that filter by pool type to skip transparent-only mempool transactions during a shielded scan. None in production today; this is a forward-compatibility risk if a future SDK begins to send the field.

**Owning venue.** Open seam, low priority. Whether to honor `poolTypes` on the mempool path or to document the field as accepted-but-ignored under the lightwalletd-compat contract is a small decision that should be made explicitly rather than left as silent drift.

**Pattern.** [Pattern 6: Performance as a Sequential Implementation](lessons-from-zaino.md). Filter implementations that materialize the full result and then truncate are the same shape as Zaino's `GetBlockRange` regression; Zinder's pruning happens in the encoder, but the read path still pays the per-tx decode for shielded-only filtering.

### G9. `GetLightdInfo` returns several empty or zero fields

**Evidence.** `services/zinder-compat-lightwalletd/src/grpc.rs:568-582` populates `consensus_branch_id` from `NetworkUpgrade::current()` but leaves `zcashd_build`, `git_commit`, `donation_address`, `upgrade_name`, and `upgrade_height` as empty strings or zero.

**Affected consumers.**

- Public lightwalletd operators per [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) who surface these fields in operator UIs.
- Compatibility tests that compare `LightdInfo` field-for-field with `lightwalletd-go`.

**Owning venue.** Open seam. The lightwalletd-side gaps are about consumer-facing strings, not policy-bearing fields. Closing them mostly requires deciding which fields belong on `LightdInfo` (compat surface) versus `ServerCapabilities` (native surface).

**Pattern.** [Pattern 2: RPC Surface With No Single Source of Truth](lessons-from-zaino.md). `LightdInfo` is upstream lightwalletd's capability descriptor; `ServerCapabilities` is Zinder's. The two overlap; Zinder's deliberate stance (per [Public interfaces](../architecture/public-interfaces.md)) is that the canonical descriptor is the native one and `LightdInfo` is a compat surface whose obligations stop at "the fields a real lightwalletd Go server populates."

### G10. The wallet data plane has no transport authentication in v1

**Evidence.** [ADR-0009](../adrs/0009-ingest-control-transport-security.md) records the bearer-token contract for the ingest-control plane only. `WalletQuery` and `CompactTxStreamer` are unauthenticated on the public listen address. The `zinder-query` and `zinder-compat-lightwalletd` binaries do not register an auth interceptor on the public wallet endpoints.

**Affected consumers.**

- Any operator who wants to terminate TLS, run rate-limiting, or enforce per-tenant quotas on the wallet endpoint. Today this is documented as a reverse-proxy responsibility at [Service operations §Deployment guidance](../architecture/service-operations.md#deployment-guidance).
- Multi-tenant operators (e.g. wallet-as-a-service providers) who would want per-tenant tokens on the wallet endpoint.

**Owning venue.** [ADR-0009](../adrs/0009-ingest-control-transport-security.md) explicitly punts on the public wallet plane; [Service operations](../architecture/service-operations.md#deployment-guidance) names the operator's responsibility. This is a closed-but-deferred decision: v1 stops at the bearer-token boundary for ingest control. Whether to extend the auth contract or to pin the operator-proxy stance is a v2 question.

**Pattern.** [Pattern 3: Status, Health, and Lifecycle as After-Thoughts](lessons-from-zaino.md). Zaino did not have an authenticated gRPC port either; the operator UX risk is identical. Zinder's deliberate stance is documented; the gap is that "documented" and "implemented" remain different states until a policy is named.

### G11. No automated zaino-parity certification suite

**Evidence.** The word "zaino" appears in non-test code at `crates/zinder-client/src/chain_index.rs:105`, `crates/zinder-testkit/src/transparent_signer.rs:132`, `crates/zinder-source/src/json_rpc_mempool.rs:47`, and `crates/zinder-source/tests/live/zebra_json_rpc.rs:183`. These are comments. The `*_parity.rs` tests at `crates/zinder-client/tests/integration/transparent_address_*_parity.rs` assert `LocalChainIndex` against `RemoteChainIndex` parity, not Zinder against Zaino. [Android wallet integration findings](android-wallet-integration-findings.md) is observational evidence from a real Zashi build against a real Zinder deployment, but it is a refresh-on-test cadence, not an automated CI gate.

**Affected consumers.** Anyone trying to certify "Zinder serves consumer X without regression versus Zaino." Today there is no harness for that claim. The `zec.rocks`-style operator deployment recipe in [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) and the Android findings carry observational evidence; neither is a release gate.

**Owning venue.** [ADR-0006: Test tiers and unified live-test config](../adrs/0006-test-tiers-and-live-config.md) provides the live-test scaffolding (`ZINDER_TEST_LIVE=1`, `--profile=ci-live`) but does not name a Zaino-parity profile. Whether to add a "consumer release certification" tier that runs Zashi, Zallet, or `lightwalletd-go testclient` against a Zinder build is a release-engineering decision.

**Pattern.** [Pattern 9: Test and Dev Surface Brittleness](lessons-from-zaino.md). Zaino's CI was self-hosted and gated on org membership; Zinder's is open but currently lacks the consumer-release certification suite this gap demands.

### G12. Operator recipe for running Zallet against Zinder ✓ closed

**Closed by** [Service operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder). The 2026-05-09 P0 refresh confirmed the anchor exists at `service-operations.md:346-385` with two topology options, readiness expectations, transport guidance, and `LocalChainIndex` colocated-optimization caveats.

**Historical evidence (kept as anchor).** Before the recipe landed, [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md) named the deployment recipe for `zec.rocks`-style operators but no equivalent existed for Zallet+Zinder. The two topology options that the service-operations doc now documents follow from [ADR-0007 §Multi-process storage access](../adrs/0007-multi-process-storage-access.md):

- **Separate-process gRPC reader (the recommended baseline).** Zallet embeds `zinder-client::RemoteChainIndex` and connects to a separately-run `zinder-query`. The operator runs three processes (`zinder-ingest`, `zinder-query`, Zallet); no store path is shared.
- **In-process secondary reader (advanced colocated optimization).** Zallet embeds `zinder-client::LocalChainIndex` (RocksDB secondary; periodic catchup interval in `LocalOpenOptions`). The operator runs `zinder-ingest tip-follow` separately; Zallet shares the store path. Subscriptions still require a service endpoint, so `zinder-query` or `zinder-ingest`'s ingest-control plane is also needed for chain and mempool events.

**Affected consumers (resolved).** Zallet operators have a documented production recipe; Zashi mobile users who run a self-hosted backend follow the same recipe.

**Pattern.** [Pattern 10: Observability as a Discussion, Not a Contract](lessons-from-zaino.md). Zaino's bundling-into-Zallet sidestepped operator UX entirely; Zinder's separation made operator UX a first-class concern, and the recipe captures that with metrics, readiness signals, transport guidance, and rollback steps.

### G13. `TxStatus::ConflictingChain` and `TxStatus::InMempool` proto projection ✓ closed by Primitive B

**Evidence.** `crates/zinder-client/src/chain_index.rs:107-123` defines `TxStatus` with four arms (`Mined(TransactionArtifact)`, `NotFound`, `InMempool(MempoolEntry)`, `ConflictingChain`). Native `WalletQuery.Transaction` in `wallet.proto:110-117` returns `TransactionResponse { Transaction transaction }` only; `Transaction` is the mined-only artifact (`transaction_id`, `block_height`, `block_hash`, `payload_bytes`). `NotFound` maps to gRPC `NOT_FOUND` by convention; `InMempool` and `ConflictingChain` have no typed wire projection. Non-Rust callers cannot distinguish "in mempool" from "not indexed" from "indexed in a non-best-chain branch" without parsing untyped error codes.

**Affected consumers.**

- Every non-Rust consumer of native `WalletQuery`. Rust consumers see the typed enum through `ChainIndex::transaction_by_id`; gRPC consumers see only the `Mined` arm.
- Block explorers and operator UIs that need to distinguish mempool vs. conflicted vs. not-indexed transactions.
- Zashi/Zodl when migrating from Zaino's `getrawtransaction(verbose=1)` enrichment shape.

**Owning venue.** Decision recorded in [Wallet data plane §Transaction Status and Enrichment](../architecture/wallet-data-plane.md#transaction-status-and-enrichment). The native gRPC surface should mirror the Rust `TxStatus` oneof. This is the same wire change as G3 and G7; collapsing the three into one PR is the design (see Decisions §1).

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). The Rust domain type has four arms; the wire type has one. Closing this gap is what makes the domain and wire shapes match.

### G14. Untyped error vocabulary forces consumers to string-match

**Evidence.** `wallet/zallet/src/components/sync.rs:617-628` and `sync.rs:677-688`:

```rust
match err {
    FetchServiceError::RpcError(rpc_err)
        if rpc_err.message.contains("No such mempool") => {
        // Treat as "transaction not in mempool"
    }
    // ...
}
```

Zaino's `FetchServiceError::RpcError` carries a freeform message string. To distinguish "transaction not in mempool" from a genuine error, Zallet matches on the error message text at two sites in `sync.rs`. A change in Zaino's error string would silently break Zallet without a type-system signal.

Zinder's `IndexerError` is closer to a typed enum, but the public-API contract for "transaction not present in mempool" vs. "indexer error" is implicit and not exhaustively asserted. The gap exists to make the rule explicit and CI-enforced.

**Affected consumers.**

- Zallet today (Zaino-side).
- Any future Rust consumer migrating from Zaino — friction disappears under exhaustively-documented `IndexerError` variants.

**Owning venue.** [Public interfaces §Error vocabulary](../architecture/public-interfaces.md) (extension). Audit `IndexerError` for explicit "valid not-found" semantics, document the rule that callers must never inspect error message strings, and add a CI assertion that scans new Rust consumers for `.contains(` patterns on error messages.

**Pattern.** [Pattern 2: RPC Surface With No Single Source of Truth](lessons-from-zaino.md). String-match-on-message is a symptom of an error type with insufficient variants.

### G15. Tip query returns untyped height; consumers must `try_into().expect`

**Evidence.** `wallet/zallet/src/components/sync.rs:659,803` and `wallet/zallet/src/components/json_rpc/methods/get_raw_transaction.rs:537`:

```rust
let height = chain
    .get_latest_block()
    .await?
    .height
    .try_into()
    .expect("BlockHeight fits in u32");
```

Zaino's `get_latest_block` returns a proto whose `height` field is `i64`/`u64`. Zallet must coerce to `u32` and panic on overflow at three call sites.

Zinder's `ChainIndex::latest_block` returns `BlockId { height: BlockHeight, hash: BlockHash }` (typed). The gap exists to assert "no `try_into().expect` on tip-query consumers" as a CI test in any new Rust consumer crate.

**Affected consumers.**

- Zallet today (Zaino-side).
- Any Rust consumer migrating from Zaino — friction disappears under typed `BlockHeight`.

**Owning venue.** Cross-check with [Wallet data plane §Tip query](../architecture/wallet-data-plane.md). The Zinder shape is correct; this gap exists to document the contrast and to assert the absence of the Zaino pattern in new code.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). Wire shapes leaking `i64`/`u64` are the root cause of consumer-side coercion panics.

### G16. Subtree roots returned as hex strings, not typed bytes

**Evidence.** `wallet/zallet/src/components/sync/steps.rs:108-115,131-138`:

```rust
hex::decode_to_slice(&subtree.root, &mut subtree_root)
    .map_err(|err| FetchServiceError::RpcError(RpcError::new_from_legacycode(/* ... */)))?;
```

Zaino's `z_get_subtrees_by_index` returns each subtree root as a hex-encoded string (Zebra-RPC convention pass-through). Zallet hex-decodes the string into a `[u8; 32]` and synthesizes `RpcError` for decode failures. The hex round trip is wasted CPU and a category error.

The Zinder native surface should return `[u8; 32]` (or a typed `SubtreeRoot([u8; 32])` newtype), never a hex string. Verify this is the case at every layer of the existing `subtree_roots_in_range_v1` capability.

**Affected consumers.**

- Zallet today (Zaino-side).
- Any Rust consumer that builds wallet-scanning logic.

**Owning venue.** [Wallet data plane §Subtree roots](../architecture/wallet-data-plane.md) (extension). Audit Zinder's `subtree_roots_in_range_v1` for typed-bytes return at every layer.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). Hex strings on the wire are a Zebra-RPC convention leak; the canonical Zinder shape is bytes.

### G17. Tree-state access requires `.fetcher` private-field bypass

**Evidence.** `wallet/zallet/src/components/json_rpc/methods/get_new_account.rs:61` and `recover_accounts.rs:91`:

```rust
chain.fetcher.get_treestate(height.to_string()).await?
```

`FetchServiceSubscriber` does not expose `get_treestate` in its public surface; the method lives on the inner `FetchService` (the `.fetcher` field). Zallet bypasses the subscriber abstraction by accessing the private field at two call sites.

Zinder's `ChainIndex` already has `tree_state_at(BlockHeight)` and `tree_state_at_epoch(BlockHeight, ChainEpoch)`. The gap exists to confirm both `LocalChainIndex` and `RemoteChainIndex` expose them through the trait, document the migration sketch for Zallet, and use the contrast as a teaching example for "no private-field bypasses."

**Affected consumers.**

- Zallet today (Zaino-side).
- Any consumer using a similarly-incomplete trait surface.

**Owning venue.** [Wallet data plane §Tree state](../architecture/wallet-data-plane.md). Confirm `tree_state_at` is on the trait, exposed by both implementations, and add a migration note for Zallet.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). When the public trait omits a method that callers need, the method either gets re-implemented poorly or callers reach into private fields. Both are vocabulary leaks.

### G18. No prevout-resolution surface

**Evidence.** `wallet/zallet/src/components/json_rpc/methods/view_transaction.rs:443-477`. To compute the input value of a transparent input, Zallet calls `chain.get_raw_transaction(prev_txid, Some(1))` once per input, fetching the entire previous transaction just to extract one `TxOut` from its `vout` array. For an N-input transaction this is N round trips with no batching.

Neither Zaino nor Zinder exposes a "given this `OutPoint`, return the `TxOut`" call. There is no single-`OutPoint` lookup and no batched prevout-resolution surface.

**Affected consumers.**

- Block explorers, accounting tools, and any UI that displays per-input values for transparent transactions.
- Zashi/Zodl over the gRPC boundary — Zallet has a wallet-DB cache that hides this friction; mobile clients without a local DB feel it sharply.

**Owning venue.** Spec gap; new spec required at `docs/specs/m6-prevout-resolution.md`. Batching strategy (single-`OutPoint` lookup vs. multi-`OutPoint` request) is non-trivial because the storage shape needs to support efficient prevout-by-outpoint reads across the reorg window.

**Pattern.** [Pattern 6: Performance as a Sequential Implementation](lessons-from-zaino.md). N+1 queries for a known-bounded fan-out is the textbook anti-pattern.

### G19. Broadcast accepts hex string instead of bytes

**Evidence.** `wallet/zallet/src/components/json_rpc/payments.rs:478`:

```rust
chain.send_raw_transaction(raw_transaction_hex).await?
```

Zallet serializes the transaction to bytes, hex-encodes those bytes, and passes them as a string to Zaino's `send_raw_transaction`. Zaino in turn decodes the hex string back into bytes before broadcasting. The hex round trip is pure friction — the same verbosity-as-string anti-pattern that drives G3 in the read direction.

Zinder's `WalletQuery.SendTransaction` takes `bytes` natively. Verify the typed-bytes shape is consistent at every boundary (`ChainIndex::broadcast_transaction`, `WalletQueryApi::broadcast_transaction`, `WalletQuery.SendTransaction`).

**Affected consumers.**

- Zallet today (Zaino-side).
- Any Rust consumer migrating from Zaino.

**Owning venue.** [Wallet data plane §Transaction Broadcast](../architecture/wallet-data-plane.md#transaction-broadcast). Confirm typed-bytes shape end-to-end.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). Hex-as-string is the same vocabulary leak as G16 on a write path.

### G20. Tip-change push event is poll-by-stream-closure

**Evidence.** `wallet/zallet/src/components/sync.rs:114-116`:

```rust
let _ = chain.get_mempool_stream(...).await?;
```

Zallet detects new tips by waiting for the mempool stream to close. This works because Zaino closes the mempool stream on tip change, but it conflates "chain advanced" with "stream closed for any reason" (operator-induced disconnect, server restart, network blip). False positives waste a wallet sync cycle; false negatives lose tip notifications entirely.

Zinder already has `ChainEvent::TipAdvanced` as a typed signal in `ChainEventEnvelope`. The gap exists to document the migration sketch for Zallet's `#136`/`#159` work and to assert that consumers must never need a stream-closure heuristic for chain progression.

**Affected consumers.**

- Zallet today (Zaino-side; addressed by Zallet's planned M2 migration to `chain_events`).
- Any consumer migrating from Zaino's mempool-stream-closure pattern.

**Owning venue.** [Wallet data plane §Chain-Event Subscription](../architecture/wallet-data-plane.md#chain-event-subscription). Confirm the contract is documented and add a migration note.

**Pattern.** [Pattern 3: Status, Health, and Lifecycle as After-Thoughts](lessons-from-zaino.md). When tip-change has no first-class event, consumers reverse-engineer it from unrelated stream lifecycles.

### G21. String-keyed pool discriminants

**Evidence.** `wallet/zallet/src/components/sync/steps.rs:104,127`:

```rust
chain.z_get_subtrees_by_index("sapling", ...).await?;
chain.z_get_subtrees_by_index("orchard", ...).await?;
```

Zaino accepts pool names as strings (Zebra-RPC convention pass-through). A typo like `"saplng"` compiles, fails at runtime against the upstream node, and surfaces as an opaque error string at the Zinder boundary.

Zinder's typed shape should be a `Pool` enum with `Sapling` and `Orchard` variants. The wire form may stay string-typed for lightwalletd compat (the lightwalletd proto carries strings), but the typed Rust API and the native proto take the enum.

**Affected consumers.**

- Zallet today (Zaino-side).
- Any Rust consumer migrating from Zaino.

**Owning venue.** [Wallet data plane §Subtree roots](../architecture/wallet-data-plane.md) (extension). Add a typed `Pool` enum to `zinder-core` and audit every "pool" consumer.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). String-keyed discriminants are vocabulary leaks; typed enums catch typos at compile time.

## Anti-Patterns Zinder Refuses to Replicate

Five Zaino patterns are documented here as deliberate non-features. Each is anchored to a `wallet/zallet/` site that demonstrates the friction it causes today and a [Lessons from Zaino](lessons-from-zaino.md) Pattern that explains the underlying design pressure. PRs that propose adding any of these shapes to Zinder must cite the matching row and explain why this case is different from the documented refusal.

### A1. The verbosity integer

**The anti-pattern.** A single read endpoint accepts a `verbosity: Option<u64>` argument where `0` means "raw bytes" and `1` means "verbose object," producing different response shapes from one method.

**Where it bites today.** `wallet/zallet/src/components/sync.rs:607-660` and `view_transaction.rs:443-477`. Every Zallet caller of `get_raw_transaction(txid, Some(1))` immediately matches on the response enum and panics on the `Raw(_)` variant that should never appear. Two `unreachable!()` at `sync.rs:611,643` and one `expect("Zaino's API should have caught this error for us")` at `sync.rs:671` come from this shape.

**Zinder's refusal.** The typed surface is `transaction_by_id(txid) -> TxStatus` returning a typed shape. There is no verbosity flag. If a different shape is needed (e.g. raw bytes), it is a different method (`raw_transaction_by_id` or similar), not a flag.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). One method serving two response shapes is the textbook case of a type compromised by two consumer needs.

### A2. The "verbose" boolean on header and block calls

**The anti-pattern.** Methods like `get_block_header(hash, verbose: bool)` and `z_get_block(selector, verbosity: Option<u64>)` encode the response format as a runtime flag rather than as a method name.

**Where it bites today.** `wallet/zallet/src/commands/migrate_zcashd_wallet.rs:14,302`. The `zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader` import (the cautionary tale called out in G4 review risk #3) crosses Zaino's public boundary because the trait surface omitted a typed-shape return for the `verbose=true` form. `wallet/zallet/src/components/sync/steps.rs:228` (annotated `#[allow(dead_code)]`) bypasses this by passing `Some(0)` and deserializing raw bytes — a workaround Zallet preserved but does not exercise in production.

**Zinder's refusal.** Block-header reads return a typed `BlockHeaderInfo` shape. Compact-block reads return `CompactBlock`. Full-block reads (when added) return a typed `BlockArtifact`-derived shape. Three different methods, three different return types, no boolean.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). Same as A1.

### A3. String-keyed pool discriminants

**The anti-pattern.** API surfaces accept pool names as strings (`"sapling"`, `"orchard"`). Zebra's JSON-RPC inherited this from `zcashd`; Zaino passed it through.

**Where it bites today.** `wallet/zallet/src/components/sync/steps.rs:104,127`. Same evidence as G21; the anti-pattern is the design choice that *creates* G21.

**Zinder's refusal.** Typed `Pool` enum (`Sapling`, `Orchard`, future variants) at the Rust trait and at the native proto. Lightwalletd-compat shim keeps the string form because the upstream proto requires it, but the compat shim translates to the enum at the boundary.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). String discriminants miss compile-time errors that enums catch.

### A4. Sentinel-overloaded `BlockId { height: 0, hash: bytes }`

**The anti-pattern.** A single `BlockId` proto message with both `height` and `hash` fields, where `height == 0` is a sentinel for "ignore height, use hash." This conflicts with genesis (height 0 is a valid height) and forces every consumer to know the sentinel rule.

**Where it bites today.** `wallet/zallet/src/components/sync/steps.rs:80,245`, `view_transaction.rs:944-957`, `migrate_zcashd_wallet.rs:186`. Four call sites construct `BlockId { height: 0, hash: vec![...] }` manually, with the height-zero-as-sentinel rule encoded as comments rather than as types.

**Zinder's refusal.** Typed `BlockSelector` oneof (`Height(BlockHeight) | Hash(BlockHash)`) at every layer. Genesis is `BlockSelector::Height(BlockHeight::ZERO)`, which is unambiguous. `BlockId` remains the *return* shape for resolved identities; it is never a request shape with sentinel fields.

**Pattern.** [Pattern 5: Reorg and Chain-Edge Correctness](lessons-from-zaino.md). Sentinel-overloaded fields are the same shape mistake as Zaino's `coinbase_height` optionality misuse (`#679`) — a field carrying two meanings.

### A5. `zaino_proto::*` types on the Rust API surface

**The anti-pattern.** A Rust trait surface that takes or returns lightwalletd-proto generated types (`GetAddressUtxosArg`, `BlockId`, `TreeState` from `zaino_proto::proto::service::*`).

**Where it bites today.** `wallet/zallet/src/components/sync.rs:50,532`, `steps.rs:80,245`, `view_transaction.rs:10`, `rosetta.rs:11`, `migrate_zcashd_wallet.rs:186`. Eleven of Zallet's thirteen `use zaino_*` imports are internal Zaino types that crossed Zaino's public boundary because the trait surface delegated to them directly.

**Zinder's refusal.** The `ChainIndex` trait accepts and returns `zinder-core` types only (`BlockHeight`, `BlockId`, `BlockSelector`, `TransparentAddressScriptHash`, `TransparentOutPoint`, `TxStatus`, etc.). The proto layer is an implementation detail of `RemoteChainIndex` and `WalletQueryGrpcAdapter`. Generated `zinder_proto::v1::wallet::*` types appear in adapter modules, never on the public Rust API.

**Pattern.** [Pattern 1: Three Domains, One Type Pile](lessons-from-zaino.md). The anti-pattern is the *cause* of nine of the eleven Zaino-internal imports; refusing it makes the imports unnecessary.

## Per-Consumer Impact Matrix

The same gap lands differently per consumer. This matrix is the prioritization input.

| Gap | Zallet | Zashi/Zodl | Public lightwalletd | Block explorers |
| --- | --- | --- | --- | --- |
| G1: transparent-address balance | not blocking | blocking | blocking | blocking |
| G2: hash-only block lookups | ✓ closed | ✓ closed | ✓ closed | ✓ closed |
| G3: `TransactionArtifact` enrichment | ✓ closed | ✓ closed | not exposed in lightwalletd proto | ✓ closed |
| G4: `get_block_header` equivalent | ✓ closed | ✓ closed | not applicable | ✓ closed |
| G5: `getrawtransaction(txid, blockhash)` | rejected at Zallet boundary | not blocking | some clients | some explorers |
| G6: transparent-mempool gRPC RPCs | not blocking (Rust trait covers) | blocking | not exposed in lightwalletd proto | blocking |
| G7: `IsInMempool` standalone RPC | ✓ closed | ✓ closed | not exposed in lightwalletd proto | ✓ closed |
| G8: `GetMempoolTx.poolTypes` filter | not applicable | not blocking | some clients (forward compat) | not applicable |
| G9: `GetLightdInfo` empty fields | not applicable | cosmetic | cosmetic | cosmetic |
| G10: wallet plane authentication | operator concern | operator concern | blocking for multi-tenant | operator concern |
| G11: zaino-parity certification suite | required for swap confidence | required for swap confidence | required for swap confidence | required for swap confidence |
| G12: Zallet+Zinder operator recipe | ✓ closed | not applicable | not applicable | not applicable |
| G13: `TxStatus` proto projection | ✓ closed | ✓ closed | not exposed in lightwalletd proto | ✓ closed |
| G14: typed error vocabulary | closes on migration | not applicable on Rust trait | not applicable | not applicable |
| G15: typed tip height | closes on migration | not applicable on Rust trait | not applicable | not applicable |
| G16: subtree-root bytes | closes on migration | not applicable on Rust trait | not applicable | not applicable |
| G17: tree-state on `ChainIndex` | closes on migration | not applicable on Rust trait | not applicable | not applicable |
| G18: prevout-resolution surface | workaround (per-tx fetch) | blocking | not applicable | blocking |
| G19: broadcast typed-bytes | closes on migration | not applicable on Rust trait | not applicable | not applicable |
| G20: tip-change push event | closes on migration | uses separate signal | not applicable | not applicable |
| G21: typed pool enum | closes on migration | not applicable on Rust trait | not applicable | not applicable |

"Not blocking" means the consumer has either a typed Rust path (G6, G7, G13 for Rust callers), a fix on the consumer side (G2: pass height already held; G3: decode raw bytes; G5: Zallet already rejects the form), or no observed need (G1 for Zallet because balance is wallet-DB-local). "Cosmetic" means the field is observable but does not gate a product flow. "Closes on migration" means the gap is a Zaino-side friction; Zinder's typed surface already has the right shape, so adopting Zinder closes the gap automatically. "✓ closed" rows are kept as historical anchors and removed from the matrix in the next refresh.

## Decisions Surfaced by This Research

These decisions surfaced from the gap inventory. Items marked "Decision" are the planning baseline in the linked architecture doc; "Open" items remain unresolved before the next milestone planning round; "✓ shipped" items closed as the cited milestone landed.

1. **Decision: typed `BlockSelector` resolver (G2, G4, G5-best-chain half).** Add a canonical best-chain hash-to-height resolver expressed as a `#[non_exhaustive]` `BlockSelector` enum (`Height(BlockHeight) | Hash(BlockHash)`). Compat hash-only reads call the resolver before any height-keyed read; native clients use the same resolver when they hold a hash but not the height. Compact-block range reads stay height-first. The block-header read model sits on top of the same resolver. Defer non-best-chain `(txid, block_hash)` lookup until explorer or zcashd-compat parity is an explicitly named milestone; non-best-chain lookup is a *different* method, not a third selector arm.
2. **Decision: typed transaction-status wire envelope with epoch-bound enrichment (G3, G7, G13, partly G5).** Replace mined-only `WalletQuery.TransactionResponse` with `TransactionStatusResponse { ChainEpoch chain_epoch; oneof status { MinedTransaction mined; MempoolTransaction in_mempool; ConflictingChainTransaction conflicting } }`. The mined arm carries `MinedDetails { consensus_branch_id, block_time, confirmations }` constructed only via `MinedDetails::from_response_epoch(epoch, mined_height, network_upgrades, block_artifact)` so all three values bind to the response's `ChainEpoch`. `NotFound` maps to gRPC `NOT_FOUND`. Capability identity stays `wallet.read.transaction_by_id_v1`; with no consumers shipped, the wire shape evolves in place. Capability versioning becomes a deprecation mechanism only after a consumer ships, at which point a wire-shape change would bump to `_v2` and retain `_v1` in `deprecated_capabilities` for the documented overlap window. G3 (enrichment), G7 (standalone IsInMempool), and G13 (TxStatus proto projection) are *one* wire change, not three.
3. **Decision: block-header read model (G4).** Expose a typed Zinder-shaped block-header read model over the `BlockSelector` resolver. Implementation may derive from existing `BlockArtifact` payloads or promote a dedicated `BlockHeader` artifact if storage pressure or parser cost justifies it; either shape stays internal. The public name is `BlockHeaderResponse` / `block_header_by_selector`, never `BlockHeaderArtifact` exported to callers.
4. **Decision: native gRPC mirroring of `ChainIndex` mempool point lookups (G6).** Promote focused mempool point lookups (`TransparentMempoolOutputsByAddress`, `TransparentMempoolSpendByOutpoint`) to native gRPC instead of requiring non-Rust clients to scan `MempoolSnapshot`. The capability namespace is `wallet.mempool.*` (a new subdomain), not `wallet.address.*`, because the storage tier and lifecycle differ from canonical address-keyed reads.
5. **Open: wallet plane authentication posture (G10).** [ADR-0009](../adrs/0009-ingest-control-transport-security.md) explicitly defers the public wallet plane to the operator's reverse proxy. A v1.x or v2 ADR may revisit this; the open question is whether multi-tenant wallet hosting is in scope.
6. **Open: consumer-release certification tier (G11).** Whether [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) gains a "consumer release" tier that runs Zashi, Zallet, or `lightwalletd-go testclient` against a Zinder build is a release-engineering decision.
7. **✓ shipped: Zallet-with-Zinder operator recipe (G12).** Separate-process `RemoteChainIndex` over `zinder-query` is the default Zallet deployment. Documented in [Service operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder). `LocalChainIndex` is documented as an advanced colocated optimization, not the baseline recipe.
8. **Decision: `MempoolEvent.Mined.block_hash` enrichment (cross-cuts G6, G7, G11).** Add the mined block hash to the mined mempool event so lifecycle consumers do not follow up with a racy tip read after receiving `Mined`.
9. **Decision: federation generic for derive-plane proxying (G1, future M6+ derive consumers).** Land `WalletQueryGrpcAdapter::proxy_to_derive::<Req, Resp>(method, request)` in M5 Slice B even though `TransparentAddressBalance` is the only consumer. Federated `WalletQuery.*` methods that proxy to `ExplorerQuery.*` advertise capability strings under the `derive.*` namespace, never `wallet.*`, and a CI assertion enforces the namespace rule. This is the entropy gate against M5+M6+M7 each shipping a copy-pasted proxy body.
10. **Decision: machine-readable gap tags on `Status::unimplemented` sites (cross-cuts G2, G3, G7, the entire compat shim).** Each `Status::unimplemented` site in `services/zinder-compat-lightwalletd/src/grpc.rs` carries a `/// gap: G{N}` doc comment immediately above the method. A test in `services/zinder-compat-lightwalletd/tests/integration/` walks the source and asserts every site has a matching gap tag. Without this, the gap inventory drifts from reality silently between refreshes (Review Risk #1).

## Review Risks

Concrete anti-patterns this research surfaces. PR review should pause and link the relevant risk number when any of these appear.

1. **Adding `Status::unimplemented` to a method without a `/// gap: G{N}` doc comment.** Every `Status::unimplemented` in `services/zinder-compat-lightwalletd/src/grpc.rs` carries a machine-readable gap tag per Decision 10. The CI assertion in `services/zinder-compat-lightwalletd/tests/integration/` walks the source and fails the build if a site lacks a tag or references a gap row that is not in this document. Manual cross-reference in this document remains the human-readable companion. Drift recreates [Pattern 2](lessons-from-zaino.md).
2. **Adding a `ChainIndex` method without a corresponding `WalletQuery` RPC, or vice versa, without an explicit per-method note in the doc that owns the asymmetry.** G6 and G7 are the existing examples; new ones should be deliberate.
3. **Re-exporting a `zaino-*` type, a `zebra-*` type, or a `zcash_client_backend::proto::*` type from a public Zinder API.** The `zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader` import in Zallet's `migrate_zcashd_wallet.rs:14` is the cautionary tale: zaino-internal types crossed Zaino's public boundary because the trait surface omitted them. Zinder must not repeat this; see also [Pattern 8](lessons-from-zaino.md).
4. **Implementing a balance, history, or aggregation surface in `zinder-store` instead of `zinder-derive`.** [Pattern 4](lessons-from-zaino.md) and [Extending artifacts §When to add an artifact family](../architecture/extending-artifacts.md#when-to-add-an-artifact-family) name this risk; M5 is the deliberate response. A balance or history accumulator landing in `zinder-store` reopens this anti-pattern.
5. **Synthesizing an enrichment field server-side without binding it to the response's `chain_epoch`.** A `confirmations` value computed from `tip_height - block_height` is racy unless it pins to one epoch; G3 carries this risk.
6. **Skipping the "Affected consumers" cell in the gap matrix when adding a new entry.** Per-consumer impact is the prioritization input; an unowned entry has no priority.
7. **Documenting a gap as "operator's problem" without a cross-link to [Service operations](../architecture/service-operations.md#deployment-guidance).** G10 and G12 are the existing examples; new ones should also resolve to a deployment-doc anchor.
8. **Closing a gap by silently widening a public type without bumping the capability string.** Capability strings are exact-match per [Public interfaces §Capability discovery](../architecture/public-interfaces.md#capability-discovery); silent widening recreates the version-pinning class of [Pattern 2](lessons-from-zaino.md).

## Migration-Side Pressures Zinder Does Not Own

For completeness, the following pressures show up in Zallet's source but are owned by `zcash/wallet`, not Zinder. They are listed so that, when Zallet files an issue against Zinder citing one of these, this document is the cross-reference that names the correct owner.

- The bundled in-process Zaino spawn at `wallet/zallet/src/components/chain.rs:113-117` is replaced by `zinder-client::LocalChainIndex::open` or `RemoteChainIndex::connect`. The `IndexerSection` config block in `wallet/zallet/src/config.rs:457-498` and the `Network::to_zaino` adapter at `wallet/zallet/src/network.rs:59-82` are zaino-shaped today and become zinder-shaped on Zallet's side.
- `wallet/zallet/src/components/sync.rs:114-116` polls for tip changes by waiting for the mempool stream to close. The replacement is `ChainIndex::chain_events`. This is Zallet's `#136`/`#159` work, not Zinder's.
- The `try_into().expect("u32")` cluster at `wallet/zallet/src/components/sync.rs:660,673,748,804,817` and the string-match error checks at `wallet/zallet/src/components/sync.rs:617,677,794` are Zallet-side workarounds against Zaino's untyped surface. Both vanish when Zallet adopts `BlockHeight` and `IndexerError`/`TxStatus` from `zinder-core` and `zinder-client`.
- The in-memory `MemoryCache` rebuild on every restart at `wallet/zallet/src/components/sync/cache.rs:18-113` exists because Zaino blocks startup on cache fill. Zinder's secondary catchup unblocks reads continuously; the cache deletes on Zallet's side once the migration lands.
- The `chain.fetcher.get_treestate(string)` private-field bypass at `wallet/zallet/src/components/json_rpc/methods/get_new_account.rs:61` and `recover_accounts.rs:91` is a workaround against Zaino's trait surface. The replacement is `ChainIndex::tree_state_at(BlockHeight)` and `tree_state_at_epoch`.

These are Zallet-side migration costs, not Zinder gaps. They are listed here so that issues filed against Zinder citing them can be redirected to the zcash/wallet tracker without losing the context.

## How To Use This Document

Read this document when:

- Triaging a new Zaino-replacement issue filed against Zinder. Find the closest gap, link the Pattern, and route the work to the owning venue (ADR, spec, or open seam).
- Reviewing a PR that adds, removes, or changes a `WalletQuery` RPC, a `ChainIndex` method, a compat shim entry, or a `Status::unimplemented` site. Cross-reference the gap inventory and the review risks.
- Planning a milestone. The per-consumer impact matrix is the prioritization input; the open decisions list is the ADR backlog.

Refresh this document when:

- A milestone ships that closes one or more gaps. Update the affected gap rows, change "open seam" to "closed by [link]," remove the row from the matrix when fully closed, and bump the `Last refresh` line with the milestone identifier.
- A new consumer is named in [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md). Add a column to the per-consumer impact matrix.
- Zaino itself ships a behavior change that creates a new parity expectation. Add a gap entry, cite the upstream commit, and link the new behavior.
- A gap is reframed from "open seam" to a closed decision in an ADR. The gap row stays as a historical anchor; the row notes the ADR that resolved it.

This document does not replace the per-consumer reference docs ([Android wallet integration findings](android-wallet-integration-findings.md), [Serving public lightwalletd clients](serving-public-lightwalletd-clients.md), [Serving Zebra and Zallet](serving-zebra-and-zallet.md)). Those carry the consumer-specific evidence and observed behavior; this page carries the cross-consumer gap inventory and the prioritization input that keeps all four documents coherent.

## Closing Note

Zaino was the only practical Zcash indexer for several years; every consumer in the ecosystem encoded its surface implicitly. Zinder's job is to make that surface explicit, then close every gap deliberately, with the consumer-neutral framing of [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md) as the design backbone. Each gap above is a discrete piece of that work, owned by a named venue or surfaced as an open decision.
