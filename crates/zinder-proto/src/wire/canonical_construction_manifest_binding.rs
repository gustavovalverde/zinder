//! Strict wire shape for canonical construction-manifest bindings.
//!
//! This module validates only protocol representation. The returned fields
//! remain a structural claim until a composition root exact-compares them with
//! an admitted canonical reader.

use thiserror::Error;

use crate::v1::ops::CanonicalConstructionManifestBinding;

const CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES: usize = 32;

/// Validated primitive fields carried by a construction-manifest binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalConstructionManifestBindingFields {
    format_version: u16,
    sha256: [u8; CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES],
}

impl CanonicalConstructionManifestBindingFields {
    /// Creates one structurally valid set of binding fields.
    #[must_use]
    pub const fn new(
        format_version: u16,
        sha256: [u8; CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES],
    ) -> Self {
        Self {
            format_version,
            sha256,
        }
    }

    /// Returns the exact construction-manifest format version.
    #[must_use]
    pub const fn format_version(self) -> u16 {
        self.format_version
    }

    /// Returns the exact construction-manifest digest.
    #[must_use]
    pub const fn sha256(self) -> [u8; CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES] {
        self.sha256
    }
}

/// Structural failure decoding a construction-manifest binding message.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CanonicalConstructionManifestBindingDecodeError {
    /// The protobuf integer cannot be represented by the canonical contract.
    #[error("canonical construction manifest format version {observed} exceeds u16")]
    FormatVersionOutOfRange {
        /// Value carried by the wire message.
        observed: u32,
    },
    /// The digest is not exactly one SHA-256 value.
    #[error(
        "canonical construction manifest digest requires {expected} bytes; observed {observed}"
    )]
    WrongDigestLength {
        /// Exact supported digest length.
        expected: usize,
        /// Wire payload length.
        observed: usize,
    },
}

/// Encodes validated primitive binding fields as the shared protocol message.
#[must_use]
pub fn encode_canonical_construction_manifest_binding(
    fields: CanonicalConstructionManifestBindingFields,
) -> CanonicalConstructionManifestBinding {
    CanonicalConstructionManifestBinding {
        format_version: u32::from(fields.format_version),
        sha256: fields.sha256.to_vec(),
    }
}

/// Strictly decodes the shared binding message into proto-local primitives.
pub fn decode_canonical_construction_manifest_binding(
    message: &CanonicalConstructionManifestBinding,
) -> Result<
    CanonicalConstructionManifestBindingFields,
    CanonicalConstructionManifestBindingDecodeError,
> {
    let format_version = u16::try_from(message.format_version).map_err(|_| {
        CanonicalConstructionManifestBindingDecodeError::FormatVersionOutOfRange {
            observed: message.format_version,
        }
    })?;
    let sha256: [u8; CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES] =
        message.sha256.as_slice().try_into().map_err(|_| {
            CanonicalConstructionManifestBindingDecodeError::WrongDigestLength {
                expected: CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES,
                observed: message.sha256.len(),
            }
        })?;
    Ok(CanonicalConstructionManifestBindingFields::new(
        format_version,
        sha256,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_round_trips_exact_fields() -> Result<(), Box<dyn std::error::Error>> {
        let fields = CanonicalConstructionManifestBindingFields::new(1, [0x5a; 32]);
        let encoded = encode_canonical_construction_manifest_binding(fields);
        assert_eq!(
            decode_canonical_construction_manifest_binding(&encoded)?,
            fields
        );
        Ok(())
    }

    #[test]
    fn binding_rejects_out_of_range_version() {
        let message = CanonicalConstructionManifestBinding {
            format_version: u32::from(u16::MAX) + 1,
            sha256: vec![0; 32],
        };
        assert!(matches!(
            decode_canonical_construction_manifest_binding(&message),
            Err(
                CanonicalConstructionManifestBindingDecodeError::FormatVersionOutOfRange { .. }
            )
        ));
    }

    #[test]
    fn binding_rejects_non_sha256_digest() {
        let message = CanonicalConstructionManifestBinding {
            format_version: 1,
            sha256: vec![0; 31],
        };
        assert!(matches!(
            decode_canonical_construction_manifest_binding(&message),
            Err(CanonicalConstructionManifestBindingDecodeError::WrongDigestLength { .. })
        ));
    }
}
