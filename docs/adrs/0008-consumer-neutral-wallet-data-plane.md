# ADR-0008: Consumer-Neutral Wallet Data Plane

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Wallet data plane, external wallet compatibility, typed client boundary |
| Related | [Wallet data plane](../architecture/wallet-data-plane.md), [Chain ingestion](../architecture/chain-ingestion.md), [Public interfaces](../architecture/public-interfaces.md), [Service operations](../architecture/service-operations.md) |

## Context

A Zinder store bootstrapped near the upstream-node tip can satisfy basic lightwalletd smoke probes and a new-wallet happy path, then fail later when a wallet asks for historical artifacts. Android SDK and Zashi expose the issue first, but the gap is not Android-specific: the same wallet data-plane needs appear across the local ecosystem.

- `lightwalletd` exposes `GetTreeState`, `GetSubtreeRoots`, `GetAddressUtxos`, and `GetAddressUtxosStream` as first-class `CompactTxStreamer` methods.
- Zallet's `wallet` code fetches birthday tree state at `birthday - 1`, loads Sapling and Orchard subtree roots from index `0`, and polls transparent UTXOs for wallet-owned transparent receivers.
- Zaino's direction is both compatibility RPCs and a Rust client/state-service model, with `ChainIndex` snapshot semantics for atomic non-finalized reads.

The architectural risk is treating the first failing wallet as the design center. A Zashi-shaped patch would make the current demo pass while leaving Zinder without a durable consumer contract. The opposite risk is becoming lightwalletd: compatibility is necessary, but it is not Zinder's native product identity.

## Decision

Zinder's durable product boundary is a **consumer-neutral wallet data plane**:

- Canonical artifacts are indexed once by `zinder-ingest`.
- Native consumers use typed, epoch-pinned Rust or gRPC surfaces.
- `zinder-compat-lightwalletd` is an adapter over the same stored artifacts.
- External wallet compatibility is a coverage contract, not an app-specific workaround.

The core contract is:

1. Serve compact block ranges from canonical artifacts.
2. Serve tree state for explicit block anchors.
3. Serve subtree roots by shielded pool and subtree index.
4. Serve transparent UTXOs by address set and start height.
5. Preserve chain consistency through `ChainEpoch` and snapshot-like request pinning.
6. Report readiness and artifact availability truthfully.

### Wallet-serving coverage

`wallet-serving` is the operator-facing coverage profile for stores intended to serve wallet flows. It means the store was built with enough historical artifact coverage for wallet creation, recovery, rescan, imported-account, and transparent-UTXO flows supported by the published API.

`wallet-serving` is not a Zashi profile and not a lightwalletd profile. It is the conservative store posture for wallet consumers. The backfill floor is derived from upstream-node-advertised shielded activation heights, because that is the simplest general rule that covers subtree roots from index `0` and historical tree-state anchors without encoding public-network constants into docs or config.

Serving coverage fails closed:

- `wallet-serving` rejects explicit `from_height` and `checkpoint_height` overrides.
- `wallet-serving` rejects `allow_near_tip_finalize`; a serving store must stop backfill outside the configured reorg window and let `tip-follow` ingest the replaceable suffix.
- Missing artifacts remain `ArtifactUnavailable`. Query services do not synthesize responses from upstream nodes.
- Readiness does not claim production traffic is safe before secondary catchup and writer-status validation have established the reader's state.

### Compatibility and native surfaces

`zinder-compat-lightwalletd` implements legacy `CompactTxStreamer` semantics as an adapter. It may preserve lightwalletd field names and wire behavior, but it does not own storage, query semantics, or product vocabulary.

`zinder-client::ChainIndex` is the native Rust integration direction for Zallet and future in-process consumers. Its method names, typed errors, and epoch-pinned variants may diverge from lightwalletd when that improves DX, UX, or AX. Compatibility and native-client readiness are validated separately.

### Naming

Use these terms consistently:

- `wallet birthday`: the user or wallet-provided lower bound for recovery/setup.
- `scan range start`: the start of a scanner-selected range.
- `tree-state anchor`: the block height whose tree state initializes a scan, commonly `scan_range_start - 1` or `birthday - 1`.
- `artifact coverage floor`: the earliest height or index available for an artifact family.
- `serving store`: a store built with `wallet-serving` coverage and caught up by `tip-follow`.

Do not use `birthday` as shorthand for every historical tree-state lookup. Many tree-state requests are scan anchors, not wallet birthdays.

## Consequences

Positive:

- Compatibility work has one general target instead of app-specific exceptions.
- Operators get one clear serving profile: build a serving store, then run readers against it.
- Wallet failures caused by insufficient historical coverage become deployment/readiness failures, not hidden query fallbacks.
- Zallet and other Rust consumers get a native typed surface without inheriting lightwalletd's wire vocabulary.
- Agents reason about coverage by artifact family and anchor height instead of guessing which wallet flow caused a lookup.

Negative:

- Initial serving stores are larger and slower to build than recent-checkpoint fixtures.
- Local test workflows that previously used near-tip finalized backfill switch to explicit disposable stores or tip-follow.
- Full prevention of excessive transparent-UTXO materialization across many addresses requires a deeper multi-address store API; the aggregate response budget bounds the read until that lands.

Tradeoffs:

The backfill floor is intentionally conservative. A future profile can narrow coverage once the product has real demand for bounded historical ranges, but the first stable profile optimizes for correct wallet behavior and low operator ambiguity.

Upstream-node fallback is rejected. It would blur the source of truth, make readiness lie, and turn query services into partial node proxies. Repair tools may use upstream nodes to rebuild canonical artifacts, but public query methods read stored artifacts only.

## Out of Scope

- Zashi-only endpoint behavior.
- A separate per-wallet coverage profile.
- Public by-address shielded queries.
- Mempool UTXO completeness; M3 owns mempool indexing.
- Cross-host read-replica architecture.
