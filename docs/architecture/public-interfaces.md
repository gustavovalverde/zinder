# Public Interfaces

This document is the vocabulary spine. Every other architecture doc, ADR, ingest path, query path, error variant, and configuration field defers to the conventions here. When you add a new public type, method, error, config field, or proto message, the reviewer's first question is "does this match the spine?"

Zinder's public interfaces should be boring, searchable, and hard to misuse. The names chosen here will be copied by contributors, downstream wallets, operators, and code-extending agents.

Optimization order:

1. **Developer Experience (DX).** A wallet, explorer, or application developer integrating Zinder can find any capability through search and complete an integration without reading internals.
2. **Agent Experience (AX).** An LLM coding agent extending Zinder can place new code in the right module, guess the right name on first try, extend an existing pattern by example, and discover capabilities at runtime.
3. **User Experience (UX).** An operator can read `--print-config`, `/readyz`, `/metrics`, or a `ServerInfo` response and know what to do without reading source.
4. **Contributor experience.** A new contributor finds checklists for common operations (adding an artifact family, RPC method, or error variant) without tribal knowledge. The cookbook lives at [Extending Artifacts](extending-artifacts.md).

## Vocabulary

Use these names consistently. When a term is marked as M2 or M3, it is canonical vocabulary for that milestone but must not be advertised through `ServerCapabilities` until the owning implementation lands.

### Product and runtimes

| Term | Meaning |
|------|---------|
| `Zinder` | The product |
| `zinder-ingest` | Production service that owns chain ingestion and canonical writes |
| `zinder-query` | Production service that serves wallet and application APIs from epoch-bound indexed state |
| `zinder-compat-lightwalletd` | Compatibility adapter that serves vendored lightwalletd gRPC over `WalletQueryApi` |
| `zinder-derive` | Optional service for replayable derived indexes |
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
| `ChainEventEnvelope` | Cursor-bound chain-event message carried over the ingest subscription plane and exposed natively on `WalletQuery.ChainEvents` in M2 |
| `ChainTipMetadata` | Chain-derived counters at the visible tip (Sapling and Orchard tree sizes) |
| `BlockArtifact` | Durable artifact derived from a block |
| `CompactBlockArtifact` | Wallet-oriented compact block artifact |
| `BlockId` | Stable block identity (`{ height: BlockHeight, hash: BlockHash }`); lives in `zinder-core` and is the canonical (height, hash) pair across the source boundary, the wallet protocol, and the reader API |
| `FinalizedBlockStore` | Storage boundary for finalized chain data |
| `ReorgWindow` | Non-finalized range where reorgs are expected and supported |
| `MempoolEntry` | M3 term for one transaction currently observed in the mempool |
| `MempoolEvent` | M3 typed mempool transition (`Added`, `Invalidated`, `Mined`) carried in the event log |
| `MempoolEventEnvelope` | M3 cursor-bound mempool-event message exposed natively on `WalletQuery.MempoolEvents` |
| `MempoolSnapshotView` | M3 bounded, pageable point-in-time projection of the live mempool with `snapshot_age_millis` |
| `TransparentMempoolOutputsRequest` | M3 bounded transparent-address request for outputs currently visible in the mempool index |
| `TransparentMempoolOutput` | M3 transparent output currently visible in the mempool index |
| `TransparentMempoolSpend` | M3 transparent outpoint-spend relationship currently visible in the mempool index |

### Source Boundary

| Term | Meaning |
|------|---------|
| `NodeSource` | Rust trait for configured source adapters in `zinder-source` |
| `NodeCapabilities` | Capability descriptor detected from the selected source |
| `NodeAuth` | Typed source authentication configuration |
| `MempoolSourceEvent` | M3 source-level mempool observation normalized from source streams or polling diffs |
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
| `BroadcastTransactionRequest` | M2 native wallet protocol request to submit a raw transaction |
| `BroadcastTransactionResponse` | M2 native wallet protocol typed broadcast outcome |
| `ChainEventsRequest` | M2 native wallet protocol request for `WalletQuery.ChainEvents` chain-event subscription |
| `MempoolEventsRequest` | M3 native wallet protocol request for `WalletQuery.MempoolEvents` mempool-event subscription |
| `MempoolSnapshotRequest` | M3 native wallet protocol request for `WalletQuery.MempoolSnapshot` |
| `MempoolSnapshotResponse` | M3 native wallet protocol response carrying the live mempool view |
| `ServerInfoRequest` | Native wallet protocol capability-descriptor request |
| `ServerInfoResponse` | Native wallet protocol capability-descriptor response |
| `ServerCapabilities` | Capability descriptor advertised by `zinder-query` to clients |

### Cursors, events, errors

