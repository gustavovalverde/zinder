# ADR-0024: Wire Format Uses RPC Byte Order for Hashes

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Wire format, hash serialization, public proto contract |
| Related | [ADR-0002](0002-boundary-specific-serialization.md), [ADR-0005](0005-consumer-neutral-wallet-data-plane.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0010](0010-transaction-public-facts.md), [ADR-0011](0011-explorer-freshness-envelope.md), [Public interfaces §Wire Conventions](../architecture/public-interfaces.md#wire-conventions), [Explorer plane §Block view shape](../architecture/explorer-plane.md#block-view-shape) |

## Context

A Zcash 32-byte hash exists in two byte orders. The first is the raw SHA-256d output, used in consensus serialization (`hashPrevBlock`, `hashMerkleRoot`) and stored verbatim in `[u8; 32]`. The Zcash protocol specification calls this **internal byte order** (`protocol.tex:13560-13564`). The second is the byte-reversed form, used by every consumer-facing surface: `zcash-cli`, every wallet UI, every block explorer URL, and the protocol spec itself when it prints block hashes in its text. The spec defines this as **RPC byte order** at `protocol.tex:1127` (`\newcommand{\rpcByteOrder}{\term{RPC byte order}}`) and applies it in normative sentences such as `protocol.tex:4036`: *"All block hashes given in this section are in RPC byte order (that is, byte-reversed relative to the normal order for a SHA-256d hash)."* ZIP 308 uses the same term for txid presentation.

Until this change, Zinder's public proto contract carried every hash-shaped field (`transaction_id`, `block_hash`, `previous_block_hash`, `merkle_root_hash`, `wtxid`, `auth_digest`, `tip_hash`, etc.) as `bytes` in internal byte order. Storage in RocksDB also held internal byte order, which kept storage and wire trivially consistent but pushed the byte-reversal responsibility onto every downstream consumer.

The cost surfaced as a Zexplorer regression. A user copied a canonical txid from a wallet (the on-chain testnet txid `c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436` in block 4031230, exactly as Zebra's `getblock` returns it) and got HTTP 404 from Zexplorer. The BFF forwarded the user's display-form hex into the proto `bytes` field; Zinder looked it up under the wrong key; the lookup missed. Patching Zexplorer alone would have left the same trap for every future consumer of Zinder. Zinder is the upstream of an ecosystem that, by spec and by every existing tool, speaks RPC byte order at the human-and-RPC boundary.

The fix that scales is to align the proto contract with the spec: every hash field on the wire is RPC byte order, conveyed as a hex `string`. Storage keeps internal byte order. The boundary between the two lives in one well-named place.

## Decision

### Wire shape

Every hash-shaped field in Zinder's public proto contract carries RPC byte order hex as a `string`. The lengths are:

- 64 lowercase ASCII hex characters for a 32-byte hash (`transaction_id`, `block_hash`, `previous_block_hash`, `merkle_root_hash`, `auth_digest`, `tip_hash`, `mined_block_hash`, `completing_block_hash`, `spending_transaction_id`).
- 128 lowercase ASCII hex characters for `wtxid`, which per ZIP 239 is the concatenation `txid || auth_digest`. The RPC form concatenates the RPC-form txid (64 chars) and the RPC-form auth_digest (64 chars); each half is reversed independently because each half is its own hash.

Every changed field's comment cites `\rpcByteOrder` so the spec reference travels with the contract.

### Storage shape

RocksDB keys and stored artifacts keep internal byte order. The newtypes `TransactionId([u8; 32])`, `BlockHash([u8; 32])`, `AuthDigest([u8; 32])`, `Wtxid([u8; 64])`, `MerkleRoot([u8; 32])` in `zinder-core` are the storage-side representation; their `as_bytes()` accessors return internal-form bytes that storage paths use directly. Storage-side comments cite "internal byte order" with a reference to the protocol spec at `protocol.tex:13560-13564`.

### Translation seam

`crates/zinder-core/src/wire/` is the only place where the two forms meet:

- `encode_internal_<thing>(value) -> [u8; 32]` and `decode_internal_<thing>(bytes) -> <Thing>`: identity-shaped wrappers used by storage paths.
- `encode_rpc_<thing>_hex(value) -> String` and `decode_rpc_<thing>_hex(input: &str) -> <Thing>`: the wire-facing pair. `encode_rpc_*_hex` reverses then hex-encodes. `decode_rpc_*_hex` hex-decodes then reverses; bad inputs surface as `WireDecodeError`.

The renamed helpers (previously `encode_display_*_hex` / `decode_display_*_hex`) adopt the spec's vocabulary so the function name documents the form it produces or accepts. New modules `wire/auth_digest.rs`, `wire/wtxid.rs`, and `wire/merkle_root.rs` provide the same pair for the three hash kinds that previously lacked dedicated wire helpers.

### Search input

`crates/zinder-core/src/explorer_search.rs` accepts user-supplied hex search input in RPC byte order. The classifier calls `decode_rpc_transaction_id_hex` and `decode_rpc_block_hash_hex` and hands the resulting internal-form `TransactionId` / `BlockHash` to the storage probe. Before this change, the classifier decoded hex literally without reversal, so users typing the canonical form missed the storage key.

### lightwalletd-compat exemption

The proto under `crates/zinder-proto/proto/compat/lightwalletd/` is frozen by lightwalletd's upstream contract: every hash field there is `bytes` in protocol (internal) order with an explicit `// MUST NOT be reversed` comment. Those fields are unchanged. The `services/zinder-compat-lightwalletd` adapter still consumes the renamed wire helpers (`encode_rpc_*_hex` is the right call where lightwalletd asks for display-form strings, such as `lightwalletd::TreeState.hash` and `lightwalletd::SendResponse.error_message`).

## Consequences

- **Breaking proto change.** Every consumer regenerates from the new proto and drops its own byte-reversal logic. Field numbers are unchanged; only the wire type flips from `bytes` to `string`. With all consumers in alpha, the change ships in one cut without a parallel-fields deprecation cycle.
- **Stored proto messages flip with the wire.** Several stored artifacts in the chain store (`BlockHeaderInfo`, `TransactionLocation`, `OutPoint`, mempool/transparent-address entries) and every derive payload (`BlockSummaryRecord`, `RecentTransactionEntry`, etc.) contain the same hash-shaped fields. Their on-disk encoding changes with the proto. `CURRENT_ARTIFACT_SCHEMA_VERSION` bumps from 9 to 10 and refuses to open a store written under the older version; each affected derive consumer bumps its own schema version and rebuilds its projection per [ADR-0028](0028-per-consumer-derive-schema-versioning.md). Existing deployments must wipe and re-sync the canonical store directory before starting the new binary.
- **One vocabulary.** "RPC byte order" and "internal byte order" are the only terms used in proto comments, code comments, ADRs, and the public-interfaces glossary. Both are direct references to the protocol specification. Folk terms ("display form", "big-endian hash", "user-facing hex") do not appear in the codebase.
- **Wire size grows ~2x for hash fields.** A `BlockDetail` response with 4,000 txids goes from ~128 KB to ~256 KB of payload. This is negligible over HTTP/2 with gzip and irrelevant for the explorer plane. The lightwalletd-compat plane streams compact blocks at scale and keeps the `bytes` form precisely because its consumer mix differs.
- **One translation site.** The `wire/` module owns the byte-reversal. Adapters call `encode_rpc_*_hex` on outbound and `decode_rpc_*_hex` on inbound; the existing `wire_invariants` integration test forbids inline `.as_bytes()` or `hex::encode` on hash material outside this module.
- **Hash newtypes for every wire form.** `AuthDigest`, `Wtxid`, and `MerkleRoot` join `TransactionId` and `BlockHash` as first-class storage types. The "bare `[u8; 32]`" anti-pattern is gone from the public surface.

## References

- Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, defining sentence at :4036)
- Zcash protocol spec, "internal byte order" on `hashPrevBlock` and `hashMerkleRoot` (protocol.tex:13560-13564)
- [ZIP 239](https://zips.z.cash/zip-0239) (wtxid definition)
- [ZIP 244](https://zips.z.cash/zip-0244) (txid_digest)
- [ZIP 308](https://zips.z.cash/zip-0308) (uses "RPC byte order" for txid presentation)
