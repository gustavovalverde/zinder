# Wallet Data Plane

The wallet data plane is the part of Zinder that wallets and wallet-like applications call. It is not a wallet and must not become one by accident.

## Responsibility

`zinder-query` owns the wallet data plane.

Per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md),
this plane is consumer-neutral. Android SDK/Zashi, lightwalletd clients,
Zallet, and future Rust consumers exercise different public contracts, but they
all depend on the same canonical artifact families. Compatibility adapters may
preserve legacy wire names; the core vocabulary stays on artifact coverage,
tree-state anchors, chain epochs, and typed errors.

It should provide:

- Compact block range APIs.
- Latest block and chain metadata APIs.
- Transaction lookup APIs where compatible with Zcash wallet expectations.
- Tree state APIs required for wallet sync.
- Sapling and Orchard subtree root APIs required for batched wallet scanning.
- Transparent-address UTXO APIs required by lightwalletd/Zashi compatibility,
  once the stored transparent UTXO artifact family lands.
- Transaction broadcast.
- Chain-event subscription per [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription).
- Mempool snapshot and mempool-event subscription in M3 per [M3 Mempool](../specs/m3-mempool.md).
- `ServerInfo` capability descriptor per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery).
- Optional compatibility endpoints for lightwalletd clients.

It should not provide:

- Spending-key custody.
- Viewing-key custody.
- Seed phrase storage.
- Server-side shielded wallet scanning.
- Address ownership inference for shielded users.
- Compliance or identity logic.

## Privacy Boundary

A wallet-facing indexer can still leak metadata. Zinder must treat wallet API design as a privacy boundary.

Required privacy rules:

- Do not require shielded users to reveal spending keys or viewing keys.
- Do not add server-side address scanning as a convenience feature.
- Keep request logs free of sensitive wallet identifiers where possible.
- Document which API calls can reveal interest in a height range, transaction, or address-like value.
- Support deployment behind privacy-preserving transport where operators need it.

## Compact Blocks

The compact block builder belongs to ingestion. The wallet data plane serves compact block artifacts through `ChainEpochReadApi` or through a query-owned store fed by canonical artifacts.

Compact block payload bytes follow [ADR-0002](../adrs/0002-boundary-specific-serialization.md): store protobuf-compatible payload bytes inside a fixed artifact envelope. The protobuf payload shape is the pinned lightwallet protocol contract recorded by `zinder-proto`, not whichever version a contributor happens to remember. `zinder-query` may decode and re-encode through generated tonic messages until a raw protobuf serving path is proven, but it must not translate compact blocks into a Zinder-only durable format.

This avoids two problems:

- Query-time construction can mix chain views under concurrent reorgs.
- Wallet traffic can force expensive upstream node reads or artifact derivation.

If a compact block artifact is missing, `zinder-query` should return a typed unavailable error or readiness failure. It should not fetch the block from the upstream node and build a one-off response.

The native wallet protocol slices expose latest block metadata, compact block ranges, tree-state reads, latest tree-state reads, subtree roots, lightd-compatible network metadata, and the chain-event subscription described below as generated `zinder_proto::v1::wallet` responses. Each response carries the `chain_epoch` used to answer the read when the response depends on chain state. Native gRPC streams compact block ranges as `CompactBlockRangeChunk` messages so range size is bounded by request limits and not by a single gRPC response message. `WalletQueryGrpcAdapter` serves the generated native `WalletQuery` tonic service over `WalletQueryApi` through `grpc/native.rs` response builders and preserves the same epoch binding, unavailable-artifact, and range limit behavior.

Tree-state storage may preserve upstream node JSON as an ingestion artifact, but the
serving path must return the public protocol shape expected by the caller. For
lightwalletd compatibility, that means a `TreeState` response, not raw
`z_gettreestate` JSON bytes.

Subtree roots are also wallet-sync artifacts. Query and compatibility services
must serve them from epoch-bound indexed state and must distinguish a valid empty
range from a not-yet-available upstream node subtree index.

The native query path makes that distinction without upstream node repair or
query-time compact-block decoding: it reads `ChainTipMetadata` from the same
`ChainEpoch` to decide whether a subtree index can exist, then reads stored
`SubtreeRootArtifact` values for completed roots. Missing completed roots
return a typed unavailable error; ranges beyond the completed subtree count
return an empty response.