| Term | Meaning |
|------|---------|
| `StreamCursorTokenV1` | Opaque cursor body for chain-event subscriptions; fork-aware, encodes epoch id, last visible block hash, in-epoch offset, and stream-family tag |
| `MempoolStreamCursorV1` | M3 opaque cursor body for mempool-event subscriptions |
| `ChainEventStreamFamily` | Stream-family enum used inside chain-event cursor bodies (`Tip`, `Finalized`; `Mempool` is a reserved M3 family code, not an active chain-event family) |
| `ArtifactFamily` | Open-ended enum naming an artifact family in storage and query errors |
| `ArtifactKey` | Open-ended enum union of keys used to look up an artifact (`BlockHeight`, `TransactionId`, `SubtreeRootIndex`, `BlockTransactionIndex`, future variants) |

### Configuration

| Term | Meaning |
|------|---------|
| `TipFollowConfig` | Service-specific configuration for polling the upstream node tip and committing live changes |
| `RetentionConfig` | Service-specific configuration for chain-event and mempool-event retention windows |
| `secondary_path` | Process-unique RocksDB secondary metadata directory for a colocated reader |
| `ingest_control_addr` | Private ingest-control gRPC endpoint used by secondary readers to compute replica lag and proxy chain-event subscriptions |

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

- For a `BlockHeight` key, use `{artifact}_at(height)`. Example: `block_at(height)`, `compact_block_at(height)`, `tree_state_at(height)`.
- For any other unique key, use `{artifact}_by_{key_noun}(key)`. Example: `transaction_by_id(txid)`, `block_by_hash(hash)`.

The `_at` suffix is reserved for height; using `_at` for any non-height key is a convention violation.

### Rule 2 — Bounded range reads

Methods that read a contiguous range of artifacts, returning a stream or vector:

- Always plural, always `_in_range` suffix. Example: `compact_blocks_in_range(range)`, `subtree_roots_in_range(range)`, `transactions_in_range(range)`.
- The argument is always a `RangeInclusive<BlockHeight>` or a domain range type (`SubtreeRootRange`).

### Rule 3 — Tip-pinned reads with no key

Methods that return the artifact at the visible tip with no caller-supplied key:

- `latest_{artifact}()`. Example: `latest_block()`, `latest_tree_state()`.

### Rule 4 — Stream subscriptions

Methods that return a server-streaming subscription with cursor resume:

- `{event_kind}_events(from_cursor)` returning `impl Stream<Item = Result<{EventKind}Envelope, _>>`.
- Example: `chain_events(from_cursor)`, `mempool_events(from_cursor)`, future `derive_events(from_cursor)`.
- The request field carrying the cursor is always `from_cursor` (see Cursor Conventions below).
- The envelope field carrying the cursor position is always `cursor`.

### Rule 5 — Capability and identity probes

Methods that ask the server about itself rather than about chain data:

- `server_info()` returning `ServerCapabilities`.
- `tip_id()` returning the visible tip identity as `BlockId { height, hash }`. (Lifted from `NodeSource` for symmetry with the read API; see the lazy-catchup short-circuit in [ADR-0007](../adrs/0007-multi-process-storage-access.md).)

### Rule 6 — Verb forms inside `zinder-source`

The source boundary uses `fetch_*` for outbound calls because the result is a remote observation, not a local read. `fetch_block_by_height(height)` is consistent with Rule 1; `tip_id()` follows Rule 5 and uses no `fetch_` prefix because the noun-named accessor matches the local-API symmetry.

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
- HMAC over the body, per [ADR-0005](../adrs/0005-chain-event-cursor-sequence.md), so a tampered cursor returns `EventCursorInvalid` rather than serving wrong data.

A cursor whose `last_visible_block_hash` is no longer present at its `epoch_id`'s tip indicates the client missed a reorg. The server emits a synthetic `ChainReorged` envelope describing the divergence before resuming the stream. Clients never see "silent" branch changes.

`MempoolStreamCursorV1` is reserved for M3. It uses the reserved `family = Mempool` code with mempool-specific position fields, defined in [M3 Mempool](../specs/m3-mempool.md). It is not an active M2 cursor type.

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
    TransparentAddressUtxo,
    TransparentUtxoSpend,
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

[node]
source = "zebra-json-rpc"
json_rpc_addr = "127.0.0.1:8232"
request_timeout_ms = 30000
max_response_bytes = 16777216

[node.auth]
method = "basic"
username = "..."
password = "..."

[storage]
path = "/var/lib/zinder"
max_size_bytes = 1099511627776
sync_writes = true
# Reader-only knobs (zinder-query, zinder-compat-lightwalletd) per ADR-0007:
secondary_path = "/var/lib/zinder/query-secondary"
secondary_catchup_interval_ms = 250
secondary_replica_lag_threshold_chain_epochs = 4
ingest_control_addr = "http://127.0.0.1:9100"
chain_event_retention_hours = 168 # descriptor mirror for zinder-query ServerInfo

