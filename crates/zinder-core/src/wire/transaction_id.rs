//! Transaction id conversions across Zcash wire dialects.
//!
//! Zcash transaction ids appear in two forms:
//!
//! - **Internal byte order** (also called Zcash internal little-endian
//!   bytes): the byte order the consensus protocol assigns to the SHA-256d
//!   output. This is the byte order [`crate::TransactionId`] stores and the
//!   byte order Zinder's `RocksDB` keys use. Reference: Zcash protocol spec,
//!   protocol.tex:13560-13564.
//! - **RPC byte order**: the byte-reversed display form every Zcash
//!   JSON-RPC reply (`getrawtransaction`, `getblock`, etc.) emits, every
//!   wallet UI shows, every block explorer shows, and the Zcash protocol
//!   specification's example block hashes use. Defined normatively in the
//!   spec at protocol.tex:1127 (`\rpcByteOrder`) and used at
//!   protocol.tex:4036.
//!
//! Pick the function whose name matches the wire surface:
//! - Native zinder protobuf and lightwalletd-compat protobuf `bytes`
//!   fields carry internal byte order. Use [`encode_internal_transaction_id`]
//!   and [`decode_internal_transaction_id`].
//! - Native zinder protobuf `string` hash fields, JSON, log records, and
//!   any human-facing surface carry RPC byte order hex. Use
//!   [`encode_rpc_transaction_id_hex`] and [`decode_rpc_transaction_id_hex`].

use crate::TransactionId;
use crate::wire::WireDecodeError;

const TRANSACTION_ID_BYTE_COUNT: usize = 32;
const TRANSACTION_ID_HEX_LEN: usize = TRANSACTION_ID_BYTE_COUNT * 2;

/// Encode a [`TransactionId`] as internal byte order bytes.
///
/// The output is the canonical byte form proto `bytes` fields carry on the
/// lightwalletd compatibility plane (where the wire shape is frozen) and
/// the byte form the canonical storage layer keys by. Equivalent to
/// [`TransactionId::as_bytes`] with a wire boundary label so reviewers
/// grep for one name when auditing what crosses the boundary.
///
/// Reference: Zcash protocol spec, protocol.tex:13560-13564.
#[must_use]
pub fn encode_internal_transaction_id(transaction_id: TransactionId) -> [u8; 32] {
    transaction_id.as_bytes()
}

/// Decode internal byte order bytes into a [`TransactionId`].
///
/// Length-validates the input and constructs the canonical domain value.
/// Pair with [`encode_internal_transaction_id`] for round-trip recovery
/// across the lightwalletd-compat `bytes` boundary.
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

/// Encode a [`TransactionId`] as a lowercase RPC byte order hex string.
///
/// Produces the canonical 64-character lowercase hex form every Zcash
/// JSON-RPC reply (`getrawtransaction`, `getblock`), every wallet UI,
/// every block explorer, and the protocol spec's example transaction ids
/// use. The bytes are reversed before hex-encoding so the leftmost hex
/// character corresponds to the txid's high byte in the form readers
/// recognize.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
#[must_use]
pub fn encode_rpc_transaction_id_hex(transaction_id: TransactionId) -> String {
    let mut bytes = transaction_id.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

/// Decode an RPC byte order hex string into a [`TransactionId`].
///
/// Inverse of [`encode_rpc_transaction_id_hex`]. Accepts canonical
/// 64-character lowercase or uppercase hex.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 64
/// characters, and [`WireDecodeError::InvalidHex`] if it contains non-hex
/// characters.
pub fn decode_rpc_transaction_id_hex(input: &str) -> Result<TransactionId, WireDecodeError> {
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

    /// Internal byte order form of a real testnet txid from block 4031230.
    /// Paired with [`TESTNET_TXID_RPC_HEX`] below: the two are byte-reversed.
    const TESTNET_TXID_INTERNAL_BYTES: [u8; 32] = [
        0x36, 0x94, 0x55, 0xb7, 0x8a, 0xfc, 0xa3, 0xdc, 0xb5, 0x2b, 0xec, 0xfd, 0x38, 0x72, 0xba,
        0xf5, 0xd0, 0x51, 0xb3, 0x2e, 0x81, 0x65, 0xbc, 0x2c, 0x79, 0x61, 0x06, 0x9e, 0xe6, 0x0c,
        0xca, 0xc3,
    ];

    /// RPC byte order hex form of the testnet txid above. Matches the value
    /// `zcash-cli getblock` returns for the txid at testnet height 4031230.
    const TESTNET_TXID_RPC_HEX: &str =
        "c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436";

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
    fn rpc_hex_matches_zcash_cli_for_testnet_txid() {
        let transaction_id = TransactionId::from_bytes(TESTNET_TXID_INTERNAL_BYTES);
        assert_eq!(
            encode_rpc_transaction_id_hex(transaction_id),
            TESTNET_TXID_RPC_HEX
        );
    }

    #[test]
    fn rpc_hex_decode_matches_storage_form_for_testnet_txid() -> TestResult {
        let decoded = decode_rpc_transaction_id_hex(TESTNET_TXID_RPC_HEX)?;
        assert_eq!(decoded.as_bytes(), TESTNET_TXID_INTERNAL_BYTES);
        Ok(())
    }

    #[test]
    fn rpc_hex_reverses_internal_bytes() {
        let transaction_id = TransactionId::from_bytes([
            0x07, 0x15, 0x50, 0xb5, 0xf9, 0x5f, 0x60, 0xe6, 0xc8, 0x93, 0x8e, 0x38, 0x00, 0xdd,
            0x06, 0xb8, 0x6d, 0xc6, 0x2a, 0xad, 0x7b, 0x15, 0x0d, 0xc1, 0x61, 0xc3, 0x94, 0xab,
            0x9f, 0x72, 0x89, 0x79,
        ]);
        let rpc_form = encode_rpc_transaction_id_hex(transaction_id);
        assert_eq!(
            rpc_form,
            "7989729fab94c361c10d157bad2ac66db806dd00388e93c8e6605ff9b5501507"
        );
    }

    #[test]
    fn rpc_hex_round_trip() -> TestResult {
        let original = sample_transaction_id();
        let rpc_form = encode_rpc_transaction_id_hex(original);
        assert_eq!(rpc_form.len(), TRANSACTION_ID_HEX_LEN);
        let decoded = decode_rpc_transaction_id_hex(&rpc_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn rpc_hex_emits_lowercase() {
        let transaction_id = TransactionId::from_bytes([0xAB; 32]);
        let rpc_form = encode_rpc_transaction_id_hex(transaction_id);
        assert_eq!(rpc_form, "ab".repeat(32));
    }

    #[test]
    fn rpc_hex_rejects_wrong_length() {
        let outcome = decode_rpc_transaction_id_hex("ab");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 64, .. })
        ));
    }

    #[test]
    fn rpc_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(TRANSACTION_ID_HEX_LEN);
        assert!(matches!(
            decode_rpc_transaction_id_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }

    #[test]
    fn rpc_hex_is_case_insensitive() -> TestResult {
        let lower_hex = format!("ab{}", "00".repeat(31));
        let upper_hex = format!("AB{}", "00".repeat(31));
        let lower_decoded = decode_rpc_transaction_id_hex(&lower_hex)?;
        let upper_decoded = decode_rpc_transaction_id_hex(&upper_hex)?;
        assert_eq!(lower_decoded, upper_decoded);
        Ok(())
    }
}
