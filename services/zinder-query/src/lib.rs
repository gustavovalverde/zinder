//! Wallet and application query boundary for Zinder.
//!
//! This crate serves indexed artifacts through [`ChainEpochReadApi`] without
//! calling upstream node sources or mutating canonical storage.

use std::{collections::HashSet, fmt, num::NonZeroU32, sync::Arc, time::Instant};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use zinder_core::{
    BlockBlobArtifact, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockId, BlockSelector,
    ChainEpoch, ChainEpochId, CompactBlockArtifact, MAX_RAW_TRANSACTION_BYTES, MinedDetails,
    MinedTransaction, NetworkUpgradeActivations, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange, TransactionBlobArtifact,
    TransactionBroadcastResult, TransactionId, TransactionLocation, TransparentAddressBalance,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TransparentOutPoint,
    TransparentOutputEntry, TransparentOutputsByOutpointResponse, TransparentSpendEntry,
    TransparentSpendsByOutpointResponse, TransparentUnspentOutput,
    TransparentUnspentOutputsByOutpointResponse, TransparentUtxoSetSummary, TxStatus,
};
use zinder_derive::DeriveStore;
use zinder_proto::capabilities::{
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1, WALLET_READ_TRANSPARENT_SPENDS_V1,
};
use zinder_source::{SourceError, TransactionBroadcaster, TreeStateUpstream};
use zinder_store::{
    AddressOutputIndexPageRequest, ArtifactFamily, BlockHashLookup, ChainEpochReadApi,
    ChainEventEnvelope, ChainEventHistoryRequest, ChainEventStreamFamily, ChainEventStreamResume,
    DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS, EventStreamStartPosition, StoreError,
    StreamCursorTokenV1,
};

mod grpc;
mod readiness_refresh;
mod wallet_projection_read;

pub use grpc::{
    ServerInfoSettings, UpstreamNodeCapabilities, WalletQueryGrpcAdapter,
    address_lookup_to_script_hash, block_header_by_selector_response,
    block_id_by_selector_response, broadcast_transaction_response,
    build_transparent_address_tx_ids_chunk, build_transparent_address_tx_ids_header,
    build_transparent_unspent_output_message, build_transparent_unspent_outputs_header,
    build_wallet_server_info, chain_events_response, compact_block_response, full_block_response,
    latest_block_response, latest_tree_state_checkpoint_response,
    network_upgrade_activations_response, status_from_query_error, subtree_roots_response,
    transaction_response, transparent_address_tx_ids_response,
    transparent_address_unspent_outputs_response, transparent_outputs_by_outpoint_response,
    transparent_spends_by_outpoint_response, transparent_unspent_outputs_by_outpoint_response,
    tree_state_at_response,
};
pub use readiness_refresh::{
    DEFAULT_READINESS_REFRESH_INTERVAL, SecondaryCatchupOptions, WriterStatusConfig,
    spawn_readiness_refresh, spawn_secondary_catchup,
};
pub use wallet_projection_read::{
    ProjectionRead, TransparentAddressHistoryPage, WalletProjectionPosition,
    WalletProjectionReadApi, WalletProjectionReadError, WalletProjectionReadiness,
    derive_store_wallet_projection_reader,
};

/// Wallet-facing read API backed by epoch-bound canonical reads.
///
/// Canonical reads take `at_epoch_id: Option<ChainEpochId>`. `None` resolves to
/// the visible chain epoch at call time; `Some(id)` pins the read to that epoch.
/// Current-projection derive reads expose their chain epoch in the response
/// instead of accepting a pin.
#[async_trait]
pub trait WalletQueryApi: Send + Sync + 'static {
    /// Returns the network-upgrade activation table advertised by the
    /// configured upstream node.
    async fn network_upgrade_activations(&self) -> Result<NetworkUpgradeActivations, QueryError>;

    /// Reads latest visible block metadata.
    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestBlock, QueryError>;

    /// Reads the block at the chain epoch's safe tip (the wallet's scan
    /// ceiling). Mirrors `latest_block` but resolves to
    /// `chain_epoch.safe_tip_height` rather than `chain_epoch.tip_height`.
    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestSafeBlock, QueryError>;

    /// Resolves a typed block selector against the canonical best chain.
    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockIdResponseValue, QueryError>;

    /// Reads the typed block-header read model at a typed block selector.
    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderResponseValue, QueryError>;

    /// Reads one compact block artifact at a given height.
    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlock, QueryError>;

    /// Reads compact block artifacts for an inclusive height range.
    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockRange, QueryError>;

    /// Reads one full serialized block at a given height.
    ///
    /// Served from the stored block blob, present only when the writer
    /// deployment retains block blobs (`raw_blob_policy = "all"`). Heights with
    /// no retained blob return [`QueryError::ArtifactUnavailable`].
    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlock, QueryError>;

    /// Streams full serialized blocks for an inclusive height range under one
    /// pinned chain epoch.
    ///
    /// Served from stored block blobs. The whole stream reads one epoch,
    /// resolved before this call returns, so an epoch-pin failure surfaces as
    /// the returned error rather than mid-stream. Blocks arrive ascending and
    /// contiguous; the first height with no retained blob terminates the
    /// stream with [`QueryError::ArtifactUnavailable`] after the blocks
    /// already delivered.
    async fn full_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlockStream, QueryError>;

    /// Reads typed transaction status by transaction id.
    ///
    /// Returns [`TxStatus::Mined`] for mined transactions, with epoch-bound
    /// [`MinedDetails`] enrichment, [`TxStatus::NotFound`] when the
    /// transaction is not visible in the canonical chain, and
    /// [`QueryError`] for storage/upstream failures. An `at_epoch_id` pin is
    /// a canonical-chain read and never consults live mempool state;
    /// mempool fall-through is the caller's responsibility on this trait.
    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransactionStatus, QueryError>;

    /// Reads the indexed transaction at `(height, tx_index)`.
    ///
    /// `tx_index` is the transaction's position within the full block, as
    /// produced by ingestion. The lookup decodes the indexed compact block
    /// at `height` and matches on the per-transaction index recorded there.
    async fn transaction_at_block_index(
        &self,
        height: BlockHeight,
        tx_index: u64,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Transaction, QueryError>;

    /// Reads an optional raw transaction blob by transaction id.
    async fn raw_transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<RawTransaction, QueryError>;

    /// Resolves a batch of canonical-chain transparent outpoints to their
    /// referenced outputs.
    ///
    /// Reads each unique outpoint from the canonical transparent-output
    /// index. Outpoints that do not resolve at the response's [`ChainEpoch`]
    /// return an entry with `prevout = None`. The response preserves input
    /// order; duplicate outpoints emit duplicate entries.
    ///
    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, QueryError>;

    /// Resolves a batch of canonical-chain transparent outpoints to where each
    /// was spent on the canonical chain.
    ///
    /// Reads the canonical spend-fact index. Outpoints unspent at the
    /// response's [`ChainEpoch`] produce no entry; consumers key results by
    /// `spent_outpoint`. Coinbase inputs spend no prevout and never appear.
    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, QueryError>;

    /// Resolves a batch of transparent outpoints to their referenced output,
    /// keeping each only while it is unspent on the canonical chain at the
    /// response's [`ChainEpoch`] (gettxout-equivalent, null-if-spent).
    ///
    /// Composes the canonical output resolver and the canonical spend-fact
    /// reader at one pinned epoch. An outpoint emits an entry only when the
    /// output is present and carries no canonical spend; spent or never-existed
    /// outpoints produce no entry, so every entry's `output` is present.
    /// Consumers key results by `outpoint`; duplicate outpoints collapse to one
    /// entry. The read is canonical-only: mempool-aware unspent-ness composes
    /// with [`Self::transparent_spends_by_outpoint`]'s mempool counterpart.
    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUnspentOutputsByOutpointResponse, QueryError>;

    /// Reads the complete unspent transparent output set for one transparent
    /// address script at a single pinned chain epoch.
    async fn transparent_address_unspent_outputs(
        &self,
        request: TransparentAddressUnspentOutputsRequest,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressUnspentOutputs, QueryError>;

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts.
    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
    ) -> Result<TransparentAddressTxIds, QueryError>;

    /// Sums the canonical confirmed balance across one or more transparent
    /// addresses at a single pinned chain epoch.
    ///
    /// Folds the complete unspent transparent output set of every requested
    /// address into one saturating `confirmed_zat` total. The returned
    /// [`TransparentAddressBalance`] carries `unconfirmed_delta_zat = 0`: the
    /// canonical read knows nothing about live mempool state. The wallet-side
    /// gRPC adapter overlays the signed mempool delta on top of this total
    /// when an ingest-control endpoint is wired.
    ///
    /// Rejects an empty address list and any list above
    /// [`MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES`] with
    /// [`QueryError::TransparentBalanceAddressCountExceeded`] so one request
    /// cannot fan out into an unbounded number of unspent-set reads.
    async fn transparent_address_balance(
        &self,
        addresses: Vec<TransparentAddressScriptHash>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressBalance, QueryError>;

    /// Aggregates the chain-wide transparent UTXO set at the settled tip.
    ///
    /// Folds the canonical current-UTXO projection into an unspent count and a
    /// total value (gettxoutsetinfo-equivalent) by a request-time full scan.
    /// The aggregate is taken at the resolved epoch's settled tip, where the
    /// projection is the irreversible unspent set. When `commitment_enabled`
    /// is set the same scan also folds the `LtHash16` homomorphic commitment.
    async fn transparent_utxo_set_summary(
        &self,
        at_epoch_id: Option<ChainEpochId>,
        commitment_enabled: bool,
    ) -> Result<TransparentUtxoSetSummary, QueryError>;

    /// Reads the tree state at exactly `height`.
    ///
    /// Served from a stored checkpoint when one exists at `height`, otherwise
    /// filled from the configured upstream node. The returned height always
    /// equals `height`.
    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError>;

    /// Reads the latest tree-state checkpoint at the visible chain epoch tip.
    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError>;

    /// Reads subtree-root artifacts for a bounded subtree range.
    async fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<SubtreeRoots, QueryError>;

    /// Reads a bounded page of replayable chain events.
    async fn chain_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEvents, QueryError>;

    /// Resolves an event-stream start position for the chain-event family
    /// once at subscribe time.
    async fn resolve_chain_events_start(
        &self,
        start: EventStreamStartPosition,
        requested_family: ChainEventStreamFamily,
    ) -> Result<ChainEventStreamResume, QueryError>;

    /// Broadcasts a raw transaction without mutating canonical storage.
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, QueryError>;
}

/// Query boundary backed by a [`ChainEpochReadApi`] implementation.
///
/// Pass `()` as the broadcaster to disable transaction broadcast.
#[derive(Clone)]
pub struct WalletQuery<ReadApi, Broadcaster = ()> {
    read_api: ReadApi,
    wallet_projection_reader: Option<Arc<dyn WalletProjectionReadApi>>,
    transaction_broadcaster: Broadcaster,
    options: WalletQueryOptions,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    tree_state_upstream: Option<Arc<dyn TreeStateUpstream>>,
}

impl<ReadApi: fmt::Debug, Broadcaster: fmt::Debug> fmt::Debug
    for WalletQuery<ReadApi, Broadcaster>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletQuery")
            .field("read_api", &self.read_api)
            .field("wallet_projection_reader", &self.wallet_projection_reader)
            .field("transaction_broadcaster", &self.transaction_broadcaster)
            .field("options", &self.options)
            .field(
                "network_upgrade_activations",
                &self.network_upgrade_activations,
            )
            .field("tree_state_upstream", &self.tree_state_upstream.is_some())
            .finish()
    }
}

