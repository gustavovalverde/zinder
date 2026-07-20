//! Node source boundary values.

use std::num::NonZeroU32;

use async_trait::async_trait;
use zinder_core::{
    BlockHeight, BlockId, BlockValuePoolBalances, ChainValuePools, CommitmentTreeCheckpoint,
    NetworkUpgradeActivations, RawTransactionBytes, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange, TransactionBroadcastOutcome,
};

use crate::{
    NodeCapabilities, SourceBlock, SourceChainSegment, SourceChainSegmentLimits, SourceChainUpdate,
    SourceError, SourceSubtreeRoots, SourceTreeState, UpstreamHealthSnapshot,
    source_chain_update::SourceChainCursorPosition,
};

/// Configured upstream node source for ingestion.
#[async_trait]
pub trait NodeSource: Send + Sync + 'static {
    /// Returns the source capabilities discovered or declared at startup.
    fn capabilities(&self) -> NodeCapabilities;

    /// Fetches one block by height from the configured node.
    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError>;

    /// Fetches a bounded ordered segment after `cursor`.
    ///
    /// Sources that can batch or stream upstream observations override this
    /// method. The default implementation preserves the final
    /// [`SourceChainUpdate`] boundary by fetching connected blocks one at a
    /// time with [`Self::fetch_block_at`].
    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError> {
        let observed_tip_id = self.tip_id().await?;
        let (start_height, expected_parent_hash) = match limits.cursor.position() {
            SourceChainCursorPosition::BeforeHeight(height) => (height, None),
            SourceChainCursorPosition::AtBlock(block_id) => {
                if observed_tip_id.height < block_id.height
                    || (observed_tip_id.height == block_id.height
                        && observed_tip_id.hash != block_id.hash)
                {
                    return Ok(SourceChainSegment::new([
                        SourceChainUpdate::reverted_block(block_id),
                    ]));
                }
                if observed_tip_id.height == block_id.height {
                    return Ok(SourceChainSegment::default());
                }
                let Some(next_height) = block_id.height.next() else {
                    return Ok(SourceChainSegment::default());
                };
                (next_height, Some(block_id.hash))
            }
        };
        if observed_tip_id.height < start_height {
            return Ok(SourceChainSegment::default());
        }

        let end_height = bounded_segment_end_height(
            start_height,
            observed_tip_id.height,
            limits.max_connected_blocks,
        );
        let mut blocks = Vec::with_capacity(block_height_span_len(start_height, end_height));
        let mut next_height = Some(start_height);
        while let Some(height) = next_height {
            if height > end_height {
                break;
            }
            let block = self.fetch_block_at(height).await?;
            if let Some(expected_parent_hash) = expected_parent_hash
                && blocks.is_empty()
                && block.parent_hash != expected_parent_hash
            {
                let block_id = BlockId::new(
                    start_height
                        .value()
                        .checked_sub(1)
                        .map_or(start_height, BlockHeight::new),
                    expected_parent_hash,
                );
                return Ok(SourceChainSegment::new([
                    SourceChainUpdate::reverted_block(block_id),
                ]));
            }
            blocks.push(block);
            next_height = height.next();
        }

        Ok(SourceChainSegment::connected_blocks(blocks))
    }

    /// Fetches a tree-state payload for one already-identified block.
    async fn fetch_tree_state_for_block(
        &self,
        block_id: BlockId,
    ) -> Result<SourceTreeState, SourceError> {
        let _ = block_id;
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::TreeState,
        })
    }

    /// Fetches one source-authenticated checkpoint with validated typed frontiers.
    async fn fetch_chain_checkpoint(
        &self,
        height: BlockHeight,
        network_upgrade_activations: &NetworkUpgradeActivations,
    ) -> Result<CommitmentTreeCheckpoint, SourceError> {
        let _ = (height, network_upgrade_activations);
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::TreeState,
        })
    }

    /// Returns the node's current best tip identity (height and hash).
    ///
    /// Tip-follow uses [`BlockId::hash`] as the cheap change-detection signal:
    /// when the hash equals the stored tip hash, the chain has not advanced
    /// and the caller can skip fetching the full block.
    async fn tip_id(&self) -> Result<BlockId, SourceError>;

    /// Fetches shielded subtree roots from the configured node.
    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        let _ = (protocol, start_index, max_entries);
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::SubtreeRoots,
        })
    }

    /// Fetches every shielded subtree root in `range` with one source request.
    ///
    /// Unlike [`Self::fetch_subtree_roots`], this exact-range contract rejects
    /// a short response. Canonical construction uses it only after computing
    /// the completed-subtree range at a fixed chain tip, so accepting a partial
    /// response could publish an incomplete wallet artifact family.
    async fn fetch_subtree_root_range(
        &self,
        range: SubtreeRootRange,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        let _ = range;
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::SubtreeRoots,
        })
    }

    /// Fetches chain-wide value pool totals at the upstream tip.
    ///
    /// Returns [`SourceError::NodeCapabilityMissing`] when the source
    /// does not advertise [`crate::NodeCapability::ChainValuePools`].
    async fn fetch_chain_value_pools_at_tip(&self) -> Result<ChainValuePools, SourceError> {
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::ChainValuePools,
        })
    }

    /// Fetches authoritative cumulative value-pool balances for an exact block.
    ///
    /// Returns [`SourceError::NodeCapabilityMissing`] when this source does not
    /// expose block-bound cumulative value-pool balances.
    async fn fetch_block_value_pool_balances(
        &self,
        block_id: BlockId,
    ) -> Result<BlockValuePoolBalances, SourceError> {
        let _ = block_id;
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::BlockValuePoolBalances,
        })
    }

    /// Polls the upstream sync-health signal.
    ///
    /// Returns an [`UpstreamHealthSnapshot`] each call so the background
    /// probe task can drive the `cause=upstream_not_ready` readiness
    /// payload per
    /// [ADR-0015 §Upstream sync detection]. The default implementation
    /// returns [`SourceError::NodeCapabilityMissing`] so sources that
    /// have not opted in surface a typed failure rather than silently
    /// degrading.
    ///
    /// [ADR-0015 §Upstream sync detection]:
    ///     ../../../docs/adrs/0015-phase-driven-ingest.md#upstream-sync-detection
    async fn poll_upstream_health(&self) -> Result<UpstreamHealthSnapshot, SourceError> {
        Err(SourceError::NodeCapabilityMissing {
            capability: crate::NodeCapability::ReadinessProbe,
        })
    }
}

