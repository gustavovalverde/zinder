# ADR-0004: Separate Node Sources from Protocol Surfaces

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Upstream node adapters, Rust API shape, and wire protocols |
| Related | [RFC-0001](../rfcs/0001-service-oriented-indexer-architecture.md), [Node source boundary](../architecture/node-source-boundary.md), [Protocol boundary](../architecture/protocol-boundary.md), [Public interfaces](../architecture/public-interfaces.md), [Service operations](../architecture/service-operations.md), [Lessons from Zaino](../reference/lessons-from-zaino.md) |

## Context

Two adjacent decisions tend to collapse into one if the boundary is not explicit before code lands: the Rust trait that adapts upstream-node data, and the protocol crate that owns wire schemas. The dangerous default is letting the first working JSON-RPC client in ingest become the architecture, embedding a Zebra internal type in public configuration, or letting query handlers reach back to the upstream node when an artifact is missing.

The Zaino research surfaces the failure mode repeatedly: wire, internal, and persistence types shape each other; upstream-node types leak into public configuration and service traits; consensus-critical parsing gets reimplemented outside the upstream node and Zcash primitive crates; the wallet-compatible protocol and the project's native RPC surface have no single owner; runtime lifecycle is scattered flags rather than a typed contract.

The Rust-specific choices follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for predictable naming, newtypes, hidden representation, and future-proof public APIs; the [async trait guidance](https://blog.rust-lang.org/inside-rust/2023/05/03/stabilizing-async-fn-in-trait/) for keeping async traits static by default and introducing erased wrappers only at composition; and [Tokio's graceful-shutdown guidance](https://tokio.rs/tokio/topics/shutdown) for propagating cancellation tokens through long-running tasks.

## Decision

Zinder uses two separate boundaries:

1. `NodeSource` is the Rust source-adapter boundary for upstream node data.
2. `zinder-proto` is the protocol ownership boundary for native, internal, and lightwalletd-compatible wire schemas.

`NodeSource` exposes the upstream-node dependency as normalized async pull methods. It returns Zinder source-domain values and `SourceError` failures, not transport DTOs. There is no separate `ChainSource` trait.

`zinder-source` is the only crate allowed to depend on upstream-node client crates, transport DTOs, JSON-RPC client libraries, Zebra parser types, or documented Zcash wallet/protocol primitive crates. It normalizes those inputs into Zinder domain values before they cross into `zinder-ingest`.

Ingest does not parse consensus-critical block or transaction bytes by hand. Source adapters delegate consensus parsing to Zebra-compatible primitives and return normalized `SourceBlock`, `SourceTransaction`, and source metadata values. Artifact builders derive Zinder artifacts from those values.

Source compatibility is capability-driven. Source adapters expose `NodeCapabilities` discovered at connection or startup. Zinder does not pin upstream-node versions as the runtime contract. Missing capabilities produce typed startup, readiness, or source errors such as `NodeCapabilityMissing`, `NodeUnavailable`, or `SourceProtocolMismatch`.

`zinder-proto` owns all service `.proto` files and generated code:

- `zinder_proto::v1::wallet` is the native wallet and application query API served by `zinder-query`; its service name is `WalletQuery`.
- `zinder_proto::v1::ingest` is the private writer-control surface (`IngestControl`) used by colocated readers.
- `zinder_proto::v1::epoch` is the internal `ChainEpochReadApi` surface.
- `zinder_proto::compat::lightwalletd` is the vendored `CompactTxStreamer`, `compact_formats.proto`, and `service.proto` compatibility surface.

`zinder-compat-lightwalletd` is a separate adapter service over `WalletQueryApi`. It does not read live canonical storage, call upstream nodes, run migrations, or build missing artifacts. It translates native query results and errors into lightwalletd-compatible gRPC behavior.

OpenRPC is not part of v1. If Zinder later exposes a JSON-RPC server, OpenRPC is generated from the primary proto and Markdown method docs. It does not become a parallel hand-maintained spec.

## Rust API Shape

The `NodeSource` trait is async, sized for backfill and tip-following:

```rust
#[async_trait::async_trait]
pub trait NodeSource: Send + Sync + 'static {
    fn capabilities(&self) -> NodeCapabilities;

    async fn fetch_block_by_height(&self, height: BlockHeight) -> Result<SourceBlock, SourceError>;

    async fn tip_id(&self) -> Result<BlockId, SourceError>;

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError>;
}
```

`tip_id()` returns `BlockId { height, hash }` so steady-state ingest can short-circuit on hash equality without fetching the full block. The Zebra JSON-RPC adapter implements `tip_id()` as `getbestblockhash` followed by `getblockheader(best_hash, true)` so the height and hash come from the same observation. Idle steady-state cost is two cheap RPCs.

Transaction broadcast is a separate boundary because it is a command, not an observation:

```rust
#[async_trait::async_trait]
pub trait TransactionBroadcaster: Send + Sync + 'static {
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, SourceError>;
}
```

The unit `()` impl returns `SourceError::TransactionBroadcastDisabled` so read-only deployments surface a distinct error in the query layer instead of "node capability missing."

Processing code is generic over `S: NodeSource`. A dynamic wrapper exists only at the runtime composition boundary:

```text
config -> SourceFactory -> DynNodeSource -> ingest runner
```

Static dispatch keeps tests simple, avoids unnecessary allocation on hot paths, and gives Rust better type information for ingest state-machine code.

Per-boundary typed errors: libraries use `thiserror`; binaries and top-level command handlers may use `anyhow` for final context. Domain crates do not expose `tonic::Status`, `jsonrpsee::core::ClientError`, `reqwest::Error`, RocksDB handles, generated proto types, or Zebra internal types in public signatures.

Every long-running task accepts a `tokio_util::sync::CancellationToken` or a child token. Shutdown is signaled, awaited, and logged as typed lifecycle state. Polling booleans plus `sleep` are forbidden in shutdown paths.

## Configuration Shape

Configuration encodes valid combinations as types:

```rust
pub enum NodeAuth {
    None,
    Cookie { path: PathBuf },
    Basic { username: String, password: SecretString },
}
```

Bool-plus-option shapes such as `node_cookie_auth: bool` with `node_cookie_path: Option<PathBuf>` are rejected.

Configuration precedence:

```text
defaults -> config file -> ZINDER_* environment variables -> CLI flags
```

The production loader uses `config-rs`. TOML is the canonical file-source format, pinned explicitly. Environment variables use `ZINDER_` with `__` for nesting, for example `ZINDER_NODE__JSON_RPC_ADDR` and `ZINDER_NODE__AUTH__METHOD`.

Each service exposes `--config` and `--print-config`, then validates configuration before opening storage, connecting to upstream nodes, or binding public listeners.

## Module Naming

Adapter modules:

- `node_source`
- `zebra_json_rpc`
- `zebra_indexer_grpc`
- `zebra_read_state`
- `zcashd_json_rpc`
- `node_capabilities`
- `node_auth`

Forbidden:

- `chain_source` as a trait name.
- `zebra_rpc` once a public module name is established; it hides the transport.
- `source_processor`, `source_service`, `node_manager`, `rpc_helper`.
- `lightwallet_service` for a component that only adapts protocol traffic.

## Capability Model

Adapters expose capabilities instead of requiring exact upstream-node versions. The internal `NodeCapabilities` set is mirrored to the public `ServerCapabilities.node` field returned by `WalletQuery.ServerInfo`, so operators and clients see one coherent picture of what the deployment supports.

Advertised capabilities:

- `best_chain_blocks`
- `tip_id`
- `tree_state`
- `subtree_roots`
- `finalized_height`
- `readiness_probe`
- `transaction_broadcast`
- `node.streaming_source` — Zebra `--features indexer` is detected; `NonFinalizedStateChange` and (in M3) `MempoolChange` gRPC streams can be consumed.
- `node.spending_tx_lookup` — Zebra's nullifier-to-spending-tx index is available behind `--features indexer`.
- `node.openrpc_discovery` — `rpc.discover` was called and the upstream node's capability surface was parsed.
- `node.json_rpc` — JSON-RPC source is the active backend.
- `mempool_stream` — internal M3 source capability for Zebra `MempoolChange`; not a public wallet capability and not advertised before M3 consumes it.

New capability names appear in `NodeCapability` only when a real consumer reads them; aspirational vocabulary stays out.

Capability discovery happens in the `connect_node` startup phase. The Zebra JSON-RPC adapter calls `rpc.discover` (Zebra v4.2+) and parses the OpenRPC method list. Required methods for the M1 surface (`getbestblockhash`, `getblockheader`, `getblock`, `z_gettreestate`, `z_getsubtreesbyindex`, `getblockcount`, `sendrawtransaction`) must be present; missing required methods produce `NodeCapabilityMissing` and readiness advances no further than `node_capability_missing`.

Capability discovery for Zebra indexer gRPC and zcashd JSON-RPC follows the same pattern: probe by attempting a no-op subscription or by method-probing `getnetworkinfo`. Version strings may be logged and included in diagnostics, but they are not the primary compatibility contract.

## Consequences

Positive:

- Zebra, zcashd, and future source backends evolve without changing wallet APIs.
- Native and compatibility protocols are tested independently while sharing one query contract.
- Consensus parsing stays in the crates that own consensus semantics.
- Runtime source selection does not infect deterministic ingest processing with dynamic dispatch.
- Downstream developers see stable names that explain ownership.

Tradeoffs:

- `zinder-proto` exists from day one even though the workspace has few services.
- Capability probing and typed config require more upfront code than version checks and free-form fields.
- The lightwalletd adapter is another deployable boundary, even when it is colocated in development.
