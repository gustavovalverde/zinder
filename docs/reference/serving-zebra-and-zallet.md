# Serving Zebra and Zallet

Reference notes for the two codebases on either side of Zinder: Zebra as the upstream node and Zallet as the primary native Rust wallet consumer. Architecture rules live in the architecture docs; this page keeps source evidence and integration constraints.

- **Zebra** is the ZFND upstream Zcash node behind Zinder's primary `NodeSource`. Zebra has shipped substantial new infrastructure for indexer consumers in the last 12 months: a streaming gRPC indexer API, dedicated `/healthy` and `/ready` probes, OpenRPC capability discovery, an `indexer` feature flag that changes the canonical DB format, and `AnyChainBlock` requests for non-best-chain access. Zaino was largely written before any of this existed; Zinder must be designed *around* it.
- **Zallet** is the full-node Zcash wallet in `zcash/wallet` and Zinder's primary native Rust downstream consumer. Zallet currently bundles Zaino in-process through `IndexerService::<FetchService>`. Its source code contains a concrete inventory of indexer capabilities Zallet still needs, expressed as `TODO` comments naming the missing capability. The Zaino tracker also contains many `Zallet,api-design,ECC/Z3-Request` issues filed by ECC engineers. Zallet's planned integration target is the `ChainIndex` trait. Zinder can provide that interface for the product guarantees it owns.

## Zebra Consumer Constraints

### What Zebra Now Exposes

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

### How Zinder Should Consume Zebra

The seven decisions below come directly from Zebra's tracker and the gRPC implementation, not from generic best practice.

#### 1. Use `NonFinalizedStateChange` as the primary block ingestion path, with a durable cursor and gap-recovery