/// Default maximum compact-block count returned by one range call.
pub const DEFAULT_MAX_COMPACT_BLOCK_RANGE: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

/// Default maximum full-block count returned by one range call.
///
/// Sized to one wallet initial-scan batch so a 1000-block window is a single
/// stream. The range is served demand-driven, so this bounds request width,
/// not memory.
pub const DEFAULT_MAX_FULL_BLOCK_RANGE: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

/// Blocks read per store multi-get while streaming a full-block range.
///
/// Caps the producer's working set to one sub-read regardless of window width.
const FULL_BLOCK_STREAM_SUB_READ_BLOCKS: u32 = 16;

/// Bounded depth of the full-block range channel.
///
/// In-flight blocks a slow consumer can stall behind stay near this many blobs
/// plus one sub-read, so per-stream memory never scales with the window.
pub const FULL_BLOCK_STREAM_CHANNEL_CAPACITY: usize = 4;

/// Hard cap on the number of addresses one balance request may sum across.
///
/// Each address fans out into one complete unspent-output read (and, in the
/// gRPC adapter, one mempool point lookup), so an unbounded list would let one
/// request issue thousands of reads. `u32` matches the `address_count` field
/// on the response so the bound check happens in the wire type's native width.
pub const MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES: u32 = 256;

/// Runtime options for [`WalletQuery`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletQueryOptions {
    /// Maximum compact-block count returned by one range call.
    pub max_compact_block_range: NonZeroU32,
    /// Maximum full-block count returned by one range call.
    pub max_full_block_range: NonZeroU32,
}

impl Default for WalletQueryOptions {
    fn default() -> Self {
        Self {
            max_compact_block_range: DEFAULT_MAX_COMPACT_BLOCK_RANGE,
            max_full_block_range: DEFAULT_MAX_FULL_BLOCK_RANGE,
        }
    }
}

impl<ReadApi, Broadcaster> WalletQuery<ReadApi, Broadcaster> {
    /// Creates a wallet query boundary backed by `read_api` and
    /// `transaction_broadcaster`.
    ///
    /// `network_upgrade_activations` is the node-discovered upgrade table
    /// that populates `MinedDetails.consensus_branch_id` for mined
    /// transactions with the value actually active at the mined height on
    /// the configured network. In production it is shared via
    /// `ZebraJsonRpcSource::discover_network_upgrade_activations`; tests use
    /// `zinder_testkit::sample_regtest_upgrade_activations`.
    #[must_use]
    pub fn new(
        read_api: ReadApi,
        transaction_broadcaster: Broadcaster,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        Self::with_options(
            read_api,
            transaction_broadcaster,
            network_upgrade_activations,
            WalletQueryOptions::default(),
        )
    }

    /// Creates a wallet query boundary with explicit runtime options.
    #[must_use]
    pub const fn with_options(
        read_api: ReadApi,
        transaction_broadcaster: Broadcaster,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
        options: WalletQueryOptions,
    ) -> Self {
        Self {
            read_api,
            wallet_projection_reader: None,
            transaction_broadcaster,
            options,
            network_upgrade_activations,
            tree_state_upstream: None,
        }
    }

    /// Attaches the derive-store reader used for derive-owned wallet projections.
    #[must_use]
    pub fn with_derive_store(mut self, derive_store: DeriveStore) -> Self {
        self.wallet_projection_reader = Some(derive_store_wallet_projection_reader(derive_store));
        self
    }

    /// Attaches a typed wallet-projection reader.
    #[must_use]
    pub fn with_wallet_projection_reader(
        mut self,
        wallet_projection_reader: Arc<dyn WalletProjectionReadApi>,
    ) -> Self {
        self.wallet_projection_reader = Some(wallet_projection_reader);
        self
    }

    /// Attaches the upstream node used to fill tree states at heights without a
    /// stored checkpoint. Without it, `tree_state_at` serves only stored
    /// checkpoint heights and returns `ArtifactUnavailable` for the gaps.
    #[must_use]
    pub fn with_tree_state_upstream(mut self, source: Arc<dyn TreeStateUpstream>) -> Self {
        self.tree_state_upstream = Some(source);
        self
    }
}

/// Outcome of the synchronous store probe in [`WalletQuery::tree_state_at`].
enum TreeStateProbe {
    /// A stored checkpoint sits exactly at the requested height.
    Stored(TreeState),
    /// No stored checkpoint at the requested height; fill from the upstream node.
    Fill {
        chain_epoch: ChainEpoch,
        block_id: BlockId,
    },
}

