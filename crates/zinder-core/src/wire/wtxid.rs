//! Witness-transaction-id conversions across Zcash wire dialects.
//!
//! Per [ZIP-239](https://zips.z.cash/zip-0239), v5+ Zcash transactions
//! are relayed under `MSG_WTX` with `wtxid = txid || auth_digest`
//! (64 bytes total). Pre-v5 transactions have no distinct wtxid because
//! their txid already covers their witness data.
//!
//! Wtxids follow the same two-form convention as their constituent halves
//! (txid, `auth_digest`), but the RPC byte order rule applies to each
//! 32-byte half *independently*: the RPC-form wtxid is the RPC-form txid
//! concatenated with the RPC-form `auth_digest`, never the byte-reversal
//! of the full 64-byte concatenation.
//!
//! - **Internal byte order**: the byte order [`crate::Wtxid`] stores and
//!   any lightwalletd-compat plane would carry on a `bytes` field.
//!   Reference: Zcash protocol spec, protocol.tex:13560-13564.
//! - **RPC byte order**: 128 lowercase hex characters, structured as
//!   (RPC-form txid, 64 chars) `||` (RPC-form `auth_digest`, 64 chars).
//!   References: ZIP-239 (the witness-id construction); Zcash protocol
//!   spec term `\rpcByteOrder` (protocol.tex:1127, :4036).

use crate::wire::WireDecodeError;
use crate::wire::auth_digest::{decode_rpc_auth_digest_hex, encode_rpc_auth_digest_hex};
use crate::wire::transaction_id::{decode_rpc_transaction_id_hex, encode_rpc_transaction_id_hex};
use crate::{AuthDigest, TransactionId, Wtxid};

const WTXID_BYTE_COUNT: usize = 64;
const WTXID_HEX_LEN: usize = WTXID_BYTE_COUNT * 2;
const HALF_HEX_LEN: usize = 64;

/// Encode a [`Wtxid`] as internal byte order bytes.
///
/// Equivalent to [`Wtxid::as_bytes`] with a wire boundary label so
/// reviewers grep for one name when auditing what crosses the boundary.
///
/// Reference: Zcash protocol spec, protocol.tex:13560-13564.
#[must_use]
pub fn encode_internal_wtxid(wtxid: Wtxid) -> [u8; 64] {
    wtxid.as_bytes()
}

/// Decode internal byte order bytes into a [`Wtxid`].
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the input is not exactly 64
/// bytes.
pub fn decode_internal_wtxid(bytes: &[u8]) -> Result<Wtxid, WireDecodeError> {
    let buffer: [u8; 64] = bytes
        .try_into()
        .map_err(|_| WireDecodeError::InvalidLength {
            expected: WTXID_BYTE_COUNT,
            actual: bytes.len(),
        })?;
    Ok(Wtxid::from_bytes(buffer))
}

/// Encode a [`Wtxid`] as a lowercase RPC byte order hex string.
///
/// Produces 128 lowercase hex characters structured as (RPC-form txid,
/// 64 chars) followed by (RPC-form `auth_digest`, 64 chars). The two
/// halves are reversed independently per ZIP-239: a wtxid is the
/// concatenation `txid || auth_digest`, and each half is rendered in the
/// canonical RPC form Zcash tooling expects.
///
/// References: ZIP-239; Zcash protocol spec term `\rpcByteOrder`
/// (protocol.tex:1127, :4036).
#[must_use]
pub fn encode_rpc_wtxid_hex(wtxid: Wtxid) -> String {
    let bytes = wtxid.as_bytes();
    let mut txid_half = [0u8; 32];
    txid_half.copy_from_slice(&bytes[0..32]);
    let mut auth_half = [0u8; 32];
    auth_half.copy_from_slice(&bytes[32..64]);
    let mut out = encode_rpc_transaction_id_hex(TransactionId::from_bytes(txid_half));
    out.push_str(&encode_rpc_auth_digest_hex(AuthDigest::from_bytes(
        auth_half,
    )));
    out
}

