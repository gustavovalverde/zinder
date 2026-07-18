//! Wallet query implementation over admitted version-1 canonical and wallet stores.

use std::{
    collections::HashSet,
    num::{NonZeroU16, NonZeroU32},
    sync::Arc,
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use zinder_core::{
    BlockHeight, BlockId, BlockSelector, ChainEpoch, ChainEpochId, MinedDetails, MinedTransaction,
    NetworkUpgradeActivations, RawTransactionBytes, ShieldedProtocol, TransactionId,
    TransparentAddressBalance, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentOutPoint, TransparentOutputsByOutpointResponse, TransparentSpendsByOutpointResponse,
    TransparentUnspentOutput, TransparentUnspentOutputsByOutpointResponse,
    TransparentUtxoSetSummary, TxStatus,
};
use zinder_source::{TransactionBroadcaster, TreeStateUpstream};
use zinder_store::{
    ArtifactFamily, ChainEventStreamFamily, EventStreamStartPosition, StreamCursorTokenV1,
};
use zinder_wallet_projection::{WalletAddressTransactionKey, WalletAddressUnspentOutputKey};

use crate::{
    ArtifactKey, BlockHeaderResponseValue, BlockIdResponseValue, ChainEvents, CompactBlock,
    CompactBlockRange, ExactReadPair, FullBlock, FullBlockStream, LatestBlock, LatestSafeBlock,
    QueryError, RawTransaction, SubtreeRoots, Transaction, TransactionStatus,
    TransparentAddressTxIds, TransparentAddressTxIdsInRangeRequest,
    TransparentAddressUnspentOutputs, TransparentAddressUnspentOutputsRequest, TreeState,
    WalletQueryApi,
};

const WALLET_READ_PAGE_SIZE: NonZeroU16 = NonZeroU16::MAX;

/// Exact-fence wallet query over the clean version-1 stores.
#[derive(Clone)]
pub struct ExactPairWalletQuery<Broadcaster> {
    read_pairs: Arc<ArcSwap<ExactReadPair>>,
    broadcaster: Broadcaster,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    tree_state_upstream: Option<Arc<dyn TreeStateUpstream>>,
}

impl<Broadcaster> std::fmt::Debug for ExactPairWalletQuery<Broadcaster> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pair = self.capture_pair();
        formatter
            .debug_struct("ExactPairWalletQuery")
            .field("canonical_fence", &pair.canonical_fence())
            .field("wallet_fence", &pair.wallet_source())
            .field("tree_state_upstream", &self.tree_state_upstream.is_some())
            .finish_non_exhaustive()
    }
}

impl<Broadcaster> ExactPairWalletQuery<Broadcaster> {
    /// Builds a query over a swappable slot of already-admitted immutable pairs.
    ///
    /// Every [`WalletQueryApi`] method captures one `Arc` from this slot before
    /// reading. A publisher can therefore atomically replace the pair without
    /// changing the canonical or wallet reader observed by an in-flight request.
    #[must_use]
    pub fn from_read_pair_slot(
        read_pairs: Arc<ArcSwap<ExactReadPair>>,
        broadcaster: Broadcaster,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        Self {
            read_pairs,
            broadcaster,
            network_upgrade_activations,
            tree_state_upstream: None,
        }
    }

    /// Attaches the node-backed sparse tree-state fill path.
    #[must_use]
    pub fn with_tree_state_upstream(mut self, upstream: Arc<dyn TreeStateUpstream>) -> Self {
        self.tree_state_upstream = Some(upstream);
        self
    }

    fn capture_pair(&self) -> Arc<ExactReadPair> {
        self.read_pairs.load_full()
    }

    fn chain_epoch(
        pair: &ExactReadPair,
        requested: Option<ChainEpochId>,
    ) -> Result<ChainEpoch, QueryError> {
        let chain_epoch = pair.canonical().chain_epoch()?;
        if requested.is_some_and(|requested| requested != chain_epoch.id) {
            return Err(QueryError::ChainEpochPinUnavailable {
                chain_epoch_id: requested.unwrap_or(chain_epoch.id),
            });
        }
        Ok(chain_epoch)
    }

    fn block_id_at(pair: &ExactReadPair, height: BlockHeight) -> Result<BlockId, QueryError> {
        pair.canonical()
            .block_header_at(height)?
            .map(|header| BlockId::new(height, header.block_hash))
            .ok_or(QueryError::BlockNotInBestChain)
    }

