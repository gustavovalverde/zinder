//! Strict wire shape for canonical construction-manifest bindings.
//!
//! This module validates only protocol representation. Every decoded version
//! remains a structural claim: it neither establishes semantic compatibility
//! nor construction identity. A composition root must exact-compare the
//! version and digest with its admitted canonical authority and reject a
//! mismatch.

use thiserror::Error;

use crate::v1::ops::CanonicalConstructionManifestBinding;

const CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES: usize = 32;

/// Structurally representable primitive fields carried by a binding.
///
/// These fields do not establish that their version or digest matches an
/// admitted canonical authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalConstructionManifestBindingFields {
    format_version: u16,
    sha256: [u8; CANONICAL_CONSTRUCTION_MANIFEST_SHA256_BYTES],
}

impl CanonicalConstructionManifestBindingFields {
    /// Creates one structurally representable set of binding fields.
    ///
    /// This does not semantically admit the binding for a composed service.
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

/// Encodes structurally representable binding fields as the shared message.
///
/// Encoding does not establish semantic compatibility with a canonical
/// authority.
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
///
/// Decoding validates representation only; composition roots make the exact
/// authority comparison that admits or rejects the construction identity.
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
    fn structurally_representable_binding_round_trips_without_authority_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let fields = CanonicalConstructionManifestBindingFields::new(4, [0x5a; 32]);
        let encoded = encode_canonical_construction_manifest_binding(fields);
        assert_eq!(
            decode_canonical_construction_manifest_binding(&encoded)?,
            fields
        );
        Ok(())
    }

    #[test]
    fn binding_rejects_version_outside_structural_range() {
        let message = CanonicalConstructionManifestBinding {
            format_version: u32::from(u16::MAX) + 1,
            sha256: vec![0; 32],
        };
        assert!(matches!(
            decode_canonical_construction_manifest_binding(&message),
            Err(CanonicalConstructionManifestBindingDecodeError::FormatVersionOutOfRange { .. })
        ));
    }

    #[test]
    fn binding_rejects_digest_outside_sha256_wire_shape() {
        let message = CanonicalConstructionManifestBinding {
            format_version: 4,
            sha256: vec![0; 31],
        };
        assert!(matches!(
            decode_canonical_construction_manifest_binding(&message),
            Err(CanonicalConstructionManifestBindingDecodeError::WrongDigestLength { .. })
        ));
    }
}
