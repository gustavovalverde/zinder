//! Public chain-index trait and consumer-facing domain types.

use std::{num::NonZeroU32, pin::Pin, time::Duration};

use crate::IndexerError;
use async_trait::async_trait;
use tokio_stream::Stream;
use zinder_core::{
    BlockBlobArtifact, BlockHeader, BlockHeight, BlockHeightRange, BlockId, BlockSelector,
    ChainEpoch, ChainEpochId, CompactBlockArtifact, SubtreeRootArtifact, SubtreeRootRange,
    TransactionId, TransparentAddressBalance, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentOutPoint, TransparentOutputsByOutpointResponse,
    TransparentSpendsByOutpointResponse, TransparentUnspentOutput,
    TransparentUnspentOutputsByOutpointResponse, TransparentUtxoSetCommitment, TreeStateArtifact,
    TxStatus,
};
#[cfg(feature = "remote")]
use zinder_core::{
    BlockHash, ChainValuePoolsAtTip, MempoolEntry, MempoolEvictionReason, RawTransactionBytes,
    TransactionBroadcastOutcome, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend,
};
#[cfg(feature = "remote")]
use zinder_proto::v1::wallet::WalletServerInfo;

/// Chain-event stream selected by a subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
pub enum ChainEventStreamFamily {
    /// Every visible chain transition, including reorgs.
    Visible,
    /// Non-reorg commits that are entirely at or below the settled tip.
    Settled,
}

/// Typed stream returned by chain-index methods.
pub type IndexStream<T> = Pin<Box<dyn Stream<Item = Result<T, IndexerError>> + Send + 'static>>;

/// Chain-event stream returned by [`EndpointBackedIndex::chain_events`].
#[cfg(feature = "remote")]
pub type ChainEventStream = IndexStream<ChainEventEnvelope>;

/// Explicit start position for a resumable event-stream subscription.
///
/// Mirrors the wire `EventStreamStart` oneof. `AfterCursor` resumes strictly
/// after an opaque cursor from a previously delivered envelope: it is the
/// reconnect path once at least one event has been applied. A fresh
/// subscription chooses `EarliestRetained` to replay the retention window or
/// `LiveTail` to resolve once at subscribe time to the current stream head,
/// receiving only events applied after subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
pub enum EventStreamStart<Cursor> {
    /// Resume strictly after this cursor; its encoded family is
    /// authoritative.
    AfterCursor(Cursor),
    /// Replay from the earliest retained event.
    EarliestRetained,
    /// Start at the stream head resolved at subscribe time.
    LiveTail,
}

/// Opaque chain-event cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg(feature = "remote")]
pub struct ChainEventCursor(Vec<u8>);