    fn resolve_block_id_by_selector(
        pair: &ExactReadPair,
        selector: BlockSelector,
        chain_epoch: ChainEpoch,
    ) -> Result<BlockIdResponseValue, QueryError> {
        let block_id = match selector {
            BlockSelector::Height(height) if height <= chain_epoch.visible_tip_height => {
                Self::block_id_at(pair, height)?
            }
            BlockSelector::Hash(hash) if hash == chain_epoch.visible_tip_hash => {
                BlockId::new(chain_epoch.visible_tip_height, hash)
            }
            BlockSelector::Height(_) | BlockSelector::Hash(_) => {
                return Err(QueryError::BlockNotInBestChain);
            }
            _ => {
                return Err(QueryError::UnsupportedBlockSelector {
                    reason: "selector is not supported by the version-1 canonical reader",
                });
            }
        };
        Ok(BlockIdResponseValue {
            chain_epoch,
            block_id,
        })
    }
}

#[async_trait]
impl<Broadcaster> WalletQueryApi for ExactPairWalletQuery<Broadcaster>
where
    Broadcaster: TransactionBroadcaster + Clone,
{
    async fn network_upgrade_activations(&self) -> Result<NetworkUpgradeActivations, QueryError> {
        let _pair = self.capture_pair();
        Ok((*self.network_upgrade_activations).clone())
    }

    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestBlock, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        Ok(LatestBlock {
            height: chain_epoch.visible_tip_height,
            block_hash: chain_epoch.visible_tip_hash,
            chain_epoch,
        })
    }

    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestSafeBlock, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        Ok(LatestSafeBlock {
            height: chain_epoch.settled_tip_height,
            block_hash: chain_epoch.settled_tip_hash,
            chain_epoch,
        })
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockIdResponseValue, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        Self::resolve_block_id_by_selector(&pair, selector, chain_epoch)
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderResponseValue, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let resolved = Self::resolve_block_id_by_selector(&pair, selector, chain_epoch)?;
        let block_header = pair
            .canonical()
            .block_header_at(resolved.block_id.height)?
            .ok_or(QueryError::BlockNotInBestChain)?
            .into_header_info();
        Ok(BlockHeaderResponseValue {
            chain_epoch: resolved.chain_epoch,
            block_header,
        })
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlock, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let compact_block = pair
            .canonical()
            .compact_block_at(height)?
            .ok_or_else(|| artifact_unavailable(ArtifactFamily::CompactBlock, height))?;
        Ok(CompactBlock {
            chain_epoch,
            compact_block,
        })
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: zinder_core::BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockRange, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        if block_range.start > block_range.end {
            return Err(QueryError::InvalidBlockRange {
                start_height: block_range.start,
                end_height: block_range.end,
            });
        }
        let compact_blocks = pair.canonical().compact_blocks_in_range(block_range)?;
        Ok(CompactBlockRange {
            chain_epoch,
            block_range,
            compact_blocks,
        })
    }

    async fn full_block_at(
        &self,
        height: BlockHeight,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlock, QueryError> {
        let _pair = self.capture_pair();
        Err(artifact_unavailable(ArtifactFamily::BlockBlob, height))
    }

    async fn full_blocks_in_range(
        &self,
        block_range: zinder_core::BlockHeightRange,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<FullBlockStream, QueryError> {
        let _pair = self.capture_pair();
        Err(artifact_unavailable(
            ArtifactFamily::BlockBlob,
            block_range.start,
        ))
    }

    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransactionStatus, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        // READY admission requires the construction manifest's transaction-location
        // and transaction-blob row counts to equal the authenticated source count;
        // live append and replacement update both families in the canonical atomic
        // batch. Under that admitted coverage contract, absence is a real miss.
        let Some(location) = pair.canonical().transaction_location(transaction_id)? else {
            return Ok(TransactionStatus {
                chain_epoch,
                status: TxStatus::NotFound,
            });
        };
        let header = pair
            .canonical()
            .block_header_at(location.block_height)?
            .ok_or(QueryError::BlockNotInBestChain)?;
        let raw_transaction_bytes = pair
            .canonical()
            .transaction_blob(location)?
            .map(|blob| blob.raw_transaction_bytes);
        let details = MinedDetails::from_response_epoch(
            &chain_epoch,
            location.block_height,
            self.network_upgrade_activations
                .consensus_branch_id_at(location.block_height),
            header.block_time,
        );
        Ok(TransactionStatus {
            chain_epoch,
            status: TxStatus::Mined(MinedTransaction::new(
                location,
                details,
                raw_transaction_bytes,
            )),
        })
    }

    async fn transaction_at_block_index(
        &self,
        height: BlockHeight,
        _tx_index: u64,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Transaction, QueryError> {
        let _pair = self.capture_pair();
        Err(artifact_unavailable(
            ArtifactFamily::TransactionLocation,
            height,
        ))
    }

    async fn raw_transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<RawTransaction, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let location = pair
            .canonical()
            .transaction_location(transaction_id)?
            .ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::TransactionLocation,
                key: ArtifactKey::TransactionId(transaction_id),
            })?;
        let transaction = pair
            .canonical()
            .transaction_blob(location)?
            .ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::TransactionBlob,
                key: ArtifactKey::TransactionId(transaction_id),
            })?;
        Ok(RawTransaction {
            chain_epoch,
            transaction,
        })
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        _outpoints: Vec<TransparentOutPoint>,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::DeriveUnavailable {
            capability: "version-1 transparent output lookup",
        })
    }

    async fn transparent_spends_by_outpoint(
        &self,
        _outpoints: Vec<TransparentOutPoint>,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::DeriveUnavailable {
            capability: "version-1 transparent spend lookup",
        })
    }

    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        _outpoints: Vec<TransparentOutPoint>,
        _at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUnspentOutputsByOutpointResponse, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::DeriveUnavailable {
            capability: "version-1 transparent outpoint lookup",
        })
    }

    async fn transparent_address_unspent_outputs(
        &self,
        request: TransparentAddressUnspentOutputsRequest,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressUnspentOutputs, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let mut outputs = Vec::new();
        let mut after: Option<WalletAddressUnspentOutputKey> = None;
        loop {
            let page = pair.wallet().address_unspent_outputs_page(
                request.address_script_hash,
                after,
                WALLET_READ_PAGE_SIZE,
            )?;
            outputs.extend(page.outputs.into_iter().filter_map(|output| {
                (output.created_at.block.height >= request.start_height).then(|| {
                    TransparentUnspentOutput::new(
                        output.address_script_hash,
                        output.script_pub_key,
                        output.outpoint,
                        output.value_zat,
                        output.created_at.block.height,
                        output.created_at.block.hash,
                    )
                })
            }));
            let Some(next) = page.next_page_after else {
                break;
            };
            after = Some(next);
        }
        Ok(TransparentAddressUnspentOutputs {
            chain_epoch,
            outputs,
        })
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
    ) -> Result<TransparentAddressTxIds, QueryError> {
        let pair = self.capture_pair();
        if request.descending {
            return Err(QueryError::DeriveUnavailable {
                capability: "descending version-1 transparent history",
            });
        }
        let chain_epoch = Self::chain_epoch(&pair, None)?;
        let after = request
            .from_cursor
            .as_ref()
            .map(|cursor| WalletAddressTransactionKey::decode(cursor.as_bytes()))
            .transpose()
            .map_err(|_| QueryError::TransparentHistoryCursorInvalid {
                reason: "cursor is not a version-1 wallet history key",
            })?;
        let page_size =
            NonZeroU16::new(u16::try_from(request.max_entries.get()).unwrap_or(u16::MAX))
                .unwrap_or(NonZeroU16::MAX);
        let page = pair.wallet().address_transaction_history_page(
            request.address_script_hash,
            after,
            page_size,
        )?;
        let artifacts = page
            .transactions
            .into_iter()
            .filter(|row| {
                let height = row.key.block_height();
                height >= request.start_height && height <= request.end_height
            })
            .map(|row| {
                TransparentAddressTxIndexArtifact::new(
                    request.address_script_hash,
                    row.key.block_height(),
                    row.key.tx_index_in_block(),
                    row.transaction_id,
                    row.block_hash,
                )
            })
            .collect();
        Ok(TransparentAddressTxIds {
            chain_epoch,
            artifacts,
            next_cursor: page
                .next_page_after
                .map(|key| StreamCursorTokenV1::from_bytes(key.as_bytes().to_vec())),
        })
    }

    async fn transparent_address_balance(
        &self,
        addresses: Vec<TransparentAddressScriptHash>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressBalance, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        if addresses.is_empty() {
            return Err(QueryError::TransparentBalanceAddressCountExceeded {
                requested: 0,
                maximum: crate::MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES,
            });
        }
        let addresses: HashSet<_> = addresses.into_iter().collect();
        let mut confirmed_zat = 0_u64;
        for address in &addresses {
            confirmed_zat = confirmed_zat.saturating_add(pair.wallet().address_balance(*address)?);
        }
        Ok(TransparentAddressBalance {
            confirmed_zat,
            unconfirmed_delta_zat: 0,
            address_count: u32::try_from(addresses.len()).unwrap_or(u32::MAX),
            chain_epoch,
        })
    }

    async fn transparent_utxo_set_summary(
        &self,
        _at_epoch_id: Option<ChainEpochId>,
        _commitment_enabled: bool,
    ) -> Result<TransparentUtxoSetSummary, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::DeriveUnavailable {
            capability: "version-1 native UTXO summary encoding",
        })
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let checkpoint = pair
            .canonical()
            .tree_state_checkpoint_at_or_before(height)?;
        if let Some(checkpoint) =
            checkpoint.filter(|checkpoint| checkpoint.block_id.height == height)
        {
            return tree_state_from_checkpoint(chain_epoch, &checkpoint);
        }
        let block_id = Self::block_id_at(&pair, height)?;
        let upstream = self
            .tree_state_upstream
            .as_ref()
            .ok_or_else(|| artifact_unavailable(ArtifactFamily::TreeState, height))?;
        let source = upstream.fetch_tree_state_for_block(block_id).await?;
        Ok(TreeState {
            chain_epoch,
            height,
            block_hash: block_id.hash,
            payload_bytes: source.payload_bytes,
        })
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let checkpoint = pair
            .canonical()
            .tree_state_checkpoint_at_or_before(chain_epoch.visible_tip_height)?
            .ok_or_else(|| {
                artifact_unavailable(ArtifactFamily::TreeState, chain_epoch.visible_tip_height)
            })?;
        tree_state_from_checkpoint(chain_epoch, &checkpoint)
    }

    async fn subtree_roots(
        &self,
        subtree_root_range: zinder_core::SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<SubtreeRoots, QueryError> {
        let pair = self.capture_pair();
        let chain_epoch = Self::chain_epoch(&pair, at_epoch_id)?;
        let completed_subtree_count = chain_epoch
            .tip_metadata
            .completed_subtree_count(subtree_root_range.protocol);
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
        let available_entries =
            NonZeroU32::new(available_entries).ok_or_else(|| QueryError::ArtifactUnavailable {
                family: ArtifactFamily::SubtreeRoot,
                key: ArtifactKey::SubtreeRootIndex {
                    protocol: subtree_root_range.protocol,
                    index: subtree_root_range.start_index,
                },
            })?;
        let available_range = zinder_core::SubtreeRootRange::new(
            subtree_root_range.protocol,
            subtree_root_range.start_index,
            available_entries,
        );
        let subtree_roots = pair.canonical().subtree_roots(available_range)?;
        Ok(SubtreeRoots {
            chain_epoch,
            protocol: subtree_root_range.protocol,
            start_index: subtree_root_range.start_index,
            subtree_roots,
        })
    }

    async fn chain_events(
        &self,
        _from_cursor: Option<StreamCursorTokenV1>,
        _family: ChainEventStreamFamily,
    ) -> Result<ChainEvents, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::UnsupportedChainEvent {
            event: "version-1 chain-event serving is not wired",
        })
    }

    async fn resolve_chain_events_start(
        &self,
        _start: EventStreamStartPosition,
        _requested_family: ChainEventStreamFamily,
    ) -> Result<zinder_store::ChainEventStreamResume, QueryError> {
        let _pair = self.capture_pair();
        Err(QueryError::UnsupportedChainEvent {
            event: "version-1 chain-event serving is not wired",
        })
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<zinder_core::TransactionBroadcastResult, QueryError> {
        let _pair = self.capture_pair();
        if raw_transaction.len() > zinder_core::MAX_RAW_TRANSACTION_BYTES {
            return Err(QueryError::BroadcastTransactionTooLarge {
                actual: raw_transaction.len(),
                maximum: zinder_core::MAX_RAW_TRANSACTION_BYTES,
            });
        }
        self.broadcaster
            .broadcast_transaction(raw_transaction)
            .await
            .map_err(QueryError::Node)
    }
}

