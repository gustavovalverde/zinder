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
    TransparentMempoolSpend, TransparentOutPoint, TransparentPrevoutsResponse, TreeStateArtifact,
    TxStatus,
};
use zinder_proto::v1::wallet::WalletServerInfo;
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
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEvent {
    /// A non-reorg commit advanced the canonical tip or finalized prefix.
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
/// Cursor for paged implementations. Today's in-memory snapshot returns the
/// head of the live index in one response and ignores supplied cursors; servers
/// that return `next_cursor` use this value for the next page.
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
    /// Upstream node refused admission of the transaction. Reserved for
    /// ZIP-401 `RecentlyEvicted`; source-side emission is pending node-side
    /// visibility per ADR-0010 §Suppression. Wallet integrators may subscribe
    /// today but should not block on receiving this variant.
    Suppressed {
        /// Identifier of the suppressed transaction.
        transaction_id: TransactionId,
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
#[async_trait]
pub trait ChainIndex: Send + Sync + 'static {
    /// Returns the wallet-plane server descriptor when the implementation has a
    /// service endpoint.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let descriptor = client.server_info().await?;
    /// # let _ = descriptor; Ok(()) }
    /// ```
    async fn server_info(&self) -> Result<WalletServerInfo, IndexerError>;

    /// Returns the current visible chain epoch.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let epoch = client.current_epoch().await?;
    /// # let _ = epoch; Ok(()) }
    /// ```
    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError>;

    /// Returns the latest visible block identity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let block = client.latest_block(None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<BlockId, IndexerError>;

    /// Resolves a block selector against the canonical best chain.
    ///
    /// Hash-only callers pass the `Hash` arm; height-only callers pass the
    /// `Height` arm and receive a normalized [`BlockId`] with the resolved
    /// hash. Returns [`IndexerError::NotFound`] when the selector addresses
    /// a block that is not visible at the request's chain epoch (reorged
    /// out or never indexed).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, BlockSelector, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let selector = BlockSelector::from_height(BlockHeight::new(0));
    /// let block = client.block_id_by_selector(selector, None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
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
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, BlockSelector, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let selector = BlockSelector::from_height(BlockHeight::new(0));
    /// let header = client.block_header_by_selector(selector, None).await?;
    /// # let _ = header; Ok(()) }
    /// ```
    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderInfo, IndexerError>;

    /// Reads one compact block artifact.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let block = client.compact_block_at(BlockHeight::new(0), None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockArtifact, IndexerError>;

    /// Streams compact block artifacts for an inclusive range.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, BlockHeightRange, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let range = BlockHeightRange::inclusive(BlockHeight::new(0), BlockHeight::new(0));
    /// let stream = client.compact_blocks_in_range(range, None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
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
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let tree = client.tree_state_at(BlockHeight::new(0), None).await?;
    /// # let _ = tree; Ok(()) }
    /// ```
    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads the tree-state artifact at the visible tip.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let tree = client.latest_tree_state(None).await?;
    /// # let _ = tree; Ok(()) }
    /// ```
    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads subtree roots for a bounded range.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::num::NonZeroU32;
    /// # use zinder_client::{
    /// #     ChainIndex, IndexerError, ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// # let Some(max_entries) = NonZeroU32::new(1) else { return Ok(()) };
    /// let range = SubtreeRootRange::new(
    ///     ShieldedProtocol::Sapling,
    ///     SubtreeRootIndex::new(0),
    ///     max_entries,
    /// );
    /// let roots = client.subtree_roots_in_range(range, None).await?;
    /// # let _ = roots; Ok(()) }
    /// ```
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
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let status = client
    ///     .transaction_by_id(TransactionId::from_bytes([0u8; 32]), None)
    ///     .await?;
    /// # let _ = status; Ok(()) }
    /// ```
    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError>;

    /// Broadcasts raw transaction bytes without mutating canonical storage.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, RawTransactionBytes};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outcome = client
    ///     .broadcast_transaction(RawTransactionBytes::new(Vec::<u8>::new()))
    ///     .await?;
    /// # let _ = outcome; Ok(()) }
    /// ```
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, IndexerError>;

    /// Streams tip-family chain events.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client.chain_events(None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events(
        &self,
        from_cursor: Option<ChainEventCursor>,
    ) -> Result<ChainEventStream, IndexerError> {
        self.chain_events_for_family(from_cursor, ChainEventStreamFamily::Tip)
            .await
    }

    /// Streams chain events for the requested family.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainEventStreamFamily, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client
    ///     .chain_events_for_family(None, ChainEventStreamFamily::Tip)
    ///     .await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events_for_family(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventStream, IndexerError>;

    /// Streams chain events with a server-side address invalidation filter
    /// applied (per [ADR-0021]).
    ///
    /// `address_filter` is a list of transparent t-addresses (canonical
    /// Base58 form). An empty list disables filtering and delivers every
    /// envelope. A non-empty list narrows commit envelopes to those whose
    /// block range touches at least one of the supplied addresses; reorgs
    /// always pass through regardless of filter. The cursor remains opaque;
    /// clients re-derive per-address state from `compact_block_at` after
    /// each received envelope.
    ///
    /// The default implementation falls back to
    /// [`Self::chain_events_for_family`] (filter ignored) so backends like
    /// `LocalChainIndex` that talk to storage directly continue to work;
    /// remote backends that talk to a Zinder server push the filter
    /// through to the wire.
    ///
    /// [ADR-0021]: ../../../docs/adrs/0021-canonical-confirmed-push-channel-for-transparent-activity.md
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainEventStreamFamily, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client
    ///     .chain_events_with_filter(None, ChainEventStreamFamily::Tip, Vec::new())
    ///     .await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events_with_filter(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
        address_filter: Vec<String>,
    ) -> Result<ChainEventStream, IndexerError> {
        let _ = address_filter;
        self.chain_events_for_family(from_cursor, family).await
    }

    /// Returns a bounded snapshot of the live mempool index.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, MempoolSnapshotRequest};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let snapshot = client
    ///     .mempool_snapshot(MempoolSnapshotRequest::default())
    ///     .await?;
    /// # let _ = snapshot; Ok(()) }
    /// ```
    async fn mempool_snapshot(
        &self,
        request: MempoolSnapshotRequest,
    ) -> Result<MempoolSnapshotView, IndexerError>;

    /// Streams replayable mempool events.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client.mempool_events(None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn mempool_events(
        &self,
        from_cursor: Option<MempoolEventCursor>,
    ) -> Result<MempoolEventStream, IndexerError>;

    /// Returns whether `transaction_id` is currently visible in the live
    /// mempool index.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let present = client
    ///     .is_in_mempool(TransactionId::from_bytes([0u8; 32]))
    ///     .await?;
    /// # let _ = present; Ok(()) }
    /// ```
    async fn is_in_mempool(&self, transaction_id: TransactionId) -> Result<bool, IndexerError>;

    /// Reads a bounded page of unspent transparent outputs.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     BlockHeight, ChainIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentAddressUtxosQuery,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let query = TransparentAddressUtxosQuery {
    ///     address_script_hash: TransparentAddressScriptHash::from_bytes([0u8; 32]),
    ///     start_height: BlockHeight::new(0),
    ///     max_entries: None,
    ///     from_cursor: None,
    /// };
    /// let page = client.transparent_address_utxos(query, None).await?;
    /// # let _ = page; Ok(()) }
    /// ```
    async fn transparent_address_utxos(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxosView, IndexerError>;

    /// Streams unspent transparent outputs for one transparent address.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     BlockHeight, ChainIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentAddressUtxosQuery,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let query = TransparentAddressUtxosQuery {
    ///     address_script_hash: TransparentAddressScriptHash::from_bytes([0u8; 32]),
    ///     start_height: BlockHeight::new(0),
    ///     max_entries: None,
    ///     from_cursor: None,
    /// };
    /// let stream = client.transparent_address_utxos_stream(query, None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn transparent_address_utxos_stream(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxoStream, IndexerError>;

    /// Streams transparent-address tx-history index entries.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     BlockHeight, ChainIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentAddressTxIdsQuery,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let query = TransparentAddressTxIdsQuery {
    ///     address_script_hash: TransparentAddressScriptHash::from_bytes([0u8; 32]),
    ///     start_height: BlockHeight::new(0),
    ///     end_height: BlockHeight::new(0),
    ///     max_entries: None,
    ///     from_cursor: None,
    ///     descending: false,
    /// };
    /// let stream = client.transparent_address_tx_ids_in_range(query, None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
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
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     ChainIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentMempoolOutputsRequest,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let request = TransparentMempoolOutputsRequest {
    ///     address_script_hash: TransparentAddressScriptHash::from_bytes([0u8; 32]),
    ///     max_entries: 64,
    /// };
    /// let outputs = client.transparent_mempool_outputs_by_address(request).await?;
    /// # let _ = outputs; Ok(()) }
    /// ```
    async fn transparent_mempool_outputs_by_address(
        &self,
        request: TransparentMempoolOutputsRequest,
    ) -> Result<Vec<TransparentMempoolOutput>, IndexerError>;

    /// Returns the mempool spend that consumes `outpoint`, when one is
    /// present.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0);
    /// let spend = client.transparent_mempool_spend_by_outpoint(outpoint).await?;
    /// # let _ = spend; Ok(()) }
    /// ```
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
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransparentAddressScriptHash};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let addresses = [TransparentAddressScriptHash::from_bytes([0u8; 32])];
    /// let balance = client.transparent_address_balance(&addresses, None).await?;
    /// # let _ = balance; Ok(()) }
    /// ```
    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressBalance, IndexerError>;

    /// Resolves a batch of canonical-chain transparent outpoints to their
    /// referenced outputs, in input order. Each entry's `prevout` is
    /// `None` when the canonical chain at the response's epoch does not
    /// have the referenced output.
    ///
    /// Implementations reject the coinbase sentinel and silently truncate
    /// requests above
    /// [`zinder_core::MAX_TRANSPARENT_PREVOUTS_PER_REQUEST`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let prevouts = client.transparent_prevouts(&outpoints, None).await?;
    /// # let _ = prevouts; Ok(()) }
    /// ```
    async fn transparent_prevouts(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentPrevoutsResponse, IndexerError>;

    /// Resolves a batch of outpoints against the live mempool index. Used
    /// when an outpoint references an output of an unconfirmed mempool
    /// transaction (chained-mempool flows).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let prevouts = client.transparent_mempool_prevouts(&outpoints).await?;
    /// # let _ = prevouts; Ok(()) }
    /// ```
    async fn transparent_mempool_prevouts(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<TransparentPrevoutsResponse, IndexerError>;

    /// Returns the catchup cadence used by local implementations, or `None`
    /// for purely remote implementations.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::ChainIndex;
    /// # fn demo<T: ChainIndex>(client: &T) {
    /// let cadence = client.local_catchup_interval();
    /// # let _ = cadence; }
    /// ```
    fn local_catchup_interval(&self) -> Option<Duration> {
        None
    }
}
