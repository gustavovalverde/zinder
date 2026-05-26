//! ZIP-311 disclosure byte-format parser.
//!
//! Stub today: every input returns [`ParseError::ProtocolVersionUnknown`].
//! M1 implements the real layout per
//! [docs/prd/zip311-payment-disclosure-verifier.md §R-PD-1][prd]. The byte
//! layout once implemented:
//!
//! - bytes 0..4: protocol version tag (`0x00 0x00 0x00 0x01` for ZIP-311 v1).
//! - bytes 4..36: transaction id (32 bytes).
//! - bytes 36..68: payment id (32 bytes).
//! - bytes 68..76: disclosed value in zatoshis (little-endian u64).
//! - byte 76: output kind discriminator (0x00 transparent, 0x01 Sapling).
//! - bytes 77..81: output index (little-endian u32).
//! - bytes 81..: proof block (variable, kind-specific).
//!
//! The parser is pure: it allocates nothing, makes no I/O calls, and never
//! logs its input. It returns the parsed struct or a typed parse error; the
//! error is dropped on the floor by the verifier (mapping to
//! [`super::Verdict::Malformed`]) and never crosses the wire.
//!
//! [prd]: https://github.com/gustavovalverde/zinder/blob/main/docs/prd/zip311-payment-disclosure-verifier.md

/// Output kind a disclosure refers to.
///
/// Reserved for M1; the scaffolding never returns a parsed disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[allow(
    dead_code,
    reason = "Consumed by the M1 parser; kept here so the verifier's public shape is stable."
)]
pub(crate) enum OutputKind {
    /// Transparent (P2PKH / P2SH) output, verified via BIP-340 Schnorr.
    Transparent,
    /// Sapling shielded output, verified via note ciphertext re-decryption.
    Sapling,
}

/// Parsed disclosure ready for the cryptographic verifier.
///
/// Reserved for M1; the scaffolding never constructs this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Consumed by the M1 parser; the public shape is stable across the proposal."
)]
pub(crate) struct ParsedDisclosure {
    /// 32-byte transaction id from the disclosure.
    pub(crate) transaction_id: [u8; 32],
    /// 32-byte payment id from the disclosure.
    pub(crate) payment_id: [u8; 32],
    /// Disclosed value in zatoshis (little-endian decoded).
    pub(crate) disclosed_value_zat: u64,
    /// Output kind discriminator.
    pub(crate) output_kind: OutputKind,
    /// Output index inside the disclosed transaction.
    pub(crate) output_index: u32,
    /// Proof block bytes; layout depends on `output_kind`.
    pub(crate) proof_bytes: Vec<u8>,
}

/// Typed parse error. Never crosses the wire; the verifier maps every error
/// to [`super::Verdict::Malformed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum ParseError {
    /// Header tag did not match a known ZIP-311 protocol version.
    ProtocolVersionUnknown,
    /// Reserved for M1; the scaffolding only emits `ProtocolVersionUnknown`.
    #[allow(
        dead_code,
        reason = "M1 parser emits this variant when the payload is shorter than the fixed header."
    )]
    TooShort,
    /// Reserved for M1.
    #[allow(
        dead_code,
        reason = "M1 parser emits this variant for an unknown output kind discriminator."
    )]
    OutputKindUnknown,
}

/// Parse a ZIP-311 disclosure payload.
///
/// Scaffolding always returns [`ParseError::ProtocolVersionUnknown`] until
/// M1 lands the real layout decoder.
pub(crate) fn parse_disclosure(_disclosure_bytes: &[u8]) -> Result<ParsedDisclosure, ParseError> {
    Err(ParseError::ProtocolVersionUnknown)
}

#[cfg(test)]
mod tests {
    use super::{ParseError, parse_disclosure};

    #[test]
    fn scaffold_rejects_every_input() {
        assert_eq!(
            parse_disclosure(&[]),
            Err(ParseError::ProtocolVersionUnknown)
        );
        assert_eq!(
            parse_disclosure(&[0u8; 81]),
            Err(ParseError::ProtocolVersionUnknown)
        );
    }
}