fn artifact_unavailable(family: ArtifactFamily, height: BlockHeight) -> QueryError {
    QueryError::ArtifactUnavailable {
        family,
        key: ArtifactKey::BlockHeight(height),
    }
}

fn tree_state_from_checkpoint(
    chain_epoch: ChainEpoch,
    checkpoint: &zinder_core::CommitmentTreeCheckpoint,
) -> Result<TreeState, QueryError> {
    let mut payload = Map::new();
    payload.insert("time".to_owned(), json!(checkpoint.block_time_seconds));
    for (name, protocol) in [
        ("sapling", ShieldedProtocol::Sapling),
        ("orchard", ShieldedProtocol::Orchard),
        ("ironwood", ShieldedProtocol::Ironwood),
    ] {
        if let Some(frontier) = checkpoint.frontiers.get(protocol) {
            payload.insert(
                name.to_owned(),
                json!({
                    "commitments": {
                        "finalState": hex::encode(frontier.final_state_bytes())
                    }
                }),
            );
        }
    }
    let payload_bytes = serde_json::to_vec(&Value::Object(payload)).map_err(|source| {
        QueryError::ArtifactCorrupt {
            family: ArtifactFamily::TreeState,
            reason: source.to_string(),
        }
    })?;
    Ok(TreeState {
        chain_epoch,
        height: checkpoint.block_id.height,
        block_hash: checkpoint.block_id.hash,
        payload_bytes,
    })
}
