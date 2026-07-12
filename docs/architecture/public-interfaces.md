# Public Interfaces

This document is the vocabulary spine. Every other architecture doc, ADR, ingest path, query path, error variant, and configuration field defers to the conventions here. When you add a new public type, method, error, config field, or proto message, the reviewer's first question is "does this match the spine?"

Zinder's public interfaces should be boring, searchable, and hard to misuse. The names chosen here will be copied by contributors, downstream wallets, operators, and code-extending agents.

Optimization order:

1. **Developer Experience (DX).** A wallet, explorer, or application developer integrating Zinder can find any capability through search and complete an integration without reading internals.
2. **Agent Experience (AX).** An LLM coding agent extending Zinder can place new code in the right module, guess the right name on first try, extend an existing pattern by example, and discover capabilities at runtime.
3. **User Experience (UX).** An operator can read `--print-config`, `/readyz`, `/metrics`, or a `ServerInfo` response and know what to do without reading source.
4. **Contributor experience.** A new contributor finds checklists for common operations (adding an artifact family, RPC method, or error variant) without tribal knowledge. The cookbook lives at [Extending Artifacts](extending-artifacts.md).

## Vocabulary

Use these names consistently across modules, RPCs, errors, and configuration.

### Chain-view envelope and the `{role}_tip` taxonomy

Every `WalletQuery`, `ExplorerQuery`, and `IngestControl` read response carries `ChainView chain_view = 1` as its first field (on `ExplorerQuery` it rides one level down, on `ExplorerFreshness.chain_view`). Consumers read chain state the same way on every surface through `response.chain_view`. Each plane fills the subset it owns: wallet responses fill `chain_view.chain_epoch` and leave the derive-plane axes unset; explorer and ingest-control responses fill the axes their plane owns. The chain-view family (`ChainView`, `ChainEpoch`, `BlockTip`, `IndexedTip`, `UpstreamTip`, `DeriveStatus`) is defined in `wallet.proto`. See [ADR-0011](../adrs/0011-explorer-freshness-envelope.md).

The four chain heights share one naming axis so the reorg-vs-replay distinction is self-evident:

| Role | Field | Meaning |
|------|-------|---------|
| `visible_tip` | `ChainEpoch.visible_tip` | Best visible block in the epoch. |
| `settled_tip` | `ChainEpoch.settled_tip` | Reorg-window ceiling and the wallet scan ceiling. Keeps the exact reorg-window semantics the former `safe_tip` fields carried. |
| `indexed_tip` | `ChainView.indexed_tip` | Derive-replay ceiling. Absent means "unknown", never "at tip". |
| `upstream_tip` | `ChainView.upstream_tip` | The upstream node's view (heights only; the probe has no single hash). |

"Finalized" stays forbidden as a tip name (it collides with NU7/Crosslink). Index lag is `chain_view.chain_epoch.visible_tip.height - chain_view.indexed_tip.tip.height`.

### Product and runtimes

| Term | Meaning |
|------|---------|
| `Zinder` | The product |
| `zinder-ingest` | Production service that owns chain ingestion and canonical writes |
| `zinder-query` | Production service that serves wallet and application APIs from epoch-bound indexed state |
| `zinder-compat-lightwalletd` | Compatibility adapter that serves vendored lightwalletd gRPC over `WalletQueryApi` |
| `zinder-explorer` | Optional service for explorer-shaped reads and replayable derived indexes ([ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md)) |
| `zinder-client` | Library crate exporting the typed Rust client surface for Rust integrations |
| `PrimaryChainStore` | `zinder-store` handle that opens canonical RocksDB as the only writer |
| `SecondaryChainStore` | `zinder-store` handle that opens canonical RocksDB as a RocksDB secondary reader |

### Domain types

| Term | Meaning |
|------|---------|
| `ChainView` | Cross-plane chain-state envelope carried at field tag 1 on every `WalletQuery`, `ExplorerQuery`, and `IngestControl` read response. Folds the chain epoch and the `{role}_tip` axes (`indexed_tip`, `upstream_tip`) plus the derive status into one shape. Defined in `wallet.proto`. See [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). |
| `ChainEpoch` | A consistent visible chain snapshot. Carries `visible_tip` and `settled_tip` as `BlockTip` values plus the epoch id, network name, artifact schema version, and the visible-tip commitment-tree sizes. Response-only: it appears nested in `ChainView`, never in a request. A request that pins a snapshot carries the bare `optional uint64 at_epoch_id` instead. |
| `BlockTip` | One named chain height with its block hash (`{ height, hash }`, hash in RPC byte order). Reused for `visible_tip`, `settled_tip`, and `indexed_tip`. |
| `IndexedTip` | Derive-replay ceiling: the highest block the derive projections have materialized (`{ tip: BlockTip, block_time_unix_seconds }`). Absent on `ChainView` means "derive head unknown", never "at tip". |
| `UpstreamTip` | The upstream node's view of the chain (`{ committed_height, estimated_height }`, heights only; the probe has no single hash). Absent means the source-plane probe has not fired yet. |
| `ChainEpochReader` | In-process read view pinned to one `ChainEpoch` |
| `ChainEpochReadApi` | Internal read API for epoch-bound canonical reads |
| `ChainEvent` | Post-commit canonical transition emitted by `zinder-ingest` |
| `ChainEventEnvelope` | Cursor-bound chain-event message carried over the ingest subscription plane and exposed natively on `WalletQuery.ChainEvents` |
| `ChainTipMetadata` | Chain-derived counters at the visible tip (Sapling, Orchard, and Ironwood tree sizes) |
| `BlockHeaderArtifact` | Durable typed block-header fact row |
| `BlockBlobArtifact` | Optional raw block blob available only when raw blob policy stores blocks |
| `CompactBlockArtifact` | Wallet-oriented compact block artifact |
| `BlockId` | Stable block identity (`{ height: BlockHeight, hash: BlockHash }`); lives in `zinder-core` and is the canonical (height, hash) pair across the source boundary, the wallet protocol, and the reader API |
| `ChainValuePools` | Live-source value-pool totals paired with `source_tip: BlockId`; Zebra supplies the height, hash, and pool list in one `getblockchaininfo` observation |
| `ChainValuePoolsAtTip` | Wallet-facing value-pool totals paired with both the writer-visible `ChainEpoch` and the hash-bound source tip so consumers can verify canonical agreement |
| `TransactionLocation` | Durable transaction id to block-location fact |
| `TransactionFactsArtifact` | Durable typed public transaction facts parsed once by ingest |
| `ReorgWindow` | Range within the reorg window where reorgs are expected and supported |
| `MempoolEntry` | One transaction currently observed in the mempool |
| `MempoolEvent` | Typed mempool transition (`Added`, `Invalidated`, `Mined`) carried in the event log |
| `MempoolEventEnvelope` | Cursor-bound mempool-event message exposed natively on `WalletQuery.MempoolEvents` |
| `MempoolSnapshotView` | Bounded, pageable point-in-time projection of the live mempool with `snapshot_age_millis` |
| `TransparentOutPoint` | Canonical transparent output identifier: transaction id plus output index |
| `TransparentOutputArtifact` | Durable mined transparent-output row carrying value, `script_pub_key`, address script hash, and producing block identity; stored in `transparent_output`, with its outpoint referenced by the block-local repair index |
| `TransparentMempoolOutputsRequest` | Bounded transparent-address request for outputs currently visible in the mempool index |
| `TransparentMempoolOutput` | Transparent output currently visible in the mempool index |
| `TransparentMempoolSpend` | Transparent outpoint-spend relationship currently visible in the mempool index |
| `NetworkUpgradeActivations` | Node-discovered consensus upgrade table (branch id, activation height, name per upgrade) carried as `Arc<NetworkUpgradeActivations>` from process startup. Source of truth for `consensus_branch_id_at`, `active_at`, and Sapling activation height across the compat shim, native query API, and signer testkit. Required at construction by every consumer: see [ADR-0008](../adrs/0008-network-parameter-discovery.md). |
| `NetworkUpgradeActivation` | One entry in a `NetworkUpgradeActivations` table: `{ branch_id: u32, activation_height: BlockHeight, name: String }`, name carried verbatim from `getblockchaininfo.upgrades` |

### Source Boundary

| Term | Meaning |
|------|---------|
| `NodeSource` | Rust trait for configured source adapters in `zinder-source` |
| `NodeCapabilities` | Capability descriptor detected from the selected source |
| `NodeAuth` | Typed source authentication configuration |
| `MempoolSourceEvent` | Source-level mempool observation normalized from source streams or polling diffs |
| `ChainTipNotification` | Source-level chain-tip wake-up payload normalized from Zebra indexer streams |
| `ChainTipNotificationSource` | Source boundary that opens chain-tip notification streams. Consumers treat it as a wake-up source and keep JSON-RPC polling as the canonical catch-up path |
| `TransactionBroadcaster` | Source-backed transaction broadcast boundary implemented by source adapters |
| `TransactionBroadcastResult` | Typed accepted, duplicate, invalid-encoding, queued, rejected, or unknown broadcast outcome (see [ADR-0023](../adrs/0023-typed-broadcast-rejection-reasons.md)) |
| `BroadcastRejectionReason` | Typed rejection reason on `BroadcastRejected`: `InvalidSignature`, `BadExpiryHeight`, `BadConsensusBranch`, `MempoolFull`, `Unknown` |
| `RawTransactionBytes` | Raw serialized transaction bytes submitted by a wallet |

### Wallet protocol surface

| Term | Meaning |
|------|---------|
| `WalletQuery` | Native protobuf service for wallet and application reads from epoch-bound Zinder data |
| `WalletQueryApi` | Rust query boundary used by `zinder-query` and compatibility adapters |
| `WalletQueryGrpcAdapter` | Tonic adapter that serves native `WalletQuery` over `WalletQueryApi` through `grpc/native.rs` response builders |
| `LatestBlockResponse` | Native wallet protocol response for latest visible block metadata |
| `CompactBlocksInRangeChunk` | Native wallet protocol stream item for one compact block bound to one chain epoch |
| `FullBlocksInRangeChunk` | Native wallet protocol stream item for one serialized full block bound to one chain epoch |
| `TransactionRequest` | Native wallet protocol request for one transaction id. Without `at_epoch_id`, `WalletQuery.Transaction` resolves canonical state first and then the writer's live mempool index; a pinned request is canonical-only. |
| `TransactionStatusResponse` | Native wallet protocol response carrying one typed `mined`, `in_mempool`, or `conflicting` location. A miss is gRPC `NOT_FOUND`. |
| `TreeStateResponse` | Native wallet protocol response for one commitment tree-state artifact |
| `SubtreeRootsResponse` | Native wallet protocol response for Sapling or Orchard subtree roots |
| `BroadcastTransactionRequest` | Native wallet protocol request to submit a raw transaction |
| `BroadcastTransactionResponse` | Native wallet protocol typed broadcast outcome |
| `ChainEventsRequest` | Native wallet protocol request for `WalletQuery.ChainEvents` chain-event subscription |
| `MempoolEventsRequest` | Native wallet protocol request for `WalletQuery.MempoolEvents` mempool-event subscription |
| `MempoolSnapshotRequest` | Native wallet protocol request for `WalletQuery.MempoolSnapshot` |
| `MempoolSnapshotResponse` | Native wallet protocol response carrying the live mempool view |
| `ServerInfoRequest` | Native wallet protocol capability-descriptor request |
| `ServerInfoResponse` | Native wallet protocol capability-descriptor response |
| `ServerCapabilities` | Capability descriptor advertised by `zinder-query` to clients |

### Explorer plane

