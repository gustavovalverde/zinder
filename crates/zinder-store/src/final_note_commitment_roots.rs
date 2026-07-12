//! Final note-commitment root artifact reads.

use zinder_core::{BlockFinalNoteCommitmentRoots, BlockHeight, BlockHeightRange, ChainEpoch};

use crate::{
    ArtifactFamily, StoreError,
    artifact_visibility::{HeightVisibilityIndex, visible_height_source_epoch},
    block_artifact::read_block_header_artifact,
    format::{StoreKey, decode_final_note_commitment_roots},
    kv::{RocksChainStoreRead, StorageTable},
};

/// Read boundary for final note-commitment roots associated with canonical blocks.
pub trait FinalNoteCommitmentRootsStore {
    /// Reads the final roots associated with one canonical block height.
    fn final_note_commitment_roots_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockFinalNoteCommitmentRoots>, StoreError>;

    /// Reads final roots for a bounded range in ascending height order.
    fn final_note_commitment_roots_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<Vec<Option<BlockFinalNoteCommitmentRoots>>, StoreError>;
}

pub(crate) fn read_final_note_commitment_roots(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Option<BlockFinalNoteCommitmentRoots>, StoreError> {
    if height > chain_epoch.visible_tip_height {
        return Ok(None);
    }

    let source_epoch = match visible_height_source_epoch(
        inner,
        chain_epoch,
        height,
        ArtifactFamily::FinalNoteCommitmentRoots,
        HeightVisibilityIndex::FinalNoteCommitmentRoots,
    ) {
        Ok(source_epoch) => source_epoch,
        Err(StoreError::ArtifactMissing { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let key = StoreKey::final_note_commitment_roots(chain_epoch.network, source_epoch, height);
    let Some(envelope_bytes) = inner.get(StorageTable::FinalNoteCommitmentRoots, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::FinalNoteCommitmentRoots,
            key: key.into(),
        });
    };
    let roots = decode_final_note_commitment_roots(&key, &envelope_bytes)?;
    if roots.height != height {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::FinalNoteCommitmentRoots,
            key: key.into(),
            reason: "final note-commitment roots height does not match requested height",
        });
    }

    let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? else {
        return Ok(None);
    };
    if block.block_hash != roots.block_hash {
        return Ok(None);
    }

    Ok(Some(roots))
}

pub(crate) fn read_final_note_commitment_roots_in_range(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    block_range: BlockHeightRange,
) -> Result<Vec<Option<BlockFinalNoteCommitmentRoots>>, StoreError> {
    block_range
        .into_iter()
        .map(|height| read_final_note_commitment_roots(inner, chain_epoch, height))
        .collect()
}
