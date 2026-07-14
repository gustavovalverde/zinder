use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalHistoryBounds, CanonicalHistoryBoundsError,
};

#[test]
fn complete_history_starts_at_height_one() {
    let bounds = CanonicalHistoryBounds::complete();

    assert_eq!(bounds.first_available_height(), BlockHeight::new(1));
    assert_eq!(bounds.preceding_checkpoint(), None);
    assert!(!bounds.intentionally_excludes(BlockHeight::new(0)));
}

#[test]
fn checkpointed_history_starts_after_the_checkpoint() -> Result<(), CanonicalHistoryBoundsError> {
    let checkpoint = BlockId::new(BlockHeight::new(20), BlockHash::from_bytes([7; 32]));
    let bounds = CanonicalHistoryBounds::checkpointed(checkpoint)?;

    assert_eq!(bounds.first_available_height(), BlockHeight::new(21));
    assert_eq!(bounds.preceding_checkpoint(), Some(checkpoint));
    assert!(bounds.intentionally_excludes(BlockHeight::new(20)));
    assert!(!bounds.intentionally_excludes(BlockHeight::new(21)));

    Ok(())
}

#[test]
fn checkpointed_history_rejects_a_checkpoint_without_a_successor() {
    let checkpoint = BlockId::new(BlockHeight::new(u32::MAX), BlockHash::from_bytes([9; 32]));

    assert_eq!(
        CanonicalHistoryBounds::checkpointed(checkpoint),
        Err(CanonicalHistoryBoundsError::CheckpointHasNoSuccessor)
    );
}