#[allow(
    clippy::too_many_lines,
    reason = "The query implementation mirrors the WalletQueryApi contract one method at a time."
)]
#[async_trait]
impl<ReadApi, Broadcaster> WalletQueryApi for WalletQuery<ReadApi, Broadcaster>
where
    ReadApi: ChainEpochReadApi + Clone + Send + Sync + 'static,
    Broadcaster: TransactionBroadcaster + Clone,
{
    async fn network_upgrade_activations(&self) -> Result<NetworkUpgradeActivations, QueryError> {
        Ok((*self.network_upgrade_activations).clone())
    }

    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            Ok(LatestBlock {
                chain_epoch,
                height: chain_epoch.visible_tip_height,
                block_hash: chain_epoch.visible_tip_hash,
            })
        }))
        .await;
        record_wallet_query_outcome("latest_block", started_at, &query_outcome, None);
        query_outcome
    }

    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestSafeBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            Ok(LatestSafeBlock {
                chain_epoch,
                height: chain_epoch.settled_tip_height,
                block_hash: chain_epoch.settled_tip_hash,
            })
        }))
        .await;
        record_wallet_query_outcome("latest_safe_block", started_at, &query_outcome, None);
        query_outcome
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockIdResponseValue, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let block_id = resolve_block_selector(&reader, selector)?;
            Ok(BlockIdResponseValue {
                chain_epoch,
                block_id,
            })
        }))
        .await;
        record_wallet_query_outcome("block_id_by_selector", started_at, &query_outcome, None);
        query_outcome
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderResponseValue, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let block_id = resolve_block_selector(&reader, selector)?;
            let block = reader.block_header_at(block_id.height)?.ok_or_else(|| {
                block_height_artifact_unavailable(ArtifactFamily::BlockHeader, block_id.height)
            })?;
            Ok(BlockHeaderResponseValue {
                chain_epoch,
                block_header: block.into_header_info(),
            })
        }))
        .await;
        record_wallet_query_outcome("block_header_by_selector", started_at, &query_outcome, None);
        query_outcome
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let compact_block = match reader.compact_block_at(height) {
                Ok(Some(compact_block)) => compact_block,
                Ok(None)
                | Err(
                    StoreError::ArtifactMissing {
                        family: ArtifactFamily::CompactBlock,
                        ..
                    }
                    | StoreError::CanonicalHistoryUnavailable { .. },
                ) => {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::CompactBlock,
                        height,
                    ));
                }
                Err(error) => return Err(QueryError::Store(error)),
            };

            Ok(CompactBlock {
                chain_epoch,
                compact_block,
            })
        }))
        .await;
        record_wallet_query_outcome("compact_block_at", started_at, &query_outcome, None);
        query_outcome
    }

    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransactionStatus, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let activations = self.network_upgrade_activations.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let Some(artifact) = reader.transaction_facts_by_id(transaction_id)? else {
                return Ok(TransactionStatus {
                    chain_epoch,
                    status: TxStatus::NotFound,
                });
            };
            let block_time = reader
                .block_header_at(artifact.location.block_height)?
                .map(|block| block.block_time)
                .unwrap_or_default();
            let consensus_branch_id =
                activations.consensus_branch_id_at(artifact.location.block_height);
            let details = MinedDetails::from_response_epoch(
                &chain_epoch,
                artifact.location.block_height,
                consensus_branch_id,
                block_time,
            );
            let raw_transaction_bytes = reader
                .transaction_blob_by_id(transaction_id)?
                .map(|blob| blob.raw_transaction_bytes);
            Ok(TransactionStatus {
                chain_epoch,
                status: TxStatus::Mined(MinedTransaction::new(
                    artifact.location,
                    details,
                    raw_transaction_bytes,
                )),
            })
        }))
        .await;
        record_wallet_query_outcome("transaction_by_id", started_at, &query_outcome, None);
        query_outcome
    }

    async fn transaction_at_block_index(
        &self,
        height: BlockHeight,
        tx_index: u64,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Transaction, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let tx_index_in_block = u32::try_from(tx_index).map_err(|_| {
                artifact_unavailable(
                    ArtifactFamily::BlockTransactionIndex,
                    ArtifactKey::BlockTransactionIndex { height, tx_index },
                )
            })?;
            let transaction_id = reader
                .transaction_id_at_block_index(height, tx_index_in_block)?
                .ok_or_else(|| {
                    artifact_unavailable(
                        ArtifactFamily::BlockTransactionIndex,
                        ArtifactKey::BlockTransactionIndex { height, tx_index },
                    )
                })?;
            let transaction = reader
                .transaction_location_by_id(transaction_id)?
                .ok_or_else(|| {
                    artifact_unavailable(
                        ArtifactFamily::TransactionLocation,
                        ArtifactKey::TransactionId(transaction_id),
                    )
                })?;

            Ok(Transaction {
                chain_epoch,
                transaction,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transaction_at_block_index",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn raw_transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<RawTransaction, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let transaction = reader
                .transaction_blob_by_id(transaction_id)?
                .ok_or_else(|| {
                    artifact_unavailable(
                        ArtifactFamily::TransactionBlob,
                        ArtifactKey::TransactionId(transaction_id),
                    )
                })?;

            Ok(RawTransaction {
                chain_epoch,
                transaction,
            })
        }))
        .await;
        record_wallet_query_outcome("raw_transaction", started_at, &query_outcome, None);
        query_outcome
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();

            let prevouts_by_outpoint = reader.transparent_outputs_by_outpoints(&outpoints)?;

            let mut entries = Vec::with_capacity(outpoints.len());
            for outpoint in outpoints {
                let prevout = prevouts_by_outpoint
                    .get(&outpoint)
                    .cloned()
                    .map(zinder_core::TransparentOutputArtifact::into_output);
                entries.push(TransparentOutputEntry {
                    outpoint,
                    output: prevout,
                });
            }

            Ok(TransparentOutputsByOutpointResponse {
                chain_epoch,
                entries,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_outputs_by_outpoint",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let wallet_projection_reader = self.wallet_projection_reader.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();

            let canonical = reader.transparent_spend_facts_by_outpoints(&outpoints)?;

            let mut spends = Vec::with_capacity(outpoints.len());
            let mut seen = HashSet::with_capacity(outpoints.len());
            let mut canonical_misses = Vec::new();
            for outpoint in &outpoints {
                if let Some(fact) = canonical.get(outpoint) {
                    if seen.insert(*outpoint) {
                        spends.push(TransparentSpendEntry::from_spend_fact(fact));
                    }
                } else {
                    canonical_misses.push(*outpoint);
                }
            }

            if !canonical_misses.is_empty() {
                if let Some(wallet_projection_reader) = wallet_projection_reader.as_deref() {
                    for entry in resolve_swept_spends_from_derive(
                        wallet_projection_reader,
                        &reader,
                        chain_epoch.settled_tip_height,
                        &canonical_misses,
                    )? {
                        if seen.insert(entry.spent_outpoint) {
                            spends.push(entry);
                        }
                    }
                } else {
                    ensure_spend_lookup_complete_without_derive(&reader)?;
                }
            }

            Ok(TransparentSpendsByOutpointResponse {
                chain_epoch,
                spends,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_spends_by_outpoint",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUnspentOutputsByOutpointResponse, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let entries = reader.transparent_unspent_outputs_by_outpoints(&outpoints)?;

            Ok(TransparentUnspentOutputsByOutpointResponse {
                chain_epoch,
                entries,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_unspent_outputs_by_outpoint",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockRange, QueryError> {
        let started_at = Instant::now();
        let requested_blocks = block_range.into_iter().len();
        if let Err(error) = validate_block_range(block_range, self.options.max_compact_block_range)
        {
            let query_outcome = Err(error);
            record_wallet_query_outcome(
                "compact_blocks_in_range",
                started_at,
                &query_outcome,
                Some(requested_blocks),
            );
            return query_outcome;
        }

        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let compact_blocks = reader
                .compact_blocks_in_range(block_range)
                .map_err(|error| {
                    map_height_artifact_store_error(
                        error,
                        ArtifactFamily::CompactBlock,
                        block_range.start,
                    )
                })?;
            let mut available_compact_blocks = Vec::with_capacity(compact_blocks.len());

            for (height, compact_block) in block_range.into_iter().zip(compact_blocks) {
                let Some(compact_block) = compact_block else {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::CompactBlock,
                        height,
                    ));
                };

                available_compact_blocks.push(compact_block);
            }

            Ok(CompactBlockRange {
                chain_epoch,
                block_range,
                compact_blocks: available_compact_blocks,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "compact_blocks_in_range",
            started_at,
            &query_outcome,
            Some(requested_blocks),
        );
        query_outcome
    }

    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let block_blob = reader.block_blob_at(height)?.ok_or_else(|| {
                block_height_artifact_unavailable(ArtifactFamily::BlockBlob, height)
            })?;

            Ok(FullBlock {
                chain_epoch,
                block_blob,
            })
        }))
        .await;
        record_wallet_query_outcome("full_block_at", started_at, &query_outcome, None);
        query_outcome
    }

    async fn full_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlockStream, QueryError> {
        let started_at = Instant::now();
        let requested_blocks = block_range.into_iter().len();
        let query_outcome = spawn_full_block_stream(
            self.read_api.clone(),
            self.options.max_full_block_range,
            block_range,
            at_epoch_id,
        )
        .await;
        record_wallet_query_outcome(
            "full_blocks_in_range",
            started_at,
            &query_outcome,
            Some(requested_blocks),
        );
        query_outcome
    }

    async fn transparent_address_unspent_outputs(
        &self,
        request: TransparentAddressUnspentOutputsRequest,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressUnspentOutputs, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let at_epoch = match at_epoch_id {
                Some(_) => Some(open_chain_epoch_reader(&read_api, at_epoch_id)?.chain_epoch()),
                None => None,
            };
            let page = read_api
                .address_output_index_page(AddressOutputIndexPageRequest {
                    at_epoch,
                    address_script_hash: request.address_script_hash,
                    start_height: request.start_height,
                    max_entries: NonZeroU32::MAX,
                    from_cursor: None,
                })
                .map_err(QueryError::Store)?;

            Ok(TransparentAddressUnspentOutputs {
                chain_epoch: page.chain_epoch,
                outputs: page.outputs,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_address_unspent_outputs",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
    ) -> Result<TransparentAddressTxIds, QueryError> {
        let started_at = Instant::now();
        if request.start_height > request.end_height {
            let outcome = Err(QueryError::InvalidBlockRange {
                start_height: request.start_height,
                end_height: request.end_height,
            });
            record_wallet_query_outcome(
                "transparent_address_tx_ids_in_range",
                started_at,
                &outcome,
                None,
            );
            return outcome;
        }

        let Some(wallet_projection_reader) = self.wallet_projection_reader.clone() else {
            let outcome = Err(QueryError::DeriveUnavailable {
                capability: WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
            });
            record_wallet_query_outcome(
                "transparent_address_tx_ids_in_range",
                started_at,
                &outcome,
                None,
            );
            return outcome;
        };
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = read_api
                .current_chain_epoch_reader()
                .map_err(QueryError::Store)?;
            let chain_epoch = reader.chain_epoch();
            let projection_read = wallet_projection_reader
                .transparent_address_history_page(&request)
                .map_err(|error| {
                    map_wallet_projection_read_error(error, WALLET_ADDRESS_TRANSPARENT_HISTORY_V1)
                })?;
            let derive_height = projection_read.materialized_height;
            if derive_height.is_none_or(|height| height < chain_epoch.visible_tip_height) {
                return Err(QueryError::DeriveLag {
                    capability: WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
                    chain_tip_height: chain_epoch.visible_tip_height,
                    derive_height,
                });
            }
            Ok(TransparentAddressTxIds {
                chain_epoch,
                artifacts: projection_read.value.artifacts,
                next_cursor: projection_read.value.next_cursor,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_address_tx_ids_in_range",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_address_balance(
        &self,
        addresses: Vec<TransparentAddressScriptHash>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressBalance, QueryError> {
        let started_at = Instant::now();
        let address_count = u32::try_from(addresses.len())
            .ok()
            .filter(|count| (1..=MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES).contains(count));
        let Some(address_count) = address_count else {
            let outcome = Err(QueryError::TransparentBalanceAddressCountExceeded {
                requested: addresses.len(),
                maximum: MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES,
            });
            record_wallet_query_outcome("transparent_address_balance", started_at, &outcome, None);
            return outcome;
        };
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let at_epoch = at_epoch_id.is_some().then_some(chain_epoch);
            let mut confirmed_zat: u64 = 0;
            for address_script_hash in addresses {
                let page = read_api
                    .address_output_index_page(AddressOutputIndexPageRequest {
                        at_epoch,
                        address_script_hash,
                        start_height: BlockHeight::new(0),
                        max_entries: NonZeroU32::MAX,
                        from_cursor: None,
                    })
                    .map_err(QueryError::Store)?;
                for output in &page.outputs {
                    confirmed_zat = confirmed_zat.saturating_add(output.value_zat);
                }
            }
            Ok(TransparentAddressBalance {
                confirmed_zat,
                unconfirmed_delta_zat: 0,
                address_count,
                chain_epoch,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_address_balance",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_utxo_set_summary(
        &self,
        at_epoch_id: Option<ChainEpochId>,
        commitment_enabled: bool,
    ) -> Result<TransparentUtxoSetSummary, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            reader
                .transparent_utxo_set_summary(commitment_enabled)
                .map_err(QueryError::Store)
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_utxo_set_summary",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let probe = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            if height > chain_epoch.visible_tip_height {
                return Err(block_height_artifact_unavailable(
                    ArtifactFamily::TreeState,
                    height,
                ));
            }

            let stored = match reader.tree_state_checkpoint_at_or_before(height) {
                Ok(stored) => stored,
                Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::TreeState,
                    ..
                }) => None,
                Err(error @ StoreError::CanonicalHistoryUnavailable { .. }) => {
                    return Err(map_height_artifact_store_error(
                        error,
                        ArtifactFamily::TreeState,
                        height,
                    ));
                }
                Err(error) => return Err(QueryError::Store(error)),
            };
            if let Some(stored) = stored
                && stored.height == height
            {
                return Ok(TreeStateProbe::Stored(TreeState {
                    chain_epoch,
                    height: stored.height,
                    block_hash: stored.block_hash,
                    payload_bytes: stored.payload_bytes,
                }));
            }

            let block = reader
                .block_header_at(height)
                .map_err(|error| {
                    map_height_artifact_store_error(error, ArtifactFamily::TreeState, height)
                })?
                .ok_or_else(|| {
                    block_height_artifact_unavailable(ArtifactFamily::TreeState, height)
                })?;
            Ok(TreeStateProbe::Fill {
                chain_epoch,
                block_id: BlockId::new(height, block.block_hash),
            })
        }))
        .await;

        let query_outcome = match probe {
            Ok(TreeStateProbe::Stored(tree_state)) => Ok(tree_state),
            Ok(TreeStateProbe::Fill {
                chain_epoch,
                block_id,
            }) => match self.tree_state_upstream.as_ref() {
                Some(source) => {
                    let height = block_id.height;
                    let block_hash = block_id.hash;
                    source
                        .fetch_tree_state_for_block(block_id)
                        .await
                        .map(|source_tree_state| TreeState {
                            chain_epoch,
                            height,
                            block_hash,
                            payload_bytes: source_tree_state.payload_bytes,
                        })
                        .map_err(QueryError::Node)
                }
                None => Err(block_height_artifact_unavailable(
                    ArtifactFamily::TreeState,
                    block_id.height,
                )),
            },
            Err(error) => Err(error),
        };
        record_wallet_query_outcome("tree_state_at", started_at, &query_outcome, None);
        query_outcome
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();

            let tree_state = match reader.latest_tree_state_checkpoint() {
                Ok(Some(tree_state)) => tree_state,
                Ok(None)
                | Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::TreeState,
                    ..
                }) => {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::TreeState,
                        chain_epoch.visible_tip_height,
                    ));
                }
                Err(error) => return Err(QueryError::Store(error)),
            };

            Ok(TreeState {
                chain_epoch,
                height: tree_state.height,
                block_hash: tree_state.block_hash,
                payload_bytes: tree_state.payload_bytes,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "latest_tree_state_checkpoint",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<SubtreeRoots, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch_id)?;
            let chain_epoch = reader.chain_epoch();
            let completed_subtree_count =
                completed_subtree_count(chain_epoch, subtree_root_range.protocol);

            if subtree_root_range.start_index.value() >= completed_subtree_count {
                return Ok(SubtreeRoots {
                    chain_epoch,
                    protocol: subtree_root_range.protocol,
                    start_index: subtree_root_range.start_index,
                    subtree_roots: Vec::new(),
                });
            }

            let available_entries = completed_subtree_count
                .saturating_sub(subtree_root_range.start_index.value())
                .min(subtree_root_range.max_entries.get());
            let available_entries = NonZeroU32::new(available_entries).ok_or_else(|| {
                subtree_root_artifact_unavailable(
                    subtree_root_range.protocol,
                    subtree_root_range.start_index,
                )
            })?;
            let available_range = SubtreeRootRange::new(
                subtree_root_range.protocol,
                subtree_root_range.start_index,
                available_entries,
            );
            let subtree_roots = reader.subtree_roots(available_range)?;
            let mut available_subtree_roots = Vec::with_capacity(subtree_roots.len());

            for (subtree_index, subtree_root) in available_range.into_iter().zip(subtree_roots) {
                let Some(subtree_root) = subtree_root else {
                    return Err(subtree_root_artifact_unavailable(
                        subtree_root_range.protocol,
                        subtree_index,
                    ));
                };

                available_subtree_roots.push(subtree_root);
            }

            Ok(SubtreeRoots {
                chain_epoch,
                protocol: subtree_root_range.protocol,
                start_index: subtree_root_range.start_index,
                subtree_roots: available_subtree_roots,
            })
        }))
        .await;
        record_wallet_query_outcome("subtree_roots_in_range", started_at, &query_outcome, None);
        query_outcome
    }

    async fn chain_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEvents, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let event_envelopes = read_api
                .chain_event_history(ChainEventHistoryRequest::new_for_family(
                    from_cursor.as_ref(),
                    family,
                    DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS,
                ))
                .map_err(map_chain_event_store_error)?;

            Ok(ChainEvents { event_envelopes })
        }))
        .await;
        record_wallet_query_outcome("chain_events", started_at, &query_outcome, None);
        query_outcome
    }

    async fn resolve_chain_events_start(
        &self,
        start: EventStreamStartPosition,
        requested_family: ChainEventStreamFamily,
    ) -> Result<ChainEventStreamResume, QueryError> {
        let read_api = self.read_api.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            read_api
                .resolve_chain_event_stream_start(&start, requested_family)
                .map_err(map_chain_event_store_error)
        }))
        .await
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, QueryError> {
        let started_at = Instant::now();
        let broadcast_outcome = async {
            guard_broadcast_payload_size(&raw_transaction)?;
            self.transaction_broadcaster
                .broadcast_transaction(raw_transaction)
                .await
                .map_err(map_broadcast_source_error)
        }
        .await;
        record_wallet_query_outcome(
            "broadcast_transaction",
            started_at,
            &broadcast_outcome,
            None,
        );
        broadcast_outcome
    }
}

