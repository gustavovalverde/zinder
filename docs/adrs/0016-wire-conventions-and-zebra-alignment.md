# ADR-0016: Centralized Wire Conventions and Zebra-Aligned Vocabulary

| Field | Value |
| ----- | ----- |
| Status | Accepted (2026-05-12) |
| Product | Zinder |
| Domain | Native to wire dialect translations across services |
| Related | [Public interfaces](../architecture/public-interfaces.md), [Service boundaries](../architecture/service-boundaries.md), [Lessons from Zaino](../reference/lessons-from-zaino.md), [ADR-0002](0002-boundary-specific-serialization.md), [ADR-0008](0008-consumer-neutral-wallet-data-plane.md), [ADR-0015](0015-network-parameter-discovery.md) |

## Context

Zinder talks to wallets through three protobuf surfaces and ingests from one upstream JSON-RPC: `zinder.v1.wallet` (native gRPC), `zinder.v1.explorer` (native gRPC, served by `zinder-derive`), `cash.z.wallet.sdk.rpc` (vendored lightwalletd, served by `zinder-compat-lightwalletd`), and Zebra's `getblockchaininfo` family on the ingest side. Each surface uses one of two byte conventions for the same domain types:

- **Internal little-endian bytes** for protobuf `bytes` fields carrying transaction ids and block hashes. Lightwalletd-go documents this explicitly at `frontend/service.go:792`: "When expressed as bytes, a txid must be little-endian." Zinder's native `bytes` fields follow the same convention.
- **Display big-endian hex strings** for JSON-RPC replies (`getrawtransaction`, `getblock`, `getblockchaininfo`), for log records that quote ids to humans, and for any error message that surfaces a txid. The byte order is the reverse of the internal form.

The same shape applies to two other dialect-sensitive identifier translations:

- `Network` to wire-string conversion: lightwalletd's `LightdInfo.chainName` and `TreeState.network` use the BIP70 names `"main"` and `"test"` (with regtest collapsed into `"test"`); Zinder-native fields use `"zcash-mainnet"`, `"zcash-testnet"`, `"zcash-regtest"`.
- Consensus branch id rendering: `LightdInfo.consensusBranchId` and the `MinedDetails.consensus_branch_id` projection both want a lowercase 8-character hex string.

Before this ADR, those translations lived inline at every call site. A 2026-05-12 parity run against `electriccoinco/lightwalletd:latest` surfaced two production bugs traced to that scattering:

1. `GetLightdInfo.chainName` returned `"regtest"` because the compat shim's private `lightwalletd_chain_name` helper had drifted from the BIP70 convention. The same drift escaped CI because `services/zinder-ingest/tests/common/mod.rs:617` held a *duplicate* `lightwalletd_chain_name` that returned `"test"` correctly; tests passed while production lied.
2. `GetTransaction(hash)` returned `NotFound` for coinbase txids round-tripped through `GetAddressUtxos`. The cause was an inverted reversal in the compat shim's `transaction_id_from_lightwalletd_hash` helper: it reversed bytes on input that were already in internal order. Inline `as_bytes()` calls at five other wire-boundary sites had each independently picked one of three contradictory docstring claims about what byte order was expected.

The pattern question is not "what is the byte order for txid bytes." It is "where in the architecture does a Zinder process express the wire convention, and how is the same expression enforced as new wire fields and new dialects are added." Without one answer, every new method risks rediscovering one of the same three bug classes:

- Chain-name dialect drift between BIP70 and Zinder-native.
- Byte-order reversal applied twice or skipped at one of several scattered sites.
- Capability literal duplicated outside the `ZINDER_CAPABILITIES` source of truth.

## Decision

Native to wire identifier translations live in `crates/zinder-core/src/wire/` and only there. Files are organized by *concept*, not by *dialect*: `transaction_id.rs` holds every transaction-id conversion across every dialect; `block_hash.rs` holds every block-hash conversion; `chain_name.rs` holds every `Network` to wire-string conversion; `branch_id.rs` holds every consensus-branch-id conversion. Adding a new dialect to a shipped concept extends the existing file; adding a new concept adds a new file.

Proto-enum mappings that require tonic-generated types (for example `ShieldedProtocol` between `zinder.v1.wallet` and `cash.z.wallet.sdk.rpc`) stay co-located with their adapters until a second caller appears for one mapping; the wire module's promise is *one canonical conversion per direction*, not *every conversion routed through a shared file*. A shared `zinder_proto::wire` was tried and removed: it had a single submodule with zero callers, because each adapter wanted a different error shape.

### Verb vocabulary

The two operations are `encode_*` (native domain value to wire bytes or string) and `decode_*` (wire bytes or string back to native domain value). Encode is infallible when the input type guarantees the output shape; decode returns `WireDecodeError`. No `to_*`, `from_*`, `parse_*`, `serialize_*`, or `deserialize_*` synonyms; one verb pair per operation prevents grep ambiguity.

### Concept-level naming

Function names describe the wire encoding, not the consumer dialect. Display-order conversions are `encode_display_*_hex` and `decode_display_*_hex`; internal-order conversions are `encode_internal_*` and `decode_internal_*`. Lightwalletd, Zebra JSON-RPC, and any future ingress dialect all source from the same primitives.

### Dialect-prefixed naming where the dialect differs

`Network` to wire-string conversions carry the dialect because the BIP70 and Zinder-native names are not interchangeable. `bip70_chain_name(Network) -> &'static str` returns `"main"` or `"test"`; `zinder_native_chain_name(Network) -> &'static str` returns the `zcash-*` form. The doc comment on each function lists every consumer so a contributor searching by surface lands on the right answer.

