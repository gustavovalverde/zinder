# Protocol Boundary

Zinder has one protocol owner: `zinder-proto`. All service `.proto` files, vendored lightwalletd-compatible protos, generated Rust modules, and method-level wire documentation live behind that crate.

This document owns the wire-schema boundary. Source adapters live in [Node source boundary](node-source-boundary.md). Storage bytes live in [ADR-0002](../adrs/0002-boundary-specific-serialization.md).

## Protocol Surfaces

Zinder exposes three protocol families:

| Surface | Rust module | Served by | Purpose |
| ------- | ----------- | --------- | ------- |
| Native wallet/application API | `zinder_proto::v1::wallet` | `zinder-query` | Future-facing Zinder query API |
| Private ingest control API | `zinder_proto::v1::ingest` | `zinder-ingest` | Writer status and retained chain-event replay for secondary readers |
| Internal epoch API | `zinder_proto::v1::epoch` | `zinder-ingest` or read sidecar | Epoch-bound canonical reads and chain-event history |
| Lightwalletd compatibility API | `zinder_proto::compat::lightwalletd` | `zinder-compat-lightwalletd` | Existing wallet migration path |

Native protocol naming must not copy lightwalletd names unless the concept is truly identical. Compat protocol naming must not be rewritten into Zinder terminology because it is a compatibility contract.

## `zinder-proto` Ownership

`zinder-proto` owns:

- Vendored `compact_formats.proto` and `service.proto` files for lightwalletd compatibility.
- Native Zinder `.proto` files.
- Generated `prost` Rust modules and generated `tonic` client/server modules.
- Golden wire fixtures for compatibility.
- Method-level Markdown docs when generated proto docs are not enough.

No service crate should hand-write `prost::Message` structs for protocol payloads. No service crate should generate its own copy of a shared proto. No domain crate should expose generated proto types in its public API.

Vendored compatibility protos are a pinned external contract. Before implementing
or extending `CompactTxStreamer`, compare the vendored files with the upstream
`zcash/lightwallet-protocol` source, then either update them and record the
source commit or document the pinned version and scope compatibility claims to
that version. Do not call a surface "current lightwalletd-compatible" when the
vendored proto is intentionally older.

The current compatibility pin is `zcash/lightwallet-protocol` commit
`dd0ea2c3c5827a433e62c2f936b89efa2dec5a9a`, verified on 2026-04-27. The
machine-readable source provenance lives in
`crates/zinder-proto/proto/compat/lightwalletd/UPSTREAM.md`, and
`crates/zinder-proto/tests/lightwalletd_protocol.rs` is the local golden decode
guard for the current compatibility message shapes. The `vendored proto drift` CI
job compares the normalized vendored files with that pinned upstream commit so
drift is a deliberate compatibility update, not an accidental edit.

Storage-control protobuf records that never cross a service or API boundary may live in `zinder-store`, not `zinder-proto`. The rule is ownership, not file extension: service protocols belong to `zinder-proto`; storage-private control records belong to storage.

## Native API

`zinder-query` serves the native `WalletQuery` protobuf service over the Rust
`WalletQueryApi` boundary.

`service WalletQuery` is defined in `zinder/v1/wallet/wallet.proto`. Tonic
server/client code is generated only for the native Zinder service; the
vendored lightwalletd protos remain message-only and are wired through
`zinder-compat-lightwalletd`.

The current native wallet read-sync surface is:

- `zinder_proto::v1::wallet::ChainEpoch`
- `zinder_proto::v1::wallet::BlockId`
- `zinder_proto::v1::wallet::LatestBlockRequest`
- `zinder_proto::v1::wallet::LatestBlockResponse`
- `zinder_proto::v1::wallet::CompactBlock`
- `zinder_proto::v1::wallet::CompactBlockRangeRequest`
- `zinder_proto::v1::wallet::CompactBlockRequest`
- `zinder_proto::v1::wallet::CompactBlockResponse`
- `zinder_proto::v1::wallet::CompactBlockRangeChunk`
- `zinder_proto::v1::wallet::TransactionRequest`
- `zinder_proto::v1::wallet::Transaction`
- `zinder_proto::v1::wallet::TransactionResponse`
- `zinder_proto::v1::wallet::TreeStateRequest`
- `zinder_proto::v1::wallet::LatestTreeStateRequest`
- `zinder_proto::v1::wallet::TreeStateResponse`
- `zinder_proto::v1::wallet::ShieldedProtocol`
- `zinder_proto::v1::wallet::SubtreeRootsRequest`
- `zinder_proto::v1::wallet::SubtreeRoot`
- `zinder_proto::v1::wallet::SubtreeRootsResponse`
- `zinder_proto::v1::wallet::ServerInfoRequest`
- `zinder_proto::v1::wallet::ServerInfoResponse`
- `zinder_proto::v1::wallet::ServerCapabilities`
- `zinder_proto::v1::wallet::DeprecatedCapability`
- `zinder_proto::v1::wallet::NodeCapabilitiesDescriptor`
- `zinder_proto::v1::wallet::wallet_query_server::WalletQuery`
- `zinder_proto::v1::wallet::wallet_query_client::WalletQueryClient`

