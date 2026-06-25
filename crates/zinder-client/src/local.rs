//! Local secondary-reader implementation of the chain-index contract.

use std::{num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_stream as stream;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockBlobArtifact, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockSelector, ChainEpoch,
    ChainEpochId, CompactBlockArtifact, MinedDetails, MinedTransaction, Network,
    NetworkUpgradeActivations, SubtreeRootArtifact, SubtreeRootRange, TransactionId,
    TransparentAddressBalance, TransparentAddressScriptHash, TreeStateArtifact, TxStatus,
};
use zinder_derive::{
    DeriveStore, DeriveStoreOptions, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TransparentAddressTransactionHistoryConsumer, TransparentAddressTransactionHistoryPageRequest,
};
use zinder_store::{
    AddressOutputIndexPageRequest, BlockHashLookup, ChainStoreOptions, RocksDbResourceBudget,
    SecondaryChainStore, StoreError,
};

use crate::{
    BlockId, ChainIndex, IndexStream, IndexerError, RemoteChainIndex, RemoteOpenOptions,
    TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem,
    TransparentAddressUnspentOutputsQuery, TransparentAddressUnspentOutputsStream,
    TransparentUnspentOutputStreamItem,
};

/// Default maximum time spent on initial secondary catchup during local open.
pub const DEFAULT_INITIAL_CATCHUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Options for opening a local chain index over a `RocksDB` secondary.
#[derive(Clone, Debug)]
pub struct LocalOpenOptions {
    /// Canonical primary store path owned by `zinder-ingest`.
    pub storage_path: PathBuf,
    /// Process-unique secondary metadata path.
    pub secondary_path: PathBuf,
    /// Expected network stored in the canonical database.
    pub network: Network,
    /// Bounded `RocksDB` resource budget applied when opening the canonical
    /// secondary store.
    pub canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// Bounded `RocksDB` resource budget applied when opening the derive
    /// secondary store.
    pub derive_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// Optional service endpoint used for subscriptions and command RPCs.
    pub subscription_endpoint: Option<String>,
    /// Periodic secondary catchup interval.
    pub catchup_interval: Duration,
    /// Maximum initial catchup duration before opening with the current
    /// secondary view.
    pub initial_catchup_timeout: Duration,
    /// Node-discovered upgrade activations used to fill
    /// `MinedDetails.consensus_branch_id` on `transaction_by_id` responses.
    /// The production binary discovers this via
    /// `ZebraJsonRpcSource::discover_network_upgrade_activations`.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

/// Local chain index backed by a `RocksDB` secondary reader.
pub struct LocalChainIndex {
    store: SecondaryChainStore,
    derive_store: Option<DeriveStore>,
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
        if options.initial_catchup_timeout.is_zero() {
            return Err(IndexerError::invalid_request(
                "initial_catchup_timeout must be greater than zero",
            ));
        }

        let storage_path = options.storage_path.clone();
        let secondary_path = options.secondary_path.clone();
        let network = options.network;
        let canonical_rocksdb_budget = options.canonical_rocksdb_budget;
        let store = join_blocking(tokio::task::spawn_blocking(move || {
            SecondaryChainStore::open(
                storage_path,
                secondary_path,
                ChainStoreOptions {
                    rocksdb_resource_budget: canonical_rocksdb_budget,
                    ..ChainStoreOptions::for_network(network)
                },
            )
            .map_err(IndexerError::from_store_error)
        }))
        .await?;
        let store_for_initial_catchup = store.clone();
        try_catch_up_store_with_timeout(
            store_for_initial_catchup,
            options.initial_catchup_timeout,
            "canonical",
        )
        .await?;
        let derive_store = open_derive_secondary_with_timeout(
            options.storage_path.clone(),
            options.secondary_path.clone(),
            options.derive_rocksdb_budget,
            options.initial_catchup_timeout,
        )
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
            derive_store,
            remote_index,
            catchup_interval: options.catchup_interval,
            catchup_cancel,
            network_upgrade_activations: options.network_upgrade_activations,
        })
    }

    async fn read_at_epoch<Output>(
        &self,
        at_epoch_id: Option<ChainEpochId>,
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
            let reader = match at_epoch_id {
                Some(at_epoch_id) => store
                    .chain_epoch_reader_at(at_epoch_id)
                    .map_err(|error| map_epoch_pin_store_error(error, at_epoch_id))?,
                None => store
                    .current_chain_epoch_reader()
                    .map_err(IndexerError::from_store_error)?,
            };
            read(&reader)
        }))
        .await
    }
}