/// Node-backed transaction broadcast boundary.
#[async_trait]
pub trait TransactionBroadcaster: Send + Sync + 'static {
    /// Broadcasts a raw transaction to the configured node or network path.
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastOutcome, SourceError>;
}

#[async_trait]
impl TransactionBroadcaster for () {
    async fn broadcast_transaction(
        &self,
        _raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastOutcome, SourceError> {
        Err(SourceError::TransactionBroadcastDisabled)
    }
}

#[async_trait]
impl<T> TransactionBroadcaster for Option<T>
where
    T: TransactionBroadcaster,
{
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastOutcome, SourceError> {
        match self {
            Some(broadcaster) => broadcaster.broadcast_transaction(raw_transaction).await,
            None => Err(SourceError::TransactionBroadcastDisabled),
        }
    }
}

/// Upstream tree-state fill boundary for the wallet query plane.
///
/// The query plane serves a coherent commitment tree state at any block height.
/// Stored sparse checkpoints answer most requests; for a height without a stored
/// checkpoint the plane fills from the configured upstream node, mirroring
/// lightwalletd's `GetTreeState`. This is the one query path permitted to contact
/// an upstream node (zinder ADR-0005), gated on an explicitly supplied source.
#[async_trait]
pub trait TreeStateUpstream: Send + Sync + 'static {
    /// Fetches the tree state for one already-identified canonical block.
    async fn fetch_tree_state_for_block(
        &self,
        block_id: BlockId,
    ) -> Result<SourceTreeState, SourceError>;
}

fn bounded_segment_end_height(
    start_height: BlockHeight,
    tip_height: BlockHeight,
    max_connected_blocks: NonZeroU32,
) -> BlockHeight {
    let last_requested_height = start_height
        .value()
        .saturating_add(max_connected_blocks.get().saturating_sub(1));
    BlockHeight::new(last_requested_height.min(tip_height.value()))
}

fn block_height_span_len(start_height: BlockHeight, end_height: BlockHeight) -> usize {
    if end_height < start_height {
        return 0;
    }
    let len = end_height
        .value()
        .saturating_sub(start_height.value())
        .saturating_add(1);
    usize::try_from(len).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use zinder_core::{BroadcastAccepted, RawTransactionBytes, TransactionId};

    use super::{SourceError, TransactionBroadcastOutcome, TransactionBroadcaster};

    #[derive(Clone, Default)]
    struct CountingBroadcaster {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TransactionBroadcaster for CountingBroadcaster {
        async fn broadcast_transaction(
            &self,
            _raw_transaction: RawTransactionBytes,
        ) -> Result<TransactionBroadcastOutcome, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TransactionBroadcastOutcome::Accepted(BroadcastAccepted {
                transaction_id: TransactionId::from_bytes([0; 32]),
            }))
        }
    }

    #[tokio::test]
    async fn option_some_delegates_to_inner_broadcaster() {
        let inner = CountingBroadcaster::default();
        let calls = inner.calls.clone();
        let broadcaster: Option<CountingBroadcaster> = Some(inner);

        let outcome = broadcaster
            .broadcast_transaction(RawTransactionBytes::new(vec![1, 2, 3]))
            .await;

        assert!(matches!(
            outcome,
            Ok(TransactionBroadcastOutcome::Accepted(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn option_none_returns_broadcast_disabled() {
        let broadcaster: Option<CountingBroadcaster> = None;

        let outcome = broadcaster
            .broadcast_transaction(RawTransactionBytes::new(vec![1, 2, 3]))
            .await;

        assert!(matches!(
            outcome,
            Err(SourceError::TransactionBroadcastDisabled)
        ));
    }
}
