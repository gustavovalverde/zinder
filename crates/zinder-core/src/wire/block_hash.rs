//! Block hash conversions across Zcash wire dialects.
//!
//! Block hashes follow the same two-form convention as transaction ids:
//!
//! - **Zcash internal little-endian bytes**, used by every `bytes` field in
//!   protobuf wire schemas (lightwalletd `BlockId.hash`, `CompactBlock.hash`,
//!   `CompactBlock.prevHash`; the native `zinder.v1.wallet.BlockId` byte
//!   fields). Same byte order [`crate::BlockHash`] stores.
//! - **Display big-endian hex strings**, used by every Zcash JSON-RPC reply
//!   (`getbestblockhash`, `getblock`, etc.), by lightwalletd's
//!   `TreeState.network`-style hex fields, by block explorers, and anywhere
//!   a block hash is quoted to humans.
//!
//! Pick the function whose name matches the wire surface. For proto `bytes`
//! fields use [`encode_internal_block_hash`] and [`decode_internal_block_hash`];
//! for hex-string surfaces use [`encode_display_block_hash_hex`] and
//! [`decode_display_block_hash_hex`].

use crate::BlockHash;
use crate::wire::WireDecodeError;

const BLOCK_HASH_BYTE_COUNT: usize = 32;
const BLOCK_HASH_HEX_LEN: usize = BLOCK_HASH_BYTE_COUNT * 2;

/// Encode a [`BlockHash`] as Zcash internal little-endian bytes.
///
/// The output is the canonical byte form proto `bytes` fields carry. The
/// function exists alongside [`BlockHash::as_bytes`] so that reviewers can
/// grep one name when auditing wire emissions.
#[must_use]
pub fn encode_internal_block_hash(block_hash: BlockHash) -> [u8; 32] {
    block_hash.as_bytes()
}

/// Decode Zcash internal little-endian bytes into a [`BlockHash`].
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 32
/// bytes.
pub fn decode_internal_block_hash(bytes: &[u8]) -> Result<BlockHash, WireDecodeError> {
    let buffer: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WireDecodeError::InvalidLength {
            expected: BLOCK_HASH_BYTE_COUNT,
            actual: bytes.len(),
        })?;
    Ok(BlockHash::from_bytes(buffer))
}

/// Encode a [`BlockHash`] as a lowercase display-order hex string.
///
/// Produces the canonical 64-character lowercase hex form used by every
/// Zcash JSON-RPC reply, by lightwalletd `string` hex fields, and by
/// log records and block explorers. Reverses the internal byte order so
/// the leftmost hex character corresponds to the block hash's high byte
/// in human-readable form.
#[must_use]
pub fn encode_display_block_hash_hex(block_hash: BlockHash) -> String {
    let mut bytes = block_hash.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

/// Decode a display-order hex string into a [`BlockHash`].
///
/// Inverse of [`encode_display_block_hash_hex`]. Accepts canonical
/// 64-character lowercase or uppercase hex.
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 64
/// characters, and [`WireDecodeError::InvalidHex`] if it contains non-hex
/// characters.
pub fn decode_display_block_hash_hex(input: &str) -> Result<BlockHash, WireDecodeError> {
    if input.len() != BLOCK_HASH_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: BLOCK_HASH_HEX_LEN,
            actual: input.len(),
        });
    }
    let mut buffer = [0u8; BLOCK_HASH_BYTE_COUNT];
    hex::decode_to_slice(input, &mut buffer).map_err(|hex_error| WireDecodeError::InvalidHex {
        reason: hex_error.to_string(),
    })?;
    buffer.reverse();
    Ok(BlockHash::from_bytes(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_block_hash() -> BlockHash {
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let index_byte = u8::try_from(index).unwrap_or_default();
            *slot = index_byte.wrapping_mul(11).wrapping_add(31);
        }
        BlockHash::from_bytes(bytes)
    }

    #[test]
    fn internal_encode_is_identity() {
        let block_hash = sample_block_hash();
        assert_eq!(
            encode_internal_block_hash(block_hash),
            block_hash.as_bytes()
        );
    }

    #[test]
    fn internal_round_trip() -> TestResult {
        let original = sample_block_hash();
        let bytes = encode_internal_block_hash(original);
        let decoded = decode_internal_block_hash(&bytes)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn internal_decode_rejects_wrong_length() {
        let outcome = decode_internal_block_hash(&[0u8; 8]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: 32,
                actual: 8,
            })
        ));
    }

    #[test]
    fn display_hex_reverses_internal_bytes() {
        let block_hash = BlockHash::from_bytes([
            0xaf, 0x7c, 0x89, 0xb6, 0x9b, 0x53, 0x8f, 0xdf, 0xd3, 0xb1, 0x2e, 0x84, 0x5f, 0x08,
            0xf2, 0x37, 0xd4, 0xeb, 0x3a, 0x93, 0x24, 0x1d, 0x27, 0x88, 0x67, 0x44, 0x4b, 0x2e,
            0x50, 0x15, 0x69, 0xee,
        ]);
        let display = encode_display_block_hash_hex(block_hash);
        assert_eq!(
            display,
            "ee6915502e4b446788271d24933aebd437f2085f842eb1d3df8f539bb6897caf"
        );
    }

    #[test]
    fn display_hex_round_trip() -> TestResult {
        let original = sample_block_hash();
        let hex_form = encode_display_block_hash_hex(original);
        assert_eq!(hex_form.len(), BLOCK_HASH_HEX_LEN);
        let decoded = decode_display_block_hash_hex(&hex_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn display_hex_emits_lowercase() {
        let block_hash = BlockHash::from_bytes([0xCD; 32]);
        let hex_form = encode_display_block_hash_hex(block_hash);
        assert_eq!(hex_form, "cd".repeat(32));
    }

    #[test]
    fn display_hex_rejects_wrong_length() {
        let outcome = decode_display_block_hash_hex("cd");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 64, .. })
        ));
    }

    #[test]
    fn display_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(BLOCK_HASH_HEX_LEN);
        assert!(matches!(
            decode_display_block_hash_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }
}
