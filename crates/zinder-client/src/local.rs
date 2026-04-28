//! Local secondary-reader implementation of the chain-index contract.

use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_stream as stream;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact, Network, RawTransactionBytes,
    SubtreeRootArtifact, SubtreeRootRange, TransactionBroadcastResult, TransactionId,
    TreeStateArtifact,
};
use zinder_proto::v1::wallet::ServerCapabilities;
use zinder_store::{ChainEventStreamFamily, ChainStoreOptions, SecondaryChainStore, StoreError};

use crate::{
    BlockId, ChainEventCursor, ChainEventStream, ChainIndex, IndexStream, IndexerError,
    RemoteChainIndex, RemoteOpenOptions, TxStatus,
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
}

/// Local chain index backed by a `RocksDB` secondary reader.
pub struct LocalChainIndex {
    store: SecondaryChainStore,
    remote_index: Option<RemoteChainIndex>,
    catchup_interval: Duration,
    catchup_cancel: CancellationToken,
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
            Some(endpoint) => {
                Some(RemoteChainIndex::connect(RemoteOpenOptions { endpoint, network }).await?)
            }
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
        })
    }

    async fn read_from_current<Output>(
        &self,
        read: impl FnOnce(&zinder_store::ChainEpochReader<'_>) -> Result<Output, IndexerError>
        + Send
        + 'static,
    ) -> Result<Output, IndexerError>
    where
        Output: Send + 'static,
    {
        self.read_from_epoch(None, read).await
    }

    async fn read_from_epoch<Output>(
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
    async fn server_info(&self) -> Result<ServerCapabilities, IndexerError> {
        self.remote("server_info")?.server_info().await
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        self.read_from_current(|reader| Ok(reader.chain_epoch()))
            .await
    }

    async fn latest_block(&self) -> Result<BlockId, IndexerError> {
        self.read_from_current(|reader| {
            let chain_epoch = reader.chain_epoch();
            Ok(BlockId {
                height: chain_epoch.tip_height,
                hash: chain_epoch.tip_hash,
            })
        })
        .await
    }

    async fn latest_block_at_epoch(&self, at_epoch: ChainEpoch) -> Result<BlockId, IndexerError> {
        self.read_from_epoch(Some(at_epoch), |reader| {
            let chain_epoch = reader.chain_epoch();
            Ok(BlockId {
                height: chain_epoch.tip_height,
                hash: chain_epoch.tip_hash,
            })
        })
        .await
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.read_from_current(move |reader| {
            reader
                .compact_block_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "compact block",
                })
        })
        .await
    }

    async fn compact_block_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.read_from_epoch(Some(at_epoch), move |reader| {
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
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        self.compact_blocks_in_range_from_epoch(block_range, None)
            .await
    }

    async fn compact_blocks_in_range_at_epoch(
        &self,
        block_range: BlockHeightRange,
        at_epoch: ChainEpoch,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        self.compact_blocks_in_range_from_epoch(block_range, Some(at_epoch))
            .await
    }

    async fn tree_state_at(&self, height: BlockHeight) -> Result<TreeStateArtifact, IndexerError> {
        self.read_from_current(move |reader| {
            reader
                .tree_state_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn tree_state_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_from_epoch(Some(at_epoch), move |reader| {
            reader
                .tree_state_at(height)
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn latest_tree_state(&self) -> Result<TreeStateArtifact, IndexerError> {
        self.read_from_current(|reader| {
            reader
                .latest_tree_state()
                .map_err(IndexerError::from_store_error)?
                .ok_or(IndexerError::NotFound {
                    resource: "tree state",
                })
        })
        .await
    }

    async fn latest_tree_state_at_epoch(
        &self,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.read_from_epoch(Some(at_epoch), |reader| {
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
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        self.subtree_roots_in_range_from_epoch(subtree_root_range, None)
            .await
    }

    async fn subtree_roots_in_range_at_epoch(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: ChainEpoch,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        self.subtree_roots_in_range_from_epoch(subtree_root_range, Some(at_epoch))
            .await
    }

    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
    ) -> Result<TxStatus, IndexerError> {
        self.transaction_by_id_from_epoch(transaction_id, None)
            .await
    }

    async fn transaction_by_id_at_epoch(
        &self,
        transaction_id: TransactionId,
        at_epoch: ChainEpoch,
    ) -> Result<TxStatus, IndexerError> {
        self.transaction_by_id_from_epoch(transaction_id, Some(at_epoch))
            .await
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

    fn local_catchup_interval(&self) -> Option<Duration> {
        Some(self.catchup_interval)
    }
}

impl LocalChainIndex {
    async fn compact_blocks_in_range_from_epoch(
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
            .read_from_epoch(at_epoch, move |reader| {
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

    async fn subtree_roots_in_range_from_epoch(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        self.read_from_epoch(at_epoch, move |reader| {
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

    async fn transaction_by_id_from_epoch(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError> {
        self.read_from_epoch(at_epoch, move |reader| {
            let Some(transaction) = reader
                .transaction_by_id(transaction_id)
                .map_err(IndexerError::from_store_error)?
            else {
                return Ok(TxStatus::NotFound);
            };

            Ok(TxStatus::Mined(transaction))
        })
        .await
    }
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
