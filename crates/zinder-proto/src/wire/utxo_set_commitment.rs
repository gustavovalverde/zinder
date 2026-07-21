//! Transparent UTXO-set commitment wire codec.
//!
//! Maps the pure-domain [`zinder_core::TransparentUtxoSetCommitment`] onto the
//! generated wallet-plane message, and its [`zinder_core::UtxoSetCommitmentScheme`]
//! onto the wire enum. The accumulator bytes ride verbatim; only the scheme
//! discriminant is translated. Inline construction of the wire message outside
//! this module is a forbidden pattern.

use zinder_core::{TransparentUtxoSetCommitment, UTXO_SET_COMMITMENT_LEN, UtxoSetCommitmentScheme};

use crate::v1::wallet::{
    TransparentUtxoSetCommitment as WireTransparentUtxoSetCommitment,
    UtxoSetCommitmentScheme as WireUtxoSetCommitmentScheme,
};

/// Translates a native commitment scheme into the wire enum.
///
/// A future native scheme with no wire counterpart is rejected until the wire
/// contract assigns it a discriminant.
#[allow(
    unreachable_patterns,
    reason = "UtxoSetCommitmentScheme is non-exhaustive; the encoder fails closed for future schemes."
)]
fn encode_utxo_set_commitment_scheme(
    scheme: UtxoSetCommitmentScheme,
) -> Result<WireUtxoSetCommitmentScheme, TransparentUtxoSetCommitmentEncodeError> {
    match scheme {
        UtxoSetCommitmentScheme::LtHash16 => Ok(WireUtxoSetCommitmentScheme::Lthash16),
        UtxoSetCommitmentScheme::Unspecified | _ => {
            Err(TransparentUtxoSetCommitmentEncodeError::UnsupportedScheme)
        }
    }
}

/// Error returned when a native commitment cannot be represented on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransparentUtxoSetCommitmentEncodeError {
    /// The native scheme has no wallet-protocol discriminant.
    UnsupportedScheme,
}

impl std::fmt::Display for TransparentUtxoSetCommitmentEncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedScheme => {
                formatter.write_str("unsupported utxo-set commitment scheme")
            }
        }
    }
}

impl std::error::Error for TransparentUtxoSetCommitmentEncodeError {}

/// Builds the wire commitment message from a native commitment.
///
/// Copies the raw accumulator bytes and sets the wire scheme enum. The result
/// is wrapped in `Some` only at the field site when the capability is
/// advertised.
pub fn encode_transparent_utxo_set_commitment(
    commitment: &TransparentUtxoSetCommitment,
) -> Result<WireTransparentUtxoSetCommitment, TransparentUtxoSetCommitmentEncodeError> {
    Ok(WireTransparentUtxoSetCommitment {
        scheme: encode_utxo_set_commitment_scheme(commitment.scheme())? as i32,
        commitment: commitment.accumulator().to_vec(),
    })
}

/// Error returned when a wire commitment message cannot be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransparentUtxoSetCommitmentDecodeError {
    /// The scheme discriminant did not map to a known scheme.
    UnknownScheme {
        /// The unrecognized scheme discriminant.
        scheme: i32,
    },
    /// The accumulator byte length did not match the scheme.
    InvalidAccumulatorLength {
        /// Number of bytes the scheme requires.
        expected: usize,
        /// Number of bytes the message carried.
        actual: usize,
    },
}

impl std::fmt::Display for TransparentUtxoSetCommitmentDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScheme { scheme } => {
                write!(formatter, "unknown utxo-set commitment scheme: {scheme}")
            }
            Self::InvalidAccumulatorLength { expected, actual } => write!(
                formatter,
                "utxo-set commitment accumulator expected {expected} bytes, received {actual}"
            ),
        }
    }
}

impl std::error::Error for TransparentUtxoSetCommitmentDecodeError {}

/// Decodes a wire commitment message into the native commitment.
///
/// # Errors
///
/// Returns [`TransparentUtxoSetCommitmentDecodeError::UnknownScheme`] when the
/// scheme discriminant is unknown, and
/// [`TransparentUtxoSetCommitmentDecodeError::InvalidAccumulatorLength`] when the
/// accumulator byte length does not match the scheme.
pub fn decode_transparent_utxo_set_commitment(
    message: &WireTransparentUtxoSetCommitment,
) -> Result<TransparentUtxoSetCommitment, TransparentUtxoSetCommitmentDecodeError> {
    let scheme = u32::try_from(message.scheme)
        .ok()
        .and_then(UtxoSetCommitmentScheme::from_id)
        .ok_or(TransparentUtxoSetCommitmentDecodeError::UnknownScheme {
            scheme: message.scheme,
        })?;
    TransparentUtxoSetCommitment::from_parts(scheme, &message.commitment).ok_or(
        TransparentUtxoSetCommitmentDecodeError::InvalidAccumulatorLength {
            expected: UTXO_SET_COMMITMENT_LEN,
            actual: message.commitment.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lthash16_maps_to_wire_lthash16() {
        assert_eq!(
            encode_utxo_set_commitment_scheme(UtxoSetCommitmentScheme::LtHash16),
            Ok(WireUtxoSetCommitmentScheme::Lthash16)
        );
    }

    #[test]
    fn unspecified_is_rejected() {
        assert_eq!(
            encode_utxo_set_commitment_scheme(UtxoSetCommitmentScheme::Unspecified),
            Err(TransparentUtxoSetCommitmentEncodeError::UnsupportedScheme)
        );
    }

    #[test]
    fn encodes_empty_commitment_with_full_accumulator() -> Result<(), Box<dyn std::error::Error>> {
        let commitment = TransparentUtxoSetCommitment::empty();
        let wire = encode_transparent_utxo_set_commitment(&commitment)?;
        assert_eq!(wire.scheme, WireUtxoSetCommitmentScheme::Lthash16 as i32);
        assert_eq!(wire.commitment.len(), commitment.accumulator().len());
        Ok(())
    }

    #[test]
    fn encode_then_decode_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let commitment = TransparentUtxoSetCommitment::empty();
        let wire = encode_transparent_utxo_set_commitment(&commitment)?;
        let decoded = decode_transparent_utxo_set_commitment(&wire);
        assert_eq!(decoded, Ok(commitment));
        Ok(())
    }

    #[test]
    fn decode_rejects_unknown_scheme() {
        let wire = WireTransparentUtxoSetCommitment {
            scheme: 99,
            commitment: vec![0u8; UTXO_SET_COMMITMENT_LEN],
        };
        assert_eq!(
            decode_transparent_utxo_set_commitment(&wire),
            Err(TransparentUtxoSetCommitmentDecodeError::UnknownScheme { scheme: 99 })
        );
    }

    #[test]
    fn decode_rejects_short_accumulator() {
        let wire = WireTransparentUtxoSetCommitment {
            scheme: WireUtxoSetCommitmentScheme::Lthash16 as i32,
            commitment: vec![0u8; 16],
        };
        assert_eq!(
            decode_transparent_utxo_set_commitment(&wire),
            Err(
                TransparentUtxoSetCommitmentDecodeError::InvalidAccumulatorLength {
                    expected: UTXO_SET_COMMITMENT_LEN,
                    actual: 16
                }
            )
        );
    }
}