### Capability strings

Every advertised capability is a `pub const FOO_V1: &str = "...";` in `crates/zinder-proto/src/capabilities.rs`, gathered into `ZINDER_CAPABILITIES`. Call sites import the constant; no source file outside that one may carry the literal string. The drift the structural test caught at landing time (five duplicates) is the kind that returns the moment the rule lapses.

### Patterns explicitly forbidden

The following inline forms are forbidden anywhere outside the wire modules:

- `transaction_id.as_bytes()` and `block_hash.as_bytes()` at a wire boundary. Use `encode_internal_transaction_id` or `encode_internal_block_hash` so the convention is grep-discoverable.
- `format!("{:08x}", branch_id)` for wire output. Use `encode_branch_id_hex`.
- Inline hex-string txid or block-hash decode. Use `decode_display_*_hex`.
- Hardcoded capability literals. Use the `pub const` from `capabilities.rs`.
- Duplicate `Network` to wire-string tables. Use `encode_bip70_chain_name` or `encode_zinder_native_chain_name`.

### Structural enforcement

Two integration tests guard the rules and run on every CI invocation:

- `crates/zinder-core/tests/integration/wire_invariants.rs` walks the `crates/` and `services/` trees and fails when a banned pattern appears outside its allow-listed home (currently `wire/branch_id.rs`).
- `crates/zinder-proto/tests/integration/capability_string_uniqueness.rs` walks the same trees and fails when any value in `ZINDER_CAPABILITIES` appears as a string literal outside `crates/zinder-proto/src/capabilities.rs`.

Both helpers strip Rust line comments before searching, so doc examples that quote the banned form for explanatory reasons do not trip the guard. Block comments and string literals embedded in source code do trip the guard, which is the intended behavior.

### Error type

Decode failures return `zinder_core::wire::WireDecodeError`. Variants cover the failure modes a caller can act on: `InvalidLength { expected, actual }`, `InvalidHex { reason }`, `UnrecognizedEnumDiscriminant { dialect, discriminant }`, `UnrecognizedString { dialect, input }`. The enum is `#[non_exhaustive]` so adding a variant is a non-breaking refinement.

### Sharing across crates

`zinder_core::wire` has no protobuf dependency, so `zinder-source`, `zinder-store`, `zinder-runtime`, and every service crate can import it without circular deps. Conversions that need tonic-generated types (proto-enum mappings such as `ShieldedProtocol`) live next to their adapters; lifting them into a shared module only pays off when more than one caller agrees on the error shape.

## Alternatives considered

### Match Zebra's two-layer `Network` and `NetworkKind` type system

Zebra distinguishes `Network` (Mainnet, Testnet) from `NetworkKind` (Mainnet, Testnet, Regtest), with a `NetworkKind::bip70_network_name()` method on the latter. The encoding convention is implicit in the type system.

Zinder uses three explicit variants (`ZcashMainnet`, `ZcashTestnet`, `ZcashRegtest`) and function-based wire helpers. The choice is deliberate: Zinder's three-variant `Network` is grep-friendly (every regtest-specific decision is one grep away), easier to extend with a new network (one variant, one match arm per helper, vs. coordinating two enums), and produces error messages that name the concrete network rather than its kind. A future refactor could fold in a structural `NetworkBip70Tag` enum if a fourth wire dialect with a different network grouping appears; the wire module is the one place that change would land.

### A single `Network::name()` method that returns the right string per call site

Rejected. The method name does not disclose which name (config? wire? display?). The signature would have to encode the dialect as a parameter, which is exactly the function-call form we already have. The user-instruction "no users, no baggage" applies: a delegating shim would coexist with the explicit form indefinitely, and contributors would inconsistently pick between them.

### Inline conversions with code-review enforcement

Rejected. The 2026-05-12 parity incident is the empirical evidence: five sites with three contradictory docstrings shipped through review. Structural tests cost two files and pass on every CI invocation; reviewer attention is finite and the bug class returns the moment a new dialect lands.

### Generic-named module (`utils`, `helpers`, `common`)

Forbidden by `CLAUDE.md`. The module name is `wire` because the bounded context is "the wire-boundary translation surface"; the name predicts the content.

## Consequences

- Adding a new wire field starts with locating or adding a function in `crates/zinder-core/src/wire/`. The forbidden-patterns list above is the checklist.
- Adding a new wire dialect (a hypothetical `zcashd-compat` JSON-RPC ingress, a future streaming RPC) extends an existing concept file rather than forking a dialect file. The display-byte-reversal convention is in front of every contributor adding such a surface.
- Adding or retiring a capability is a one-line change in `crates/zinder-proto/src/capabilities.rs`; every caller imports the constant, and the doc-mirror test (`capability_docs.rs`) plus the source-tree uniqueness test (`capability_string_uniqueness.rs`) keep the rest of the workspace consistent.
- Renaming a wire-conversion helper is a workspace-wide rename through `cargo` and `rust-analyzer`; no string-literal duplicates lurk in tests, docs, or generated modules.
- The structural tests fire on `cargo nextest run --profile=ci`. A regression that reintroduces an inline conversion fails the default validation gate before reaching review.

## Forward compatibility

- A future Shape A landing for any compute-at-read-time RPC ([ADR-0014](0014-compute-at-read-time-canonical-reads.md)) does not interact with this ADR: storage shape and wire shape are independent.
- A new wire dialect adds entries to the existing concept files, never a new module structure.
- A future `NetworkBip70Tag` enum (if a fourth dialect groups networks differently) lands inside `wire/chain_name.rs`; callers continue to call `encode_bip70_chain_name` and `encode_zinder_native_chain_name` and observe no signature change.