#[cfg(feature = "remote")]
impl ChainEventCursor {
    /// Creates a cursor from bytes previously returned by Zinder.
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

/// Cursor-bound chain event returned to Rust consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
pub struct ChainEventEnvelope {
    /// Cursor for resuming strictly after this event.
    pub cursor: ChainEventCursor,
    /// Monotonic sequence in this event stream.
    pub event_sequence: u64,
    /// Chain epoch visible after this event.
    pub chain_epoch: ChainEpoch,
    /// Settled tip height reported with this event.
    pub settled_tip_height: BlockHeight,
    /// Canonical chain transition.
    pub event: ChainEvent,
}

/// Canonical chain transition carried by [`ChainEventEnvelope`].
///
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[cfg(feature = "remote")]
pub enum ChainEvent {
    /// A non-reorg commit advanced the canonical tip or settled-tip prefix.
    ChainCommitted {
        /// Committed epoch payload.
        committed: ChainEpochCommitted,
    },
    /// A range that had not yet reached the settled tip was replaced.
    ChainReorged {
        /// Previously visible range invalidated by this transition.
        reverted: ChainRangeReverted,
        /// Replacement range committed by this transition.
        committed: ChainEpochCommitted,
    },
}

#[cfg(feature = "remote")]
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
#[cfg(feature = "remote")]
pub struct ChainEpochCommitted {
    /// Chain epoch visible after the commit.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range included in the commit.
    pub block_range: BlockHeightRange,
}

/// Durable range reverted by one chain event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
pub struct ChainRangeReverted {
    /// Chain epoch that contained the reverted range.
    pub chain_epoch: ChainEpoch,
    /// Inclusive block range invalidated by this transition.
    pub block_range: BlockHeightRange,
}

/// Opaque mempool-event cursor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg(feature = "remote")]
pub struct MempoolEventCursor(Vec<u8>);

#[cfg(feature = "remote")]
impl MempoolEventCursor {
    /// Creates a cursor from bytes previously returned by Zinder.
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

/// Bounded snapshot request for [`EndpointBackedIndex::mempool_snapshot`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(feature = "remote")]
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
#[cfg(feature = "remote")]
pub struct MempoolSnapshotCursor(Vec<u8>);

#[cfg(feature = "remote")]
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

/// Snapshot view returned by [`EndpointBackedIndex::mempool_snapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
pub struct MempoolSnapshotView {
    /// Chain epoch visible at snapshot time.
    pub chain_epoch: ChainEpoch,
    /// `MempoolEvents` after-cursor anchored at the moment the snapshot walk
    /// began; identical on every page of one paged walk. `None` when the
    /// server had applied no mempool event yet, in which case a consumer
    /// subscribes with [`EventStreamStart::EarliestRetained`]. Replaying from
    /// it yields at-least-once delivery; consumers apply events idempotently.
    pub events_resume_cursor: Option<MempoolEventCursor>,
    /// Snapshot age in milliseconds when the response was constructed.
    pub snapshot_age_millis: u64,
    /// Hydrated mempool entries.
    pub entries: Vec<MempoolEntry>,
    /// Next-page cursor when the response was truncated.
    pub next_cursor: Option<MempoolSnapshotCursor>,
}

/// Cursor-bound mempool source-event delivered to resumable consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "remote")]
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
#[cfg(feature = "remote")]
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

/// Mempool-event stream returned by [`EndpointBackedIndex::mempool_events`].
#[cfg(feature = "remote")]
pub type MempoolEventStream = IndexStream<MempoolEventEnvelope>;

/// Read parameters for [`ChainIndex::transparent_address_unspent_outputs`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUnspentOutputsQuery {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Wallet-birthday floor: minimum mined height to include.
    pub start_height: BlockHeight,
    /// Optional chain-epoch pin. `None` resolves against the live visible
    /// tip; `Some(id)` pins the unspent read to that epoch.
    pub at_epoch_id: Option<ChainEpochId>,
}

/// Stream of unspent transparent outputs returned by
/// [`ChainIndex::transparent_address_unspent_outputs`]. The stream always
/// carries the complete unspent set at one pinned chain epoch.
pub type TransparentAddressUnspentOutputsStream = IndexStream<TransparentUnspentOutputChunk>;

/// Opaque transparent-address tx-history cursor bound to one visible-chain fence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TransparentHistoryCursor(Vec<u8>);

impl TransparentHistoryCursor {
    /// Creates a cursor from bytes previously returned by Zinder.
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
    /// Optional cursor returned by a previous read. The cursor is valid only
    /// while the visible-chain event fence is unchanged.
    pub from_cursor: Option<TransparentHistoryCursor>,
    /// Iterate newest-first when true.
    pub descending: bool,
}

/// One streamed tx-history chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTransactionChunk {
    /// Chain epoch used to answer this chunk.
    pub chain_epoch: ChainEpoch,
    /// Indexed tx-history artifact.
    pub artifact: TransparentAddressTxIndexArtifact,
    /// Resume cursor on the last chunk when more entries may be available.
    pub cursor: Option<TransparentHistoryCursor>,
}

/// Stream of tx-history chunks returned by
/// [`ChainIndex::transparent_address_tx_ids_in_range`].
pub type TransparentAddressTxIdsStream = IndexStream<TransparentAddressTransactionChunk>;

/// One streamed unspent transparent output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentUnspentOutputChunk {
    /// Chain epoch pinned for the entire stream.
    pub chain_epoch: ChainEpoch,
    /// Single unspent output.
    pub output: TransparentUnspentOutput,
}

