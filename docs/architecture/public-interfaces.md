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

### Product and runtimes

| Term | Meaning |
|------|---------|
| `Zinder` | The product |
| `zinder-ingest` | Production service that owns chain ingestion and canonical writes |
| `zinder-query` | Production service that serves wallet and application APIs from epoch-bound indexed state |
| `zinder-compat-lightwalletd` | Compatibility adapter that serves vendored lightwalletd gRPC over `WalletQueryApi` |
| `zinder-explorer` | Optional service for explorer-shaped reads and replayable derived indexes ([ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md)) |
| `zinder-client` | Library crate exporting the typed Rust client surface for in-process consumers (Zallet) and Rust integrations |
| `PrimaryChainStore` | `zinder-store` handle that opens canonical RocksDB as the only writer |
| `SecondaryChainStore` | `zinder-store` handle that opens canonical RocksDB as a RocksDB secondary reader |

### Domain types

| Term | Meaning |
|------|---------|
| `ChainEpoch` | A consistent visible chain snapshot |
| `ChainEpochReader` | In-process read view pinned to one `ChainEpoch` |
| `ChainEpochReadApi` | Internal read API for epoch-bound canonical reads |
| `ChainEvent` | Post-commit canonical transition emitted by `zinder-ingest` |
| `ChainEventEnvelope` | Cursor-bound chain-event message carried over the ingest subscription plane and exposed natively on `WalletQuery.ChainEvents` |
| `ChainTipMetadata` | Chain-derived counters at the visible tip (Sapling and Orchard tree sizes) |
| `BlockHeaderArtifact` | Durable typed block-header fact row |
| `BlockBlobArtifact` | Optional raw block blob available only when raw blob policy stores blocks |
| `CompactBlockArtifact` | Wallet-oriented compact block artifact |
| `BlockId` | Stable block identity (`{ height: BlockHeight, hash: BlockHash }`); lives in `zinder-core` and is the canonical (height, hash) pair across the source boundary, the wallet protocol, and the reader API |
| `TransactionLocation` | Durable transaction id to block-location fact |
| `TransactionFactsArtifact` | Durable typed public transaction facts parsed once by ingest |
| `ReorgWindow` | Non-finalized range where reorgs are expected and supported |
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
| `TransactionBroadcastResult` | Typed accepted, duplicate, invalid-encoding, rejected, or unknown broadcast outcome |
| `RawTransactionBytes` | Raw serialized transaction bytes submitted by a wallet |

### Wallet protocol surface

| Term | Meaning |
|------|---------|
| `WalletQuery` | Native protobuf service for wallet and application reads from epoch-bound Zinder data |
| `WalletQueryApi` | Rust query boundary used by `zinder-query` and compatibility adapters |
| `WalletQueryGrpcAdapter` | Tonic adapter that serves native `WalletQuery` over `WalletQueryApi` through `grpc/native.rs` response builders |
| `LatestBlockResponse` | Native wallet protocol response for latest visible block metadata |
| `CompactBlockRangeChunk` | Native wallet protocol stream item for one compact block bound to one chain epoch |
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
| `ExplorerQueryGrpcAdapter` | Tonic adapter for `ExplorerQuery`; carries the optional `WalletQuery` endpoint that backs the Shape C federated balance compute path |
| `TransactionPublicFacts` | Single typed transaction-fact value parsed once at ingest/mempool/explorer-read time. See [ADR-0010](../adrs/0010-transaction-public-facts.md). |
| `ExplorerFreshness` | Cross-cutting freshness envelope embedded on every explorer response. See [ADR-0011](../adrs/0011-explorer-freshness-envelope.md). |
| `SearchCandidate` | Typed search-result oneof distinguishing every classifiable input class, including the `NotPubliclyIndexable` arm for shielded receivers. See [ADR-0012](../adrs/0012-typed-explorer-search-and-privacy-refusal.md). |

### Derive-plane SDK

