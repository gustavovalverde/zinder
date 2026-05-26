//! Block-header Merkle root conversions across Zcash wire dialects.
//!
//! The `hashMerkleRoot` field in a Zcash block header is a 32-byte
//! SHA-256d output computed over the block's transaction ids. It follows
//! the same two-form convention as transaction ids and block hashes:
//!
//! - **Internal byte order**: the byte order the consensus protocol
//!   assigns to the SHA-256d output. Stored verbatim in
//!   `BlockHeader.merkle_root_hash`.
//! - **RPC byte order**: the byte-reversed display form
//!   `zcash-cli getblock` emits as `merkleroot`.
//!
//! Reference: Zcash protocol spec, term `\rpcByteOrder`
//! (protocol.tex:1127, :4036).

use crate::wire::WireDecodeError;

const MERKLE_ROOT_BYTE_COUNT: usize = 32;
const MERKLE_ROOT_HEX_LEN: usize = MERKLE_ROOT_BYTE_COUNT * 2;

/// Encode a 32-byte Merkle root as a lowercase RPC byte order hex string.
///
/// Reverses the input bytes and hex-encodes them, matching the form
/// `zcash-cli getblock` emits as `merkleroot`.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
#[must_use]
pub fn encode_rpc_merkle_root_hex(merkle_root: [u8; 32]) -> String {
    let mut bytes = merkle_root;
    bytes.reverse();
    hex::encode(bytes)
}

/// Decode an RPC byte order hex string into a 32-byte Merkle root.
///
/// Inverse of [`encode_rpc_merkle_root_hex`]. Accepts canonical
/// 64-character lowercase or uppercase hex.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 64
/// characters, and [`WireDecodeError::InvalidHex`] if it contains non-hex
/// characters.
pub fn decode_rpc_merkle_root_hex(input: &str) -> Result<[u8; 32], WireDecodeError> {
    if input.len() != MERKLE_ROOT_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: MERKLE_ROOT_HEX_LEN,
            actual: input.len(),
        });
    }
    let mut buffer = [0u8; MERKLE_ROOT_BYTE_COUNT];
    hex::decode_to_slice(input, &mut buffer).map_err(|hex_error| WireDecodeError::InvalidHex {
        reason: hex_error.to_string(),
    })?;
    buffer.reverse();
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn rpc_hex_round_trip() -> TestResult {
        let mut merkle_root = [0u8; 32];
        for (index, slot) in merkle_root.iter_mut().enumerate() {
            let index_byte = u8::try_from(index).unwrap_or_default();
            *slot = index_byte.wrapping_mul(17).wrapping_add(5);
        }
        let rpc_form = encode_rpc_merkle_root_hex(merkle_root);
        assert_eq!(rpc_form.len(), MERKLE_ROOT_HEX_LEN);
        let decoded = decode_rpc_merkle_root_hex(&rpc_form)?;
        assert_eq!(decoded, merkle_root);
        Ok(())
    }

    #[test]
    fn rpc_hex_emits_lowercase() {
        let merkle_root = [0xAB; 32];
        let rpc_form = encode_rpc_merkle_root_hex(merkle_root);
        assert_eq!(rpc_form, "ab".repeat(32));
    }

    #[test]
    fn rpc_hex_rejects_wrong_length() {
        let outcome = decode_rpc_merkle_root_hex("ab");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 64, .. })
        ));
    }

    #[test]
    fn rpc_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(MERKLE_ROOT_HEX_LEN);
        assert!(matches!(
            decode_rpc_merkle_root_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }
}