/// Resolves canonical spend-fact misses against the durable
/// transparent-outpoint-spend projection.
///
/// The canonical read is authoritative inside the reorg window and above the
/// deleted-through marker. Below that marker canonical holds no spend fact, so
/// the projection is the only source of spender identity. Only spends at or
/// below the pinned epoch's settled tip are surfaced: an above-settled-tip miss
/// keeps today's semantics (absent means no fact visible in the window). The
/// read refuses with the derive-lag vocabulary only when facts were actually
/// deleted below the projection's durable height, so an empty projection on a
/// store that never swept still returns the correct absent answer.
fn resolve_swept_spends_from_derive(
    wallet_projection_reader: &dyn WalletProjectionReadApi,
    reader: &zinder_store::ChainEpochReader<'_>,
    settled_tip_height: BlockHeight,
    canonical_misses: &[TransparentOutPoint],
) -> Result<Vec<TransparentSpendEntry>, QueryError> {
    let deleted_through = reader
        .transparent_retention_deleted_through_height()
        .map_err(QueryError::Store)?
        .map_or(0, BlockHeight::value);
    let projection_read = wallet_projection_reader
        .transparent_outpoint_spenders(canonical_misses)
        .map_err(|error| {
            map_wallet_projection_read_error(error, WALLET_READ_TRANSPARENT_SPENDS_V1)
        })?;
    let derive_height = projection_read.materialized_height;
    if derive_height.map_or(0, BlockHeight::value) < deleted_through {
        return Err(QueryError::DeriveLag {
            capability: WALLET_READ_TRANSPARENT_SPENDS_V1,
            chain_tip_height: BlockHeight::new(deleted_through),
            derive_height,
        });
    }
    let derive_hits = projection_read.value;
    let mut resolved = Vec::with_capacity(canonical_misses.len().min(derive_hits.len()));
    for outpoint in canonical_misses {
        let Some(entry) = derive_hits.get(outpoint) else {
            continue;
        };
        if entry.spending_block_height <= settled_tip_height
            && derive_spend_matches_canonical_header(reader, entry)?
        {
            resolved.push(entry.clone());
        }
    }
    Ok(resolved)
}

/// Refuses an ambiguous canonical miss after retention has deleted spend facts.
///
/// Before the first real deletion, a canonical miss proves that no spend fact
/// exists at the pinned epoch. Afterwards, only the durable derive projection
/// can distinguish an old spent outpoint from an unspent one.
fn ensure_spend_lookup_complete_without_derive(
    reader: &zinder_store::ChainEpochReader<'_>,
) -> Result<(), QueryError> {
    if reader
        .transparent_retention_deleted_through_height()
        .map_err(QueryError::Store)?
        .is_some()
    {
        return Err(QueryError::DeriveUnavailable {
            capability: WALLET_READ_TRANSPARENT_SPENDS_V1,
        });
    }
    Ok(())
}

