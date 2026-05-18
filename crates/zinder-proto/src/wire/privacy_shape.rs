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
}
