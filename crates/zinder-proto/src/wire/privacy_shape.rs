//! Privacy-shape wire codec.
//!
//! Maps the pure-domain [`zinder_core::PrivacyShape`] classifier output to
//! the generated wire enum on `ExplorerQuery` responses. The mapping is
//! a constant table: every native variant has exactly one wire counterpart
//! and vice versa.

use zinder_core::PrivacyShape;

use crate::v1::explorer::PrivacyShape as WirePrivacyShape;

/// Translates a native privacy-shape classifier value into the wire enum.
///
/// Total: every [`PrivacyShape`] variant maps to a [`WirePrivacyShape`]
/// variant, so callers can assign the result with `as i32` without a
/// fallback arm.
#[must_use]
pub const fn encode_privacy_shape(shape: PrivacyShape) -> WirePrivacyShape {
    match shape {
        PrivacyShape::TransparentOnly => WirePrivacyShape::TransparentOnly,
        PrivacyShape::Shielding => WirePrivacyShape::Shielding,
        PrivacyShape::Deshielding => WirePrivacyShape::Deshielding,
        PrivacyShape::ShieldedOnly => WirePrivacyShape::ShieldedOnly,
        PrivacyShape::Mixed => WirePrivacyShape::Mixed,
        PrivacyShape::Coinbase => WirePrivacyShape::Coinbase,
        PrivacyShape::ShieldedCoinbase => WirePrivacyShape::ShieldedCoinbase,
        PrivacyShape::Unclassified => WirePrivacyShape::Unclassified,
    }
}

/// Translates a generated privacy-shape enum value into the native classifier.
///
/// Returns `None` for the wire-only `UNSPECIFIED` marker and unknown numeric
/// values. Callers must preserve that absence rather than inventing a privacy
/// classification.
#[must_use]
pub fn decode_privacy_shape(wire_privacy_shape: i32) -> Option<PrivacyShape> {
    match WirePrivacyShape::try_from(wire_privacy_shape).ok()? {
        WirePrivacyShape::TransparentOnly => Some(PrivacyShape::TransparentOnly),
        WirePrivacyShape::Shielding => Some(PrivacyShape::Shielding),
        WirePrivacyShape::Deshielding => Some(PrivacyShape::Deshielding),
        WirePrivacyShape::ShieldedOnly => Some(PrivacyShape::ShieldedOnly),
        WirePrivacyShape::Mixed => Some(PrivacyShape::Mixed),
        WirePrivacyShape::Coinbase => Some(PrivacyShape::Coinbase),
        WirePrivacyShape::ShieldedCoinbase => Some(PrivacyShape::ShieldedCoinbase),
        WirePrivacyShape::Unclassified => Some(PrivacyShape::Unclassified),
        WirePrivacyShape::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_only_maps_to_wire_transparent_only() {
        assert_eq!(
            encode_privacy_shape(PrivacyShape::TransparentOnly),
            WirePrivacyShape::TransparentOnly
        );
    }

    #[test]
    fn unclassified_maps_to_wire_unclassified() {
        assert_eq!(
            encode_privacy_shape(PrivacyShape::Unclassified),
            WirePrivacyShape::Unclassified
        );
    }

    #[test]
    fn shielded_coinbase_maps_to_wire_shielded_coinbase() {
        assert_eq!(
            encode_privacy_shape(PrivacyShape::ShieldedCoinbase),
            WirePrivacyShape::ShieldedCoinbase
        );
    }

    #[test]
    fn transparent_only_decodes_from_wire() {
        assert_eq!(
            decode_privacy_shape(WirePrivacyShape::TransparentOnly as i32),
            Some(PrivacyShape::TransparentOnly)
        );
    }

    #[test]
    fn unspecified_and_unknown_values_do_not_invent_a_shape() {
        assert_eq!(
            decode_privacy_shape(WirePrivacyShape::Unspecified as i32),
            None
        );
        assert_eq!(decode_privacy_shape(i32::MAX), None);
    }
}