| Term | Meaning |
|------|---------|
| `DeriveStore` | Projection `RocksDB` wrapper opened as primary by `zinder-ingest` and as secondary by reader gateways. Owns the `chain_event_cursor`, `mempool_event_cursor`, and `consumer_metadata` column families. |
| `DeriveStoreTable` | Logical column-family identifier referenced by reads and `WriteBatch` puts |
| `DeriveConsumerName` | Stable static identifier scoping cursor and metadata rows; renaming is a schema migration, not a config change |
| `DeriveConsumer` | Rust trait every chain-events derive consumer implements: dispatches `ChainCommittedEvent` and `ChainReorgedEvent` through `apply_chain_committed` / `apply_chain_reorged` |
| `DeriveMempoolConsumer` | Rust trait for consumers that observe `MempoolEvents` instead of (or in addition to) chain events |
| `DeriveConsumerCtx` | Per-event consumer context carrying a `&DeriveStore` borrow for reads and a `&mut WriteBatch` consumers stage their writes into; the SDK appends the cursor advance to the same batch and commits atomically |
| `ChainCommittedEvent`, `ChainReorgedEvent` | Typed wrappers around the wire chain-event variants, decoded into `zinder-core` primitives so consumers never see prost-generated types |
| `MempoolConsumerEvent` | Typed wrapper around one `MempoolEventEnvelope`, carrying borrowed transaction-id and raw-transaction-bytes slices for the duration of the apply call |
| `DeriveStore::write_chain_event` | Ingest-hosted dispatcher that applies `BlockKeyedConsumer` implementations and persists the chain-event cursor atomically with consumer writes |
| `DeriveStore::write_mempool_event` | Ingest-hosted dispatcher that applies a `DeriveMempoolConsumer` and persists the mempool-event cursor atomically with consumer writes |

### Cursors, events, errors

| Term | Meaning |
|------|---------|
| `StreamCursorTokenV1` | Opaque cursor body for chain-event subscriptions; fork-aware, encodes epoch id, last visible block hash, in-epoch offset, and stream-family tag |
| `MempoolStreamCursorV1` | Opaque cursor body for mempool-event subscriptions |
| `ChainEventStreamFamily` | Stream-family enum used inside chain-event cursor bodies (`Tip`, `Finalized`; `Mempool` is a reserved family code, not an active chain-event family) |
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

- For an exact `BlockHeight` key, use `{artifact}_at(height)`. Example: `block_at(height)`, `compact_block_at(height)`.
- For checkpoint floor reads, use `{artifact}_checkpoint_at_or_before(max_height)`. Example: `tree_state_checkpoint_at_or_before(max_height)`.
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

- `{event_kind}_events(from_cursor)` returning `impl Stream<Item = Result<{EventKind}Envelope, _>>`.
- Example: `chain_events(from_cursor)`, `mempool_events(from_cursor)`, future `derive_events(from_cursor)`.
- The request field carrying the cursor is always `from_cursor` (see Cursor Conventions below).
- The envelope field carrying the cursor position is always `cursor`.

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

## Cursor Conventions

Cursors are opaque to clients, fork-aware on the server, and authenticated where applicable.

### Body shape

`StreamCursorTokenV1` (chain events) is the canonical cursor body. The body is encoded as a `postcard`-serialized internal struct, then base-64 over the wire, never parsed by clients. The body fields are:

- `network`: target network (mainnet / testnet / regtest)
- `family`: `ChainEventStreamFamily` tag (`Tip`, `Finalized`, `Mempool`, `Derive`)
- `epoch_id`: `ChainEpochId` of the most recently delivered envelope
- `last_visible_block_hash`: block hash at the tip of that epoch (used to detect forks across reconnect)
- `in_epoch_offset`: position within the epoch's emitted events
- `event_sequence`: monotonic per-store sequence (also surfaced on the envelope for diagnostics)
- HMAC over the body so a tampered cursor returns `EventCursorInvalid` rather than serving wrong data.

A cursor whose `last_visible_block_hash` is no longer present at its `epoch_id`'s tip indicates the client missed a reorg. The server emits a synthetic `ChainReorged` envelope describing the divergence before resuming the stream. Clients never see "silent" branch changes.

`MempoolStreamCursorV1` uses the `family = Mempool` cursor-family code with mempool-specific position fields, defined in [ADR-0007](../adrs/0007-mempool-topology-and-retention.md).

### Field naming