| Term | Meaning |
|------|---------|
| `ExplorerQuery` | Native protobuf service for explorer-shaped reads served by `zinder-explorer`. See [Explorer plane](explorer-plane.md) and [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md). |
| `ExplorerQueryGrpcAdapter` | Tonic adapter for `ExplorerQuery`; carries the optional `WalletQuery` endpoint that backs its wallet-composed reads (transaction detail, block views, search, mempool activity, value pools) |
| `TransactionPublicFacts` | Single typed transaction-fact value parsed once at ingest/mempool/explorer-read time. See [ADR-0010](../adrs/0010-transaction-public-facts.md). |
| `ExplorerFreshness` | Explorer response envelope at field tag 1. Wraps the cross-plane `ChainView` (chain-state axes) and keeps only the metadata that varies per explorer call: `snapshot_age_millis`, `unavailable[]`, and `capability_version`. The upstream tip rides on `chain_view.upstream_tip`. See [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). |
| `SearchCandidate` | Typed search-result oneof distinguishing every classifiable input class, including the `NotPubliclyIndexable` arm for shielded receivers. See [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). |
| `ChainReorgHistory` | Explorer RPC returning recorded reorg incidents from the `ReorgIncidentsConsumer` derive projection. The projection starts from the earliest retained chain event when the consumer first runs and then keeps future incidents beyond chain-event retention; it does not reconstruct incidents already pruned before deployment. |
| `DisplacedBlockArchive` | Writer-owned append-only archive of blocks displaced by an accepted canonical replacement. Capture is atomic with `ReorgWindowChange::Replace`; hash is identity, event/height is observation order, and explicit activation coverage prevents claims about earlier reorgs. |
| `DisplacedBlockHistory` | Explorer RPC returning bounded newest-first displaced-block observations plus each block's current canonical counterpart at the former height. It exposes raw payout scripts and values without product-specific address labels or miner branding. |
| `DisplacedBlockDetail` | Explorer hash lookup for one displaced block, its current canonical counterpart, optional already-retained consensus bytes, and archive activation coverage. |
| `BlockFinalNoteCommitmentRoots` | Typed canonical artifact containing the post-block Sapling, Orchard, and Ironwood note-commitment-tree roots. Pool fields are optional before activation; an absent artifact means enrichment has not reached that height. A store persisted below artifact schema 17 is refused at open and rebuilt from genesis. |
| `TransactionIntrinsicValueBalances` | Signed Sprout, Sapling, Orchard, and Ironwood value balances parsed from one transaction. Positive values enter the transaction from the named pool; negative values leave it for that pool. Transparent value is excluded because it requires prevout resolution. The value-pool flow-history projection reads one such row per retained transparent-participating transaction, so a store persisted below artifact schema 17 is refused at open and rebuilt from genesis. |
| `CommitmentRootSearch` | Explorer RPC that reverse-indexes canonical final note-commitment roots and returns explicit historical coverage. It does not claim transaction-intermediate anchors or orphaned blocks. |
| `TransactionHistory` | Explorer RPC returning bounded, filter-aware, newest-first canonical transaction pages. Version 2 adds a projection read fence, verified contiguous coverage, and explicit count scope without replacing the v1 RPC or entry fields. |
| `TransactionHistoryReadFence` | Exact identity of one history projection view: canonical chain epoch, projection revision, and projection tip height and hash. Requests and opaque cursors carrying a stale fence fail closed. |
| `TransactionHistoryCoverage` | Verified contiguous height interval for the history projection. Full-history completeness requires height 1 through the fenced projection tip with the same tip hash. |

### Derive-plane SDK

| Term | Meaning |
|------|---------|
| `DeriveStore` | Projection `RocksDB` wrapper opened as primary by `zinder-ingest` and as secondary by reader gateways. Owns the `chain_event_cursor`, `mempool_event_cursor`, and `consumer_metadata` column families. |
| `DeriveStoreTable` | Logical column-family identifier referenced by reads and `WriteBatch` puts |
| `DeriveConsumerName` | Stable static identifier scoping cursor and metadata rows; renaming is a schema migration, not a config change |
| `DeriveConsumer` | Rust trait every chain-events derive consumer implements: dispatches `ChainCommittedEvent` and `ChainReorgedEvent` through `apply_chain_committed` / `apply_chain_reorged` |
| `ReorgIncidentsConsumer` | Event-only chain-event derive consumer that writes one durable `reorg_incidents` row per `ChainReorged` event, keyed by ascending `event_sequence`. It does not hydrate committed block contexts and has an event-only cursor independent from block-keyed derive replay. |
| `CommitmentRootSearchConsumer` | Block-keyed derive consumer that maps each canonical final note-commitment root to newest-first block matches. Its resumable historical coverage is independent from the shared chain-event cursor because ingest first enriches the canonical artifact and then writes bounded backfill batches. |
| `ConsumerProjectionState` | Per-consumer canonical epoch, projection tip, monotonic revision, and optional contiguous coverage. Block-keyed consumers stage it atomically with projection rows and their chain-event cursor. |
| `DeriveStoreReadSnapshot` | One-sequence view used for projection metadata, rows, joins, and exact counts. A secondary snapshot holds the read side of the catch-up barrier until all request-local reads finish. |
| `DeriveMempoolConsumer` | Rust trait for consumers that observe `MempoolEvents` instead of (or in addition to) chain events |
| `DeriveConsumerCtx` | Per-event consumer context carrying a `&DeriveStore` borrow for reads and a `&mut WriteBatch` consumers stage their writes into; the SDK appends the cursor advance to the same batch and commits atomically |
| `ChainCommittedEvent`, `ChainReorgedEvent` | Typed wrappers around the wire chain-event variants, decoded into `zinder-core` primitives so consumers never see prost-generated types |
| `MempoolConsumerEvent` | Typed wrapper around one `MempoolEventEnvelope`, carrying borrowed transaction-id and raw-transaction-bytes slices for the duration of the apply call |
| `DeriveStore::write_chain_event` | Ingest-hosted dispatcher that applies `BlockKeyedConsumer` implementations and persists the chain-event cursor atomically with consumer writes |
| `DeriveStore::write_chain_event_chunk_with_event_consumers` | Ingest-hosted dispatcher for event-only `DeriveConsumer` implementations, with optional block-keyed consumers for rare mixed cases. Use it when a projection observes the chain event itself rather than each committed block. |
| `DeriveStore::write_mempool_event` | Ingest-hosted dispatcher that applies a `DeriveMempoolConsumer` and persists the mempool-event cursor atomically with consumer writes |

### Cursors, events, errors

| Term | Meaning |
|------|---------|
| `StreamCursorTokenV1` | The single opaque, HMAC-authenticated cursor envelope for every resumable read: chain-event subscriptions (fork-aware locator), mempool-event subscriptions, transparent-history paging, address-output paging, and `MempoolSnapshot` paging. The family nibble at byte offset 49 selects the body shape |
| `ChainEventStreamFamily` | Stream-family enum used inside chain-event cursor bodies (`Tip`, `Safe`; `Mempool` is a reserved family code, not an active chain-event family) |
| `ArtifactFamily` | Open-ended enum naming an artifact family in storage and query errors |
| `ArtifactKey` | Open-ended enum union of keys used to look up an artifact (`BlockHeight`, `TransactionId`, `SubtreeRootIndex`, `BlockTransactionIndex`, future variants) |

### Configuration

| Term | Meaning |
|------|---------|
| `RetentionConfig` | Service-specific configuration for chain-event and mempool-event retention windows |
| `secondary_path` | Process-unique RocksDB secondary metadata directory for a colocated reader |
| `ingest_control_addr` | Private ingest-control gRPC endpoint used by secondary readers to compute replica lag and proxy chain-event subscriptions |
| `IngestPhase` | The unified ingest loop's classifier output (`AwaitingUpstream`, `BulkCatchup`, `TipFollow`); orthogonal to readiness `cause`. See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md). |

### Avoid

- `Service` as a standalone Rust type, trait, crate, or module name. `Service` remains acceptable when it means a deployable runtime.
- `Manager`, `Processor`, `Handler`, `Helper`, and `Util`.
- `common`, `shared`, `misc`, or `utils` crates or modules.
- `wallet service` for a service that does not custody keys.
- `block splitter` as a public name.
- `zinder-serve` as a crate or deployable boundary. Use `zinder-query`, `zinder-ingest`, or `zinder-compat-lightwalletd`.
- `WalletApi` for the native proto service. Use `WalletQuery` because the service is query-scoped and epoch-bound.
- `IndexerError::Other`, `Other`, or any catch-all error variant in a public boundary.
- `BlockMetadata` for a height-and-hash type. Use `BlockId`.
- `PendingTransaction` for a server-observed mempool record. Use
  `MempoolEntry`; pending transaction is a wallet-local outbound UX state.
- `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value` as identifier names. The lint baseline already enforces this.

## Method Naming Conventions

Method names on public traits, types, and gRPC services follow a small set of rules so that an agent or contributor adding a new capability can guess the correct shape on first try.

### Rule 1 — Single-key lookups

Methods that look up exactly one artifact by exactly one key:

- For an exact `BlockHeight` key, use `{artifact}_at(height)`. Example: `block_at(height)`, `compact_block_at(height)`, `tree_state_at(height)`.
- For internal store-layer checkpoint-floor reads, use `{artifact}_checkpoint_at_or_before(max_height)`. Example: the store primitive `tree_state_checkpoint_at_or_before(max_height)` that backs the public `tree_state_at` read.
- For any other unique key, use `{artifact}_by_{key_noun}(key)`. Example: `transaction_by_id(txid)`, `block_by_hash(hash)`.

The `_at` suffix is reserved for height; using `_at` for any non-height key is a convention violation.

### Rule 2 — Bounded range reads

Methods that read a contiguous range of artifacts, returning a stream or vector:

- Always plural, always `_in_range` suffix. Example: `compact_blocks_in_range(range)`, `subtree_roots_in_range(range)`, `transactions_in_range(range)`.
- The argument is always a `RangeInclusive<BlockHeight>` or a domain range type (`SubtreeRootRange`).

### Rule 3 — Tip-pinned reads with no key

Methods that return the artifact at the visible tip with no caller-supplied key:

- `latest_{artifact}()`. Example: `latest_block()`, `latest_tree_state_checkpoint()`.

### Rule 4 — Stream subscriptions

Methods that return a server-streaming subscription with cursor resume:

- `{event_kind}_events(start)` returning `impl Stream<Item = Result<{EventKind}Envelope, _>>`.
- Example: `chain_events(start)`, `mempool_events(start)`, future `derive_events(start)`.
- `start` is the required `EventStreamStart` position (`after_cursor` | `earliest_retained` | `live_tail`, [ADR-0027](../adrs/0027-event-stream-start-positions.md)); an unset position is `INVALID_ARGUMENT`. Cursor-paged history reads keep `from_cursor` (see Cursor Conventions below).
- The envelope field carrying the cursor position is always `cursor`.

### Rule 4a — Current-projection streams use a one-shot header

A read that walks a single pinned chain epoch once and streams the whole result set (no cursor, no entry cap) carries the `ChainView` as one leading header message, never repeated on every item. The chunk message is a `oneof body { ChainView header = 1; <Item> item = 2; }`. The server sends exactly one `header` message first, then one `item` per element. This makes the stream-wide single-epoch guarantee structural rather than a repeated field: a consumer reads the epoch once from the header and binds every later item to it.

`TransparentAddressUnspentOutputs` (`TransparentUnspentOutputsChunk`) and `TransparentAddressTxIdsInRange` (`TransparentAddressTxIdsChunk`) follow this shape. The header carries the one pinned epoch; it does not add per-element epoch pins, client cursors, or page sizes to these streams. Cursor-resumed event subscriptions (Rule 4) keep their per-envelope `chain_view` because each envelope is independently resumable.

### Rule 5 — Capability and identity probes

Methods that ask the server about itself rather than about chain data:

- `server_info()` returning `ServerCapabilities`.
- `tip_id()` returning the visible tip identity as `BlockId { height, hash }`. (Lifted from `NodeSource` for symmetry with the read API; see the lazy-catchup short-circuit in [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md).)

### Rule 6 — Verb forms inside `zinder-source`

The source boundary uses `fetch_*` for outbound calls because the result is a remote observation, not a local read. `fetch_block_at(height)` follows Rule 1 unchanged; `stream_block_range(range, options)` is the bulk-catchup counterpart per [ADR-0016](../adrs/0016-source-streaming-pipeline.md); `tip_id()` follows Rule 5 and uses no `fetch_` prefix because the noun-named accessor matches the local-API symmetry. Earlier source-boundary spellings (`fetch_block_by_height`) are retired so the boundary stops carrying redundant `_by_<key>` qualifiers when every method on the trait is keyed by chain position.

### Forbidden mixed forms

Do not mix these in the same crate:

- `get_*` and `fetch_*` and bare-noun (`block`, `transaction`) all referring to the same operation. Pick one per boundary.
- `_at` for non-height keys (e.g. `transaction_at`).
- Singular range methods (`block_in_range`).
- Cursor-bearing methods named `subscribe_*` instead of `*_events`.

### Wire-RPC naming rule (native `zinder.v1`)

Native gRPC RPC names are the `PascalCase` spelling of the Rust method the rules above produce, so a consumer derives the wire name from the Rust method and vice versa. The native surface is `zinder.v1` only; the vendored lightwalletd compatibility proto is frozen (ADR-0024) and does not follow this rule.

