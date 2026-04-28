//! Artifact envelope byte contract.

use thiserror::Error;

/// Size in bytes of an [`ArtifactEnvelopeHeaderV1`].
pub(crate) const ARTIFACT_ENVELOPE_HEADER_LEN: usize = 16;

const ARTIFACT_ENVELOPE_MAGIC: [u8; 4] = *b"ZAE1";
const ARTIFACT_ENVELOPE_VERSION: u8 = 1;

/// Payload format stored inside an artifact envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PayloadFormat {
    /// Zinder protobuf payload for a block artifact.
    ZinderBlockArtifactV1 = 1,
    /// Zinder protobuf payload for a compact block artifact.
    ZinderCompactBlockArtifactV1 = 2,
    /// Zinder protobuf payload for a transaction artifact.
    ZinderTransactionArtifactV1 = 3,
    /// Zinder protobuf payload for a tree-state artifact.
    ZinderTreeStateArtifactV1 = 4,
    /// Zinder protobuf payload for a subtree-root artifact.
    ZinderSubtreeRootArtifactV1 = 5,
    /// Zinder protobuf payload for a transparent address UTXO artifact.
    ZinderTransparentAddressUtxoArtifactV1 = 6,
    /// Zinder protobuf payload for a transparent UTXO spend artifact.
    ZinderTransparentUtxoSpendArtifactV1 = 7,
}

impl PayloadFormat {
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::ZinderBlockArtifactV1),
            2 => Some(Self::ZinderCompactBlockArtifactV1),
            3 => Some(Self::ZinderTransactionArtifactV1),
            4 => Some(Self::ZinderTreeStateArtifactV1),
            5 => Some(Self::ZinderSubtreeRootArtifactV1),
            6 => Some(Self::ZinderTransparentAddressUtxoArtifactV1),
            7 => Some(Self::ZinderTransparentUtxoSpendArtifactV1),
            _ => None,
        }
    }
}

/// Compression format applied to an artifact payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum CompressionFormat {
    /// Payload is stored without compression.
    None = 0,
}

impl CompressionFormat {
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            _ => None,
        }
    }
}

/// Checksum format applied to an artifact payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ChecksumFormat {
    /// Payload checksum is not present.
    None = 0,
}

impl ChecksumFormat {
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            _ => None,
        }
    }
}

/// Fixed artifact envelope header stored before artifact payload bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactEnvelopeHeaderV1 {
    /// Format of the payload bytes that follow this header.
    pub(crate) payload_format: PayloadFormat,
    /// Compression format used by the payload bytes.
    pub(crate) compression_format: CompressionFormat,
    /// Checksum format used by this envelope.
    pub(crate) checksum_format: ChecksumFormat,
    /// Payload length in bytes.
    pub(crate) payload_len: u32,
    /// Payload length after decompression.
    pub(crate) uncompressed_len: u32,
}

impl ArtifactEnvelopeHeaderV1 {
    /// Creates a header for an uncompressed payload.
    pub(crate) fn for_uncompressed_payload(
        payload_format: PayloadFormat,
        payload_len: usize,
    ) -> Result<Self, ArtifactEnvelopeError> {
        let payload_len = u32::try_from(payload_len)
            .map_err(|_| ArtifactEnvelopeError::PayloadTooLarge { payload_len })?;

        Ok(Self {
            payload_format,
            compression_format: CompressionFormat::None,
            checksum_format: ChecksumFormat::None,
            payload_len,
            uncompressed_len: payload_len,
        })
    }

    /// Encodes a payload as `ArtifactEnvelopeHeaderV1 || payload`.
    pub(crate) fn encode_payload(
        payload_format: PayloadFormat,
        payload_bytes: &[u8],
    ) -> Result<Vec<u8>, ArtifactEnvelopeError> {
        let header = Self::for_uncompressed_payload(payload_format, payload_bytes.len())?;
        let mut envelope = Vec::with_capacity(ARTIFACT_ENVELOPE_HEADER_LEN + payload_bytes.len());
        header.write_to(&mut envelope);
        envelope.extend_from_slice(payload_bytes);
        Ok(envelope)
    }