- Request resume field: always `from_cursor: bytes` (proto) or `from_cursor: Option<&[u8]>` (Rust). Never `start_cursor`, `cursor`, `since`, or `after`.
- Envelope position field: always `cursor: bytes`. Never `next_cursor`, `position`, or `token`.
- Cursor-related errors: always `{Stream}CursorExpired` and `{Stream}CursorInvalid`. Examples: `EventCursorExpired`, `MempoolCursorExpired`.

### Cursor varieties

`WalletQuery.ChainEvents` exposes two consumer modes through the `family` tag in the request cursor:

- `Tip` — receives every `ChainCommitted` and `ChainReorged` envelope. Wallet-shaped: clients must handle reorgs.
- `Finalized` — receives only events past the reorg window. Never receives `ChainReorged`. Settlement-shaped: explorers and analytics that prefer slightly delayed but reorg-free data.

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
    FinalizedBlock,
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

Errors named here are the contract. Internal modules may carry richer detail types, but every public-boundary error maps to one of these names with stable gRPC `Status` codes:

| Variant | gRPC `Status` | Metadata `reason` |
|---------|---------------|-------------------|
| `NodeUnavailable` | `Unavailable` | `node_unavailable` |
| `NodeCapabilityMissing { capability }` | `FailedPrecondition` | `node_capability_missing` |
| `StorageUnavailable { kind: StorageErrorKind }` | `Unavailable` | `storage_unavailable` |
| `EntropyUnavailable` | `Internal` | `entropy_unavailable` |
| `SchemaMismatch { persisted, expected }` | `FailedPrecondition` | `schema_mismatch` |
| `ReorgWindowExceeded { depth, configured }` | `FailedPrecondition` | `reorg_window_exceeded` |
| `EventCursorExpired { oldest_retained }` | `FailedPrecondition` | `event_cursor_expired` |
| `EventCursorInvalid { reason: CursorInvalidReason }` | `InvalidArgument` | `event_cursor_invalid` |
| `MempoolCursorExpired { oldest_retained }` | `FailedPrecondition` | `mempool_cursor_expired` |
| `MempoolCursorInvalid` | `InvalidArgument` | `mempool_cursor_invalid` |
| `ArtifactUnavailable { family, key }` | `NotFound` | `artifact_unavailable` |
| `EpochNotFound` | `NotFound` | `epoch_not_found` |
| `NoVisibleChainEpoch` | `Unavailable` | `no_visible_chain_epoch` |
| `InvalidChainStoreOptions { detail }` | `InvalidArgument` | `invalid_chain_store_options` |
| `CompactBlockRangeTooLarge { requested, max }` | `InvalidArgument` | `compact_block_range_too_large` |
| `BroadcastInvalidEncoding` | `InvalidArgument` | `broadcast_invalid_encoding` |
| `BroadcastRejected { reason: BroadcastRejectReason }` | `FailedPrecondition` | `broadcast_rejected` |
| `BroadcastDisabled` | `FailedPrecondition` | `broadcast_disabled` |

This table is the single source of truth. The mapping function lives in `services/zinder-query/src/grpc/mod.rs::status_from_query_error` and is used by both `WalletQueryGrpcAdapter` (native) and `LightwalletdGrpcAdapter` (compat). No service has its own copy.

### Richer Error Model

gRPC error responses carry structured detail via `tonic-types`:

- `PreconditionFailure` for `EventCursorExpired` (with `suggested_resume_height`), `ReorgWindowExceeded`, `BroadcastRejected`.
- `BadRequest` for `EventCursorInvalid`, `CompactBlockRangeTooLarge` (with `field` and `description`).
- `ResourceInfo` for `ArtifactUnavailable` (with `resource_type = "block"` etc., `resource_name = key`).

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
| `ZINDER_NODE__JSON_RPC_ADDR` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | Required | `node.json_rpc_addr` | Upstream Zebra JSON-RPC URL the service connects to. |
| `ZINDER_NODE__INDEXER_GRPC_ADDR` | zinder-ingest | Optional | `node.indexer_grpc_addr` | Optional Zebra indexer gRPC endpoint enabling the streaming mempool source and chain-tip wakeups. Falls back to JSON-RPC polling when unset. |
| `ZINDER_NODE__AUTH__METHOD` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | Optional | `node.auth.method` | Upstream-node auth shape: `basic`, `cookie`, or unset for no auth. |
| `ZINDER_NODE__AUTH__USERNAME` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ZINDER_NODE__AUTH__METHOD=basic` | `node.auth.username` | Basic-auth username. Paired with `ZINDER_NODE__AUTH__PASSWORD`. |
| `ZINDER_NODE__AUTH__PASSWORD` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ZINDER_NODE__AUTH__METHOD=basic` | `node.auth.password` | Basic-auth password. Redacted in `--print-config` and structured logs. (sensitive; redacted) |
| `ZINDER_NODE__AUTH__PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ZINDER_NODE__AUTH__METHOD=cookie` | `node.auth.path` | Path to a cookie file. Mutually exclusive with `ZINDER_NODE__AUTH__COOKIE`. |
| `ZINDER_NODE__AUTH__COOKIE` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ZINDER_NODE__AUTH__METHOD=cookie` | `node.auth.cookie` | Inline cookie credentials (`username:password`). Mutually exclusive with `ZINDER_NODE__AUTH__PATH`. Accepted for PaaS environments without persistent disks. (sensitive; redacted) |
| `ZINDER_NODE__REQUEST_TIMEOUT_SECS` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | Optional | `node.request_timeout_secs` | Upstream-node JSON-RPC request timeout in seconds. Defaults to 30. |
| `ZINDER_NODE__MAX_RESPONSE_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | Optional | `node.max_response_bytes` | Maximum JSON-RPC response body size (bytes) accepted from the node. |
| `ZINDER_NODE__HEALTH__ADDR` | zinder-ingest | Optional | `node.health.addr` | URL of the upstream's HTTP `/ready` endpoint. When set, the writer polls it as the primary upstream-sync signal; when unset, the writer falls back to `getblockchaininfo.verificationprogress`/`estimatedheight`. See [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md). |
| `ZINDER_NODE__HEALTH__POLL_INTERVAL_MS` | zinder-ingest | Optional | `node.health.poll_interval_ms` | Cadence of the upstream-health probe in milliseconds. Defaults to 30000. Must be greater than zero. |
| `ZINDER_NODE__HEALTH__VERIFICATION_PROGRESS_FLOOR` | zinder-ingest | Optional | `node.health.verification_progress_floor` | Lower bound on `getblockchaininfo.verificationprogress` below which the fallback path reports `upstream_not_ready`. Defaults to 0.999. Must be in `(0.0, 1.0)`. |
| `ZINDER_NODE__HEALTH__ESTIMATED_GAP_FLOOR_BLOCKS` | zinder-ingest | Optional | `node.health.estimated_gap_floor_blocks` | Block gap between `estimatedheight` and the local tip above which the fallback path reports `upstream_not_ready`. Defaults to 10. |
| `ZINDER_OPS__LISTEN_ADDR` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `ops.listen_addr` | Listen address for the operational HTTP endpoint (`/healthz`, `/readyz`, `/metrics`). Defaults to a per-service loopback address (`127.0.0.1:9105` ingest, `9106` query, `9107` compat, `9069` explorer). Set to an empty string to disable the endpoint entirely. |
| `ZINDER_INGEST_CONTROL__LISTEN_ADDR` | zinder-ingest | Optional | `ingest_control.listen_addr` | Listen address of the private IngestControl gRPC endpoint. Localhost-only by default; cross-host deployments must add bearer-token auth per ADR-0006. Set to an empty string to disable the endpoint for diagnostic one-shot runs (such as `--target-height` pre-seed). |
| `ZINDER_INGEST_CONTROL__ADDR` | zinder-query, zinder-compat-lightwalletd | Optional | `ingest_control.addr` | URL of the colocated IngestControl writer (`http://host:port`). Readers use it for tip-change subscriptions, mempool reads, and writer-status lookups. Defaults to `http://127.0.0.1:9100`. |
| `ZINDER_INGEST_CONTROL__BEARER_TOKEN_PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd | When `ingest enforces auth` | `ingest_control.bearer_token_path` | Path to the shared-secret bearer token the IngestControl endpoint enforces on every request (ADR-0006). The writer reads it to verify; the readers read the same file to present. File-only by policy; inline secrets are rejected at config load. |
| `ZINDER_STORAGE__PATH` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `storage.path` | Canonical RocksDB store path. Writers open it as primary; readers open it as a secondary. |
| `ZINDER_STORAGE__SECONDARY_PATH` | zinder-query, zinder-compat-lightwalletd, zinder-explorer | Required | `storage.secondary_path` | Process-unique RocksDB secondary metadata directory. Never share this path across reader processes. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__BLOCK_CACHE_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.block_cache_bytes` | Canonical-store RocksDB block cache budget in bytes. Defaults to 536870912 for writers and 134217728 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WAL_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_wal_bytes` | Canonical-store RocksDB live WAL ceiling in bytes. Defaults to 268435456 for writers and 33554432 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_OPEN_FILES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_open_files` | Canonical-store RocksDB open SST file cap. Defaults to 512 for writers and 128 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__WRITE_BUFFER_BYTES` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.write_buffer_bytes` | Canonical-store per-column-family RocksDB write buffer size. Defaults to 16777216 for writers and 8388608 for readers. |
| `ZINDER_STORAGE__CANONICAL__ROCKSDB__MAX_WRITE_BUFFER_COUNT` | zinder-ingest, zinder-query, zinder-compat-lightwalletd, zinder-explorer | Optional | `storage.canonical.rocksdb.max_write_buffer_count` | Canonical-store per-column-family mutable plus immutable RocksDB write buffer count. Defaults to 2. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__BLOCK_CACHE_BYTES` | zinder-ingest, zinder-query, zinder-explorer | Optional | `storage.derive.rocksdb.block_cache_bytes` | Derive-store RocksDB block cache budget in bytes. Defaults to 134217728 for writers and 67108864 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WAL_BYTES` | zinder-ingest, zinder-query, zinder-explorer | Optional | `storage.derive.rocksdb.max_wal_bytes` | Derive-store RocksDB live WAL ceiling in bytes. Defaults to 67108864 for writers and 16777216 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_OPEN_FILES` | zinder-ingest, zinder-query, zinder-explorer | Optional | `storage.derive.rocksdb.max_open_files` | Derive-store RocksDB open SST file cap. Defaults to 256 for writers and 64 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__WRITE_BUFFER_BYTES` | zinder-ingest, zinder-query, zinder-explorer | Optional | `storage.derive.rocksdb.write_buffer_bytes` | Derive-store per-column-family RocksDB write buffer size. Defaults to 8388608 for writers and 4194304 for readers. |
| `ZINDER_STORAGE__DERIVE__ROCKSDB__MAX_WRITE_BUFFER_COUNT` | zinder-ingest, zinder-query, zinder-explorer | Optional | `storage.derive.rocksdb.max_write_buffer_count` | Derive-store per-column-family mutable plus immutable RocksDB write buffer count. Defaults to 2. |
| `ZINDER_INGEST__SOURCE` | zinder-ingest | Required | `ingest.source` | Source-adapter selector. Lives on `[ingest]` (not `[node]`) because the choice is a writer-private implementation decision: `[node]` describes the upstream node itself, `[ingest].source` describes which adapter ingest uses to talk to it. See [ADR-0016](../adrs/0016-source-streaming-pipeline.md). |
| `ZINDER_STORAGE__RAW_BLOB_POLICY` | zinder-ingest | Optional | `storage.raw_blob_policy` | Raw-byte blob write policy: `none`, `transactions`, or `all`. Defaults to `none` so fact-first indexing does not write raw block or transaction blobs unless a deployment explicitly needs raw export. |
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
| `ZINDER_INGEST__BULK_CATCHUP__COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES` | zinder-ingest | Optional | `ingest.bulk_catchup.commit_reassembly_max_queued_artifact_bytes` | Maximum finalized artifact bytes that can accumulate while the previous bulk-catchup batch is attaching metadata, committing, or flushing. Defaults to 536870912. |
| `ZINDER_INGEST__DERIVE__REPLAY_BATCH_BLOCKS` | zinder-ingest | Optional | `ingest.derive.replay_batch_blocks` | Maximum block contexts hydrated and dispatched in one derive replay write. Must be greater than zero. Defaults to 100. |
| `ZINDER_INGEST__DERIVE__REPLAY_POLICY` | zinder-ingest | Optional | `ingest.derive.replay_policy` | Derive replay pressure policy. `canonical-first` pauses rebuildable derive replay under memory pressure so canonical ingest keeps the process budget. `continuous` replays retained chain events whenever they are available. Defaults to `canonical-first`. |
| `ZINDER_INGEST__DERIVE__MEMORY_BUDGET_BYTES` | zinder-ingest | Optional | `ingest.derive.memory_budget_bytes` | Explicit derive replay memory budget in bytes. When unset, derive replay uses the runtime cgroup `memory.high` or `memory.max` value when present. |
| `ZINDER_INGEST__DERIVE__MEMORY_DEGRADE_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_degrade_ratio` | Memory pressure ratio at which derive replay shrinks the effective replay batch size. Defaults to 0.90. |
| `ZINDER_INGEST__DERIVE__MEMORY_PAUSE_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_pause_ratio` | Memory pressure ratio at which canonical-first derive replay pauses. Defaults to 0.99. |
| `ZINDER_INGEST__DERIVE__MEMORY_RESUME_RATIO` | zinder-ingest | Optional | `ingest.derive.memory_resume_ratio` | Memory pressure ratio below which degraded derive replay returns to the normal replay batch size. Paused replay resumes as degraded work once pressure falls below memory_pause_ratio. Defaults to 0.80. |
| `ZINDER_INGEST__DERIVE__MIN_REPLAY_BATCH_BLOCKS` | zinder-ingest | Optional | `ingest.derive.min_replay_batch_blocks` | Smallest effective derive replay batch size under memory degradation. Must be greater than zero and no larger than replay_batch_blocks. Defaults to 10. |
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
| `ZINDER_EXPLORER__WALLET_QUERY_ENDPOINT` | zinder-explorer | Optional | `explorer.wallet_query_endpoint` | WalletQuery gRPC endpoint backing the federated `TransparentAddressBalance` compute path. Empty/unset disables the `explorer.transparent_address.balance_v1` capability. |

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