- A bounded range read (Rule 2) is plural with the `InRange` suffix on both the RPC and its messages: `CompactBlocksInRange(CompactBlocksInRangeRequest) returns (stream CompactBlocksInRangeChunk)`, `TransparentAddressTxIdsInRange`. The Rust counterpart is `compact_blocks_in_range`.
- A request message is named for what it carries, not for the one RPC that first used it. `BlockSelectorRequest` carries a `BlockSelector` and backs both `BlockIdBySelector` and `BlockHeaderBySelector`; neither RPC name appears in the request name.
- A transaction-location discriminator is the shared `TransactionLocation` oneof `{ mined, in_mempool, conflicting }`. Every read surface that answers "where does this transaction live" embeds that one message (`WalletQuery.Transaction`, `ExplorerQuery.TransactionDetail`) so a consumer writes one match shape.

Renames applied to converge the divergent names this rule exposed: the range RPC `CompactBlockRange` and its `*Request`/`*Chunk` messages became the `CompactBlocksInRange` form; the shared `BlockIdBySelectorRequest` became `BlockSelectorRequest`. Single-key and tip-pinned reads (`CompactBlock`, `LatestBlock`, `BlockIdBySelector`) already matched their rules and were left unchanged; capability strings are unchanged because a method rename does not alter the semantic response shape.

## Cursor Conventions

Cursors are opaque to clients, fork-aware on the server, and authenticated where applicable.

### Body shape

`StreamCursorTokenV1` (chain events) is the canonical cursor body. It is a fixed-prefix byte layout, base64url over the wire, never parsed by clients. The body fields are:

- schema version (1 byte) and target network id (4 bytes).
- `event_sequence`: monotonic per-store sequence (8 bytes), also surfaced on the envelope for diagnostics.
- a fork-aware locator: a tip-first, exponentially back-spaced set of `(height, hash)` pairs. The tip pair occupies the fixed body (4-byte height, 32-byte hash); a one-byte count and the back-spaced ancestor pairs follow. The locator carries at least the tip and at most `CHAIN_EVENT_LOCATOR_MAX = 32` entries.
- `family`: the `ChainEventStreamFamily` nibble at byte offset 49 (`Tip`, `Safe`; `Mempool` is a reserved family code).
- a 32-byte HMAC over the whole body so a tampered cursor (including a tampered locator entry) returns `EventCursorInvalid` rather than serving wrong data.

The chain-event cursor is variable-length because the locator grows with its entry count; the mempool-event, transparent-history, and address-output families keep a fixed-length body of `STREAM_CURSOR_TOKEN_V1_LEN` bytes, and the snapshot-page family is fixed-length with a 32-byte anchor transaction id between the fixed body and the auth tag. Hash bytes inside the cursor are storage-internal byte order (ADR-0024); the cursor is server-internal material, distinct from the RPC-byte-order hashes on the wire.

Every cursor family is the same `StreamCursorTokenV1` envelope distinguished by the family nibble at byte offset 49: `Tip`/`Safe` (chain events), `Mempool` (mempool events, `0x2`), `TransparentHistory` (`0x3`, with the iteration-direction bit in the flags high nibble), `AddressOutput` (`0x4`), and `SnapshotPage` (`0x5`). The `SnapshotPage` family bookmarks one `MempoolSnapshot` paging walk; its body carries the walk's mempool-event anchor (event sequence plus anchor transaction id, from which every page re-mints the identical `events_resume_cursor`) plus the last yielded transaction id, and it shares the same HMAC authentication as every other family. There is no separate, unauthenticated snapshot-cursor codec: a tampered snapshot cursor fails the HMAC and returns `SNAPSHOT_PAGE_CURSOR_INVALID`, and a cursor anchored ahead of the mempool-event sequence the writer has applied returns `SNAPSHOT_PAGE_CURSOR_EXPIRED`.

On reconnect the server resolves the fork point as the most recent locator entry whose hash equals the canonical block hash at that height. The block index outlives the pruned event-log window, so the fork point resolves even when the divergence base is no longer in retained history. If the cursor's branch was reorged out and the real reorg event was pruned, the server delivers a synthetic `ChainReorged` envelope describing the divergence before resuming; clients never see "silent" branch changes. A divergence deeper than the cap, or an unresolvable fork-point block, degrades to `EventCursorExpired` with re-derive guidance. The `Safe` family never receives a synthesized reorg. See [ADR-0025](../adrs/0025-chain-event-reconnect-reorg-locator.md).

The mempool-event family uses the `family = Mempool` cursor-family code with mempool-specific position fields, defined in [ADR-0007](../adrs/0007-mempool-topology-and-retention.md). It is the same `StreamCursorTokenV1` envelope, not a separate codec.

### Field naming

- Event-stream subscription start: always `start: EventStreamStart` (proto) whose `after_cursor` arm carries the resume bytes ([ADR-0027](../adrs/0027-event-stream-start-positions.md)). The store-side mirror is `EventStreamStartPosition`.
- Paged-read resume field: always `from_cursor: bytes` (proto) or `from_cursor: Option<&StreamCursorTokenV1>` (Rust). Never `start_cursor`, `cursor`, `since`, or `after`.
- Envelope position field: always `cursor: bytes`. Never `next_cursor`, `position`, or `token`.
- Cursor-related errors: always `{Stream}CursorExpired` and `{Stream}CursorInvalid`. Examples: `EventCursorExpired`, `MempoolCursorExpired`.

### Cursor varieties

`WalletQuery.ChainEvents` exposes two consumer modes through the `family` tag in the request cursor:

- `Tip` — receives every `ChainCommitted` and `ChainReorged` envelope. Wallet-shaped: clients must handle reorgs.
- `Safe` — receives only events past the reorg window. Never receives `ChainReorged`. Settlement-shaped: explorers and analytics that prefer slightly delayed but reorg-free data.

Both varieties share the wire format, retention policy, and resume semantics. They differ only in which envelopes the server emits.

## Error Conventions

Error variants are typed, boundary-scoped, and stable. Catch-all variants are forbidden in public boundaries.

### Per-artifact unavailability is unified

The single canonical "this artifact is not available" variant is:

```rust
ArtifactUnavailable {
    family: ArtifactFamily,
    key: ArtifactKey,
}
```

`ArtifactFamily` and `ArtifactKey` are `#[non_exhaustive]` open-ended enums:

```rust
#[non_exhaustive]
pub enum ArtifactFamily {
    SafeBlock,
    CompactBlock,
    Transaction,
    TreeState,
    SubtreeRoot,
    AddressOutputIndex,
    TransparentSpendFact,
}

#[non_exhaustive]
pub enum ArtifactKey {
    BlockHeight(BlockHeight),
    TransactionId(TransactionId),
    SubtreeRootIndex { protocol: ShieldedProtocol, index: SubtreeRootIndex },
    BlockTransactionIndex { height: BlockHeight, tx_index: u64 },
}
```

Adding a new artifact family means adding one `ArtifactFamily` variant and (if the lookup key is novel) one `ArtifactKey` variant. It does not mean adding a new top-level error variant. This rule keeps per-artifact unavailability in one shape and makes future extensions unambiguous.

### Canonical error vocabulary

The reason vocabulary is the `zinder.v1.ops.ErrorReason` proto enum, and the single authoritative reason-to-(`Status` code, retry) table is the [Error Vocabulary reference](../reference/error-vocabulary.md). Every value, its gRPC code, its retry disposition, and its auxiliary detail live there; do not maintain a second copy here.

The mapping is authored once in `crates/zinder-proto/src/error_policy.rs` as `reason_policy`, and every surface builds its `Status` through `status_with_reason`/`status_for_reason`. Each library boundary error enum implements `BoundaryError::error_reason` next to its own definition:

- `zinder-store`: `StoreError`.
- `services/zinder-query`: `QueryError` (reused by `LightwalletdGrpcAdapter` through `status_from_query_error`).
- `services/zinder-explorer`: `ExplorerError`.

The outer `Status` code stays the canonical gRPC retry signal; the reason rides as `google.rpc.ErrorInfo{domain = "zinder.dev", reason = NAME}`. `ERROR_REASON_UNSPECIFIED` is never produced by a boundary enum; the `error_reason_policy_drift` guard and the per-boundary "no variant maps to unspecified" tests enforce this. `BroadcastRejectionReason` stays a separate payload verdict (ADR-0023) and is never folded into `ErrorReason`.

### Richer Error Model

gRPC error responses carry structured detail via `tonic-types`:

- `PreconditionFailure` for cursor-expiry and epoch-pin reasons, `BROADCAST_DISABLED`, and the derive-projection reasons (typed `type` + `subject` + `description`).
- `BadRequest` for cursor-invalid, range, and address reasons (`field` + `description`).
- `ResourceInfo` for `ARTIFACT_UNAVAILABLE` and `CHAIN_EPOCH_MISSING`, with `resource_type` set to the on-wire artifact-family label from `zinder_core::artifact_family` and `resource_name` set to the missing key.

Clients (and LLM agents) can extract a structured remediation from any error without parsing prose.

### Boundary-scoped enums

Library crates use per-boundary `thiserror` enums:

- `zinder-core`: domain types do not return errors except for newtype constructors (`BlockHeightOutOfRange`, etc.).
- `zinder-store`: `StoreError`.
- `zinder-source`: `SourceError`, with `#[from]` conversions from `jsonrpsee::core::ClientError` and `reqwest::Error` *internally*.
- `zinder-runtime`: `ConfigError`.
- `services/zinder-query`: `QueryError`.
- `services/zinder-ingest`: `IngestError`.

Public domain crates do not expose `tonic::Status`, generated proto types, RocksDB handles, upstream node internal types, or transport errors. The mapping happens at the service boundary.

### Public structs and newtypes

- Public structs hide fields unless they are passive data records with no invariants.
- Use domain newtypes for heights, hashes, cursor tokens, reorg-window depths, and schema versions.
- Use `#[non_exhaustive]` for public enums expected to gain variants.
- Newtype accessor: `.value()` is acceptable and consistent across the workspace.

## Configuration Conventions

Field names are public contract because they are written into TOML files and environment variables. Renaming them is a breaking change.

### Section layout

```toml
[network]
name = "zcash-mainnet"

[ops]
# Per-service default: 127.0.0.1:9105 (ingest), :9106 (query), :9107 (compat), :9069 (explorer).
# Set to "" to disable the operational endpoint entirely.
listen_addr = "127.0.0.1:9106"

[node]
json_rpc_addr = "127.0.0.1:8232"
request_timeout_ms = 30000
max_response_bytes = 67108864

[node.auth]
method = "basic"
username = "..."
password = "..."

[node.health]
# Optional. When `addr` is set, the writer polls Zebra's `/ready` as the
# primary upstream-sync signal. Otherwise it derives the signal from
# `getblockchaininfo.verificationprogress`/`estimatedheight`. See [ADR-0015].
# addr = "http://127.0.0.1:8080"
poll_interval_ms = 30000
verification_progress_floor = 0.999
estimated_gap_floor_blocks = 10

[storage]
path = "/var/lib/zinder"
# Reader-only knobs (zinder-query, zinder-compat-lightwalletd):
secondary_path = "/var/lib/zinder/query-secondary"
secondary_catchup_interval_ms = 1000
secondary_replica_lag_threshold_chain_epochs = 4

# Reader defaults shown here. Writer defaults are larger because the writer
# owns bulk catch-up and WAL flushing.
[storage.canonical.rocksdb]
block_cache_bytes = 134217728
max_wal_bytes = 33554432
max_open_files = 128
write_buffer_bytes = 8388608
max_write_buffer_count = 2

[storage.derive.rocksdb]
block_cache_bytes = 67108864
max_wal_bytes = 16777216
max_open_files = 64
write_buffer_bytes = 4194304
max_write_buffer_count = 2

[ingest_control]
# Writer-side (zinder-ingest): bind address. "" disables the endpoint.
listen_addr = "127.0.0.1:9100"
# Reader-side (zinder-query, zinder-compat-lightwalletd): writer URL.
addr = "http://127.0.0.1:9100"
# Shared: bearer-token file enforced by the writer and presented by readers
# when ADR-0006 auth is enabled. File-only; inline secrets rejected.
bearer_token_path = "/run/secrets/zinder-ingest-control"

[retention]
# Enforced by `zinder-ingest`; advertised by `zinder-query` through
# `ServerInfo`. One section, one source of truth.
chain_event_retention_hours = 168
chain_event_retention_check_interval_ms = 60000
cursor_at_risk_warning_hours = 24
mempool_mined_retention_minutes = 60
mempool_invalidated_retention_hours = 24
mempool_event_retention_check_interval_ms = 30000
mempool_cursor_at_risk_warning_minutes = 12

[ingest]
# Source-adapter selector. See [ADR-0016](../adrs/0016-source-streaming-pipeline.md)
# for the full enum vocabulary (`auto`, `zebra-json-rpc`,
# `zebra-indexer-grpc`, `zebra-in-process`).
source = "zebra-json-rpc"
# Chain-truth invariant; classifier defaults to this.
reorg_window_blocks = 100

[ingest.phases]
# Phase classifier boundary; defaults to ingest.reorg_window_blocks.
catchup_threshold_blocks = 100

[ingest.derive]
replay_batch_blocks = 500
replay_policy = "canonical-first"
memory_degrade_ratio = 0.90
memory_pause_ratio = 0.99
memory_resume_ratio = 0.80
min_replay_batch_blocks = 50

[ingest.bulk_catchup]
canonical_batch_max_blocks = 1000
canonical_batch_max_artifact_bytes = 536870912
canonical_batch_max_estimated_write_bytes = 536870912
canonical_batch_min_blocks_before_estimated_write_close = 100
source_segment_max_blocks = 16
source_segment_target_response_bytes = 33554432
source_fetch_max_in_flight_requests = 20
source_fetch_max_in_flight_bytes = 671088640
block_prepare_concurrency = 16
block_prepare_max_in_flight_artifact_bytes = 536870912
commit_reassembly_max_queued_artifact_bytes = 536870912

[ingest.tip_follow]
poll_interval_ms = 1000
lag_threshold_blocks = 1

[ingest.modifiers]
# Optional one-shot or disposable-store knobs.
# target_height = 4000000           # process exits 0 after reaching this height
# checkpoint_height = 3999999       # pre-seed an empty store from this checkpoint
# allow_near_tip_finalize = false   # disposable-store override; invalid with coverage="wallet-serving"
# coverage = "explicit"             # "explicit" | "wallet-serving"

[backup]
to_path = "/var/backups/zinder/checkpoint-2026-04-28"

[query]
listen_addr = "127.0.0.1:9101"
max_compact_block_range = 1000

[query.grpc]
enable_reflection = true
enable_health = true

[compat]
listen_addr = "127.0.0.1:9067"
```