## Chain-Event Subscription

Wallet sync needs durable chain-state notifications. `WalletQuery.ChainEvents` is the M2 native subscription that delivers `ChainEventEnvelope` messages to wallet clients in `event_sequence` order, settled by [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription). Chain ingestion already produces these envelopes at every canonical commit; this RPC is the wire boundary that exposes them to wallet clients without requiring them to poll latest-block metadata or infer tip changes from unrelated stream lifecycles.

The contract:

- The cursor is the `StreamCursorTokenV1` bytes documented in [Chain events](chain-events.md). Clients persist the exact bytes returned in the previous envelope and resume strictly after that cursor.
- Empty `from_cursor` returns events from the earliest retained event sequence, which is the bootstrap path for a fresh wallet install.
- The server emits historical events first (replay phase) and then continues with live events in one ordered sequence; clients see no transition.
- Stream end means the consumer disconnected or the server is shutting down. It never means a new block arrived. Clients must distinguish stream end from stream error and reconnect with their last persisted cursor in both cases.
- An expired cursor returns the typed `EventCursorExpired` error and does not silently restart from the current tip.

`ChainCommitted` and `ChainReorged` are the two event variants. `ChainReorged` carries both the reverted range and the replacement range, so a wallet receiving a reorg event truncates its local view at the reverted boundary and resumes from the replacement range without making additional indexer calls. Per [Chain events §Cursor varieties](chain-events.md#cursor-varieties), if a client reconnects with a cursor whose branch was reorged out, the server emits a synthetic `ChainReorged` envelope before resuming the stream; clients never observe silent branch changes.

Two cursor varieties are advertised under capability string `wallet.events.chain_v1`: `Tip` and `Finalized`. Tip consumers receive every envelope including reorgs. Finalized consumers receive only post-finality envelopes and never see `ChainReorged`. The finalized cursor family is represented in the cursor body, not by a separate `WalletQuery.ChainEventFinalizedAnchor` RPC.

The lightwalletd compatibility shim does not expose this subscription. The vendored `CompactTxStreamer` proto has no equivalent method, and ADR-0004 forbids inventing parallel surfaces in the compat layer. Wallet clients on the lightwalletd contract continue to use `GetLatestBlock` polling. Native Zinder clients receive the subscription contract from day one.

## Mempool Snapshot and Subscription (M3)

Mempool surfaces are M3. [M3 Mempool](../specs/m3-mempool.md) owns the source, live index, event-log, API, compatibility, and validation plan; M2 only reserved vocabulary and capability strings so it would not invent conflicting names.

M3 is the unconfirmed-transaction contract for several Zcash ecosystem
products, but each product consumes a different boundary. This table is the
canonical product map; reference documents carry the line-numbered source
evidence and observed wallet-run details.

| Ecosystem product | Zinder relationship | M3 enables | Required boundary |
| ----------------- | ------------------- | ---------- | ----------------- |
| Zallet (`zcash/wallet`) | Primary native Rust consumer | Typed transaction lifecycle, rebroadcast decisions, transparent unmined UTXO updates, and removal of the "mempool stream closed means tip changed" workaround | `zinder-client::ChainIndex` plus native `WalletQuery`; no dependency on the lightwalletd compatibility adapter |
| Zashi/Zodl and Android SDK wallets | lightwalletd-compatible wallet clients | SDK mempool observation, faster pending-send feedback, shielded mempool scanning, and clearer submitted/unmined/resubmitted transaction UX | `zinder-compat-lightwalletd` mapping `GetMempoolStream` and `GetMempoolTx` over the native M3 index and event log |
| Existing lightwalletd clients and operators | Migration consumers | Backend option for clients that already speak `CompactTxStreamer`, including current mempool methods and transaction submission behavior | Compatibility adapter only; no upstream node calls, no independent storage, and no Zinder-only method extensions in the lightwalletd proto |
| Block explorers and analytics | Application or `zinder-derive` consumers | Live mempool pages, pending transaction lifecycle, pending transparent address/outpoint overlays, and "mempool in sync" status | Native `WalletQuery` or replayable `zinder-derive` views; full explorer parity also needs transparent history and balance |
| Zebra | Upstream node source, not a Zinder client | Keeps wallet and explorer indexing outside the node while reusing Zebra's verified mempool observations | `zinder-source` consumes Zebra `MempoolChange` when available, or falls back to `getrawmempool` polling |
| Zaino | Ecosystem prior art and compatibility reference | Reference behavior for wallet, full-client, and block-explorer use cases while Zinder keeps its own wire, domain, and storage boundaries | Compatibility tests and reference analysis only; Zinder public APIs must not expose Zaino types |

Three architectural consequences follow from that map:

- The canonical path is `NodeSource -> MempoolSourceEvent -> MempoolIndex + MempoolEventLog -> WalletQuery -> adapters`. Compatibility methods translate over that path; they do not own their own mempool cache.
- Source observations must become hydrated `MempoolEntry` records before they reach public APIs. Zebra's streaming mempool event carries transaction hash and auth digest, so raw transaction fetching and compact-transaction construction belong in the source/ingest path, not in `zinder-compat-lightwalletd`.
- Native M3 does not inherit lightwalletd's stream-close lifecycle. `MempoolEvents` stream end means disconnect or shutdown. Chain-tip changes are delivered through `ChainEvents`.
- The public server-observed type is `MempoolEntry`, not `PendingTransaction`. A pending transaction is a wallet-local UX state: it can include a transaction that was created locally but never accepted by the network.
- Product readiness claims are boundary-specific. Zallet readiness means typed Rust `ChainIndex` coverage in a deterministic harness plus a real Zallet binary/app run. Zashi/Zodl readiness means lightwalletd-compatible methods plus SDK or app validation. Explorer readiness means M3 plus the transparent history and balance surfaces needed for address-oriented views.

The M3 native protocol will expose two complementary methods:

- **`WalletQuery.MempoolSnapshot`** returns a bounded, pageable point-in-time view of the live mempool index, bound to the visible `ChainEpoch` at call time. The response carries `snapshot_age_millis` so clients with strict freshness needs can choose to subscribe to `MempoolEvents` when the age exceeds a threshold.
- **`WalletQuery.MempoolEvents`** is a server-streaming subscription that mirrors Zebra's `MempoolChange` semantics: typed `Added`, `Invalidated`, `Mined` envelopes with cursor-resume via `MempoolStreamCursorV1`.

`Invalidated` is not optional. If the polling backend observes a txid disappear
from `getrawmempool` without a corresponding block commit, it emits
`Invalidated { reason: Unknown }` or a more specific reason when the source can
prove one. Silently dropping a txid would recreate the stale insert-only
mempool cache class documented in the Zaino comparison.

Mempool retention is two-tier (60 minutes mined / 24 hours invalidated by default, both configurable). Expired cursors return `MempoolCursorExpired` with `oldest_retained_sequence` in `PreconditionFailure` detail.

The M3 transparent-address mempool methods (`is_in_mempool`, `transparent_mempool_outputs_by_address`, `transparent_mempool_spend_by_outpoint`) will live on `WalletQueryApi` and `ChainIndex`. They are explicitly transparent-only: the privacy boundary forbids by-address shielded queries, and clients scan mempool compact-transaction payloads locally for shielded interest.

The lightwalletd compat shim will map `GetMempoolStream` and `GetMempoolTx` over `MempoolEvents` and `MempoolSnapshot` only after M3 lands. Until then, `wallet.events.mempool_v1` and `wallet.snapshot.mempool_v1` are absent from `ServerCapabilities`.

## Transparent Address UTXOs

Transparent-address UTXO queries are not shielded scanning. They reveal
transparent addresses that are already public on chain, but they still must be
served from epoch-bound indexed artifacts. `GetAddressUtxos` and
`GetAddressUtxosStream` map over `WalletQueryApi::transparent_address_utxos`,
backed by the canonical transparent address UTXO and transparent spend artifact
families in `zinder-store`.

The compatibility shim must not answer these methods by scanning compact blocks
on demand, calling upstream nodes, returning synthetic empty results for unknown
indexed state, or materializing an unbounded address result before truncating
it. Missing indexed state is a readiness or artifact-availability failure; it
is not a reason to bypass the wallet data plane.

`GetLightdInfo.taddr_support` is `true` only when the adapter reads from stored
transparent UTXO artifacts. It is a product contract for legacy lightwalletd
clients, not a way to silence Android SDK logs. The public native gRPC
capability `wallet.address.transparent_utxos_v1` remains reserved until the
native `WalletQuery` proto and `zinder-client::ChainIndex` expose the same
artifact-backed method.

Operators must only publish a `zinder-compat-lightwalletd` deployment with
`taddr_support=true` when the store was produced with the wallet-serving
backfill profile. A recent-checkpoint or tip-bootstrapped store may have the
UTXO artifact family enabled but still lack the historical rows needed by wallet
birthdays and resync anchors; that deployment posture is not wallet-serving.

`GetAddressUtxos.maxEntries` is an aggregate response budget across the
requested address set. The compatibility adapter may make several internal
artifact reads to satisfy the request, but the public response and stream must
be deterministically ordered and capped as one result set. `maxEntries = 0`
uses the adapter's configured default bound rather than becoming an unbounded
query.

## Capability Discovery

`WalletQuery.ServerInfo` returns a `ServerCapabilities` descriptor per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery). Capability strings are exact-match; clients gate features on capability strings such as `wallet.events.chain_v1` rather than on Zinder version. New methods land with new capability strings; deprecated capabilities continue to be advertised alongside their replacement until the documented removal version. The `node` field of the descriptor carries upstream-node capabilities detected by `zinder-source` (e.g. `node.streaming_source`, `node.spending_tx_lookup`), giving operators a single picture of the deployment's read surface.

