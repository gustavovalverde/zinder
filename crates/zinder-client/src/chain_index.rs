//! Public chain-index trait and consumer-facing domain types.

use std::{num::NonZeroU32, pin::Pin, time::Duration};

use async_trait::async_trait;
use tokio_stream::Stream;
use zinder_core::{
    BlockHash, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockId, BlockSelector, ChainEpoch,
    CompactBlockArtifact, MempoolEntry, MempoolEvictionReason, RawTransactionBytes,
    SubtreeRootArtifact, SubtreeRootRange, TransactionBroadcastResult, TransactionId,
    TransparentAddressBalance, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentAddressUtxoArtifact, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend, TransparentOutPoint, TreeStateArtifact, TxStatus,
};
use zinder_proto::v1::wallet::ServerCapabilities;
use zinder_store::{ChainEventStreamFamily, StreamCursorTokenV1};

use crate::IndexerError;

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
///
/// closes: G20
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// A non-reorg commit advanced the canonical tip.
    TipAdvanced {
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

impl ChainEvent {
    /// Returns `true` when this event represents a non-reorg tip advance.
    ///
    /// Convenience for consumers that need a single boolean for tip-change
    /// notifications without `match`-ing on every variant.
    #[must_use]
    pub const fn is_tip_advance(&self) -> bool {
        matches!(self, Self::TipAdvanced { .. })
    }
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

/// Opaque mempool-event cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MempoolEventCursor(StreamCursorTokenV1);

impl MempoolEventCursor {
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

/// Bounded snapshot request for [`ChainIndex::mempool_snapshot`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MempoolSnapshotRequest {
    /// Server-enforced maximum entry count.
    pub max_entries: u32,
    /// Optional next-page cursor returned by a previous paged snapshot.
    pub from_cursor: Option<MempoolSnapshotCursor>,
}

/// Opaque next-page cursor for paged mempool snapshots.
///
/// Reserved for future paged implementations. Today's in-memory snapshot
/// returns the head of the live index in one response and ignores
/// supplied cursors; persistent storage will populate this on the
/// follow-up implementation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MempoolSnapshotCursor(Vec<u8>);

impl MempoolSnapshotCursor {
    /// Creates a snapshot cursor from bytes returned by Zinder.
    #[must_use]
    pub fn from_bytes(cursor_bytes: impl Into<Vec<u8>>) -> Self {
        Self(cursor_bytes.into())
    }

    /// Returns the opaque cursor bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Snapshot view returned by [`ChainIndex::mempool_snapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MempoolSnapshotView {
    /// Chain epoch visible at snapshot time.
    pub chain_epoch: ChainEpoch,
    /// Monotonic snapshot sequence reported by the server.
    pub snapshot_sequence: u64,
    /// Snapshot age in milliseconds when the response was constructed.
    pub snapshot_age_millis: u64,
    /// Hydrated mempool entries.
    pub entries: Vec<MempoolEntry>,
    /// Next-page cursor when the response was truncated.
    pub next_cursor: Option<MempoolSnapshotCursor>,
}

/// Cursor-bound mempool source-event delivered to resumable consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MempoolEventEnvelope {
    /// Cursor for resuming strictly after this event.
    pub cursor: MempoolEventCursor,
    /// Monotonic sequence in the mempool-event stream. Independent from
    /// the chain-event sequence space.
    pub event_sequence: u64,
    /// Wall-clock time when the indexer observed the source change.
    pub source_observed_unix_millis: u64,
    /// Source transition observed by the indexer.
    pub event: MempoolEvent,
}

/// Mempool source transition delivered to consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "Added carries the full hydrated MempoolEntry by design; consumers replay state from the event log without a follow-up snapshot call."
)]
pub enum MempoolEvent {
    /// Mempool transaction observed.
    Added {
        /// Hydrated entry observed by the indexer.
        entry: MempoolEntry,
    },
    /// Mempool transaction removed without being mined.
    Invalidated {
        /// Identifier of the invalidated transaction.
        transaction_id: TransactionId,
        /// Source-classified eviction reason.
        reason: MempoolEvictionReason,
    },
    /// Mempool transaction observed mined into a block.
    Mined {
        /// Identifier of the mined transaction.
        transaction_id: TransactionId,
        /// Height at which the source observed the mining.
        mined_height: BlockHeight,
        /// Hash of the block that mined the transaction, as observed by the
        /// source. Lifecycle consumers can resolve mined block identity
        /// without a follow-up tip read.
        block_hash: BlockHash,
    },
}

