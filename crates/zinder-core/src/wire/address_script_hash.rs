//! Transparent address script-hash wire codec.
//!
//! Materialized-view store column families that key on a transparent address (for
//! example the address-activity projection) use the canonical 32-byte
//! [`TransparentAddressScriptHash`] form as their per-address discriminator.
//! Inline `.as_bytes()` calls outside this module are forbidden so that the
//! storage and wire layers stay reachable from one well-known function.

use crate::TransparentAddressScriptHash;
use crate::wire::WireDecodeError;

/// Number of bytes a transparent address script-hash occupies on the wire
/// and in materialized-view keys.
pub const ADDRESS_SCRIPT_HASH_LEN: usize = 32;

/// Encodes a transparent address script-hash into its canonical bytes.
#[must_use]
pub const fn encode_address_script_hash(
    address_script_hash: TransparentAddressScriptHash,
) -> [u8; ADDRESS_SCRIPT_HASH_LEN] {
    address_script_hash.as_bytes()
}

/// Decodes the canonical 32 bytes back into a script hash.
///
/// Returns [`WireDecodeError::InvalidLength`] when `bytes` is not exactly
/// [`ADDRESS_SCRIPT_HASH_LEN`] bytes long.
pub fn decode_address_script_hash(
    bytes: &[u8],
) -> Result<TransparentAddressScriptHash, WireDecodeError> {
    let array: [u8; ADDRESS_SCRIPT_HASH_LEN] =
        bytes
            .try_into()
            .map_err(|_| WireDecodeError::InvalidLength {
                expected: ADDRESS_SCRIPT_HASH_LEN,
                actual: bytes.len(),
            })?;
    Ok(TransparentAddressScriptHash::from_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_script_hash_bytes() {
        let mut bytes = [0u8; ADDRESS_SCRIPT_HASH_LEN];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap_or(0);
        }
        let script_hash = TransparentAddressScriptHash::from_bytes(bytes);
        let encoded = encode_address_script_hash(script_hash);
        let outcome = decode_address_script_hash(&encoded);
        assert!(matches!(outcome, Ok(value) if value == script_hash));
    }

    #[test]
    fn decode_rejects_short_input() {
        let too_short = [0u8; 16];
        let outcome = decode_address_script_hash(&too_short);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: ADDRESS_SCRIPT_HASH_LEN,
                actual: 16
            })
        ));
    }

    #[test]
    fn decode_rejects_long_input() {
        let too_long = [0u8; 48];
        let outcome = decode_address_script_hash(&too_long);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: ADDRESS_SCRIPT_HASH_LEN,
                actual: 48
            })
        ));
    }
}