/// Confirms a projection spend row still names the canonical block at its
/// spending height.
///
/// The projection records rows for in-window blocks a later reorg can remove.
/// Until the tailer replays that reorg the stale row survives, and once the
/// safe tip advances past its height the settled-tip filter alone would surface
/// it as the spender. Cross-checking the row's stored block hash against the
/// retained canonical header (headers are never swept) makes a reorged-out row
/// absent instead of a wrong spending transaction id.
fn derive_spend_matches_canonical_header(
    reader: &zinder_store::ChainEpochReader<'_>,
    entry: &TransparentSpendEntry,
) -> Result<bool, QueryError> {
    let canonical_header = reader
        .block_header_at(entry.spending_block_height)
        .map_err(QueryError::Store)?;
    Ok(canonical_header.is_some_and(|header| header.block_hash == entry.spending_block_hash))
}

fn open_chain_epoch_reader<ReadApi>(
    read_api: &ReadApi,
    at_epoch_id: Option<ChainEpochId>,
) -> Result<zinder_store::ChainEpochReader<'_>, QueryError>
where
    ReadApi: ChainEpochReadApi,
{
    let Some(requested_epoch_id) = at_epoch_id else {
        return read_api
            .current_chain_epoch_reader()
            .map_err(QueryError::Store);
    };

    read_api
        .chain_epoch_reader_at(requested_epoch_id)
        .map_err(|error| map_epoch_pin_store_error(error, requested_epoch_id))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only a missing pinned epoch changes category; all other storage failures keep the shared query storage mapping."
)]
fn map_epoch_pin_store_error(error: StoreError, chain_epoch_id: ChainEpochId) -> QueryError {
    match error {
        StoreError::ChainEpochMissing { .. } => {
            QueryError::ChainEpochPinUnavailable { chain_epoch_id }
        }
        _ => QueryError::Store(error),
    }
}

/// Awaits a `spawn_blocking` task and flattens the join error into a
/// `QueryError` so callers see one consistent error vocabulary.
async fn join_blocking<Output>(
    handle: tokio::task::JoinHandle<Result<Output, QueryError>>,
) -> Result<Output, QueryError> {
    match handle.await {
        Ok(blocking_outcome) => blocking_outcome,
        Err(join_error) => Err(QueryError::BlockingTaskFailed {
            reason: join_error.to_string(),
        }),
    }
}

/// Validates the range, resolves the pinning epoch, then spawns the async
/// driver that streams it.
///
/// The chain epoch resolves before returning, so an over-cap request or an
/// epoch-pin failure is the returned error rather than a mid-stream one. The
/// driver holds no blocking thread across the client drain: each sub-read is
/// its own short blocking task and blocks flow to the sink with async sends.
async fn spawn_full_block_stream<ReadApi>(
    read_api: ReadApi,
    max_full_block_range: NonZeroU32,
    block_range: BlockHeightRange,
    at_epoch_id: Option<ChainEpochId>,
) -> Result<FullBlockStream, QueryError>
where
    ReadApi: ChainEpochReadApi + Clone + Send + 'static,
{
    validate_block_range(block_range, max_full_block_range)?;

    let chain_epoch = resolve_full_block_epoch(read_api.clone(), at_epoch_id).await?;

    let (block_sender, block_receiver) = mpsc::channel(FULL_BLOCK_STREAM_CHANNEL_CAPACITY);
    tokio::spawn(drive_full_block_range(
        read_api,
        chain_epoch.id,
        block_range,
        block_sender,
    ));

    Ok(FullBlockStream {
        chain_epoch,
        blocks: block_receiver,
    })
}

/// Resolves the chain epoch that pins the whole full-block stream.
///
/// Runs in a short blocking task so the pin failure or store error surfaces as
/// the returned error before any block is streamed.
async fn resolve_full_block_epoch<ReadApi>(
    read_api: ReadApi,
    at_epoch_id: Option<ChainEpochId>,
) -> Result<ChainEpoch, QueryError>
where
    ReadApi: ChainEpochReadApi + Send + 'static,
{
    join_blocking(tokio::task::spawn_blocking(move || {
        open_chain_epoch_reader(&read_api, at_epoch_id).map(|reader| reader.chain_epoch())
    }))
    .await
}

/// Async driver that streams one full-block range from a pinned epoch.
///
/// Walks the range in [`FULL_BLOCK_STREAM_SUB_READ_BLOCKS`] sub-reads, each in
/// its own short blocking task re-pinned to `chain_epoch_id`, and forwards each
/// blob to the sink with async sends. The first missing blob, store error,
/// blocking-task failure, or mid-stream epoch sweep ends the stream with a
/// terminal error after the blocks already sent; a dropped receiver stops the
/// walk on the next send.
async fn drive_full_block_range<ReadApi>(
    read_api: ReadApi,
    chain_epoch_id: ChainEpochId,
    block_range: BlockHeightRange,
    block_sender: mpsc::Sender<Result<BlockBlobArtifact, QueryError>>,
) where
    ReadApi: ChainEpochReadApi + Clone + Send + 'static,
{
    let last_height = block_range.end.value();
    let mut sub_read_start = block_range.start.value();
    loop {
        let sub_read_end = sub_read_start
            .saturating_add(FULL_BLOCK_STREAM_SUB_READ_BLOCKS - 1)
            .min(last_height);
        let sub_range = BlockHeightRange::inclusive(
            BlockHeight::new(sub_read_start),
            BlockHeight::new(sub_read_end),
        );
        let block_blobs =
            match read_full_block_sub_range(read_api.clone(), chain_epoch_id, sub_range).await {
                Ok(block_blobs) => block_blobs,
                Err(error) => {
                    let _ = block_sender.send(Err(error)).await;
                    return;
                }
            };
        if !forward_full_block_sub_range(&block_sender, sub_range, block_blobs).await {
            return;
        }
        if sub_read_end == last_height {
            return;
        }
        sub_read_start = sub_read_end.saturating_add(1);
    }
}

/// Reads one epoch-pinned sub-range of block blobs in a short blocking task.
///
/// Re-pins `chain_epoch_id` per call so a mid-stream sweep of the epoch fails
/// this read with [`QueryError::ChainEpochPinUnavailable`]; a panic in the walk
/// surfaces as [`QueryError::BlockingTaskFailed`] rather than silent truncation.
async fn read_full_block_sub_range<ReadApi>(
    read_api: ReadApi,
    chain_epoch_id: ChainEpochId,
    sub_range: BlockHeightRange,
) -> Result<Vec<Option<BlockBlobArtifact>>, QueryError>
where
    ReadApi: ChainEpochReadApi + Send + 'static,
{
    join_blocking(tokio::task::spawn_blocking(move || {
        read_api
            .chain_epoch_reader_at(chain_epoch_id)
            .map_err(|error| map_epoch_pin_store_error(error, chain_epoch_id))?
            .block_blobs_in_range(sub_range)
            .map_err(|error| {
                map_height_artifact_store_error(error, ArtifactFamily::BlockBlob, sub_range.start)
            })
    }))
    .await
}

/// Forwards one sub-read's blobs to the sink in ascending height order.
///
/// Returns `true` when every blob was delivered and the walk should continue. A
/// missing blob is sent as a terminal [`ArtifactFamily::BlockBlob`] unavailable
/// error and returns `false`; a dropped receiver also returns `false`.
async fn forward_full_block_sub_range(
    block_sender: &mpsc::Sender<Result<BlockBlobArtifact, QueryError>>,
    sub_range: BlockHeightRange,
    block_blobs: Vec<Option<BlockBlobArtifact>>,
) -> bool {
    for (height, block_blob) in sub_range.into_iter().zip(block_blobs) {
        let Some(block_blob) = block_blob else {
            let _ = block_sender
                .send(Err(block_height_artifact_unavailable(
                    ArtifactFamily::BlockBlob,
                    height,
                )))
                .await;
            return false;
        };
        if block_sender.send(Ok(block_blob)).await.is_err() {
            return false;
        }
    }
    true
}

fn map_broadcast_source_error(error: SourceError) -> QueryError {
    if matches!(error, SourceError::TransactionBroadcastDisabled) {
        QueryError::TransactionBroadcastDisabled
    } else {
        QueryError::Node(error)
    }
}

fn guard_broadcast_payload_size(raw_transaction: &RawTransactionBytes) -> Result<(), QueryError> {
    let actual = raw_transaction.len();
    if actual > MAX_RAW_TRANSACTION_BYTES {
        return Err(QueryError::BroadcastTransactionTooLarge {
            actual,
            maximum: MAX_RAW_TRANSACTION_BYTES,
        });
    }
    Ok(())
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only cursor-specific store errors become query cursor errors; all other current and future storage failures remain storage failures."
)]
fn map_chain_event_store_error(error: StoreError) -> QueryError {
    match error {
        StoreError::ChainEventCursorInvalid { reason } => {
            QueryError::ChainEventCursorInvalid { reason }
        }
        StoreError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        } => QueryError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence,
        },
        _ => QueryError::Store(error),
    }
}

fn map_wallet_projection_read_error(
    error: WalletProjectionReadError,
    capability: &'static str,
) -> QueryError {
    match error {
        WalletProjectionReadError::ProjectionUnavailable { .. } => {
            QueryError::DeriveUnavailable { capability }
        }
        WalletProjectionReadError::TransparentAddressHistoryCursorInvalid => {
            QueryError::TransparentHistoryCursorInvalid {
                reason: "cursor does not match the transparent-address transaction-history stream",
            }
        }
        WalletProjectionReadError::Storage { source } => {
            QueryError::WalletProjectionRead { source }
        }
    }
}

/// Metric pair the `WalletQuery` adapter emits per request.
const QUERY_RPC_METRICS: zinder_runtime::RpcMetricNames =
    zinder_runtime::RpcMetricNames::for_service(
        "zinder_query_request_duration_seconds",
        "zinder_query_request_total",
    );

