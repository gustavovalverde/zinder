# Serving Zebra and Zallet

| Field | Value |
|-------|-------|
| Status | Background research |
| Audience | Zinder maintainers, contributors |
| Sources | Local Zebra, Zallet, Zaino, lightwalletd, Android SDK, and Zodl source trees as of 2026-04-29; GitHub issues for Zebra and Zallet |
| Related | [PRD-0001](../prd-0001-zinder-indexer.md), [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md), [Lessons from Zaino](lessons-from-zaino.md), [Wallet Data Plane](../architecture/wallet-data-plane.md), [Chain Ingestion](../architecture/chain-ingestion.md), [Chain events](../architecture/chain-events.md) |
| Last refresh | 2026-04-29: M3 mempool consumer roles re-checked against local Zebra, Zallet, Zaino, lightwalletd, Android SDK, and Zodl source trees. |

## Purpose

[Lessons from Zaino](lessons-from-zaino.md) captures prior-art lessons from Zaino's public tracker. This document covers what Zinder must do *for the codebases on either side of it*.

Zinder sits between the upstream node and downstream wallet consumers:

- **Zebra** is the ZFND upstream Zcash node behind Zinder's primary `NodeSource`. Zebra has shipped substantial new infrastructure for indexer consumers in the last 12 months: a streaming gRPC indexer API, dedicated `/healthy` and `/ready` probes, OpenRPC capability discovery, an `indexer` feature flag that changes the canonical DB format, and `AnyChainBlock` requests for non-best-chain access. Zaino was largely written before any of this existed; Zinder must be designed *around* it.
- **Zallet** is the full-node Zcash wallet in `zcash/wallet` and Zinder's primary native Rust downstream consumer. Zallet currently bundles Zaino in-process through `IndexerService::<FetchService>`. Its source code contains a concrete inventory of indexer capabilities Zallet still needs, expressed as `TODO` comments naming the missing capability. The Zaino tracker also contains many `Zallet,api-design,ECC/Z3-Request` issues filed by ECC engineers. Zallet's planned integration target is the `ChainIndex` trait. Zinder can provide that interface for the product guarantees it owns.

This document captures what each side has stated, implied, or implemented as a contract; where Zinder needs different guarantees from the current indexer boundary; and how Zinder's existing decisions already serve those guarantees or still need to.

