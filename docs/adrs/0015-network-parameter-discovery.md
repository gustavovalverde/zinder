# ADR-0015: Per-Network Consensus Parameters Discovered From The Running Node

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-12) |
| Product | Zinder |
| Domain | Per-network consensus-rule data flow across process boundaries |
| Related | [Chain ingestion](../architecture/chain-ingestion.md), [Public interfaces](../architecture/public-interfaces.md), [Service boundaries](../architecture/service-boundaries.md), [ADR-0006](0006-test-tiers-and-live-config.md), [ADR-0007](0007-multi-process-storage-access.md) |

## Context

Three pieces of per-network consensus data flow through Zinder's read path: activation heights (Sapling, NU5, NU6, NU6\_1, ...), the consensus branch id active at a given height, and the human-facing upgrade name. Zinder serves them on at least three observable surfaces:

- `WalletQuery.Transaction` (`MinedDetails.consensus_branch_id`), consumed by Zinder-native wallets via the native gRPC and by the in-process local client (`zinder-client/local`).
- `compat-lightwalletd::GetLightdInfo` (`saplingActivationHeight`, `consensusBranchId`, `upgradeName`, `upgradeHeight`), consumed by every lightwalletd-compatible wallet: Zashi, Zodl, librustzcash-based wallets, and any future lightwalletd client.
- Transparent V5 transaction signing in `zinder-testkit`, which needs the active branch id to compute ZIP-244 sighashes; mismatched heights cause Zebra to reject broadcasts with `incorrect consensus branch id`.

Mainnet and testnet have stable activation schedules baked into upstream `zebra-chain`. Regtest and operator-configured custom testnets do not: their activation heights come from each operator's `zebrad.toml` (or equivalent) and are only authoritative on the running node.

Before this ADR, Zinder constructed a static `OnceLock<ZebraNetwork>` per-variant using `RegtestParameters::default()` for regtest. That singleton produced the wrong active upgrade on regtest: `RegtestParameters::default()` leaves NU5/NU6/NU6\_1 unset, so `NetworkUpgrade::current` falls back to Canopy at height 1, even when the running Zebra has NU6 active at height 2. The bug surfaced through `GetLightdInfo` reporting `consensusBranchId = e9ff75a6 (Canopy)` and `upgradeName = "Canopy"` while Zaino on the same node reported `c8e71055 (NU6)` and `"NU6"`. The same wrong branch id silently propagated into `MinedDetails.consensus_branch_id` on every native wallet `Transaction` lookup. The transparent signer's `regtest_local_network()` had a parallel, independently maintained hardcoded copy of the same heights, equally susceptible to drift.

The pattern question is not "what numbers should the regtest singleton hold." It is "where in the architecture does a Zinder process learn the per-network consensus activations, and how is that knowledge shared with every component that needs it." Without a single answer, every new read-path consumer would rediscover the same trap and risk a different copy of the same constants.

## Decision

Zinder treats the running upstream node as the source of truth for per-network consensus parameters on every supported network, including mainnet and testnet. The activations are parsed once at process startup from `getblockchaininfo.upgrades` and carried through the process lifetime as an immutable, shared `Arc<NetworkUpgradeActivations>`. Every consumer that needs consensus parameters accepts the table at construction time; no service degrades silently to `PRE_OVERWINTER_BRANCH_ID` when the discovery call fails.

### The carrier type

`zinder_core::NetworkUpgradeActivations` owns:

- A `Network` identifier (the chain the table describes).
- A `Vec<NetworkUpgradeActivation>` sorted by `activation_height` ascending. Each entry is `{ branch_id: u32, activation_height: BlockHeight, name: String }`. The `name` is carried verbatim from the node's `getblockchaininfo.upgrades[<branch>].name` so future upgrades remain serviceable without a Zinder code change.

The type exposes pure-data queries:

- `active_at(height: BlockHeight) -> Option<&NetworkUpgradeActivation>`: the highest activation with `activation_height <= height`.
- `consensus_branch_id_at(height: BlockHeight) -> u32`: defaults to `PRE_OVERWINTER_BRANCH_ID` (0) for heights below the earliest activation.
- `activation_height_by_name(name)` and `activation_height_by_branch_id(branch_id)` for targeted lookups.
- `earliest_wallet_servable_activation()` for the wallet-serving backfill floor and the lightwalletd shim. Returns the activation itself (name and height) so callers don't have to re-look-up the upgrade by name.

### Source-of-truth flow

1. `ZebraJsonRpcSource::fetch_network_upgrade_activations()` parses `getblockchaininfo.upgrades` and validates branch-id uniqueness. The map key (hex branch id) becomes `branch_id`; the value's `name` and `activationheight` become `name` and `activation_height`.
2. `ZebraJsonRpcSource::discover_network_upgrade_activations(target)` wraps the fetch, emits a `network_upgrade_activations_discovered` structured log event keyed by service name, and returns `Arc<NetworkUpgradeActivations>`. The four service binaries call this one helper so the discovery event shape stays consistent across the workspace.
3. Consumers receive `Arc<NetworkUpgradeActivations>` at construction and call methods on the owned type. No process ever consults `zcash_chain::Network::sapling_activation_height` or any other library-default constant.

