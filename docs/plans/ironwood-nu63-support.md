# Ironwood (NU6.3) support

Zinder must keep indexing correctly across the NU6.3 (Ironwood) activation.
The upgrade introduces version 6 transactions and a new note-commitment tree.
Zinder's network-upgrade metadata path is already upgrade-agnostic (the running
node is the source of truth; branch ids and upgrade names are carried verbatim
from `getblockchaininfo`), so no activation table or branch-id code changes.
The work concentrates in two places: the `zebra-chain` dependency, which must
understand the v6 wire format before any Ironwood-era block can deserialize,
and the two exhaustive matches over `Transaction::V1..V5` that the compiler
forces open once `V6` exists.

Validation follows a mandatory ladder: regtest first, then testnet. Zebra
`v6.0.0-rc.0` is a release candidate and is not pointed at mainnet or the
Railway production deployment.

## Status

Phases 0 through 3 are complete on branch `feat/ironwood-nu63`; the full
Default Validation Gate passes. Phase 3 was validated against a z3 regtest
`zfnd/zebra:6.0.0-rc.0` node with NU5..NU6.3 at distinct heights: every
source-level v6-parsing live test and every ingest read-side live test passes,
including the NU6.2 to NU6.3 branch-id boundary and transparent indexing of v6
coinbase outputs.

Phase 4 pre-activation testnet validation passes against the real public
testnet on `zfnd/zebra:6.0.0-rc.0`. The 5.0.0 node upgraded in place to the rc
(state format v27 to v28 is a reusable major upgrade: rename plus a
genesis-tree backfill, no resync), reusing the synced chain. Public testnet has
not activated NU6.3 yet (activation height 4,134,000); all source and ingest
read-side live tests pass pre-activation. Authentic public-testnet v6
validation follows once the node crosses that height. Merge remains.

## Decisions

1. **`zebra-chain` moves from crates.io `9.0.0` to crates.io `11.0.0`.** The
   Ironwood surface (v6 transactions, Ironwood note-commitment tree, subtree
   exposure) ships first in `zebra-chain 11.0.0`, the release that decodes the
   version-6 wire format. It is a normal registry dependency; no git source is
   used.

2. **The librustzcash stack moves to the pre-release versions `zebra-chain
   11.0.0` requires, all from crates.io.** It depends on `zcash_protocol
   0.10.0-pre.0`, `zcash_primitives 0.29.0-pre.0`, `zcash_address 0.13.0-pre.0`,
   `zcash_transparent 0.9.0-pre.0`, and `orchard 0.15.0-pre.1`. Zinder bumps its
   own `zcash_address` (0.12), `zcash_protocol` (0.9), `zcash_primitives`
   (0.28), and `zip32` (0.2) to the versions that unify with `zebra-chain 11`,
   so no second copy of a librustzcash type exists at the `zebra-chain`
   boundary. No `[patch]` section is introduced.

3. **Version 6 is a first-class supported transaction version, not an
   `Unsupported` future header.** V6 transactions still carry transparent
   inputs and outputs, and Zinder's transparent indexing must cover them across
   the activation boundary. `TransactionVersion::V6` becomes a supported
   variant; the stale "`Unsupported` covers ... NU7 v6" doc note is corrected.

4. **NU6.3 stays node-discovered.** No branch id, activation height, or upgrade
   name for Ironwood is hardcoded in Zinder. The activation table already
   carries unknown upgrade names verbatim, so a node that advertises NU6.3 flows
   through unchanged. This preserves the single correct path across mainnet,
   testnet, regtest, and custom testnets.

5. **The Ironwood note-commitment tree is proxied, not computed, in v1.** Zinder
   already fills tree state from an upstream node through `TreeStateUpstream`
   and does not compute shielded-pool trees itself. Ironwood's tree follows the
   same boundary: served by proxying the upstream node, which handles it in
   `v6.0.0-rc.0`. Computing Ironwood subtree roots inside Zinder is out of
   scope until Ironwood-aware wallet clients require it.

6. **Environment ladder is regtest then testnet only.** Mainnet and the Railway
   production deployment stay on a stable Zebra; the rc image is never pointed
   at them. The renamed 275 GB synced mainnet volume is reserved for a later
   mainnet stage gated on a stable Ironwood Zebra release.

## Phases

### Phase 0: dependency-stack uplift (critical path)