/// Chain-wide transparent UTXO-set summary returned to a client.
///
/// `utxo_count` and `total_value_zat` are the order-independent set totals at
/// `summarized_height`. `commitment` is present only when the serving
/// deployment advertises `wallet.read.transparent_utxo_set_commitment_v1`;
/// absence is `None`, not an error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentUtxoSetSummaryView {
    /// Chain epoch the summary was taken at.
    pub chain_epoch: ChainEpoch,
    /// Settled tip height the aggregate was taken at.
    pub summarized_height: BlockHeight,
    /// Number of unspent transparent outputs at the settled tip.
    pub utxo_count: u64,
    /// Sum of the values of every unspent transparent output, in zatoshi.
    pub total_value_zat: u64,
    /// Homomorphic commitment to the full unspent set, when advertised.
    pub commitment: Option<TransparentUtxoSetCommitment>,
}

impl TransparentUtxoSetSummaryView {
    /// Returns true only when both summaries carry a commitment under the same
    /// scheme.
    ///
    /// Two commitments are comparable only when their schemes match: a scheme
    /// mismatch (or either side absent) means not-comparable, never diverged.
    /// Callers that get `true` may then compare the commitment bytes to detect
    /// genuine divergence at the same scheme and epoch.
    #[must_use]
    pub fn comparable_with(&self, other: &Self) -> bool {
        match (self.commitment.as_ref(), other.commitment.as_ref()) {
            (Some(left), Some(right)) => left.scheme() == right.scheme(),
            _ => false,
        }
    }
}

