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

The trait is async, sized for the unified ingest loop's pipelined bulk-catchup and serial tip-follow phases:

```rust
#[async_trait::async_trait]
pub trait NodeSource: Send + Sync + 'static {
    fn capabilities(&self) -> NodeCapabilities;

    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError>;

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError>;

    async fn fetch_tree_state_for_block(
        &self,
        block_id: BlockId,
    ) -> Result<SourceTreeState, SourceError>;

    async fn tip_id(&self) -> Result<BlockId, SourceError>;

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError>;
}
```

The bulk-catchup phase of the unified ingest loop ([ADR-0022](../adrs/0022-resource-budgeted-bulk-catchup.md)) drives `fetch_chain_segment` with `SourceChainSegmentLimits`. The limits express both the requested block ceiling and the response-byte target/hard cap. JSON-RPC segments fetch raw block bytes only; checkpoint tree state is fetched separately through `fetch_tree_state_for_block` for the committed batch tip.

Tip-follow and reorg-ancestor traversal use `fetch_block_at` directly because random access at the live edge is the natural shape. Native streaming transports can satisfy the same `SourceChainUpdate` values behind `fetch_chain_segment` without changing canonical ingest.

`tip_id()` returns `BlockId { height, hash }` so steady-state ingest can short-circuit on hash equality. The Zebra JSON-RPC adapter implements it as `getbestblockhash` followed by `getblockheader(best_hash, true)` so the height and hash come from the same observation.

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

`zinder-ingest` follows this boundary by making the unified ingest loop take an injected `NodeSource`. The CLI binary owns the Zebra JSON-RPC factory because it is the runtime composition edge. Production-shaped tests use the same injection point instead of reaching into private `#[cfg(test)]` helpers.

## Capability Model

Adapters expose capabilities instead of requiring exact upstream-node versions. `NodeCapabilities` is the source-boundary diagnostic contract used by ingest, readiness, and source tests. It is not automatically mirrored into `WalletQuery.ServerInfo` in storage-only query deployments because `zinder-query` does not call upstream nodes. The `ServerCapabilities.node` field is reserved for a source capability snapshot once the runtime has an explicit handoff from the source-owning process.

Current `NodeCapability` names:

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

New capability names are added to `NodeCapability` when a real consumer reads the capability; aspirational vocabulary is not pre-declared.

Capability discovery happens at startup in the `connect_node` phase. The probe is implementation-specific per backend:

- **Zebra JSON-RPC**: call `rpc.discover` (Zebra v4.2+) and parse the OpenRPC method list. Canonical ingest requires `getblock`, `getbestblockhash`, `getblockheader`, and `z_getsubtreesbyindex`; missing required methods produce `NodeCapabilityMissing` and the readiness state advances no further than `node_capability_missing`. `z_gettreestate` enables checkpoint tree-state wallet capabilities but is not required for canonical catchup. The block-fetch and checkpoint paths use height-keyed `getblock`; `getblockhash` is not part of the source contract. Optional methods such as `sendrawtransaction` for broadcast and `getblockchaininfo` for `chain_value_pools` are advertised when present but are not required for canonical ingestion.
- **Zebra indexer gRPC**: the mempool adapter detects feature presence by opening the configured gRPC stream. A block-streaming source and spending-transaction lookup capability are future extensions; they must add real `NodeCapability` variants and runtime wiring in the same change.
- **zcashd JSON-RPC**: future. Capability probe via `getnetworkinfo` and method probing.

Startup validates required capabilities before ingestion mutates state. Missing or contradictory capabilities produce typed errors:

- `NodeCapabilityMissing { capability }`
- `NodeUnavailable`
- `SourceProtocolMismatch`
- `BlockUnavailable`
- `TipViewChanged` when Zebra invalidates the just-observed best hash before its header can be read; the adapter performs bounded fresh observations before returning it.
- `TransactionBroadcastDisabled` for the no-op broadcaster path.

Source errors describe what the upstream did; lifecycle decisions are owned by the writer loops, not by this boundary. `SourceError::upstream_classification()` returns a [`SourceFailureClass`](../adrs/0013-source-failure-recovery-topology.md) (`NodeUnreachable`, `UpstreamViewChanged`, `StreamDisconnected`, `CapabilityMissing`, `ProtocolMismatch`, `Malformed`, `Configuration`) that the recovery primitive in `zinder-ingest` consumes to select backoff and populate the `node_unavailable` readiness payload. Every source-shaped failure is loop-recoverable; storage and reorg-window failures are the only ingest-exit paths.

Operators triaging a `/readyz` response with `cause.node_unavailable.failure_class = <label>` map the label to an action:

