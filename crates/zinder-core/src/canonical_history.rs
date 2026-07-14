//! Durable bounds of the canonical history retained by a store.

use thiserror::Error;

use crate::{BlockHeight, BlockId};

/// Durable boundary between intentionally omitted and retained canonical history.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalHistoryBounds {
    preceding_checkpoint: Option<BlockId>,
}

impl CanonicalHistoryBounds {
    /// Returns bounds for a store built without intentional history truncation.
    #[must_use]
    pub const fn complete() -> Self {
        Self {
            preceding_checkpoint: None,
        }
    }

    /// Returns bounds for history beginning immediately after `checkpoint`.
    pub fn checkpointed(checkpoint: BlockId) -> Result<Self, CanonicalHistoryBoundsError> {
        checkpoint
            .height
            .next()
            .ok_or(CanonicalHistoryBoundsError::CheckpointHasNoSuccessor)?;
        Ok(Self {
            preceding_checkpoint: Some(checkpoint),
        })
    }

    /// Returns the first height at which canonical artifacts are expected.
    #[must_use]
    pub const fn first_available_height(self) -> BlockHeight {
        match self.preceding_checkpoint {
            None => BlockHeight::new(1),
            Some(checkpoint) => {
                // `checkpointed` rejects `u32::MAX`, so this branch is unreachable.
                BlockHeight::new(checkpoint.height.value() + 1)
            }
        }
    }

    /// Returns the checkpoint immediately preceding available history, when present.
    #[must_use]
    pub const fn preceding_checkpoint(self) -> Option<BlockId> {
        self.preceding_checkpoint
    }

    /// Returns whether `height` is absent because of an intentional checkpoint bootstrap.
    #[must_use]
    pub const fn intentionally_excludes(self, height: BlockHeight) -> bool {
        match self.preceding_checkpoint {
            None => false,
            Some(_) => height.value() < self.first_available_height().value(),
        }
    }
}

/// Invalid canonical-history bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CanonicalHistoryBoundsError {
    /// A checkpoint at `u32::MAX` cannot have retained history after it.
    #[error("canonical history checkpoint has no successor height")]
    CheckpointHasNoSuccessor,
}
