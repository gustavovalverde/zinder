//! Local secondary-reader implementation of the chain-index contract.

use std::{num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_stream as stream;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockArtifact, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockSelector, ChainEpoch,
    CompactBlockArtifact, MinedDetails, MinedTransaction, Network, NetworkUpgradeActivations,
    RawTransactionBytes, SubtreeRootArtifact, SubtreeRootRange, TransactionBroadcastResult,
    TransactionId, TreeStateArtifact, TxStatus,
};
use zinder_proto::v1::wallet::WalletServerInfo;
use zinder_source::{
    block_header_info_from_raw_block_bytes, transparent_prevout_from_raw_transaction_bytes,
};
use zinder_store::{
    BlockHashLookup, ChainEventStreamFamily, ChainStoreOptions, SecondaryChainStore, StoreError,
    TransparentAddressTxIndexPageRequest, TransparentAddressUtxosPageRequest,
};

use crate::{
    BlockId, ChainEventCursor, ChainEventStream, ChainIndex, IndexStream, IndexerError,
    RemoteChainIndex, RemoteOpenOptions, TransparentAddressTxIdsQuery,
    TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem, TransparentAddressUtxoStream,
    TransparentAddressUtxoStreamItem, TransparentAddressUtxosQuery, TransparentAddressUtxosView,
};

/// Options for opening a local chain index over a `RocksDB` secondary.
#[derive(Clone, Debug)]
pub struct LocalOpenOptions {
    /// Canonical primary store path owned by `zinder-ingest`.
    pub storage_path: PathBuf,
    /// Process-unique secondary metadata path.
    pub secondary_path: PathBuf,
    /// Expected network stored in the canonical database.
    pub network: Network,
    /// Optional service endpoint used for subscriptions and command RPCs.
    pub subscription_endpoint: Option<String>,
    /// Periodic secondary catchup interval.
    pub catchup_interval: Duration,
    /// Node-discovered upgrade activations used to fill
    /// `MinedDetails.consensus_branch_id` on `transaction_by_id` responses.
    /// The production binary discovers this via
    /// `ZebraJsonRpcSource::discover_network_upgrade_activations`.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

/// Local chain index backed by a `RocksDB` secondary reader.
pub struct LocalChainIndex {
    store: SecondaryChainStore,
    remote_index: Option<RemoteChainIndex>,
    catchup_interval: Duration,
    catchup_cancel: CancellationToken,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

impl LocalChainIndex {
    /// Opens a local chain index and starts its secondary catchup loop.
    pub async fn open(options: LocalOpenOptions) -> Result<Self, IndexerError> {
        if options.catchup_interval.is_zero() {
            return Err(IndexerError::invalid_request(
                "catchup_interval must be greater than zero",
            ));
        }

        let storage_path = options.storage_path.clone();
        let secondary_path = options.secondary_path.clone();
        let network = options.network;
        let store = join_blocking(tokio::task::spawn_blocking(move || {
            SecondaryChainStore::open(
                storage_path,
                secondary_path,
                ChainStoreOptions::for_network(network),
            )
            .map_err(IndexerError::from_store_error)
        }))
        .await?;
        let store_for_initial_catchup = store.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            store_for_initial_catchup
                .try_catch_up()
                .map_err(IndexerError::from_store_error)
        }))
        .await?;

        let remote_index = match options.subscription_endpoint {
            Some(endpoint) => Some(RemoteChainIndex::connect(RemoteOpenOptions {
                endpoint,
                network,
            })?),
            None => None,
        };
        let catchup_cancel = CancellationToken::new();
        spawn_catchup_loop(
            store.clone(),
            options.catchup_interval,
            catchup_cancel.clone(),
        );