```proto
message ServerCapabilities {
  string network = 1;                            // "zcash-mainnet" / "zcash-testnet" / "zcash-regtest"
  string service_version = 2;                    // semver of the running binary
  string lightwalletd_protocol_commit = 3;       // vendored lightwalletd commit hash
  uint32 schema_version = 4;                     // canonical artifact schema version
  uint32 reorg_window_blocks = 5;                // configured non-finalized window depth
  uint64 chain_event_retention_seconds = 6;         // 0 = unbounded retention (development only)
  uint64 mempool_mined_retention_seconds = 7;       // 0 = mined-event family not retained on this deployment
  uint64 mempool_invalidated_retention_seconds = 8; // 0 = invalidated-event family not retained on this deployment
  repeated string capabilities = 9;              // capability strings; clients match exact strings
  NodeCapabilitiesDescriptor node = 10;
  optional string mcp_endpoint = 11;             // unset in v1
}

message NodeCapabilitiesDescriptor {
  optional string version = 1;
  repeated string capabilities = 2;
}
```

### Capability strings

Capability strings are exact-match (no version negotiation, no regex). New capabilities are additive. Each wire shape pairs with one capability identifier; a wire-shape change lands as a new `_vN` identifier. The naming convention is `domain.subdomain.capability_name_v{N}`; the suffix is part of the identity, never decoded as a version field.

