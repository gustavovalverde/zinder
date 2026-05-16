# ADR-0009: Explorer Plane As First-Class Product Surface

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Service topology, explorer wire surface, capability namespace, federation contract |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0005](0005-consumer-neutral-wallet-data-plane.md), [ADR-0006](0006-ingest-control-transport-security.md), [ADR-0007](0007-mempool-topology-and-retention.md), [Explorer plane](../architecture/explorer-plane.md), [Derive plane](../architecture/derive-plane.md), [Service boundaries](../architecture/service-boundaries.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

The original derive plane shipped as `zinder-derive`: a fourth deployable that opened its own RocksDB, served `ExplorerQuery` over tonic, and federated one capability (`derive.explorer.transparent_balance_v1`) back into `WalletQuery`. The shape works, but it leaks an implementation detail (the word "derive") into every operator-facing surface: process name, config namespace, capability strings, Prometheus prefix.

Explorer support is a product surface, not an implementation pattern. The product question is "what does the block explorer call?" The answer should be `zinder-explorer`, not "the derive consumer named explorer running inside `zinder-derive`." Operators reading `ps aux`, contributors reading `services/`, and integrators reading the capability list should all see the same word.

At the same time, the SDK that powers the explorer is reusable. `DeriveConsumer`, `DeriveStore`, `DeriveProxy`, `run_chain_events_subscriber`, and `run_mempool_events_subscriber` describe a pattern (chain-events flowing into a stateful consumer with atomic cursor persistence), not the explorer product specifically. A second consumer some day (analytics, search index sink, anything else) would link the same SDK and ship its own service binary.

The decisions to record here are:

1. The service binary, config namespace, capability namespace, and Prometheus prefix all rebrand to `explorer`.
2. The SDK names (`DeriveConsumer`, `DeriveStore`, etc.) stay because they describe the pattern, not the product.
3. The dual-capability federation rule from ADR-0007's mempool work generalizes: a federated explorer method on `WalletQuery` advertises one always-on `wallet.*` capability plus one optional richer `explorer.*` capability.
4. The explorer plane must not call upstream node RPCs directly. Source-boundary extensions land first; the explorer consumes canonical artifacts and chain/mempool events.

## Decision

### The service rebrands to `zinder-explorer`

Production processes, config keys, capability strings, and Prometheus metrics rename in one wave:

| Surface | Before | After |
| ------- | ------ | ----- |
| Service crate | `services/zinder-derive/` | `services/zinder-explorer/` |
| Cargo package | `zinder-derive` | `zinder-explorer` |
| Binary | `zinder-derive` | `zinder-explorer` |
| Config TOML namespace | `[derive.explorer]` | `[explorer]` |
| Environment-variable prefix | `ZINDER_DERIVE__*` | `ZINDER_EXPLORER__*` |
| Capability prefix | `derive.explorer.*` | `explorer.*` |
| Prometheus metric prefix | `zinder_derive_*` | `zinder_explorer_*` |
| CLI flag namespace | `--derive-explorer-*` | `--explorer-*` |

Two existing capability strings rename in lockstep:

| Before | After |
| ------ | ----- |
| `derive.explorer.server_info_v1` | `explorer.server_info_v1` |
| `derive.explorer.transparent_balance_v1` | `explorer.transparent_address.balance_v1` |

The federated capability rename also reshapes the noun (`transparent_balance` → `transparent_address.balance`) to align with the wallet-plane convention `wallet.address.transparent_balance_v1`. After the rename, the dual-capability pair is symmetric:

- `wallet.address.transparent_balance_v1` — always-on canonical-confirmed path.
- `explorer.transparent_address.balance_v1` — same RPC carries the live-mempool overlay when the explorer proxy is ready.

There is no `_v2` deprecation window for the two renamed strings: Zinder has no public compatibility burden, and the rename ships before any external consumer has wired against the old names.

### SDK names stay derive-shaped

The reusable SDK abstractions keep their `Derive*` names because they describe the integration pattern, not the explorer product:

- `DeriveConsumer` (trait), `DeriveMempoolConsumer` (trait)
- `DeriveStore` (RocksDB wrapper), `DeriveStoreTable`, `DeriveConsumerName`
- `DeriveConsumerCtx` (per-event context with `&DeriveStore` + `&mut WriteBatch`)
- `DeriveProxy<Client>` (federation primitive in `services/zinder-query/src/derive_proxy.rs`)
- `DeriveReadinessGauge`, `spawn_derive_readiness_probe`
- `run_chain_events_subscriber`, `run_mempool_events_subscriber`

The boundary is "Zinder process or library entity belonging to the running explorer service" → renames; "reusable SDK abstraction representing the derive pattern" → keeps its name. A future second consumer (`zinder-analytics`, hypothetical) would link the same SDK without confusion.

### Dual-capability federation rule generalizes

Every federated method on `WalletQuery` that piggybacks on the explorer plane advertises two capabilities:

- `wallet.<surface>.<noun>_v{N}` — the always-on shape, computed from canonical artifacts at read time when the explorer proxy is unavailable.
- `explorer.<surface>.<noun>_v{N}` — the richer shape, signaling that the response carries the explorer-derived enrichment (mempool overlay, multi-source aggregation, etc.).

Clients that only need the canonical answer gate on the wallet capability. Clients that need the enrichment gate on the explorer capability. The wire response shape is identical between the two paths; only the semantic content (overlay present vs absent) differs.

This rule applies retroactively to `TransparentAddressBalance` and forward to every future federated method.

### Capability namespace structure

The explorer plane's capability namespace mirrors the wallet plane's:

- `explorer.server_info_v1`
- `explorer.<noun>.<capability>_v{N}` for read methods.

The noun is a domain category (`transaction`, `block`, `mempool`, `transparent_address`, `fee`, `value_pool`, `search`), and the capability names the operation (`detail_v1`, `summary_v1`, `activity_v1`). Examples:

- `explorer.transaction.detail_v1`
- `explorer.block.summary_v1`
- `explorer.mempool.summary_v1`
- `explorer.value_pool.summary_v1`
- `explorer.search.v1`

The `domain.noun.capability_v{N}` shape is identical to the wallet plane's `wallet.subdomain.capability_v{N}` pattern so the namespace stays predictable across surfaces.

### The explorer plane never calls upstream node RPCs

The explorer plane consumes canonical artifacts (via `ChainEpochReadApi` colocated reads or `WalletQuery` over gRPC) and replayable event streams (`WalletQuery.ChainEvents`, `WalletQuery.MempoolEvents`). It does not import `zinder-source`. It does not call Zebra.

If an explorer view needs a fact the canonical artifact and event surface do not expose (chain value pools, hypothetical future block-level analytics), the source boundary extends first. The new source fact lands on `NodeSource`, gets included in `SourceBlock` or a typed `Source*` value, then surfaces through a canonical artifact or chain event the explorer consumer can subscribe to.

This rule is structural: it keeps canonical artifacts as the single source of truth for chain facts, prevents the explorer plane from becoming a parallel chain-following process, and preserves the invariant that any explorer view is deterministically rebuildable from canonical state.

## Consequences

### Operational

- Operators running the prior `zinder-derive` migrate config: `[derive.explorer]` → `[explorer]`, `ZINDER_DERIVE__*` env vars → `ZINDER_EXPLORER__*`. The rename is a single coordinated change; there is no compatibility shim.
- Prometheus scrapes pick up the new `zinder_explorer_*` metric prefix; dashboards and alerts that grep for `zinder_derive_*` need an in-place edit.
- `WalletQuery.ServerInfo` capability lists shift from `derive.explorer.*` to `explorer.*`. Clients that probe `WalletQuery.ServerInfo` and gate on the old strings see the capability as absent until they update.
- A deployment that does not run `zinder-explorer` continues to advertise only the `wallet.*` capabilities. The wallet plane's federated method (`TransparentAddressBalance`) continues to answer from canonical UTXOs.

### Implementation

- The `services/zinder-derive/` directory moves to `services/zinder-explorer/`. The `Cargo.toml` package name, binary name, and `pkg-name` derivations all change in lockstep.
- `crates/zinder-proto/src/capabilities.rs` renames the two `DERIVE_EXPLORER_*` constants to `EXPLORER_*`; `ZINDER_CAPABILITIES` advertises the new strings.
- `services/zinder-query/src/grpc/adapter.rs` updates the federation gating: probe target capability becomes `explorer.transparent_address.balance_v1`; advertised capability matches.
- The `derive-plane.md` architecture doc stays as the SDK documentation. A new `explorer-plane.md` documents the product surface. Both reference each other.
- Doc edits land in the same change as the rename so the docs/code drift never exists. The `capability_docs.rs` test in `zinder-proto` already enforces that `ZINDER_CAPABILITIES` matches the public-interfaces capability list; that test fails until the docs and constants are aligned.

### Testing

- The validation gate passes after the rename. The `capability_coverage.rs` test in `zinder-client` references the new capability strings; the env-var docs mirror test in `zinder-runtime` references the new env-var names.
- One additional integration test in `services/zinder-explorer/tests/integration/` asserts that `ExplorerServerInfo.common.capabilities` contains both `explorer.server_info_v1` and (when the wallet endpoint is configured) `explorer.transparent_address.balance_v1`.
- Live tests under `services/zinder-explorer/tests/live/` retain their previous coverage and adjust env-var names.

## Alternatives Considered

### Keep the umbrella and add per-consumer namespaces over time

Rejected. The PRD framing is unambiguous: "Make explorer support a first-class Zinder product surface." Calling the deployable `zinder-derive` muddles that. The umbrella naming would also imply a multi-consumer future that does not exist today (YAGNI applies). When a second consumer materializes, it gets its own service binary (`zinder-analytics` or similar) and the SDK abstractions are already in place to support it.

### Rename `DeriveConsumer`/`DeriveStore`/etc. to `ExplorerConsumer`/`ExplorerStore`/etc.

Rejected. The SDK describes a pattern, not a product. Naming the trait `ExplorerConsumer` forces the second consumer to either rename the trait again (breaking change) or live with a confusing name. The boundary is "service binary entity" → product-renamed; "reusable SDK abstraction" → pattern-named. The `Derive*` prefix on the SDK is correct and stays.

### Keep `derive.explorer.*` as the capability namespace but rename only the binary

Rejected. The capability namespace is the durable contract; if the binary is `zinder-explorer`, the capabilities are `explorer.*`. A capability prefix that does not match the service name is harder to grep and pushes operators to context-switch between two vocabularies for the same thing.

### Ship a backwards-compatible alias for the old capability strings

Rejected. Zinder has no external compatibility burden ([ADR-0005](0005-consumer-neutral-wallet-data-plane.md)); aliasing keeps the old strings alive for no benefit and creates a permanent mental hop ("which is the canonical name?"). The clean cut is cheaper than the alias.

## Out of Scope

- Splitting the SDK into a dedicated `zinder-derive-sdk` crate. The shipped helpers under `services/zinder-explorer/src/consumer/` are reusable as a module path; extraction waits until a second consumer justifies the crate boundary.
- Renaming the chain-event subscription endpoint (`WalletQuery.ChainEvents`). The wallet-plane RPC stays in place; explorer consumers subscribe to it through `WalletQuery`, not through a parallel `ExplorerQuery.ChainEvents`.
- The explorer plane's full wire surface beyond `ServerInfo` and `TransparentAddressBalance`. Adding `TransactionDetail`, `BlockSummary`, `MempoolSummary`, `Search`, `FeeSummary`, `ValuePoolSummary`, `TransparentAddressActivity` is the work of subsequent slices and is documented in [Explorer plane](../architecture/explorer-plane.md). This ADR records the topology and namespace decision; the message vocabulary lands incrementally.