        Ok(Self {
            store,
            remote_index,
            catchup_interval: options.catchup_interval,
            catchup_cancel,
            network_upgrade_activations: options.network_upgrade_activations,
        })
    }

    async fn read_at_epoch<Output>(
        &self,
        at_epoch: Option<ChainEpoch>,
        read: impl FnOnce(&zinder_store::ChainEpochReader<'_>) -> Result<Output, IndexerError>
        + Send
        + 'static,
    ) -> Result<Output, IndexerError>
    where
        Output: Send + 'static,
    {
        let store = self.store.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let reader = match at_epoch {
                Some(at_epoch) => {
                    let reader = store
                        .chain_epoch_reader_at(at_epoch.id)
                        .map_err(|error| map_epoch_pin_store_error(error, at_epoch))?;
                    if reader.chain_epoch() != at_epoch {
                        return Err(IndexerError::FailedPrecondition {
                            reason: "stored chain epoch does not match at_epoch".to_owned(),
                        });
                    }
                    reader
                }
                None => store
                    .current_chain_epoch_reader()
                    .map_err(IndexerError::from_store_error)?,
            };
            read(&reader)
        }))
        .await
    }

    fn remote(&self, operation: &'static str) -> Result<&RemoteChainIndex, IndexerError> {
        self.remote_index
            .as_ref()
            .ok_or(IndexerError::RemoteEndpointUnconfigured { operation })
    }
}

impl Drop for LocalChainIndex {
    fn drop(&mut self) {
        self.catchup_cancel.cancel();
    }
}