## In-Process Rust API

In-process consumers (Zallet, future SDK integrations) call `zinder-client` per [Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape). The `ChainIndex` trait exposes the same methods the gRPC service does, with typed Rust types (`BlockHeight`, `ChainEpoch`, `TxStatus`, `TransactionBroadcastResult`, `IndexerError`). No tonic round-trip; no untyped error strings; no `unreachable!()` guards required.

The `ChainIndex` trait does not duplicate the `WalletQueryApi` Rust trait inside `services/zinder-query`. They serve different consumer profiles: `WalletQueryApi` is the gRPC server's internal trait; `ChainIndex` is the published Rust API for in-process consumers. Both share types from `zinder-core`. A compatibility test asserts that every advertised `ServerCapabilities` capability string has a corresponding `ChainIndex` method.

## Transaction Broadcast

Transaction broadcast is a network operation, not a canonical chain commit.

`zinder-query` may expose transaction broadcast because wallets expect a single endpoint. This is wallet operation parity, not part of the minimum read-only shielded sync surface. The broadcast path must:

- Forward the raw transaction to a configured network or upstream node path.
- Return `TransactionBroadcastResult` with accepted, duplicate, invalid-encoding, rejected, or unknown outcomes.
- Avoid writing canonical chain state.
- Rely on ingestion to observe the transaction later in mempool or block data.