Capability strings are the deprecation boundary. A wire-shape change lands as a new `_vN` capability; removing an older capability requires the architecture doc for that surface to name the consumer constraint and removal rule.

The active list mirrors [`ZINDER_CAPABILITIES`](../../crates/zinder-proto/src/capabilities.rs); a CI test (`zinder-proto::integration::capability_docs::public_interfaces_capability_list_mirrors_zinder_capabilities`) fails when this list and the constant diverge, so any drift is caught at build time.

<!-- capability-list:public-interfaces:start -->
- `wallet.read.latest_block_v1`
- `wallet.read.block_id_by_selector_v1`
- `wallet.read.block_header_by_selector_v1`
- `wallet.read.compact_block_at_v1`
- `wallet.read.compact_block_range_v1`
- `wallet.read.tree_state_checkpoint_v1`
- `wallet.read.latest_tree_state_checkpoint_v1`
- `wallet.read.subtree_roots_in_range_v1`
- `wallet.read.transaction_by_id_v1`
- `wallet.read.server_info_v1`
- `wallet.broadcast.transaction_v1`
- `wallet.events.chain_v1`
- `wallet.snapshot.mempool_v1`
- `wallet.events.mempool_v1`
- `wallet.mempool.transparent_outputs_by_address_v1`
- `wallet.mempool.transparent_spend_by_outpoint_v1`
- `wallet.mempool.transparent_outputs_by_outpoint_v1`
- `wallet.read.transparent_outputs_by_outpoint_v1`
- `wallet.read.chain_value_pools_at_tip_v1`
- `wallet.address.output_index_v1`
- `wallet.address.transparent_history_v1`
- `wallet.address.transparent_balance_v1`
- `explorer.server_info_v1`
- `explorer.transparent_address.balance_v1`
- `explorer.transaction.detail_v1`
- `explorer.block.summary_v1`
- `explorer.block.detail_v1`
- `explorer.search_v1`
- `explorer.mempool.summary_v1`
- `explorer.mempool.activity_v1`
- `explorer.transparent_address.activity_v1`
- `explorer.fee.summary_v1`
- `explorer.value_pool.summary_v1`
- `explorer.mempool.event_counts_v1`
- `explorer.transaction.fees_v1`
- `explorer.transaction.recent_v1`
- `explorer.overview.snapshot_v1`
<!-- capability-list:public-interfaces:end -->

