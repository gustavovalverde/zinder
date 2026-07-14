use zinder_core::{
    BlockHeight, BlockId, CanonicalHistoryBounds,
    wire::{decode_internal_block_hash, encode_internal_block_hash},
};

use crate::{ArtifactFamily, StoreError};

use super::StoreKey;

const FORMAT_VERSION: u8 = 1;
const COMPLETE_KIND: u8 = 0;
const CHECKPOINTED_KIND: u8 = 1;
const COMPLETE_LEN: usize = 2;
const CHECKPOINTED_LEN: usize = 38;

pub(crate) fn encode_canonical_history_bounds(bounds: CanonicalHistoryBounds) -> Vec<u8> {
    bounds.preceding_checkpoint().map_or_else(
        || vec![FORMAT_VERSION, COMPLETE_KIND],
        |checkpoint| {
            let mut bytes = Vec::with_capacity(CHECKPOINTED_LEN);
            bytes.extend_from_slice(&[FORMAT_VERSION, CHECKPOINTED_KIND]);
            bytes.extend_from_slice(&checkpoint.height.value().to_be_bytes());
            bytes.extend_from_slice(&encode_internal_block_hash(checkpoint.hash));
            bytes
        },
    )
}

pub(crate) fn decode_canonical_history_bounds(
    bytes: &[u8],
) -> Result<CanonicalHistoryBounds, StoreError> {
    let corrupt = |reason| StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEpoch,
        key: StoreKey::canonical_history_bounds().into(),
        reason,
    };
    let Some((&version, remainder)) = bytes.split_first() else {
        return Err(corrupt("canonical history bounds must include a version"));
    };
    if version != FORMAT_VERSION {
        return Err(corrupt("canonical history bounds has an unknown version"));
    }
    let Some((&kind, payload)) = remainder.split_first() else {
        return Err(corrupt("canonical history bounds must include a kind"));
    };
    match kind {
        COMPLETE_KIND if bytes.len() == COMPLETE_LEN => Ok(CanonicalHistoryBounds::complete()),
        COMPLETE_KIND => Err(corrupt("complete canonical history bounds must be 2 bytes")),
        CHECKPOINTED_KIND if bytes.len() == CHECKPOINTED_LEN => {
            let height_bytes: [u8; 4] = payload[..4]
                .try_into()
                .map_err(|_| corrupt("checkpoint height must be 4 bytes"))?;
            let height = BlockHeight::new(u32::from_be_bytes(height_bytes));
            let hash = decode_internal_block_hash(&payload[4..])
                .map_err(|_| corrupt("checkpoint hash must be 32 bytes"))?;
            CanonicalHistoryBounds::checkpointed(BlockId::new(height, hash))
                .map_err(|_| corrupt("checkpoint height must have a successor"))
        }
        CHECKPOINTED_KIND => Err(corrupt(
            "checkpointed canonical history bounds must be 38 bytes",
        )),
        _ => Err(corrupt("canonical history bounds has an unknown kind")),
    }
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHash, BlockHeight, BlockId};

    use super::*;

    #[test]
    fn canonical_history_bounds_codec_round_trips_supported_variants()
    -> Result<(), Box<dyn std::error::Error>> {
        let checkpointed = CanonicalHistoryBounds::checkpointed(BlockId::new(
            BlockHeight::new(42),
            BlockHash::from_bytes([7; 32]),
        ))?;
        for bounds in [CanonicalHistoryBounds::complete(), checkpointed] {
            let encoded = encode_canonical_history_bounds(bounds);
            assert_eq!(decode_canonical_history_bounds(&encoded).ok(), Some(bounds));
        }
        Ok(())
    }

    #[test]
    fn canonical_history_bounds_codec_rejects_malformed_values() {
        let mut max_checkpoint = vec![FORMAT_VERSION, CHECKPOINTED_KIND];
        max_checkpoint.extend_from_slice(&u32::MAX.to_be_bytes());
        max_checkpoint.extend_from_slice(&[0; 32]);
        for malformed in [
            vec![],
            vec![FORMAT_VERSION],
            vec![2, COMPLETE_KIND],
            vec![FORMAT_VERSION, 9],
            vec![FORMAT_VERSION, COMPLETE_KIND, 0],
            vec![FORMAT_VERSION, CHECKPOINTED_KIND],
            max_checkpoint,
        ] {
            assert!(matches!(
                decode_canonical_history_bounds(&malformed),
                Err(StoreError::ArtifactCorrupt { .. })
            ));
        }
    }
}