`zinder-source` owns source-specific broadcast I/O through `TransactionBroadcaster`. `zinder-query` may delegate to that boundary, but it must not embed Zebra or zcashd JSON-RPC behavior in query logic.

`zinder-compat-lightwalletd` exposes the same broadcast path as the
lightwalletd-compatible `SendTransaction`. The adapter only carries raw
transaction bytes through `WalletQueryApi::broadcast_transaction` and maps
typed outcomes to `lightwalletd::SendResponse` error codes. No viewing keys,
spending keys, or scanning material cross the Zinder boundary on this path; the
privacy contract from the read-sync surface applies unchanged.

Regtest can prove forwarding, typed rejection mapping, and the no-storage-mutation boundary. Testnet validation is required before Zinder promises public relay, stable fee-policy taxonomy, rebroadcast behavior, or accepted-transaction propagation semantics.

## Compatibility

`zinder-compat-lightwalletd` serves the vendored `CompactTxStreamer` proto from `zinder_proto::compat::lightwalletd` by translating `WalletQueryApi`. The full responsibility list, allowed request shapes, error mapping, and test surface are owned by [Protocol boundary §Lightwalletd Compatibility](protocol-boundary.md#lightwalletd-compatibility); native `WalletQueryApi` remains the primary API and new functionality lands there first.

v1 wallet APIs target self-hosted, single-operator deployments backed by a configured upstream node. The v1 binaries do not implement TLS termination, authentication, rate limiting, or quota accounting; an operator who needs any of those terminates them at a load balancer or reverse proxy in front of Zinder. Public-internet hosting requirements are out of v1 scope.

### External Wallet Compatibility Claims

This section is the canonical Zinder contract for external wallet serving
claims. Per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md),
the contract is consumer-neutral; Android SDK and Zashi are evidence-bearing
clients, not the architecture center. The observed method sequence, logs,
heights, and reproduction steps live in
[Android wallet integration findings](../reference/android-wallet-integration-findings.md);
do not copy those test-run details into architecture docs.

