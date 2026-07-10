# ZIP-311 Payment Disclosure Verifier

Status: Draft
Date: 2026-05-26
Owner: ZFND
Reference consumer: zpay (Zcash-native payments facilitator), zexplorer (public `payment-disclosures/verify` endpoint)

## Problem Statement

ZIP-311 defines a payment-disclosure format that lets a sender prove they paid a specific amount to a specific recipient inside a specific transaction, without revealing any other notes in that transaction or any other Zcash activity. zinder already advertises the capability `explorer.payment_disclosure.verify_v1` and exposes the typed RPC `ExplorerQuery.VerifyPaymentDisclosure`. The server-side verifier is not bundled.

The capability is operator-gated off by default, and the handler returns `Status::failed_precondition` when enabled. Every downstream consumer that wants proof-of-payment for shielded transactions has to ship its own ZIP-311 verifier, duplicating the same cryptographic code across the ecosystem and creating multiple places where the cryptography can be subtly wrong.

This document is the implementation plan that lands the verifier inside zinder so every consumer can rely on one validated implementation.

## Source References

- [ZIP-311 Payment Disclosures](https://zips.z.cash/zip-0311)
- [ZIP-32 HD Hierarchy](https://zips.z.cash/zip-0032)
- [ZIP-204 Transparent Disclosures](https://zips.z.cash/zip-0204) (referenced by ZIP-311)
- `zcash_protocol::consensus` for network parameters and activation tables
- `zcash_primitives::transaction::Transaction` for v5 transaction parsing
- `sapling-crypto` for note ciphertext decryption
- `crates/zinder-store` for indexed transaction lookup

## Product Positioning

The zinder ZIP-311 verifier is the canonical Zcash ecosystem implementation. It owns:

- The byte-format parser for ZIP-311 disclosure payloads.
- The cryptographic verification for both transparent (ZIP-204) and Sapling output disclosures.
- The on-chain cross-check: confirming the disclosed transaction is mined in the indexed state, with the disclosed transaction id matching the one the verifier derives from the parsed payload.
- The typed verdict surface (`VALID`, `INVALID_SIGNATURE`, `TRANSACTION_NOT_FOUND`, `MALFORMED`).

It does **not** own:

- Decoding of viewing keys. Disclosures are self-contained; the verifier requires no viewing key from the operator.
- Trial-decryption of other notes in the transaction. The disclosure carries the ephemeral key for the disclosed output; only that output is decrypted.
- Persistence of past verifications. Verification is a pure function of the indexed chain state plus the supplied disclosure bytes.

## ZIP-311 Recap

A disclosure proves that one specific output in a specific transaction paid a specific value to a specific recipient. The disclosure contains:

1. A 4-byte protocol version tag (`0x00 0x00 0x00 0x01` for the current ZIP-311 version).
2. The 32-byte transaction id (txid) of the transaction containing the disclosed output.
3. The 32-byte payment id (an opaque identifier the sender chose to label this disclosure).
4. The 8-byte disclosed value (little-endian zatoshis).
5. An output-kind discriminator (1 byte: 0x00 transparent, 0x01 Sapling).
6. The output index inside the transaction (4 bytes, little-endian u32).
7. A proof block whose shape depends on the output kind:
   - **Transparent**: a BIP-340 Schnorr signature over the disclosure metadata, using the recipient's transparent public key.
   - **Sapling**: the ephemeral key (32 bytes), the diversifier (11 bytes), and the recipient's incoming viewing key tag (4 bytes) sufficient to re-derive the note ciphertext and confirm the disclosed value matches.

Cryptographic correctness is the entire point. A verifier that confirms the structure but not the proof is worse than no verifier, because consumers treat `VALID` as "the payment is proven" and the verifier becomes the trust root.

## Architecture Requirements

Single new module `services/zinder-explorer/src/payment_disclosure/` containing:

- `mod.rs`: exported `verify_payment_disclosure(disclosure_bytes: &[u8], chain_lookup: &dyn ChainLookup) -> VerdictWithFacts`. No other public surface.
- `parse.rs`: pure byte parser, no I/O. Maps malformed inputs to `Verdict::Malformed` with a typed reason kept internal (never echoed to the wire).
- `transparent.rs`: BIP-340 Schnorr verification for transparent disclosures. Depends on `secp256k1` already in the workspace.
- `sapling.rs`: Sapling note ciphertext re-derivation using disclosed `esk` + `epk`. Depends on `sapling-crypto`.
- `chain_lookup.rs`: typed trait the verifier calls with a txid; returns the indexed transaction's raw bytes plus its mined location. The implementation reuses `DeriveStore::transaction_bytes_by_id` (already used by `TransactionDetail`).
- `tests/`: parser conformance vectors, transparent-disclosure round trips, Sapling-disclosure round trips, on-chain cross-check rejections.

Verifier outputs:

```rust
pub enum Verdict {
    Valid {
        transaction_id: [u8; 32],
        payment_id: [u8; 32],
        disclosed_value_zat: u64,
    },
    InvalidSignature,
    TransactionNotFound,
    Malformed,
}
```

The handler in `services/zinder-explorer/src/grpc/adapter.rs::verify_payment_disclosure` translates `Verdict::Valid` into `PaymentDisclosureVerdict::VALID` with the `PaymentDisclosurePublicFacts` echo. All other variants map to their matching proto enum value, with no echo.

## Capability Requirements By Surface

### R-PD-1. Parse disclosure bytes

Now: nothing.
Why it belongs in zinder: byte format and lookup live next to the indexed transactions.
Proposed change: implement the byte parser per the ZIP-311 layout above. Invalid version tag, truncated payload, or oversize input maps to `Verdict::Malformed`. The parser holds no state, makes no allocations beyond the parsed struct, and never logs the input bytes.

### R-PD-2. Transparent disclosure proof

Now: nothing.
Why it belongs in zinder: BIP-340 Schnorr verification is a tiny well-defined operation; centralising it removes a per-consumer copy.
Proposed change: implement `verify_transparent` using `secp256k1::SECP256K1.verify_schnorr`. Public key is derived from the indexed transaction's matching output script. Disclosure rejects when the derived public key does not match the script's address.

### R-PD-3. Sapling disclosure proof

Now: nothing.
Why it belongs in zinder: Sapling note decryption requires `sapling-crypto`'s lower-level primitives that consumers should not reimplement.
Proposed change: use `sapling_crypto::note_encryption::try_sapling_compact_note_decryption_with_esk` with the disclosed `esk` and the on-chain `epk + cmu + enc_ciphertext`. Verify the decrypted note's value equals the disclosed value and the diversifier matches.

### R-PD-4. On-chain cross-check

Now: nothing.
Why it belongs in zinder: the indexed transaction store is the source of truth and already lives here.
Proposed change: look up the transaction by id via `DeriveStore::transaction_bytes_by_id`. Map missing transactions to `Verdict::TransactionNotFound`. Decode the v5 transaction with `zcash_primitives::transaction::Transaction::read`; map decode failures to `Verdict::TransactionNotFound` (the txid is indexed but the bytes are corrupt; consumer should retry).

### R-PD-5. Public facts echo on `VALID`

Now: protobuf exists; field never populated.
Why it belongs in zinder: the wire contract already specifies the echo shape.
Proposed change: when `verify_payment_disclosure` returns `Verdict::Valid`, populate `PaymentDisclosurePublicFacts` with the parsed `transaction_id`, `payment_id`, and `disclosed_value_zat`. The verifier never echoes the proof block or the ephemeral key.

### R-PD-6. Flip capability to enabled-by-default

Now: opt-in via operator config (`with_payment_disclosure_verifier_online`).
Why: zinder advertises the capability whenever the verifier is built. Operators who want to disable it can still toggle the flag off.
Proposed change: change the default to `true`, document the off-by-config escape hatch in the runbook.

## Privacy and Logging Requirements

- The handler at `services/zinder-explorer/src/grpc/adapter.rs::verify_payment_disclosure` never logs or spans `request.payment_disclosure_bytes`. This contract is already documented; the new code preserves it.
- The parser, verifier, and adapter never log the parsed `payment_id` outside the `VALID` echo path. A `payment_id` is an opaque sender-chosen tag that may carry application-specific meaning.
- The disclosed `esk` and `epk` are never logged. They appear only as parser inputs and immediately drop.
- Verdict mappings never carry the original disclosure bytes back to the gRPC response or to logs.

## Testing Decisions

- T0 unit: parser conformance against the ZIP-311 reference vectors (need to be sourced from the spec or generated from `zcash_primitives` test helpers).
- T0 unit: BIP-340 Schnorr verification against a hand-rolled fixture (a known keypair, a known message, a known signature).
- T0 unit: Sapling note re-encryption round trip using a `sapling-crypto` test fixture.
- T1 integration: a real testnet transaction's disclosure verifies `VALID`. Live test, opt-in via `ZINDER_TEST_LIVE=1`.
- T1 integration: a corrupted disclosure for an indexed transaction returns `INVALID_SIGNATURE`, not `MALFORMED` (we want the strongest verdict).

Mutation testing: required on `services/zinder-explorer/src/payment_disclosure/parse.rs` and `transparent.rs`. The verifier is a trust root; the existing `cargo mutants` workflow already targets critical files.

## Implementation Milestones

### M0: Scaffolding (this PR, no live capability change)

Land the module structure, the typed `Verdict` enum, the parser skeleton, and the trait `ChainLookup`. Replace the `failed_precondition` in the adapter with a call to the verifier that returns `Verdict::Malformed` for every input. The capability stays off by default; nothing observable changes for any caller.

### M1: Parser + on-chain cross-check (no cryptographic proof verification yet)

Implement R-PD-1 and R-PD-4. Invalid byte layouts map to `Verdict::Malformed`; unknown txids map to `Verdict::TransactionNotFound`. Every well-formed disclosure for an indexed txid still returns `Verdict::InvalidSignature` because the cryptographic step is stubbed. Capability stays off by default.

### M2: Transparent disclosures verifiable

Implement R-PD-2. Capability flips on by default for builds that include the verifier. Transparent disclosures now verify; Sapling disclosures still return `Verdict::InvalidSignature`.

### M3: Sapling disclosures verifiable

Implement R-PD-3. All ZIP-311-conformant disclosures now verify.

### M4: Public-facts echo + capability default flip

Implement R-PD-5 and R-PD-6. Update the runbook with the operator-disable escape hatch.

### M5: Live test parity

Add the testnet live tests. Document the test vectors in `docs/reference/zip311-test-vectors.md`.

## Acceptance Criteria

- All M0 deliverables compile and pass the existing zinder validation gate.
- The parser handles every ZIP-311 conformance vector with the right verdict.
- The cryptographic verification passes round-trip tests for both output kinds.
- A live testnet disclosure verifies as `VALID`.
- The capability default is `true` once M4 lands, with operator-disable documented.

## Open Questions

1. Where do the ZIP-311 conformance vectors live? The spec references "test vectors" but no public corpus is bundled with `zcash_primitives`. We may need to generate our own from a controlled `zcashd`/`zebrad` regtest run and commit them as fixtures.
2. The disclosure format historically supported Sprout outputs. Should the verifier handle Sprout, or reject with `Verdict::Malformed`? Decision pending; the proposal currently scopes to transparent + Sapling.
3. Does the public-facts echo need to include the output kind (transparent vs Sapling)? Useful for downstream auditing but reveals whether the disclosure was shielded. Defer to M4.

## Out of Scope

- Sprout disclosure support.
- Persisting verification history.
- Rate-limiting the verifier; that lives at the gRPC adapter / load balancer layer, not in the verifier itself.
- Generating disclosures. The verifier consumes disclosures; production-grade disclosure generation belongs to wallet software (zally, Zodl, Zallet).