#[async_trait]
impl ChainIndex for LocalChainIndex {
    async fn server_info(&self) -> Result<WalletServerInfo, IndexerError> {
        self.remote("server_info")?.server_info().await
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        self.read_at_epoch(None, |reader| Ok(reader.chain_epoch()))
            .await
    }

    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<BlockId, IndexerError> {
        self.read_at_epoch(at_epoch, |reader| {
            let chain_epoch = reader.chain_epoch();
            Ok(BlockId {
                height: chain_epoch.tip_height,
                hash: chain_epoch.tip_hash,
            })
        })
        .await
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockId, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| resolve_block_id(reader, selector))
            .await
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderInfo, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| {
            let block_id = resolve_block_id(reader, selector)?;
            let block = reader
                .block_at(block_id.height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound { resource: "block" })?;
            block_header_info_from_raw_block_bytes(block_id.height, &block.payload_bytes).map_err(
                |error| IndexerError::DataLoss {
                    reason: error.to_string(),
                },
            )
        })
        .await
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| {
            reader
                .compact_block_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "compact block",
                })
        })
        .await
    }

    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockArtifact, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| {
            reader
                .block_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound { resource: "block" })
        })
        .await
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        if block_range.start > block_range.end {
            return Err(IndexerError::invalid_request(
                "start height exceeds end height",
            ));
        }

        let compact_blocks = self
            .read_at_epoch(at_epoch, move |reader| {
                let maybe_blocks = reader
                    .compact_blocks_in_range(block_range)
                    .map_err(IndexerError::from_store_error)?;
                let mut compact_blocks = Vec::with_capacity(maybe_blocks.len());
                for (height, maybe_block) in block_range.into_iter().zip(maybe_blocks) {
                    let compact_block = maybe_block.ok_or(IndexerError::NotFound {
                        resource: "compact block",
                    })?;
                    if compact_block.height != height {
                        return Err(IndexerError::malformed(
                            "compact_block.height",
                            "height does not match requested range",
                        ));
                    }
                    compact_blocks.push(compact_block);
                }
                Ok(compact_blocks)
            })
            .await?;

        Ok(Box::pin(stream::iter(compact_blocks.into_iter().map(Ok))))
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| {
            reader
                .tree_state_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_at_epoch(at_epoch, |reader| {
            reader
                .latest_tree_state()
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn subtree_roots_in_range(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        self.read_at_epoch(at_epoch, move |reader| {
            let maybe_roots = reader
                .subtree_roots(subtree_root_range)
                .map_err(IndexerError::from_store_error)?;
            let mut subtree_roots = Vec::with_capacity(maybe_roots.len());
            for maybe_root in maybe_roots {
                subtree_roots.push(maybe_root.ok_or(IndexerError::NotFound {
                    resource: "subtree root",
                })?);
            }
            Ok(subtree_roots)
        })
        .await
    }

    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError> {
        let activations = self.network_upgrade_activations.clone();
        let mined_outcome = self
            .read_at_epoch(at_epoch, move |reader| {
                let Some(artifact) = reader
                    .transaction_by_id(transaction_id)
                    .map_err(IndexerError::from_store_error)?
                else {
                    return Ok(TxStatus::NotFound);
                };
                let chain_epoch = reader.chain_epoch();
                let block_time = reader
                    .block_at(artifact.block_height)
                    .map_err(IndexerError::from_store_error)?
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
                Ok(TxStatus::Mined(MinedTransaction::new(artifact, details)))
            })
            .await?;

        if !matches!(mined_outcome, TxStatus::NotFound) || at_epoch.is_some() {
            // Found mined, OR caller bound the read to a specific epoch
            // (mempool state is non-canonical and is not part of any
            // chain epoch). Either way, return the canonical answer.
            return Ok(mined_outcome);
        }

        // Canonical chain has no record. Delegate to the remote endpoint
        // to consult the live mempool index. Local secondary readers
        // cannot observe the writer's in-process mempool state, so this
        // path is skipped (NotFound is returned) when no remote endpoint
        // is wired.
        match self.remote("transaction_by_id_mempool_fallback") {
            Ok(remote) => remote.transaction_by_id(transaction_id, None).await,
            Err(IndexerError::RemoteEndpointUnconfigured { .. }) => Ok(TxStatus::NotFound),
            Err(error) => Err(error),
        }
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, IndexerError> {
        self.remote("broadcast_transaction")?
            .broadcast_transaction(raw_transaction)
            .await
    }

    async fn chain_events_for_family(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventStream, IndexerError> {
        self.remote("chain_events")?
            .chain_events_for_family(from_cursor, family)
            .await
    }

    async fn mempool_snapshot(
        &self,
        request: crate::MempoolSnapshotRequest,
    ) -> Result<crate::MempoolSnapshotView, IndexerError> {
        self.remote("mempool_snapshot")?
            .mempool_snapshot(request)
            .await
    }

    async fn mempool_events(
        &self,
        from_cursor: Option<crate::MempoolEventCursor>,
    ) -> Result<crate::MempoolEventStream, IndexerError> {
        self.remote("mempool_events")?
            .mempool_events(from_cursor)
            .await
    }

    async fn is_in_mempool(&self, transaction_id: TransactionId) -> Result<bool, IndexerError> {
        self.remote("is_in_mempool")?
            .is_in_mempool(transaction_id)
            .await
    }

    async fn transparent_address_utxos(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxosView, IndexerError> {
        let max_entries = query
            .max_entries
            .unwrap_or(DEFAULT_MAX_TRANSPARENT_ADDRESS_UTXOS);
        let store = self.store.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let cursor_token = query.from_cursor.map(|cursor| {
                zinder_store::StreamCursorTokenV1::from_bytes(cursor.as_bytes().to_vec())
            });
            let page = store
                .transparent_address_utxos_page(TransparentAddressUtxosPageRequest {
                    at_epoch,
                    address_script_hash: query.address_script_hash,
                    start_height: query.start_height,
                    max_entries,
                    from_cursor: cursor_token.as_ref(),
                })
                .map_err(map_transparent_utxo_store_error)?;
            Ok(TransparentAddressUtxosView {
                chain_epoch: page.chain_epoch,
                utxos: page.utxos,
                next_cursor: page.next_cursor.map(|cursor| {
                    crate::TransparentUtxoCursor::from_bytes(cursor.as_bytes().to_vec())
                }),
            })
        }))
        .await
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressTxIdsStream, IndexerError> {
        let max_entries = query
            .max_entries
            .unwrap_or(DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES);
        let store = self.store.clone();
        let page = join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let cursor_token = query.from_cursor.map(|cursor| {
                zinder_store::StreamCursorTokenV1::from_bytes(cursor.as_bytes().to_vec())
            });
            store
                .transparent_address_tx_index_page(TransparentAddressTxIndexPageRequest {
                    at_epoch,
                    address_script_hash: query.address_script_hash,
                    start_height: query.start_height,
                    end_height: query.end_height,
                    max_entries,
                    descending: query.descending,
                    from_cursor: cursor_token.as_ref(),
                })
                .map_err(map_transparent_utxo_store_error)
        }))
        .await?;
        let chain_epoch = page.chain_epoch;
        let next_cursor = page
            .next_cursor
            .map(|cursor| crate::TransparentHistoryCursor::from_bytes(cursor.as_bytes().to_vec()));
        let last_index = page.artifacts.len().saturating_sub(1);
        let items = page
            .artifacts
            .into_iter()
            .enumerate()
            .map(move |(index, artifact)| {
                Ok(TransparentAddressTxIdsStreamItem {
                    chain_epoch,
                    artifact,
                    cursor: if index == last_index {
                        next_cursor.clone()
                    } else {
                        None
                    },
                })
            });
        Ok(Box::pin(stream::iter(items)))
    }

    async fn transparent_address_utxos_stream(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxoStream, IndexerError> {
        let view = self.transparent_address_utxos(query, at_epoch).await?;
        let chain_epoch = view.chain_epoch;
        let next_cursor = view.next_cursor;
        let last_index = view.utxos.len().saturating_sub(1);
        let items = view
            .utxos
            .into_iter()
            .enumerate()
            .map(move |(index, utxo)| {
                Ok(TransparentAddressUtxoStreamItem {
                    chain_epoch,
                    utxo,
                    cursor: if index == last_index {
                        next_cursor.clone()
                    } else {
                        None
                    },
                })
            });
        Ok(Box::pin(stream::iter(items)))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: zinder_core::TransparentMempoolOutputsRequest,
    ) -> Result<Vec<zinder_core::TransparentMempoolOutput>, IndexerError> {
        self.remote("transparent_mempool_outputs_by_address")?
            .transparent_mempool_outputs_by_address(request)
            .await
    }

    async fn transparent_mempool_spend_by_outpoint(
        &self,
        outpoint: zinder_core::TransparentOutPoint,
    ) -> Result<Option<zinder_core::TransparentMempoolSpend>, IndexerError> {
        self.remote("transparent_mempool_spend_by_outpoint")?
            .transparent_mempool_spend_by_outpoint(outpoint)
            .await
    }

    async fn transparent_address_balance(
        &self,
        addresses: &[zinder_core::TransparentAddressScriptHash],
        at_epoch: Option<zinder_core::ChainEpoch>,
    ) -> Result<zinder_core::TransparentAddressBalance, IndexerError> {
        self.remote("transparent_address_balance")?
            .transparent_address_balance(addresses, at_epoch)
            .await
    }

    async fn transparent_mempool_prevouts(
        &self,
        outpoints: &[zinder_core::TransparentOutPoint],
    ) -> Result<zinder_core::TransparentPrevoutsResponse, IndexerError> {
        self.remote("transparent_mempool_prevouts")?
            .transparent_mempool_prevouts(outpoints)
            .await
    }

    async fn transparent_prevouts(
        &self,
        outpoints: &[zinder_core::TransparentOutPoint],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<zinder_core::TransparentPrevoutsResponse, IndexerError> {
        let outpoints = normalize_transparent_prevout_outpoints(outpoints)?;
        self.read_at_epoch(at_epoch, move |reader| {
            let chain_epoch = reader.chain_epoch();
            let mut entries = Vec::with_capacity(outpoints.len());
            let mut payload_cache: std::collections::HashMap<TransactionId, Option<Vec<u8>>> =
                std::collections::HashMap::new();
            for outpoint in outpoints {
                let cached_payload = match payload_cache.entry(outpoint.transaction_id) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let payload = reader
                            .transaction_by_id(outpoint.transaction_id)
                            .map_err(IndexerError::from_store_error)?
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
                    .map_err(|error| {
                        IndexerError::malformed("transparent_prevout", error.to_string())
                    })?,
                };
                entries.push(zinder_core::TransparentPrevoutEntry { outpoint, prevout });
            }
            Ok(zinder_core::TransparentPrevoutsResponse {
                chain_epoch,
                entries,
            })
        })
        .await
    }

    fn local_catchup_interval(&self) -> Option<Duration> {
        Some(self.catchup_interval)
    }
}

fn normalize_transparent_prevout_outpoints(
    outpoints: &[zinder_core::TransparentOutPoint],
) -> Result<Vec<zinder_core::TransparentOutPoint>, IndexerError> {
    for (request_index, outpoint) in outpoints.iter().enumerate() {
        if outpoint.is_coinbase_sentinel() {
            return Err(IndexerError::invalid_request(format!(
                "outpoints[{request_index}] is the coinbase sentinel \
                 (transaction_id == [0u8; 32], output_index == 0xFFFFFFFF); \
                 filter coinbase inputs at the request boundary",
            )));
        }
    }

    Ok(outpoints
        .iter()
        .take(zinder_core::MAX_TRANSPARENT_PREVOUTS_PER_REQUEST)
        .copied()
        .collect())
}

fn spawn_catchup_loop(store: SecondaryChainStore, interval: Duration, cancel: CancellationToken) {
    let _catchup_handle: JoinHandle<()> = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    let catchup_store = store.clone();
                    let _catchup_result = tokio::task::spawn_blocking(move || {
                        catchup_store.try_catch_up()
                    })
                    .await;
                }
            }
        }
    });
}