`[retention]` is the single source of truth: `zinder-ingest` enforces eviction against it; `zinder-query` reads the same section when it builds `ServerCapabilities`, so wallet clients see the windows the writer is enforcing. `[ingest_control]` is also shared: the writer reads `listen_addr` to bind, the readers read `addr` to dial, and both read `bearer_token_path` when ADR-0006 auth is enabled.

### Unit suffix rule

Every duration field uses the `_ms` suffix. Sub-second granularity is supported by some operators; mixing `_secs` and `_ms` makes operator scripts error-prone.

| Unit | Suffix | Example |
|------|--------|---------|
| Milliseconds | `_ms` | `request_timeout_ms`, `poll_interval_ms` |
| Bytes | `_bytes` | `max_response_bytes`, `max_size_bytes` |
| Block count | `_blocks` | `reorg_window_blocks`, `canonical_batch_max_blocks` |
| Hour count | `_hours` | `chain_event_retention_hours` |
| Minute count | `_minutes` | `mempool_mined_retention_minutes` |
| Bare counts (events, items) | `_events`, `_entries`, `_chunks` | `max_event_history_entries` |

A bare count (`max_compact_block_range`) is acceptable when the unit is intrinsic to the field name (a "block range" is a count of blocks).

### Environment variable mapping

`ZINDER_<SECTION>__<FIELD>` is the convention. Nested sections double-underscore: `ZINDER_NODE__JSON_RPC_ADDR`, `ZINDER_RETENTION__CHAIN_EVENT_RETENTION_HOURS`, `ZINDER_INGEST_CONTROL__BEARER_TOKEN_PATH`. Every TOML field is reachable through this mapping.

Secrets are accepted through the environment; redaction happens at every emit boundary (`--print-config`, structured logs, `Debug` impls). The supported upstream-node auth shapes are:

| Env var | Resolves to |
| ------- | ----------- |
| `ZINDER_NODE__AUTH__METHOD=basic` + `ZINDER_NODE__AUTH__USERNAME` + `ZINDER_NODE__AUTH__PASSWORD` | HTTP Basic auth |
| `ZINDER_NODE__AUTH__METHOD=cookie` + `ZINDER_NODE__AUTH__PATH=/var/run/auth/.cookie` | Cookie auth from a file on disk |
| `ZINDER_NODE__AUTH__METHOD=cookie` + `ZINDER_NODE__AUTH__COOKIE=<credentials>` | Cookie auth from inline credentials (PaaS pattern) |

`__PATH` and `__COOKIE` are mutually exclusive. Per-surface file-only constraints that remain load-bearing for security reasons (the ingest-control bearer token at `ingest_control.bearer_token_path`, per [ADR-0006](../adrs/0006-ingest-control-transport-security.md)) are enforced at their respective config types, not as a blanket env-var policy.

#### Operator-facing variables

The table below lists the `ZINDER_*` variables every Zinder binary advertises. The content mirrors [`zinder_runtime::ENVIRONMENT_VARIABLES`](../../crates/zinder-runtime/src/env_var_docs.rs); the doc-mirror integration test `zinder-runtime::integration::env_var_docs::public_interfaces_env_var_table_mirrors_runtime_constant` fails when this block and the source list drift apart. Regenerate the rendered table via `cargo run -p zinder-runtime --example dump_env_var_table` and paste the output between the markers below.

