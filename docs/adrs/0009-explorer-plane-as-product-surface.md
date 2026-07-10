# ADR-0009: Explorer Plane As First-Class Product Surface

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Service topology, explorer wire surface, capability namespace, federation contract |
| Related | [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0005](0005-consumer-neutral-wallet-data-plane.md), [ADR-0006](0006-ingest-control-transport-security.md), [ADR-0007](0007-mempool-topology-and-retention.md), [Explorer plane](../architecture/explorer-plane.md), [Derive plane](../architecture/derive-plane.md), [Service boundaries](../architecture/service-boundaries.md), [Public interfaces](../architecture/public-interfaces.md) |

## Revision history

- 2026-06: Transparent-address balance is computed in the wallet plane behind one native primitive (`WalletQuery.TransparentAddressBalance`, capability `wallet.address.transparent_balance_v1`), not federated through the explorer. The wallet handler sums the confirmed total in-process from the canonical unspent-output index and overlays the live mempool delta through the colocated ingest-control endpoint. The explorer plane no longer serves a balance RPC, and the wallet plane no longer dials the explorer. The dual-capability federation rule remains the contract for any future federated explorer method on `WalletQuery`; balance is no longer one of them.

## Context

Explorer support is a product surface, not an implementation pattern. The
product answer is `zinder-explorer`: operators reading `ps aux`, contributors
reading `services/`, and integrators reading the capability list should all see
the same word.

At the same time, the SDK that powers the explorer is reusable. `DeriveConsumer`, `DeriveStore`, `DeriveProxy`, and the `DeriveStore::write_*` dispatch entry points describe a pattern (chain and mempool events flowing into stateful consumers with atomic cursor persistence), not the explorer product specifically. Additional consumers link the same SDK and ship their own service binaries.

The decisions to record here are:

1. The service binary, config namespace, capability namespace, and Prometheus prefix use `explorer`.
2. The SDK names (`DeriveConsumer`, `DeriveStore`, etc.) stay because they describe the pattern, not the product.
3. The dual-capability federation rule from ADR-0007's mempool work generalizes: a federated explorer method on `WalletQuery` advertises one always-on `wallet.*` capability plus one optional richer `explorer.*` capability.
4. The explorer plane must not call upstream node RPCs directly. Source-boundary extensions land first; the explorer consumes canonical artifacts and chain/mempool events.

## Decision

### The service is `zinder-explorer`

Production processes, config keys, capability strings, and Prometheus metrics use one namespace:

| Surface | Value |
| ------- | ----- |
| Service crate | `services/zinder-explorer/` |
| Cargo package | `zinder-explorer` |
| Binary | `zinder-explorer` |
| Config TOML namespace | `[explorer]` |
| Environment-variable prefix | `ZINDER_EXPLORER__*` |
| Capability prefix | `explorer.*` |
| Prometheus metric prefix | `zinder_explorer_*` |
| CLI flag namespace | `--explorer-*` |

The explorer plane exposes this baseline capability string:

| Capability | Meaning |
| ------ | ----- |
| `explorer.server_info_v1` | Explorer server-info descriptor |

Transparent-address balance is a wallet-plane primitive, not an explorer capability: `WalletQuery.TransparentAddressBalance` advertises `wallet.address.transparent_balance_v1` and is documented in [Wallet data plane](../architecture/wallet-data-plane.md).

### SDK names stay derive-shaped

The reusable SDK abstractions keep their `Derive*` names because they describe the integration pattern, not the explorer product:

- `DeriveConsumer` (trait), `DeriveMempoolConsumer` (trait)
- `DeriveStore` (RocksDB wrapper), `DeriveStoreTable`, `DeriveConsumerName`
- `DeriveConsumerCtx` (per-event context with `&DeriveStore` + `&mut WriteBatch`)
- `DeriveStore::write_chain_event`, `DeriveStore::write_mempool_event`

The boundary is "Zinder process or library entity belonging to the running explorer service" -> `explorer` namespace; "reusable SDK abstraction representing the derive pattern" -> `Derive*` name.

### Dual-capability federation rule generalizes

Every federated method on `WalletQuery` that piggybacks on the explorer plane advertises two capabilities:

