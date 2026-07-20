//! In-block transaction position encoder for materialized-view composite keys.
//!
//! Several materialized-view column families key on
//! `(<discriminator>, height, in_block_position)` so per-block ordering is
//! preserved inside a height bucket. The position bytes are the
//! big-endian `u32` representation; lexicographic order matches numeric
//! order, so a forward range scan visits the coinbase first.
//!
//! Inline `in_block_position.to_be_bytes()` at materialized-view boundaries is a
//! forbidden pattern enforced by
//! `crates/zinder-core/tests/integration/wire_invariants.rs`.

use crate::wire::WireDecodeError;

/// Number of bytes an in-block transaction position occupies in a materialized-view key.
pub const IN_BLOCK_POSITION_KEY_LEN: usize = 4;

/// Encodes an in-block transaction position into its big-endian key bytes.
#[must_use]
pub const fn encode_in_block_position(position: u32) -> [u8; IN_BLOCK_POSITION_KEY_LEN] {
    position.to_be_bytes()
}

/// Decodes in-block transaction position bytes back into a `u32`.
///
/// Returns [`WireDecodeError::InvalidLength`] when `bytes` is not exactly
/// [`IN_BLOCK_POSITION_KEY_LEN`] bytes long.
pub fn decode_in_block_position(bytes: &[u8]) -> Result<u32, WireDecodeError> {
    let array: [u8; IN_BLOCK_POSITION_KEY_LEN] =
        bytes
            .try_into()
            .map_err(|_| WireDecodeError::InvalidLength {
                expected: IN_BLOCK_POSITION_KEY_LEN,
                actual: bytes.len(),
            })?;
    Ok(u32::from_be_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_position() {
        let encoded = encode_in_block_position(42);
        let decoded = decode_in_block_position(&encoded);
        assert!(matches!(decoded, Ok(value) if value == 42));
    }

    #[test]
    fn coinbase_position_is_zero_bytes() {
        assert_eq!(encode_in_block_position(0), [0u8; 4]);
    }

    #[test]
    fn keys_sort_lexicographically_by_position() {
        let coinbase = encode_in_block_position(0);
        let later = encode_in_block_position(100);
        assert!(coinbase < later);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let outcome = decode_in_block_position(&[0u8; 3]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: IN_BLOCK_POSITION_KEY_LEN,
                actual: 3
            })
        ));
    }
}