/// Registers `# HELP` and `# TYPE` text for every metric this crate emits.
///
/// Call once at startup, after `install_metrics_recorder` returns and before
/// the gRPC server records its first request. Delegates the RPC metric pair
/// to [`zinder_runtime::describe_rpc_metrics`] so every Zinder service
/// shares one description template; the compact-block-range histogram is
/// `WalletQuery`-specific and stays registered here.
pub fn describe_request_metrics() {
    zinder_runtime::describe_rpc_metrics(QUERY_RPC_METRICS, "WalletQuery");
    metrics::describe_histogram!(
        "zinder_query_compact_block_range_block_count",
        metrics::Unit::Count,
        "Number of blocks returned by a single CompactBlockRange response, labelled \
         by status. Used to monitor request fan-out and page sizing."
    );
}

fn record_wallet_query_outcome<Response>(
    operation: &'static str,
    started_at: Instant,
    query_outcome: &Result<Response, QueryError>,
    block_count: Option<usize>,
) {
    let outcome = outcome_from_query_result(query_outcome);
    zinder_runtime::record_rpc_request(QUERY_RPC_METRICS, operation, started_at.elapsed(), outcome);

    if let Some(block_count) = block_count {
        metrics::histogram!(
            "zinder_query_compact_block_range_block_count",
            "status" => outcome.status_label()
        )
        .record(usize_to_u32_saturating(block_count));
    }
}

fn outcome_from_query_result<Response>(
    query_outcome: &Result<Response, QueryError>,
) -> zinder_runtime::RpcOutcome {
    match query_outcome {
        Ok(_) => zinder_runtime::RpcOutcome::Ok,
        Err(error) => zinder_runtime::RpcOutcome::Error {
            class: query_error_class(Some(error)),
        },
    }
}

fn query_error_class(error: Option<&QueryError>) -> &'static str {
    match error {
        None => "none",
        Some(QueryError::InvalidBlockRange { .. }) => "invalid_block_range",
        Some(QueryError::CompactBlockRangeTooLarge { .. }) => "compact_block_range_too_large",
        Some(QueryError::TransparentBalanceAddressCountExceeded { .. }) => {
            "transparent_balance_address_count_exceeded"
        }
        Some(QueryError::ArtifactUnavailable { .. }) => "artifact_unavailable",
        Some(QueryError::CompactBlockPayloadMalformed { .. }) => "compact_block_payload_malformed",
        Some(QueryError::UnsupportedShieldedProtocol { .. }) => "unsupported_shielded_protocol",
        Some(QueryError::ChainEventCursorInvalid { .. }) => "chain_event_cursor_invalid",
        Some(QueryError::ChainEventCursorExpired { .. }) => "chain_event_cursor_expired",
        Some(QueryError::TransparentHistoryCursorInvalid { .. }) => {
            "transparent_history_cursor_invalid"
        }
        Some(QueryError::InvalidAddress { .. }) => "invalid_address",
        Some(QueryError::ChainEpochPinUnsupported) => "chain_epoch_pin_unsupported",
        Some(QueryError::ChainEpochPinUnavailable { .. }) => "chain_epoch_pin_unavailable",
        Some(QueryError::UnsupportedChainEvent { .. }) => "unsupported_chain_event",
        Some(QueryError::UnsupportedBlockSelector { .. }) => "unsupported_block_selector",
        Some(QueryError::UnsupportedTransactionStatus { .. }) => "unsupported_transaction_status",
        Some(QueryError::TransactionBroadcastDisabled) => "transaction_broadcast_disabled",
        Some(QueryError::BroadcastTransactionTooLarge { .. }) => "broadcast_transaction_too_large",
        Some(QueryError::DeriveUnavailable { .. }) => "derive_unavailable",
        Some(QueryError::DeriveLag { .. }) => "derive_lag",
        Some(QueryError::BlockingTaskFailed { .. }) => "blocking_task_failed",
        Some(QueryError::ArtifactCorrupt { .. }) => "artifact_corrupt",
        Some(QueryError::BlockNotInBestChain) => "block_not_in_best_chain",
        Some(QueryError::Store(_)) => "store",
        Some(QueryError::DeriveStore(_)) => "derive_store",
        Some(QueryError::WalletProjectionRead { .. }) => "wallet_projection_read",
        Some(QueryError::Node(_)) => "node",
    }
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).map_or(u32::MAX, |converted| converted)
}

/// Records duration + outcome for a `WalletQuery` RPC that the gRPC adapter
/// proxies to the colocated ingest-control writer.
///
/// Shares the `zinder_query_request_*` metric names with
/// [`record_wallet_query_outcome`] so dashboards see one series per
/// operation regardless of which layer handled the request. The
/// `error_class` label carries the `tonic::Code` name rather than a typed
/// `QueryError` variant, because the gRPC adapter handles the response as
/// an opaque `Status`.
pub(crate) fn record_proxy_outcome<ResponseT>(
    operation: &'static str,
    started_at: Instant,
    proxy_outcome: &Result<tonic::Response<ResponseT>, tonic::Status>,
) {
    let outcome = match proxy_outcome {
        Ok(_) => zinder_runtime::RpcOutcome::Ok,
        Err(status) => zinder_runtime::RpcOutcome::Error {
            class: proxy_error_class(Some(status)),
        },
    };
    zinder_runtime::record_rpc_request(QUERY_RPC_METRICS, operation, started_at.elapsed(), outcome);
}

fn proxy_error_class(error: Option<&tonic::Status>) -> &'static str {
    let Some(status) = error else {
        return "none";
    };
    match status.code() {
        tonic::Code::Ok => "none",
        tonic::Code::Cancelled => "cancelled",
        tonic::Code::InvalidArgument => "invalid_argument",
        tonic::Code::DeadlineExceeded => "deadline_exceeded",
        tonic::Code::NotFound => "not_found",
        tonic::Code::AlreadyExists => "already_exists",
        tonic::Code::PermissionDenied => "permission_denied",
        tonic::Code::ResourceExhausted => "resource_exhausted",
        tonic::Code::FailedPrecondition => "failed_precondition",
        tonic::Code::Aborted => "aborted",
        tonic::Code::OutOfRange => "out_of_range",
        tonic::Code::Unimplemented => "unimplemented",
        tonic::Code::Internal => "internal",
        tonic::Code::Unavailable => "unavailable",
        tonic::Code::DataLoss => "data_loss",
        tonic::Code::Unauthenticated => "unauthenticated",
        tonic::Code::Unknown => "unknown",
    }
}

/// Latest visible block metadata bound to one chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatestBlock {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Latest visible block height.
    pub height: BlockHeight,
    /// Latest visible block hash.
    pub block_hash: zinder_core::BlockHash,
}

/// Safe-tip block metadata bound to one chain epoch. The block sits at
/// `chain_epoch.safe_tip_height` and is the highest height the wallet can
/// safely use as its scan ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatestSafeBlock {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Safe tip height (`chain_epoch.safe_tip_height`).
    pub height: BlockHeight,
    /// Block hash at `safe_tip_height`.
    pub block_hash: zinder_core::BlockHash,
}

/// Block-identity resolver response bound to one chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockIdResponseValue {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Resolved block identity in the canonical best chain.
    pub block_id: BlockId,
}

/// Block-header read response bound to one chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockHeaderResponseValue {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Block-header read-model value at the resolved selector.
    pub block_header: BlockHeaderInfo,
}

/// Single compact block response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactBlock {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Compact block artifact at the requested height.
    pub compact_block: CompactBlockArtifact,
}

/// Compact block range response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactBlockRange {
    /// Chain epoch used for every compact block in this range.
    pub chain_epoch: ChainEpoch,
    /// Inclusive height range requested.
    pub block_range: BlockHeightRange,
    /// Compact block artifacts in ascending height order.
    pub compact_blocks: Vec<CompactBlockArtifact>,
}

/// Full serialized block response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullBlock {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Raw block blob at the requested height.
    pub block_blob: BlockBlobArtifact,
}

/// Demand-driven full-block range stream bound to one pinned chain epoch.
///
/// [`chain_epoch`](Self::chain_epoch) is resolved once and describes every
/// delivered block. [`blocks`](Self::blocks) yields raw block blobs in
/// ascending, contiguous height order until the range is exhausted; a missing
/// blob or store failure arrives as a terminal `Err` after the blocks already
/// delivered, and dropping the receiver stops the backing producer.
#[derive(Debug)]
pub struct FullBlockStream {
    /// Chain epoch every delivered block is read under.
    pub chain_epoch: ChainEpoch,
    /// Ascending block blobs, then optionally one terminal error.
    pub blocks: mpsc::Receiver<Result<BlockBlobArtifact, QueryError>>,
}

/// Single mined-transaction response bound to one chain epoch. Used by
/// [`WalletQueryApi::transaction_at_block_index`], which only resolves
/// mined transactions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Transaction location at the requested block-local index.
    pub transaction: TransactionLocation,
}

/// Raw transaction blob response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTransaction {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Optional raw transaction blob.
    pub transaction: TransactionBlobArtifact,
}

/// Typed transaction-status response bound to one chain epoch.
///
/// Returned by [`WalletQueryApi::transaction`]. Carries the typed
/// [`TxStatus`] (`Mined`/`InMempool`/`ConflictingChain`/`NotFound`) and
/// the epoch used to answer the read; the wire-side adapter maps
/// `TxStatus::NotFound` to gRPC `NOT_FOUND`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionStatus {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Typed transaction status.
    pub status: TxStatus,
}

/// Transparent-address unspent output request.
///
/// Address inputs are typed: the wire boundary parses string addresses and
/// SHA-256-hashes them to `address_script_hash` before constructing this
/// request. The native API never carries a `String` form on the read path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUnspentOutputsRequest {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Wallet-birthday floor: minimum mined height to include.
    /// `BlockHeight::new(0)` means scan from genesis.
    pub start_height: BlockHeight,
}