M2 adds the following native wallet messages and RPCs:

- `zinder_proto::v1::wallet::BroadcastTransactionRequest`
- `zinder_proto::v1::wallet::BroadcastTransactionResponse`
- `zinder_proto::v1::wallet::ChainEventsRequest`
- `zinder_proto::v1::wallet::ChainEventEnvelope`
- `zinder_proto::v1::wallet::ChainCommitted`
- `zinder_proto::v1::wallet::ChainReorged`

M3 reserves, but does not expose in M2, the following native wallet messages and RPCs:

- `zinder_proto::v1::wallet::MempoolEventsRequest`
- `zinder_proto::v1::wallet::MempoolEventEnvelope`
- `zinder_proto::v1::wallet::MempoolSnapshotRequest`
- `zinder_proto::v1::wallet::MempoolSnapshotResponse`

`services/zinder-query/src/grpc/adapter.rs` owns `WalletQueryGrpcAdapter`, a
thin tonic adapter over `WalletQueryApi`. Generated native response shaping
lives in `services/zinder-query/src/grpc/native.rs`. The adapter must not open
storage, call upstream nodes, or build missing artifacts; those responsibilities
stay behind `WalletQueryApi`, `ChainEpochReadApi`, and ingestion.

The current native network service exposes Zinder concepts:

- `LatestBlock`, `CompactBlock`, `CompactBlockRange`, `Transaction`
- `TreeState`, `LatestTreeState`, `SubtreeRoots`
- `ServerInfo`

M2 adds `BroadcastTransaction` and `ChainEvents` with Tip and Finalized cursor families per [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) and [§D2](../public-interfaces.md). M3 adds `MempoolEvents` and `MempoolSnapshot` per [M3 Mempool](../specs/m3-mempool.md).

Native M2 read requests that depend on canonical chain state carry an optional
`at_epoch` field. When it is absent, the server answers from the visible epoch
at request time. When it is present, the server must answer from that exact
`ChainEpoch` or return `Code::FailedPrecondition` with a structured
`CHAIN_EPOCH_PIN_*` detail.

## Private Ingest Control API

`zinder-ingest` serves the private `IngestControl` protobuf service for
colocated readers. This is not a wallet-facing API.

`service IngestControl` is defined in `zinder/v1/ingest/ingest.proto`. The current
surface is:

- `zinder_proto::v1::ingest::WriterStatusRequest`
- `zinder_proto::v1::ingest::WriterStatusResponse`
- `zinder_proto::v1::wallet::ChainEventsRequest`
- `zinder_proto::v1::wallet::ChainEventEnvelope`
- `zinder_proto::v1::ingest::ingest_control_server::IngestControl`
- `zinder_proto::v1::ingest::ingest_control_client::IngestControlClient`

Future native slices may add:

- Transparent address artifacts (paginated `TransparentAddressTxIdsInRange`, `TransparentAddressUtxosAt`, `TransparentAddressBalanceAt`)
- Internal `ChainEpochReadApi` over gRPC for multi-process query mode

Every chain-dependent response either binds to one `ChainEpoch` or explicitly says why a field is independent of the epoch. Range and list APIs must have cursors or explicit maximum sizes.

## Internal Epoch API

`ChainEpochReadApi` is the internal read surface for epoch-bound canonical reads. It exposes retained chain-event history to `IngestControl.ChainEvents`; wallet-facing replay/live delivery is public through `WalletQuery.ChainEvents`. The `zinder-query` binary proxies public chain-event streams to the private ingest-control endpoint, while direct `WalletQueryGrpcAdapter::new` usage keeps a secondary-store fallback for embedded and integration-test contexts.

It should provide:

- `resolve_chain_epoch`
- `read_compact_block_range`
- `read_tree_state`
- `read_latest_tree_state`
- `read_subtree_roots`
- `read_transaction`
- `chain_event_history` (library/internal helper for the subscription plane, not a second public stream)

