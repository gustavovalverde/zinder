//! ZIP-311 payment-disclosure verifier scaffolding.
//!
//! This module is the M0 scaffolding for the ZIP-311 verifier described in
//! [docs/prd/zip311-payment-disclosure-verifier.md][prd]. The byte parser,
//! cryptographic verification, and on-chain cross-check are tracked as M1-M3
//! in that document.
//!
//! Until M1 lands, every input maps to [`Verdict::Malformed`]. The capability
//! `explorer.payment_disclosure.verify_v1` stays operator-gated off by
//! default. The handler in
//! `services/zinder-explorer/src/grpc/adapter.rs::verify_payment_disclosure`
//! calls into [`verify`] only when an operator has explicitly opted in; the
//! current implementation returns a typed `Verdict::Malformed` for every
//! input so callers receive a stable wire shape instead of `Status::failed_precondition`.
//!
//! Privacy invariant (matches the gRPC adapter):
//!
//! - The verifier never logs or otherwise echoes the disclosure bytes, the
//!   parsed `payment_id`, or any ephemeral key material derived from the
//!   disclosure.
//! - Only the [`Verdict`] is returned. On a `Verdict::Valid` outcome the
//!   public facts echo (transaction id, payment id, disclosed value zatoshis)
//!   is the responsibility of the gRPC adapter, not this module.
//!
//! [prd]: https://github.com/gustavovalverde/zinder/blob/main/docs/prd/zip311-payment-disclosure-verifier.md

mod parse;

/// Verdict returned by [`verify`].
///
/// Maps one-to-one to the proto enum
/// `zinder.v1.explorer.PaymentDisclosureVerdict`. Translation lives in the
/// gRPC adapter, not here, so this module stays free of proto types.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum Verdict {
    /// The disclosure is valid and the named facts are echoed.
    ///
    /// Reserved for M4. The current scaffolding never returns this variant.
    #[allow(
        dead_code,
        reason = "Reserved for M3 (Sapling proof verification); the scaffolding only emits Malformed today."
    )]
    Valid {
        /// 32-byte transaction id from the disclosure.
        transaction_id: [u8; 32],
        /// 32-byte payment id from the disclosure.
        payment_id: [u8; 32],
        /// Disclosed value in zatoshis.
        disclosed_value_zat: u64,
    },
    /// The cryptographic proof inside the disclosure failed verification.
    ///
    /// Reserved for M2 (transparent) and M3 (Sapling). The scaffolding does
    /// not return this variant yet.
    #[allow(
        dead_code,
        reason = "Reserved for M2 (transparent Schnorr) and M3 (Sapling re-decrypt)."
    )]
    InvalidSignature,
    /// The disclosure references a transaction not present in the indexed state.
    ///
    /// Reserved for M1 (on-chain cross-check). The scaffolding does not
    /// return this variant yet.
    #[allow(dead_code, reason = "Reserved for M1 (chain lookup via DeriveStore).")]
    TransactionNotFound,
    /// The disclosure bytes did not parse, or parsed values fall outside the
    /// ZIP-311 layout (truncated payload, unknown protocol version tag,
    /// unknown output kind, oversize input).
    Malformed,
}

/// Lookup trait the verifier calls with a transaction id.
///
/// Implementations return the indexed raw transaction bytes plus the mined
/// location, or `None` when the transaction is not indexed. The trait stays
/// out of the verifier crate boundary so callers can plug in test
/// implementations without pulling the real [`DeriveStore`].
///
/// [`DeriveStore`]: ../../crates/zinder-store
#[allow(
    dead_code,
    reason = "Consumed once R-PD-4 lands; kept here so the wire shape is stable."
)]
pub(crate) trait ChainLookup {
    /// Returns the raw bytes of the transaction whose id is `transaction_id`,
    /// or `None` when the transaction is not present in the indexed state.
    ///
    /// Implementations must not log the transaction id; the verifier's
    /// privacy contract delegates this constraint to its callers.
    fn raw_transaction_bytes(&self, transaction_id: &[u8; 32]) -> Option<Vec<u8>>;
}

/// Verify a ZIP-311 payment disclosure.
///
/// Returns a [`Verdict`] without ever echoing the input bytes. The
/// scaffolding maps every input to [`Verdict::Malformed`]; M1-M3 fill in the
/// real parsing, cross-check, and cryptographic verification per the PRD.
///
/// Callers must not pass the disclosure bytes to any logging or tracing
/// surface; the verifier preserves this contract on the way out, but the
/// caller is responsible for not leaking the bytes on the way in.
#[allow(
    dead_code,
    reason = "Wired into the gRPC adapter once the M0 PR lands."
)]
pub(crate) fn verify<L: ChainLookup + ?Sized>(
    disclosure_bytes: &[u8],
    _chain_lookup: &L,
) -> Verdict {
    // M0 scaffolding always returns Malformed. The parser is called for its
    // side-effect-free type signature so a future change can swap the body
    // for `match parse_disclosure(...) { Ok(parsed) => verify_proof(...),
    // Err(_) => Verdict::Malformed }` without changing this module's
    // public shape. Both arms collapse to the same verdict until M1.
    let _ = parse::parse_disclosure(disclosure_bytes);
    Verdict::Malformed
}

#[cfg(test)]
mod tests {
    use super::{ChainLookup, Verdict, verify};

    struct AlwaysMissing;
    impl ChainLookup for AlwaysMissing {
        fn raw_transaction_bytes(&self, _transaction_id: &[u8; 32]) -> Option<Vec<u8>> {
            None
        }
    }

    #[test]
    fn empty_input_returns_malformed() {
        assert_eq!(verify(&[], &AlwaysMissing), Verdict::Malformed);
    }

    #[test]
    fn random_input_returns_malformed() {
        let scratch = [0xau8; 16];
        assert_eq!(verify(&scratch, &AlwaysMissing), Verdict::Malformed);
    }
}