/// Complete unspent transparent output set bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUnspentOutputs {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Unspent outputs in ascending `(block_height, outpoint)` order.
    pub outputs: Vec<TransparentUnspentOutput>,
}

/// Transparent-address tx-history range request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTxIdsInRangeRequest {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Inclusive minimum block height. Ignored when `from_cursor` is `Some`.
    pub start_height: BlockHeight,
    /// Inclusive maximum block height.
    pub end_height: BlockHeight,
    /// Server-bounded maximum entries per page.
    pub max_entries: NonZeroU32,
    /// Optional cursor returned by a previous response.
    pub from_cursor: Option<StreamCursorTokenV1>,
    /// Iterate newest-first when true.
    pub descending: bool,
}

/// Transparent-address tx-history page response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressTxIds {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Tx-history artifacts in the requested order.
    pub artifacts: Vec<TransparentAddressTxIndexArtifact>,
    /// Resume cursor when more entries may be available.
    pub next_cursor: Option<StreamCursorTokenV1>,
}

/// Tree-state response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeState {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Height the tree state belongs to.
    pub height: BlockHeight,
    /// Hash of the block this tree state belongs to.
    pub block_hash: zinder_core::BlockHash,
    /// Encoded tree-state payload bytes.
    pub payload_bytes: Vec<u8>,
}

/// Subtree-root response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtreeRoots {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Shielded protocol returned.
    pub protocol: ShieldedProtocol,
    /// First requested subtree-root index.
    pub start_index: SubtreeRootIndex,
    /// Subtree-root artifacts in ascending index order.
    pub subtree_roots: Vec<SubtreeRootArtifact>,
}

/// Bounded page of chain-event envelopes in stream order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainEvents {
    /// Chain-event envelopes returned for this page.
    pub event_envelopes: Vec<ChainEventEnvelope>,
}

/// Artifact lookup key used in unavailable-artifact errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArtifactKey {
    /// Artifact keyed by block height.
    BlockHeight(BlockHeight),
    /// Artifact keyed by transaction id.
    TransactionId(TransactionId),
    /// Subtree-root artifact keyed by shielded protocol and subtree index.
    SubtreeRootIndex {
        /// Shielded protocol requested.
        protocol: ShieldedProtocol,
        /// Requested subtree-root index.
        index: SubtreeRootIndex,
    },
    /// Transaction lookup keyed by a transaction index inside a block.
    BlockTransactionIndex {
        /// Requested block height.
        height: BlockHeight,
        /// Requested transaction index within the block.
        tx_index: u64,
    },
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlockHeight(height) => write!(formatter, "height {}", height.value()),
            Self::TransactionId(transaction_id) => {
                write!(formatter, "transaction id {transaction_id:?}")
            }
            Self::SubtreeRootIndex { protocol, index } => {
                write!(formatter, "{protocol:?} subtree index {}", index.value())
            }
            Self::BlockTransactionIndex { height, tx_index } => write!(
                formatter,
                "height {} transaction index {tx_index}",
                height.value()
            ),
        }
    }
}

/// Error returned by wallet query operations.
#[derive(Debug, Error)]
pub enum QueryError {
    /// Requested block range has an invalid shape.
    #[error("invalid block range: start {start_height:?} exceeds end {end_height:?}")]
    InvalidBlockRange {
        /// First requested height.
        start_height: BlockHeight,
        /// Last requested height.
        end_height: BlockHeight,
    },

    /// Requested block range exceeds the configured response bound.
    #[error("compact block range is too large: requested {requested}, maximum {maximum}")]
    CompactBlockRangeTooLarge {
        /// Requested compact-block count.
        requested: usize,
        /// Maximum allowed compact-block count.
        maximum: usize,
    },

    /// Balance request named an empty address list or more addresses than the
    /// per-request cap.
    #[error(
        "transparent balance address count is out of range: requested {requested}, maximum {maximum}"
    )]
    TransparentBalanceAddressCountExceeded {
        /// Requested address count.
        requested: usize,
        /// Maximum allowed address count.
        maximum: u32,
    },

    /// Indexed artifact is unavailable for the requested key.
    #[error("{family:?} artifact is unavailable for {key}")]
    ArtifactUnavailable {
        /// Artifact family requested.
        family: ArtifactFamily,
        /// Requested lookup key.
        key: ArtifactKey,
    },

    /// Indexed compact block payload could not be decoded.
    #[error("compact block payload at height {height:?} is malformed: {reason}")]
    CompactBlockPayloadMalformed {
        /// Requested block height.
        height: BlockHeight,
        /// Decoder failure reason.
        reason: String,
    },

    /// Stored artifact bytes failed Zcash protocol decoding at request time.
    #[error("{family:?} artifact is corrupt: {reason}")]
    ArtifactCorrupt {
        /// Artifact family that failed to decode.
        family: ArtifactFamily,
        /// Decoder failure reason.
        reason: String,
    },

    /// Block selector resolved to no visible block in the canonical best chain.
    #[error("block selector resolved to no visible block in the canonical best chain")]
    BlockNotInBestChain,

    /// Shielded protocol cannot be represented on the native wallet protocol.
    #[error("{protocol:?} is not supported by the native wallet protocol")]
    UnsupportedShieldedProtocol {
        /// Shielded protocol that cannot be encoded.
        protocol: ShieldedProtocol,
    },

    /// Transparent address tx-history cursor failed validation.
    #[error("transparent history cursor is invalid: {reason}")]
    TransparentHistoryCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Transparent address selector failed validation.
    #[error("invalid transparent address: {reason}")]
    InvalidAddress {
        /// Validation failure reason.
        reason: &'static str,
    },

    /// Chain-event cursor failed validation.
    #[error("chain-event cursor is invalid: {reason}")]
    ChainEventCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
    },

    /// Chain-event cursor points before retained event history.
    #[error(
        "chain-event cursor expired: event sequence {event_sequence}, oldest retained {oldest_retained_sequence}"
    )]
    ChainEventCursorExpired {
        /// Cursor event sequence.
        event_sequence: u64,
        /// Oldest retained event sequence.
        oldest_retained_sequence: u64,
    },

    /// The query implementation does not support request-side epoch pinning.
    #[error("chain-epoch pinning is unsupported by this query implementation")]
    ChainEpochPinUnsupported,

    /// Requested chain epoch is no longer available.
    #[error("chain-epoch pin is unavailable: {chain_epoch_id:?}")]
    ChainEpochPinUnavailable {
        /// Requested chain epoch id.
        chain_epoch_id: ChainEpochId,
    },

    /// Store returned a chain-event variant unsupported by the native protocol.
    #[error("unsupported chain event: {event}")]
    UnsupportedChainEvent {
        /// Unsupported event description.
        event: &'static str,
    },

    /// Block selector variant cannot be resolved by this query implementation.
    #[error("unsupported block selector: {reason}")]
    UnsupportedBlockSelector {
        /// Stable diagnostic reason.
        reason: &'static str,
    },

    /// Typed transaction status cannot be encoded on the native wire protocol.
    #[error("unsupported transaction status: {reason}")]
    UnsupportedTransactionStatus {
        /// Stable diagnostic reason.
        reason: &'static str,
    },

    /// Transaction broadcast is disabled for this query handle.
    #[error("transaction broadcast is disabled")]
    TransactionBroadcastDisabled,

    /// Raw transaction exceeds the maximum serialized size accepted for broadcast.
    #[error("raw transaction is too large: {actual} bytes exceeds maximum {maximum}")]
    BroadcastTransactionTooLarge {
        /// Serialized length of the submitted transaction.
        actual: usize,
        /// Maximum accepted serialized length.
        maximum: usize,
    },

    /// Derive-owned wallet projection is not configured for this query handle.
    #[error("derive projection is unavailable for {capability}")]
    DeriveUnavailable {
        /// Capability that requires the derive projection.
        capability: &'static str,
    },

    /// Derive-owned wallet projection has not caught up to the requested epoch.
    #[error(
        "derive projection {capability} is behind chain tip {chain_tip_height:?}: derive height {derive_height:?}"
    )]
    DeriveLag {
        /// Capability that requires the derive projection.
        capability: &'static str,
        /// Canonical chain tip height required by the request.
        chain_tip_height: BlockHeight,
        /// Latest materialized derive height, when any block has been processed.
        derive_height: Option<BlockHeight>,
    },

    /// A blocking read task failed unexpectedly (panic or runtime shutdown).
    #[error("query read task failed: {reason}")]
    BlockingTaskFailed {
        /// Operator-facing failure reason.
        reason: String,
    },

    /// Canonical read API returned a storage error.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Derive store returned a storage error.
    #[error(transparent)]
    DeriveStore(#[from] zinder_derive::DeriveStoreError),

    /// Typed wallet-projection backend returned a storage failure.
    #[error("wallet projection read failed: {source}")]
    WalletProjectionRead {
        /// Backend-specific source retained without leaking its API contract.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Upstream node operation failed.
    #[error(transparent)]
    Node(#[from] SourceError),
}

impl zinder_proto::BoundaryError for QueryError {
    /// Maps each [`QueryError`] variant to its stable
    /// [`ErrorReason`](zinder_proto::v1::ops::ErrorReason).
    ///
    /// `Store` failures route through `StoreError::error_reason` at the gRPC
    /// adapter and never reach this match in practice; the arm keeps the match
    /// total and fails closed as storage-unavailable.
    fn error_reason(&self) -> zinder_proto::v1::ops::ErrorReason {
        use zinder_proto::v1::ops::ErrorReason;
        match self {
            Self::InvalidBlockRange { .. } => ErrorReason::InvalidBlockRange,
            Self::CompactBlockRangeTooLarge { .. } => ErrorReason::CompactBlockRangeTooLarge,
            Self::TransparentBalanceAddressCountExceeded { .. } => {
                ErrorReason::TransparentBalanceAddressCountExceeded
            }
            Self::ChainEventCursorInvalid { .. } => ErrorReason::ChainEventCursorInvalid,
            Self::TransparentHistoryCursorInvalid { .. } => {
                ErrorReason::TransparentHistoryCursorInvalid
            }
            Self::InvalidAddress { .. } => ErrorReason::InvalidAddress,
            Self::UnsupportedShieldedProtocol { .. } => ErrorReason::UnsupportedShieldedProtocol,
            Self::TransactionBroadcastDisabled => ErrorReason::BroadcastDisabled,
            Self::BroadcastTransactionTooLarge { .. } => ErrorReason::BroadcastTransactionTooLarge,
            Self::DeriveUnavailable { .. } => ErrorReason::DeriveProjectionUnavailable,
            Self::DeriveLag { .. } => ErrorReason::DeriveProjectionLagging,
            Self::ChainEventCursorExpired { .. } => ErrorReason::ChainEventCursorExpired,
            Self::ChainEpochPinUnsupported => ErrorReason::ChainEpochPinUnsupported,
            Self::ChainEpochPinUnavailable { .. } => ErrorReason::ChainEpochPinUnavailable,
            Self::ArtifactUnavailable { .. } => ErrorReason::ArtifactUnavailable,
            Self::CompactBlockPayloadMalformed { .. } => ErrorReason::CompactBlockPayloadMalformed,
            Self::ArtifactCorrupt { .. } => ErrorReason::ArtifactCorrupt,
            Self::BlockNotInBestChain => ErrorReason::BlockNotInBestChain,
            Self::UnsupportedChainEvent { .. } => ErrorReason::UnsupportedChainEvent,
            Self::UnsupportedBlockSelector { .. } => ErrorReason::UnsupportedBlockSelector,
            Self::UnsupportedTransactionStatus { .. } => ErrorReason::UnsupportedTransactionStatus,
            Self::BlockingTaskFailed { .. } => ErrorReason::BlockingTaskFailed,
            Self::Node(source_error) if source_error.is_node_capability_missing() => {
                ErrorReason::NodeCapabilityMissing
            }
            Self::Node(_) => ErrorReason::NodeUnavailable,
            Self::DeriveStore(_) | Self::Store(_) | Self::WalletProjectionRead { .. } => {
                ErrorReason::StorageUnavailable
            }
        }
    }
}

fn block_height_artifact_unavailable(family: ArtifactFamily, height: BlockHeight) -> QueryError {
    artifact_unavailable(family, ArtifactKey::BlockHeight(height))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "BlockSelector is #[non_exhaustive]; new selector variants are a deliberate decision per the gap doc, not a default fall-through"
)]
fn resolve_block_selector(
    reader: &zinder_store::ChainEpochReader<'_>,
    selector: BlockSelector,
) -> Result<BlockId, QueryError> {
    match selector {
        BlockSelector::Height(height) => {
            let chain_epoch = reader.chain_epoch();
            if height > chain_epoch.visible_tip_height {
                return Err(QueryError::BlockNotInBestChain);
            }
            let block = reader
                .block_header_at(height)?
                .ok_or(QueryError::BlockNotInBestChain)?;
            Ok(BlockId::new(height, block.block_hash))
        }
        BlockSelector::Hash(hash) => match reader.block_hash_lookup(hash)? {
            BlockHashLookup::Resolved(block_id) => Ok(block_id),
            BlockHashLookup::NotInBestChain | BlockHashLookup::NotIndexed => {
                Err(QueryError::BlockNotInBestChain)
            }
        },
        _ => Err(QueryError::UnsupportedBlockSelector {
            reason: "selector variant has no canonical resolver",
        }),
    }
}

fn subtree_root_artifact_unavailable(
    protocol: ShieldedProtocol,
    index: SubtreeRootIndex,
) -> QueryError {
    artifact_unavailable(
        ArtifactFamily::SubtreeRoot,
        ArtifactKey::SubtreeRootIndex { protocol, index },
    )
}

fn artifact_unavailable(family: ArtifactFamily, key: ArtifactKey) -> QueryError {
    QueryError::ArtifactUnavailable { family, key }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only intentional history absence and a matching missing artifact change category; every other current and future store failure remains a storage error."
)]
fn map_height_artifact_store_error(
    error: StoreError,
    family: ArtifactFamily,
    requested_height: BlockHeight,
) -> QueryError {
    match error {
        StoreError::CanonicalHistoryUnavailable { .. } => {
            block_height_artifact_unavailable(family, requested_height)
        }
        StoreError::ArtifactMissing {
            family: missing_family,
            ..
        } if missing_family == family => {
            block_height_artifact_unavailable(family, requested_height)
        }
        _ => QueryError::Store(error),
    }
}