[ingest]
reorg_window_blocks = 100
commit_batch_blocks = 1000

[ingest.retention]
chain_event_retention_hours = 168
chain_event_retention_check_interval_ms = 60000
cursor_at_risk_warning_hours = 24
# M3 only:
mempool_mined_retention_minutes = 60
mempool_invalidated_retention_hours = 24

[ingest.control]
listen_addr = "127.0.0.1:9100"

[backfill]
from_height = 1
to_height = 1000000
allow_near_tip_finalize = false

[backup]
to_path = "/var/backups/zinder/checkpoint-2026-04-28"

[tip_follow]
poll_interval_ms = 1000

[mempool]
# M3 only:
poll_interval_ms = 10000
max_snapshot_age_ms = 30000

[query]
listen_addr = "127.0.0.1:9101"
max_compact_block_range = 1000

[query.grpc]
enable_reflection = true
enable_health = true

[compat]
listen_addr = "127.0.0.1:9067"
```

### Unit suffix rule

Every duration field uses the `_ms` suffix. Sub-second granularity is supported by some operators; mixing `_secs` and `_ms` makes operator scripts error-prone.

| Unit | Suffix | Example |
|------|--------|---------|
| Milliseconds | `_ms` | `request_timeout_ms`, `poll_interval_ms` |
| Bytes | `_bytes` | `max_response_bytes`, `max_size_bytes` |
| Block count | `_blocks` | `reorg_window_blocks`, `commit_batch_blocks` |
| Hour count | `_hours` | `chain_event_retention_hours` |
| Minute count | `_minutes` | `mempool_mined_retention_minutes` |
| Bare counts (events, items) | `_events`, `_entries`, `_chunks` | `max_event_history_entries` |

A bare count (`max_compact_block_range`) is acceptable when the unit is intrinsic to the field name (a "block range" is a count of blocks).

### Environment variable mapping

`ZINDER_<SECTION>__<FIELD>` is the convention. Nested sections double-underscore: `ZINDER_NODE__JSON_RPC_ADDR`, `ZINDER_INGEST__RETENTION__CHAIN_EVENT_RETENTION_HOURS`. Sensitive leaves (`password`, `secret`, `token`, `cookie`, `private_key`) are rejected from environment variables; they must come from the TOML file or a secret manager.

### `--print-config`

Every production binary exposes `--config` and `--print-config`. The print form must show explicit `[REDACTED]` markers for sensitive fields. The output should round-trip: feeding `--print-config` back as `--config` produces the same effective configuration.

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
  uint64 chain_event_retention_seconds = 6;      // 0 = unbounded
  uint64 mempool_mined_retention_seconds = 7;    // 0 until M3 support exists
  uint64 mempool_invalidated_retention_seconds = 8; // 0 until M3 support exists
  repeated string capabilities = 9;              // capability strings; clients match exact strings
  repeated DeprecatedCapability deprecated_capabilities = 10;
  NodeCapabilitiesDescriptor node = 11;
  optional string mcp_endpoint = 12;             // unset in v1
}

message DeprecatedCapability {
  string capability = 1;
  optional string replaced_by = 2;
  string earliest_removal_version = 3;
  string deprecation_notice = 4;
}

message NodeCapabilitiesDescriptor {
  optional string version = 1;
  repeated string capabilities = 2;
}
```

### Capability strings

Capability strings are exact-match (no version negotiation, no regex). New capabilities are additive. Removing a capability from a deployed server is a breaking change. The naming convention is `domain.subdomain.capability_name_v{N}`.

Current active wallet capabilities:

- `wallet.broadcast.transaction_v1`
- `wallet.events.chain_v1`
- `wallet.read.compact_block_range_v1`
- `wallet.read.compact_block_at_v1`
- `wallet.read.latest_block_v1`
- `wallet.read.latest_tree_state_v1`
- `wallet.read.server_info_v1`
- `wallet.read.subtree_roots_in_range_v1`
- `wallet.read.transaction_by_id_v1`
- `wallet.read.tree_state_at_v1`

`wallet.broadcast.transaction_v1` is deployment-gated: binaries support the RPC, but `ServerInfo` advertises it only when a transaction broadcaster is configured. Read-only query deployments return `FailedPrecondition` from the RPC and omit the capability.

M2 wallet-compatibility hardening capability strings, reserved but not
advertised until the native proto surface exists:

- `wallet.address.transparent_utxos_v1` (advertised only when the native
  `WalletQuery` proto and `zinder-client::ChainIndex` expose transparent UTXO
  reads backed by stored artifacts; the lightwalletd compatibility adapter
  already uses that stored-artifact path)