/// Canonical and materialized-view reads served identically by every chain-index
/// handle.
///
/// Both the optional local `RocksDB`-secondary reader and remote `WalletQuery`
/// gRPC client implement this
/// trait with identical semantics. Every method here answers from the
/// canonical chain store or the materialized view, so a handle that cannot
/// reach a live ingest-control endpoint still honors the full contract.
///
/// Methods that require a live ingest-control/broadcast endpoint (broadcast,
/// live-mempool reads, the chain-event stream, chain value-pools, the
/// wallet-plane server descriptor) live on the separate
/// `EndpointBackedIndex` trait, which only the remote client
/// implements. A caller that needs one of those methods adds an
/// `EndpointBackedIndex` bound, so a handle without an endpoint fails to
/// compile instead of failing at runtime.
///
/// Canonical reads take `at_epoch_id: Option<ChainEpochId>`. `None` resolves
/// to the visible chain epoch at call time; `Some(id)` pins the read to that
/// epoch. Current materialized-view reads expose their chain epoch in stream
/// items instead of accepting a pin.
///
/// All trait methods take and return `zinder-core` types; generated
/// `zinder_proto::*` types appear only in adapter modules, never on this
/// public Rust API.
#[async_trait]
pub trait ChainIndex: Send + Sync + 'static {
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

    /// Returns the visible-tip block identity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let block = client.visible_tip_block(None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
    async fn visible_tip_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError>;

    /// Resolves the block at the chain epoch's settled tip (the height a
    /// wallet may safely use as its scan ceiling: both a compact block and
    /// a coherent tree-state checkpoint are guaranteed available within the
    /// wallet's rewind cap of that height).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let block = client.settled_tip_block(None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
    async fn settled_tip_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError>;

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
        at_epoch_id: Option<ChainEpochId>,
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeader, IndexerError>;

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
        at_epoch_id: Option<ChainEpochId>,
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError>;

    /// Reads one full serialized block.
    ///
    /// Served only when the writer deployment retains block blobs
    /// (`raw_blob_policy = "all"`). Heights with no retained blob return
    /// [`IndexerError::NotFound`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let block = client.full_block_at(BlockHeight::new(0), None).await?;
    /// # let _ = block; Ok(()) }
    /// ```
    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockBlobArtifact, IndexerError>;

    /// Streams full serialized blocks for an inclusive range.
    ///
    /// Served only when the writer deployment retains block blobs
    /// (`raw_blob_policy = "all"`); the stream errors on the first height with
    /// no retained blob.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{BlockHeight, BlockHeightRange, ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let range = BlockHeightRange::inclusive(BlockHeight::new(0), BlockHeight::new(0));
    /// let stream = client.full_blocks_in_range(range, None).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn full_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<IndexStream<BlockBlobArtifact>, IndexerError>;

    /// Reads the tree-state artifact at exactly `height`.
    ///
    /// `at_epoch_id = None` resolves to the live tip; `Some(id)` pins the
    /// read to that chain epoch. The returned artifact's height always equals
    /// `height`: remote clients fill non-checkpoint heights from the query
    /// plane's upstream node, while local clients serve only stored heights and
    /// return [`IndexerError::NotFound`] for the gaps.
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeStateArtifact, IndexerError>;

    /// Reads the tree-state artifact at the visible tip.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let tree = client.latest_tree_state_checkpoint(None).await?;
    /// # let _ = tree; Ok(()) }
    /// ```
    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError>;

    /// Looks up a transaction by id.
    ///
    /// `None` for `at_epoch_id` consults the live mempool when the canonical
    /// chain has no record. `Some(id)` pins the read to that epoch and
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TxStatus, IndexerError>;

    /// Streams the complete unspent transparent output set for one
    /// transparent address at a single pinned chain epoch.
    ///
    /// The stream cannot be truncated: there is no cursor and no entry
    /// cap. Client memory for a drained stream is proportional to the
    /// address's unspent set.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     BlockHeight, ChainIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentAddressUnspentOutputsQuery,
    /// # };
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let query = TransparentAddressUnspentOutputsQuery {
    ///     address_script_hash: TransparentAddressScriptHash::from_bytes([0u8; 32]),
    ///     start_height: BlockHeight::new(0),
    ///     at_epoch_id: None,
    /// };
    /// let stream = client.transparent_address_unspent_outputs(query).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn transparent_address_unspent_outputs(
        &self,
        query: TransparentAddressUnspentOutputsQuery,
    ) -> Result<TransparentAddressUnspentOutputsStream, IndexerError>;

    /// Streams current transparent-address tx-history index entries.
    ///
    /// The returned rows and any resume cursor are bound to the response's
    /// visible chain epoch. A reorg invalidates a prior resume cursor rather
    /// than resuming it against a different branch.
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
    /// let stream = client.transparent_address_tx_ids_in_range(query).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
    ) -> Result<TransparentAddressTxIdsStream, IndexerError>;

    /// Returns the transparent-address balance summed across `addresses`.
    ///
    /// The confirmed total is summed from the canonical unspent-output index,
    /// so both adapters answer it from the store. The signed
    /// `unconfirmed_delta_zat` overlays the live mempool only when an
    /// ingest-control endpoint is reachable; a local handle without an
    /// endpoint reports `unconfirmed_delta_zat = 0`, matching the always-on
    /// `wallet.address.transparent_balance_v1` capability semantics.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransparentAddressScriptHash};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let addresses = [TransparentAddressScriptHash::from_bytes([0u8; 32])];
    /// let balance = client.transparent_address_balance(&addresses).await?;
    /// # let _ = balance; Ok(()) }
    /// ```
    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
    ) -> Result<TransparentAddressBalance, IndexerError>;

    /// Resolves a batch of canonical-chain transparent outpoints to their
    /// referenced outputs, in input order. Each entry's `prevout` is
    /// `None` when the canonical chain at the response's epoch does not
    /// have the referenced output.
    ///
    /// Implementations reject the coinbase sentinel and silently truncate
    /// requests above
    /// [`zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let prevouts = client.transparent_outputs_by_outpoint(&outpoints, None).await?;
    /// # let _ = prevouts; Ok(()) }
    /// ```
    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, IndexerError>;

    /// Resolves a batch of canonical-chain transparent outpoints to where each
    /// was spent: the spending transaction, input index, and the block that
    /// mined the spend.
    ///
    /// Outpoints unspent on the canonical chain at the response's epoch produce
    /// no entry; consumers key results by `spent_outpoint`. Implementations
    /// reject the coinbase sentinel and silently truncate requests above
    /// [`zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`]. This is the
    /// canonical half of a getspentinfo-equivalent lookup; the unmined half is
    /// the endpoint-backed `transparent_mempool_spends_by_outpoint` method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let spends = client.transparent_spends_by_outpoint(&outpoints, None).await?;
    /// # let _ = spends; Ok(()) }
    /// ```
    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, IndexerError>;

    /// Resolves a batch of transparent outpoints to their referenced output,
    /// returning each only while it is unspent on the canonical chain at the
    /// response's epoch (gettxout-equivalent, null-if-spent).
    ///
    /// An outpoint produces an entry only when the canonical chain at that
    /// epoch has the output and it carries no canonical spend; spent or
    /// never-existed outpoints produce no entry, so every entry's `output` is
    /// present. Consumers key results by `outpoint`. Implementations reject the
    /// coinbase sentinel and silently truncate requests above
    /// [`zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`]. The read is
    /// canonical-only: a mempool-aware caller subtracts the spends returned by
    /// the endpoint-backed `transparent_mempool_spends_by_outpoint` method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError, TransactionId, TransparentOutPoint};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let unspent = client.transparent_unspent_outputs_by_outpoint(&outpoints, None).await?;
    /// # let _ = unspent; Ok(()) }
    /// ```
    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUnspentOutputsByOutpointResponse, IndexerError>;

    /// Aggregates the chain-wide transparent UTXO set at the settled tip.
    ///
    /// Returns the order-independent count and total value (the
    /// `gettxoutsetinfo`-equivalent), plus the homomorphic commitment when the
    /// serving deployment advertises
    /// `wallet.read.transparent_utxo_set_commitment_v1`. Absence of the
    /// commitment is `None`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{ChainIndex, IndexerError};
    /// # async fn demo<T: ChainIndex>(client: &T) -> Result<(), IndexerError> {
    /// let summary = client.transparent_utxo_set_summary(None).await?;
    /// # let _ = summary; Ok(()) }
    /// ```
    async fn transparent_utxo_set_summary(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUtxoSetSummaryView, IndexerError>;

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

/// Live ingest-control reads layered on top of [`ChainIndex`].
///
/// Only [`crate::RemoteChainIndex`] implements this trait: every method needs a
/// reachable ingest-control/broadcast endpoint that the canonical and materialized-view
/// stores cannot stand in for. Broadcast forwards bytes to the upstream node;
/// the mempool reads observe the writer's in-process mempool index; the
/// chain-event stream and chain value-pools are produced by the writer at the
/// settled tip; the wallet-plane server descriptor is the endpoint's own
/// advertisement.
///
/// A consumer that needs any of these methods bounds its handle as
/// `T: ChainIndex + EndpointBackedIndex`, so a colocated local reader without
/// an endpoint is rejected at compile time rather than at call time.
#[async_trait]
#[cfg(feature = "remote")]
pub trait EndpointBackedIndex: ChainIndex {
    /// Returns the wallet-plane server descriptor advertised by the endpoint.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, IndexerError};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let descriptor = client.server_info().await?;
    /// # let _ = descriptor; Ok(()) }
    /// ```
    async fn server_info(&self) -> Result<WalletServerInfo, IndexerError>;

    /// Reads chain-wide value-pool totals at the upstream node's current tip.
    ///
    /// The response is bound to the Zinder chain epoch visible when the writer
    /// answered the proxied source read. The response carries the source tip's
    /// height and hash so callers can verify the snapshot against a canonical
    /// chain identity. The upstream value-pool list is preserved as entries
    /// rather than projected into fixed pool names.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, IndexerError};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let pools = client.chain_value_pools_at_tip().await?;
    /// # let _ = pools; Ok(()) }
    /// ```
    async fn chain_value_pools_at_tip(&self) -> Result<ChainValuePoolsAtTip, IndexerError>;

    /// Broadcasts raw transaction bytes without mutating canonical storage.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, IndexerError, RawTransactionBytes};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outcome = client
    ///     .broadcast_transaction(RawTransactionBytes::new(Vec::<u8>::new()))
    ///     .await?;
    /// # let _ = outcome; Ok(()) }
    /// ```
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastOutcome, IndexerError>;

    /// Streams visible-family chain events.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, EventStreamStart, IndexerError};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client.chain_events(EventStreamStart::EarliestRetained).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events(
        &self,
        start: EventStreamStart<ChainEventCursor>,
    ) -> Result<ChainEventStream, IndexerError> {
        self.chain_events_for_family(start, ChainEventStreamFamily::Visible)
            .await
    }

    /// Streams chain events for the requested family.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     ChainEventStreamFamily, EndpointBackedIndex, EventStreamStart, IndexerError,
    /// # };
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client
    ///     .chain_events_for_family(EventStreamStart::LiveTail, ChainEventStreamFamily::Visible)
    ///     .await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events_for_family(
        &self,
        start: EventStreamStart<ChainEventCursor>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventStream, IndexerError>;

    /// Streams chain events with a server-side address invalidation filter
    /// applied.
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
    /// [`Self::chain_events_for_family`] (filter ignored); the remote backend
    /// overrides it to push the filter through to the wire.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     ChainEventStreamFamily, EndpointBackedIndex, EventStreamStart, IndexerError,
    /// # };
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client
    ///     .chain_events_with_filter(
    ///         EventStreamStart::EarliestRetained,
    ///         ChainEventStreamFamily::Visible,
    ///         Vec::new(),
    ///     )
    ///     .await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn chain_events_with_filter(
        &self,
        start: EventStreamStart<ChainEventCursor>,
        family: ChainEventStreamFamily,
        address_filter: Vec<String>,
    ) -> Result<ChainEventStream, IndexerError> {
        let _ = address_filter;
        self.chain_events_for_family(start, family).await
    }

    /// Returns a bounded snapshot of the live mempool index.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, IndexerError, MempoolSnapshotRequest};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
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
    /// # use zinder_client::{EndpointBackedIndex, EventStreamStart, IndexerError};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let stream = client.mempool_events(EventStreamStart::LiveTail).await?;
    /// # let _ = stream; Ok(()) }
    /// ```
    async fn mempool_events(
        &self,
        start: EventStreamStart<MempoolEventCursor>,
    ) -> Result<MempoolEventStream, IndexerError>;

    /// Returns whether `transaction_id` is currently visible in the live
    /// mempool index.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{EndpointBackedIndex, IndexerError, TransactionId};
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let present = client
    ///     .is_in_mempool(TransactionId::from_bytes([0u8; 32]))
    ///     .await?;
    /// # let _ = present; Ok(()) }
    /// ```
    async fn is_in_mempool(&self, transaction_id: TransactionId) -> Result<bool, IndexerError>;

    /// Returns transparent mempool outputs tied to one address.
    ///
    /// Bounded by the request's `max_entries`; values larger than the
    /// server's configured cap are silently clamped to that cap.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     EndpointBackedIndex, IndexerError, TransparentAddressScriptHash,
    /// #     TransparentMempoolOutputsRequest,
    /// # };
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
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

    /// Returns the mempool spends that consume any of `outpoints`.
    ///
    /// Outpoints with no unmined spend produce no entry; callers key
    /// results by `spent_outpoint`. Implementations silently truncate
    /// requests above
    /// [`zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     EndpointBackedIndex, IndexerError, TransactionId, TransparentOutPoint,
    /// # };
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let spends = client.transparent_mempool_spends_by_outpoint(&outpoints).await?;
    /// # let _ = spends; Ok(()) }
    /// ```
    async fn transparent_mempool_spends_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<Vec<TransparentMempoolSpend>, IndexerError>;

    /// Resolves a batch of outpoints against the live mempool index. Used
    /// when an outpoint references an output of an unconfirmed mempool
    /// transaction (chained-mempool flows).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zinder_client::{
    /// #     EndpointBackedIndex, IndexerError, TransactionId, TransparentOutPoint,
    /// # };
    /// # async fn demo<T: EndpointBackedIndex>(client: &T) -> Result<(), IndexerError> {
    /// let outpoints = [TransparentOutPoint::new(TransactionId::from_bytes([0u8; 32]), 0)];
    /// let prevouts = client.transparent_mempool_outputs_by_outpoint(&outpoints).await?;
    /// # let _ = prevouts; Ok(()) }
    /// ```
    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<TransparentOutputsByOutpointResponse, IndexerError>;
}