The free-standing helpers `zinder_source::zebra_network()` and `zinder_source::consensus_branch_id_at()` are removed. The `RegtestParameters::default()` codepath is removed from production code. The testkit retains `regtest_local_network()` (derived from `sample_regtest_upgrade_activations()` so they cannot drift) for in-process unit-test fixtures that do not broadcast; both helpers carry doc comments steering live tests toward `local_network_from_activations(&activations)` and `discover_network_upgrade_activations()`.

### Wiring contract per service

- **`zinder-ingest`** discovers the activations for backfill-floor resolution (the existing `--wallet-serving` path). Held in the binary main; no further plumbing in this ADR.
- **`zinder-query`** holds `Arc<NetworkUpgradeActivations>` on `WalletQuery` as a required constructor parameter. `MinedDetails.consensus_branch_id` always reflects the running node's active branch at the mined height. The production binary errors out at startup if `[node]` is not configured (the table cannot be discovered without an RPC endpoint).
- **`zinder-compat-lightwalletd`** holds `Arc<NetworkUpgradeActivations>` on `LightwalletdGrpcAdapter` as a required constructor parameter. `GetLightdInfo` reads the table directly. Same startup-time failure if `[node]` is not configured.
- **`zinder-client/local`** holds the same `Arc<NetworkUpgradeActivations>` on `LocalChainIndex` via a required `LocalOpenOptions.network_upgrade_activations` field.
- **`zinder-testkit`** exposes `local_network_from_activations(&activations) -> LocalNetwork` for live tests; `TransparentTestKey::from_seed_with_local_network` accepts the result. `regtest_local_network()` is derived from `sample_regtest_upgrade_activations()` so the two regtest fixtures cannot drift.

### Regression test

A live test in `crates/zinder-source/tests/live/zebra_json_rpc.rs` calls `fetch_network_upgrade_activations()` against the configured node, asserts the table advertises Sapling at a height ≥ 1, asserts `active_at(tip).name` is non-empty, and asserts `consensus_branch_id_at(tip)` is non-zero whenever tip ≥ Sapling. The test runs under `ci-live` on regtest, testnet, and mainnet. If a future zebra-chain bump silently changes regtest defaults again, or the proto shape changes, this test catches it.

## Consequences

### What this enables

- Operators can iterate on regtest activation heights via their `zebrad.toml` without forking Zinder or coordinating a code change. Restart Zinder to pick up the new table.
- `MinedDetails.consensus_branch_id` is correct on every network for every supported upgrade automatically; no Zinder code-change tax per upgrade.
- New consumers that need consensus parameters have one obvious answer: accept `Arc<NetworkUpgradeActivations>` in your constructor; the production binary already discovers and shares one.
- Custom testnets become first-class: the table discovers itself from the node; nothing in Zinder hardcodes mainnet/testnet activation heights.
- Required-at-construction wiring eliminates the "silently serves branch id 0 forever" failure mode: a misconfigured service refuses to start instead of misreporting `MinedDetails.consensus_branch_id` for its entire lifetime.

### What this costs

- Each process pays one `getblockchaininfo` round-trip at startup. The cost is the same call already required for backfill-floor resolution where applicable; for query and compat shim it adds one round-trip.
- The table is cached for process lifetime. If the operator reconfigures the node's activation heights mid-flight without restarting Zinder, the cache goes stale until restart. Documented in the runbook.
- Tests that exercise `WalletQuery` or `LightwalletdGrpcAdapter` must pass an activations table. The testkit's `sample_regtest_upgrade_activations()` covers the common case; the cost is one extra line per test.

### Out of scope for this ADR

- Sharing the discovered table via `IngestControl` from the writer to readers. The current per-process discovery is the smallest correct fix; the unified-writer-source pattern is a future extension if cross-process drift becomes a real concern. The `NetworkUpgradeActivations` carrier type does not change shape in that future.
- Persisting the table in the on-disk store metadata. Discovery from the live node remains the source of truth ([ADR-0006](0006-test-tiers-and-live-config.md), [ADR-0007](0007-multi-process-storage-access.md)). Operators iterating on regtest must not be punished by a store-reset cost.

## Vocabulary

- `NetworkUpgradeActivations` (the carrier).
- `NetworkUpgradeActivation` (one row).
- `PRE_OVERWINTER_BRANCH_ID = 0` (the wire convention for "no branch active at this height").
- `ZebraJsonRpcSource::fetch_network_upgrade_activations` (the discovery function).
- `ZebraJsonRpcSource::discover_network_upgrade_activations(target)` (the shared startup wrapper that returns `Arc<NetworkUpgradeActivations>` and emits a structured discovery log).
- `local_network_from_activations` (the testkit bridge to `zcash_protocol::LocalNetwork`).

See [Public interfaces §Domain types](../architecture/public-interfaces.md#domain-types).