    /// Decodes and validates an envelope, returning the payload bytes.
    pub(crate) fn decode_payload(
        envelope_bytes: &[u8],
        expected_payload_format: PayloadFormat,
    ) -> Result<&[u8], ArtifactEnvelopeError> {
        let header = Self::decode_header(envelope_bytes)?;

        if header.payload_format != expected_payload_format {
            return Err(ArtifactEnvelopeError::PayloadFormatMismatch {
                expected: expected_payload_format,
                actual: header.payload_format,
            });
        }

        let payload_bytes = &envelope_bytes[ARTIFACT_ENVELOPE_HEADER_LEN..];
        let actual_payload_len = payload_bytes.len();
        let expected_payload_len = header.payload_len as usize;

        if actual_payload_len != expected_payload_len {
            return Err(ArtifactEnvelopeError::PayloadLengthMismatch {
                expected: expected_payload_len,
                actual: actual_payload_len,
            });
        }

        Ok(payload_bytes)
    }

    fn decode_header(envelope_bytes: &[u8]) -> Result<Self, ArtifactEnvelopeError> {
        if envelope_bytes.len() < ARTIFACT_ENVELOPE_HEADER_LEN {
            return Err(ArtifactEnvelopeError::EnvelopeTooShort {
                actual: envelope_bytes.len(),
            });
        }

        if envelope_bytes[0..4] != ARTIFACT_ENVELOPE_MAGIC {
            return Err(ArtifactEnvelopeError::UnsupportedMagic);
        }

        if envelope_bytes[4] != ARTIFACT_ENVELOPE_VERSION {
            return Err(ArtifactEnvelopeError::UnsupportedEnvelopeVersion {
                version: envelope_bytes[4],
            });
        }

        let payload_format = PayloadFormat::from_byte(envelope_bytes[5]).ok_or(
            ArtifactEnvelopeError::UnsupportedPayloadFormat {
                format: envelope_bytes[5],
            },
        )?;
        let compression_format = CompressionFormat::from_byte(envelope_bytes[6]).ok_or(
            ArtifactEnvelopeError::UnsupportedCompressionFormat {
                format: envelope_bytes[6],
            },
        )?;
        let checksum_format = ChecksumFormat::from_byte(envelope_bytes[7]).ok_or(
            ArtifactEnvelopeError::UnsupportedChecksumFormat {
                format: envelope_bytes[7],
            },
        )?;

        let payload_len = read_u32_be(envelope_bytes, 8)?;
        let uncompressed_len = read_u32_be(envelope_bytes, 12)?;

        Ok(Self {
            payload_format,
            compression_format,
            checksum_format,
            payload_len,
            uncompressed_len,
        })
    }

    fn write_to(&self, envelope: &mut Vec<u8>) {
        envelope.extend_from_slice(&ARTIFACT_ENVELOPE_MAGIC);
        envelope.push(ARTIFACT_ENVELOPE_VERSION);
        envelope.push(self.payload_format as u8);
        envelope.push(self.compression_format as u8);
        envelope.push(self.checksum_format as u8);
        envelope.extend_from_slice(&self.payload_len.to_be_bytes());
        envelope.extend_from_slice(&self.uncompressed_len.to_be_bytes());
    }
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32, ArtifactEnvelopeError> {
    let mut value_bytes = [0; 4];
    value_bytes.copy_from_slice(
        bytes
            .get(offset..offset + 4)
            .ok_or(ArtifactEnvelopeError::HeaderFieldTruncated { offset })?,
    );
    Ok(u32::from_be_bytes(value_bytes))
}

/// Error returned while encoding or decoding artifact envelopes.
#[derive(Debug, Error)]
pub(crate) enum ArtifactEnvelopeError {
    /// Envelope has fewer bytes than [`ARTIFACT_ENVELOPE_HEADER_LEN`].
    #[error("artifact envelope is too short: {actual} bytes")]
    EnvelopeTooShort {
        /// Actual byte length.
        actual: usize,
    },

    /// A fixed-width header field was not fully present.
    #[error("artifact envelope header field at offset {offset} is truncated")]
    HeaderFieldTruncated {
        /// Field offset inside the v1 header.
        offset: usize,
    },

    /// Envelope magic does not match the Zinder artifact envelope magic.
    #[error("artifact envelope has unsupported magic")]
    UnsupportedMagic,