impl Drop for LocalChainIndex {
    fn drop(&mut self) {
        self.catchup_cancel.cancel();
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "The local implementation mirrors the public ChainIndex trait one method at a time."
)]
#[async_trait]
impl ChainIndex for LocalChainIndex {
    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        self.read_at_epoch(None, |reader| Ok(reader.chain_epoch()))
            .await
    }

    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        self.read_at_epoch(at_epoch_id, |reader| {
            let chain_epoch = reader.chain_epoch();
            Ok(BlockId {
                height: chain_epoch.visible_tip_height,
                hash: chain_epoch.visible_tip_hash,
            })
        })
        .await
    }

    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        self.read_at_epoch(at_epoch_id, |reader| {
            let chain_epoch = reader.chain_epoch();
            Ok(BlockId {
                height: chain_epoch.settled_tip_height,
                hash: chain_epoch.settled_tip_hash,
            })
        })
        .await
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
            resolve_block_id(reader, selector)
        })
        .await
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderInfo, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
            let block_id = resolve_block_id(reader, selector)?;
            let block = reader
                .block_header_at(block_id.height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound { resource: "block" })?;
            Ok(block.into_header_info())
        })
        .await
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
            reader
                .compact_block_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "compact block",
                })
        })
        .await
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        if block_range.start > block_range.end {
            return Err(IndexerError::invalid_request(
                "start height exceeds end height",
            ));
        }

        let compact_blocks = self
            .read_at_epoch(at_epoch_id, move |reader| {
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

    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockBlobArtifact, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
            reader
                .block_blob_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "full block",
                })
        })
        .await
    }

    async fn full_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<IndexStream<BlockBlobArtifact>, IndexerError> {
        if block_range.start > block_range.end {
            return Err(IndexerError::invalid_request(
                "start height exceeds end height",
            ));
        }

        let block_blobs = self
            .read_at_epoch(at_epoch_id, move |reader| {
                let maybe_blobs = reader
                    .block_blobs_in_range(block_range)
                    .map_err(IndexerError::from_store_error)?;
                let mut block_blobs = Vec::with_capacity(maybe_blobs.len());
                for (height, maybe_blob) in block_range.into_iter().zip(maybe_blobs) {
                    let block_blob = maybe_blob.ok_or(IndexerError::NotFound {
                        resource: "full block",
                    })?;
                    if block_blob.height != height {
                        return Err(IndexerError::malformed(
                            "full_block.height",
                            "height does not match requested range",
                        ));
                    }
                    block_blobs.push(block_blob);
                }
                Ok(block_blobs)
            })
            .await?;

        Ok(Box::pin(stream::iter(block_blobs.into_iter().map(Ok))))
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
            reader
                .tree_state_checkpoint_at_or_before(height)
                .map_err(IndexerError::from_store_error)?
                .filter(|tree_state| tree_state.height == height)
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_at_epoch(at_epoch_id, |reader| {
            reader
                .latest_tree_state_checkpoint()
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        self.read_at_epoch(at_epoch_id, move |reader| {
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TxStatus, IndexerError> {
        let activations = self.network_upgrade_activations.clone();
        let mined_outcome = self
            .read_at_epoch(at_epoch_id, move |reader| {
                let Some(artifact) = reader
                    .transaction_facts_by_id(transaction_id)
                    .map_err(IndexerError::from_store_error)?
                else {
                    return Ok(TxStatus::NotFound);
                };
                let chain_epoch = reader.chain_epoch();
                let block_time = reader
                    .block_header_at(artifact.location.block_height)
                    .map_err(IndexerError::from_store_error)?
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
                    .transaction_blob_by_id(transaction_id)
                    .map_err(IndexerError::from_store_error)?
                    .map(|blob| blob.raw_transaction_bytes)
                    .unwrap_or_default();
                Ok(TxStatus::Mined(MinedTransaction::new(
                    artifact.location,
                    details,
                    raw_transaction_bytes,
                )))
            })
            .await?;

        if !matches!(mined_outcome, TxStatus::NotFound) || at_epoch_id.is_some() {
            // Found mined, OR caller bound the read to a specific epoch
            // (mempool state is non-canonical and is not part of any
            // chain epoch). Either way, return the canonical answer.
            return Ok(mined_outcome);
        }

        // Canonical chain has no record. A colocated secondary reader cannot
        // observe the writer's in-process mempool state, so consult the live
        // mempool only when an ingest-control endpoint is wired; otherwise the
        // answer is NotFound.
        match &self.remote_index {
            Some(remote) => remote.transaction_by_id(transaction_id, None).await,
            None => Ok(TxStatus::NotFound),
        }
    }

    async fn transparent_address_unspent_outputs(
        &self,
        query: TransparentAddressUnspentOutputsQuery,
    ) -> Result<TransparentAddressUnspentOutputsStream, IndexerError> {
        let store = self.store.clone();
        let (chain_epoch, outputs) = join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let page = store
                .address_output_index_page(AddressOutputIndexPageRequest {
                    at_epoch: None,
                    address_script_hash: query.address_script_hash,
                    start_height: query.start_height,
                    max_entries: NonZeroU32::MAX,
                    from_cursor: None,
                })
                .map_err(IndexerError::from_store_error)?;
            Ok((page.chain_epoch, page.outputs))
        }))
        .await?;
        let items = outputs.into_iter().map(move |output| {
            Ok(TransparentUnspentOutputStreamItem {
                chain_epoch,
                output,
            })
        });
        Ok(Box::pin(stream::iter(items)))
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
    ) -> Result<TransparentAddressTxIdsStream, IndexerError> {
        let max_entries = query
            .max_entries
            .unwrap_or(DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES);
        let Some(derive_store) = self.derive_store.clone() else {
            return Err(IndexerError::FailedPrecondition {
                reason: "transparent-address transaction history derive projection is unavailable"
                    .to_owned(),
            });
        };
        let store = self.store.clone();
        let page = join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let chain_epoch = store
                .current_chain_epoch_reader()
                .map_err(IndexerError::from_store_error)?
                .chain_epoch();
            derive_store
                .try_catch_up()
                .map_err(IndexerError::from_derive_store_error)?;
            let derive_height = derive_store
                .last_materialized_height_ascending(
                    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
                )
                .map_err(IndexerError::from_derive_store_error)?;
            if derive_height.is_none_or(|height| height < chain_epoch.visible_tip_height) {
                return Err(IndexerError::FailedPrecondition {
                    reason: format!(
                        "transparent-address transaction history is behind canonical height {}: derive height {:?}",
                        chain_epoch.visible_tip_height.value(),
                        derive_height.map(BlockHeight::value)
                    ),
                });
            }
            let cursor_token = query.from_cursor.map(|cursor| {
                zinder_store::StreamCursorTokenV1::from_bytes(cursor.as_bytes().to_vec())
            });
            let page = TransparentAddressTransactionHistoryConsumer::read_page(
                &derive_store,
                TransparentAddressTransactionHistoryPageRequest {
                    address_script_hash: query.address_script_hash,
                    start_height: query.start_height,
                    end_height: query.end_height,
                    max_entries,
                    descending: query.descending,
                    from_cursor: cursor_token.as_ref(),
                },
            )
            .map_err(IndexerError::from_derive_store_error)?;
            Ok((chain_epoch, page.artifacts, page.next_cursor))
        }))
        .await?;
        let (chain_epoch, artifacts, next_cursor) = page;
        let next_cursor = next_cursor
            .map(|cursor| crate::TransparentHistoryCursor::from_bytes(cursor.as_bytes().to_vec()));
        let last_index = artifacts.len().saturating_sub(1);
        let items = artifacts
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

    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
    ) -> Result<TransparentAddressBalance, IndexerError> {
        let address_count = u32::try_from(addresses.len())
            .ok()
            .filter(|count| (1..=MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES).contains(count))
            .ok_or_else(|| IndexerError::InvalidRequest {
                reason: format!(
                    "transparent address balance accepts 1..={MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES} addresses, got {}",
                    addresses.len()
                ),
            })?;
        let addresses = addresses.to_vec();
        let store = self.store.clone();
        join_blocking(tokio::task::spawn_blocking(move || {
            store
                .try_catch_up()
                .map_err(IndexerError::from_store_error)?;
            let chain_epoch = store
                .current_chain_epoch_reader()
                .map_err(IndexerError::from_store_error)?
                .chain_epoch();
            let mut confirmed_zat: u64 = 0;
            for address_script_hash in addresses {
                let page = store
                    .address_output_index_page(AddressOutputIndexPageRequest {
                        at_epoch: None,
                        address_script_hash,
                        start_height: BlockHeight::new(0),
                        max_entries: NonZeroU32::MAX,
                        from_cursor: None,
                    })
                    .map_err(IndexerError::from_store_error)?;
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
        .await
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: &[zinder_core::TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<zinder_core::TransparentOutputsByOutpointResponse, IndexerError> {
        let outpoints = normalize_transparent_outpoints(outpoints)?;
        self.read_at_epoch(at_epoch_id, move |reader| {
            let chain_epoch = reader.chain_epoch();
            let prevouts_by_outpoint = reader
                .transparent_outputs_by_outpoints(&outpoints)
                .map_err(IndexerError::from_store_error)?;
            let mut entries = Vec::with_capacity(outpoints.len());
            for outpoint in outpoints {
                let prevout = prevouts_by_outpoint
                    .get(&outpoint)
                    .cloned()
                    .map(zinder_core::TransparentOutputArtifact::into_output);
                entries.push(zinder_core::TransparentOutputEntry {
                    outpoint,
                    output: prevout,
                });
            }
            Ok(zinder_core::TransparentOutputsByOutpointResponse {
                chain_epoch,
                entries,
            })
        })
        .await
    }

    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: &[zinder_core::TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<zinder_core::TransparentSpendsByOutpointResponse, IndexerError> {
        let outpoints = normalize_transparent_outpoints(outpoints)?;
        self.read_at_epoch(at_epoch_id, move |reader| {
            let chain_epoch = reader.chain_epoch();
            let spends_by_outpoint = reader
                .transparent_spend_facts_by_outpoints(&outpoints)
                .map_err(IndexerError::from_store_error)?;
            let mut spends = Vec::with_capacity(spends_by_outpoint.len());
            let mut seen = std::collections::HashSet::with_capacity(spends_by_outpoint.len());
            for outpoint in outpoints {
                if let Some(fact) = spends_by_outpoint.get(&outpoint)
                    && seen.insert(outpoint)
                {
                    spends.push(zinder_core::TransparentSpendEntry::from_spend_fact(fact));
                }
            }
            Ok(zinder_core::TransparentSpendsByOutpointResponse {
                chain_epoch,
                spends,
            })
        })
        .await
    }

    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        outpoints: &[zinder_core::TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<zinder_core::TransparentUnspentOutputsByOutpointResponse, IndexerError> {
        let outpoints = normalize_transparent_outpoints(outpoints)?;
        self.read_at_epoch(at_epoch_id, move |reader| {
            let chain_epoch = reader.chain_epoch();
            let outputs_by_outpoint = reader
                .transparent_outputs_by_outpoints(&outpoints)
                .map_err(IndexerError::from_store_error)?;
            let spends_by_outpoint = reader
                .transparent_spend_facts_by_outpoints(&outpoints)
                .map_err(IndexerError::from_store_error)?;
            let mut entries = Vec::with_capacity(outputs_by_outpoint.len());
            let mut seen = std::collections::HashSet::with_capacity(outputs_by_outpoint.len());
            for outpoint in outpoints {
                if spends_by_outpoint.contains_key(&outpoint) {
                    continue;
                }
                if let Some(output) = outputs_by_outpoint.get(&outpoint)
                    && seen.insert(outpoint)
                {
                    entries.push(zinder_core::TransparentOutputEntry {
                        outpoint,
                        output: Some(output.clone().into_output()),
                    });
                }
            }
            Ok(zinder_core::TransparentUnspentOutputsByOutpointResponse {
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

/// Rejects the coinbase sentinel and caps an outpoint batch.
///
/// Caps at [`zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`]. Shared by the
/// canonical output resolver and the canonical reverse-spend resolver, which
/// apply the same coinbase-rejection and cap rules.
fn normalize_transparent_outpoints(
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
        .take(zinder_core::MAX_TRANSPARENT_OUTPUTS_PER_REQUEST)
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

const DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

/// Upper bound on the address count accepted by a single transparent-address
/// balance read, matching the wallet plane's `WalletQuery` bound.
const MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES: u32 = 256;

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only a missing pinned epoch becomes a client precondition error; all other storage failures keep the shared storage mapping."
)]
fn map_epoch_pin_store_error(error: StoreError, at_epoch_id: ChainEpochId) -> IndexerError {
    match error {
        StoreError::ChainEpochMissing { .. } => IndexerError::FailedPrecondition {
            reason: format!("chain epoch {} is not retained", at_epoch_id.value()),
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

async fn open_derive_secondary_with_timeout(
    storage_path: PathBuf,
    secondary_path: PathBuf,
    derive_rocksdb_budget: RocksDbResourceBudget,
    timeout: Duration,
) -> Result<Option<DeriveStore>, IndexerError> {
    let derive_storage_path = DeriveStore::path_for_canonical(&storage_path);
    let derive_secondary_path = secondary_path.join("derive");
    let derive_store =
        join_blocking(tokio::task::spawn_blocking(
            move || match DeriveStore::open_secondary(
                derive_storage_path,
                derive_secondary_path,
                DeriveStoreOptions {
                    sync_writes: false,
                    consumer_column_families: DeriveStore::bundled_consumer_column_families(),
                    rocksdb_resource_budget: derive_rocksdb_budget,
                },
            ) {
                Ok(derive_store) => Ok(Some(derive_store)),
                Err(zinder_derive::DeriveStoreError::Open { .. }) => Ok(None),
                Err(error) => Err(IndexerError::from_derive_store_error(error)),
            },
        ))
        .await?;
    if let Some(derive_store_for_initial_catchup) = derive_store.clone() {
        try_catch_up_derive_store_with_timeout(derive_store_for_initial_catchup, timeout).await?;
    }
    Ok(derive_store)
}

async fn try_catch_up_store_with_timeout(
    store: SecondaryChainStore,
    timeout: Duration,
    role: &'static str,
) -> Result<(), IndexerError> {
    let handle = tokio::task::spawn_blocking(move || {
        store
            .try_catch_up()
            .map(|_| ())
            .map_err(IndexerError::from_store_error)
    });
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(catchup_outcome)) => catchup_outcome,
        Ok(Err(join_error)) => Err(IndexerError::BlockingTaskFailed {
            reason: join_error.to_string(),
        }),
        Err(_) => {
            tracing::warn!(
                target: "zinder::client",
                event = "initial_secondary_catchup_timed_out",
                role,
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "initial secondary catchup timed out; opening with the current secondary view"
            );
            Ok(())
        }
    }
}

async fn try_catch_up_derive_store_with_timeout(
    derive_store: DeriveStore,
    timeout: Duration,
) -> Result<(), IndexerError> {
    let handle = tokio::task::spawn_blocking(move || {
        derive_store
            .try_catch_up()
            .map_err(IndexerError::from_derive_store_error)
    });
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(catchup_outcome)) => catchup_outcome,
        Ok(Err(join_error)) => Err(IndexerError::BlockingTaskFailed {
            reason: join_error.to_string(),
        }),
        Err(_) => {
            tracing::warn!(
                target: "zinder::client",
                event = "initial_secondary_catchup_timed_out",
                role = "derive",
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "initial derive secondary catchup timed out; opening with the current secondary view"
            );
            Ok(())
        }
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
            if height > chain_epoch.visible_tip_height {
                return Err(IndexerError::NotFound { resource: "block" });
            }
            let block = reader
                .block_header_at(height)
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