`wallet.broadcast.transaction_v1` is deployment-gated: binaries support the RPC, but `ServerInfo` advertises it only when a transaction broadcaster is configured and its source probe reports `transaction_broadcast`. Read-only query deployments return `FailedPrecondition` from the RPC and omit the capability.

`WalletQuery.TransparentAddressBalance` is always answered. `wallet.address.transparent_balance_v1` is the always-on canonical-confirmed path (computed at read time from the transparent output column family, no mempool overlay). `explorer.transparent_address.balance_v1` coexists with it when `zinder-explorer` is configured and ready, signalling that the same response carries the live-mempool overlay in `unconfirmed_delta_zat`. Clients that need the overlay must gate on the explorer capability; clients that just need confirmed totals can rely on the wallet capability being present whenever the method itself is exposed. The dual-capability federation rule generalizes to every future federated explorer method per [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md).

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
- `finalized_height`
- `readiness_probe`
- `transaction_broadcast`
- `json_rpc`
- `openrpc_discovery`
- `chain_value_pools`

Do not advertise future source capabilities such as block-stream ingestion or
spending-transaction lookup until the source adapter and runtime wiring both
exist. `chain_value_pools` is source-backed by
`getblockchaininfo.valuePools`; the wallet and explorer read planes proxy it
through the ingest writer instead of opening independent upstream-node handles.

## Wire Conventions

Native to wire identifier translations live in `crates/zinder-core/src/wire/` and only there. Files are organized by concept (`transaction_id`, `block_hash`, `chain_name`, `branch_id`), not by dialect; every dialect for one concept shares one file.

When adding a new wire field or a new ingress dialect, locate or add a function in that module before writing any boundary code. The following inline forms are forbidden anywhere outside the wire module:

- `transaction_id.as_bytes()` and `block_hash.as_bytes()` at a wire boundary. Use `encode_internal_transaction_id` or `encode_internal_block_hash`.
- `format!("{:08x}", branch_id)` for wire output. Use `encode_branch_id_hex`.
- Inline hex-string transaction id or block hash decode. Use `decode_display_*_hex`.
- Hardcoded capability literals. Import the `pub const` from [`crates/zinder-proto/src/capabilities.rs`](../../crates/zinder-proto/src/capabilities.rs).
- Duplicate `Network` to wire-string tables. Use `encode_bip70_chain_name` (`"main"`/`"test"`, BIP70/lightwalletd/Zebra JSON-RPC) or `encode_zinder_native_chain_name` (`"zcash-mainnet"`/`"zcash-testnet"`/`"zcash-regtest"`, native config and protobuf).