/// Mempool-event stream returned by [`ChainIndex::mempool_events`].
pub type MempoolEventStream = IndexStream<MempoolEventEnvelope>;

/// Opaque transparent-UTXO cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TransparentUtxoCursor(StreamCursorTokenV1);

impl TransparentUtxoCursor {
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

/// Read parameters for [`ChainIndex::transparent_address_utxos`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxosQuery {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Wallet-birthday optimization: minimum mined height to include.
    /// Ignored when `from_cursor` is `Some`.
    pub start_height: BlockHeight,
    /// Server-bounded entry cap. `None` defers to the server default.
    pub max_entries: Option<NonZeroU32>,
    /// Optional cursor returned by a previous read.
    pub from_cursor: Option<TransparentUtxoCursor>,
}

/// Page of unspent transparent outputs returned by `ChainIndex`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxosView {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Unspent outputs in ascending `(block_height, outpoint)` order.
    pub utxos: Vec<TransparentAddressUtxoArtifact>,
    /// Resume cursor when more UTXOs may be available.
    pub next_cursor: Option<TransparentUtxoCursor>,
}

/// Stream of transparent-UTXO chunks returned by
/// [`ChainIndex::transparent_address_utxos_stream`]. Each item carries one
/// UTXO and the cursor to resume strictly after it.
pub type TransparentAddressUtxoStream = IndexStream<TransparentAddressUtxoStreamItem>;

/// Opaque transparent-address tx-history cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TransparentHistoryCursor(StreamCursorTokenV1);

impl TransparentHistoryCursor {
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

/// Read parameters for [`ChainIndex::transparent_address_tx_ids_in_range`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTxIdsQuery {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Inclusive minimum block height.
    pub start_height: BlockHeight,
    /// Inclusive maximum block height.
    pub end_height: BlockHeight,
    /// Server-bounded entry cap. `None` defers to the server default.
    pub max_entries: Option<NonZeroU32>,
    /// Optional cursor returned by a previous read.
    pub from_cursor: Option<TransparentHistoryCursor>,
    /// Iterate newest-first when true.
    pub descending: bool,
}

/// One streamed tx-history chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTxIdsStreamItem {
    /// Chain epoch used to answer this chunk.
    pub chain_epoch: ChainEpoch,
    /// Indexed tx-history artifact.
    pub artifact: TransparentAddressTxIndexArtifact,
    /// Resume cursor on the last chunk when more entries may be available.
    pub cursor: Option<TransparentHistoryCursor>,
}

/// Stream of tx-history chunks returned by
/// [`ChainIndex::transparent_address_tx_ids_in_range`].
pub type TransparentAddressTxIdsStream = IndexStream<TransparentAddressTxIdsStreamItem>;

/// One streamed transparent-UTXO chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxoStreamItem {
    /// Chain epoch used to answer this chunk.
    pub chain_epoch: ChainEpoch,
    /// Single unspent output.
    pub utxo: TransparentAddressUtxoArtifact,
    /// Resume cursor for this position. `None` on every chunk except the
    /// last one, where it carries the next-page cursor when more UTXOs may
    /// be available.
    pub cursor: Option<TransparentUtxoCursor>,
}