| `failure_class` | Meaning | Operator action |
| --- | --- | --- |
| `node_unreachable` | Zebra is down or unreachable | Investigate Zebra liveness, connection limits, transport |
| `upstream_view_changed` | Best-chain race; a height or just-observed tip moved out of the best chain | None (normal during reorgs and node restarts) |
| `stream_disconnected` | Chain-tip or mempool subscription dropped | Self-heals; check indexer endpoint if persistent |
| `capability_missing` | Zebra is missing a required RPC method | Upgrade Zebra or switch source |
| `protocol_mismatch` | Zebra response shape does not match expectations | Investigate Zebra version mismatch |
| `malformed` | Zebra returned bytes that did not parse | Investigate Zebra version mismatch or data corruption |
| `configuration` | Adapter configuration is invalid | Fix configuration (auth scheme, broadcast mode, etc.) |

Operators also see this label through the `zinder_readiness_node_failure_class{class=...}` Prometheus gauge, so alert rules can route differently per class without parsing log payloads.

Streaming-source-cursor errors will appear here when the streaming follower lands.

Version strings may be logged and included in diagnostics, but they are not the primary compatibility contract.

## Mempool Source Adapter

`zinder-source` produces `MempoolSourceEvent` values consumed by `zinder-ingest` (see [ADR-0007](../adrs/0007-mempool-topology-and-retention.md)). Two backends are supported, selected by capability discovery:

- **Streaming backend** (preferred): consumes Zebra's `MempoolChange` gRPC stream when an indexer gRPC endpoint is configured. It reports `MempoolSourceCapabilities::streaming()` internally. Maps `ADDED` → `Added`, `INVALIDATED` → `Invalidated`, `MINED` → `Mined`. Sub-second latency.
- **Polling backend** (fallback): calls `getrawmempool` on `[mempool] poll_interval_ms` (default 10000) and diffs successive responses to synthesize `Added` and `Invalidated` events. `Mined` events are inferred from chain commits, not from `getrawmempool`. Default-second latency.

The backend choice is invisible to clients except through the `mempool_snapshot_age_ms` metric. Operators choose the backend by configuring whether Zebra runs with `--features indexer`; Zinder does not require the streaming backend. `wallet.snapshot.mempool_v1` and `wallet.events.mempool_v1` are advertised when the public mempool methods, storage, and retention path are reachable.

Reorg interaction: Zinder's mempool reflects Zebra's `MempoolChange` directly.
When a `ChainReorged` event fires in `zinder-ingest`, mempool state is **not**
synthesized from the reverted block; Zinder waits for Zebra to emit
corresponding `MempoolSourceEvent` values. This keeps mempool truth tied to the
upstream node instead of reconstructing it from reverted chain artifacts.

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

The current dependency boundary is resolver-only: Zinder uses the stable
Zcash-family releases required for Ironwood, all from crates.io. No git
patches or source exceptions are needed.

The allowed flow is:

```text
upstream-node response
  -> source parser owned by Zebra-compatible primitives
  -> SourceBlock and source metadata
  -> zinder-ingest artifact builders
  -> BlockHeaderArtifact, BlockTransactionIndexArtifact, TransactionFactsArtifact, CompactBlockArtifact, TreeStateArtifact
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
  "requiredCapability": "safe_tip_height"
}
```

## Upstream Platform Bindings

`NodeSource` is the Rust shape. In production every `NodeSource`
implementation lives inside an *upstream platform binding*: a bundle
of node binary, authentication mechanism, networking contract, and
operational packaging that wraps the node. The binding is the unit
operators install, not the trait.

