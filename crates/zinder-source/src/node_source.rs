//! Node source boundary values.

use std::num::NonZeroU32;

use async_trait::async_trait;
use zinder_core::{
    BlockHeight, BlockId, RawTransactionBytes, ShieldedProtocol, SubtreeRootIndex,
    TransactionBroadcastResult,
};

use crate::{NodeCapabilities, SourceBlock, SourceError, SourceSubtreeRoots};

/// Configured upstream node source for ingestion.
#[async_trait]
pub trait NodeSource: Send + Sync + 'static {
    /// Returns the source capabilities discovered or declared at startup.
    fn capabilities(&self) -> NodeCapabilities;

    /// Fetches one block by height from the configured node.
    async fn fetch_block_by_height(&self, height: BlockHeight) -> Result<SourceBlock, SourceError>;

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
}

/// Node-backed transaction broadcast boundary.
#[async_trait]
pub trait TransactionBroadcaster: Send + Sync + 'static {
    /// Broadcasts a raw transaction to the configured node or network path.
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, SourceError>;
}

#[async_trait]
impl TransactionBroadcaster for () {
    async fn broadcast_transaction(
        &self,
        _raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, SourceError> {
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
    ) -> Result<TransactionBroadcastResult, SourceError> {
        match self {
            Some(broadcaster) => broadcaster.broadcast_transaction(raw_transaction).await,
            None => Err(SourceError::TransactionBroadcastDisabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use zinder_core::{BroadcastAccepted, RawTransactionBytes, TransactionId};

    use super::{SourceError, TransactionBroadcastResult, TransactionBroadcaster};

    #[derive(Clone, Default)]
    struct CountingBroadcaster {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TransactionBroadcaster for CountingBroadcaster {
        async fn broadcast_transaction(
            &self,
            _raw_transaction: RawTransactionBytes,
        ) -> Result<TransactionBroadcastResult, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TransactionBroadcastResult::Accepted(BroadcastAccepted {
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
            Ok(TransactionBroadcastResult::Accepted(_))
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