M3 and later capability strings, reserved but not advertised in M2:

- `wallet.events.mempool_v1`
- `wallet.snapshot.mempool_v1`
- `wallet.address.transparent_history_v1` (added when transparent-address history lands)
- `wallet.address.transparent_balance_v1` (added when transparent-address balance lands)

Do not add native capability strings for lightwalletd-shaped mempool products
such as raw-transaction streams or compact-transaction streams. Those are
compatibility adapter views derived from `MempoolSnapshot` and `MempoolEvents`;
the native capability vocabulary stays on snapshot and event semantics.

Source capability strings live under `ServerCapabilities.node.capabilities`:

- `node.streaming_source` (advertised when ingest consumes Zebra's gRPC indexer stream)
- `node.spending_tx_lookup` (advertised when Zebra's `--features indexer` is detected)
- `node.openrpc_discovery` (advertised when a JSON-RPC source successfully parses `rpc.discover`)
- `node.json_rpc` (advertised when JSON-RPC is the active source backend)

### Deprecation policy

When a capability is deprecated, `capabilities` continues to advertise it, and a parallel `deprecated_capabilities` list names the replacement and earliest removal version. Clients compatible with the deprecated capability keep working; clients reading the deprecation list can plan migration. Capabilities are not silently removed.

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
    subscription_endpoint: Some("http://127.0.0.1:9101".into()),
    catchup_interval: Duration::from_millis(250),
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

[Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape) defines the `zinder-client` shape. ADR-0007 owns the multi-process model that makes both implementations necessary.

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

Storage and cursor byte contracts are lower-level than the normal public API, but their names are stable and searchable: `StoreKey`, `ArtifactEnvelopeHeaderV1`, and `StreamCursorTokenV1` for M2, plus reserved `MempoolStreamCursorV1` for M3. Mechanism-shaped names such as `key_codec`, `cursor_helper`, or `bytes_utils` are forbidden.

## Crate Boundaries

Workspace shape:

```text
crates/
  zinder-core/
  zinder-store/
  zinder-source/
  zinder-proto/
  zinder-client/
  zinder-testkit/
services/
  zinder-ingest/
  zinder-query/
  zinder-compat-lightwalletd/
  zinder-derive/
```

Add a crate only when it has a stable domain boundary and enough behavior to justify its interface. The current set is the target list, not a command to create every crate immediately.

## Contract Hygiene

Public shapes describe behavior that production code can actually reach.

- Public event variants, error variants, API transitions, cursor fields, and proto surfaces must be produced, consumed, or explicitly reserved by the owning architecture document.
- Delete unreachable public variants while Zinder has no compatibility burden. Do not keep fallback variants only because they might be useful later.
- Names identify the source of truth. Use `created_at` for the wall-clock time when Zinder created a record. Use a chain-derived name such as `tip_block_time_millis` when the value comes from block header time.
- Use `ChainTipMetadata` for chain-derived wallet counters at the visible tip, such as Sapling and Orchard note commitment tree sizes. Do not make query code rediscover those counters by decoding wallet protocol payloads. The proto `ChainEpoch` message carries `sapling_commitment_tree_size` and `orchard_commitment_tree_size` directly.
- Backfill ranges that publish `ChainTipMetadata` must be contiguous with a known metadata base. Fresh stores start at height 1; non-empty stores append after the current tip; checkpoint-bounded stores start at `SourceChainCheckpoint.height + 1` after ingest seeds the builder from the checkpoint's chain-global tree sizes.
- Wallet-serving backfill coverage is selected with `backfill.coverage = "wallet-serving"` or `zinder-ingest backfill --wallet-serving`. Per [ADR-0008](../adrs/0008-consumer-neutral-wallet-data-plane.md), this is a consumer-neutral serving-store profile, not a Zashi-specific mode. In that mode, ingest derives `from_height` and `checkpoint_height` from upstream-node-advertised activation heights; explicit height overrides and `allow_near_tip_finalize` are rejected so serving stores do not silently become recent-checkpoint or near-tip-finalized fixtures.
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

## Cross-references

- [Extending Artifacts cookbook](extending-artifacts.md) — the agent-extensibility checklist for adding artifact families, RPC methods, and error variants.
- [ADR-0005: Chain event cursor sequence](../adrs/0005-chain-event-cursor-sequence.md) — the cursor body authentication contract.
- [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) — the chain-event subscription wire shape.
- [Public interfaces §Rust API Shape](public-interfaces.md#rust-api-shape) — typed Rust client surface.
- [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure) — retention windows and pruning.
- [M3 Mempool](../specs/m3-mempool.md) — reserved mempool index, event log, and protocol surface.
- [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery) — `ServerInfo` shape and deprecation rules.