    /// Envelope version is not supported by this parser.
    #[error("artifact envelope version {version} is unsupported")]
    UnsupportedEnvelopeVersion {
        /// Unsupported envelope version.
        version: u8,
    },

    /// Payload format byte is not supported by this parser.
    #[error("payload format {format} is unsupported")]
    UnsupportedPayloadFormat {
        /// Unsupported payload format byte.
        format: u8,
    },

    /// Payload format differs from the caller's expected format.
    #[error("payload format mismatch: expected {expected:?}, actual {actual:?}")]
    PayloadFormatMismatch {
        /// Expected payload format.
        expected: PayloadFormat,
        /// Actual payload format.
        actual: PayloadFormat,
    },

    /// Compression format byte is not supported by this parser.
    #[error("compression format {format} is unsupported")]
    UnsupportedCompressionFormat {
        /// Unsupported compression format byte.
        format: u8,
    },

    /// Checksum format byte is not supported by this parser.
    #[error("checksum format {format} is unsupported")]
    UnsupportedChecksumFormat {
        /// Unsupported checksum format byte.
        format: u8,
    },

    /// Payload length in the header does not match the envelope body.
    #[error("payload length mismatch: expected {expected}, actual {actual}")]
    PayloadLengthMismatch {
        /// Expected payload byte length from the header.
        expected: usize,
        /// Actual payload byte length in the envelope.
        actual: usize,
    },

    /// Payload is too large for the v1 envelope length field.
    #[error("payload is too large: {payload_len} bytes")]
    PayloadTooLarge {
        /// Payload byte length.
        payload_len: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        ARTIFACT_ENVELOPE_HEADER_LEN, ArtifactEnvelopeError, ArtifactEnvelopeHeaderV1,
        PayloadFormat,
    };

    #[test]
    fn encoded_payload_uses_compact_v1_header_without_checksum_padding()
    -> Result<(), ArtifactEnvelopeError> {
        let payload_bytes = b"compact payload";
        let envelope_bytes = ArtifactEnvelopeHeaderV1::encode_payload(
            PayloadFormat::ZinderCompactBlockArtifactV1,
            payload_bytes,
        )?;

        assert_eq!(ARTIFACT_ENVELOPE_HEADER_LEN, 16);
        assert_eq!(
            envelope_bytes.len(),
            ARTIFACT_ENVELOPE_HEADER_LEN + payload_bytes.len()
        );
        let payload_len = u32::try_from(payload_bytes.len()).map_err(|_| {
            ArtifactEnvelopeError::PayloadTooLarge {
                payload_len: payload_bytes.len(),
            }
        })?;
        assert_eq!(&envelope_bytes[0..4], b"ZAE1");
        assert_eq!(envelope_bytes[4], 1);
        assert_eq!(
            envelope_bytes[5],
            PayloadFormat::ZinderCompactBlockArtifactV1 as u8
        );
        assert_eq!(envelope_bytes[6], 0);
        assert_eq!(envelope_bytes[7], 0);
        assert_eq!(&envelope_bytes[8..12], &payload_len.to_be_bytes());
        assert_eq!(&envelope_bytes[12..16], &payload_len.to_be_bytes());
        assert_eq!(
            ArtifactEnvelopeHeaderV1::decode_payload(
                &envelope_bytes,
                PayloadFormat::ZinderCompactBlockArtifactV1,
            )?,
            payload_bytes,
        );

        Ok(())
    }

    #[test]
    fn decode_rejects_unsupported_checksum_format_without_reading_checksum_bytes()
    -> Result<(), ArtifactEnvelopeError> {
        let mut envelope_bytes = ArtifactEnvelopeHeaderV1::encode_payload(
            PayloadFormat::ZinderBlockArtifactV1,
            b"payload",
        )?;
        envelope_bytes[7] = 1;

        assert!(matches!(
            ArtifactEnvelopeHeaderV1::decode_payload(
                &envelope_bytes,
                PayloadFormat::ZinderBlockArtifactV1,
            ),
            Err(ArtifactEnvelopeError::UnsupportedChecksumFormat { format: 1 })
        ));

        Ok(())
    }
}
