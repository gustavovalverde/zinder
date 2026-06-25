# Wallet Data Plane

The wallet data plane is the part of Zinder that wallets and wallet-like applications call. It is not a wallet and must not become one by accident.

## Responsibility

`zinder-query` owns the wallet data plane.

Per [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md),
this plane is consumer-neutral. Android SDK/Zashi, lightwalletd clients,
Zallet, and future Rust consumers exercise different public contracts, but they
all depend on the same canonical artifact families. Compatibility adapters may
preserve lightwalletd wire names; the core vocabulary stays on artifact coverage,
tree-state anchors, chain epochs, and typed errors.

It should provide:

- Compact block range APIs.
- Latest block and chain metadata APIs.
- Transaction lookup APIs where compatible with Zcash wallet expectations.
- Tree state APIs required for wallet sync.
- Sapling and Orchard subtree root APIs required for batched wallet scanning.
- Transparent-address output APIs required by lightwalletd/Zashi compatibility,
  backed by the stored transparent output artifact family.
- Transaction broadcast.
- Chain-event subscription per [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription).
- Mempool snapshot and mempool-event subscription per [ADR-0007](../adrs/0007-mempool-topology-and-retention.md).
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

The native wallet protocol slices expose latest block metadata, compact block ranges, checkpoint tree-state reads, latest checkpoint tree-state reads, subtree roots, lightd-compatible network metadata, and the chain-event subscription described below as generated `zinder_proto::v1::wallet` responses. Each response carries the cross-plane `ChainView` at field tag 1; wallet responses fill `chain_view.chain_epoch` with the epoch used to answer the read and leave the derive-plane axes unset (see [ADR-0011](../adrs/0011-explorer-freshness-envelope.md)). Native gRPC streams compact block ranges as `CompactBlocksInRangeChunk` messages so range size is bounded by request limits and not by a single gRPC response message. `WalletQueryGrpcAdapter` serves the generated native `WalletQuery` tonic service over `WalletQueryApi` through `grpc/native.rs` response builders and preserves the same epoch binding, unavailable-artifact, and range limit behavior.

A request pins a chain snapshot with `optional uint64 at_epoch_id`: absent resolves to the visible epoch at request time; present resolves the canonical epoch by id. The store keys the epoch by id, so a pinned read either resolves it or returns `CHAIN_EPOCH_PIN_UNAVAILABLE` when the id is no longer retained. `ChainEpoch` is a response-only descriptor nested in `ChainView`; the request never echoes the epoch body.