- `wallet.<surface>.<noun>_v{N}` — the always-on shape, computed from canonical artifacts at read time when the explorer proxy is unavailable.
- `explorer.<surface>.<noun>_v{N}` — the richer shape, signaling that the response carries the explorer-derived enrichment (mempool overlay, multi-source aggregation, etc.).

Clients that only need the canonical answer gate on the wallet capability. Clients that need the enrichment gate on the explorer capability. The wire response shape is identical between the two paths; only the semantic content (overlay present vs absent) differs.

This rule governs any future federated explorer method on `WalletQuery`. It has no current instance: transparent-address balance, which originally motivated the rule, is now a wallet-plane primitive that computes its own mempool overlay in-process.

### Capability namespace structure

The explorer plane's capability namespace mirrors the wallet plane's:

- `explorer.server_info_v1`
- `explorer.<noun>.<capability>_v{N}` for read methods.

The noun is a domain category (`transaction`, `block`, `mempool`, `transparent_address`, `fee`, `value_pool`, `search`), and the capability names the operation (`detail_v1`, `summary_v1`, `activity_v1`). Examples:

- `explorer.transaction.detail_v3`
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

- Operators configure the explorer through `[explorer]` and `ZINDER_EXPLORER__*`.
- Prometheus scrapes use the `zinder_explorer_*` metric prefix.
- A deployment that does not run `zinder-explorer` continues to advertise only the `wallet.*` capabilities, including `wallet.address.transparent_balance_v1`. The wallet plane answers balance from canonical UTXOs and degrades the mempool overlay to a zero delta when no ingest-control endpoint is wired.

### Implementation

- `services/zinder-explorer/` owns the explorer binary and gRPC service.
- `crates/zinder-proto/src/capabilities.rs` exposes the `EXPLORER_*` constants.
- `services/zinder-query/src/grpc/adapter.rs` computes `TransparentAddressBalance` in-process and overlays the mempool delta through the ingest-control endpoint.
- `derive-plane.md` documents the SDK boundary. `explorer-plane.md` documents the product surface. Both reference each other.
- The capability docs tests enforce that the wallet and explorer rows of the `CAPABILITIES` table match the public-interfaces capability list.

### Testing

- The validation gate covers capability strings and env-var docs. The `capability_coverage.rs` test in `zinder-client` references the explorer capability strings; the env-var docs mirror test in `zinder-runtime` references the explorer env-var names.
- The balance overflow, saturation, and per-request address-cap unit tests live in the wallet plane (`zinder-core` and `services/zinder-query/tests/integration/transparent_address_balance.rs`).
- Live tests under `services/zinder-explorer/tests/live/` retain their previous coverage and adjust env-var names.

## Alternatives Considered

### Use an implementation-pattern service name

Rejected. Explorer is a first-class product surface. The deployable name should
match what operators run and what integrators discover in capability strings.
Reusable SDK abstractions already carry the implementation-pattern name.

### Rename `DeriveConsumer`/`DeriveStore`/etc. to `ExplorerConsumer`/`ExplorerStore`/etc.

Rejected. The SDK describes a pattern, not a product. Naming the trait
`ExplorerConsumer` would force every non-explorer consumer through a product
name it does not own. The boundary is service product name -> `explorer`;
reusable SDK abstraction -> `Derive*`.

### Use a capability namespace that differs from the binary

Rejected. The capability namespace is the durable contract; if the binary is
`zinder-explorer`, the capabilities are `explorer.*`. A capability prefix that
does not match the service name is harder to grep and pushes operators to
context-switch between two vocabularies for the same thing.

## Out of Scope

- Splitting the SDK into another crate. The reusable consumer traits, store wrapper, and dispatch entry points live in `crates/zinder-derive`; another split waits until a second consumer justifies the crate boundary.
- Renaming the chain-event subscription endpoint (`WalletQuery.ChainEvents`). The wallet-plane RPC stays in place; explorer consumers subscribe to it through `WalletQuery`, not through a parallel `ExplorerQuery.ChainEvents`.
- The explorer plane's full wire vocabulary is owned by [Explorer plane](../architecture/explorer-plane.md). This ADR records the topology and namespace decision.