The binding model is what survives when Z3 evolves, when alternative
platforms appear, and when in-process integration lands
(see [ADR-0016 §Phase 3](../adrs/0016-source-streaming-pipeline.md#phase-3-in-process-backend)).

### The binding contract

Every upstream-platform binding fulfils three required sub-contracts
and one optional one.

**Chain-source contract (required).** The binding must surface a
`NodeSource` implementation reachable from Zinder. The binding
declares:

- Node endpoint(s) per protocol (JSON-RPC, indexer gRPC, future
  streaming RPCs from [ADR-0016 §Phase 2](../adrs/0016-source-streaming-pipeline.md#phase-2-native-streaming-backend)).
- Authentication mechanism: cookie file path, basic-auth credentials,
  or no auth for regtest/dev.
- Per-network identity (`zcash-mainnet`, `zcash-testnet`,
  `zcash-regtest`).

Delivery mechanism is binding-specific: shared volumes, environment
variables, container DNS, host networking, in-process function calls.

**Identity contract (required).** The binding must establish
operator-controllable identity for the node connection. Two
production patterns:

- *Cookie file in a shared volume.* The binding writes `.cookie` into
  a volume Zinder mounts read-only. Rotation is implicit (cookie
  regenerated on node restart). This is Z3's pattern.
- *Operator-managed credentials.* The operator provisions a
  username/password (or token) and feeds it to Zinder via env vars or
  a secret file. Suitable for PaaS shapes where no shared volume
  exists.

The contract is the binding's promise that the credential at the
named path works against the named endpoint across the node's
lifecycle.

**Discovery contract (required).** The binding declares the names
operators and Zinder must agree on:

- Network name (Docker network, Kubernetes namespace, host loopback):
  where Zinder's containers must live to reach the node.
- Container/service DNS: how to dial each platform service.
- Per-network port matrix: testnet vs mainnet differences.

The discovery contract is a published interface Zinder reads, not a
configuration mechanism Zinder owns. Z3 publishes its contract at
[`docs/contract.md`](https://github.com/ZcashFoundation/z3/blob/main/docs/contract.md);
Zinder's compose substitutes against it.

**Observability contract (optional).** The binding may provide
shared Prometheus, Grafana, Jaeger, and Alertmanager pods for the
node services it owns. Zinder does not require this contract to
function; federation patterns are operator-driven and live in
[Service operations §Observability Federation](service-operations.md#observability-federation).

### Bindings shipped today

| Binding | Status | Source | Deployment shape | Observability |
|---|---|---|---|---|
| `z3` | Production | [ZcashFoundation/z3](https://github.com/ZcashFoundation/z3) Zebra + Zaino + Zallet | Sibling Docker networks; shared cookie volume | Optional per-network profile |
| `bare-zebra` | Supported | Operator-managed Zebra | Operator-defined; host networking common | Operator-managed |
| `in-process` | [Future](../adrs/0016-source-streaming-pipeline.md#phase-3-in-process-backend) | Zebra-as-a-library | Single binary | N/A (single process owns both metric paths) |

**Z3 binding.** Zinder's `deploy/docker-compose.yml` and
`deploy/.env.{testnet,mainnet}` are calibrated for this binding:

- The compose file declares `z3-${Z3_NETWORK_LOWER}` and
  `z3-${Z3_NETWORK_LOWER}-cookie` as external Docker resources. Z3
  owns their lifecycle; Zinder attaches.
- Zinder dials the upstream node by container DNS
  (`http://zebra:18232` testnet JSON-RPC, `http://zebra:8232` mainnet
  JSON-RPC, etc.).
- Cookie auth is the default and is read from
  `/var/run/auth/.cookie` inside the Zinder containers, which mount
  the Z3-published `z3-${network}-cookie` volume read-only.

**Bare-Zebra binding.** For operators who run Zebra themselves
(systemd unit, hand-managed container, build from source). Same
contract, no platform packaging. The operator publishes the node
endpoint, points Zinder at a cookie file or basic-auth creds, chooses
the network topology, and runs whatever observability they already
have. The [VM runbook](../runbooks/deploying-on-a-vm.md) and
[Railway runbook](../runbooks/deploying-on-railway.md) cover this
binding.

**In-process binding (future).** Zinder runs as a library inside the
Zebra binary, calling `zebra_state::ReadStateService` directly. No
network, no authentication, no discovery. The binding is "you compile
them together."

### Binding-naming conventions

| Name | Role | Rationale |
|---|---|---|
| `upstream platform binding` | Concept noun | "Upstream" is Zinder's perspective (the node feeds Zinder); "platform" names what wraps the node; "binding" names the operator-side attachment. The phrase reads end-to-end and avoids overloaded words like "stack" or "deployment". |
| `chain-source contract` | Sub-contract | Names what the platform provides (a node) and what Zinder uses (a `NodeSource`). |
| `identity contract` | Sub-contract | "Identity" is broader than "auth": it covers cookie, basic creds, future token shapes, and the no-auth dev case. |
| `discovery contract` | Sub-contract | The platform publishes the names; operators do not invent them. |
| `observability contract` | Optional sub-contract | Explicit about its optional status. |

## Review Checklist

A change touching upstream-node access is not ready unless:

- The public trait is `NodeSource`.
- New upstream-node-specific types stop inside `zinder-source`.
- The adapter returns typed `SourceError` values.
- Capability probing covers the feature before it is used.
- Tests include a deterministic fake source from `zinder-testkit`.
- No query path calls an upstream node directly.
- No ingest artifact builder hand-parses consensus-critical bytes.
- A new platform binding declares all three required sub-contracts
  (chain-source, identity, discovery) and is listed in
  §Bindings shipped today.