/// Typed chain-index contract consumed by wallets and applications.
///
/// Every read that depends on chain state takes `at_epoch: Option<ChainEpoch>`.
/// `None` resolves to the visible chain epoch at call time; `Some(epoch)`
/// pins the read to that epoch. Implementations that cannot honor a pinned
/// epoch return [`IndexerError::FailedPrecondition`].
///
/// All trait methods take and return `zinder-core` types; generated
/// `zinder_proto::*` types appear only in adapter modules, never on this
/// public Rust API.
///
/// refuses: A5
#[async_trait]
pub trait ChainIndex: Send + Sync + 'static {
    /// Returns the server capability descriptor when the implementation has a
    /// service endpoint.
    async fn server_info(&self) -> Result<ServerCapabilities, IndexerError>;

    /// Returns the current visible chain epoch.
    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError>;

    /// Returns the latest visible block identity.
    ///
    /// closes: G15
    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<BlockId, IndexerError>;

    /// Resolves a block selector against the canonical best chain.
    ///
    /// Replaces the lightwalletd `BlockId { height, hash }` request shape:
    /// hash-only callers no longer need to pretend `height = 0` is a
    /// sentinel, and height-only callers get a normalized [`BlockId`] with
    /// the resolved hash. Returns [`IndexerError::NotFound`] when the
    /// selector addresses a block that is not visible at the request's
    /// chain epoch (reorged out or never indexed).
    ///
    /// closes: G2
    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockId, IndexerError>;

    /// Returns the typed block-header read model for a block selector.
    ///
    /// The Zinder header shape is independent of the lightwalletd compact
    /// header and the upstream node's JSON-RPC `getblockheader` shape.
    ///
    /// closes: G4
    /// refuses: A2
    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderInfo, IndexerError>;

    /// Reads one compact block artifact.
    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockArtifact, IndexerError>;

    /// Streams compact block artifacts for an inclusive range.
    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError>;

    /// Reads one tree-state artifact.
    ///
    /// `at_epoch = None` resolves to the live tip; `Some(epoch)` pins the
    /// read to that chain epoch.
    ///
    /// closes: G17
    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads the tree-state artifact at the visible tip.
    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads subtree roots for a bounded range.
    ///
    /// closes: G16
    /// closes: G21
    async fn subtree_roots_in_range(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError>;

    /// Looks up a transaction by id.
    ///
    /// `None` for `at_epoch` consults the live mempool when the canonical
    /// chain has no record. `Some(epoch)` pins the read to that epoch and
    /// never consults mempool state.
    ///
    /// closes: G3
    /// closes: G13
    /// refuses: A1
    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError>;

    /// Broadcasts raw transaction bytes without mutating canonical storage.
    ///
    /// closes: G19
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

    /// Returns a bounded snapshot of the live mempool index.
    async fn mempool_snapshot(
        &self,
        request: MempoolSnapshotRequest,
    ) -> Result<MempoolSnapshotView, IndexerError>;

    /// Streams replayable mempool events.
    async fn mempool_events(
        &self,
        from_cursor: Option<MempoolEventCursor>,
    ) -> Result<MempoolEventStream, IndexerError>;

    /// Returns whether `transaction_id` is currently visible in the live
    /// mempool index.
    ///
    /// closes: G7
    async fn is_in_mempool(&self, transaction_id: TransactionId) -> Result<bool, IndexerError>;

    /// Reads a bounded page of unspent transparent outputs.
    async fn transparent_address_utxos(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxosView, IndexerError>;

    /// Streams unspent transparent outputs for one transparent address.
    async fn transparent_address_utxos_stream(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxoStream, IndexerError>;

    /// Streams transparent-address tx-history index entries.
    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressTxIdsStream, IndexerError>;

    /// Returns transparent mempool outputs tied to one address.
    ///
    /// Bounded by the request's `max_entries`; values larger than the
    /// server's configured cap are silently clamped to that cap.
    ///
    /// closes: G6
    async fn transparent_mempool_outputs_by_address(
        &self,
        request: TransparentMempoolOutputsRequest,
    ) -> Result<Vec<TransparentMempoolOutput>, IndexerError>;

    /// Returns the mempool spend that consumes `outpoint`, when one is
    /// present.
    async fn transparent_mempool_spend_by_outpoint(
        &self,
        outpoint: TransparentOutPoint,
    ) -> Result<Option<TransparentMempoolSpend>, IndexerError>;

    /// Returns the transparent-address balance summed across `addresses`.
    ///
    /// Federated to the derive plane: deployments without `zinder-derive`
    /// reachable surface this as
    /// [`IndexerError::ServiceUnavailable`]/derive-unavailable.
    ///
    /// closes: G1
    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressBalance, IndexerError>;

    /// Returns the catchup cadence used by local implementations, or `None`
    /// for purely remote implementations.
    fn local_catchup_interval(&self) -> Option<Duration> {
        None
    }
}