/// Decode an RPC byte order hex string into a [`Wtxid`].
///
/// Inverse of [`encode_rpc_wtxid_hex`]. Accepts 128 lowercase or
/// uppercase hex characters; the leading 64 chars decode as the RPC-form
/// txid and the trailing 64 chars decode as the RPC-form `auth_digest`.
///
/// References: ZIP-239; Zcash protocol spec term `\rpcByteOrder`
/// (protocol.tex:1127, :4036).
///
/// # Errors
///
/// Returns [`WireDecodeError::InvalidLength`] if the string is not 128
/// characters, and [`WireDecodeError::InvalidHex`] if either half contains
/// non-hex characters.
pub fn decode_rpc_wtxid_hex(input: &str) -> Result<Wtxid, WireDecodeError> {
    if input.len() != WTXID_HEX_LEN {
        return Err(WireDecodeError::InvalidLength {
            expected: WTXID_HEX_LEN,
            actual: input.len(),
        });
    }
    let (txid_half, auth_half) = input.split_at(HALF_HEX_LEN);
    let txid = decode_rpc_transaction_id_hex(txid_half)?;
    let auth_digest = decode_rpc_auth_digest_hex(auth_half)?;
    let mut buffer = [0u8; 64];
    buffer[0..32].copy_from_slice(&txid.as_bytes());
    buffer[32..64].copy_from_slice(&auth_digest.as_bytes());
    Ok(Wtxid::from_bytes(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_wtxid() -> Wtxid {
        // Distinct, non-symmetric halves so per-half reversal is observable.
        let mut bytes = [0u8; 64];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let index_byte = u8::try_from(index).unwrap_or_default();
            *slot = index_byte.wrapping_mul(13).wrapping_add(17);
        }
        Wtxid::from_bytes(bytes)
    }

    #[test]
    fn internal_round_trip() -> TestResult {
        let original = sample_wtxid();
        let bytes = encode_internal_wtxid(original);
        let decoded = decode_internal_wtxid(&bytes)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn internal_decode_rejects_wrong_length() {
        let outcome = decode_internal_wtxid(&[0u8; 32]);
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength {
                expected: 64,
                actual: 32,
            })
        ));
    }

    #[test]
    fn rpc_hex_reverses_each_half_independently() {
        // First 32 bytes are 0xab, next 32 bytes are 0xcd; per-half
        // reversal still yields the same byte values, but the boundary
        // between the halves stays at character 64.
        let mut bytes = [0u8; 64];
        bytes[0..32].copy_from_slice(&[0xab; 32]);
        bytes[32..64].copy_from_slice(&[0xcd; 32]);
        let wtxid = Wtxid::from_bytes(bytes);
        let rpc_form = encode_rpc_wtxid_hex(wtxid);
        assert_eq!(rpc_form.len(), WTXID_HEX_LEN);
        let (txid_part, auth_part) = rpc_form.split_at(HALF_HEX_LEN);
        assert_eq!(txid_part, "ab".repeat(32));
        assert_eq!(auth_part, "cd".repeat(32));
    }

    #[test]
    fn rpc_hex_round_trip() -> TestResult {
        let original = sample_wtxid();
        let rpc_form = encode_rpc_wtxid_hex(original);
        assert_eq!(rpc_form.len(), WTXID_HEX_LEN);
        let decoded = decode_rpc_wtxid_hex(&rpc_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn rpc_hex_round_trip_with_asymmetric_halves() -> TestResult {
        let mut bytes = [0u8; 64];
        // Distinct asymmetric halves: txid half uses 0x07..., auth half
        // uses 0x79... (matches the per-half test vectors used in
        // `transaction_id::tests` and `auth_digest::tests`).
        bytes[0..32].copy_from_slice(&[
            0x07, 0x15, 0x50, 0xb5, 0xf9, 0x5f, 0x60, 0xe6, 0xc8, 0x93, 0x8e, 0x38, 0x00, 0xdd,
            0x06, 0xb8, 0x6d, 0xc6, 0x2a, 0xad, 0x7b, 0x15, 0x0d, 0xc1, 0x61, 0xc3, 0x94, 0xab,
            0x9f, 0x72, 0x89, 0x79,
        ]);
        bytes[32..64].copy_from_slice(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ]);
        let original = Wtxid::from_bytes(bytes);
        let rpc_form = encode_rpc_wtxid_hex(original);
        let decoded = decode_rpc_wtxid_hex(&rpc_form)?;
        assert_eq!(decoded, original);
        Ok(())
    }

    #[test]
    fn rpc_hex_emits_lowercase() {
        let wtxid = Wtxid::from_bytes([0xCD; 64]);
        let rpc_form = encode_rpc_wtxid_hex(wtxid);
        assert_eq!(rpc_form, "cd".repeat(64));
    }

    #[test]
    fn rpc_hex_rejects_wrong_length() {
        let outcome = decode_rpc_wtxid_hex("cd");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::InvalidLength { expected: 128, .. })
        ));
    }

    #[test]
    fn rpc_hex_rejects_non_hex_characters() {
        let invalid = "z".repeat(WTXID_HEX_LEN);
        assert!(matches!(
            decode_rpc_wtxid_hex(&invalid),
            Err(WireDecodeError::InvalidHex { .. })
        ));
    }
}