<!-- env-var-table:public-interfaces:start -->
| Variable | Used by | Requirement | TOML field | Description |
| -------- | ------- | ----------- | ---------- | ----------- |
| `ZINDER_NETWORK__NAME` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `network.name` | Network identifier: `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`. Note: live-test gating reads the bare `ZINDER_NETWORK` env var directly and never reaches the config loader, so test runbooks still quote that form. |
| `ZINDER_NODE__JSON_RPC_ADDR` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `node.json_rpc_addr` | Upstream Zebra JSON-RPC URL the service connects to. Optional for `zinder-explorer`: without it the upstream-observation probe stays off and `ExplorerFreshness.chain_view.upstream_tip` is always unset. |
| `ZINDER_NODE__INDEXER_GRPC_ADDR` | zinder-ingest | Optional | `node.indexer_grpc_addr` | Optional Zebra indexer gRPC endpoint enabling the streaming mempool source and chain-tip wakeups. Falls back to JSON-RPC polling when unset or empty. |
| `ZINDER_NODE__AUTH__METHOD` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `node.auth.method` | Upstream-node auth shape: `basic`, `cookie`, or unset for no auth. |
| `ZINDER_NODE__AUTH__USERNAME` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | When `ZINDER_NODE__AUTH__METHOD=basic` | `node.auth.username` | Basic-auth username. Paired with `ZINDER_NODE__AUTH__PASSWORD`. |
| `ZINDER_NODE__AUTH__PASSWORD` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | When `ZINDER_NODE__AUTH__METHOD=basic` | `node.auth.password` | Basic-auth password. Redacted in `--print-config` and structured logs. (sensitive; redacted) |
| `ZINDER_NODE__AUTH__PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | When `ZINDER_NODE__AUTH__METHOD=cookie` | `node.auth.path` | Path to a cookie file. Mutually exclusive with `ZINDER_NODE__AUTH__COOKIE`. |
| `ZINDER_NODE__AUTH__COOKIE` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | When `ZINDER_NODE__AUTH__METHOD=cookie` | `node.auth.cookie` | Inline cookie credentials (`username:password`). Mutually exclusive with `ZINDER_NODE__AUTH__PATH`. Accepted for PaaS environments without persistent disks. (sensitive; redacted) |
| `ZINDER_NODE__REQUEST_TIMEOUT_SECS` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `node.request_timeout_secs` | Upstream-node JSON-RPC request timeout in seconds. Defaults to 30. |
| `ZINDER_NODE__MAX_RESPONSE_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `node.max_response_bytes` | Maximum JSON-RPC response body size (bytes) accepted from the node. |
| `ZINDER_NODE__BROADCAST_TIMEOUT_SECS` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `node.broadcast_timeout_secs` | Per-call timeout (seconds) applied only to `sendrawtransaction`. When unset, the global `request_timeout_secs` applies instead. Recommended: 7. |
| `ZINDER_NODE__HEALTH__ADDR` | zinder-ingest | Optional | `node.health.addr` | URL of the upstream's HTTP `/ready` endpoint. When set, the writer polls it as the primary upstream-sync signal; when unset, the writer falls back to `getblockchaininfo.verificationprogress`/`estimatedheight`. See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md). |
| `ZINDER_NODE__HEALTH__POLL_INTERVAL_MS` | zinder-ingest, zinder-explorer | Optional | `node.health.poll_interval_ms` | Cadence of the upstream-health probe in milliseconds. Defaults to 30000. Must be greater than zero. `zinder-explorer` reuses the same cadence for its upstream-observation probe (the one that populates `ExplorerFreshness.chain_view.upstream_tip`). |
| `ZINDER_NODE__HEALTH__VERIFICATION_PROGRESS_FLOOR` | zinder-ingest | Optional | `node.health.verification_progress_floor` | Lower bound on `getblockchaininfo.verificationprogress` below which the fallback path reports `upstream_not_ready`. Defaults to 0.999. Must be in `(0.0, 1.0)`. |
| `ZINDER_NODE__HEALTH__ESTIMATED_GAP_FLOOR_BLOCKS` | zinder-ingest | Optional | `node.health.estimated_gap_floor_blocks` | Block gap between `estimatedheight` and the local tip above which the fallback path reports `upstream_not_ready`. Defaults to 10. |
| `ZINDER_OPS__LISTEN_ADDR` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `ops.listen_addr` | Listen address for the operational HTTP endpoint (`/healthz`, `/readyz`, `/metrics`). Defaults to a per-service loopback address (`127.0.0.1:9105` ingest, `9106` query, `9107` compat, `9069` explorer). Set to an empty string to disable the endpoint entirely. |
| `ZINDER_SECURITY__ALLOW_PUBLIC_BIND` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `security.allow_public_bind` | Opts a binary in to binding its plaintext serving and operational surfaces to a public or unspecified (`0.0.0.0`, `::`) address. Defaults to `false`: a loopback or private-range bind is always allowed, but a public or unspecified bind is refused at startup unless this is `true`. Zinder ships no server TLS (ADR-0006); set this only when a reverse proxy terminates TLS and authorization in front of the listener. |
| `ZINDER_INGEST_CONTROL__LISTEN_ADDR` | zinder-ingest | Optional | `ingest_control.listen_addr` | Listen address of the private IngestControl gRPC endpoint. Localhost-only by default; cross-host deployments must add bearer-token auth per ADR-0006. Set to an empty string to disable the endpoint for diagnostic one-shot runs (such as `--target-height` pre-seed). |
| `ZINDER_INGEST_CONTROL__ADDR` | zinder-query, zinder-compat-lightwalletd | Optional | `ingest_control.addr` | URL of the colocated IngestControl writer (`http://host:port`). Readers use it for tip-change subscriptions, mempool reads, and writer-status lookups. Defaults to `http://127.0.0.1:9100`. |
| `ZINDER_INGEST_CONTROL__BEARER_TOKEN_PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ingest enforces auth` | `ingest_control.bearer_token_path` | Path to the shared-secret bearer token the IngestControl endpoint enforces on every request (ADR-0006). The writer reads it to verify; the readers read the same file to present. File-only by policy; inline secrets are rejected at config load. |
| `ZINDER_STORAGE__PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `storage.path` | Canonical RocksDB store path. Writers open it as primary; readers open it as a secondary. |
| `ZINDER_STORAGE__SECONDARY_PATH` | zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `storage.secondary_path` | Process-unique RocksDB secondary metadata directory. Never share this path across reader processes. |
| `ZINDER_STORAGE__INITIAL_CATCHUP_TIMEOUT_MS` | zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.initial_catchup_timeout_ms` | Maximum startup RocksDB secondary catchup duration before a reader starts with the opened secondary and lets /readyz report replica lag. Defaults to 30000. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__BLOCK_CACHE_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.block_cache_bytes` | Canonical-store RocksDB block cache budget in bytes. Defaults to 536870912 for writers and 134217728 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WAL_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_wal_bytes` | Canonical-store RocksDB live WAL ceiling in bytes. Defaults to 268435456 for writers and 33554432 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_OPEN_FILES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_open_files` | Canonical-store RocksDB open SST file cap. Defaults to 512 for writers and 128 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__WRITE_BUFFER_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.write_buffer_bytes` | Canonical-store per-column-family RocksDB write buffer size. Defaults to 16777216 for writers and 8388608 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WRITE_BUFFER_COUNT` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_write_buffer_count` | Canonical-store per-column-family mutable plus immutable RocksDB write buffer count. Defaults to 2. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MEMTABLE_BUDGET_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.memtable_budget_bytes` | Canonical-store total RocksDB memtable budget across column families. Defaults to 268435456 for writers and 16777216 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__STATISTICS_LEVEL` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.statistics_level` | Canonical-store RocksDB statistics collection gate: `off`, `tickers`, or `full`. Defaults to `tickers`. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__BLOCK_CACHE_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.block_cache_bytes` | Derive-store RocksDB block cache budget in bytes. Defaults to 134217728 for writers and 67108864 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WAL_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.max_wal_bytes` | Derive-store RocksDB live WAL ceiling in bytes. Defaults to 67108864 for writers and 16777216 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_OPEN_FILES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.max_open_files` | Derive-store RocksDB open SST file cap. Defaults to 256 for writers and 64 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__WRITE_BUFFER_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.write_buffer_bytes` | Derive-store per-column-family RocksDB write buffer size. Defaults to 8388608 for writers and 4194304 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WRITE_BUFFER_COUNT` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.max_write_buffer_count` | Derive-store per-column-family mutable plus immutable RocksDB write buffer count. Defaults to 2. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MEMTABLE_BUDGET_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.memtable_budget_bytes` | Derive-store total RocksDB memtable budget across column families. Defaults to 67108864 for writers and 16777216 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__STATISTICS_LEVEL` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.derive.rocksdb.statistics_level` | Derive-store RocksDB statistics collection gate: `off`, `tickers`, or `full`. Defaults to `tickers`. |
| `ZINDER_INGEST__SOURCE` | zinder-ingest | Required | `ingest.source` | Source-adapter selector. Lives on `[ingest]` (not `[node]`) because the choice is a writer-private implementation decision: `[node]` describes the upstream node itself, `[ingest].source` describes which adapter ingest uses to talk to it. See [ADR-0016](../adrs/0016-source-streaming-pipeline.md). |
| `ZINDER_STORAGE__RAW_BLOB_POLICY` | zinder-ingest | Optional | `storage.raw_blob_policy` | Raw-byte blob write policy: `none`, `transactions`, or `all`. Defaults to `none` for explicit coverage so fact-first indexing does not write raw block or transaction blobs unless a deployment explicitly needs raw export. Wallet-serving coverage defaults to `transactions` and rejects `none`, because lightwalletd transaction and transparent-history methods require retained bytes. |
| `ZINDER_INGEST__REORG_WINDOW_BLOCKS` | zinder-ingest | Optional | `ingest.reorg_window_blocks` | Chain-truth invariant: how deep the live reorg window extends. Bounds finalization, classifier default, and replacement traversal. Must be greater than zero. Defaults to 100. |
| `ZINDER_INGEST__PHASES__CATCHUP_THRESHOLD_BLOCKS` | zinder-ingest | Optional | `ingest.phases.catchup_threshold_blocks` | Gap (in blocks) at which the unified loop transitions between `BulkCatchup` and `TipFollow`. Defaults to `ingest.reorg_window_blocks`. See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md). |
| `ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_BLOCKS` | zinder-ingest | Optional | `ingest.bulk_catchup.canonical_batch_max_blocks` | Block count per bulk-catchup commit batch. Defaults to 1000. |
| `ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_ARTIFACT_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.canonical_batch_max_artifact_bytes` | Canonical artifact bytes accumulated before closing a bulk-catchup batch. Defaults to 536870912. |
| `ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes` | Estimated canonical write bytes accumulated before closing a bulk-catchup batch. Defaults to 536870912. |
| `ZINDER_INGEST__BULK_CATCHUP__CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE` | zinder-ingest | Optional | `ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close` | Minimum blocks accumulated before estimated write bytes can close a bulk-catchup batch. Single oversized blocks can still close immediately. Defaults to 100. |
| `ZINDER_INGEST__BULK_CATCHUP__SOURCE_SEGMENT_MAX_BLOCKS` | zinder-ingest | Optional | `ingest.bulk_catchup.source_segment_max_blocks` | Maximum connected blocks requested from the source in one bulk-catchup segment. Defaults to 16. |
| `ZINDER_INGEST__BULK_CATCHUP__SOURCE_SEGMENT_TARGET_RESPONSE_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.source_segment_target_response_bytes` | Target source response bytes for adaptive segment sizing. Defaults to 33554432. |
| `ZINDER_INGEST__BULK_CATCHUP__SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS` | zinder-ingest | Optional | `ingest.bulk_catchup.source_fetch_max_in_flight_requests` | Maximum concurrent source segment requests. Defaults to 12. |
| `ZINDER_INGEST__BULK_CATCHUP__SOURCE_FETCH_MAX_IN_FLIGHT_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.source_fetch_max_in_flight_bytes` | Maximum reserved source response bytes across active fetches and completed source reassembly. Must be greater than or equal to node.max_response_bytes. Defaults to 402653184. |
| `ZINDER_INGEST__BULK_CATCHUP__BLOCK_PREPARE_CONCURRENCY` | zinder-ingest | Optional | `ingest.bulk_catchup.block_prepare_concurrency` | Parallel canonical block-prepare slots. Defaults to `min(available_parallelism(), 16)`. |
| `ZINDER_INGEST__BULK_CATCHUP__BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.block_prepare_max_in_flight_artifact_bytes` | Maximum reserved derived artifact bytes across active and completed block-prepare work. Defaults to 536870912. |
| `ZINDER_INGEST__BULK_CATCHUP__COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.commit_reassembly_max_queued_artifact_bytes` | Maximum safe-tip artifact bytes that can accumulate while the previous bulk-catchup batch is attaching metadata, committing, or flushing. Defaults to 536870912. |
| `ZINDER_INGEST__DERIVE__REPLAY_BATCH_BLOCKS` | zinder-ingest | Optional | `ingest.derive.replay_batch_blocks` | Maximum block contexts hydrated and dispatched in one derive replay write. Must be greater than zero. Defaults to 100. |
| `ZINDER_INGEST__DERIVE__REPLAY_POLICY` | zinder-ingest | Optional | `ingest.derive.replay_policy` | Derive replay pressure policy. `canonical-first` pauses rebuildable derive replay under memory pressure so canonical ingest keeps the process budget. `continuous` replays retained chain events whenever they are available while the writer is at tip; during bulk catch-up a canonical-phase gate still throttles it to residual capacity. Defaults to `canonical-first`. |
| `ZINDER_INGEST__DERIVE__MEMORY_BUDGET_BYTES` | zinder-ingest | Optional | `ingest.derive.memory_budget_bytes` | Explicit derive replay memory budget in bytes. When unset, derive replay uses the runtime cgroup `memory.high` or `memory.max` value when present. |
| `ZINDER_INGEST__DERIVE__MEMORY_DEGRADE_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_degrade_ratio` | Memory pressure ratio at which derive replay shrinks the effective replay batch size. Defaults to 0.90. |
| `ZINDER_INGEST__DERIVE__MEMORY_PAUSE_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_pause_ratio` | Memory pressure ratio at which canonical-first derive replay pauses. Defaults to 0.99. |
| `ZINDER_INGEST__DERIVE__MEMORY_RESUME_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_resume_ratio` | Memory pressure ratio below which degraded derive replay returns to the normal replay batch size. Paused replay resumes as degraded work once pressure falls below memory_pause_ratio. Defaults to 0.80. |
| `ZINDER_INGEST__DERIVE__MIN_REPLAY_BATCH_BLOCKS` | zinder-ingest | Optional | `ingest.derive.min_replay_batch_blocks` | Smallest effective derive replay batch size under memory degradation. Must be greater than zero and no larger than replay_batch_blocks. Defaults to 10. |
| `ZINDER_INGEST__DERIVE__STARTUP_HANDOFF_LAG_BLOCKS` | zinder-ingest | Optional | `ingest.derive.startup_handoff_lag_blocks` | Residual derive lag in blocks at which the startup catch-up stops replaying synchronously and hands the remainder to the always-on tailer, so the API and ops surfaces come up while the tailer drains the rest. A bounded wall-clock budget caps the startup catch-up regardless of this value. Defaults to 1000. |
| `ZINDER_INGEST__BULK_CATCHUP__FLUSH_INTERVAL_EPOCHS` | zinder-ingest | Optional | `ingest.bulk_catchup.flush_interval_epochs` | Bulk-catchup RocksDB flush cadence in committed epochs. Must be greater than zero. Defaults to 5. |
| `ZINDER_INGEST__TIP_FOLLOW__POLL_INTERVAL_MS` | zinder-ingest | Optional | `ingest.tip_follow.poll_interval_ms` | Tip-follow poll cadence in milliseconds. Must be greater than zero. Defaults to 1000. |
| `ZINDER_INGEST__TIP_FOLLOW__LAG_THRESHOLD_BLOCKS` | zinder-ingest | Optional | `ingest.tip_follow.lag_threshold_blocks` | Block lag at which tip-follow reports `cause=syncing`. Defaults to 1. |
| `ZINDER_INGEST__MODIFIERS__TARGET_HEIGHT` | zinder-ingest | Optional | `ingest.modifiers.target_height` | One-shot stop-at modifier; the loop exits 0 after committing this height. Renamed from `to_height`. |
| `ZINDER_INGEST__MODIFIERS__CHECKPOINT_HEIGHT` | zinder-ingest | Optional | `ingest.modifiers.checkpoint_height` | Pre-seed an empty store from an upstream-supplied checkpoint at this height. |
| `ZINDER_INGEST__MODIFIERS__ALLOW_NEAR_TIP_FINALIZE` | zinder-ingest | Optional | `ingest.modifiers.allow_near_tip_finalize` | Disposable-store override: lets bulk-catchup finalize inside the reorg window. Invalid combined with `coverage = "wallet-serving"`. |
| `ZINDER_INGEST__MODIFIERS__COVERAGE` | zinder-ingest | Optional | `ingest.modifiers.coverage` | Ingest coverage mode: `"explicit"` or `"wallet-serving"`. Defaults to `"explicit"`. |
| `ZINDER_RETENTION__CHAIN_EVENT_RETENTION_HOURS` | zinder-ingest, zinder-query | Optional | `retention.chain_event_retention_hours` | Chain-event retention window in hours, enforced by `zinder-ingest` and advertised by `zinder-query` through `ServerInfo`. Defaults to 168 (7 days). `0` disables eviction. |
| `ZINDER_RETENTION__CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS` | zinder-ingest | Optional | `retention.chain_event_retention_check_interval_ms` | Chain-event retention sweep cadence in milliseconds. Must be greater than zero. Defaults to 60000 (one minute). |
| `ZINDER_RETENTION__CURSOR_AT_RISK_WARNING_HOURS` | zinder-ingest | Optional | `retention.cursor_at_risk_warning_hours` | Cursor-at-risk warning lead time in hours. Must be ≤ `retention.chain_event_retention_hours`. Defaults to 24. |
| `ZINDER_RETENTION__MEMPOOL_MINED_RETENTION_MINUTES` | zinder-ingest, zinder-query | Optional | `retention.mempool_mined_retention_minutes` | Mined-mempool retention window in minutes, enforced by `zinder-ingest` and advertised by `zinder-query`. Defaults to 60. `0` disables retention. |
| `ZINDER_RETENTION__MEMPOOL_INVALIDATED_RETENTION_HOURS` | zinder-ingest, zinder-query | Optional | `retention.mempool_invalidated_retention_hours` | Invalidated-mempool retention window in hours, enforced by `zinder-ingest` and advertised by `zinder-query`. Defaults to 24. `0` disables retention. |
| `ZINDER_RETENTION__MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS` | zinder-ingest | Optional | `retention.mempool_event_retention_check_interval_ms` | Mempool-event retention sweep cadence in milliseconds. Must be greater than zero. Defaults to 30000. |
| `ZINDER_RETENTION__MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES` | zinder-ingest | Optional | `retention.mempool_cursor_at_risk_warning_minutes` | Mempool cursor-at-risk warning lead time in minutes. Must be ≤ the shortest configured mempool retention window. Defaults to 12. |
| `ZINDER_EXPLORER__BEARER_TOKEN_PATH` | zinder-explorer | Optional | `explorer.bearer_token_path` | Path to the shared-secret bearer token the ExplorerQuery endpoint enforces on cross-service explorer-plane reads (ADR-0006). |
| `ZINDER_EXPLORER__LISTEN_ADDR` | zinder-explorer | Optional | `explorer.listen_addr` | Listen address for the ExplorerQuery gRPC endpoint. Defaults to 127.0.0.1:9068. |
| `ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT` | zinder-explorer | Optional | `explorer.wallet_query_endpoint` | WalletQuery gRPC endpoint backing the explorer's wallet-composed reads (transaction detail, block views, search, mempool activity). Empty/unset disables the explorer capabilities that compose canonical wallet reads. |
<!-- env-var-table:public-interfaces:end -->

