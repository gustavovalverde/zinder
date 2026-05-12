//! Transaction id conversions across Zcash wire dialects.
//!
//! Zcash transaction ids appear in two forms on the wire:
//!
//! - **Zcash internal little-endian bytes**, used by every `bytes` field in
//!   protobuf wire schemas (lightwalletd `RawTransaction.hash`,
//!   `TxFilter.hash`, `GetAddressUtxosReply.txid`, `CompactTx.hash`; the
//!   native `zinder.v1.wallet.Transaction` byte fields). This is the same
//!   byte order [`crate::TransactionId`] stores. Lightwalletd-go documents
//!   this explicitly at `frontend/service.go:792`: "When expressed as bytes,
//!   a txid must be little-endian."
//! - **Display big-endian hex strings**, used by every Zcash JSON-RPC reply
//!   (`getrawtransaction`, `getblock`, etc.), by lightwalletd's hex-encoded
//!   error messages, by block explorers, and anywhere a txid is quoted to
//!   humans. The byte order is the reverse of the internal form.
//!
//! Pick the function whose name matches the wire surface. For proto `bytes`
//! fields use [`encode_internal_transaction_id`] and
//! [`decode_internal_transaction_id`]; for hex-string surfaces use
//! [`encode_display_transaction_id_hex`] and
//! [`decode_display_transaction_id_hex`].

use crate::TransactionId;
use crate::wire::WireDecodeError;

const TRANSACTION_ID_BYTE_COUNT: usize = 32;
const TRANSACTION_ID_HEX_LEN: usize = TRANSACTION_ID_BYTE_COUNT * 2;

/// Encode a [`TransactionId`] as Zcash internal little-endian bytes.
///
/// The output is the canonical byte form proto `bytes` fields carry across
/// every Zcash wire surface (lightwalletd, native zinder, future
/// streaming RPCs). Equivalent to [`TransactionId::as_bytes`] with a wire
/// boundary label so reviewers grep for one name when auditing what crosses
/// the boundary.
#[must_use]
pub fn encode_internal_transaction_id(transaction_id: TransactionId) -> [u8; 32] {
    transaction_id.as_bytes()
}

/// Decode Zcash internal little-endian bytes into a [`TransactionId`].
///
/// Length-validates the input and constructs the canonical domain value.
/// Pair with [`encode_internal_transaction_id`] for round-trip recovery
/// across proto `bytes` field boundaries.
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 32
/// bytes.
pub fn decode_internal_transaction_id(bytes: &[u8]) -> Result<TransactionId, WireDecodeError> {
    let buffer: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WireDecodeError::InvalidLength {
            expected: TRANSACTION_ID_BYTE_COUNT,
            actual: bytes.len(),
        })?;
    Ok(TransactionId::from_bytes(buffer))
}

/// Encode a [`TransactionId`] as a lowercase display-order hex string.
///
/// Produces the canonical 64-character lowercase hex form used by every
/// Zcash JSON-RPC reply (`getrawtransaction`, `getblock`), by lightwalletd's
/// hex-encoded error messages, by block explorers, and by log records that
/// quote txids to humans. The output reverses the internal byte order so
/// the leftmost hex character corresponds to the txid's high byte in
/// human-readable form.
#[must_use]
pub fn encode_display_transaction_id_hex(transaction_id: TransactionId) -> String {
    let mut bytes = transaction_id.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

/// Decode a display-order hex string into a [`TransactionId`].
///
/// Inverse of [`encode_display_transaction_id_hex`]. Accepts canonical
/// 64-character lowercase or uppercase hex.
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 64
/// characters, and [`WireDecodeError::InvalidHex`] if it contains non-hex
/// characters.
pub fn decode_display_transaction_id_hex(input: &str) -> Result<TransactionId, WireDecodeError> {
    if input.len() != TRANSACTION_ID_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: TRANSACTION_ID_HEX_LEN,
            actual: input.len(),
        });
    }
    let mut buffer = [0u8; TRANSACTION_ID_BYTE_COUNT];
    hex::decode_to_slice(input, &mut buffer).map_err(|hex_error| WireDecodeError::InvalidHex {
        reason: hex_error.to_string(),
    })?;
    buffer.reverse();
    Ok(TransactionId::from_bytes(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_transaction_id() -> TransactionId {
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            // Choose a non-symmetric pattern so reversal is observable.
            let index_byte = u8::try_from(index).unwrap_or_default();
            *slot = index_byte.wrapping_mul(7).wrapping_add(13);
        }
        TransactionId::from_bytes(bytes)
    }

    #[test]
    fn internal_encode_is_identity() {
        let transaction_id = sample_transaction_id();
        assert_eq!(
            encode_internal_transaction_id(transaction_id),
            transaction_id.as_bytes()
        );
    }

    #[test]
    fn internal_round_trip() -> TestResult {
        let original = sample_transaction_id();
        let bytes = encode_internal_transaction_id(original);
        let decoded = decode_internal_transaction_id(&bytes)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn internal_decode_rejects_wrong_length() {
        let outcome = decode_internal_transaction_id(&[0u8; 16]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: 32,
                actual: 16,
            })
        ));
    }

    #[test]
    fn display_hex_reverses_internal_bytes() {
        let transaction_id = TransactionId::from_bytes([
            0x07, 0x15, 0x50, 0xb5, 0xf9, 0x5f, 0x60, 0xe6, 0xc8, 0x93, 0x8e, 0x38, 0x00, 0xdd,
            0x06, 0xb8, 0x6d, 0xc6, 0x2a, 0xad, 0x7b, 0x15, 0x0d, 0xc1, 0x61, 0xc3, 0x94, 0xab,
            0x9f, 0x72, 0x89, 0x79,
        ]);
        // Display order is the same bytes reversed and rendered as hex.
        let display = encode_display_transaction_id_hex(transaction_id);
        assert_eq!(
            display,
            "7989729fab94c361c10d157bad2ac66db806dd00388e93c8e6605ff9b5501507"
        );
    }

    #[test]
    fn display_hex_round_trip() -> TestResult {
        let original = sample_transaction_id();
        let hex_form = encode_display_transaction_id_hex(original);
        assert_eq!(hex_form.len(), TRANSACTION_ID_HEX_LEN);
        let decoded = decode_display_transaction_id_hex(&hex_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn display_hex_emits_lowercase() {
        let transaction_id = TransactionId::from_bytes([0xAB; 32]);
        let hex_form = encode_display_transaction_id_hex(transaction_id);
        assert_eq!(hex_form, "ab".repeat(32));
    }

    #[test]
    fn display_hex_rejects_wrong_length() {
        let outcome = decode_display_transaction_id_hex("ab");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 64, .. })
        ));
    }

    #[test]
    fn display_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(TRANSACTION_ID_HEX_LEN);
        assert!(matches!(
            decode_display_transaction_id_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }

    #[test]
    fn display_hex_is_case_insensitive() -> TestResult {
        let lower_hex = format!("ab{}", "00".repeat(31));
        let upper_hex = format!("AB{}", "00".repeat(31));
        let lower_decoded = decode_display_transaction_id_hex(&lower_hex)?;
        let upper_decoded = decode_display_transaction_id_hex(&upper_hex)?;
        assert_eq!(lower_decoded, upper_decoded);
        Ok(())
    }
}