Tree-state storage preserves upstream node JSON at canonical checkpoints and the
latest committed tip. The native read surface is
`tree_state_at(height, at_epoch_id)` and `latest_tree_state_checkpoint(at_epoch_id)`.
`tree_state_at` serves the tree state at exactly `height`: a stored checkpoint
when one exists there, otherwise a cache-fill from the configured upstream node
(mirroring lightwalletd's `GetTreeState`), so the returned height always equals
the requested height. The fill is the one query path permitted to contact an
upstream node, gated on an explicitly supplied source (see ADR-0005). Without a
source the read serves only stored checkpoint heights and returns
`ArtifactUnavailable` for the gaps. Lightwalletd compatibility returns a
`TreeState` response for `GetLatestTreeState`; `GetTreeState(BlockID)` is a clean
passthrough to `tree_state_at` and serves any height the upstream can answer.

Subtree roots are also wallet-sync artifacts. Query and compatibility services
must serve them from epoch-bound indexed state and must distinguish a valid empty
range from a not-yet-available upstream node subtree index.

The native query path makes that distinction without upstream node repair or
query-time compact-block decoding: it reads `ChainTipMetadata` from the same
`ChainEpoch` to decide whether a subtree index can exist, then reads stored
`SubtreeRootArtifact` values for completed roots. Missing completed roots
return a typed unavailable error; ranges beyond the completed subtree count
return an empty response.

## Block Identity and Hash-Keyed Reads

Height remains the primary key for compact-block reads. Wallet scanning is
height-ordered, compact-block ranges are height-bounded, and the canonical store
keeps those reads cheap and predictable. Do not turn compact-block range APIs
into hash-driven scans.

Zinder ships a canonical best-chain hash-to-height resolver through the typed
[`zinder_core::BlockSelector`] enum (`Height(BlockHeight) | Hash(BlockHash)`).
The resolver is backed by the `block_hash_index` column family in `zinder-store`:
every committed block writes a `(network, hash) -> (height, source_chain_epoch)`
entry, and read paths verify the recorded height is still visible at the
request's chain epoch via the existing height-visibility index. Reorged-out
hashes return `BlockHashLookup::NotInBestChain` without an eager delete.

The native surface is `WalletQuery.BlockIdBySelector` (capability
`wallet.read.block_id_by_selector_v1`) returning `BlockIdResponse { chain_view,
block_id }`. Compat hash-only requests (`GetBlock`, `GetTreeState`,
`GetTransaction`-by-block-hash) call the resolver before reaching the existing
height-keyed read; height-only callers get a normalized `BlockId` with the
resolved hash. Both the gRPC adapter and `zinder-client::ChainIndex` expose
`block_id_by_selector(BlockSelector)`.

The same resolver backs the typed block-header read model:
`WalletQuery.BlockHeaderBySelector` (capability
`wallet.read.block_header_by_selector_v1`) returns `BlockHeaderResponse {
chain_view, block_header }` where `BlockHeaderInfo` is the Zinder-native
header shape (block identity, previous hash, merkle root, commitment bytes,
block time, bits, nonce, version). The shape does not re-export Zebra's
JSON-RPC `getblockheader` object or the lightwalletd compact block header. The
header is read at request time from the typed `BlockHeaderArtifact`; raw block
bytes are not part of the normal wallet read path. If repeated reads become
the larger cost, the implementation should improve the typed header row rather
than reintroducing raw-block parsing.

`BlockSelector` is `#[non_exhaustive]`. Non-best-chain `(txid, block_hash)`
lookup is a *separate method*, not a third selector arm. That form is a
non-best-chain transaction lookup and has different retention, secondary-reader,
and explorer semantics than ordinary wallet sync. `Transaction` /
`TransactionStatus` answers against the visible canonical chain plus the live
mempool; a non-best-chain lookup must add its own named API surface.

## Transaction Status and Enrichment

Native transaction lookup returns typed transaction status. The public Rust shape is
`zinder_core::TxStatus` and the native gRPC surface mirrors it through
`WalletQuery.Transaction(TransactionRequest) returns (TransactionStatusResponse)`
under capability `wallet.read.transaction_by_id_v1`. The response carries the
shared `TransactionLocation` oneof on its `location` field; the oneof has three
arms. `NotFound` is gRPC `NOT_FOUND`, not an oneof slot, because typed errors do not consume oneof variants:

- `mined`: `MinedTransaction { MinedBlockLocation location; MinedDetails details; bytes raw_transaction_bytes }`.
- `in_mempool`: `MempoolTransaction { bytes payload_bytes; int64 first_seen_unix_seconds }`.
- `conflicting`: `ConflictingChainTransaction {}` (reserved shape; status
  is the signal, fields are reserved for future non-best-chain lookup).

`TransactionLocation` is one message defined in `wallet.proto` and embedded by
every read surface that answers "where does this transaction live", including
`ExplorerQuery.TransactionDetail`, so a consumer writes one match shape for both
planes and the explorer detail carries the `conflicting` arm rather than dropping it.

The mined variant carries epoch-bound `MinedDetails {
consensus_branch_id, block_time, confirmations }`. These fields are
response/read-model values, not persisted transaction-artifact fields.
`MinedDetails::from_response_epoch(epoch, mined_height, consensus_branch_id,
block_time)` is the **only** public constructor in `zinder-core`. Callers
cannot construct `MinedDetails` without the response's `ChainEpoch` in scope,
so the racy `tip_height - block_height` confirmations computation is
prevented by construction. `consensus_branch_id` is resolved from the
process-startup `Arc<NetworkUpgradeActivations>` discovered via
`ZebraJsonRpcSource::discover_network_upgrade_activations()`: callers invoke
`activations.consensus_branch_id_at(height)`, which returns
`PRE_OVERWINTER_BRANCH_ID` for heights below the earliest activation. The
service binary refuses to start when `[node]` is not configured, so the
field always reflects a real node-discovered table at request time; see
[ADR-0008](../adrs/0008-network-parameter-discovery.md). `block_time` is
read from the stored `BlockHeaderArtifact` and falls back to `0` only when the
typed header row is unavailable.

The mined arm also carries `raw_transaction_bytes`: the serialized consensus
transaction bytes, symmetric with the mempool arm's `payload_bytes`. This makes
`WalletQuery.Transaction` a verbose mined-transaction read that returns the
serialized bytes, the mined block hash and height, the block time, and
epoch-bound confirmations in one response, which is the shape a
`getrawtransaction verbose` consumer needs. The bytes are filled from the same
`TransactionBlobArtifact` the canonical reader resolves; they are not a separate
RPC. The field rides on the existing `wallet.read.transaction_by_id_v1`
capability rather than a new one, because the bytes are not unconditionally
present: ingest writes transaction blobs only when `raw_blob_policy` is
`transactions` or `all`. When the policy is `none`, the field is empty and the
location plus enrichment fields are still returned. A consumer that requires the
serialized form runs against a deployment configured to retain transaction
blobs.

A response builder must not call the upstream node or latest tip again during
response construction.

The epoch rule is stricter for mempool. An `at_epoch_id` transaction lookup is a
canonical chain read and never consults live mempool state. A non-epoch-pinned
lookup may fall through to the writer-owned mempool index after the canonical
chain returns `NotFound`.

Each native wire response shape pairs to one capability string. The capability records the semantic shape; a change to the response shape requires a new capability string, even when the RPC name stays the same.

## Chain-Event Subscription

Wallet sync needs durable chain-state notifications. `WalletQuery.ChainEvents` is the native subscription that delivers `ChainEventEnvelope` messages to wallet clients in `event_sequence` order, settled by [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription). Chain ingestion already produces these envelopes at every canonical commit; this RPC is the wire boundary that exposes them to wallet clients without requiring them to poll latest-block metadata or infer tip changes from unrelated stream lifecycles.

The contract:

- Each `ChainEventEnvelope` carries the cross-plane `ChainView` at field tag 3 (`chain_view`). `chain_view.chain_epoch` is the epoch visible after the event, and `chain_view.chain_epoch.settled_tip.height` is the safe tip height that was true for the event. The envelope carries no separate `safe_tip_height` field.
- The cursor is the `StreamCursorTokenV1` bytes documented in [Chain events](chain-events.md). Clients persist the exact bytes returned in the previous envelope and resume strictly after that cursor.
- Empty `from_cursor` returns events from the earliest retained event sequence, which is the bootstrap path for a fresh wallet install.
- The server emits historical events first (replay phase) and then continues with live events in one ordered sequence; clients see no transition.
- Stream end means the consumer disconnected or the server is shutting down. It never means a new block arrived. Clients must distinguish stream end from stream error and reconnect with their last persisted cursor in both cases.
- An expired cursor returns the typed `EventCursorExpired` error and does not silently restart from the current tip.

`ChainCommitted` and `ChainReorged` are the two event variants. `ChainReorged` carries both the reverted range and the replacement range, so a wallet receiving a reorg event truncates its local view at the reverted boundary and resumes from the replacement range without making additional indexer calls. If a client reconnects with a cursor whose branch was reorged out, the server resolves the fork point from the cursor's back-spaced locator against the canonical block index and delivers a `ChainReorged` envelope before resuming, synthesizing it when the real reorg event has aged out of retention. A wallet recovers from a reconnect reorg without a full re-derive and never observes silent branch changes. A divergence deeper than the locator cap degrades to the typed `EventCursorExpired`. See [ADR-0025](../adrs/0025-chain-event-reconnect-reorg-locator.md).

Two cursor varieties are advertised under capability string `wallet.events.chain_v1`: `Tip` and `Safe`. Tip consumers receive every envelope including reorgs. Safe consumers receive only commits past the safe tip and never see `ChainReorged`. The safe cursor family is represented in the cursor body, not by a separate `WalletQuery.ChainEventSafeAnchor` RPC.

The lightwalletd compatibility shim does not expose this subscription. The vendored `CompactTxStreamer` proto has no equivalent method, and ADR-0004 forbids inventing parallel surfaces in the compat layer. Wallet clients on the lightwalletd contract continue to use `GetLatestBlock` polling. Native Zinder clients receive the subscription contract from day one.

## Mempool Snapshot and Subscription

Mempool surfaces are owned by [ADR-0007](../adrs/0007-mempool-topology-and-retention.md), which records the source, live index, event-log, API, compatibility, retention windows, and readiness causes.

The unconfirmed-transaction contract serves several Zcash ecosystem products,
but each product consumes a different boundary. This table is the canonical
product map; reference documents carry the line-numbered source evidence and
observed wallet-run details.

| Ecosystem product | Zinder relationship | What the mempool surface enables | Required boundary |
| ----------------- | ------------------- | ---------- | ----------------- |
| Zallet (`zcash/wallet`) | Primary native Rust consumer | Typed transaction lifecycle, rebroadcast decisions, transparent unmined UTXO updates, and chain-tip notifications separate from mempool stream lifecycle | `zinder-client::ChainIndex` plus native `WalletQuery`; no dependency on the lightwalletd compatibility adapter |
| Zashi/Zodl and Android SDK wallets | lightwalletd-compatible wallet clients | SDK mempool observation, faster pending-send feedback, shielded mempool scanning, and clearer submitted/unmined/resubmitted transaction UX | `zinder-compat-lightwalletd` mapping `GetMempoolStream` and `GetMempoolTx` over the native mempool index and event log |
| Lightwalletd clients and operators | Compatibility consumers | Backend option for clients that speak `CompactTxStreamer`, including mempool methods and transaction submission behavior | Compatibility adapter only; no upstream node calls, no independent storage, and no Zinder-only method extensions in the lightwalletd proto |
| Block explorers and analytics | Application or `zinder-explorer` consumers | Live mempool pages, pending transaction lifecycle, pending transparent address/outpoint overlays, and "mempool in sync" status | Native `WalletQuery` or replayable `zinder-explorer` views; full explorer parity also needs transparent history and balance |
| Zebra | Upstream node source, not a Zinder client | Keeps wallet and explorer indexing outside the node while reusing Zebra's verified mempool observations | `zinder-source` consumes Zebra `MempoolChange` when available, or falls back to `getrawmempool` polling |

Three architectural consequences follow from that map:

- The canonical path is `NodeSource -> MempoolSourceEvent -> MempoolIndex + MempoolEventLog -> WalletQuery -> adapters`. Compatibility methods translate over that path; they do not own their own mempool cache.
- Source observations must become hydrated `MempoolEntry` records before they reach public APIs. Zebra's streaming mempool event carries transaction hash and auth digest, so raw transaction fetching and compact-transaction construction belong in the source/ingest path, not in `zinder-compat-lightwalletd`.
- The native mempool surface does not inherit lightwalletd's stream-close lifecycle. `MempoolEvents` stream end means disconnect or shutdown. Chain-tip changes are delivered through `ChainEvents`.
- The public server-observed type is `MempoolEntry`, not `PendingTransaction`. A pending transaction is a wallet-local UX state: it can include a transaction that was created locally but never accepted by the network.
- Product readiness claims are boundary-specific. Zallet readiness means typed Rust `ChainIndex` coverage in a deterministic harness plus a real Zallet binary/app run. Zashi/Zodl readiness means lightwalletd-compatible methods plus SDK or app validation. Explorer readiness means the mempool surface plus the transparent history and balance surfaces needed for address-oriented views.

The native protocol exposes two complementary mempool methods:

- **`WalletQuery.MempoolSnapshot`** returns a bounded, pageable point-in-time view of the live mempool index, bound to the visible `ChainEpoch` at call time. The response carries `snapshot_age_millis` so clients with strict freshness needs can choose to subscribe to `MempoolEvents` when the age exceeds a threshold. Paging uses the standard opaque, HMAC-authenticated `StreamCursorTokenV1` under its `SnapshotPage` family (offset-49 nibble `0x5`): the next-page `bytes` carry the snapshot sequence and the last yielded transaction id. A tampered cursor returns `SNAPSHOT_PAGE_CURSOR_INVALID`; a cursor for a snapshot sequence newer than the writer currently retains returns `SNAPSHOT_PAGE_CURSOR_EXPIRED`. There is no separate snapshot-cursor codec.
- **`WalletQuery.MempoolEvents`** is a server-streaming subscription that mirrors Zebra's `MempoolChange` semantics: typed `Added`, `Invalidated`, `Mined` envelopes with cursor-resume via the `StreamCursorTokenV1` mempool-event family.

`Invalidated` is not optional. If the polling backend observes a txid disappear
from `getrawmempool` without a corresponding block commit, it emits
`Invalidated { reason: Unknown }` or a more specific reason when the source can
prove one. Silently dropping a txid would make the mempool cache insert-only and
break rebroadcast and pending-transaction views.

Mempool retention is two-tier (60 minutes mined / 24 hours invalidated by default, both configurable). Expired cursors return `MempoolCursorExpired` with `oldest_retained_sequence` in `PreconditionFailure` detail.

### Mempool Point Lookups

`MempoolSnapshot` is the bootstrap and bounded-enumeration surface. It is not
the long-term contract for every non-Rust client that wants a single transaction,
address, or outpoint answer. Native gRPC exposes focused point lookups that
mirror the `ChainIndex` methods:

- `WalletQuery.Transaction` returns the typed `TransactionStatusResponse`
  carrying the shared `TransactionLocation` oneof
  (`mined`/`in_mempool`/`conflicting`, with `NotFound` mapped to gRPC
  `NOT_FOUND`).
- `WalletQuery.TransparentMempoolOutputsByAddress` (capability
  `wallet.mempool.transparent_outputs_by_address_v1`) returns unmined
  transparent outputs that fund one transparent address.
- `WalletQuery.TransparentMempoolSpendsByOutpoint` (capability
  `wallet.mempool.transparent_spends_by_outpoint_v1`) returns the unmined
  spends of a batch of transparent outpoints; outpoints with no unmined
  spend produce no entry.

The mempool live state is in-memory and not chain-epoch-pinnable; both
responses bind to `chain_epoch` visible at lookup time and the requests do not
take an `at_epoch_id` field. Cap rules: `optional max_entries` defaults to a
server constant, and values larger than the server's hard cap are silently
clamped.

These methods read the writer-owned `MempoolIndex` through the `IngestControl`
proxy path settled by [ADR-0007](../adrs/0007-mempool-topology-and-retention.md).
They do not open a second mempool source and do not reconstruct a local live
index inside `zinder-query`. The native adapter parses the public
`AddressLookup` (string or script-hash form) at the public boundary and
forwards a normalized script-hash-only request to `IngestControl`; the
private writer-side handler rejects the address-string arm because the
adapter is the only client.

The transparent-address mempool methods are explicitly transparent-only: the
privacy boundary forbids by-address shielded queries, and clients scan mempool
compact-transaction payloads locally for shielded interest.

`MempoolMinedEvent` carries `transaction_id`, `mined_height`, and `block_hash`.
Block hash is source-driven enrichment: the source backends extract the mined
block hash from the upstream node's observation (Zebra streaming
`MempoolChange::Mined` plus `getrawtransaction`, JSON-RPC polling
`getrawtransaction`'s `blockhash` field) so the orchestrator passes through
authoritative bytes without a chain-store-not-yet-caught-up race. Consumers
that track a pending transaction through mining receive the full mined block
identity in one cursor delivery.

The lightwalletd compat shim maps `GetMempoolStream` and `GetMempoolTx` over
`MempoolEvents` and `MempoolSnapshot` when the adapter is configured with the
mempool surface. Deployments without that surface omit
`wallet.events.mempool_v1` and `wallet.snapshot.mempool_v1` from
`ServerCapabilities` and return a typed unavailable response from the compat
methods.

## Transparent Address Outputs

Transparent-address output queries are not shielded scanning. They reveal
transparent addresses that are already public on chain, but they still must be
served from epoch-bound indexed artifacts. `GetAddressUtxos` and
`GetAddressUtxosStream` map over
`WalletQueryApi::transparent_address_unspent_outputs`,
backed by the canonical transparent address output and transparent spend artifact
families in `zinder-store`.

The compatibility shim must not answer these methods by scanning compact blocks
on demand, calling upstream nodes, returning synthetic empty results for unknown
indexed state, or materializing an unbounded address result before truncating
it. Missing indexed state is a readiness or artifact-availability failure; it
is not a reason to bypass the wallet data plane.

`GetLightdInfo.taddr_support` is `true` only when the adapter reads from stored
transparent output artifacts. It is a product contract for lightwalletd
clients, not a way to silence Android SDK logs.

The native `WalletQuery` proto exposes the same artifact-backed read through
one server-streaming RPC, `TransparentAddressUnspentOutputs`, which streams
`TransparentUnspentOutputsChunk` messages. The server walks the address-output
projection once at a single pinned chain epoch and emits the `ChainView` as one
leading header message (`chunk.body.header`), then one `TransparentUnspentOutput`
per unspent output (`chunk.body.item`); `start_height` is the wallet-birthday
floor. The single header makes the stream-wide single-epoch guarantee
structural: the epoch is carried once, never repeated on every item. There is
no cursor and no entry cap on the wire: a stream that is always complete cannot
be truncated by a client that ignores pagination. The request consumes the shared `AddressLookup` oneof
that accepts either a 32-byte `script_hash` (typed clients) or a base58
transparent `address` (CLI, tests, debug callers); the native adapter parses
string addresses through `ZebraTransparentAddress`, validates the network,
and SHA-256-hashes the `scriptPubKey` before any in-process call. The Rust
`ChainIndex` trait carries the same surface:
`transparent_address_unspent_outputs(query)` keyed by the typed
`TransparentAddressScriptHash`. The in-process `WalletQueryApi` form keeps
the standard `at_epoch_id: Option<ChainEpochId>` pin (the compat shim uses it
to pin multi-address reads to one epoch); the wire request carries no epoch
pin because the server always pins internally and no consumer pins old
epochs. Capability `wallet.address.transparent_unspent_outputs_v1` is
advertised on every deployment that can serve the read.

Server streams hold the materialized unspent set for the stream lifetime,
and a drained stream costs the client memory proportional to the address's
unspent set; acceptable for wallet receivers and documented here for future
consumers.

`TransparentAddressTxIdsInRange` streams `TransparentAddressTxIdsChunk`
messages with the same one-shot header shape: one leading `ChainView` header
(`chunk.body.header`), then one `TransparentAddressTxId` per indexed
transaction (`chunk.body.item`). The chain epoch is carried once on the header,
not on every item.

Cursor cadence on the tx-history streaming surface: the wire emits the resume
cursor only on the terminal item of a server stream. Non-terminal
`TransparentAddressTxId` items carry empty `cursor` bytes; the last item
carries either the next-page cursor (when more entries may be available) or
empty bytes (stream fully drained). Clients that lose the connection mid-stream
must restart the page rather than resume from a non-terminal item; bounded
history pages keep that restart cost small.

Operators must only publish a `zinder-compat-lightwalletd` deployment with
`taddr_support=true` when the store was produced with the wallet-serving
coverage profile (`ingest.coverage = "wallet-serving"` or
`zinder-ingest --wallet-serving`). A recent-checkpoint or tip-bootstrapped
store may have the address-output index family enabled but still lack the historical
rows needed by wallet birthdays and resync anchors; that deployment posture is
not wallet-serving.

`GetAddressUtxos.maxEntries` is an aggregate response budget across the
requested address set. The compatibility adapter may make several internal
artifact reads to satisfy the request, but the public response and stream must
be deterministically ordered and capped as one result set. `maxEntries = 0`
uses the adapter's configured default bound rather than becoming an unbounded
query.

## Transparent Address Tx History

Transaction-history reads return the txids that touch a given transparent
address within an inclusive height range. The native surface is
`WalletQuery.TransparentAddressTxIdsInRange`, a server-streamed page-bounded
read; the matching Rust API is
`ChainIndex::transparent_address_tx_ids_in_range`. The derive projection is a
current read model, so callers use the `chain_view.chain_epoch` returned with
each chunk as the response binding instead of supplying an `at_epoch_id` pin. The
compatibility adapter implements `GetTaddressTxids` and
`GetTaddressTransactions` over the same native method.

Both surfaces are backed by the derive-owned transparent-address transaction
history projection. Canonical ingest writes typed transaction, transparent
output, and transparent spend facts; the derive tailer materializes one row per
`(address, transaction)` pair after the corresponding chain event is durable.
Capability `wallet.address.transparent_history_v1` is advertised only when the
query service can open a derive store that has caught up to the canonical tip.
Cursor-based pagination uses the derive projection's opaque cursor; the
`descending` bit selects newest-first iteration. Cursor cadence: only the
terminal chunk carries a non-empty resume cursor.

The full projection path is the canonical worked example in
[Extending artifacts §A worked example: transparent address transaction history](extending-artifacts.md#a-worked-example-transparent-address-transaction-history).

## Transparent Prevout Resolution

Transparent output resolution turns an `OutPoint` (a `(transaction_id, output_index)` pair) into the `TxOut` that funds the referenced input. Two paired RPCs cover both chain views:

- `WalletQuery.TransparentOutputsByOutpoint(TransparentOutputsByOutpointRequest) returns (TransparentOutputsByOutpointResponse)` resolves outpoints against the canonical chain. Capability `wallet.read.transparent_outputs_by_outpoint_v1`. The handler reads first-class `transparent_output` rows from `zinder-store`; pinned reads verify the row's producing-block identity against the requested epoch.
- `WalletQuery.TransparentMempoolOutputsByOutpoint(TransparentMempoolOutputsByOutpointRequest) returns (TransparentOutputsByOutpointResponse)` resolves outpoints against the writer's live mempool index, sharing the canonical surface's response shape so consumers decode both surfaces through one path. Capability `wallet.mempool.transparent_outputs_by_outpoint_v1`. The handler reads `MempoolEntry.transparent_outputs` directly through `MempoolIndex::transparent_outputs_by_outpoints`; no parsing at read time because the mempool ingest path pre-extracts transparent outputs at admission time. The wallet adapter proxies the call through the `IngestControl` private endpoint since secondary readers cannot observe live writer state.

Every response binds to a `ChainEpoch` (canonical: the read's epoch; mempool: the writer's epoch visible at lookup time), then carries `repeated TransparentOutputEntry entries` in input order. Each entry has the request's `OutPoint` and an `optional TransparentOutput prevout`; absence means the canonical chain at the bound epoch (canonical) or the live mempool index (mempool) does not have the referenced output. The inner `TransparentOutput` carries `value_zat: uint64` and `script_pub_key: bytes`; identifying fields stay on the entry's `outpoint` so the inner payload carries no redundant fields. Duplicate request outpoints emit duplicate entries.

The shared `OutPoint` proto message is the canonical wire-level outpoint shape across every wallet-plane RPC keyed by `(transaction_id, output_index)`. `TransparentMempoolSpendsByOutpoint` and the prevout-resolution surfaces use the same message; future outpoint-keyed RPCs reuse it without inventing parallel shapes. The coinbase sentinel outpoint (`transaction_id = 0x00..00`, `output_index = u32::MAX`) is rejected at the wallet adapter rather than carried as a magic value.

Both methods cap the request at `MAX_TRANSPARENT_OUTPUTS_PER_REQUEST = 1024` outpoints. Requests above the cap are silently truncated to the first 1024 entries. The coinbase sentinel (`transaction_id == [0u8; 32] && output_index == 0xFFFFFFFF`) is rejected with gRPC `INVALID_ARGUMENT` at the wallet adapter; consumers filter coinbase inputs at the request boundary (Zallet's `view_transaction.rs` is the canonical example).

The `ChainIndex` Rust API exposes two methods: `transparent_outputs_by_outpoint(outpoints, at_epoch_id)` and `transparent_mempool_outputs_by_outpoint(outpoints)` (no epoch pin, per the live-state convention). Both return `TransparentOutputsByOutpointResponse`. `LocalChainIndex` reads the canonical method directly from the secondary store; the mempool method delegates to `RemoteChainIndex`. The capability-coverage test asserts both methods exist for any consumer advertising the corresponding capability strings.

The prevout-resolution surface is native-only. `CompactTxStreamer` has no prevout endpoint, and inventing a parallel surface is forbidden by [Service boundaries §Anti-Patterns](service-boundaries.md#anti-patterns).

## Transparent Reverse-Spend Resolution

Reverse-spend resolution is the inverse of prevout resolution: given an `OutPoint`, it returns where that output was spent rather than the output itself. This is the getspentinfo-equivalent surface. Two RPCs cover the canonical and mempool chain views; a full getspentinfo composes both (this RPC for confirmed spends, the mempool RPC for unmined):

- `WalletQuery.TransparentSpendsByOutpoint(TransparentSpendsByOutpointRequest) returns (TransparentSpendsByOutpointResponse)` resolves outpoints to their canonical (confirmed) spend. Capability `wallet.read.transparent_spends_by_outpoint_v1`, always advertised because the canonical spend-fact index is present on every wallet-plane deployment. The handler reads the `transparent_spend_fact` table through the epoch-bound `transparent_spend_facts_by_outpoints` reader; pinned reads (`at_epoch_id`) verify each spend's producing-block visibility against the requested epoch.
- `WalletQuery.TransparentMempoolSpendsByOutpoint(TransparentMempoolSpendsByOutpointRequest) returns (TransparentMempoolSpendsByOutpointResponse)` resolves outpoints to their unmined spend in the writer's live mempool index. Capability `wallet.mempool.transparent_spends_by_outpoint_v1`.

The canonical response binds to a `ChainEpoch`, then carries `repeated TransparentSpend spends`. Each `TransparentSpend` carries the request's `spent_outpoint`, the `spending_transaction_id` (RPC byte order), the `input_index` of the spend within the spending transaction, and a `BlockTip spending_block` (height plus RPC-form hash of the block that mined the spend). The spent output's value and script are intentionally omitted: a consumer wanting them already has `TransparentOutputsByOutpoint`. Outpoints unspent on the canonical chain at the bound epoch produce no entry; consumers key results by `spent_outpoint`. Duplicate request outpoints collapse to one entry. Coinbase inputs spend no prevout and never appear in the spend-fact index; the coinbase sentinel outpoint is rejected with gRPC `INVALID_ARGUMENT` at the wallet adapter and the request is capped at `MAX_TRANSPARENT_OUTPUTS_PER_REQUEST = 1024`, identical to the prevout surface.

The `ChainIndex` Rust API exposes `transparent_spends_by_outpoint(outpoints, at_epoch_id)`, returning `TransparentSpendsByOutpointResponse`. `LocalChainIndex` reads it directly from the secondary store; `RemoteChainIndex` calls the gRPC method. The accessor a consumer uses for confirmed reverse-spend is this single base-trait method; both colocated and cross-process readers serve it and both honor the `at_epoch_id` pin. The reverse-spend surface is native-only for the same reason as prevout resolution.

## Transparent Unspent-Output Probe

The unspent-output probe is the gettxout-equivalent surface: given an `OutPoint`, it returns the referenced output only while that output is unspent on the canonical chain, and nothing if the output has been spent or never existed (null-if-spent). It composes the output resolver and the canonical reverse-spend reader on the server so a transparent-flow explorer issues one round-trip rather than an output lookup, a spend lookup, and a client-side join per outpoint.

- `WalletQuery.TransparentUnspentOutputsByOutpoint(TransparentUnspentOutputsByOutpointRequest) returns (TransparentUnspentOutputsByOutpointResponse)`. Capability `wallet.read.transparent_unspent_outputs_by_outpoint_v1`, always advertised because both the canonical output index and the canonical spend-fact index are present on every wallet-plane deployment.

The handler opens one epoch-bound reader and reads both `transparent_outputs_by_outpoints` and `transparent_spend_facts_by_outpoints` at that single pinned epoch. An outpoint emits an entry only when the output is present and carries no canonical spend at the epoch; spent or never-existed outpoints emit no entry, so every entry's `output` is populated. The response binds to a `ChainEpoch` at field tag 1, then carries `repeated TransparentOutputEntry entries`, the same entry shape as `TransparentOutputsByOutpoint` so consumers share one decoder. Consumers key results by `outpoint`; duplicate request outpoints collapse to one entry. The coinbase sentinel outpoint is rejected with gRPC `INVALID_ARGUMENT` at the wallet adapter and the request is capped at `MAX_TRANSPARENT_OUTPUTS_PER_REQUEST = 1024`, identical to the prevout and reverse-spend surfaces.

The read is canonical-only. The mempool overlay is intentionally absent: a mempool-aware caller composes this canonical probe with `TransparentMempoolSpendsByOutpoint` and subtracts those unmined spends from the result. Keeping the overlay out of this RPC keeps it a clean base-trait read that both colocated and cross-process readers serve and that honors the `at_epoch_id` pin. The `ChainIndex` Rust API exposes `transparent_unspent_outputs_by_outpoint(outpoints, at_epoch_id)`, returning `TransparentUnspentOutputsByOutpointResponse`; `LocalChainIndex` reads it directly from the secondary store and `RemoteChainIndex` calls the gRPC method. The surface is native-only for the same reason as prevout resolution.

## Chain Value Pools

The native surface is `WalletQuery.ChainValuePoolsAtTip(ChainValuePoolsAtTipRequest) returns (ChainValuePoolsAtTipResponse)`. Capability `wallet.read.chain_value_pools_at_tip_v1` is advertised when the query deployment can proxy to an ingest writer whose source probe reported `chain_value_pools`.

The response binds to the writer-visible `ChainEpoch`, carries the upstream tip height used by `getblockchaininfo`, and preserves `repeated ChainValuePool pools` in upstream order. Each pool entry carries `id`, `monitored`, and optional `chain_value_zat`. The list-shaped contract is intentional: consumers can render known pools by id without forcing Zinder to drop or rename future consensus pools.

`zinder-query` does not open an upstream-node connection for this method. It proxies through `IngestControl.ChainValuePoolsAtTip`, because the ingest writer owns the source handle and the source capability snapshot. Storage-only or unproxied query deployments reject the call with `UNAVAILABLE`; writers whose source lacks `chain_value_pools` reject with `FAILED_PRECONDITION`.

## Transparent Address Balance

The native surface is `WalletQuery.TransparentAddressBalance(TransparentAddressBalanceRequest) returns (TransparentAddressBalanceResponse)`. The request carries `repeated AddressLookup addresses` and an `optional uint64 at_epoch_id`. The response carries the binding `chain_view` at field tag 1, then `confirmed_zat: uint64`, `unconfirmed_delta_zat: int64`, and `address_count: uint32`.

The balance is served in the wallet plane (`zinder-query`) and advertises one capability: `wallet.address.transparent_balance_v1`, always on. The canonical unspent-output index it sums is present on every wallet-plane deployment, so the capability never gates on a separate plane. The handler sums the confirmed total in-process from the canonical unspent-output index (a saturating sum) pinned to `at_epoch_id`; absent, the read resolves against the visible tip. It then overlays the signed mempool delta by reading the live mempool through the colocated `IngestControl` endpoint: mempool outputs paid to the addresses are pending inflows, and mempool spends of the addresses' confirmed unspent set are pending outflows. `unconfirmed_delta_zat` is pending inflows minus pending outflows. When no ingest-control endpoint is wired, the delta is zero and the call still succeeds.

The address list is capped at 256 per request (`MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES`), enforced in the wallet/native layer. An over-cap list is rejected with `INVALID_ARGUMENT` carrying `TRANSPARENT_BALANCE_ADDRESS_COUNT_EXCEEDED`; an empty list is rejected with `INVALID_ARGUMENT`. The signed delta saturates to the `int64` range; `confirmed_zat` is a `uint64`.

The mempool live state is not chain-epoch-pinnable. `at_epoch_id` pins only the canonical confirmed read; the mempool overlay always reads live state, and the response binds to the chain epoch the confirmed read answered against. Historical balance at an arbitrary height is out of scope.

The lightwalletd compat shim answers `GetTaddressBalance` and `GetTaddressBalanceStream` by calling the wallet primitive and projecting it into one `int64 value_zat` (confirmed total minus pending outflows, saturating to zero, capped at `i64::MAX`); the lightwalletd `Balance { value_zat: int64 }` proto carries no overlay slot. `GetTaddressBalanceStream` is a per-address loop over the unary form for compatibility clients and exists only for the lightwalletd contract.

The `ChainIndex` Rust API exposes `transparent_address_balance(addresses)` returning `TransparentAddressBalance`. `LocalChainIndex` and `RemoteChainIndex` both implement it; the capability-coverage test asserts the method exists for the `wallet.address.transparent_balance_v1` capability.

## Capability Discovery

`WalletQuery.ServerInfo` returns a `ServerCapabilities` descriptor per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery). Capability strings are exact-match; clients gate features on capability strings such as `wallet.events.chain_v1` rather than on Zinder version. New methods land with new capability strings; deprecated capabilities continue to be advertised alongside their replacement until the documented removal version. The descriptor's `node` field is reserved for upstream-node capability snapshots, but storage-only `zinder-query` deployments leave it empty unless a runtime handoff supplies source probe results.

## In-Process Rust API

In-process consumers (Zallet, future SDK integrations) call `zinder-client` per [Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape). The `ChainIndex` trait exposes the same methods the gRPC service does, with typed Rust types (`BlockHeight`, `ChainEpoch`, `TxStatus`, `TransactionBroadcastResult`, `IndexerError`). No tonic round-trip; no untyped error strings; no `unreachable!()` guards required.

The `ChainIndex` trait does not duplicate the `WalletQueryApi` Rust trait inside `services/zinder-query`. They serve different consumer profiles: `WalletQueryApi` is the gRPC server's internal trait; `ChainIndex` is the published Rust API for in-process consumers. Both share types from `zinder-core`. A compatibility test asserts that every advertised `ServerCapabilities` capability string has a corresponding `ChainIndex` method.

## Transaction Broadcast

Transaction broadcast is a network operation, not a canonical chain commit.

`zinder-query` may expose transaction broadcast because wallets expect a single endpoint. This is wallet operation parity, not part of the minimum read-only shielded sync surface. The broadcast path must:

- Forward the raw transaction to a configured network or upstream node path.
- Advertise `wallet.broadcast.transaction_v1` only after the configured source probe reports `transaction_broadcast`.
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
claims. Per [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md),
the contract is consumer-neutral: client-specific validation belongs in test
evidence, while this document owns the durable serving criteria.

A deployment may claim Android SDK or Zashi compatibility only when:

- The serving store contains the subtree-root history required by fresh wallet
  bootstrap and tree-state history for every anchor height a supported wallet
  flow can request, including create, resync, and restore/import flows. Use
  `zinder-ingest --wallet-serving` (or `ingest.coverage = "wallet-serving"`)
  for this serving profile; recent checkpoints are validation fixtures, not
  wallet-serving stores.
- The transparent output surface in [§Transparent Address Outputs](#transparent-address-outputs)
  is implemented end-to-end.
- The transport requirement in [Service operations](service-operations.md#deployment-guidance)
  is satisfied for real Zashi endpoint tests.

The upstream Go `lightwalletd/testclient` remains smoke coverage for the basic
compat surface. It is not a substitute for an Android SDK or Zashi bootstrap
test when the release claim names those clients.

A deployment may claim Android SDK or Zashi/Zodl mempool compatibility only
when `GetMempoolStream` and `GetMempoolTx` are mapped over the native mempool
index and event log, and an SDK or app flow has observed mempool transactions
against that endpoint. A sync-only Zashi proof does not establish
pending-transaction UX.

## Query Consistency

Wallet sync APIs must read from one `ChainEpoch`. Primary in-process reads may also be backed by one RocksDB snapshot; secondary reads are snapshotless and rely on epoch-bound visibility retention per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md).

For example, a compact block range response should bind:

- Start height.
- End height.
- Tip hash.
- Tip height.
- Safe tip height.
- Artifact schema version.
- Subtree-root range or cursor when subtree data is returned.

If the chain tip changes while the request is executing, the response should still finish from the epoch it started with or restart from a new epoch. It should not mix both.

## Performance and Pagination

`lightwalletd` compatibility is the floor for wallet-sync performance, not the target. Zinder publishes P50, P99, and worst-case budgets for hot wallet endpoints and gates `GetBlockRange`-equivalent behavior in CI.

### Published Budgets (calibration target)

The first published numbers are calibration targets, not strict limits. They will promote to strict CI gates after two consecutive releases stabilize them.

The "regtest baseline" column records observed times from `services/zinder-ingest/tests/live/latency.rs::read_endpoint_latency_baseline` against a live Zebra regtest with 101 coinbase-only blocks. These are sanity-floor numbers; mainnet blocks carry real shielded payloads.

The endpoint-specific mainnet columns record observed times from
`services/zinder-ingest/tests/live/bulk_catchup.rs::bulk_catchup_last_1000_blocks_from_checkpoint`
after bulk catching up the last 1000 mainnet blocks from a checkpoint at
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
| `tree_state_checkpoint_at_or_before` | one height | ~97 µs | ~58 µs | ~69 µs | ~69 µs |
| `subtree_roots` | 1..=8 entries from checkpoint | n/a | ~11 µs | ~12 µs | ~12 µs |

The mainnet `subtree_roots` row times the checkpoint-bootstrapped read shape: querying from `start_index = checkpoint_completed_subtree_count` with `max_entries = 8`. Subtree roots completed before the checkpoint are not in the store; operators must seed them out-of-band if a wallet needs them.

The report-based mainnet baseline below comes from
`scripts/observability-smoke.sh calibrate` against a synced local mainnet Zebra
on 2026-04-28. It verifies checkpoint bulk catchup, checkpoint backup restore,
native wallet gRPC, lightwalletd-compatible gRPC, readiness gauges, source
RPC metrics, store-read metrics, RocksDB property gauges, and Prometheus alert
rule loading. The current sample count is intentionally small because it proves
the harness and captures a release-readiness anchor; release signoff should run
the same command with at least 6 samples.

| Operational metric | Mainnet P50 (n=2) | Mainnet P99 (n=2, max-of-sample) | Mainnet worst-case (n=2) |
| ------------------ | ----------------- | -------------------------------- | ------------------------ |
| `bulk_catchup_seconds` | 13.204 s | 13.243 s | 13.243 s |
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
bounded transparent output artifact read, never from an unbounded in-memory
address result.

Do not materialize an unbounded list and truncate it after the fact. The storage and protocol signatures should make that shape impossible.