The push stream is the intended primary contract. Issue `#8610` (closed, the original tracking issue for Zebra's indexer support) makes this explicit. Polling Zebra's JSON-RPC for `getbestblockhash` or `getblock` is the wrong default.

The non-obvious constraint: the stream has *no replay on reconnect*. The receiver in `zebra-state/src/response.rs:282-292` is consumed once. The internal mpsc channel in `zebra-rpc/src/indexer/methods.rs:21` is bounded at `RESPONSE_BUFFER_SIZE = 4_000`. If Zinder's consumer falls behind, the send fails and the gRPC task drops. If Zebra restarts, the stream terminates. On reconnect, Zinder gets blocks from the *current* non-finalized state forward; nothing fills the gap.

Zinder's mitigation must be a durable last-processed-hash cursor, paired with an `AnyChainBlock` walk on reconnect to fetch any hashes Zinder has not yet committed. This belongs in `zinder-source` (the Zebra adapter), not in `zinder-ingest`'s state machine. The state machine should see a continuous, gap-free observation stream.

#### 2. Treat `AnyChainBlock` as the canonical fetch primitive, not `Block`

`ReadRequest::Block` returns only best-chain blocks. Once a non-finalized side-chain block falls off the listener, `Block` cannot retrieve it. This is the Zaino bug class behind `#10305`: side-chain blocks vanished, Zaino served stale data. Zinder must pair every block hash observation with an `AnyChainBlock` fetch and a separate is-best-chain tag refreshed from `AnyChainTransactionIdsForBlock`. When the best chain reorganises, retag; do not re-fetch.

#### 3. Gate Zinder readiness on Zebra readiness

Zebra's `/ready` returns 200 only when block lag is `≤ ready_max_blocks_behind` (default 2) and tip age is `≤ ready_max_tip_age` (default 5 minutes). Zinder's own readiness signal must not contradict Zebra's. If Zebra is `not_ready/syncing`, Zinder is at most `not_ready/node_unavailable` regardless of how many epochs Zinder has materialised locally. The typed readiness causes named in [RFC-0001 §Operations Model](../rfcs/0001-service-oriented-indexer-architecture.md) already enumerate this; the implementation must consume Zebra's probe, not just expose Zinder's own.

#### 4. Refuse to start if Zebra's DB does not have the indexer feature when Zinder needs it

Zebra's `--features indexer` flag is not a runtime toggle. It changes the on-disk DB format version: `zebra-state/src/constants.rs:74-77` appends `+indexer` build metadata to the semver. It also adds three nullifier-to-`TransactionLocation` column families (`book/src/dev/state-db-upgrades.md:354-365`). A Zebra started with the flag and restarted without it will trigger a migration that drops these column families. A Zinder that depends on `SpendingTransactionId` will then panic.

Zinder's startup phase `connect_node` (named in [Service Operations](../architecture/service-operations.md)) should probe Zebra's reported version and stay out of `ready` if the indexer feature is missing and any Zinder-served API needs it. This is operator UX work: the operator risk today is silent misconfiguration.

#### 5. Add Zinder-side auth on the gRPC indexer port

Zebra's gRPC indexer endpoint has no authentication. The JSON-RPC server has cookie auth by default (`book/src/user/lightwalletd.md:49`); the gRPC port does not. Anyone with network access to the indexer port can subscribe to all chain and mempool events.

This is Zebra's gap, not Zinder's, but Zinder is the most likely co-located service. Zinder's deployment guide must require Zebra's gRPC port to be reachable only on localhost or a private network, and Zinder's documentation must call this out as a security-relevant assumption. The closed issue `#10405` (RPC method access groups, planned but unbuilt) signals that future Zebra auth will be method-group-scoped; Zinder's source adapter should be structured so per-method tokens can be slotted in.

#### 6. Discover capabilities, do not version-pin

Zebra exposes `rpc.discover` (OpenRPC, v4.2.0+) and `getblockchaininfo` (which includes upgrade activation heights). These are the canonical capability surfaces. Zaino's tracker includes several "Support Zebra X.Y" issues (`#1034`, `#926`, `#816`, `#561`); Zinder should avoid a per-release dependency cycle by probing capabilities directly.

`zinder-source` should call `rpc.discover` on connect, parse the method list, fail loud if a required method is absent, and warn (not fail) if extra methods are present. Activation heights come from Zebra, not Zinder constants. This is the Zaino `#743` lesson made operational.

#### 7. Parallel, not duplicated, observability

Zebra emits `rpc.requests.total`, `rpc.request.duration_seconds`, `rpc.active_requests` per method. Zinder should emit equivalents on its own surfaces (`zinder.api.requests.total`, `zinder.api.request.duration_seconds`) so an operator with one Prometheus dashboard can correlate Zebra-side latency with Zinder-side latency. The `state.*` and `sync.*` Zebra metrics already cover upstream-node state; Zinder should not re-collect them; it should reference them in dashboards.

### Remaining Zebra Integration Constraints

These are the gaps Zinder will have to live with, with concrete mitigations Zinder owns.

| Gap | Evidence | Zinder mitigation |
|-----|----------|-------------------|
| `NonFinalizedStateChange` has no replay on reconnect | `zebra-rpc/src/indexer/methods.rs:21`, `zebra-state/src/response.rs:282-292` | Durable cursor + `AnyChainBlock` gap fill in `zinder-source` |
| No side-chain tip enumeration | Issue `#9541` partially closed; only `AnyChainBlock` fix shipped | Track non-finalized chain heads in `zinder-ingest` from observed block hashes; do not trust Zebra to enumerate them |
| No auth on gRPC indexer port | `zebra-rpc/src/indexer/server.rs` | Operational: localhost-only deployment, network policy enforced by docs |
| Indexer feature flag changes DB format silently | `zebra-state/src/constants.rs:74-77` | Startup probe + typed `schema_mismatch` readiness state |
| `getaddresstxids` had correctness bugs | Issue `#9742` (closed) | Snapshot tests against mainnet data, not parity claims |
| `MAX_BLOCK_REORG_HEIGHT = 99` is a Zebra constant | `zebra-state/src/constants.rs:31` | Zinder's `ReorgWindow` must be configurable but bounded by Zebra's; query Zebra at startup |

### NU7, Crosslink, and Schema Forward-Compatibility

`NetworkUpgrade::Nu7` is defined in `zebra-chain/src/parameters/network_upgrade.rs:61-63` with no activation height set on mainnet or testnet (`network_upgrade.rs:110`, branch ID placeholder `0xffffffff`). Transaction V6 is gated behind `zcash_unstable=nu7`. The fee logic already branches on `Nu7` (`zebra-rpc/src/methods/types/get_block_template/zip317.rs:179`).

For Zinder, this is a forward-compatibility constraint. The Zaino tracker's `#1007` ("zaino assumes 100 blocks") and `#1006` ("zaino assumes genesis is committed") came from Crosslink workshop testing. Zinder's storage shape must absorb Nu7 without a reindex:

- `Spend` enum variants stored as open-ended tagged unions, not fixed-size arrays. New nullifier types (e.g. unified-pool nullifiers) become a new variant with a new tag byte; old variants do not move.
- Activation heights are upstream-node-supplied, never compiled in.
- Transaction artifact storage carries a version tag so a Nu7-format V6 transaction is distinguishable from a V5 with the same wire bytes.

This is consistent with [ADR-0002](../adrs/0002-boundary-specific-serialization.md) (envelope header carries schema version) but the artifact-internal versioning is not yet documented; it should be added to the [Storage Backend](../architecture/storage-backend.md) doc when artifact families are finalised.

## Zallet Data Plane Constraints

### Current Zallet Indexer Boundary

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

### Zallet Integration Constraints in the Current API

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

### What Zinder Owes Zallet

These are the concrete API contracts Zallet's source code and tracker imply. None of them are speculative; each ties to a cited workaround or open issue.

#### Atomic chain snapshots (the `ChainEpoch` bet)

Every "chaininfo-aware" TODO in Zallet, the entire `#237` migration, and `components/json_rpc/methods/view_transaction.rs:944` depend on this single capability: take a snapshot of chain state at one height-and-hash, and issue multiple queries against it consistently.

[PRD-0001 Implementation Decisions](../prd-0001-zinder-indexer.md) commits to this: _"Query responses that require chain consistency must read from one epoch. Mixing latest values from different epochs is a correctness bug."_ The `ChainEpoch` type is the canonical mechanism. The Rust API surface Zallet consumes must expose epoch-pinned readers, not method-by-method calls against an implicitly-current chain.

#### Typed Rust API: heights, errors, transaction status

Zallet's seven `try_into().expect(...)` calls and three string-matching error checks both vanish if Zinder's Rust API uses:

- `zinder_core::BlockHeight` for every block height, never `u64` or `i64`.
- Typed `TxStatus { Mined { height, hash, time }, InMempool, NotFound, ConflictingChain }` for transaction lookup.
- Typed `TransactionBroadcastResult { Accepted { txid }, Rejected, Duplicate { txid }, InvalidEncoding, Unknown }` for broadcast.
- Typed `IndexerError` enums where Zaino currently returns RPC error strings.

This is not a wire-protocol decision; it is a Rust-API decision in `zinder-query` (or a `zinder-client` companion crate). The gRPC and JSON-RPC surfaces stay protocol-pinned; the Rust client is what Zallet imports.

#### Chain notifications, separated from mempool streams

Zallet's `#136`, `#159`, and the `Notify` workaround at `components/sync.rs:115-119` all come from conflating "new block" with "stream closed". Zinder's [Chain events](../architecture/chain-events.md) vocabulary distinguishes `ChainSourceEvent`, `ChainEvent`, and `ChainEventEnvelope`; the wallet data plane should expose at least:

- A chain-event subscription consumers can resume from a cursor. Closing the stream means the consumer disconnected, never "a new block arrived." [Wallet data plane §Chain-Event Subscription](../architecture/wallet-data-plane.md#chain-event-subscription) defines this as `WalletQuery.ChainEvents`.
- A separate mempool subscription with typed `MempoolChange { Added | Invalidated | Mined }` events (mirroring Zebra's `MempoolChangeKind`).

This is also the right granularity for Zallet's `#403` (rebroadcast detection) because `MempoolChange::Invalidated` carries the eviction reason. The existing `ChainEvent::ChainReorged { reverted, committed }` shape (in `chain-events.md`) collapses Zallet's N-round-trip `find_fork` walk at `components/sync/steps.rs:156-187` into a single envelope: receive event, `db_data.truncate_to_height(reverted.from_height - 1)`, resume scan. The 10-block safety margin at `components/sync.rs:265` becomes unnecessary.

#### Mempool as a queryable index

The Zallet `#139`, `#403`, and `sync.rs:467` cluster all need mempool *queries*, not just *streams*. Zinder must back the mempool with an indexed view (epoch-bound or sequence-numbered, per [RFC-0001 §Mempool Model](../rfcs/0001-service-oriented-indexer-architecture.md)) that supports at minimum:

- `is_in_mempool(txid) -> bool`
- `transparent_mempool_outputs_by_address(request) -> Vec<TransparentMempoolOutput>`
- `transparent_mempool_spend_by_outpoint(outpoint) -> Option<TransparentMempoolSpend>`

Insert-only mempool caches are explicitly forbidden by RFC-0001; the design rationale is now grounded in Zallet `#403` and `#139` evidence.

#### Fixed-height variants of every chain query

Zallet's `components/sync.rs:528-532` (`z_get_address_utxos` not chaininfo-aware) and the verbosity-without-blockhash workaround in `components/json_rpc/methods/get_raw_transaction.rs` both want the same thing: query at a specific epoch, not at "current chain". Every query in Zinder's Rust API that depends on chain state must accept an optional `at: ChainEpoch` parameter. The default may be "latest", but the typed escape hatch must exist.

#### Compact block streaming with batch range fetch

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

### What Zinder Provides Against These Contracts

This subsection summarizes the Zinder code against the contracts above. The source of truth is `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto` for the native surface and `services/zinder-compat-lightwalletd/src/grpc.rs` for the lightwalletd-compatible surface.

Zinder's durable direction is captured in [ADR-0008: Consumer-neutral wallet data plane](../adrs/0008-consumer-neutral-wallet-data-plane.md): compatibility methods and native `ChainIndex` methods are different public contracts over the same canonical artifacts. Zashi/Zodl integration is evidence for the lightwalletd-compatible flow, not the architecture center.

#### Native `WalletQuery` (`zinder_proto::v1::wallet`)

Implemented end-to-end:

- `LatestBlock`, `CompactBlock`, `CompactBlockRange` (server streaming, capped by `max_compact_block_range` with default `1000`).
- `Transaction` by transaction id.
- `TreeState` by height, `LatestTreeState`, `SubtreeRoots` (paged, request-bounded).
- `BroadcastTransaction` with typed accepted/duplicate/invalid-encoding/rejected/unknown outcomes (gated on `[node]` config plus the source-advertised `transaction_broadcast` capability).
- `ChainEvents` server-streaming with replayable `StreamCursorTokenV1` cursors and Tip/Finalized families.
- `MempoolSnapshot` and `MempoolEvents`: bounded snapshot plus replayable `Added`/`Invalidated`/`Mined` events with `MempoolStreamCursorV1` resumption.
- `TransparentAddressUtxos` and `TransparentAddressUtxosStream`: paged and streaming UTXO reads keyed by either `script_hash` or base58 `address` through the shared `AddressLookup` selector.
- `TransparentAddressTxIdsInRange`: server-streamed transparent-address tx-history index with ascending or descending iteration.
- `ServerInfo` returning the `ServerCapabilities` descriptor with capability strings and node-side capabilities.

Every response carries the `ChainEpoch` it was answered from, and native read requests accept an optional `at_epoch` pin. This satisfies the atomic-snapshot contract for multi-call wallet flows: a client can call `LatestBlock`, persist the returned `ChainEpoch`, and require `CompactBlockRange`, `TreeState`, `LatestTreeState`, `SubtreeRoots`, `Transaction`, `TransparentAddressUtxos`, and `TransparentAddressTxIdsInRange` to answer from that same epoch.

Not yet on the native surface:

- `getrawtransaction(txid, blockhash)` form (non-best-chain transaction lookup) needs a separate API if explorer or zcashd-compat parity requires it.

#### Lightwalletd compat (`zinder_proto::compat::lightwalletd`)

Implemented end-to-end in `services/zinder-compat-lightwalletd/src/grpc.rs`:

- `GetLatestBlock`, `GetBlock`, `GetBlockRange`, `GetBlockNullifiers`, `GetBlockRangeNullifiers`.
- `GetTransaction` by hash and by block index.
- `GetTreeState` by height, `GetLatestTreeState`.
- `GetSubtreeRoots`. `maxEntries = 0` is clamped to `DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS` rather than treated as unbounded.
- `GetAddressUtxos`, `GetAddressUtxosStream`. `maxEntries = 0` is clamped to `DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS` and results are served from stored transparent UTXO artifacts.
- `GetTaddressTxids`, `GetTaddressTransactions`. Both consume the bounded `TransparentAddressTxIdsInRange` native surface; `GetTaddressTxids` returns txid bytes in `RawTransaction.data` (matching the upstream lightwalletd-go quirk), `GetTaddressTransactions` fetches and returns full raw transaction bytes.
- `SendTransaction`, gated on the `[node]` configuration block.
- `GetMempoolTx`, `GetMempoolStream`. Both gated on the adapter's `mempool_surface` option; `Status::unavailable` without it. With it, `GetMempoolStream` closes cleanly on tip change when a `tip_change_watcher` is wired, preserving the lightwalletd Go server's de-facto contract.
- `GetLightdInfo`. Several fields (`zcashd_build`, `git_commit`, `donation_address`, `upgrade_name`, `upgrade_height`) are populated as empty strings or zero; `taddr_support` is `true` because the UTXO stream is backed by stored transparent artifacts.
- `Ping`.

Returning `Status::unimplemented` today:

| Compat method | Returns |
| ------------- | ------- |
| (no compat methods currently return `Status::unimplemented` after the prevout, balance, and selector surfaces shipped) ||

Android SDK and Zashi compatibility details are owned by
[Findings from Android wallet integration](android-wallet-integration-findings.md)
and the canonical claim in
[Wallet data plane](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims).

#### Status-by-contract summary

| Contract from "What Zinder Owes Zallet" | Status | Notes |
| --------------------------------------- | ------ | ----- |
| Atomic chain snapshots | Done | `ChainEpochReadApi` snapshots in place; per-response `chain_epoch` advertised; native requests and `zinder-client::ChainIndex` support request-side epoch pins |
| Typed Rust API | Done for read + mempool surfaces | `zinder-client` exports `ChainIndex`, `LocalChainIndex`, `RemoteChainIndex`, typed `TxStatus`, typed `TransactionBroadcastResult`, `IndexerError`, chain-event streams, mempool snapshot/event streams, transparent-mempool overlay reads, and epoch-pinned variants |
| Chain notifications | Done | `WalletQuery.ChainEvents`, `IngestControl.ChainEvents`, and `zinder-client::ChainIndex::chain_events` expose replayable Tip/Finalized chain events with retention pruning; deployed query processes proxy public streams to the private ingest-control endpoint |
| Mempool as queryable index | Done | `MempoolSnapshot` (bounded snapshot), `MempoolEvents` (replayable `Added`/`Invalidated`/`Mined` stream), `ChainIndex::is_in_mempool`, `transparent_mempool_outputs_by_address`, `transparent_mempool_spend_by_outpoint`, and native gRPC point lookups for transparent mempool outputs and spends |
| Fixed-height variants of every chain query | Done | Heighted reads, transaction queries, transparent UTXO reads, and transparent tx-history reads accept request-side `ChainEpoch` pins on both native and typed Rust surfaces |
| Compact block streaming with batch range fetch | Done | `CompactBlockRange` streams up to `max_compact_block_range` per request, bounded |
| Transparent-address read surface | Done | `TransparentAddressUtxos[Stream]`, `TransparentAddressTxIdsInRange`, `TransparentAddressBalance`, matching `ChainIndex` methods, lightwalletd-compat `GetAddressUtxos*`, `GetTaddressTxids`, `GetTaddressTransactions`, and `GetTaddressBalance*` |
| Transaction broadcast | Done | Native `BroadcastTransaction` with typed outcomes; compat `SendTransaction` maps onto the same path; gated on `[node]` config plus the source-advertised `transaction_broadcast` capability |
