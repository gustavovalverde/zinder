//! Public chain-index trait and consumer-facing domain types.

use std::{pin::Pin, time::Duration};

use async_trait::async_trait;
use tokio_stream::Stream;
use zinder_core::{
    BlockHeight, BlockHeightRange, BlockId, ChainEpoch, CompactBlockArtifact, RawTransactionBytes,
    SubtreeRootArtifact, SubtreeRootRange, TransactionArtifact, TransactionBroadcastResult,
    TransactionId, TreeStateArtifact,
};
use zinder_proto::v1::wallet::ServerCapabilities;
use zinder_store::{ChainEventStreamFamily, StreamCursorTokenV1};

use crate::IndexerError;

/// Capability strings covered by the typed [`ChainIndex`] surface.
pub const CHAIN_INDEX_CAPABILITIES: &[&str] = &[
    "wallet.read.latest_block_v1",
    "wallet.read.compact_block_at_v1",
    "wallet.read.compact_block_range_v1",
    "wallet.read.tree_state_at_v1",
    "wallet.read.latest_tree_state_v1",
    "wallet.read.subtree_roots_in_range_v1",
    "wallet.read.transaction_by_id_v1",
    "wallet.read.server_info_v1",
    "wallet.broadcast.transaction_v1",
    "wallet.events.chain_v1",
];

/// Typed stream returned by chain-index methods.
pub type IndexStream<T> = Pin<Box<dyn Stream<Item = Result<T, IndexerError>> + Send + 'static>>;

/// Chain-event stream returned by [`ChainIndex::chain_events`].
pub type ChainEventStream = IndexStream<ChainEventEnvelope>;

/// Opaque chain-event cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChainEventCursor(StreamCursorTokenV1);

impl ChainEventCursor {
    /// Creates a cursor from bytes previously returned by Zinder.
    #[must_use]
    pub fn from_bytes(cursor_bytes: impl Into<Vec<u8>>) -> Self {
        Self(StreamCursorTokenV1::from_bytes(cursor_bytes))
    }

    /// Returns the opaque cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Cursor-bound chain event returned to Rust consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEventEnvelope {
    /// Cursor for resuming strictly after this event.
    pub cursor: ChainEventCursor,
    /// Monotonic sequence in this event stream.
    pub event_sequence: u64,
    /// Chain epoch visible after this event.
    pub chain_epoch: ChainEpoch,
    /// Finalized height reported with this event.
    pub finalized_height: BlockHeight,
    /// Canonical chain transition.
    pub event: ChainEvent,
}

/// Canonical chain transition carried by [`ChainEventEnvelope`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// A pure append or visibility advance was committed.
    ChainCommitted {
        /// Committed epoch payload.
        committed: ChainEpochCommitted,
    },
    /// A non-finalized range was replaced.
    ChainReorged {
        /// Previously visible range invalidated by this transition.
        reverted: ChainRangeReverted,
        /// Replacement range committed by this transition.
        committed: ChainEpochCommitted,
    },
}

/// Durable range committed by one chain event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainEpochCommitted {
    /// Chain epoch visible after the commit.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range included in the commit.
    pub block_range: BlockHeightRange,
}

/// Durable range reverted by one chain event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainRangeReverted {
    /// Chain epoch that contained the reverted range.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range invalidated by this transition.
    pub block_range: BlockHeightRange,
}

/// Transaction lookup status.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TxStatus {
    /// Transaction is mined in the canonical chain.
    Mined(TransactionArtifact),
    /// Transaction is not indexed in the visible canonical chain.
    NotFound,
    /// Transaction is known to be in the mempool.
    InMempool,
    /// Transaction conflicts with the visible canonical chain.
    ConflictingChain,
}

/// Typed chain-index contract consumed by wallets and applications.
#[async_trait]
pub trait ChainIndex: Send + Sync + 'static {
    /// Returns the server capability descriptor when the implementation has a
    /// service endpoint.
    async fn server_info(&self) -> Result<ServerCapabilities, IndexerError>;

    /// Returns the current visible chain epoch.
    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError>;

    /// Returns the latest visible block identity.
    async fn latest_block(&self) -> Result<BlockId, IndexerError>;

    /// Returns the latest block identity from a requested chain epoch.
    async fn latest_block_at_epoch(&self, at_epoch: ChainEpoch) -> Result<BlockId, IndexerError>;

    /// Reads one compact block artifact.
    async fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<CompactBlockArtifact, IndexerError>;

    /// Reads one compact block artifact from a requested chain epoch.
    async fn compact_block_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<CompactBlockArtifact, IndexerError>;

    /// Streams compact block artifacts for an inclusive range.
    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError>;

    /// Streams compact block artifacts for an inclusive range from a requested
    /// chain epoch.
    async fn compact_blocks_in_range_at_epoch(
        &self,
        block_range: BlockHeightRange,
        at_epoch: ChainEpoch,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError>;

    /// Reads one tree-state artifact.
    async fn tree_state_at(&self, height: BlockHeight) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads one tree-state artifact from a requested chain epoch.
    async fn tree_state_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads the tree-state artifact at the visible tip.
    async fn latest_tree_state(&self) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads the tree-state artifact at a requested chain epoch's tip.
    async fn latest_tree_state_at_epoch(
        &self,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads subtree roots for a bounded range.
    async fn subtree_roots_in_range(
        &self,
        subtree_root_range: SubtreeRootRange,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError>;

    /// Reads subtree roots for a bounded range from a requested chain epoch.
    async fn subtree_roots_in_range_at_epoch(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: ChainEpoch,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError>;

    /// Looks up a transaction by id.
    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<TxStatus, IndexerError>;

    /// Looks up a transaction by id from a requested chain epoch.
    async fn transaction_by_id_at_epoch(
        &self,
        transaction_id: TransactionId,
        at_epoch: ChainEpoch,
    ) -> Result<TxStatus, IndexerError>;

    /// Broadcasts raw transaction bytes without mutating canonical storage.
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, IndexerError>;

    /// Streams tip-family chain events.
    async fn chain_events(
        &self,
        from_cursor: Option<ChainEventCursor>,
    ) -> Result<ChainEventStream, IndexerError> {
        self.chain_events_for_family(from_cursor, ChainEventStreamFamily::Tip)
            .await
    }

    /// Streams chain events for the requested family.
    async fn chain_events_for_family(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventStream, IndexerError>;

    /// Returns the catchup cadence used by local implementations, or `None`
    /// for purely remote implementations.
    fn local_catchup_interval(&self) -> Option<Duration> {
        None
    }
}
