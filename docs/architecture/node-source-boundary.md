# Node Source Boundary

`NodeSource` is Zinder's boundary around upstream node sources. It lets ingestion consume upstream node observations without learning Zebra's internal types, zcashd JSON-RPC DTOs, or streaming-source details.

This document owns the Rust API shape and naming rules for upstream node adapters. Protocol schemas live in [Protocol boundary](protocol-boundary.md). Chain-event semantics live in [Chain events](chain-events.md).

## Boundary Rule

Only `zinder-source` talks to upstream nodes.

Allowed inside `zinder-source`:

- Zebra JSON-RPC clients.
- `jsonrpsee` HTTP transport and JSON-RPC error mapping.
- Zebra indexer gRPC clients.
- zcashd JSON-RPC clients.
- Zebra parser and consensus primitive types, including `zebra-chain`.
- librustzcash primitives only when a specific wallet/protocol need is documented and contained inside the source boundary.
- Upstream-node-specific capability probes and authentication adapters.

Forbidden outside `zinder-source`:

- Zebra internal types in public signatures.
- zcashd JSON-RPC response structs.
- Upstream-node transport errors such as `jsonrpsee::core::ClientError`, `reqwest::Error`, or JSON-RPC crate errors.
- Hand-written parsers for consensus-critical block and transaction bytes.
- Upstream-node fallback logic hidden in query handlers.

`zinder-ingest` receives normalized source values and decides canonical state. `zinder-query` never follows upstream nodes.

Transaction broadcast is the explicit exception to the read-path rule: wallets may submit raw transactions through `zinder-query`, but the upstream-node-specific I/O still belongs to `zinder-source` behind `TransactionBroadcaster`. Query logic delegates to that boundary and does not learn Zebra or zcashd JSON-RPC details.

## Source Trait

The canonical trait name is `NodeSource`. It is the upstream dependency boundary: every adapter for Zebra, zcashd, or a future streaming source implements it, and ingest depends only on the trait, not on adapter internals.

The trait is async, sized for backfill and polling tip-following:

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

`tip_id()` returns `BlockId { height, hash }` so steady-state ingest can short-circuit on hash equality. The Zebra JSON-RPC adapter implements it as `getbestblockhash` followed by `getblockheader(best_hash, true)` so the height and hash come from the same observation.

A streaming follower with explicit source observations, resume cursors, and interruption reasons is an open extension: when a streaming backend lands, the trait gains a backpressure-aware streaming method and the corresponding event and cursor types appear in `zinder-source`. Until then, ingest drives the source through async polling calls.

Transaction broadcast uses a separate boundary because it is a command, not a chain observation stream:

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

This separation keeps ingestion source observation and wallet transaction submission from collapsing into a generic node service.

Processing code is generic over `S: NodeSource`. A dynamic wrapper exists only at the runtime composition boundary:

```text
config -> SourceFactory -> DynNodeSource -> ingest runner
```

The ingest state machine does not depend on dynamic dispatch just because runtime configuration needs it at the edge. Static dispatch keeps tests simple, avoids unnecessary allocation on hot paths, and gives Rust better type information.

`zinder-ingest` follows this boundary by making the library backfill and tip-follow runners take an injected `NodeSource`. The CLI binary owns the Zebra JSON-RPC factory because it is the runtime composition edge. Production-shaped tests use the same injection point instead of reaching into private `#[cfg(test)]` helpers.

## Capability Model

Adapters expose capabilities instead of requiring exact upstream-node versions. The internal `NodeCapabilities` is mirrored to the public `ServerCapabilities.node` field returned by `WalletQuery.ServerInfo`, so operators and clients see one coherent picture of "what does this Zinder deployment support."

Advertised capabilities:

- `best_chain_blocks`
- `tip_id`
- `tree_state`
- `subtree_roots`
- `finalized_height`
- `readiness_probe`
- `transaction_broadcast`
- `node.streaming_source` — Zebra `--features indexer` is detected; `NonFinalizedStateChange` and, in M3, `MempoolChange` gRPC streams can be consumed.
- `node.spending_tx_lookup` — Zebra's nullifier-to-spending-tx index is available behind `--features indexer`.
- `node.openrpc_discovery` — `rpc.discover` was called and the upstream-node capability surface was parsed.
- `node.json_rpc` — JSON-RPC source is the active backend.
- `mempool_stream` — internal M3 source capability for Zebra `MempoolChange`; not a public wallet capability and not advertised before M3 consumes it.

New capability names are added to `NodeCapability` when a real consumer reads the capability; aspirational vocabulary is not pre-declared.

Capability discovery happens at startup in the `connect_node` phase. The probe is implementation-specific per backend:

- **Zebra JSON-RPC**: call `rpc.discover` (Zebra v4.2+) and parse the OpenRPC method list. Required methods (`getbestblockhash`, `getblockheader`, `getblock`, `z_gettreestate`, `z_getsubtreesbyindex`, `getblockcount`, `sendrawtransaction`) must be present; missing required methods produce `NodeCapabilityMissing` and the readiness state advances no further than `node_capability_missing`. Optional methods (`getrawmempool`, etc.) are advertised but not required.
- **Zebra indexer gRPC**: detect feature presence by attempting a no-op subscription and observing whether the gRPC port is reachable. Signal `node.streaming_source` and `node.spending_tx_lookup` only when confirmed.
- **zcashd JSON-RPC**: future. Capability probe via `getnetworkinfo` and method probing.