fn validate_block_range(
    block_range: BlockHeightRange,
    max_blocks: NonZeroU32,
) -> Result<(), QueryError> {
    if block_range.start > block_range.end {
        return Err(QueryError::InvalidBlockRange {
            start_height: block_range.start,
            end_height: block_range.end,
        });
    }

    let requested = block_range.into_iter().len();
    let maximum = u32_to_usize(max_blocks.get());
    if requested > maximum {
        return Err(QueryError::CompactBlockRangeTooLarge { requested, maximum });
    }

    Ok(())
}

fn completed_subtree_count(chain_epoch: ChainEpoch, protocol: ShieldedProtocol) -> u32 {
    chain_epoch.tip_metadata.completed_subtree_count(protocol)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}

#[cfg(test)]
mod error_reason_tests {
    use zinder_proto::{BoundaryError, v1::ops::ErrorReason};

    use super::*;

    /// One representative of every [`QueryError`] variant.
    ///
    /// The list is exhaustive, so a new variant fails to compile until it is
    /// listed; [`no_query_error_variant_maps_to_unspecified`] then asserts the
    /// new variant carries a real reason.
    #[allow(
        clippy::too_many_lines,
        reason = "One literal per QueryError variant; the length tracks the enum, not branching complexity."
    )]
    fn one_of_each_variant() -> Vec<QueryError> {
        vec![
            QueryError::InvalidBlockRange {
                start_height: BlockHeight::new(2),
                end_height: BlockHeight::new(1),
            },
            QueryError::CompactBlockRangeTooLarge {
                requested: 2,
                maximum: 1,
            },
            QueryError::TransparentBalanceAddressCountExceeded {
                requested: 257,
                maximum: MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES,
            },
            QueryError::ArtifactUnavailable {
                family: ArtifactFamily::CompactBlock,
                key: ArtifactKey::BlockHeight(BlockHeight::new(1)),
            },
            QueryError::CompactBlockPayloadMalformed {
                height: BlockHeight::new(1),
                reason: "probe".to_owned(),
            },
            QueryError::ArtifactCorrupt {
                family: ArtifactFamily::CompactBlock,
                reason: "probe".to_owned(),
            },
            QueryError::BlockNotInBestChain,
            QueryError::UnsupportedShieldedProtocol {
                protocol: ShieldedProtocol::Sapling,
            },
            QueryError::TransparentHistoryCursorInvalid { reason: "probe" },
            QueryError::InvalidAddress { reason: "probe" },
            QueryError::ChainEventCursorInvalid { reason: "probe" },
            QueryError::ChainEventCursorExpired {
                event_sequence: 1,
                oldest_retained_sequence: 2,
            },
            QueryError::ChainEpochPinUnsupported,
            QueryError::ChainEpochPinUnavailable {
                chain_epoch_id: ChainEpochId::new(1),
            },
            QueryError::UnsupportedChainEvent { event: "probe" },
            QueryError::UnsupportedBlockSelector { reason: "probe" },
            QueryError::UnsupportedTransactionStatus { reason: "probe" },
            QueryError::TransactionBroadcastDisabled,
            QueryError::BroadcastTransactionTooLarge {
                actual: MAX_RAW_TRANSACTION_BYTES + 1,
                maximum: MAX_RAW_TRANSACTION_BYTES,
            },
            QueryError::DeriveUnavailable {
                capability: "probe",
            },
            QueryError::DeriveLag {
                capability: "probe",
                chain_tip_height: BlockHeight::new(2),
                derive_height: Some(BlockHeight::new(1)),
            },
            QueryError::BlockingTaskFailed {
                reason: "probe".to_owned(),
            },
            QueryError::Store(StoreError::NoVisibleChainEpoch),
            QueryError::DeriveStore(zinder_derive::DeriveStoreError::InvalidOptions {
                reason: "probe",
            }),
            QueryError::WalletProjectionRead {
                source: Box::new(std::io::Error::other("probe")),
            },
            QueryError::Node(SourceError::InvalidBlockHashHex {
                reason: "probe".to_owned(),
            }),
        ]
    }

    #[test]
    fn no_query_error_variant_maps_to_unspecified() {
        for error in one_of_each_variant() {
            assert_ne!(
                error.error_reason(),
                ErrorReason::Unspecified,
                "QueryError variant {error:?} mapped to ERROR_REASON_UNSPECIFIED"
            );
        }
    }

    #[test]
    fn typed_projection_errors_preserve_public_wallet_error_vocabulary() {
        assert!(matches!(
            map_wallet_projection_read_error(
                WalletProjectionReadError::ProjectionUnavailable {
                    projection: "probe",
                },
                "wallet.probe.v1",
            ),
            QueryError::DeriveUnavailable {
                capability: "wallet.probe.v1",
            }
        ));
        assert!(matches!(
            map_wallet_projection_read_error(
                WalletProjectionReadError::TransparentAddressHistoryCursorInvalid,
                WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
            ),
            QueryError::TransparentHistoryCursorInvalid { .. }
        ));
    }
}