The exact RPC method names can differ if the proto review finds clearer names, but they must preserve the same concepts. Do not use vague names such as `get_data`, `query_store`, or `epoch_history` when the operation is replaying chain events.

## Lightwalletd Compatibility

`zinder-compat-lightwalletd` adapts `WalletQueryApi` to the vendored `CompactTxStreamer` schema.

It may:

- Translate request and response shapes.
- Map Zinder typed errors to lightwalletd-compatible status codes.
- Preserve lightwalletd streaming behavior for block ranges.
- Run compatibility tests against lightwalletd fixtures.

It must not:

- Open live canonical storage.
- Call upstream nodes.
- Build compact blocks.
- Run migrations.
- Define a second wallet query model.
- Modify vendored proto files except through an explicit compatibility update.

The compatibility adapter must not advertise working lightwalletd wallet sync
until stored compact-block artifacts contain the block identity fields, compact
transaction data, and commitment-tree sizes required by the pinned lightwalletd
contract. If the pinned protocol says a field such as `CompactBlock.header`
should be empty, the builder and tests must follow that contract instead of
using a non-empty value as a proof of completeness. Decoding an empty protobuf
shell is a fixture milestone, not a compatibility claim.

The compatibility adapter consumes `WalletQueryApi`; it does not own storage,
source I/O, or artifact construction. Its production claim is limited to the
read-sync methods below and to the range shapes validated by live
regtest/testnet tests.
For non-genesis public-history ranges, ingestion must already have a contiguous
stored tip metadata base or a future source backend for chain-global tree
sizes.

The minimum lightwalletd-compatible read-sync surface is:

- `GetLightdInfo`
- `GetLatestBlock`
- `GetBlock`
- `GetBlockRange`
- `GetTreeState`
- `GetLatestTreeState`
- `GetSubtreeRoots`
- `GetAddressUtxos`
- `GetAddressUtxosStream`

