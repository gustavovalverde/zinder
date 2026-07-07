//! Authorization-digest conversions across Zcash wire dialects.
//!
//! Per [ZIP-244](https://zips.z.cash/zip-0244), v5+ Zcash transactions
//! carry a 32-byte authorization digest computed alongside the txid.
//! Authorization digests follow the same two-form convention as txids and
//! block hashes:
//!
//! - **Internal byte order**: the byte order [`crate::AuthDigest`] stores
//!   and the lightwalletd-compat plane carries on `bytes` fields.
//!   Reference: Zcash protocol spec, protocol.tex:13560-13564.
//! - **RPC byte order**: the byte-reversed display form Zcash JSON-RPC
//!   replies emit and wallets, explorers, and log records render.
//!   Reference: Zcash protocol spec, term `\rpcByteOrder`
//!   (protocol.tex:1127, :4036).
//!
//! Pick the function whose name matches the wire surface: internal for
//! storage and lightwalletd-compat `bytes`; RPC for native zinder
//! `string` hash fields and any human-facing surface.

use crate::AuthDigest;
use crate::wire::WireDecodeError;

const AUTH_DIGEST_BYTE_COUNT: usize = 32;
const AUTH_DIGEST_HEX_LEN: usize = AUTH_DIGEST_BYTE_COUNT * 2;

/// Encode an [`AuthDigest`] as internal byte order bytes.
///
/// Equivalent to [`AuthDigest::as_bytes`] with a wire boundary label so
/// reviewers grep for one name when auditing what crosses the boundary.
///
/// Reference: Zcash protocol spec, protocol.tex:13560-13564.
#[must_use]
pub fn encode_internal_auth_digest(auth_digest: AuthDigest) -> [u8; 32] {
    auth_digest.as_bytes()
}

/// Decode internal byte order bytes into an [`AuthDigest`].
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 32
/// bytes.
pub fn decode_internal_auth_digest(bytes: &[u8]) -> Result<AuthDigest, WireDecodeError> {
    let buffer: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WireDecodeError::InvalidLength {
            expected: AUTH_DIGEST_BYTE_COUNT,
            actual: bytes.len(),
        })?;
    Ok(AuthDigest::from_bytes(buffer))
}

/// Encode an [`AuthDigest`] as a lowercase RPC byte order hex string.
///
/// Produces the canonical 64-character lowercase hex form, byte-reversed
/// from the internal form. Matches how `zcash-cli getrawtransaction
/// <txid> 1` displays the `authdigest` field on v5+ transactions.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
#[must_use]
pub fn encode_rpc_auth_digest_hex(auth_digest: AuthDigest) -> String {
    let mut bytes = auth_digest.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

/// Decode an RPC byte order hex string into an [`AuthDigest`].
///
/// Inverse of [`encode_rpc_auth_digest_hex`]. Accepts canonical
/// 64-character lowercase or uppercase hex.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 64
/// characters, and [`WireDecodeError::InvalidHex`] if it contains non-hex
/// characters.
pub fn decode_rpc_auth_digest_hex(input: &str) -> Result<AuthDigest, WireDecodeError> {
    if input.len() != AUTH_DIGEST_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: AUTH_DIGEST_HEX_LEN,
            actual: input.len(),
        });
    }
    let mut buffer = [0u8; AUTH_DIGEST_BYTE_COUNT];
    hex::decode_to_slice(input, &mut buffer).map_err(|hex_error| WireDecodeError::InvalidHex {
        reason: hex_error.to_string(),
    })?;
    buffer.reverse();
    Ok(AuthDigest::from_bytes(buffer))
}

/// Decode RPC byte order bytes into an [`AuthDigest`].
///
/// The raw-bytes analogue of [`decode_rpc_auth_digest_hex`]. Zebra's
/// indexer gRPC surface (`zebra_indexer_rpc`) fills digest `bytes` fields
/// with `bytes_in_display_order`, so the input is reversed into internal
/// byte order before constructing the domain value.
///
/// Reference: Zcash protocol spec, term `\rpcByteOrder` (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 32
/// bytes.
pub fn decode_rpc_auth_digest_bytes(bytes: &[u8]) -> Result<AuthDigest, WireDecodeError> {
    let mut buffer: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WireDecodeError::InvalidLength {
            expected: AUTH_DIGEST_BYTE_COUNT,
            actual: bytes.len(),
        })?;
    buffer.reverse();
    Ok(AuthDigest::from_bytes(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_auth_digest() -> AuthDigest {
        // Non-symmetric byte pattern so reversal is observable.
        AuthDigest::from_bytes([0xab; 32])
    }

    #[test]
    fn internal_round_trip() -> TestResult {
        let original = sample_auth_digest();
        let bytes = encode_internal_auth_digest(original);
        let decoded = decode_internal_auth_digest(&bytes)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn internal_decode_rejects_wrong_length() {
        let outcome = decode_internal_auth_digest(&[0u8; 16]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: 32,
                actual: 16,
            })
        ));
    }

    #[test]
    fn rpc_hex_reverses_internal_bytes() {
        let auth_digest = AuthDigest::from_bytes([
            0x07, 0x15, 0x50, 0xb5, 0xf9, 0x5f, 0x60, 0xe6, 0xc8, 0x93, 0x8e, 0x38, 0x00, 0xdd,
            0x06, 0xb8, 0x6d, 0xc6, 0x2a, 0xad, 0x7b, 0x15, 0x0d, 0xc1, 0x61, 0xc3, 0x94, 0xab,
            0x9f, 0x72, 0x89, 0x79,
        ]);
        let rpc_form = encode_rpc_auth_digest_hex(auth_digest);
        assert_eq!(
            rpc_form,
            "7989729fab94c361c10d157bad2ac66db806dd00388e93c8e6605ff9b5501507"
        );
    }

    #[test]
    fn rpc_hex_round_trip() -> TestResult {
        let original = AuthDigest::from_bytes([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ]);
        let rpc_form = encode_rpc_auth_digest_hex(original);
        assert_eq!(rpc_form.len(), AUTH_DIGEST_HEX_LEN);
        let decoded = decode_rpc_auth_digest_hex(&rpc_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn rpc_hex_emits_lowercase() {
        let auth_digest = AuthDigest::from_bytes([0xCD; 32]);
        let rpc_form = encode_rpc_auth_digest_hex(auth_digest);
        assert_eq!(rpc_form, "cd".repeat(32));
    }

    #[test]
    fn rpc_bytes_decode_reverses_into_internal_order() -> TestResult {
        let internal_bytes: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ];
        let mut display_order_bytes = internal_bytes;
        display_order_bytes.reverse();
        let decoded = decode_rpc_auth_digest_bytes(&display_order_bytes)?;
        assert_eq!(decoded.as_bytes(), internal_bytes);
        Ok(())
    }

    #[test]
    fn rpc_bytes_decode_rejects_wrong_length() {
        let outcome = decode_rpc_auth_digest_bytes(&[0u8; 16]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: 32,
                actual: 16,
            })
        ));
    }

    #[test]
    fn rpc_hex_rejects_wrong_length() {
        let outcome = decode_rpc_auth_digest_hex("cd");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 64, .. })
        ));
    }

    #[test]
    fn rpc_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(AUTH_DIGEST_HEX_LEN);
        assert!(matches!(
            decode_rpc_auth_digest_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }
}