### `--print-config`

Every production binary exposes `--config` and `--print-config`. The print form shows explicit `[REDACTED]` markers for every sensitive field regardless of how the value was supplied (config file, env var, or CLI override). The output round-trips: feeding `--print-config` back as `--config` produces the same effective configuration.

### Avoid ambiguous names

- `timeout` without a unit.
- `channel_size` without saying which channel.
- `data_dir` when the directory owns canonical storage. Use `storage.path`.
- `server_settings` when the section is really `query` or `grpc`.
- `rpc_user` and `rpc_password` when the fields are really one `node.auth` variant.
- `interval` without a unit (`interval_ms`).
- `enabled` without a noun (`enable_reflection` is preferred over `reflection_enabled`).

## Capability Discovery

Every public gRPC service exposes a `ServerInfo` RPC that returns a `ServerCapabilities` descriptor. Capability discovery is the canonical alternative to version pinning. [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) names the public vocabulary.

### Descriptor shape

The cross-service descriptor is `zinder.v1.ops.ServerInfo`; per-service descriptors (`WalletServerInfo`, `ExplorerServerInfo`) embed it as `common` and layer service-specific fields on top.

```proto
message zinder.v1.ops.ServerInfo {
  string network = 1;                  // "zcash-mainnet" / "zcash-testnet" / "zcash-regtest"
  string service_name = 2;             // "zinder-ingest" / "zinder-query" / ...
  string service_version = 3;          // semver of the running binary
  repeated string capabilities = 4;    // capability strings; clients match exact strings
  uint32 contract_revision = 5;        // monotonic in-place-revision marker (ADR-0027)
}

message WalletServerInfo {
  zinder.v1.ops.ServerInfo common = 1;
  string lightwalletd_protocol_commit = 2;          // vendored lightwalletd commit hash
  uint32 schema_version = 3;                        // canonical artifact schema version
  uint32 reorg_window_blocks = 4;                   // configured reorg window depth
  uint64 chain_event_retention_seconds = 5;         // 0 = unbounded retention (development only)
  uint64 mempool_mined_retention_seconds = 6;       // 0 = mined-event family not retained on this deployment
  uint64 mempool_invalidated_retention_seconds = 7; // 0 = invalidated-event family not retained on this deployment
  NodeCapabilitiesDescriptor node = 8;
}

message NodeCapabilitiesDescriptor {
  optional string version = 1;
  repeated string capabilities = 2;
}
```

`contract_revision` is a monotonically increasing marker, incremented whenever the semantics of an existing wire surface are revised in place. Capability strings identify wire shapes additively; the revision marker covers what they cannot: an RPC whose name and message tags survive while its meaning changes. The value is the single `zinder_proto::CONTRACT_REVISION` constant, currently 1. Consumers assert a minimum (`contract_revision >= N`) and refuse to run against an older server rather than misinterpreting its streams. See [ADR-0027](../adrs/0027-event-stream-start-positions.md).

### Capability strings

Capability strings are exact-match (no version negotiation, no regex). New capabilities are additive. Each wire shape pairs with one capability identifier; a wire-shape change lands as a new `_vN` identifier. The naming convention is `domain.subdomain.capability_name_v{N}`; the suffix is part of the identity, never decoded as a version field.

Capability strings are the deprecation boundary. A wire-shape change lands as a new `_vN` capability; removing an older capability requires the architecture doc for that surface to name the consumer constraint and removal rule.

RPC removal is an explicit contract migration, not a side effect of adding a broader method. A replacement lands additively with its own capability. Removing the older method requires evidence that known consumers have migrated, a documented deployment and rollback sequence, and any required derive-store consumer removal migration. `RecentTransactions` therefore remains available alongside `TransactionHistory`.

The single source of truth is the `CAPABILITIES` table in [`crates/zinder-proto/src/capabilities.rs`](../../crates/zinder-proto/src/capabilities.rs). Each row binds a capability string to its surface (`Wallet`, `Explorer`, `Ingest`), the proto method it gates, and a declarative advertise policy. The three `ServerInfo` builders fold over the table filtered by surface and evaluate each row's policy against their own readiness; no service hand-maintains a parallel capability array. Two CI guards keep the table honest: `capability_descriptor_drift` cross-checks every row's proto-method binding against the compiled `FileDescriptorSet`, and `capability_docs::public_interfaces_capability_list_mirrors_zinder_capabilities` fails when the list below diverges from the wallet and explorer rows of the table.

Advertise policies name the precondition each surface evaluates: `AlwaysOn`; the wallet-plane `RequiresBroadcaster`, `RequiresChainEvents`, `RequiresChainValuePools`, `RequiresBlockBlobs`, and `RequiresTransactionBlobs`; and the explorer-plane readiness gates. `RequiresBlockBlobs` gates `wallet.read.full_block_at_v1` and `wallet.read.full_block_range_v1`; `RequiresTransactionBlobs` gates `wallet.read.transaction_bytes_v1` (the `MinedTransaction.raw_transaction_bytes` field). Both resolve against the store's persisted raw-blob retention, not against reader config: ingest persists the active `raw_blob_policy` into a `StorageControl` singleton on every primary open, and readers read it. A legacy store with no signal reads back as `none`, so a blob-serving capability is never advertised unless the store demonstrably retains the bytes. See [ADR-0018](../adrs/0018-capability-gated-optional-payload-fields.md).

`wallet.read.compact_block_ironwood_v1` is `AlwaysOn` on every deployment of this binary and gates the `ironwoodActions`/`ironwoodCommitmentTreeSize` fields inside `CompactBlock.payload_bytes` (the vendored lightwalletd `CompactTx`/`ChainMetadata` shape, not a native `zinder.v1.wallet` field). A server advertising it has derived Ironwood action data for every block it serves, so an absent `ironwoodActions` on a given block means that block has no Ironwood activity. A server that does not advertise it predates Ironwood wallet-plane support: a missing `ironwoodActions` there is not authoritative, and a client must not read it as "no Ironwood activity".

`wallet.read.subtree_roots_ironwood_v1` is `AlwaysOn` on every deployment of this binary and gates the Ironwood protocol on `WalletQuery.SubtreeRoots` and the lightwalletd-compat `GetSubtreeRoots` surface. A server that does not advertise it rejects Ironwood subtree-root requests; clients fall back to linear scanning of the Ironwood tree rather than reading an error (or an empty response from an older server) as "no completed subtrees".

`explorer.transaction.intrinsic_value_balances_v1` gates the optional signed Sprout, Sapling, Orchard, and Ironwood balances on `TransactionHistoryEntry` and `TransactionDetailResponse`. It is advertised only when transaction history, transaction detail's WalletQuery dependency, and a canonical secondary at artifact schema 15 or newer are all online. Both reads prefer the materialized intrinsic-balance artifact and can bridge its unsettled-tip reconciliation lag from a retained canonical transaction blob at the same pinned epoch. An absent field remains unknown rather than an all-zero balance when neither source is available.

`explorer.transaction.history_v1` is advertised when the transaction-history
projection has persisted state and its `WalletQuery` dependency is online.
`explorer.transaction.history_v2` is dynamic: it is advertised only when
verified coverage starts at height 1, reaches the current projection tip, and
ends at the same hash. The additive v2 fields are
`TransactionHistoryReadFence`, `TransactionHistoryCoverage`, and
`TransactionHistoryCountScope`. One request reads projection metadata, rows,
joins, and any exact count from a single derive-store snapshot. Cursors bind the
request filter and read fence; stale fences or cursors return
`FAILED_PRECONDITION`. A requested count is present with `FULL_HISTORY` scope
only under complete coverage. Partial or unverified projections keep v1
available, omit the exact total, and do not advertise v2.

<!-- capability-list:public-interfaces:start -->
- `wallet.read.latest_block_v1`
- `wallet.read.block_id_by_selector_v1`
- `wallet.read.block_header_by_selector_v1`
- `wallet.read.compact_block_at_v1`
- `wallet.read.compact_block_range_v1`
- `wallet.read.compact_block_ironwood_v1`
- `wallet.read.full_block_at_v1`
- `wallet.read.full_block_range_v1`
- `wallet.read.tree_state_at_height_v1`
- `wallet.read.latest_tree_state_checkpoint_v1`
- `wallet.read.subtree_roots_in_range_v1`
- `wallet.read.subtree_roots_ironwood_v1`
- `wallet.read.transaction_by_id_v1`
- `wallet.read.transaction_bytes_v1`
- `wallet.read.server_info_v1`
- `wallet.broadcast.transaction_v1`
- `wallet.events.chain_v1`
- `wallet.snapshot.mempool_v1`
- `wallet.events.mempool_v1`
- `wallet.mempool.transparent_outputs_by_address_v1`
- `wallet.mempool.transparent_spends_by_outpoint_v1`
- `wallet.mempool.transparent_outputs_by_outpoint_v1`
- `wallet.read.transparent_outputs_by_outpoint_v1`
- `wallet.read.transparent_spends_by_outpoint_v1`
- `wallet.read.transparent_unspent_outputs_by_outpoint_v1`
- `wallet.read.chain_value_pools_at_tip_v1`
- `wallet.read.transparent_utxo_set_summary_v1`
- `wallet.read.transparent_utxo_set_commitment_v1`
- `wallet.address.transparent_unspent_outputs_v1`
- `wallet.address.transparent_history_v1`
- `wallet.address.transparent_balance_v1`
- `explorer.server_info_v1`
- `explorer.transaction.detail_v3`
- `explorer.block.summary_v1`
- `explorer.block.production_series_v2`
- `explorer.block.detail_v1`
- `explorer.block.transactions_v2`
- `explorer.block.final_note_commitment_roots_v1`
- `explorer.block.activity_distribution_v1`
- `explorer.search_v1`
- `explorer.commitment_root.search_v1`
- `explorer.commitment_root.displaced_matches_v1`
- `explorer.mempool.summary_v1`
- `explorer.mempool.snapshot_v1`
- `explorer.mempool.activity_v1`
- `explorer.transparent_address.activity_v2`
- `explorer.transparent_address.deltas_v1`
- `explorer.fee.summary_v1`
- `explorer.fee.conventional_distribution_v1`
- `explorer.fee.paid_distribution_v1`
- `explorer.value_pool.summary_v1`
- `explorer.network_upgrade.status_v1`
- `explorer.value_pool.flow_history_v1`
- `explorer.value_pool.flow_summary_v1`
- `explorer.value_pool.flow_amount_threshold_summary_v1`
- `explorer.value_pool.flow_rounded_amount_summary_v1`
- `explorer.value_pool.balance_history_v1`
- `explorer.utxo_set.summary_v1`
- `explorer.utxo_set.commitment_v1`
- `explorer.chain.reorg_history_v1`
- `explorer.chain.displaced_block_history_v1`
- `explorer.chain.displaced_block_detail_v1`
- `explorer.mempool.event_counts_v1`
- `explorer.transaction.fees_v1`
- `explorer.transaction.history_v1`
- `explorer.transaction.recent_v1`
- `explorer.transaction.history_v2`
- `explorer.transaction.intrinsic_value_balances_v1`
- `explorer.transaction.component_summary_v2`
- `explorer.transparent_address.ranking_v1`
- `explorer.payment_disclosure.verify_v1`
- `explorer.overview.snapshot_v1`
- `explorer.migration.overview_v1`
- `explorer.migration.cohorts_v1`
- `explorer.migration.denominations_v1`
<!-- capability-list:public-interfaces:end -->

`wallet.broadcast.transaction_v1` is deployment-gated: binaries support the RPC, but `ServerInfo` advertises it only when a transaction broadcaster is configured and its source probe reports `transaction_broadcast`. Read-only query deployments return `FailedPrecondition` from the RPC and omit the capability.

`WalletQuery.TransparentAddressBalance` is served in the wallet plane and advertises `wallet.address.transparent_balance_v1` on every deployment. The handler sums the confirmed total in-process from the canonical unspent-output index, then overlays the signed mempool delta (`unconfirmed_delta_zat`) through the colocated ingest-control endpoint; deployments without that endpoint return a zero delta rather than failing. Lightwalletd-shaped confirmed-only balance stays on the compatibility plane: `GetTaddressBalance` projects the same wallet primitive into one `value_zat`.

