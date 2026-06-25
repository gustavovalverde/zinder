//! Transparent-delta-kind wire codec.
//!
//! Owns the one-byte storage discriminant that orders received and spent
//! events under a transparent-address key and its translation to the
//! generated `ExplorerQuery` wire enum. The byte ordering (`RECEIVED` before
//! `SPENT`) places a receive before a spend at the same transaction position,
//! the canonical order the per-event series emits.

use crate::v1::explorer::TransparentDeltaKind as WireTransparentDeltaKind;

/// Storage discriminant for a received-output event.
pub const TRANSPARENT_DELTA_KIND_RECEIVED_BYTE: u8 = 0;

/// Storage discriminant for a spent-prevout event.
pub const TRANSPARENT_DELTA_KIND_SPENT_BYTE: u8 = 1;

/// Error returned when a persisted delta-kind byte is outside the known set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UnknownTransparentDeltaKindByte {
    /// The byte that did not map to a known kind.
    pub byte: u8,
}

impl std::fmt::Display for UnknownTransparentDeltaKindByte {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unknown transparent delta kind byte: {}",
            self.byte
        )
    }
}

impl std::error::Error for UnknownTransparentDeltaKindByte {}

/// Translates a persisted delta-kind byte into the wire enum.
///
/// # Errors
///
/// Returns [`UnknownTransparentDeltaKindByte`] when `byte` is neither
/// [`TRANSPARENT_DELTA_KIND_RECEIVED_BYTE`] nor
/// [`TRANSPARENT_DELTA_KIND_SPENT_BYTE`].
pub const fn decode_transparent_delta_kind(
    byte: u8,
) -> Result<WireTransparentDeltaKind, UnknownTransparentDeltaKindByte> {
    match byte {
        TRANSPARENT_DELTA_KIND_RECEIVED_BYTE => Ok(WireTransparentDeltaKind::Received),
        TRANSPARENT_DELTA_KIND_SPENT_BYTE => Ok(WireTransparentDeltaKind::Spent),
        other => Err(UnknownTransparentDeltaKindByte { byte: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_byte_maps_to_wire_received() {
        assert_eq!(
            decode_transparent_delta_kind(TRANSPARENT_DELTA_KIND_RECEIVED_BYTE),
            Ok(WireTransparentDeltaKind::Received)
        );
    }

    #[test]
    fn spent_byte_maps_to_wire_spent() {
        assert_eq!(
            decode_transparent_delta_kind(TRANSPARENT_DELTA_KIND_SPENT_BYTE),
            Ok(WireTransparentDeltaKind::Spent)
        );
    }

    #[test]
    fn unknown_byte_is_rejected() {
        assert_eq!(
            decode_transparent_delta_kind(9),
            Err(UnknownTransparentDeltaKindByte { byte: 9 })
        );
    }
}
