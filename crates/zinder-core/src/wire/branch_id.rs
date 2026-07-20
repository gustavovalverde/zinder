//! Consensus branch id hex conversions for Zcash wire dialects.
//!
//! Every Zcash wire surface (lightwalletd `LightdInfo.consensusBranchId`,
//! Zebra and zcashd JSON-RPC `getblockchaininfo.consensusBranchId`) emits the
//! consensus branch id as the same 8-character lowercase hex string: the
//! `u32` value formatted with the standard `{:08x}` shape. There is no byte
//! reversal at this layer.

use crate::ConsensusBranchId;
use crate::wire::WireDecodeError;

const BRANCH_ID_HEX_LEN: usize = 8;

/// Encode a [`ConsensusBranchId`] as 8-character lowercase hex.
///
/// Produces the canonical hex shape used by every Zcash wire surface that
/// names the active consensus branch id (lightwalletd, Zebra and zcashd
/// JSON-RPC). The output is always exactly 8 lowercase hex characters,
/// zero-padded for branch ids below `0x10000000`.
#[must_use]
pub fn encode_branch_id_hex(branch_id: ConsensusBranchId) -> String {
    format!("{:0width$x}", branch_id.value(), width = BRANCH_ID_HEX_LEN)
}

/// Decode an 8-character hex string into a [`ConsensusBranchId`].
///
/// Inverse of [`encode_branch_id_hex`]. Accepts upper or lower case hex.
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 8
/// characters, and [`WireDecodeError::InvalidHex`] if the input contains
/// non-hex characters.
pub fn decode_branch_id_hex(input: &str) -> Result<ConsensusBranchId, WireDecodeError> {
    if input.len() != BRANCH_ID_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: BRANCH_ID_HEX_LEN,
            actual: input.len(),
        });
    }
    let parsed =
        u32::from_str_radix(input, 16).map_err(|parse_error| WireDecodeError::InvalidHex {
            reason: parse_error.to_string(),
        })?;
    Ok(ConsensusBranchId::new(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Branch ID for Zcash NU5; the same value appears in Zebra's activation
    /// table and in lightwalletd's `LightdInfo.consensusBranchId` field.
    const NU5_BRANCH_ID: u32 = 0xc2d6_d0b4;

    /// Regtest branch ID used as a lightwalletd parity fixture.
    const REGTEST_BRANCH_ID: u32 = 0xc8e7_1055;

    #[test]
    fn encode_matches_lightwalletd_observed_value() {
        assert_eq!(
            encode_branch_id_hex(ConsensusBranchId::new(REGTEST_BRANCH_ID)),
            "c8e71055",
        );
    }

    #[test]
    fn encode_pads_low_values_to_eight_characters() {
        assert_eq!(
            encode_branch_id_hex(ConsensusBranchId::new(0x42)),
            "00000042",
        );
    }

    #[test]
    fn encode_emits_lowercase() {
        let encoded = encode_branch_id_hex(ConsensusBranchId::new(NU5_BRANCH_ID));
        assert_eq!(encoded, encoded.to_lowercase());
        assert_eq!(encoded, "c2d6d0b4");
    }

    #[test]
    fn round_trip_lowercase() -> TestResult {
        let original = ConsensusBranchId::new(NU5_BRANCH_ID);
        let encoded = encode_branch_id_hex(original);
        let decoded = decode_branch_id_hex(&encoded)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn decode_accepts_uppercase() -> TestResult {
        let decoded = decode_branch_id_hex("C2D6D0B4")?;
        assert_eq!(decoded, ConsensusBranchId::new(NU5_BRANCH_ID));
        Ok(())
    }

    #[test]
    fn decode_rejects_short_input() {
        let outcome = decode_branch_id_hex("c8e71");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 8, .. })
        ));
    }

    #[test]
    fn decode_rejects_long_input() {
        let outcome = decode_branch_id_hex("c8e710550000");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 8, .. })
        ));
    }

    #[test]
    fn decode_rejects_non_hex_characters() {
        let outcome = decode_branch_id_hex("zzzzzzzz");
        assert!(matches!(outcome, Err(WireDecodeError::InvalidHex { .. })));
    }
}
