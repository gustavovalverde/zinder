//! Post-commit chain event payload values.

use zinder_core::{BlockHeight, BlockHeightRange, ChainEpoch};

use crate::StreamCursorTokenV1;

/// Result returned after a chain epoch commit becomes durable and visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEpochCommitOutcome {
    /// Chain epoch committed by the storage operation.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range included in the commit.
    pub block_range: BlockHeightRange,
    /// Post-commit event produced by the durable transition.
    pub event: ChainEvent,
    /// Cursor-bound event persisted for resumable consumers.
    pub event_envelope: ChainEventEnvelope,
}

impl ChainEpochCommitOutcome {
    pub(crate) fn new(committed: ChainEpochCommitted, event_envelope: ChainEventEnvelope) -> Self {
        let event = event_envelope.event.clone();
        Self {
            chain_epoch: committed.chain_epoch,
            block_range: committed.block_range,
            event,
            event_envelope,
        }
    }
}

/// Cursor-bound chain event returned to resumable consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEventEnvelope {
    /// Opaque cursor for resuming strictly after this event.
    pub cursor: StreamCursorTokenV1,
    /// Monotonic sequence in the chain-event stream.
    pub event_sequence: u64,
    /// Chain epoch visible after this event.
    pub chain_epoch: ChainEpoch,
    /// Settled tip height that was true for this event.
    pub settled_tip_height: BlockHeight,
    /// Post-commit canonical transition.
    pub event: ChainEvent,
}

impl ChainEventEnvelope {
    pub(crate) const fn new(
        cursor: StreamCursorTokenV1,
        event_sequence: u64,
        chain_epoch: ChainEpoch,
        settled_tip_height: BlockHeight,
        event: ChainEvent,
    ) -> Self {
        Self {
            cursor,
            event_sequence,
            chain_epoch,
            settled_tip_height,
            event,
        }
    }
}

/// Post-commit canonical chain transition.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// A non-reorg commit advanced the canonical tip or settled-tip prefix.
    ChainCommitted {
        /// Committed epoch payload.
        committed: ChainEpochCommitted,
    },
    /// A range that had not yet reached the settled tip was replaced by a new committed range.
    ChainReorged {
        /// Previously visible range invalidated by this transition.
        reverted: ChainRangeReverted,
        /// Replacement range committed by this transition.
        committed: ChainEpochCommitted,
    },
}

impl ChainEvent {
    /// Returns `true` when this event is the [`Self::ChainCommitted`] variant.
    ///
    /// Convenience for consumers that need a single boolean for tip-change
    /// notifications without `match`-ing on every variant.
    #[must_use]
    pub const fn is_chain_committed(&self) -> bool {
        matches!(self, Self::ChainCommitted { .. })
    }
}

/// Event payload for a previously visible range that had not yet reached the settled tip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainRangeReverted {
    /// Chain epoch that contained the reverted range.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range invalidated by this transition.
    pub block_range: BlockHeightRange,
}

impl ChainRangeReverted {
    pub(crate) const fn new(chain_epoch: ChainEpoch, block_range: BlockHeightRange) -> Self {
        Self {
            chain_epoch,
            block_range,
        }
    }
}

/// Event payload returned after a chain epoch becomes durable and visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainEpochCommitted {
    /// Chain epoch committed by the storage operation.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range included in the commit.
    pub block_range: BlockHeightRange,
}

impl ChainEpochCommitted {
    pub(crate) const fn new(chain_epoch: ChainEpoch, block_range: BlockHeightRange) -> Self {
        Self {
            chain_epoch,
            block_range,
        }
    }
}