A deployment may claim Android SDK or Zashi compatibility only when:

- The serving store contains the subtree-root history required by fresh wallet
  bootstrap and tree-state history for every anchor height a supported wallet
  flow can request, including create, resync, and restore/import flows. Use
  `zinder-ingest backfill --wallet-serving` for this serving profile; recent
  checkpoints are validation fixtures, not wallet-serving stores.
- The transparent UTXO surface in [§Transparent Address UTXOs](#transparent-address-utxos)
  is implemented end-to-end.
- The transport requirement in [Service operations](service-operations.md#deployment-guidance)
  is satisfied for real Zashi endpoint tests.

The upstream Go `lightwalletd/testclient` remains smoke coverage for the basic
compat surface. It is not a substitute for an Android SDK or Zashi bootstrap
test when the release claim names those clients.

After M3, a deployment may claim Android SDK or Zashi/Zodl mempool
compatibility only when `GetMempoolStream` and `GetMempoolTx` are mapped over
the native M3 index and event log, and an SDK or app flow has observed mempool
transactions against that endpoint. A sync-only Zashi proof does not establish
pending-transaction UX.

## Query Consistency

Wallet sync APIs must read from one `ChainEpoch`. Primary in-process reads may also be backed by one RocksDB snapshot; secondary reads are snapshotless and rely on epoch-bound visibility retention per [ADR-0007](../adrs/0007-multi-process-storage-access.md).

For example, a compact block range response should bind:

- Start height.
- End height.
- Tip hash.
- Tip height.
- Finalized height.
- Artifact schema version.
- Subtree-root range or cursor when subtree data is returned.

If the chain tip changes while the request is executing, the response should still finish from the epoch it started with or restart from a new epoch. It should not mix both.

## Performance and Pagination

`lightwalletd` compatibility is the floor for wallet-sync performance, not the target. Zinder publishes P50, P99, and worst-case budgets for hot wallet endpoints and gates `GetBlockRange`-equivalent behavior in CI.

### Published Budgets (calibration target)

The first published numbers are calibration targets, not strict limits. They will promote to strict CI gates after two consecutive releases stabilize them.

The "regtest baseline" column records observed times from `services/zinder-ingest/tests/live/latency.rs::read_endpoint_latency_baseline` against a live Zebra regtest with 101 coinbase-only blocks. These are sanity-floor numbers; mainnet blocks carry real shielded payloads.

The endpoint-specific mainnet columns record observed times from
`services/zinder-ingest/tests/live/backfill.rs::backfills_last_1000_blocks_from_checkpoint`
after backfilling the last 1000 mainnet blocks from a checkpoint at
`tip - 1000`. Each cell aggregates 6 single-shot observations across separate
test runs (5 warm, 1 with a cold first-call cache effect): "P50" is the median,
"P99" is approximated by the maximum because n is too small to derive a true
99th percentile.

| Endpoint | Range / Shape | Regtest baseline (one observation) | Mainnet P50 (n=6) | Mainnet P99 (n=6, max-of-sample) | Mainnet worst-case (n=6) |
| -------- | ------------- | ---------------------------------- | ----------------- | -------------------------------- | ------------------------ |
| `latest_block` | single read | ~325 µs | ~48 µs | ~113 µs | ~113 µs |
| `compact_block_at` | one block | ~133 µs | ~40 µs | ~59 µs | ~59 µs |
| `compact_block_range` | 1 block | <1 ms | ~27 µs | ~37 µs | ~37 µs |
| `compact_block_range` | 10 blocks | <1 ms | ~57 µs | ~80 µs | ~80 µs |
| `compact_block_range` | 50 blocks | ~915 µs | ~179 µs | ~205 µs | ~205 µs |
| `compact_block_range` | 1000 blocks | <2 s (synthetic) | ~3.07 ms | ~3.26 ms | ~3.26 ms |
| `tree_state_at` | one height | ~97 µs | ~58 µs | ~69 µs | ~69 µs |
| `subtree_roots` | 1..=8 entries from checkpoint | TBD | ~11 µs | ~12 µs | ~12 µs |
| `transaction` | by id | TBD | TBD | TBD | TBD |

The mainnet `subtree_roots` row times the checkpoint-bootstrapped read shape: querying from `start_index = checkpoint_completed_subtree_count` with `max_entries = 8`. Subtree roots completed before the checkpoint are not in the store; operators must seed them out-of-band if a wallet needs them.

The report-based mainnet baseline below comes from
`scripts/observability-smoke.sh calibrate` against a synced local mainnet Zebra
on 2026-04-28. It verifies checkpoint backfill, checkpoint backup restore,
native wallet gRPC, lightwalletd-compatible gRPC, readiness gauges, source
RPC metrics, store-read metrics, RocksDB property gauges, and Prometheus alert
rule loading. The current sample count is intentionally small because it proves
the harness and captures a release-readiness anchor; release signoff should run
the same command with at least 6 samples.

| Operational metric | Mainnet P50 (n=2) | Mainnet P99 (n=2, max-of-sample) | Mainnet worst-case (n=2) |
| ------------------ | ----------------- | -------------------------------- | ------------------------ |
| `backfill_seconds` | 13.204 s | 13.243 s | 13.243 s |
| `wallet_query_p95_max_seconds` | 0.742 ms | 1.053 ms | 1.053 ms |
| `node_rpc_p95_max_seconds` | 4.118 ms | 4.228 ms | 4.228 ms |
| `store_read_p95_max_seconds` | 0.510 ms | 0.895 ms | 0.895 ms |
| `secondary_catchup_p95_max_seconds` | 4.309 ms | 4.716 ms | 4.716 ms |
| `readiness_sync_lag_blocks` | 0 | 0 | 0 |
| `readiness_replica_lag_chain_epochs` | 0 | 0 | 0 |
| `rocksdb_pending_compaction_bytes` | 0 | 0 | 0 |

The `services/zinder-query/tests/perf_smoke.rs` regtest test enforces a generous regression-only budget (`compact_block_range(1, 1000)` under 2 s, `latest_block` under 250 ms) so CI catches catastrophic regressions. Tight per-percentile gates ship after the calibration harness collects enough samples for a real 99th percentile; the n=6 endpoint table and n=2 report table above are baseline anchors, not strict release gates.

Every range or list endpoint defines:

- A maximum response size.
- A cursor or explicit closed range.
- Stable ordering.
- The epoch or source timestamp that bounds the response.

The native compact-block range API rejects requests above `max_compact_block_range` before opening a reader. Latest block metadata and tree-state reads use the same epoch-bound reader contract without upstream node repair. Batched storage reads are still bounded by the range limit; native gRPC streams one compact block per message instead of packing the whole range into a single response.

The lightwalletd compatibility adapter must also keep unbounded upstream
semantics bounded at Zinder's boundary. For example, `GetSubtreeRoots` treats
`maxEntries = 0` as a bounded compatibility request, not permission to
materialize every retained subtree root.
`GetBlockRange` consumes the bounded native range result and streams decoded
lightwalletd blocks without a second full-range allocation. True per-item store
back-pressure requires an owned snapshot streaming read API; until then the
range cap is the memory bound.
`GetAddressUtxosStream` must follow the same rule: page or stream from a
bounded transparent UTXO artifact read, never from an unbounded in-memory
address result.

Do not materialize an unbounded list and truncate it after the fact. The storage and protocol signatures should make that shape impossible.
