# Zaino feature comparison and inheritance candidates

> **Snapshot date:** 2026-05-27
> **Compared release:** zainod 0.3.1 (2026-05-25)
> **Sources:** Zaino [`CHANGELOG.md`](../../../zaino/CHANGELOG.md) "Unreleased" plus v0.2.0 entries; Zaino source tree at `../zaino`; release-announcement thread at <https://forum.zcashcommunity.com/t/zainod-release-announcements/55845>.

This document records a feature-by-feature comparison between Zinder and the Zaino 0.3.1 release. For every Zaino feature the comparison asks one question: should Zinder inherit it, reimplement it differently, or skip it because the same outcome is already covered by a Zinder-native mechanism. Each "inherit" candidate is sized roughly so the team can decide whether to convert it into an ADR plus an implementation slice.

This is a discovery note, not a commitment. Retire each item once it becomes an ADR or a closed-out follow-up.

## What this document is not

- Not a roadmap commitment. The team can drop any item below after a closer look.
- Not an ADR. Items that survive design review get their own ADR; items that close-out without an architectural change get crossed off here and removed.
- Not a competitive-positioning document. The reading frame is technical inheritance, not market comparison.

## Findings at a glance

| Zaino feature (release 0.3.1) | Status in Zinder | Action |
| --- | --- | --- |
| `gettxoutsetinfo` served from local UTXO-set accumulator (XOR-of-BLAKE2b-256 multiset over the transparent UTXO set) | No equivalent. `ChainValuePoolsAtTip` is a passthrough of `getblockchaininfo.valuePools` from Zebra and reports only sums-per-pool. | **Inherit as a derive consumer.** See [§1](#1-transparent-utxo-set-summary-as-a-derive-consumer). |
| Spent-outpoint index promoted to core data (Zaino DB v1.2.0) | Already a canonical hot table: `transparent_spend_fact` in [`fact-first-indexer.md`](../architecture/fact-first-indexer.md). | None. Naming differs; semantics identical. |
| `ChainIndex` exposes `get_address_balance`, `get_address_deltas`, `get_address_txids`, `get_address_utxos`; Zaino's `validator_connector` routes them to Zebra's JSON-RPC | Zinder has real local indexes via `address_output_index`, `transparent_spend_fact`, `TransparentAddressTxIndex`, plus pagination and mempool overlay. | None at the surface level. **One real gap**: spending-side history. See [§2](#2-spending-side-transparent-address-history). |
| Non-finalised-state serviceability policy | Different model. Zinder pins reads to a committed `ChainEpoch`; live tip surfaces through `MempoolSnapshot` + `MempoolEvents`. | **Do not inherit.** See [§Explicitly not inheriting](#explicitly-not-inheriting). |
| Block lookups by hash or height | `BlockSelector::{Hash,Height}` shared by `WalletQuery` and `ExplorerQuery`. | None. |
| Subtree-root reporting | `WalletQuery.SubtreeRootsInRange`. | None. |
| `JsonRpSeeConnector::get_tree_state` returns optional sapling/orchard fields in regtest | Tree state is bytes-shaped via `WalletQuery.TreeState`; regtest behaviour driven by per-`Network` parameterization. | **Audit only.** See [§Follow-up audits](#follow-up-audits). |
| `z_validateaddress` passthrough ("shipped pre-deprecated for bugwards compatibility") | Address typing is client-side via the `zcash_address` crate. `ExplorerQuery.Search` already classifies typed candidates. | **Do not inherit.** See [§Explicitly not inheriting](#explicitly-not-inheriting). |
| `LightdInfo.version` reports running binary version | Already reports `env!("CARGO_PKG_VERSION")` in [`services/zinder-compat-lightwalletd/src/grpc.rs:1144`](../../services/zinder-compat-lightwalletd/src/grpc.rs). | None. |
| Adoption of lightclient-protocol v0.4.0 (`CompactTxIn`, `TxOut`, `PoolType`, `BlockRange.poolTypes`, `CompactTx.vin/vout`) | Vendored proto pin already on v0.4.0 schema; see [`crates/zinder-proto/proto/compat/lightwalletd/compact_formats.proto:70`](../../crates/zinder-proto/proto/compat/lightwalletd/compact_formats.proto). | **Audit:** confirm the compact-block builder populates `vin`/`vout`. See [§3](#3-compacttx-vin-vout-population-in-the-lightwalletd-compat-builder). |
| In-place resumable finalised-state DB migration | Migration ownership belongs to the writer per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md). Resumability semantics are not as prominently documented as Zaino's. | **Audit only.** See [§Follow-up audits](#follow-up-audits). |
| Dedicated public Docker Hub repo `zingodevops/zainod` with SHA-pinned image | `deploy/` ships local-build Docker compose plus per-network env files. | Release-engineering decision for ZFND; not in scope here. |
| Integration tests migrated to `corez` because `core2` is yanked | Workspace `Cargo.toml` patches `core2` to a git source, gated by cargo-deny `allow-git`. | None. Parallel fix, different vehicle. |

## Inheritance candidates

### 1. Transparent UTXO-set summary as a derive consumer

**Goal.** Surface the same operator-facing answer Zaino's `gettxoutsetinfo` gives (UTXO count, total transparent value, byte-length of the canonical UTXO encoding, and an optional cryptographic commitment), but compute it inside Zinder's derive plane instead of asking Zebra.

**Status today.** `ChainValuePoolsAtTip` ([`crates/zinder-core/src/chain_value_pools.rs:41`](../../crates/zinder-core/src/chain_value_pools.rs)) carries sums per pool. It is a passthrough from Zebra's `getblockchaininfo.valuePools`, gated by `NodeCapability::ChainValuePools`. The wallet capability surfaces it via `wallet.read.chain_value_pools_at_tip_v1`; the explorer capability is `explorer.value_pool.summary_v1`. Neither reports UTXO count, neither reports a commitment, neither is computed locally.

**Why a derive consumer, not canonical state.**

1. The fact-first rule ([`docs/architecture/fact-first-indexer.md`](../architecture/fact-first-indexer.md)) is that canonical storage holds typed facts; aggregations over those facts belong to the derive plane.
2. The commitment scheme is indexer-defined, not consensus-defined. Putting a non-consensus hash in canonical state would conflict with ADR-0003's storage discipline.
3. A failing summary consumer must leave wallet sync healthy. That requirement is automatic on the derive plane via [ADR-0017](../adrs/0017-derive-consumer-template-and-key-codec-convention.md) and would be hand-coded if hosted on the canonical writer.

**Proposed shape.**

- New derive consumer `TransparentUtxoSetSummaryConsumer` that tails `chain_event`, reads `transparent_output` and `transparent_spend_fact`, and writes a single keyed row per network into the derive store.
- Stored value carries (`utxo_count: u64`, `total_zats: u64`, `bytes_serialized: u64`, optional `utxo_set_commitment: [u8; 32]`, `derived_at_height`, `derived_at_block_hash`).
- New RPC: `ExplorerQuery.TransparentUtxoSetSummary` (signature: takes no parameters, returns the freshness-stamped row).
- Proposed capability strings: `explorer.transparent_utxo_set_summary_v1` and, if the commitment is enabled, `explorer.transparent_utxo_set_commitment_v1`. The two capabilities split so operators can disable the commitment without losing the summary.

**Commitment scheme: open question.**

Zaino uses XOR-of-BLAKE2b-256 over a 65-byte canonical entry (`prev_txid || output_index || value || script_hash || script_type`) with domain tag `b"ZcashTxOutSet___"` ([`packages/zaino-state/src/chain_index/types/db/metadata.rs:24-66`](../../../zaino/packages/zaino-state/src/chain_index/types/db/metadata.rs)). The scheme is order-independent and supports cheap removal because XOR is self-inverse.

Two paths Zinder could take:

1. **Match Zaino's scheme byte-for-byte.** Cross-indexer reconciliation becomes a one-call probe: any Zinder operator and any Zaino operator at the same tip should publish the same hash. Cost: we commit to Zaino's domain-separation choice.
2. **Define a Zinder-specific multiset commitment.** Independence from Zaino's evolution, including any future domain-tag changes Zaino might make.

Recommendation: match Zaino's scheme exactly, behind the optional capability. The reconciliation property is the most valuable thing the commitment buys; if the operator does not care about cross-indexer parity, they can disable it via capability.

**Effort sizing.** One derive consumer plus one RPC. Reference patterns: `BlockSummaryConsumer` (the first real `DeriveConsumer`, see memory `M5/M6` notes). Probably one focused slice; close-out becomes ADR-0025 or later.

**Open questions worth flagging in the ADR.**

- Are coinbase outputs counted before maturity? Zaino's `is_unspendable_tx_out` excludes only non-P2PKH/P2SH scripts; coinbase before 100 confirmations is *not* excluded. Zinder should declare its policy explicitly.
- Reorg behaviour: the consumer must replay against `chain_event` revert envelopes. The XOR-multiset makes this cheap (XOR each reverted UTXO back in), but it must be wired correctly.
- Does the summary surface live mempool deltas? Recommendation: no. The summary is a confirmed-state view; mempool views live elsewhere.

### 2. Spending-side transparent address history

**Goal.** Make `WalletQuery.TransparentAddressTxIdsInRange` return every transaction touching an address, including spends from the address (not only outputs to it).

**Status today.** M4 Slice B shipped the `TransparentAddressTxIndex` artifact family (nibble `0x3`, capability `wallet.address.transparent_history_v1`), but the artifact builder indexes outputs only. The memory bank's M4 Slice B note records this explicitly as "Output-side indexing only; spending-side history is a known follow-up". Zaino's `getaddresstxids` passthrough to Zebra gives both sides because Zebra's address index does. To be a clean replacement, Zinder must close this gap.

**Proposed shape.**

- Extend the artifact builder for `TransparentAddressTxIndex` so each transparent **input** also produces an index entry. The script-hash key comes from the prevout's `transparent_output.address_script_hash`, which is already in canonical storage.
- One open design point: whether to write two separate rows per address-spending tx (one per spending input) or one row keyed only by `(address_script_hash, height, tx_index)` deduplicated. Recommendation: one row per address-tx pair (dedup), matching the output-side shape; the multiplicity per tx is a separate concern.
- No new capability needed; bump the artifact family version and the existing capability instead. Operators get the richer behaviour after they replay derive (no canonical rebuild needed if the spending-side index is a derive-plane projection, which it should be).

**Important re-question.** The output-side index landed on the canonical writer in M4 Slice B. The spending side may be a better fit for the derive plane because it is a projection over `transparent_spend_fact` plus `transparent_output.address_script_hash`. Splitting "produced-side on writer, spent-side on derive" creates a vocabulary mismatch ("which one answers `TransparentAddressTxIdsInRange`?"). The ADR for this slice should explicitly pick one home, not silently grow a second one.

**Effort sizing.** Smaller than §1 if the spending-side index moves to the derive plane (one new derive consumer, one builder change, one capability bump). Larger if it stays on the writer (a canonical artifact change plus a one-shot backfill).

### 3. CompactTx.vin/vout population in the lightwalletd-compat builder

**Goal.** Confirm that mobile wallets sitting on `zinder-compat-lightwalletd` see transparent transfers via the v0.4.0 fields. The proto already speaks the schema; the question is whether the builder fills the fields.

**Status today.** The vendored proto schema includes `CompactTx.vin = 7` (repeated `CompactTxIn`) and `CompactTx.vout = 8` (repeated `TxOut`) at [`crates/zinder-proto/proto/compat/lightwalletd/compact_formats.proto:70-89`](../../crates/zinder-proto/proto/compat/lightwalletd/compact_formats.proto). The pinned upstream commit is `dd0ea2c3c5827a433e62c2f936b89efa2dec5a9a`. The unanswered question is whether the compact-block builder in `zinder-ingest` (or wherever the `CompactBlockArtifact` builder lives) populates these fields from `transparent_output` and `transparent_spend_fact`, or leaves them empty.

**Proposed audit.**

- Grep for the `compact_block` builder and confirm whether it reads from `transparent_output` and `transparent_spend_fact` when assembling each transaction's compact form.
- If unpopulated, fill them in the same builder that emits the existing shielded fields.
- Add a parity test against the reference `lightwalletd-go` so we catch any future schema drift; the test belongs alongside the existing `live::parity_against_lightwalletd::` suite (referenced in [`CLAUDE.md`](../../CLAUDE.md)).

**Effort sizing.** If the fields are already populated: zero work, one parity test, one closing note. If unpopulated: one builder change, one parity test, one rebuild path for stored compact blocks (or accept "new blocks only from this version forward").

## Already covered (or done better) in Zinder

The following Zaino 0.3.1 surfaces are matched or exceeded today. No action needed beyond doc keeping.

- **Spent-outpoint index as core data:** `transparent_spend_fact` is already in the canonical hot tables list in [`fact-first-indexer.md`](../architecture/fact-first-indexer.md).
- **Local transparent-address indexing:** `address_output_index` table; artifacts `TransparentUtxoStreamFamily` (nibble `0x4`) and `TransparentAddressTxIndex` (nibble `0x3`); RPCs `WalletQuery.AddressOutputIndexStream`, `WalletQuery.TransparentAddressTxIdsInRange`, `WalletQuery.TransparentAddressBalance`, `WalletQuery.TransparentPrevouts`, `WalletQuery.TransparentMempoolPrevouts`, `ExplorerQuery.TransparentAddressActivity`. Zaino still routes the equivalent `ChainIndex` methods through `validator_connector` to Zebra ([`packages/zaino-state/src/chain_index.rs:2134-2174`](../../../zaino/packages/zaino-state/src/chain_index.rs)).
- **Atomic reorg discipline at read time:** every read pins to a `ChainEpoch`; `commit_ingest_batch` is the only transition that makes a new epoch visible. See [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) and [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md).
- **Mempool surfaces:** `WalletQuery.MempoolSnapshot`, `WalletQuery.MempoolEvents`, and `WalletQuery.TransparentMempoolPrevouts`. Zaino's non-finalised-state model overlaps in intent; Zinder's is more conservative about consistency.
- **Capability negotiation:** per-feature capability strings federated across source, ingest, derive, and query layers; see [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) and [ADR-0018](../adrs/0018-capability-gated-optional-payload-fields.md).
- **`LightdInfo.version` reports binary version:** [`services/zinder-compat-lightwalletd/src/grpc.rs:1144`](../../services/zinder-compat-lightwalletd/src/grpc.rs) already uses `env!("CARGO_PKG_VERSION")`; `zcashd_build` and `zcashd_subversion` are deliberately empty because Zinder is not zcashd.
- **lightclient-protocol v0.4.0 schema:** present in the vendored proto pin.
- **`core2` yanked-crate workaround:** Zinder's workspace `Cargo.toml` patches `core2` to a git source, allowed via cargo-deny.

## Explicitly not inheriting

Listed with reasoning so future contributors do not re-litigate.

- **`z_validateaddress` passthrough.** zcashd-shaped RPC kept by Zaino "pre-deprecated for bugwards compatibility". New clients use the `zcash_address` crate client-side; legacy lightwalletd clients never called this RPC. Adding it would create a third API shape (zcashd-style) alongside the native `WalletQuery` and the lightwalletd compat plane. Cognitive cost for zero new capability.
- **Non-finalised state visible to readers.** Zaino's `NonFinalizedState` ([`packages/zaino-state/src/chain_index/non_finalised_state.rs`](../../../zaino/packages/zaino-state/src/chain_index/non_finalised_state.rs)) exposes Zebra's non-finalised tip to query handlers. Zinder pins each read to a committed `ChainEpoch`; the live tip is observed via mempool and chain-event subscriptions instead. Adopting Zaino's shape would weaken the core consistency property the rest of Zinder is built on.
- **zcashd-shaped address RPCs (`getaddressdeltas`, `getaddressbalance`, `getaddresstxids`, `getaddressutxos`) as RPC names.** Zinder's data is richer; the surface is `WalletQuery.*` (native) and `CompactTxStreamer.*` (lightwalletd compat). Nothing in the live ecosystem calls the zcashd-shaped names without going through one of those two paths.
- **`ChainIndex` as an embeddable in-process library link.** Zaino's `zaino-state` is consumed in-process by Zallet. Zinder's contract is gRPC plus `zinder-client`'s `ChainIndex` trait with `LocalChainIndex` (RocksDB-secondary) and `RemoteChainIndex` (gRPC). The seam stays at the storage and protocol boundary, not at a shared library link. See [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md).

## Follow-up audits

Smaller items that need verification, not necessarily change.

- **Resumable migration semantics.** Zaino calls out resumable in-place migration of the finalised-state DB. Zinder's writer-owned migration ownership ([ADR-0003](../adrs/0003-canonical-storage-access-boundary.md)) combined with phase-driven ingest ([ADR-0015](../adrs/0015-unified-phase-driven-ingest.md)) almost certainly already supports restart mid-migration, but the resumability is not stated explicitly in [`storage-backend.md`](../architecture/storage-backend.md). One paragraph would close this.
- **Regtest tree-state leniency.** Zaino made `GetTreestateResponse.sapling` and `.orchard` optional so regtest can omit them before activation. Zinder returns tree state as bytes, so the schema is already lenient, but a focused regtest test for "Sapling activation at height N, tree-state response at heights N-1 and N+1" would confirm.
- **CompactTx.vin/vout population.** See [§3](#3-compacttx-vin-vout-population-in-the-lightwalletd-compat-builder).

## References

### Zinder

- [Service boundaries](../architecture/service-boundaries.md)
- [Fact-first indexer](../architecture/fact-first-indexer.md)
- [Indexer / wallet boundary](../architecture/indexer-wallet-boundary.md)
- [Derive plane](../architecture/derive-plane.md)
- [Explorer plane](../architecture/explorer-plane.md)
- [Public interfaces](../architecture/public-interfaces.md)
- [ADR-0003: Canonical storage access boundary](../adrs/0003-canonical-storage-access-boundary.md)
- [ADR-0005: Consumer-neutral wallet data plane](../adrs/0005-consumer-neutral-wallet-data-plane.md)
- [ADR-0009: Explorer plane as first-class product surface](../adrs/0009-explorer-plane-as-product-surface.md)
- [ADR-0015: Unified phase-driven ingest](../adrs/0015-unified-phase-driven-ingest.md)
- [ADR-0017: Derive-consumer template and key-codec convention](../adrs/0017-derive-consumer-template-and-key-codec-convention.md)
- [ADR-0018: Capability-gated optional payload fields](../adrs/0018-capability-gated-optional-payload-fields.md)

### Zaino (compared release)

- [`zaino/CHANGELOG.md`](../../../zaino/CHANGELOG.md)
- [`zaino/packages/zaino-state/src/chain_index/types/db/metadata.rs`](../../../zaino/packages/zaino-state/src/chain_index/types/db/metadata.rs) (UTXO-set accumulator)
- [`zaino/packages/zaino-state/src/chain_index.rs`](../../../zaino/packages/zaino-state/src/chain_index.rs) (transparent-address methods, lines 2134 onward)
- [`zaino/packages/zaino-state/src/chain_index/source.rs`](../../../zaino/packages/zaino-state/src/chain_index/source.rs) (`BlockchainSource` trait)
- [`zaino/packages/zaino-state/src/chain_index/non_finalised_state.rs`](../../../zaino/packages/zaino-state/src/chain_index/non_finalised_state.rs)
- [Zaino release announcements forum thread](https://forum.zcashcommunity.com/t/zainod-release-announcements/55845)