`WalletQuery.TransparentUtxoSetSummary` is served in the wallet plane and advertises `wallet.read.transparent_utxo_set_summary_v1` on every deployment. It is the chain-wide transparent UTXO accounting (`gettxoutsetinfo`-equivalent): a request-time streaming scan of the canonical current-UTXO projection that folds the set into `utxo_count` and `total_value_zat` without buffering it. The scan runs at the resolved epoch's settled tip, where the projection is the irreversible unspent set, so it applies no per-row spend re-check or block-visibility check; `summarized_height` reports that tip and an optional `at_epoch_id` pins it. There is no materialized counter and no new column family, so the cost is one full-set scan per call. The serialized-set hash and byte size of `gettxoutsetinfo` are not reported: both require a UTXO-set serialization ordering Zinder does not define. In their place the response carries an optional order-independent `commitment` (LtHash16, see [ADR-0026](../adrs/0026-utxo-set-commitment.md)) that binds full set membership and is reproducible across deployments at the same settled tip. The commitment fold has per-output CPU cost, so it is operator opt-in: it is present only when the deployment advertises `wallet.read.transparent_utxo_set_commitment_v1`, absent (`None`) otherwise. `ExplorerQuery.UtxoSetSummary` wraps this primitive in the `ExplorerFreshness` envelope and mirrors the commitment under `explorer.utxo_set.commitment_v1`.

`ExplorerQuery.TransactionComponentSummary` advertises
`explorer.transaction.component_summary_v2`. Requests carry an exact half-open
Unix-second range. Responses carry typed totals, UTC-day rows, the current
`ExplorerFreshness`, and contiguous historical/live coverage. Component names
state the counted protocol object (`sapling_output_count`,
`orchard_action_count`, and `ironwood_action_count`). Predicate counters state
their exact Sapling/Orchard/Ironwood scope in their identifiers, including
`sapling_orchard_or_ironwood_transaction_count` and three explicitly named
non-coinbase predicates. Unsupported parsed
sections increment `transaction_predicate_unavailable_count` and exclude the
transaction from every predicate counter; consumers require that count to be
zero before claiming exact predicate totals. `totals_only` defaults to false
and includes UTC-day rows; true returns totals and coverage only. Completeness
requires height-1 coverage joined through the visible tip and is never inferred
from block timestamps alone.

`ExplorerQuery.ConventionalFeeDistribution` advertises
`explorer.fee.conventional_distribution_v1` only when its additive projection
has materialized coverage. Requests carry an exact half-open Unix-second range.
Responses contain sorted ZIP-317 conventional-fee frequency counts per UTC
day, unavailable-transaction counts, `ExplorerFreshness`, and contiguous
coverage. The contract never calls these values paid fees and does not expose
percentiles or compatibility-adapter response vocabulary.

`ExplorerQuery.PaidFeeDistribution` advertises
`explorer.fee.paid_distribution_v1` only when its separate exact-fee
projection has materialized coverage. Each fee combines resolved transparent
prevouts and outputs with canonical signed Sprout, Sapling, Orchard, and
Ironwood transaction value balances. Missing prevouts or intrinsic balance
artifacts remain explicit unavailable counts; the method never substitutes a
ZIP-317 conventional fee.

`ExplorerQuery.ValuePoolFlowHistory`, `ExplorerQuery.ValuePoolFlowSummary`,
`ExplorerQuery.ValuePoolFlowAmountThresholdSummary`, and
`ExplorerQuery.ValuePoolFlowRoundedAmountSummary` advertise the corresponding
`explorer.value_pool.flow_*_v1` capabilities. All four read one additive canonical
per-transaction flow projection. History provides typed direction and pool
filters, minimum net amount, opaque filter-bound paging, optional exact totals,
and explicit historical/live-tail coverage. Summary aggregates the same events
into UTC hour or day buckets over an exact half-open Unix-second range. The two
amount summaries provide bounded exact cumulative thresholds and reusable
nearest-quantum frequency groups, respectively; they do not persist a second
analytics projection. The
native contract retains signed Sprout, Sapling, Orchard, and Ironwood balances;
it does not expose adapter database identifiers, REST cursors, address
labels, risk classifications, or display units.

`ExplorerQuery.ValuePoolBalanceHistory` advertises
`explorer.value_pool.balance_history_v1`. It returns authoritative cumulative
post-block balances sampled at the highest canonical height observed in each
UTC day. Each point carries its exact height, hash, block time, and a dynamic
list of `{id, monitored, value_zat}` entries, so transparent, Lockbox, and
future pools remain first-class facts instead of being inferred from the four
currently known shielded pools. Pages are bounded and newest first with an
opaque day cursor. Completeness requires contiguous scanned heights from 1
through the visible tip; block timestamps never establish coverage because
they are not monotonic. Historical scanning fetches only daily candidates from
Zebra's verbose `getblock`, while the replaceable live tail retains every
block for exact reorg reconciliation. The optional schema-16 canonical
artifact binds each source snapshot to the requested block hash and time; a
store persisted below artifact schema 17 is refused at open and rebuilt from
genesis.

`ExplorerQuery.DisplacedBlockHistory` and
`ExplorerQuery.DisplacedBlockDetail` advertise
`explorer.chain.displaced_block_history_v1` and
`explorer.chain.displaced_block_detail_v1`. The writer captures the old
branch's complete header facts, ordered transaction identifiers, transparent
coinbase payout scripts and values, and any already-retained raw block bytes in
the same RocksDB batch that publishes the replacement branch. History uses an
opaque `(event sequence, former height, block hash)` cursor and returns the
current canonical block at each former height from one pinned epoch. Detail is
hash-addressed. Coverage starts at the first replacement event observed by an
archive-enabled writer; an empty archive before that event is valid, and no
surface claims to reconstruct older displaced branches from the current best
chain. Product-specific terms such as uncle, orphan, database id, miner pool,
report source, and external node status stay outside the native contract.

Do not add native capability strings for lightwalletd-shaped mempool products
such as raw-transaction streams or compact-transaction streams. Those are
compatibility adapter views derived from `MempoolSnapshot` and `MempoolEvents`;
the native capability vocabulary stays on snapshot and event semantics.

`ServerCapabilities.node.capabilities` is reserved for source capability
snapshots when the runtime can pass the source probe result to the query
service. Today's storage-only `zinder-query` adapter does not call upstream
nodes, so it returns an empty node-capability list by default rather than
guessing.

The current `zinder-source::NodeCapability` diagnostic names are:

- `best_chain_blocks`
- `tip_id`
- `tree_state`
- `subtree_roots`
- `safe_tip_height`
- `readiness_probe`
- `transaction_broadcast`
- `json_rpc`
- `openrpc_discovery`
- `chain_value_pools`

Do not advertise future source capabilities such as block-stream ingestion or
spending-transaction lookup until the source adapter and runtime wiring both
exist. `chain_value_pools` is source-backed by
`getblockchaininfo.valuePools`; the source response binds those totals to the
same RPC response's `blocks` and `bestblockhash` as one `BlockId`. The wallet
and explorer read planes proxy it through the ingest writer instead of opening
independent upstream-node handles.

## Wire Conventions

Native to wire identifier translations live in `crates/zinder-core/src/wire/` and only there. Files are organized by concept (`transaction_id`, `block_hash`, `auth_digest`, `wtxid`, `merkle_root`, `chain_name`, `branch_id`), not by dialect; every dialect for one concept shares one file.

### Hash byte order

Zcash 32-byte hashes have two byte orders. Both are spec-defined terms; both appear in this codebase by exactly those names:

- **Internal byte order**: the raw SHA-256d output. Used in consensus serialization (`hashPrevBlock`, `hashMerkleRoot` per Zcash protocol spec `protocol.tex:13560-13564`), stored verbatim in the `[u8; 32]` newtypes (`TransactionId`, `BlockHash`, `AuthDigest`, `MerkleRoot`) and the `[u8; 64]` `Wtxid` newtype, and used as RocksDB keys.
- **RPC byte order**: the byte-reversed form (per Zcash protocol spec `\rpcByteOrder`, `protocol.tex:1127`, defining sentence at `:4036`). The form `zcash-cli`, every wallet UI, every block explorer URL, and ZIP 308 (`zip-0308.rst:389`) use to present a hash to a human or an RPC client.

The public proto contract uses **RPC byte order** for every hash-shaped field (`transaction_id`, `block_hash`, `previous_block_hash`, `merkle_root_hash`, `auth_digest`, `wtxid`, `BlockTip.hash`, `mined_block_hash`, `completing_block_hash`, `spending_transaction_id`, etc.), conveyed as a lowercase ASCII hex `string`. The two forms convert via the `encode_rpc_*_hex` / `decode_rpc_*_hex` pair in `wire/`; the storage-facing `encode_internal_*` / `decode_internal_*` pair is the identity for `[u8; 32]` <-> `[u8; 32]` and exists so storage code never names raw byte slices. See [ADR-0024](../adrs/0024-wire-format-rpc-byte-order.md).

Zebra's indexer gRPC surface (`zebra_indexer_rpc`, an ingress-only dialect) carries RPC byte order as raw proto `bytes` (Zebra fills each hash field with `bytes_in_display_order`). The `decode_rpc_*_bytes` functions in `wire/` decode that form; reading those fields verbatim as internal order yields byte-reversed hashes that fail every downstream lookup.

### UTXO-set commitment element encoding

The transparent UTXO-set commitment binds each unspent output through a fixed-width little-endian preimage: `network_id(u32 LE) ‖ encoding_version(u8) ‖ txid(32, internal order) ‖ output_index(u32 LE) ‖ value_zat(u64 LE) ‖ script_len(u32 LE) ‖ raw_scriptPubKey ‖ block_height(u32 LE)`. The preimage and the 16-byte BLAKE2X personalization (`b"ZinderUtxoSet___"`) live in `crates/zinder-core/src/wire/utxo_set_commitment.rs`; `encode_utxo_set_commitment_element` is the only encoder. `network_id` and `encoding_version` ride in the preimage rather than the BLAKE2 personalization so a third party reproduces the bytes from a plain UTXO dump. The scheme is carried on the wire as the `UtxoSetCommitmentScheme` enum, never a string. See [ADR-0026](../adrs/0026-utxo-set-commitment.md).

### Forbidden inline forms

When adding a new wire field or a new ingress dialect, locate or add a function in `crates/zinder-core/src/wire/` before writing any boundary code. The following inline forms are forbidden anywhere outside that module:

- `transaction_id.as_bytes()` and `block_hash.as_bytes()` at a wire boundary. Use `encode_internal_transaction_id` or `encode_internal_block_hash`.
- `format!("{:08x}", branch_id)` for wire output. Use `encode_branch_id_hex`.
- Inline hex-string transaction id or block hash encode or decode. Use `encode_rpc_*_hex` / `decode_rpc_*_hex`.
- Manual byte-reversal of a hash before hex-encoding or after hex-decoding. The reversal lives inside `encode_rpc_*_hex` / `decode_rpc_*_hex`; callers never reverse.
- Hardcoded capability literals. Import the `pub const` from [`crates/zinder-proto/src/capabilities.rs`](../../crates/zinder-proto/src/capabilities.rs).
- Inline serialization of a UTXO-set commitment element preimage, or the personalization literal `ZinderUtxoSet___`. Use `encode_utxo_set_commitment_element`.
- Duplicate `Network` to wire-string tables. Use `encode_bip70_chain_name` (`"main"`/`"test"`, BIP70/lightwalletd/Zebra JSON-RPC) or `encode_zinder_native_chain_name` (`"zcash-mainnet"`/`"zcash-testnet"`/`"zcash-regtest"`, native config and protobuf).

Two integration tests enforce the rules on every CI invocation: [`crates/zinder-core/tests/integration/wire_invariants.rs`](../../crates/zinder-core/tests/integration/wire_invariants.rs) and [`crates/zinder-proto/tests/integration/capability_string_uniqueness.rs`](../../crates/zinder-proto/tests/integration/capability_string_uniqueness.rs).

Decode failures return `zinder_core::wire::WireDecodeError`. Encode operations are infallible by construction.

## Rust API Shape

The public API should make a normal integration obvious without implying that production services share storage handles.

### Wallet integrators (gRPC client)

Wallet and application developers integrate with the native protobuf service. The generated client name is `WalletQueryClient`:

```rust
use zinder_proto::v1::wallet::{
    wallet_query_client::WalletQueryClient, CompactBlocksInRangeRequest, LatestBlockRequest,
};

let mut wallet = WalletQueryClient::connect("https://zinder.example").await?;
let tip = wallet.latest_block(LatestBlockRequest {}).await?;
let blocks = wallet
    .compact_blocks_in_range(CompactBlocksInRangeRequest {
        start_height: 1_000_000,
        end_height: 1_000_100,
    })
    .await?;
```

