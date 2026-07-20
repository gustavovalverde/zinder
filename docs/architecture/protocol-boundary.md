# Protocol Boundary

`zinder-proto` owns Zinder's service protobufs, vendored compatibility
protobufs, generated Rust modules, and wire-level byte conventions. Domain and
storage crates use their own types and convert at service adapters.

Upstream node protocols belong to [Node source boundary](node-source-boundary.md).
Storage encodings belong to [Storage backend](storage-backend.md).

## Protocol surfaces

| Surface | Rust module | Service owner | Release status |
| --- | --- | --- | --- |
| Native wallet API | `zinder_proto::v1::wallet` | `WalletQueryGrpcAdapter` in `zinder-query` | Library contract; no standalone query runtime |
| Native explorer API | `zinder_proto::v1::explorer` | `zinder-explorer` | Optional workspace runtime; no published release image |
| Private control APIs | `zinder_proto::v1::ingest` | `zinder-ingest` and `zinder-projector` | Internal deployed services |
| Shared operations messages | `zinder_proto::v1::ops` | Runtime and service adapters | Embedded in native/control surfaces |
| Lightwalletd compatibility API | `zinder_proto::compat::lightwalletd` | `zinder-compat-lightwalletd` | Published compatibility service |
| Zebra indexer client contract | `zinder_proto::external::zebra` | `zinder-source` | Client-only upstream protocol |

`ChainEpochReadApi` is a Rust storage trait, not a gRPC service. It provides the
epoch-bound read boundary used by `zinder-query` and store implementations.

## Ownership rules

`zinder-proto` owns:

- native `.proto` files and generated `prost`/`tonic` modules;
- vendored lightwalletd protobuf files and their provenance record;
- the external Zebra indexer client schema used by `zinder-source`;
- protocol golden fixtures and wire-compatibility tests; and
- shared enums and messages whose semantics cross a service boundary.

Service crates do not hand-write `prost::Message` types or generate private
copies of shared protocols. Domain crates do not expose generated protocol types
in public APIs. Storage-private control records may live in `zinder-store` when
they never cross a service boundary.

Native protocol names use Zinder's domain vocabulary. Compatibility protocol
names remain identical to the external contract even when their wording differs
from native APIs.

## Native wallet API

`WalletQuery` is the native wallet and application read contract. The
`zinder-query` crate implements it over `WalletQueryApi` and exact-fence
canonical/wallet-projection readers. The release topology embeds that library in
the lightwalletd adapter rather than running a standalone query listener.

Wallet responses carry the chain identity needed to prevent mixed-epoch reads.
Streaming and pagination requests use bounded ranges and authenticated cursors.
Capabilities are advertised by exact strings through `ServerInfo`; clients gate
features on those strings rather than parsing a Zinder version.

## Native explorer API

`ExplorerQuery` serves explorer, dashboard, and analytics shapes. It composes
materialized views, a canonical secondary, and selected `WalletQuery` calls.
Every response carries `ExplorerFreshness`, and optional fields use typed
unavailability reasons. See [Explorer plane](explorer-plane.md).

## Private control APIs

The `zinder.v1.ingest` package contains:

- `CanonicalControl` for canonical writer position and authenticated checkpoint
  evidence;
- `ProjectorControl` for wallet-projection coordination; and
- `IngestControl` for writer status, retained chain events, mempool events, and
  source-backed control operations.

These endpoints are private operational contracts. They use the shared
transport policy and fail closed when authentication or transport requirements
are not met. See [ADR-0006](../adrs/0006-ingest-control-transport-security.md).

## Lightwalletd compatibility

The vendored `CompactTxStreamer` schema is an external contract. Its source pin
and provenance live in
`crates/zinder-proto/proto/compat/lightwalletd/UPSTREAM.md`. The compatibility
adapter translates native query results, byte order, errors, and capability
claims into that contract. It does not mutate canonical storage or fetch missing
artifacts from an upstream node.

Compatibility claims are bounded by executable certification. The stable method
matrix lives in [Lightwalletd compatibility](../reference/lightwalletd-compatibility.md).

## Byte conventions

Wire fields follow explicit conventions:

- hash strings use lowercase RPC display order;
- raw hash bytes use the order documented by the owning protobuf field;
- opaque cursors are not parsed or synthesized by clients;
- integer fields include units in their names when the type alone is
  insufficient; and
- unknown enum values fail or degrade according to the method contract rather
  than silently mapping to a valid domain value.

Conversion helpers live at protocol adapters. Storage encodings and protobuf
encodings must not share serializers merely because both contain the same
domain value.

## Evolution and verification

- Additive fields receive new tags. This pre-compat native protocol may reuse a
  deleted native tag as part of an explicitly breaking contract change.
- Vendored compatibility schemas retain upstream field and reservation rules.
- Breaking native shapes use a new capability version or package version.
- Compatibility schemas change only with an explicit upstream-pin update.
- Proto tests cover golden decoding, byte order, enum mappings, pagination, and
  epoch identity.
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`
  keeps generated and adapter documentation references valid.