The broader ecosystem product map for M3 mempool lives in
[Wallet data plane §Mempool Snapshot and Subscription](../architecture/wallet-data-plane.md#mempool-snapshot-and-subscription-m3).
This page remains the evidence trail for the Zebra/Zallet part of that map; it
does not duplicate the Android/Zodl or explorer contract tables.

## Method

Two parallel codebase explorations were performed. For Zebra: every indexer-facing surface (`zebra-rpc`, `zebra-grpc`, `zebra-state` request types, `zebra-node-services`, `zebrad/components/health.rs`), the changelog from v2.0 through v4.3.1, and the GitHub issue tracker for indexer-related issues. For Zallet: every call site that touches the indexer (`zallet/src/components/chain.rs`, `zallet/src/components/sync.rs`, `zallet/src/components/sync/steps.rs`, `zallet/src/components/json_rpc/methods/*.rs`, `zallet/src/components/json_rpc/payments.rs`, `zallet/src/commands/migrate_zcashd_wallet.rs`), every TODO/FIXME mentioning Zaino, and the wallet's own issue tracker.

Citations point at concrete file paths or issue numbers. Where prose is summarised across multiple sources, the supporting evidence is enumerated in line.

# Part A: Zinder as a Zebra Consumer

## What Zebra Now Exposes

Zebra's indexer-facing surface as of v4.3.1 is broader and more push-oriented than the Zaino architecture assumed.

- **Streaming gRPC indexer API** at `rpc.indexer_listen_addr` (compile with `--features indexer`). Three server-streaming methods, defined in `zebra-rpc/proto/indexer.proto` and implemented in `zebra-rpc/src/indexer/methods.rs`:
  - `ChainTipChange` emits best-chain tip transitions.
  - `NonFinalizedStateChange` emits full block bytes for every block entering any non-finalized chain (best or side).
  - `MempoolChange` emits typed transitions: `ADDED`, `INVALIDATED`, `MINED`.
- **`ReadStateService` request set**, in `zebra-state/src/request.rs`. The relevant requests for an indexer:
  - `Block(HashOrHeight)` (best chain only) and `AnyChainBlock(HashOrHeight)` (any chain, added v4.2.0 in response to Zaino issues `#9541` and `#10305`).
  - `Transaction`, `AnyChainTransaction`, `AnyChainTransactionIdsForBlock` (the last includes an `is_best_chain` flag).
  - `SpendingTransactionId(Spend)` for nullifier-and-outpoint to spending-tx lookups, gated behind `#[cfg(feature = "indexer")]`.
  - `AddressBalance`, `TransactionIdsByAddresses`, `UtxosByAddresses`.
  - `SaplingTree`, `OrchardTree`, `SaplingSubtrees`, `OrchardSubtrees`.
  - `IsTransparentOutputSpent(OutPoint)`.
  - `NonFinalizedBlocksListener` for the gRPC stream's internal channel.
- **JSON-RPC** for everything else, with cookie auth on by default and OpenRPC capability discovery at the `rpc.discover` method (added v4.2.0).
- **Health probes** at `/healthy` and `/ready` on a dedicated port (Zebra `[health]` config, added v2.4.0 in issue `#8830`). `/ready` is gated on block lag and tip age.
- **Prometheus metrics**: `rpc.requests.total`, `rpc.request.duration_seconds`, `rpc.active_requests`, plus `state.*`, `sync.*`, `peer.*`, `checkpoint.*`, `zcash.chain.*`.

Zebra's `AGENTS.md` (line 59) explicitly: _"Features outside Zebra's scope (wallets, block explorers, mining pools — these belong in Zaino, Zallet, or librustzcash)."_ Zebra is intentionally narrow. Zinder is the named recipient of the work.

## How Zinder Should Consume Zebra

The seven decisions below come directly from Zebra's tracker and the gRPC implementation, not from generic best practice.

### 1. Use `NonFinalizedStateChange` as the primary block ingestion path, with a durable cursor and gap-recovery

The push stream is the intended primary contract. Issue `#8610` (closed, the original tracking issue for Zebra's indexer support) makes this explicit. Polling Zebra's JSON-RPC for `getbestblockhash` or `getblock` is the wrong default.

The non-obvious constraint: the stream has *no replay on reconnect*. The receiver in `zebra-state/src/response.rs:282-292` is consumed once. The internal mpsc channel in `zebra-rpc/src/indexer/methods.rs:21` is bounded at `RESPONSE_BUFFER_SIZE = 4_000`. If Zinder's consumer falls behind, the send fails and the gRPC task drops. If Zebra restarts, the stream terminates. On reconnect, Zinder gets blocks from the *current* non-finalized state forward; nothing fills the gap.

Zinder's mitigation must be a durable last-processed-hash cursor, paired with an `AnyChainBlock` walk on reconnect to fetch any hashes Zinder has not yet committed. This belongs in `zinder-source` (the Zebra adapter), not in `zinder-ingest`'s state machine. The state machine should see a continuous, gap-free observation stream.

### 2. Treat `AnyChainBlock` as the canonical fetch primitive, not `Block`

`ReadRequest::Block` returns only best-chain blocks. Once a non-finalized side-chain block falls off the listener, `Block` cannot retrieve it. This is the Zaino bug class behind `#10305`: side-chain blocks vanished, Zaino served stale data. Zinder must pair every block hash observation with an `AnyChainBlock` fetch and a separate is-best-chain tag refreshed from `AnyChainTransactionIdsForBlock`. When the best chain reorganises, retag; do not re-fetch.

### 3. Gate Zinder readiness on Zebra readiness

Zebra's `/ready` returns 200 only when block lag is `≤ ready_max_blocks_behind` (default 2) and tip age is `≤ ready_max_tip_age` (default 5 minutes). Zinder's own readiness signal must not contradict Zebra's. If Zebra is `not_ready/syncing`, Zinder is at most `not_ready/node_unavailable` regardless of how many epochs Zinder has materialised locally. The typed readiness causes named in [RFC-0001 §Operations Model](../rfcs/0001-service-oriented-indexer-architecture.md) already enumerate this; the implementation must consume Zebra's probe, not just expose Zinder's own.

### 4. Refuse to start if Zebra's DB does not have the indexer feature when Zinder needs it

Zebra's `--features indexer` flag is not a runtime toggle. It changes the on-disk DB format version: `zebra-state/src/constants.rs:74-77` appends `+indexer` build metadata to the semver. It also adds three nullifier-to-`TransactionLocation` column families (`book/src/dev/state-db-upgrades.md:354-365`). A Zebra started with the flag and restarted without it will trigger a migration that drops these column families. A Zinder that depends on `SpendingTransactionId` will then panic.

Zinder's startup phase `connect_node` (named in [Service Operations](../architecture/service-operations.md)) should probe Zebra's reported version and stay out of `ready` if the indexer feature is missing and any Zinder-served API needs it. This is operator UX work: the operator risk today is silent misconfiguration.

### 5. Add Zinder-side auth on the gRPC indexer port

Zebra's gRPC indexer endpoint has no authentication. The JSON-RPC server has cookie auth by default (`book/src/user/lightwalletd.md:49`); the gRPC port does not. Anyone with network access to the indexer port can subscribe to all chain and mempool events.

This is Zebra's gap, not Zinder's, but Zinder is the most likely co-located service. Zinder's deployment guide must require Zebra's gRPC port to be reachable only on localhost or a private network, and Zinder's documentation must call this out as a security-relevant assumption. The closed issue `#10405` (RPC method access groups, planned but unbuilt) signals that future Zebra auth will be method-group-scoped; Zinder's source adapter should be structured so per-method tokens can be slotted in.

### 6. Discover capabilities, do not version-pin

Zebra exposes `rpc.discover` (OpenRPC, v4.2.0+) and `getblockchaininfo` (which includes upgrade activation heights). These are the canonical capability surfaces. Zaino's tracker includes several "Support Zebra X.Y" issues (`#1034`, `#926`, `#816`, `#561`); Zinder should avoid a per-release dependency cycle by probing capabilities directly.

`zinder-source` should call `rpc.discover` on connect, parse the method list, fail loud if a required method is absent, and warn (not fail) if extra methods are present. Activation heights come from Zebra, not Zinder constants. This is the Zaino `#743` lesson made operational.

### 7. Parallel, not duplicated, observability

Zebra emits `rpc.requests.total`, `rpc.request.duration_seconds`, `rpc.active_requests` per method. Zinder should emit equivalents on its own surfaces (`zinder.api.requests.total`, `zinder.api.request.duration_seconds`) so an operator with one Prometheus dashboard can correlate Zebra-side latency with Zinder-side latency. The `state.*` and `sync.*` Zebra metrics already cover upstream-node state; Zinder should not re-collect them; it should reference them in dashboards.

## Remaining Zebra Integration Constraints

These are the gaps Zinder will have to live with, with concrete mitigations Zinder owns.

| Gap | Evidence | Zinder mitigation |
|-----|----------|-------------------|
| `NonFinalizedStateChange` has no replay on reconnect | `zebra-rpc/src/indexer/methods.rs:21`, `zebra-state/src/response.rs:282-292` | Durable cursor + `AnyChainBlock` gap fill in `zinder-source` |
| No side-chain tip enumeration | Issue `#9541` partially closed; only `AnyChainBlock` fix shipped | Track non-finalized chain heads in `zinder-ingest` from observed block hashes; do not trust Zebra to enumerate them |
| No auth on gRPC indexer port | `zebra-rpc/src/indexer/server.rs` | Operational: localhost-only deployment, network policy enforced by docs |
| Indexer feature flag changes DB format silently | `zebra-state/src/constants.rs:74-77` | Startup probe + typed `schema_mismatch` readiness state |
| `getaddresstxids` had correctness bugs | Issue `#9742` (closed) | Snapshot tests against mainnet data, not parity claims |
| `MAX_BLOCK_REORG_HEIGHT = 99` is a Zebra constant | `zebra-state/src/constants.rs:31` | Zinder's `ReorgWindow` must be configurable but bounded by Zebra's; query Zebra at startup |

## NU7, Crosslink, and Schema Forward-Compatibility

`NetworkUpgrade::Nu7` is defined in `zebra-chain/src/parameters/network_upgrade.rs:61-63` with no activation height set on mainnet or testnet (`network_upgrade.rs:110`, branch ID placeholder `0xffffffff`). Transaction V6 is gated behind `zcash_unstable=nu7`. The fee logic already branches on `Nu7` (`zebra-rpc/src/methods/types/get_block_template/zip317.rs:179`).

For Zinder, this is a forward-compatibility constraint. The Zaino tracker's `#1007` ("zaino assumes 100 blocks") and `#1006` ("zaino assumes genesis is committed") came from Crosslink workshop testing. Zinder's storage shape must absorb Nu7 without a reindex:

- `Spend` enum variants stored as open-ended tagged unions, not fixed-size arrays. New nullifier types (e.g. unified-pool nullifiers) become a new variant with a new tag byte; old variants do not move.
- Activation heights are upstream-node-supplied, never compiled in.
- Transaction artifact storage carries a version tag so a Nu7-format V6 transaction is distinguishable from a V5 with the same wire bytes.

This is consistent with [ADR-0002](../adrs/0002-boundary-specific-serialization.md) (envelope header carries schema version) but the artifact-internal versioning is not yet documented; it should be added to the [Storage Backend](../architecture/storage-backend.md) doc when artifact families are finalised.

# Part B: Zinder as Zallet's Data Plane

## Current Zallet Indexer Boundary

Zallet does not speak gRPC to Zaino. It bundles Zaino in-process. The wiring is:

```
Zallet  --[Rust trait calls]-->  zaino_state::FetchServiceSubscriber
                                   --[JSON-RPC]-->  Zebra
```

`zallet/src/components/chain.rs:113-117` constructs `IndexerService::<FetchService>::spawn(config)` and exposes `FetchServiceSubscriber` (Zaino's `LightWalletIndexer + ZcashIndexer` traits) as the chain handle. Zallet calls Rust methods on it. Zaino, in turn, calls Zebra's JSON-RPC.

This means **the surface Zallet cares about is Zinder's Rust API, not its gRPC API**. The gRPC and JSON-RPC surfaces matter for external consumers (mobile wallets, explorers, third-party tools); for Zallet specifically, the contract is in-process.

The concrete call set Zallet makes today (cited from Zallet source, paths relative to `zallet/src/`, line numbers verified against `main` on 2026-04-28):

| Zallet need | Source | Zaino method |
|-------------|--------|--------------|
| Latest tip polling | `components/sync/steps.rs:63`, `components/sync.rs:659,803`, `commands/migrate_zcashd_wallet.rs:158` | `get_latest_block` |
| Compact block by id | `components/sync/steps.rs:64,86,251` | `get_block(BlockId)` |
| Mempool stream (also tip-change signal) | `components/sync.rs:388` | `get_mempool_stream` |
| Subtree roots | `components/sync/steps.rs:104,127` | `z_get_subtrees_by_index` |
| Treestate by height | `components/sync/steps.rs:308`, `components/json_rpc/methods/get_new_account.rs:61`, `components/json_rpc/methods/recover_accounts.rs:91` | `z_get_treestate`, `get_treestate` |
| Treestate by `BlockId` | `commands/migrate_zcashd_wallet.rs:186` | `get_tree_state` |
| Transparent UTXOs | `components/sync.rs:532,742` | `z_get_address_utxos`, `get_address_utxos` |
| Address tx ids | `components/sync.rs:756` | `get_address_tx_ids` |
| Raw tx with metadata | `components/sync.rs:607,639,766`, `components/json_rpc/methods/view_transaction.rs:446`, `components/json_rpc/methods/get_raw_transaction.rs:494` | `get_raw_transaction(verbose=1)` |
| Chain height | `components/sync.rs:711` | `chain_height` |
| Block header (for block time) | `commands/migrate_zcashd_wallet.rs:302` | `get_block_header(verbose=true)` |
| Broadcast | `components/json_rpc/payments.rs:478` | `send_raw_transaction` |
| Block by hash | `components/sync/steps.rs:223` | `z_get_block` (currently dead path) |

Zallet's wallet keys never leave Zallet. The indexer never sees viewing keys, and `scan_cached_blocks` runs locally in `zcash_client_backend`. This is exactly the privacy boundary [Wallet Data Plane](../architecture/wallet-data-plane.md) and [PRD-0001 Out of Scope](../prd-0001-zinder-indexer.md) commit Zinder to: no server-side scanning, no viewing-key custody.

## Zallet Integration Constraints in the Current API

The Zallet source code is unusually explicit. The eleven items below are not inferred; each is a `TODO`/`FIXME` or workaround comment in Zallet that names an indexer capability Zallet needs. Paths are relative to `zallet/src/`.

1. **Chain-tip push notification.** Zallet detects new blocks by waiting for the mempool stream to close (`components/sync.rs:388,407`). The TODO at `components/sync.rs:114-116` is explicit: _"TODO: Zaino should provide us an API that allows us to be notified when the chain tip changes."_ Issue `#159` is the Zallet-side integration request. A `Notify` at `components/sync.rs:115-119` propagates the inferred signal to other tasks.
2. **Typed error responses.** Multiple call sites in the `data_requests` task match `e.message.contains("No such mempool")` because the current error surface does not expose that case as a typed contract. Zallet `components/sync.rs:617,677` are the load-bearing examples; the same pattern recurs at `components/sync.rs:794`.
3. **Untyped Rust API for transaction fetch.** `get_raw_transaction(..., Some(1))` returns `GetRawTransaction::Object`; Zallet has multiple `unreachable!()` guards for the `::Raw(_)` case (`components/sync.rs:608,640,767`, `components/json_rpc/methods/get_raw_transaction.rs:494`): _"Zaino should have a Rust API for fetching tx details, instead of requiring us to specify a verbosity and then deal with an enum variant that should never occur."_
4. **Consensus branch ID in transaction data.** Zallet roundtrips back to `get_latest_block()` and computes `BranchId::for_height` because the current indexer response does not return the branch ID with the transaction (`components/sync.rs:647-660`, `components/sync.rs:791-804`).
5. **Block heights as untyped `u64`.** Repeated `.try_into().expect("TODO: Zaino's API should have caught this error for us")` at `components/sync.rs:660,673,748,804,817`. Zinder's typed `BlockHeight` value would eliminate the panics without exposing parser-crate types at the public API boundary.
6. **Atomic / snapshot semantics.** `components/sync.rs:703`: _"we're making the *large* assumption that the chain data doesn't update between the multiple chain calls."_ `components/json_rpc/methods/view_transaction.rs:944`: _"Once Zaino updates its API to support atomic queries…"_ This is the single most consequential integration constraint.
7. **Mempool UTXO index.** `components/sync.rs:572`: _"Once Zaino has an index over the mempool, monitor it for changes to the unmined UTXO set."_ This affects zero-conf transparent chains (`#139`) and rebroadcast (`#403`).
8. **Chaininfo-aware transparent UTXO reads.** `components/sync.rs:528-532`: TOCTOU-prone, no fixed-height variant. The TODO is explicit: _"I really want to use the chaininfo-aware version (which Zaino doesn't expose)."_
9. **Zaino's block cache disabled.** Zallet rebuilds a process-local `MemoryCache` (`components/sync/cache.rs:18-113`) on every restart because Zaino's startup synchronously fills its own cache and blocks indefinitely (`zaino #249`).
10. **`getrawtransaction` `blockhash` parameter unsupported.** `components/json_rpc/methods/get_raw_transaction.rs` falls back to verbosity-without-blockhash because the Zaino API does not accept the third argument: _"We can't support this via the current Zaino API; wait for `ChainIndex`."_
11. **Block-not-in-best-chain handling.** Zallet `#222`: the current path propagates Zebra's `"block height not in best chain"` and Zallet's sync task panics. Root cause: non-finalized state held a height Zebra had not finalised, so Zallet observed a phantom commitment.

The pattern: every Zallet integration constraint is either (a) an untyped Rust surface, (b) missing snapshot semantics, or (c) missing push notification. Zinder addresses all three at the architecture level.

## What Zallet Has Asked For (Issue Tracker)

Eleven issues from `zcash/wallet`'s tracker that name Zinder-shaped requirements:

- **`#237`: Migrate `chain_view` to `ChainIndex` trait.** The single most direct mapping: Zallet has decided its long-term indexer interface is the `ChainIndex` trait. Zinder's Rust API can *be* that trait. This is the central decision point.
- **`#222`: block-height-not-in-best-chain crash.** Zinder must not expose heights uncommitted by the upstream node.
- **`#126`: tree insertion conflict during sync.** Caused by racing block deliveries from Zaino. Zinder must guarantee monotone, non-duplicated block delivery to in-process consumers.
- **`#159`: `initialize` should behave like `steady_state`.** Currently uses scan-range tricks to detect reorgs. A push notification plus header-traversal API removes the workaround.
- **`#180`: global lock when not in sync.** Needs a typed "distance from tip" or "finalized height" signal. Zinder's `ChainEpoch` already includes both.
- **`#403`: periodic transaction rebroadcast.** Needs queryable mempool presence.
- **`#167`: `broadcast_transactions` partial-success.** Needs structured broadcast errors, not raw RPC strings.
- **`#136`: steady_state loops on task failure.** Caused by ambiguous stream termination. Zinder's stream contract must distinguish "stream ended because new block" from "stream ended because error".
- **`#179`: `rescan_from_last_finalized_block`.** Needs a stable finalized-height anchor with retention guarantee.
- **`#62`: `listsinceblock`.** Needs point-in-time block metadata queries (epoch-bound).
- **`#349`, `#348`: PCZT, `z_spendoutputs`.** New payment RPCs. Need fetch UTXOs by outpoint, not just by address.

## What Zinder Owes Zallet

These are the concrete API contracts Zallet's source code and tracker imply. None of them are speculative; each ties to a cited workaround or open issue.

### Atomic chain snapshots (the `ChainEpoch` bet)

Every "chaininfo-aware" TODO in Zallet, the entire `#237` migration, and `components/json_rpc/methods/view_transaction.rs:944` depend on this single capability: take a snapshot of chain state at one height-and-hash, and issue multiple queries against it consistently.

[PRD-0001 Implementation Decisions](../prd-0001-zinder-indexer.md) commits to this: _"Query responses that require chain consistency must read from one epoch. Mixing latest values from different epochs is a correctness bug."_ The `ChainEpoch` type is the canonical mechanism. The Rust API surface Zallet consumes must expose epoch-pinned readers, not method-by-method calls against an implicitly-current chain.

### Typed Rust API: heights, errors, transaction status

Zallet's seven `try_into().expect(...)` calls and three string-matching error checks both vanish if Zinder's Rust API uses:

- `zinder_core::BlockHeight` for every block height, never `u64` or `i64`.
- Typed `TxStatus { Mined { height, hash, time }, InMempool, NotFound, ConflictingChain }` for transaction lookup.
- Typed `TransactionBroadcastResult { Accepted { txid }, Rejected, Duplicate { txid }, InvalidEncoding, Unknown }` for broadcast.
- Typed `IndexerError` enums where Zaino currently returns RPC error strings.

This is not a wire-protocol decision; it is a Rust-API decision in `zinder-query` (or a `zinder-client` companion crate). The gRPC and JSON-RPC surfaces stay protocol-pinned; the Rust client is what Zallet imports.

### Chain notifications, separated from mempool streams

Zallet's `#136`, `#159`, and the `Notify` workaround at `components/sync.rs:115-119` all come from conflating "new block" with "stream closed". Zinder's [Chain events](../architecture/chain-events.md) vocabulary distinguishes `ChainSourceEvent`, `ChainEvent`, and `ChainEventEnvelope`; the wallet data plane should expose at least:

- A chain-event subscription consumers can resume from a cursor. Closing the stream means the consumer disconnected, never "a new block arrived." [Wallet data plane §Chain-Event Subscription](../architecture/wallet-data-plane.md#chain-event-subscription) defines this as `WalletQuery.ChainEvents`.
- A separate M3 mempool subscription with typed `MempoolChange { Added | Invalidated | Mined }` events (mirroring Zebra's `MempoolChangeKind`).

This is also the right granularity for Zallet's `#403` (rebroadcast detection) because `MempoolChange::Invalidated` carries the eviction reason. The existing `ChainEvent::ChainReorged { reverted, committed }` shape (in `chain-events.md`) collapses Zallet's N-round-trip `find_fork` walk at `components/sync/steps.rs:156-187` into a single envelope: receive event, `db_data.truncate_to_height(reverted.from_height - 1)`, resume scan. The 10-block safety margin at `components/sync.rs:265` becomes unnecessary.

### Mempool as a queryable index

The Zallet `#139`, `#403`, and `sync.rs:467` cluster all need mempool *queries*, not just *streams*. Zinder must back the mempool with an indexed view (epoch-bound or sequence-numbered, per [RFC-0001 §Mempool Model](../rfcs/0001-service-oriented-indexer-architecture.md)) that supports at minimum:

- `is_in_mempool(txid) -> bool`
- `transparent_mempool_outputs_by_address(request) -> Vec<TransparentMempoolOutput>`
- `transparent_mempool_spend_by_outpoint(outpoint) -> Option<TransparentMempoolSpend>`

Insert-only mempool caches are explicitly forbidden by RFC-0001; the design rationale is now grounded in Zallet `#403` and `#139` evidence.

### Fixed-height variants of every chain query

Zallet's `components/sync.rs:528-532` (`z_get_address_utxos` not chaininfo-aware) and the verbosity-without-blockhash workaround in `components/json_rpc/methods/get_raw_transaction.rs` both want the same thing: query at a specific epoch, not at "current chain". Every query in Zinder's Rust API that depends on chain state must accept an optional `at: ChainEpoch` parameter. The default may be "latest", but the typed escape hatch must exist.

### Compact block streaming with batch range fetch

Zallet's `components/sync/steps.rs:194-300` fetches blocks in a one-at-a-time loop. Zaino's `get_block_range` ([#791](https://github.com/zingolabs/zaino/issues/791)) is 3x slower than `lightwalletd` for the same reason. Zinder's Rust API should expose a batch range fetch:

```rust
fn compact_block_range(
    &self,
    range: RangeInclusive<BlockHeight>,
    at: ChainEpoch,
) -> impl Stream<Item = Result<CompactBlockArtifact, IndexerError>>;
```

The same streaming contract underlies the native gRPC `WalletQuery` service and
the Rust `WalletQueryApi` boundary. Both share the same generated code path.

## What Zinder Currently Provides Against These Contracts

This subsection is a 2026-04-28 audit of the Zinder code in this repository against the contracts above. It is a point-in-time snapshot; the source of truth is `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` for the native surface and `services/zinder-compat-lightwalletd/src/grpc.rs` for the lightwalletd-compatible surface.

Zinder's durable direction is now [ADR-0008: Consumer-neutral wallet data
plane](../adrs/0008-consumer-neutral-wallet-data-plane.md): compatibility
methods and native `ChainIndex` methods are different public contracts over the
same canonical artifacts. Zashi/Zodl integration remains evidence for the
lightwalletd-compatible flow, not the architecture center.

### Native `WalletQuery` (`zinder_proto::v1::wallet`)

Implemented end-to-end:

- `LatestBlock`, `CompactBlock`, `CompactBlockRange` (server streaming, capped by `max_compact_block_range` with default `1000`).
- `Transaction` by transaction id.
- `TreeState` by height, `LatestTreeState`, `SubtreeRoots` (paged, request-bounded).

Every response carries the `ChainEpoch` it was answered from, and native read requests accept an optional `at_epoch` pin. This satisfies the atomic-snapshot contract for multi-call wallet flows: a client can call `LatestBlock`, persist the returned `ChainEpoch`, and require `CompactBlockRange`, `TreeState`, `LatestTreeState`, `SubtreeRoots`, and `Transaction` to answer from that same epoch.

Not yet on the native surface:

- Enriched transaction response. `Transaction` returns the artifact bytes; it does not yet carry `(height, hex, confirmations, branch_id)`, which Zallet currently reconstructs from `RawTransaction` via the workaround at `components/sync.rs:647-660`.
- Address-history queries. `TransactionsInvolvingAddress` (Zallet's `#237` use case) and t-address tx-id lookups are not on the native surface.
- Mempool query or subscription. No native RPC exposes mempool presence, mempool spends, or mempool UTXOs.

### Lightwalletd compat (`zinder_proto::compat::lightwalletd`)

Implemented end-to-end in `services/zinder-compat-lightwalletd/src/grpc.rs`:

- `GetLatestBlock`, `GetBlock`, `GetBlockRange`, `GetBlockNullifiers`, `GetBlockRangeNullifiers`.
- `GetTransaction` by hash and by block index.
- `GetTreeState` by height, `GetLatestTreeState`. Hash-only lookup returns `Status::unimplemented`.
- `GetSubtreeRoots`. `maxEntries = 0` is clamped to `DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS` rather than treated as unbounded.
- `GetAddressUtxos`, `GetAddressUtxosStream`. `maxEntries = 0` is clamped to `DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS` and results are served from stored transparent UTXO artifacts.
- `SendTransaction`, gated on the `[node]` configuration block.
- `GetLightdInfo`. Several fields (`zcashd_build`, `git_commit`, `donation_address`, `upgrade_name`, `upgrade_height`) are populated as empty strings or zero; `taddr_support` is `true` because the UTXO stream is backed by stored transparent artifacts.
- `Ping`.

Returning `Status::unimplemented` today, with each row mapped to a Zallet call site that exercises it in steady state:

| Compat method | Returns | Zallet call site |
| ------------- | ------- | ---------------- |
| `GetMempoolTx`, `GetMempoolStream` | `Status::unimplemented` | `components/sync.rs:388,407` (also the Zallet tip-change signal) |
| `GetTaddressTxids`, `GetTaddressTransactions` | `Status::unimplemented` | `components/sync.rs:756` |

Android SDK and Zashi compatibility details are owned by
[Findings from Android wallet integration](android-wallet-integration-findings.md)
and the canonical claim in
[Wallet data plane](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims).

A Zallet build wired against `zinder-compat-lightwalletd` today still needs
mempool and transparent-address history parity. Closing those rows is parity
work, not differentiation work.

### Status-by-contract summary

| Contract from "What Zinder Owes Zallet" | Status | Notes |
| --------------------------------------- | ------ | ----- |
| Atomic chain snapshots | Done for M2 | `ChainEpochReadApi` snapshots in place; per-response `chain_epoch` advertised; native requests and `zinder-client::ChainIndex` support request-side epoch pins |
| Typed Rust API | M2 slice implemented | `zinder-client` exports `ChainIndex`, `LocalChainIndex`, `RemoteChainIndex`, typed `TxStatus`, typed `TransactionBroadcastResult`, `IndexerError`, chain-event streams, and epoch-pinned read variants |
| Chain notifications | Native exposed | `WalletQuery.ChainEvents`, `IngestControl.ChainEvents`, and `zinder-client::ChainIndex::chain_events` expose replayable Tip/Finalized chain events with retention pruning; deployed query processes proxy public streams to the private ingest-control endpoint |
| Mempool as queryable index | Not exposed | Compat shim returns `Status::unimplemented`; native surface has no mempool method |
| Fixed-height variants of every chain query | M2 read surface done | Heighted reads and transaction queries accept request-side `ChainEpoch` pins on the native and typed Rust surfaces |
| Compact block streaming with batch range fetch | Done | `CompactBlockRange` streams up to `max_compact_block_range` per request, bounded |

### Architectural opportunities the existing internal vocabulary already covers

These are not new findings; they are existing internal Zinder mechanisms whose wallet-facing exposure would directly close numbered Zallet TODOs. Each is either an architectural decision pending (ADR-shaped) or implementation work behind an existing contract:

- `ChainEvent::ChainReorged { reverted, committed }` already exists in `crates/zinder-store/src/chain_event.rs`. Exposing it via `WalletQuery.ChainEvents` (M2 spec §D1) makes Zallet's `find_fork` walk obsolete.
- `StreamCursorTokenV1` (per [ADR-0005](../adrs/0005-chain-event-cursor-sequence.md)) is already designed for resumable streaming. The same cursor that backs internal `chain_event_history` paging can back wallet-facing live subscriptions.
- `ChainEpochReadApi::chain_epoch_reader_at(ChainEpochId)` backs request-side epoch pinning on `WalletQuery` and `zinder-client::ChainIndex`, so clients can keep multi-call wallet reads on one chain epoch.

# Part C: Cross-Cutting Requirements

The two halves agree on more than they disagree. Six themes cross both Zebra and Zallet evidence.

## 1. Push, with durable cursors and gap recovery

Zebra wants Zinder to push, not poll (Zebra `#8610`). Zallet wants Zinder to push, not poll (Zallet `sync.rs:68-73`). Both push surfaces lack replay on disconnect today (Zebra's gRPC stream is one-shot; Zaino's mempool stream signals "new block" by closing).

Zinder must:

- Consume Zebra's push streams with a durable cursor in `zinder-source`.
- Expose its own push streams to consumers with a typed `cursor: u64` or `epoch: ChainEpoch` so consumers can resume after disconnect.
- Distinguish stream end (graceful) from stream error (recoverable) from data delivered (no end).

## 2. Typed errors and typed states everywhere

Both Zebra (issue `#10405` for method access groups, `#10404` for typed auth) and Zallet (string-matching workarounds) want typed contracts. Zinder's RFC-0001 §Operations Model already enumerates typed readiness causes. Extend this to:

- Typed indexer errors (no `tonic::Status` leaking past `zinder-source`).
- Typed transaction statuses.
- Typed broadcast results.
- Typed readiness causes (already named).

## 3. Capability discovery, not version pinning

Zaino's tracker has eight "Support Zebra X.Y" issues across two years. Zebra now exposes OpenRPC capability discovery. Zallet's `#237` (`ChainIndex` migration) is itself a capability-discovery problem ("does this indexer support snapshot semantics?"). Zinder must:

- Probe Zebra capabilities via `rpc.discover` on connect.
- Expose its own capability descriptor (proto reflection plus a `zinder-proto` capability message) so Zallet and external consumers can detect feature support without version pinning.

## 4. Privacy boundary as a hard contract

Zallet runs the wallet locally; the indexer never sees viewing keys. Zebra's stance is that wallets belong outside Zebra. Zinder's stance is that scanning belongs outside Zinder. All three agree.

Zinder must keep this contract enforceable in code:

- No `ViewingKey`, `SpendingKey`, or `Seed` types in `zinder-core`, `zinder-store`, `zinder-query`, or `zinder-proto`.
- `zinder-derive` may build address-history-style views only over public chain data; never over wallet-supplied secrets.

## 5. Operator-visible schema and feature flags

Zebra's `--features indexer` flag changes the DB format silently. Zaino's DbV1/DbV2 split changes storage silently. Both are operator footguns. Zinder must:

- Surface a typed `schema_mismatch` readiness state if Zebra's feature flag is missing.
- Refuse to start (not silently auto-migrate) if Zinder's own schema fingerprint disagrees with on-disk state.
- Print the effective configuration and connected source capabilities at startup, including Zebra's feature flag set.

## 6. Compatibility matrix as a CI artifact

Zinder must stay compatible with at least three counterpart surfaces: Zallet (in-process Rust), external lightwalletd-compatible mobile wallets (gRPC), and Zebra as the upstream node. Each pair of versions defines a compatibility matrix.

A nightly CI job should run `(zinder ⨯ zebra ⨯ zallet)` integration tests against the supported version range. Zaino's `#991` (CI fails on non-zingolabs branches) and `#893` (CI overwhelmed by noisy tests) show why Zinder should design this as early contributor infrastructure rather than a later release task.

# Open Decisions Surfaced by This Research

These are not yet captured in PRD-0001, RFC-0001, or any accepted ADR. They should be:

1. **Rust-API client crate.** Zallet's primary contract is in-process Rust, not gRPC. `zinder-client` now exposes the M2 typed Rust API with `BlockHeight`, typed errors, typed transaction status, broadcast outcomes, local secondary reads, remote gRPC reads, request-side `ChainEpoch` pinning, and chain-event streams.
2. **Zebra source-adapter contract.** `zinder-source` must own: (a) durable cursor for `NonFinalizedStateChange`, (b) gap-fill via `AnyChainBlock`, (c) capability probe via `rpc.discover`, (d) Zebra DB-feature-flag detection, (e) gating on Zebra `/ready`. This is one ADR or extended architecture doc.
3. **Mempool index contract.** Insert-only is forbidden by RFC-0001, but the queryable-index requirements (presence, mempool spends, mempool UTXOs by address) are now grounded in Zallet evidence. [M3 Mempool](../specs/m3-mempool.md) picks `MempoolSnapshot` plus `MempoolEvents`; future work should refine payload fields and retention behavior, not reopen the basic surface split without new evidence.
4. **Schema versioning beyond envelopes.** ADR-0002 covers envelope headers; artifact-internal version tags for forward-compat with NU7 transaction V6 are not yet documented.
5. **Compatibility test harness.** Concrete CI design: which versions, which fixtures, what counts as a regression.
6. **gRPC indexer port auth posture.** Zinder's deployment guide must address Zebra's unauthenticated indexer port. Either by hard-requiring localhost-only, or by recommending a network policy. This is a docs-and-guardrails decision.

# Review Risks from Zaino-Zallet Integration

These are concrete integration risks Zinder's review process should catch early, drawn from Zallet source code. Paths are relative to `zallet/src/`.

1. **Polling for state by waiting for a stream to close.** (`components/sync.rs:114-116,388,407`)
2. **Untyped block heights in any public Rust API.** (`components/sync.rs:660,673,748,804,817` `try_into().expect(...)` cluster)
3. **Returning enum variants documented as "should never occur" with `unreachable!()` guards.** (`components/sync.rs:608,640,767`)
4. **String-matching on RPC error messages.** (`components/sync.rs:617,677,794`)
5. **Recomputing values the indexer should provide (e.g. consensus branch ID).** (`components/sync.rs:647-660,791-804`)
6. **Synchronously filling caches at startup.** (Zaino `#249`; Zallet workaround in `components/sync/cache.rs:18-113`)
7. **Exposing chain heights that the upstream node has not committed.** (Zallet `#222`)
8. **Race-prone non-atomic chain queries.** (`components/sync.rs:703`, `components/json_rpc/methods/view_transaction.rs:944`)
9. **Insert-only mempool caches with no queryability.** (`components/sync.rs:572`)

# How To Use This Document

When a Zinder PR adds a new public Rust API method, gRPC method, configuration option, or storage column family, the reviewer should ask:

- Does it satisfy a stated Zallet need (Part B)? If so, link the Zallet source line or issue.
- Does it correctly consume Zebra (Part A)? If it touches `zinder-source`, the seven decisions above are the checklist.
- Does it keep the privacy boundary (Cross-Cutting §4) and the schema operator-visible (§5)?
- Does it fall into one of the review risks above? If so, pause the change and link the relevant risk.

When Zallet files an issue against Zinder or `zinder-client`, this document is the cross-reference: which Zallet workaround does the issue close? Which design pressure in [Lessons from Zaino](lessons-from-zaino.md) does it address?

# Closing Note

Zaino was designed before Zebra had a streaming indexer API, before Zebra had `/ready`, before Zallet's `ChainIndex` migration plan existed, and before NU7 was on the roadmap. Each of those is now real. Zinder's most consequential design choice is to be the integration *between* a Zebra that already exposes the right primitives and a Zallet that has spent two years writing down what it needs from an indexer. Both sides have done substantial work that Zinder gets to inherit. The job is to make the inheritance visible in the code.