### Rust integrators

Rust consumers can depend on `zinder-client`, which exports the typed `ChainIndex` trait with two implementations: `LocalChainIndex` for colocated canonical reads through RocksDB secondaries, and `RemoteChainIndex` for consumers that need the full gRPC and endpoint-backed surface. Applications select an implementation by the operations and topology they require.

Colocated read-only application:

```rust
use zinder_client::{ChainIndex, LocalChainIndex, LocalOpenOptions, BlockHeight};
use std::time::Duration;
use tokio_stream::StreamExt as _;

let chain = LocalChainIndex::open(LocalOpenOptions {
    storage_path: "/var/lib/zinder".into(),
    secondary_path: "/var/lib/zinder/application-secondary".into(),
    network: zinder_client::Network::ZcashTestnet,
    canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::canonical_reader_defaults(),
    derive_rocksdb_budget: zinder_store::RocksDbResourceBudget::derive_reader_defaults(),
    subscription_endpoint: Some("http://127.0.0.1:9101".into()),
    catchup_interval: Duration::from_millis(1_000),
}).await?;

let tip = chain.latest_block().await?;
let block = chain.compact_block_at(BlockHeight::new(1_000_000)).await?;
let mut events = chain.chain_events(None).await?;
while let Some(envelope) = events.next().await {
    let envelope = envelope?;
    // typed ChainEventEnvelope, no tonic::Status anywhere
}
```

`subscription_endpoint` points at the colocated `zinder-query` proxy when the consumer also needs `ServerInfo` or `BroadcastTransaction`; direct ingest subscription endpoints are reserved for event-only colocated consumers once the private ingest subscription server lands.

Remote Rust application:

```rust
use zinder_client::{ChainIndex, RemoteChainIndex, RemoteOpenOptions};

let chain = RemoteChainIndex::connect(RemoteOpenOptions {
    endpoint: "http://zinder.internal:9101".into(),
    network: zinder_client::Network::ZcashTestnet,
}).await?;

// Canonical read methods share the ChainIndex vocabulary.
let tip = chain.latest_block().await?;
```

Consumers that cannot link `zinder-client` can vendor the `WalletQuery` protocol and generate client stubs with their own toolchain. This wire-only boundary avoids coupling the consumer to Zinder's Rust dependency graph while preserving the same `ChainEpoch` and capability contracts.

[Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape) defines the `zinder-client` shape. [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) owns the multi-process model that makes both implementations necessary.

### Local development composition facade

Local development may expose an `Indexer` composition facade that runs ingest and query in one process:

```rust
use zinder::{Indexer, NodeSource};

let indexer = Indexer::builder()
    .source(NodeSource::zebra_json_rpc("http://127.0.0.1:8232"))
    .storage_path("./zinder")
    .build()
    .await?;

let reader = indexer.chain_epoch_reader().await?;
let tip = reader.latest_block().await?;
```

This example is illustrative, not final. The important points:

- `Indexer` is a local composition facade, not the production service boundary.
- Node configuration is a `NodeSource`.
- Read access is explicit and epoch-bound.
- Production binaries use service-specific config types, not a shared `IndexerConfig` god object.

### Storage-level names

Storage and cursor byte contracts are lower-level than the normal public API, but their names are stable and searchable: `StoreKey`, `ArtifactEnvelopeHeaderV1`, and `StreamCursorTokenV1` (the one cursor envelope for every resumable read). Mechanism-shaped names such as `key_codec`, `cursor_helper`, or `bytes_utils` are forbidden.

## Crate Boundaries

Workspace shape:

```text
crates/
  zinder-core/
  zinder-store/
  zinder-source/
  zinder-proto/
  zinder-client/
  zinder-runtime/
  zinder-testkit/
services/
  zinder-ingest/
  zinder-query/
  zinder-compat-lightwalletd/
  zinder-explorer/
```

Add a crate only when it has a stable domain boundary and enough behavior to justify its interface. The current set is the target list, not a command to create every crate immediately.

## Contract Hygiene

Public shapes describe behavior that production code can actually reach.

- Public event variants, error variants, API transitions, cursor fields, and proto surfaces must be produced, consumed, or explicitly reserved by the owning architecture document.
- Delete unreachable public variants. Do not keep fallback variants only because they might be useful later.
- Names identify the source of truth. Use `created_at` for the wall-clock time when Zinder created a record. Use a chain-derived name such as `tip_block_time_millis` when the value comes from block header time.
- Use `ChainTipMetadata` for chain-derived wallet counters at the visible tip, such as Sapling, Orchard, and Ironwood note commitment tree sizes. Do not make query code rediscover those counters by decoding wallet protocol payloads. The proto `ChainEpoch` message carries `sapling_commitment_tree_size`, `orchard_commitment_tree_size`, and `ironwood_commitment_tree_size` directly.
- Bulk-catchup ranges that publish `ChainTipMetadata` must be contiguous with a known metadata base. Fresh stores start at height 1; non-empty stores append after the current tip; checkpoint-bounded stores start at `SourceChainCheckpoint.height + 1` after ingest seeds the builder from the checkpoint's chain-global tree sizes.
- Wallet-serving coverage is selected with `ingest.coverage = "wallet-serving"` or `zinder-ingest --wallet-serving`. Per [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md), this is a consumer-neutral serving-store profile, not a Zodl-specific mode. In that mode, ingest derives the bulk-catchup floor and `checkpoint_height` from upstream-node-advertised activation heights; explicit height overrides and `allow_near_tip_finalize` are rejected so serving stores do not silently become recent-checkpoint or near-tip-safe-tip fixtures.
- Transition names match the visible state change. If finality advances, use a finality transition such as `FinalizeThrough`; if no visible transition side effect occurred, use `Unchanged`.
- Cursor fields that are serialized and authenticated must either be validated on read or documented as reserved state in the owning cursor contract.
- Operator-facing errors name the real cause and carry useful fields. Prefer `NoVisibleChainEpoch`, sequence-overflow, and payload-size errors over sentinel IDs or reused malformed-input errors.

## Interface Documentation

Every new public runtime, crate, or protocol type answers:

- What owns this?
- What does it read?
- What does it write?
- What can restart independently?
- What can be rebuilt from canonical artifacts?
- What privacy assumptions does it make?

If the answer is unclear, the boundary is not ready.

## ZIP cross-reference

Zinder's vocabulary is the durable spine; this table is how to read each term in the context of the Zcash improvement proposals it touches. Each row points to the canonical ZIP that defines the concept and notes whether Zinder re-exposes the same name, picks a richer domain name, or deliberately leaves the concept out of scope.

| Zinder term | ZIP | Treatment |
|---|---|---|
| `TransactionId` (32-byte canonical) | [ZIP-244](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst) `txid_digest` (v5+); pre-v5 SHA256d | Same bytes; derivation differs by tx version. Zinder treats the value as opaque; the doc comment in `zinder-core::transaction.rs` records the split. |
| `AuthDigest` | [ZIP-244](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst) `auth_digest` | Aligned (v5+ only; `Option`-gated on pre-v5). |
| `ConsensusBranchId` | [ZIP-200](https://github.com/zcash/zips/blob/main/zips/zip-0200.rst) `CONSENSUS_BRANCH_ID` | Aligned (newtype over `u32`; rendered as `{:#010x}` to match `getblockchaininfo`). |
| `NetworkUpgradeActivation.name` | [ZIP-252](https://github.com/zcash/zips/blob/main/zips/zip-0252.rst) `NU5`, [ZIP-253](https://github.com/zcash/zips/blob/main/zips/zip-0253.md) `NU6`, NU6.1, NU7 | Carried verbatim from the node per [ADR-0008](../adrs/0008-network-parameter-discovery.md); no Zinder-side enum. |
| `BlockHeaderInfo.commitment_bytes` | [ZIP-221](https://github.com/zcash/zips/blob/main/zips/zip-0221.rst) `hashChainHistoryRoot`, [ZIP-244](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst) §3.2 `hashBlockCommitments` | Single raw 32-byte field; interpretation depends on height + upgrade. Doc comment records all three possible meanings (`hashFinalSaplingRoot`, `hashChainHistoryRoot`, `hashBlockCommitments`). |
| `MempoolEvictionReason::Expired` | [ZIP-203](https://github.com/zcash/zips/blob/main/zips/zip-0203.rst) `nExpiryHeight` | Aligned (expiry surfaced as a removal reason, not the raw header field). |
| `MempoolEvictionReason::LowFee` | [ZIP-401](https://github.com/zcash/zips/blob/main/zips/zip-0401.rst) `low_fee_penalty` | Aligned. |
| `MempoolEvent::Suppressed` | [ZIP-401](https://github.com/zcash/zips/blob/main/zips/zip-0401.rst) `RecentlyEvicted` | Wire and Rust shape wired; source-side emission reserved per [ADR-0007 §Suppression](../adrs/0007-mempool-topology-and-retention.md#suppression-zip-401-recentlyevicted-is-wired-but-reserved). |
| `CompactTxStreamer`, `CompactBlock`, `CompactTx`, `BlockID`, `ChainSpec` | [ZIP-307](https://github.com/zcash/zips/blob/main/zips/zip-0307.rst) | Vendored verbatim into `proto/compat/lightwalletd/`, pinned to the upstream commit in `LIGHTWALLETD_PROTOCOL_COMMIT`. Compat layer maps Zinder's `Network` strings to lightwalletd's `chainName` (`"main"`/`"test"`). |
| `BlockSelector`, `BlockMetadata` | [ZIP-307](https://github.com/zcash/zips/blob/main/zips/zip-0307.rst) `BlockID` | Intentional improvement: split request shape (oneof `height_or_hash`) from response shape (typed `BlockMetadata`); response uses `uint32` height and `block_hash` instead of `uint64` + `hash`. |
| `ServerCapabilities.network` (`"zcash-mainnet"`, `"zcash-testnet"`, `"zcash-regtest"`) | [ZIP-307](https://github.com/zcash/zips/blob/main/zips/zip-0307.rst) `ChainSpec` | Intentional improvement: `ChainSpec` is structurally empty in ZIP-307; Zinder identifies the network with a machine-readable string. |
| Transparent address selectors (`AddressLookup`) | [ZIP-316](https://github.com/zcash/zips/blob/main/zips/zip-0316.rst) `Receiver`, `Typecode` | Out of scope on the indexer side: Zinder accepts base58 P2PKH/P2SH addresses or a SHA-256 script-hash. UA parsing and receiver extraction are wallet responsibilities. |
| Conventional/marginal fee surface | [ZIP-317](https://github.com/zcash/zips/blob/main/zips/zip-0317.rst) `conventional_fee`, `marginal_fee` | Not exposed. Fee estimation is a wallet concern; Zinder is an indexer. The compat `CompactTx.fee` field is inherited legacy from lightwalletd and is not re-typed. |
| Chain value pools | [ZIP-209](https://github.com/zcash/zips/blob/main/zips/zip-0209.rst) Sprout/Sapling/Orchard chain value pool | Not exposed. Zebra enforces the invariant; Zinder does not re-account. |
| Shielded scanning surfaces | [ZIP-307](https://github.com/zcash/zips/blob/main/zips/zip-0307.rst) §payment detection, [ZIP-302](https://github.com/zcash/zips/blob/main/zips/zip-0302.rst), [ZIP-310](https://github.com/zcash/zips/blob/main/zips/zip-0310.rst) | Out of scope (PRD: no server-side viewing-key custody, no memo decryption). |
| Key derivation, mnemonics, payment URIs | [ZIP-32](https://github.com/zcash/zips/blob/main/zips/zip-0032.rst), [ZIP-339](https://github.com/zcash/zips/blob/main/zips/zip-0339.rst), [ZIP-321](https://github.com/zcash/zips/blob/main/zips/zip-0321.rst) | Out of scope. `zinder-testkit::transparent_signer` exists for test broadcast cycles only and is consumed exclusively under `[dev-dependencies]`. |

## Cross-references

- [Extending Artifacts cookbook](extending-artifacts.md) — the agent-extensibility checklist for adding artifact families, RPC methods, and error variants.
- [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) — the chain-event subscription wire shape.
- [Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape) — typed Rust client surface.
- [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) — retention windows and pruning.
- [ADR-0007](../adrs/0007-mempool-topology-and-retention.md) — mempool topology, retention windows, and protocol surface.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — `ServerInfo` shape and deprecation rules.