Android/Zashi compatibility claims are governed by
[Wallet data plane §External Wallet Compatibility Claims](wallet-data-plane.md#external-wallet-compatibility-claims).
`GetLightdInfo.taddrSupport` may be true only when `GetAddressUtxos[Stream]`
reads from stored transparent UTXO artifacts. The compatibility service must
not claim broader Zashi compatibility until the full external-wallet contract
is satisfied.

`SendTransaction` forwards `request.data` to
`WalletQueryApi::broadcast_transaction` and maps each
`TransactionBroadcastResult` variant into a `lightwalletd::SendResponse` shape:

| Outcome | `errorCode` | `errorMessage` |
| ------- | ----------- | -------------- |
| `Accepted` | `0` | display-hex transaction id |
| `InvalidEncoding` | upstream node code or `-22` | upstream node message |
| `Rejected` | upstream node code or `-26` | upstream node message |
| `Duplicate` | upstream node code or `-27` | upstream node message |
| `Unknown` | upstream node code or `-1` | upstream node message |

## Native Single-Artifact RPCs

`WalletQuery` exposes single-artifact lookups alongside the streaming and batch RPCs:

- `CompactBlock(CompactBlockRequest) returns (CompactBlockResponse)` — fetch one indexed compact block by height.
- `Transaction(TransactionRequest) returns (TransactionResponse)` — fetch one indexed transaction by transaction id.

Both responses are bound to one `ChainEpoch` so wallets cannot mix epochs across follow-up requests. The lightwalletd compat adapter routes `GetBlock` to `compact_block_at` and `GetTransaction` to `transaction`. The compat layer reverses transaction-hash bytes to translate lightwalletd display order to the canonical Zinder `TransactionId` byte order.

The defaults reuse Bitcoin/Zcash JSON-RPC error-code conventions so wallet
clients that already track those codes do not need a Zinder-specific table.
When the upstream node reports its own `error_code`, that code is forwarded
unchanged so operators can correlate Zinder responses with upstream node logs.

`SendTransaction` does not mutate canonical storage; the request is forwarded to
the configured source broadcaster and the typed outcome is reported back.
Wallets that need to disable transaction submission can wire `WalletQuery` with
`()` as the broadcaster, in which case `SendTransaction` returns
`Code::FailedPrecondition` with a `TransactionBroadcastDisabled` reason.

The adapter can be colocated with `zinder-query` in local development, but it remains a separate deployable boundary.

## QueryError to gRPC Status

The mapping is centralized in `services/zinder-query/src/grpc/mod.rs::status_from_query_error` and shared by both `WalletQueryGrpcAdapter` (native) and `LightwalletdGrpcAdapter` (compat), so wallet clients see one stable contract regardless of which surface served the request. The variant-by-variant table and structured-detail conventions are owned by [Public Interfaces §Error Conventions](public-interfaces.md#error-conventions).

The category contract: `Unavailable` is reserved for transient infrastructure failures the client may retry, `NotFound` for absent resources inside a successful read path, and `FailedPrecondition` for state mismatches the client must resolve (typically by reseeding a cursor or adjusting a range). Mixing categories is forbidden.

## Capability Descriptor

`WalletQuery.ServerInfo` returns `ServerCapabilities` per [Public interfaces §Capability Discovery](public-interfaces.md#capability-discovery). The descriptor is the canonical client-facing capability protocol; clients gate features on capability strings, not on Zinder version. Server reflection (via `tonic-reflection`) is enabled in development for tooling like `grpcurl`; it exposes the proto schema, not semantic guarantees, so it is not the discovery primitive.

Capability strings follow `domain.subdomain.capability_name_v{N}`. The naming spine in [Public Interfaces §Capability Discovery](public-interfaces.md#capability-discovery) lists the active capabilities.

## Deprecation Policy

When a `WalletQuery` capability is deprecated:

1. The capability appears in both `capabilities` and `deprecated_capabilities` in `ServerCapabilities`. The `DeprecatedCapability` entry names the replacement (if any), the deprecation rationale, and the earliest semver at which the capability may be removed (`earliest_removal_version`).
2. The capability remains functional during the deprecation window. The window is at least one minor version.
3. At removal, the capability is removed from both lists. The proto method may be marked `[deprecated = true]` to preserve the wire identifier; new methods may not reuse the wire number.

`buf breaking` runs as a CI gate on every PR touching the owned Zinder proto tree and rejects package-level non-additive changes. `buf lint` also checks the owned Zinder proto package layout. The vendored lightwalletd protos under `proto/compat/lightwalletd/` are excluded from Buf because they are governed by the upstream commit pin and the existing `vendored proto drift` job.

A second CI gate, `capability-coverage`, asserts that every `WalletQuery` RPC has a corresponding entry in `crates/zinder-proto/src/capabilities.rs::ZINDER_CAPABILITIES`. Adding an RPC without a capability string fails the job.

## OpenRPC

OpenRPC is not part of v1.

Zebra may expose OpenRPC for JSON-RPC capability discovery, and source adapters may consume that as upstream node input. That does not make OpenRPC a Zinder public API format.

If Zinder later ships a JSON-RPC server, OpenRPC must be generated from the primary proto and method docs. It must not become a parallel hand-maintained spec.

## Compatibility Tests

Protocol changes require tests at the right boundary:

- Native API golden tests use `zinder_proto::v1::wallet`.
- Internal API tests prove `ChainEpochReadApi` preserves epoch consistency.
- Lightwalletd compatibility tests decode stored compact-block payload bytes through the vendored `CompactBlock` schema.
- In-workspace lightwalletd compatibility tests should connect with the
  generated client from Zinder's vendored protos. Wallet-SDK compatibility tests
  may use `zcash_client_backend` only when that SDK graph passes the dependency
  policy gate; SDK scanning must run locally without sending wallet secrets to
  Zinder.
- Android SDK/Zashi compatibility tests must satisfy the canonical external
  wallet contract in [Wallet data plane](wallet-data-plane.md#external-wallet-compatibility-claims).
- Error mapping tests prove typed Zinder errors reach clients as stable wire responses.
- Cursor tests prove cross-network, expired, and tampered cursors fail closed.

## Review Checklist

A protocol change is not ready unless:

- The `.proto` file lives in `zinder-proto`.
- The generated Rust type does not leak into `zinder-core` or `zinder-store` public APIs.
- The method has a clear owner: native, internal, or compat.
- The change is additive, or it follows the deprecation policy (capability string moved to `deprecated_capabilities` with `replaced_by` and `earliest_removal_version`; `buf breaking` CI passes either because the change is additive or because the maintainers have approved the breaking semver bump).
- The new method has a corresponding capability string in `ZINDER_CAPABILITIES` (`capability-coverage` CI passes).
- Tests cover wire compatibility and domain error mapping.
- The docs name the service that serves the method.
- Compatibility methods are backed by native artifacts, document their
  capability or `taddrSupport` behavior, and do not proxy upstream node reads.
