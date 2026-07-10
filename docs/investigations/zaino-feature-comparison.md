# Zaino feature comparison

> **Snapshot date:** 2026-07-10
> **Compared release:** zainod 0.3.1 (2026-05-25)
> **Sources:** Zaino [`CHANGELOG.md`](../../../zaino/CHANGELOG.md) "Unreleased" plus v0.2.0 entries; Zaino source tree at `../zaino`; release-announcement thread at <https://forum.zcashcommunity.com/t/zainod-release-announcements/55845>.

This document records a feature-by-feature comparison between Zinder and the Zaino 0.3.1 release. It distinguishes native semantic equivalents from raw zcashd-compatibility methods and records the remaining deliberate differences and follow-up audits.

This is a point-in-time technical audit, not a roadmap commitment.

## What this document is not

- Not a roadmap commitment. Follow-up audits still require their own decision.
- Not an ADR. Architectural changes belong in an ADR; this document records comparison results.
- Not a competitive-positioning document. The reading frame is technical inheritance, not market comparison.

## Findings at a glance

| Zaino feature (release 0.3.1) | Status in Zinder | Action |
| --- | --- | --- |
| `gettxoutsetinfo` served from local UTXO-set accumulator (XOR-of-BLAKE2b-256 multiset over the transparent UTXO set) | Native semantic equivalent: `WalletQuery.TransparentUtxoSetSummary` and `ExplorerQuery.UtxoSetSummary` return the settled-tip count, total value, and optional LTHash16 commitment from the canonical UTXO projection. They deliberately omit zcashd's serialized-set hash and byte size. | **Closed.** Keep the native surface; do not add raw zcashd RPC compatibility. |
| Spent-outpoint index promoted to core data (Zaino DB v1.2.0) | Already a canonical hot table: `transparent_spend_fact` in [`fact-first-indexer.md`](../architecture/fact-first-indexer.md). | None. Naming differs; semantics identical. |
| `ChainIndex` exposes `get_address_balance`, `get_address_deltas`, `get_address_txids`, `get_address_utxos`; Zaino's `validator_connector` routes them to Zebra's JSON-RPC | Zinder has local indexes, derived transparent-address history, and `ExplorerQuery.TransparentAddressDeltas`. The history consumer indexes both received outputs and resolved transparent spends. | **Closed.** Keep the native and lightwalletd surfaces rather than adding zcashd-shaped method names. |
| Non-finalised-state serviceability policy | Different model. Zinder pins reads to a committed `ChainEpoch`; live tip surfaces through `MempoolSnapshot` + `MempoolEvents`. | **Do not inherit.** See [§Explicitly not inheriting](#explicitly-not-inheriting). |
| Block lookups by hash or height | `BlockSelector::{Hash,Height}` shared by `WalletQuery` and `ExplorerQuery`. | None. |
| Subtree-root reporting | `WalletQuery.SubtreeRootsInRange`. | None. |
| `JsonRpSeeConnector::get_tree_state` returns optional sapling/orchard fields in regtest | Tree state is bytes-shaped via `WalletQuery.TreeState`; regtest behaviour driven by per-`Network` parameterization. | **Audit only.** See [§Follow-up audits](#follow-up-audits). |
| `z_validateaddress` passthrough ("shipped pre-deprecated for bugwards compatibility") | Address typing is client-side via the `zcash_address` crate. `ExplorerQuery.Search` already classifies typed candidates. | **Do not inherit.** See [§Explicitly not inheriting](#explicitly-not-inheriting). |
| `LightdInfo.version` reports running binary version | Already reports `env!("CARGO_PKG_VERSION")` in [`services/zinder-compat-lightwalletd/src/grpc.rs:1144`](../../services/zinder-compat-lightwalletd/src/grpc.rs). | None. |
| Adoption of lightwallet-protocol v0.5.0 (`CompactTxIn`, `TxOut`, `PoolType`, `BlockRange.poolTypes`, `CompactTx.vin/vout`) | Vendored proto pin is v0.5.0, and the compact-block builder emits both transparent `vin` and `vout`. | **Closed.** Fixture and lightwalletd compatibility tests cover the serialized shape. |
| In-place resumable finalised-state DB migration | Migration ownership belongs to the writer per [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md). Resumability semantics are not as prominently documented as Zaino's. | **Audit only.** See [§Follow-up audits](#follow-up-audits). |
| Dedicated public Docker Hub repo `zingodevops/zainod` with SHA-pinned image | `deploy/` ships local-build Docker compose plus per-network env files. | Release-engineering decision for ZFND; not in scope here. |
| Integration tests migrated to `corez` because `core2` is yanked | Workspace `Cargo.toml` patches `core2` to a git source, gated by cargo-deny `allow-git`. | None. Parallel fix, different vehicle. |

## Closed candidates

The three original inheritance candidates are now implemented or intentionally
covered by native surfaces:

- **Transparent UTXO-set summary:** `WalletQuery.TransparentUtxoSetSummary`
  performs a request-time scan of the canonical current-UTXO projection, and
  `ExplorerQuery.UtxoSetSummary` composes it into an explorer response. It
  reports count, total value, and an optional LTHash16 commitment. The missing
  zcashd serialized hash and byte size are deliberate raw-RPC differences.
- **Spending-side transparent history:** the derive consumer emits one
  address/transaction row for each received output and each resolved
  transparent spend. Retention remains an operator-facing serving-profile
  decision, not an indexing gap.
- **Transparent `CompactTx` fields:** the v0.5.0 compact builder populates
  `vin` from non-coinbase inputs and `vout` from all transparent outputs.
  The fixture and lightwalletd compatibility tests cover the serialized shape.

## Already covered (or done better) in Zinder

The following Zaino 0.3.1 surfaces are matched or exceeded today. No action needed beyond doc keeping.

- **Spent-outpoint index as core data:** `transparent_spend_fact` is already in the canonical hot tables list in [`fact-first-indexer.md`](../architecture/fact-first-indexer.md).
- **Local transparent-address indexing:** canonical `address_output_index` table for unspent outputs plus derive-owned transparent-address transaction history; RPCs `WalletQuery.AddressOutputIndexStream`, `WalletQuery.TransparentAddressTxIdsInRange`, `WalletQuery.TransparentAddressBalance`, `WalletQuery.TransparentPrevouts`, `WalletQuery.TransparentMempoolPrevouts`, `ExplorerQuery.TransparentAddressActivity`. Zaino still routes the equivalent `ChainIndex` methods through `validator_connector` to Zebra ([`packages/zaino-state/src/chain_index.rs:2134-2174`](../../../zaino/packages/zaino-state/src/chain_index.rs)).
- **Atomic reorg discipline at read time:** every read pins to a `ChainEpoch`; `commit_ingest_batch` is the only transition that makes a new epoch visible. See [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) and [ADR-0015](../adrs/0015-unified-phase-driven-ingest.md).
- **Mempool surfaces:** `WalletQuery.MempoolSnapshot`, `WalletQuery.MempoolEvents`, and `WalletQuery.TransparentMempoolPrevouts`. Zaino's non-finalised-state model overlaps in intent; Zinder's is more conservative about consistency.
- **Capability negotiation:** per-feature capability strings federated across source, ingest, derive, and query layers; see [ADR-0009](../adrs/0009-explorer-plane-as-product-surface.md) and [ADR-0018](../adrs/0018-capability-gated-optional-payload-fields.md).
- **`LightdInfo.version` reports binary version:** [`services/zinder-compat-lightwalletd/src/grpc.rs:1144`](../../services/zinder-compat-lightwalletd/src/grpc.rs) already uses `env!("CARGO_PKG_VERSION")`; `zcashd_build` and `zcashd_subversion` are deliberately empty because Zinder is not zcashd.
- **lightwallet-protocol v0.5.0 schema:** present in the vendored proto pin; the compact builder emits transparent `vin`/`vout`.
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
- **Regtest tree-state leniency.** The compatibility integration suite accepts absent pools and empty commitments. A live activation-boundary probe would strengthen deployment confidence, but no protocol or adapter gap remains.

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
