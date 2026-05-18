//! Unix-seconds key encoder for time-bucketed derive-store column families.
//!
//! Consumers that bucket events by wall-clock second (mempool event-rate
//! counters today) key their per-bucket rows on the 8-byte big-endian Unix
//! second. Lexicographic order matches chronological order, so a
//! forward range scan visits buckets oldest-first and a reverse iterator
//! reads the latest bucket first.
//!
//! Inline `unix_seconds.to_be_bytes()` at derive-store boundaries is a
//! forbidden pattern enforced by
//! `crates/zinder-core/tests/integration/wire_invariants.rs`.

use crate::wire::WireDecodeError;

/// Number of bytes a Unix-seconds timestamp occupies in a derive-store key.
pub const UNIX_SECONDS_KEY_LEN: usize = 8;

/// Encodes a Unix-seconds timestamp into its big-endian key bytes.
#[must_use]
pub const fn encode_unix_seconds(unix_seconds: u64) -> [u8; UNIX_SECONDS_KEY_LEN] {
    unix_seconds.to_be_bytes()
}

/// Decodes Unix-seconds key bytes back into a `u64`.
///
/// Returns [`WireDecodeError::InvalidLength`] when `bytes` is not exactly
/// [`UNIX_SECONDS_KEY_LEN`] bytes long.
pub fn decode_unix_seconds(bytes: &[u8]) -> Result<u64, WireDecodeError> {
    let array: [u8; UNIX_SECONDS_KEY_LEN] =
        bytes
            .try_into()
            .map_err(|_| WireDecodeError::InvalidLength {
                expected: UNIX_SECONDS_KEY_LEN,
                actual: bytes.len(),
            })?;
    Ok(u64::from_be_bytes(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_seconds() {
        let encoded = encode_unix_seconds(1_700_000_000);
        let decoded = decode_unix_seconds(&encoded);
        assert!(matches!(decoded, Ok(value) if value == 1_700_000_000));
    }

    #[test]
    fn keys_sort_lexicographically_oldest_first() {
        let older = encode_unix_seconds(1_700_000_000);
        let newer = encode_unix_seconds(1_700_000_001);
        assert!(older < newer);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let outcome = decode_unix_seconds(&[0u8; 4]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: UNIX_SECONDS_KEY_LEN,
                actual: 4
            })
        ));
    }
}