Startup validates required capabilities before ingestion mutates state. Missing or contradictory capabilities produce typed errors:

- `NodeCapabilityMissing { capability }`
- `NodeUnavailable`
- `SourceProtocolMismatch`
- `BlockUnavailable`
- `TransactionBroadcastDisabled` for the no-op broadcaster path.

Streaming-source-cursor errors will appear here when the streaming follower lands.

Version strings may be logged and included in diagnostics, but they are not the primary compatibility contract.

## Mempool Source Adapter

`zinder-source` produces `MempoolSourceEvent` values consumed by `zinder-ingest` in M3 (see [M3 Mempool](../specs/m3-mempool.md)). Two backends are planned, selected by capability discovery:

- **Streaming backend** (preferred): consumes Zebra's `MempoolChange` gRPC stream. Requires `node.streaming_source`. Maps `ADDED` → `Added`, `INVALIDATED` → `Invalidated`, `MINED` → `Mined`. Sub-second latency.
- **Polling backend** (fallback): calls `getrawmempool` on `[mempool] poll_interval_ms` (default 10000) and diffs successive responses to synthesize `Added` and `Invalidated` events. `Mined` events are inferred from chain commits, not from `getrawmempool`. Default-second latency.

The backend choice is invisible to clients except through the `mempool_snapshot_age_ms` metric after M3 lands. Operators choose the backend by configuring whether Zebra runs with `--features indexer`; Zinder does not require the streaming backend. `wallet.snapshot.mempool_v1` and `wallet.events.mempool_v1` are advertised only when the public M3 methods, storage, and retention path exist.

Reorg interaction: Zinder's mempool reflects Zebra's `MempoolChange` directly. When a `ChainReorged` event fires in `zinder-ingest`, mempool state is **not** synthesized from the reverted block; Zinder waits for Zebra to emit corresponding `MempoolSourceEvent` values. This avoids the Zaino phantom-mempool-entry bug class.

## Adapter Selection

Adapter modules:

| Module | Purpose |
| ------ | ------- |
| `zebra_json_rpc` | JSON-RPC source for Zebra methods and fallback paths |
| `zebra_indexer_grpc` | Zebra indexer gRPC stream source when available |
| `zebra_read_state` | Zebra in-process state source when a colocated deployment explicitly chooses that coupling |
| `zcashd_json_rpc` | zcashd compatibility and comparator source |
| `node_capabilities` | Shared capability vocabulary and probing helpers |
| `node_auth` | Typed authentication configuration |

Modules stay flat until the crate has enough cohesive adapter files to justify a subdirectory. A premature `sources/` tree adds navigation cost without creating a clearer boundary.

## Auth and Config

Authentication is represented by valid states:

```rust
pub enum NodeAuth {
    None,
    Cookie { path: PathBuf },
    Basic { username: String, password: SecretString },
}
```

Bool-plus-option combinations are rejected. Configuration is validated before network connections begin. TLS and cookie file readability errors become typed startup or readiness causes, not late connection failures with transport-specific messages.

Environment variables use `ZINDER_` with nested `__` sections:

```text
ZINDER_NODE__SOURCE=zebra-json-rpc
ZINDER_NODE__JSON_RPC_ADDR=127.0.0.1:8232
ZINDER_NODE__AUTH__COOKIE__PATH=/var/lib/zebra/.cookie
```

## Consensus Parsing

Source adapters parse upstream-node responses using Zebra-compatible primitives. The parser boundary is `zebra-chain`. `zinder-source` uses Zebra to derive source block metadata from raw block bytes, and `zinder-ingest` uses Zebra to derive compact-block artifacts. Ingest artifact builders do not parse raw block headers, transaction bytes, or compact-block wire messages by hand.

The current dependency compromise is resolver-only: `zebra-chain 6.0.2` reaches `equihash 0.2.2`, which still requires yanked `core2 0.3.x` crates.io releases. The root manifest pins upstream `core2` commit `7bf2611` through `[patch.crates-io]`, and `deny.toml` allows only that git source. Remove this patch when Zebra or Equihash publishes a crates.io-only resolution path.

The allowed flow is:

```text
upstream-node response
  -> source parser owned by Zebra-compatible primitives
  -> SourceBlock and source metadata
  -> zinder-ingest artifact builders
  -> BlockArtifact, CompactBlockArtifact, TreeStateArtifact
```

This keeps consensus interpretation in the crates that own consensus semantics and keeps Zinder artifacts focused on indexing.

JSON-RPC adapters bound response body size and make that cap configurable. The default is conservative for current block and tree-state sizes, but future network upgrades or upstream-node payload changes do not require a code patch just to raise the local ingest limit.

## Readiness

`zinder-ingest` readiness is capped by upstream-node readiness. If the selected upstream node reports not ready or cannot prove required capabilities, Zinder reports `node_unavailable` or a more specific typed cause even if local storage is healthy.

Readiness carries operator-useful detail:

```json
{
  "status": "not_ready",
  "cause": "node_unavailable",
  "nodeSource": "zebra-json-rpc",
  "requiredCapability": "finalized_height"
}
```

## Review Checklist

A change touching upstream-node access is not ready unless:

- The public trait is `NodeSource`.
- New upstream-node-specific types stop inside `zinder-source`.
- The adapter returns typed `SourceError` values.
- Capability probing covers the feature before it is used.
- Tests include a deterministic fake source from `zinder-testkit`.
- No query path calls an upstream node directly.
- No ingest artifact builder hand-parses consensus-critical bytes.