Bump `zebra-chain` to `11.0.0` and align the librustzcash pins (Decisions 1,
2). Resolve every compile break across the `zebra_chain::` call sites:
`transparent::Address`, `parameters::NetworkKind`, `transaction::{Transaction,
LockTime}`, `serialization::{SerializationError, ZcashDeserializeInto}`, and
`network_upgrade`. The two exhaustive `Transaction` matches in
`crates/zinder-source/src/source_transaction.rs` break here and are extended in
Phase 1.

Gate: the full Default Validation Gate green, starting with `cargo check
--workspace --all-targets --all-features`.

### Phase 1: v6 transaction classification

Add `TransactionVersion::V6` in
`crates/zinder-core/src/transaction_public_facts.rs` as a supported variant and
correct the stale version-coverage doc note (Decision 3). Extend
`classify_transaction_version` and `resolve_consensus_branch_id` in
`crates/zinder-source/src/source_transaction.rs` with the `Transaction::V6`
arms; v6 carries its own `network_upgrade()`, so branch-id resolution takes the
existing early-return path. The transparent, Sapling, and Orchard accessors in
`zebra-chain 11` already return v6 data (the v6 Orchard bundle unwraps its
`ShieldedDataV6` wrapper through `orchard_actions()`), so
`transaction_component_counts` reports those components without change. Add a v6
fixture round-trip test in `crates/zinder-proto/tests/` and a
`parse_transaction_public_facts` test.

Gate: facts and wire-invariant tests; `capability_string_uniqueness`.

### Phase 2: Ironwood shielded surface (counts, tree, value pool)

`zebra-chain 11` exposes the new Ironwood bundle through
`ironwood_actions()`/`ironwood_nullifiers()`, which
`transaction_component_counts` does not count. Add an `ironwood_action_count`
to `TransactionComponentCounts` so the explorer's component reporting is
complete and its ZIP-317 conventional-fee floor
(`zip317_conventional_fee_zat`) treats Ironwood actions as logical actions on
v6 transactions. This changes a wire-carried counts type, so update the proto
and its round-trip test in the same slice.

Document the proxy boundary for the Ironwood note-commitment tree (Decision 5)
in the relevant architecture doc. Confirm a new Ironwood value pool surfaces
through the existing `repeated ChainValuePool` shape in
`ExplorerQuery.ValuePoolSummary` without a proto change. Verify `tree_state_at`
and `subtree_roots` behave correctly when the upstream serves an empty Ironwood
tree before activation.

Gate: facts, fee-summary, explorer value-pool, and tree-state probes.

### Phase 3: z3 regtest activation and live validation

Add NU6.2 and NU6.3 activation heights to `config/regtest/zebra.toml` in the z3
stack, sequential above NU6.1 (`NU6.1 = 1000`), for example `"NU6.2" = 1000`
and `"NU6.3" = 1010`. Mirror the branch ids in `config/regtest/zallet.toml`'s
`regtest_nuparams`: NU6.2 is `5437f330`, NU6.3 (Ironwood) is `37a5165b`. Point
`Z3_ZEBRA_IMAGE` at `zfnd/zebra:6.0.0-rc.0`.

Cross-stack constraint: the current z3 regtest activation heights are pinned to
match Zaino's hardcoded table and an older Zallet whose `zcash_protocol` (0.7.2)
cannot parse the NU6.2 branch id. Exercising NU6.3 therefore requires an
Ironwood-aware Zallet (built against `zcash_protocol 0.10.x`) and either a
Zaino that accepts the new heights or a regtest bring-up that does not depend on
Zaino. Isolate the Ironwood heights in a regtest override rather than mutating
the shared Zaino-matched config so the existing regtest path is not disturbed.

Run the Zinder live regtest suite across the NU6.3 activation boundary: v6
transaction indexing, transparent funding round-trip, and a reorg spanning the
boundary.

Gate: `cargo nextest run --profile=ci-live` green on regtest.

### Phase 4: testnet ladder

Point the z3 testnet stack at `zfnd/zebra:6.0.0-rc.0`. A synced 5.0.0 node
upgrades in place: the v27 to v28 state format change is a reusable major
upgrade, so Zebra renames the directory and backfills the genesis Ironwood
tree rather than resyncing. Run the Zinder live testnet suite (cookie auth).
Public testnet activates NU6.3 at height 4,134,000; before the node crosses it,
validate the pre-activation path (real testnet blocks index, the upgrade table
reports NU6.3 pending, reachable branch-id boundaries hold). After it crosses,
validate v6 indexing on authentic public-testnet blocks.

Gate: `cargo nextest run --profile=ci-live` green on testnet (source-level and
ingest read-side; mining-dependent tests do not run against a public chain).

Mainnet validation is deferred until a stable Ironwood Zebra release exists
(Decision 6).