Two integration tests enforce the rules on every CI invocation: [`crates/zinder-core/tests/integration/wire_invariants.rs`](../../crates/zinder-core/tests/integration/wire_invariants.rs) and [`crates/zinder-proto/tests/integration/capability_string_uniqueness.rs`](../../crates/zinder-proto/tests/integration/capability_string_uniqueness.rs).

Decode failures return `zinder_core::wire::WireDecodeError`. Encode operations are infallible by construction.

## Rust API Shape

The public API should make a normal integration obvious without implying that production services share storage handles.

### Wallet integrators (gRPC client)

Wallet and application developers integrate with the native protobuf service. The generated client name is `WalletQueryClient`:

```rust
use zinder_proto::v1::wallet::{
    wallet_query_client::WalletQueryClient, CompactBlockRangeRequest, LatestBlockRequest,
};

let mut wallet = WalletQueryClient::connect("https://zinder.example").await?;
let tip = wallet.latest_block(LatestBlockRequest {}).await?;
let blocks = wallet
    .compact_block_range(CompactBlockRangeRequest {
        start_height: 1_000_000,
        end_height: 1_000_100,
    })
    .await?;
```

### Rust integrators (Zallet, future SDKs)

Rust consumers depend on `zinder-client`, which exports the typed `ChainIndex` trait with two implementations: `LocalChainIndex` for colocated consumers (RocksDB-secondary reads plus an explicit subscription endpoint), and `RemoteChainIndex` for cross-host consumers (full gRPC). Both implement the same trait; the consumer picks by deployment topology.

Colocated Zallet (running on the same host as the Zinder cluster):

```rust
use zinder_client::{ChainIndex, LocalChainIndex, LocalOpenOptions, BlockHeight};
use std::time::Duration;
use tokio_stream::StreamExt as _;

let chain = LocalChainIndex::open(LocalOpenOptions {
    storage_path: "/var/lib/zinder".into(),
    secondary_path: "/var/lib/zinder/zallet-secondary".into(),
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

Cross-host Zallet (running on a different host from the Zinder cluster):

```rust
use zinder_client::{ChainIndex, RemoteChainIndex, RemoteOpenOptions};

let chain = RemoteChainIndex::connect(RemoteOpenOptions {
    endpoint: "http://zinder.internal:9101".into(),
    network: zinder_client::Network::ZcashTestnet,
}).await?;

// Same trait methods as LocalChainIndex; consumer code is identical.
let tip = chain.latest_block().await?;
```

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

Storage and cursor byte contracts are lower-level than the normal public API, but their names are stable and searchable: `StoreKey`, `ArtifactEnvelopeHeaderV1`, `StreamCursorTokenV1`, and `MempoolStreamCursorV1`. Mechanism-shaped names such as `key_codec`, `cursor_helper`, or `bytes_utils` are forbidden.

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
- Use `ChainTipMetadata` for chain-derived wallet counters at the visible tip, such as Sapling and Orchard note commitment tree sizes. Do not make query code rediscover those counters by decoding wallet protocol payloads. The proto `ChainEpoch` message carries `sapling_commitment_tree_size` and `orchard_commitment_tree_size` directly.
- Bulk-catchup ranges that publish `ChainTipMetadata` must be contiguous with a known metadata base. Fresh stores start at height 1; non-empty stores append after the current tip; checkpoint-bounded stores start at `SourceChainCheckpoint.height + 1` after ingest seeds the builder from the checkpoint's chain-global tree sizes.
- Wallet-serving coverage is selected with `ingest.coverage = "wallet-serving"` or `zinder-ingest --wallet-serving`. Per [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md), this is a consumer-neutral serving-store profile, not a Zashi-specific mode. In that mode, ingest derives the bulk-catchup floor and `checkpoint_height` from upstream-node-advertised activation heights; explicit height overrides and `allow_near_tip_finalize` are rejected so serving stores do not silently become recent-checkpoint or near-tip-finalized fixtures.
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