const DEFAULT_MAX_TRANSPARENT_ADDRESS_UTXOS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
const DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only the typed transparent-UTXO cursor error becomes a client invalid-request error; every other storage failure preserves its shared mapping."
)]
fn map_transparent_utxo_store_error(error: StoreError) -> IndexerError {
    match error {
        StoreError::TransparentUtxoCursorInvalid { reason } => {
            IndexerError::invalid_request(reason)
        }
        _ => IndexerError::from_store_error(error),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only a missing pinned epoch becomes a client precondition error; all other storage failures keep the shared storage mapping."
)]
fn map_epoch_pin_store_error(error: StoreError, at_epoch: ChainEpoch) -> IndexerError {
    match error {
        StoreError::ChainEpochMissing { .. } => IndexerError::FailedPrecondition {
            reason: format!("chain epoch {} is not retained", at_epoch.id.value()),
        },
        error => IndexerError::from_store_error(error),
    }
}

async fn join_blocking<Output>(
    handle: tokio::task::JoinHandle<Result<Output, IndexerError>>,
) -> Result<Output, IndexerError> {
    match handle.await {
        Ok(blocking_outcome) => blocking_outcome,
        Err(error) => Err(IndexerError::BlockingTaskFailed {
            reason: error.to_string(),
        }),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "BlockSelector is #[non_exhaustive]; adding new selector variants is a separate decision per the gap doc, not a default fall-through"
)]
fn resolve_block_id(
    reader: &zinder_store::ChainEpochReader<'_>,
    selector: BlockSelector,
) -> Result<BlockId, IndexerError> {
    match selector {
        BlockSelector::Height(height) => {
            let chain_epoch = reader.chain_epoch();
            if height > chain_epoch.tip_height {
                return Err(IndexerError::NotFound { resource: "block" });
            }
            let block = reader
                .block_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound { resource: "block" })?;
            Ok(BlockId::new(height, block.block_hash))
        }
        BlockSelector::Hash(hash) => {
            match reader
                .block_hash_lookup(hash)
                .map_err(IndexerError::from_store_error)?
            {
                BlockHashLookup::Resolved(block_id) => Ok(block_id),
                BlockHashLookup::NotInBestChain | BlockHashLookup::NotIndexed => {
                    Err(IndexerError::NotFound { resource: "block" })
                }
            }
        }
        _ => Err(IndexerError::InvalidRequest {
            reason: "unsupported block selector variant".to_owned(),
        }),
    }
}
