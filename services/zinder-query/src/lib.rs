//! Wallet and application query boundary for Zinder.
//!
//! This crate serves indexed artifacts through [`ChainEpochReadApi`] without
//! calling upstream node sources or mutating canonical storage.

use std::{
    collections::{HashMap, hash_map::Entry as HashMapEntry},
    num::NonZeroU32,
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use thiserror::Error;
use zinder_core::{
    BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockId, BlockSelector, ChainEpoch,
    ChainEpochId, CompactBlockArtifact, MinedDetails, MinedTransaction, NetworkUpgradeActivations,
    RawTransactionBytes, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange,
    TransactionArtifact, TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentAddressUtxoArtifact, TransparentOutPoint,
    TransparentPrevoutEntry, TransparentPrevoutsResponse, TxStatus,
};
use zinder_source::{
    SourceError, TransactionBroadcaster, block_header_info_from_raw_block_bytes,
    transparent_prevout_from_raw_transaction_bytes,
};
use zinder_store::{
    ArtifactFamily, BlockHashLookup, ChainEpochReadApi, ChainEventEnvelope,
    ChainEventHistoryRequest, ChainEventStreamFamily, DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS,
    StoreError, StreamCursorTokenV1, TransparentAddressTxIndexPageRequest,
    TransparentAddressUtxosPageRequest,
};

mod derive_proxy;
mod grpc;
mod readiness_refresh;

pub use derive_proxy::{
    DEFAULT_DERIVE_PROBE_INTERVAL, DeriveProxy, DeriveProxyConfig, DeriveReadinessGauge,
    DeriveReadinessProbeConfig, MIN_DERIVE_PROBE_INTERVAL, spawn_derive_readiness_probe,
};

pub use grpc::{
    MAX_TRANSPARENT_ADDRESSES_PER_BALANCE_REQUEST, ServerInfoSettings, UpstreamNodeCapabilities,
    WalletQueryGrpcAdapter, address_lookup_to_script_hash, block_header_by_selector_response,
    block_id_by_selector_response, broadcast_transaction_response,
    build_transparent_address_tx_ids_chunk, build_transparent_address_utxos_stream_chunk,
    build_wallet_server_info, chain_events_response, compact_block_response, latest_block_response,
    latest_tree_state_response, status_from_query_error, subtree_roots_response,
    transaction_response, transparent_address_confirmed_balance_response,
    transparent_address_tx_ids_response, transparent_address_utxos_response,
    transparent_prevouts_response, tree_state_response,
};
pub use readiness_refresh::{
    DEFAULT_READINESS_REFRESH_INTERVAL, SecondaryCatchupOptions, WriterStatusConfig,
    spawn_readiness_refresh, spawn_secondary_catchup,
};

/// Wallet-facing read API backed by epoch-bound canonical reads.
///
/// Every read that depends on chain state takes `at_epoch: Option<ChainEpoch>`.
/// `None` resolves to the visible chain epoch at call time; `Some(epoch)` pins
/// the read to that epoch. Implementations that cannot honor a pinned epoch
/// return [`QueryError::ChainEpochPinUnsupported`].
#[async_trait]
pub trait WalletQueryApi: Send + Sync + 'static {
    /// Reads latest visible block metadata.
    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<LatestBlock, QueryError>;

    /// Resolves a typed block selector against the canonical best chain.
    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockIdResponseValue, QueryError>;

    /// Reads the typed block-header read model at a typed block selector.
    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderResponseValue, QueryError>;

    /// Reads one compact block artifact at a given height.
    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlock, QueryError>;

    /// Reads compact block artifacts for an inclusive height range.
    async fn compact_block_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockRange, QueryError>;

    /// Reads typed transaction status by transaction id.
    ///
    /// Returns [`TxStatus::Mined`] for mined transactions, with epoch-bound
    /// [`MinedDetails`] enrichment, [`TxStatus::NotFound`] when the
    /// transaction is not visible in the canonical chain, and
    /// [`QueryError`] for storage/upstream failures. An `at_epoch` pin is
    /// a canonical-chain read and never consults live mempool state;
    /// mempool fall-through is the caller's responsibility on this trait.
    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
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
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Transaction, QueryError>;

    /// Resolves a batch of canonical-chain transparent outpoints to their
    /// referenced outputs.
    ///
    /// Reads each unique `transaction_id` once from the canonical
    /// transaction artifact and indexes into the transaction's `vout` list
    /// (compute-at-read-time, no dedicated column family). Outpoints that
    /// do not resolve at the response's [`ChainEpoch`] return an entry with
    /// `prevout = None`. The response preserves input order; duplicate
    /// outpoints emit duplicate entries.
    ///
    async fn transparent_prevouts(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentPrevoutsResponse, QueryError>;

    /// Reads unspent transparent outputs for one transparent address script.
    async fn transparent_address_utxos(
        &self,
        request: TransparentAddressUtxosRequest,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxos, QueryError>;

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts.
    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressTxIds, QueryError>;

    /// Reads the tree-state artifact at a block height.
    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeState, QueryError>;

    /// Reads the latest tree-state artifact at the visible chain epoch tip.
    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeState, QueryError>;

    /// Reads subtree-root artifacts for a bounded subtree range.
    async fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<SubtreeRoots, QueryError>;

    /// Reads a bounded page of replayable chain events.
    async fn chain_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEvents, QueryError>;

    /// Broadcasts a raw transaction without mutating canonical storage.
    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, QueryError>;
}

/// Query boundary backed by a [`ChainEpochReadApi`] implementation.
///
/// Pass `()` as the broadcaster to disable transaction broadcast.
#[derive(Clone, Debug)]
pub struct WalletQuery<ReadApi, Broadcaster = ()> {
    read_api: ReadApi,
    transaction_broadcaster: Broadcaster,
    options: WalletQueryOptions,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

/// Default maximum compact-block count returned by one range call.
pub const DEFAULT_MAX_COMPACT_BLOCK_RANGE: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

/// Runtime options for [`WalletQuery`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletQueryOptions {
    /// Maximum compact-block count returned by one range call.
    pub max_compact_block_range: NonZeroU32,
}

impl Default for WalletQueryOptions {
    fn default() -> Self {
        Self {
            max_compact_block_range: DEFAULT_MAX_COMPACT_BLOCK_RANGE,
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
            transaction_broadcaster,
            options,
            network_upgrade_activations,
        }
    }
}

#[async_trait]
impl<ReadApi, Broadcaster> WalletQueryApi for WalletQuery<ReadApi, Broadcaster>
where
    ReadApi: ChainEpochReadApi + Clone + Send + Sync + 'static,
    Broadcaster: TransactionBroadcaster + Clone,
{
    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<LatestBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            Ok(LatestBlock {
                chain_epoch,
                height: chain_epoch.tip_height,
                block_hash: chain_epoch.tip_hash,
            })
        }))
        .await;
        record_wallet_query_outcome("latest_block", started_at, &query_outcome, None);
        query_outcome
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockIdResponseValue, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
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
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderResponseValue, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let block_id = resolve_block_selector(&reader, selector)?;
            let block = reader.block_at(block_id.height)?.ok_or_else(|| {
                block_height_artifact_unavailable(ArtifactFamily::FinalizedBlock, block_id.height)
            })?;
            let block_header =
                block_header_info_from_raw_block_bytes(block_id.height, &block.payload_bytes)
                    .map_err(|error| QueryError::ArtifactCorrupt {
                        family: ArtifactFamily::FinalizedBlock,
                        reason: error.to_string(),
                    })?;
            Ok(BlockHeaderResponseValue {
                chain_epoch,
                block_header,
            })
        }))
        .await;
        record_wallet_query_outcome("block_header_by_selector", started_at, &query_outcome, None);
        query_outcome
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlock, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let compact_block = match reader.compact_block_at(height) {
                Ok(Some(compact_block)) => compact_block,
                Ok(None)
                | Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::CompactBlock,
                    ..
                }) => {
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
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransactionStatus, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let activations = self.network_upgrade_activations.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let Some(artifact) = reader.transaction_by_id(transaction_id)? else {
                return Ok(TransactionStatus {
                    chain_epoch,
                    status: TxStatus::NotFound,
                });
            };
            let block_time = reader
                .block_at(artifact.block_height)?
                .and_then(|block| {
                    block_header_info_from_raw_block_bytes(
                        artifact.block_height,
                        &block.payload_bytes,
                    )
                    .ok()
                    .map(|header| header.block_time)
                })
                .unwrap_or_default();
            let consensus_branch_id = activations.consensus_branch_id_at(artifact.block_height);
            let details = MinedDetails::from_response_epoch(
                &chain_epoch,
                artifact.block_height,
                consensus_branch_id,
                block_time,
            );
            Ok(TransactionStatus {
                chain_epoch,
                status: TxStatus::Mined(MinedTransaction::new(artifact, details)),
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
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Transaction, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let compact_block_artifact = match reader.compact_block_at(height) {
                Ok(Some(compact_block)) => compact_block,
                Ok(None)
                | Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::CompactBlock,
                    ..
                }) => {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::CompactBlock,
                        height,
                    ));
                }
                Err(error) => return Err(QueryError::Store(error)),
            };
            let transaction_id = transaction_id_at_block_index(
                compact_block_artifact.payload_bytes.as_slice(),
                tx_index,
                height,
            )?;
            let transaction = reader.transaction_by_id(transaction_id)?.ok_or_else(|| {
                artifact_unavailable(
                    ArtifactFamily::Transaction,
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

    async fn transparent_prevouts(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentPrevoutsResponse, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let mut entries = Vec::with_capacity(outpoints.len());
            let mut payload_cache: HashMap<TransactionId, Option<Vec<u8>>> = HashMap::new();

            for outpoint in outpoints {
                let cached_payload = match payload_cache.entry(outpoint.transaction_id) {
                    HashMapEntry::Occupied(entry) => entry.into_mut(),
                    HashMapEntry::Vacant(entry) => {
                        let payload = reader
                            .transaction_by_id(outpoint.transaction_id)?
                            .map(|artifact| artifact.payload_bytes);
                        entry.insert(payload)
                    }
                };
                let prevout = match cached_payload {
                    None => None,
                    Some(payload_bytes) => transparent_prevout_from_raw_transaction_bytes(
                        payload_bytes,
                        outpoint.output_index,
                    )
                    .map_err(|error| QueryError::ArtifactCorrupt {
                        family: ArtifactFamily::Transaction,
                        reason: error.to_string(),
                    })?,
                };
                entries.push(TransparentPrevoutEntry { outpoint, prevout });
            }

            Ok(TransparentPrevoutsResponse {
                chain_epoch,
                entries,
            })
        }))
        .await;
        record_wallet_query_outcome("transparent_prevouts", started_at, &query_outcome, None);
        query_outcome
    }

    async fn compact_block_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockRange, QueryError> {
        let started_at = Instant::now();
        let requested_blocks = block_range.into_iter().len();
        if let Err(error) = validate_block_range(block_range, self.options) {
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
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();
            let compact_blocks = reader.compact_blocks_in_range(block_range)?;
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

    async fn transparent_address_utxos(
        &self,
        request: TransparentAddressUtxosRequest,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxos, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let page = read_api
                .transparent_address_utxos_page(TransparentAddressUtxosPageRequest {
                    at_epoch,
                    address_script_hash: request.address_script_hash,
                    start_height: request.start_height,
                    max_entries: request.max_entries,
                    from_cursor: request.from_cursor.as_ref(),
                })
                .map_err(map_transparent_utxo_store_error)?;

            Ok(TransparentAddressUtxos {
                chain_epoch: page.chain_epoch,
                utxos: page.utxos,
                next_cursor: page.next_cursor,
            })
        }))
        .await;
        record_wallet_query_outcome(
            "transparent_address_utxos",
            started_at,
            &query_outcome,
            None,
        );
        query_outcome
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
        at_epoch: Option<ChainEpoch>,
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

        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let page = read_api
                .transparent_address_tx_index_page(TransparentAddressTxIndexPageRequest {
                    at_epoch,
                    address_script_hash: request.address_script_hash,
                    start_height: request.start_height,
                    end_height: request.end_height,
                    max_entries: request.max_entries,
                    descending: request.descending,
                    from_cursor: request.from_cursor.as_ref(),
                })
                .map_err(map_transparent_utxo_store_error)?;

            Ok(TransparentAddressTxIds {
                chain_epoch: page.chain_epoch,
                artifacts: page.artifacts,
                next_cursor: page.next_cursor,
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

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeState, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();

            let tree_state = match reader.tree_state_at(height) {
                Ok(Some(tree_state)) => tree_state,
                Ok(None)
                | Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::TreeState,
                    ..
                }) => {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::TreeState,
                        height,
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
        record_wallet_query_outcome("tree_state_at", started_at, &query_outcome, None);
        query_outcome
    }

    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeState, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
            let chain_epoch = reader.chain_epoch();

            let tree_state = match reader.latest_tree_state() {
                Ok(Some(tree_state)) => tree_state,
                Ok(None)
                | Err(StoreError::ArtifactMissing {
                    family: ArtifactFamily::TreeState,
                    ..
                }) => {
                    return Err(block_height_artifact_unavailable(
                        ArtifactFamily::TreeState,
                        chain_epoch.tip_height,
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
        record_wallet_query_outcome("latest_tree_state", started_at, &query_outcome, None);
        query_outcome
    }

    async fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<SubtreeRoots, QueryError> {
        let started_at = Instant::now();
        let read_api = self.read_api.clone();
        let query_outcome = join_blocking(tokio::task::spawn_blocking(move || {
            let reader = open_chain_epoch_reader(&read_api, at_epoch)?;
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

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, QueryError> {
        let started_at = Instant::now();
        let broadcast_outcome = self
            .transaction_broadcaster
            .broadcast_transaction(raw_transaction)
            .await
            .map_err(map_broadcast_source_error);
        record_wallet_query_outcome(
            "broadcast_transaction",
            started_at,
            &broadcast_outcome,
            None,
        );
        broadcast_outcome
    }
}

fn open_chain_epoch_reader<ReadApi>(
    read_api: &ReadApi,
    at_epoch: Option<ChainEpoch>,
) -> Result<zinder_store::ChainEpochReader<'_>, QueryError>
where
    ReadApi: ChainEpochReadApi,
{
    let Some(requested_epoch) = at_epoch else {
        return read_api
            .current_chain_epoch_reader()
            .map_err(QueryError::Store);
    };

    let reader = read_api
        .chain_epoch_reader_at(requested_epoch.id)
        .map_err(|error| map_epoch_pin_store_error(error, requested_epoch.id))?;
    let stored_epoch = reader.chain_epoch();
    if stored_epoch != requested_epoch {
        return Err(QueryError::ChainEpochPinMismatch {
            chain_epoch_id: requested_epoch.id,
            reason: "stored chain epoch does not match at_epoch",
        });
    }

    Ok(reader)
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

fn map_broadcast_source_error(error: SourceError) -> QueryError {
    if matches!(error, SourceError::TransactionBroadcastDisabled) {
        QueryError::TransactionBroadcastDisabled
    } else {
        QueryError::Node(error)
    }
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

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only the typed transparent-address cursor errors become query cursor errors; every other storage failure is preserved."
)]
fn map_transparent_utxo_store_error(error: StoreError) -> QueryError {
    match error {
        StoreError::TransparentUtxoCursorInvalid { reason } => {
            QueryError::TransparentUtxoCursorInvalid { reason }
        }
        StoreError::TransparentHistoryCursorInvalid { reason } => {
            QueryError::TransparentHistoryCursorInvalid { reason }
        }
        _ => QueryError::Store(error),
    }
}

fn record_wallet_query_outcome<Response>(
    operation: &'static str,
    started_at: Instant,
    query_outcome: &Result<Response, QueryError>,
    block_count: Option<usize>,
) {
    metrics::histogram!(
        "zinder_query_request_duration_seconds",
        "operation" => operation,
        "status" => outcome_status(query_outcome),
        "error_class" => query_error_class(query_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_query_request_total",
        "operation" => operation,
        "status" => outcome_status(query_outcome),
        "error_class" => query_error_class(query_outcome.as_ref().err())
    )
    .increment(1);

    if let Some(block_count) = block_count {
        metrics::histogram!(
            "zinder_query_compact_block_range_block_count",
            "status" => outcome_status(query_outcome)
        )
        .record(usize_to_u32_saturating(block_count));
    }
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn query_error_class(error: Option<&QueryError>) -> &'static str {
    match error {
        None => "none",
        Some(QueryError::InvalidBlockRange { .. }) => "invalid_block_range",
        Some(QueryError::CompactBlockRangeTooLarge { .. }) => "compact_block_range_too_large",
        Some(QueryError::ArtifactUnavailable { .. }) => "artifact_unavailable",
        Some(QueryError::CompactBlockPayloadMalformed { .. }) => "compact_block_payload_malformed",
        Some(QueryError::UnsupportedShieldedProtocol { .. }) => "unsupported_shielded_protocol",
        Some(QueryError::ChainEventCursorInvalid { .. }) => "chain_event_cursor_invalid",
        Some(QueryError::ChainEventCursorExpired { .. }) => "chain_event_cursor_expired",
        Some(QueryError::TransparentUtxoCursorInvalid { .. }) => "transparent_utxo_cursor_invalid",
        Some(QueryError::TransparentHistoryCursorInvalid { .. }) => {
            "transparent_history_cursor_invalid"
        }
        Some(QueryError::InvalidAddress { .. }) => "invalid_address",
        Some(QueryError::ChainEpochPinUnsupported) => "chain_epoch_pin_unsupported",
        Some(QueryError::ChainEpochPinUnavailable { .. }) => "chain_epoch_pin_unavailable",
        Some(QueryError::ChainEpochPinMismatch { .. }) => "chain_epoch_pin_mismatch",
        Some(QueryError::UnsupportedChainEvent { .. }) => "unsupported_chain_event",
        Some(QueryError::UnsupportedBlockSelector { .. }) => "unsupported_block_selector",
        Some(QueryError::UnsupportedTransactionStatus { .. }) => "unsupported_transaction_status",
        Some(QueryError::TransactionBroadcastDisabled) => "transaction_broadcast_disabled",
        Some(QueryError::BlockingTaskFailed { .. }) => "blocking_task_failed",
        Some(QueryError::ArtifactCorrupt { .. }) => "artifact_corrupt",
        Some(QueryError::BlockNotInBestChain) => "block_not_in_best_chain",
        Some(QueryError::Store(_)) => "store",
        Some(QueryError::Node(_)) => "node",
    }
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).map_or(u32::MAX, |converted| converted)
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

/// Single mined-transaction response bound to one chain epoch. Used by
/// [`WalletQueryApi::transaction_at_block_index`], which only resolves
/// mined transactions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Transaction artifact at the requested transaction id.
    pub transaction: TransactionArtifact,
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

/// Transparent address UTXO request.
///
/// Address inputs are typed: the wire boundary parses string addresses and
/// SHA-256-hashes them to `address_script_hash` before constructing this
/// request. The native API never carries a `String` form on the read path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxosRequest {
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Wallet-birthday optimization: minimum mined height to include.
    /// `BlockHeight::new(0)` means scan from genesis. Ignored when
    /// `from_cursor` is `Some`.
    pub start_height: BlockHeight,
    /// Server-bounded maximum entries per page.
    pub max_entries: NonZeroU32,
    /// Optional cursor returned by a previous unary or streaming response.
    /// When supplied, the scan resumes strictly after that position and
    /// `start_height` is ignored.
    pub from_cursor: Option<StreamCursorTokenV1>,
}

/// Transparent address UTXO response bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxos {
    /// Chain epoch used to answer the query.
    pub chain_epoch: ChainEpoch,
    /// Unspent outputs in ascending `(block_height, outpoint)` order.
    pub utxos: Vec<TransparentAddressUtxoArtifact>,
    /// Resume cursor when more UTXOs may be available. `None` when the
    /// scan was fully drained.
    pub next_cursor: Option<StreamCursorTokenV1>,
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

    /// Transparent-UTXO cursor failed validation.
    #[error("transparent-UTXO cursor is invalid: {reason}")]
    TransparentUtxoCursorInvalid {
        /// Cursor validation failure reason.
        reason: &'static str,
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

    /// Requested chain epoch id exists but the supplied epoch body does not match.
    #[error("chain-epoch pin mismatch for {chain_epoch_id:?}: {reason}")]
    ChainEpochPinMismatch {
        /// Requested chain epoch id.
        chain_epoch_id: ChainEpochId,
        /// Stable diagnostic reason.
        reason: &'static str,
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

    /// A blocking read task failed unexpectedly (panic or runtime shutdown).
    #[error("query read task failed: {reason}")]
    BlockingTaskFailed {
        /// Operator-facing failure reason.
        reason: String,
    },

    /// Canonical read API returned a storage error.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Upstream node operation failed.
    #[error(transparent)]
    Node(#[from] SourceError),
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
            if height > chain_epoch.tip_height {
                return Err(QueryError::BlockNotInBestChain);
            }
            let block = reader
                .block_at(height)?
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

fn validate_block_range(
    block_range: BlockHeightRange,
    options: WalletQueryOptions,
) -> Result<(), QueryError> {
    if block_range.start > block_range.end {
        return Err(QueryError::InvalidBlockRange {
            start_height: block_range.start,
            end_height: block_range.end,
        });
    }

    let requested = block_range.into_iter().len();
    let maximum = u32_to_usize(options.max_compact_block_range.get());
    if requested > maximum {
        return Err(QueryError::CompactBlockRangeTooLarge { requested, maximum });
    }

    Ok(())
}

fn completed_subtree_count(chain_epoch: ChainEpoch, protocol: ShieldedProtocol) -> u32 {
    chain_epoch.tip_metadata.completed_subtree_count(protocol)
}

fn transaction_id_at_block_index(
    compact_block_payload: &[u8],
    tx_index: u64,
    height: BlockHeight,
) -> Result<TransactionId, QueryError> {
    use prost::Message as _;
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;

    let lightwalletd_block =
        LightwalletdCompactBlock::decode(compact_block_payload).map_err(|source| {
            QueryError::CompactBlockPayloadMalformed {
                height,
                reason: source.to_string(),
            }
        })?;

    let compact_transaction = lightwalletd_block
        .vtx
        .iter()
        .find(|compact_tx| compact_tx.index == tx_index)
        .ok_or_else(|| {
            artifact_unavailable(
                ArtifactFamily::Transaction,
                ArtifactKey::BlockTransactionIndex { height, tx_index },
            )
        })?;

    let txid_bytes: [u8; 32] = compact_transaction
        .txid
        .as_slice()
        .try_into()
        .map_err(|_| QueryError::CompactBlockPayloadMalformed {
            height,
            reason: format!(
                "indexed compact transaction txid is {} bytes, expected 32",
                compact_transaction.txid.len()
            ),
        })?;
    Ok(TransactionId::from_bytes(txid_bytes))
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
