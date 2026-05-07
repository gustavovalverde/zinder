//! Remote gRPC implementation of the chain-index contract.

use std::ops::ControlFlow;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_stream::StreamExt as _;
use tonic::{Request, transport::Channel};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, CompactBlockArtifact, MempoolEntry,
    Network, RawTransactionBytes, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash,
    SubtreeRootIndex, SubtreeRootRange, TransactionArtifact, TransactionBroadcastResult,
    TransactionId, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend, TransparentOutPoint, TreeStateArtifact,
};
use zinder_proto::v1::wallet::{self, ServerCapabilities, wallet_query_client::WalletQueryClient};
use zinder_store::{
    self, ChainEventStreamFamily, MempoolDecodeError, chain_epoch_from_message,
    mempool_entry_from_message,
    mempool_event_envelope_from_message as mempool_event_envelope_from_message_shared,
};

use crate::{
    BlockId, ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope,
    ChainEventStream, ChainIndex, ChainRangeReverted, IndexStream, IndexerError, MempoolEvent,
    MempoolEventCursor, MempoolEventEnvelope, MempoolEventStream, MempoolSnapshotCursor,
    MempoolSnapshotRequest, MempoolSnapshotView, TxStatus,
};

/// Options for opening a remote chain index over the native wallet gRPC API.
#[derive(Clone, Debug)]
pub struct RemoteOpenOptions {
    /// Native `WalletQuery` endpoint URI.
    pub endpoint: String,
    /// Expected network served by the endpoint.
    pub network: Network,
}

/// Remote chain index backed by the native wallet gRPC API.
#[derive(Clone)]
pub struct RemoteChainIndex {
    client: Arc<Mutex<WalletQueryClient<Channel>>>,
    network: Network,
}

impl RemoteChainIndex {
    /// Connects to a remote `WalletQuery` endpoint.
    pub async fn connect(options: RemoteOpenOptions) -> Result<Self, IndexerError> {
        let channel = Channel::from_shared(options.endpoint)
            .map_err(|error| IndexerError::invalid_request(error.to_string()))?
            .connect()
            .await
            .map_err(IndexerError::from_transport_error)?;

        Ok(Self {
            client: Arc::new(Mutex::new(WalletQueryClient::new(channel))),
            network: options.network,
        })
    }

    async fn client(&self) -> tokio::sync::MutexGuard<'_, WalletQueryClient<Channel>> {
        self.client.lock().await
    }
}

#[async_trait]
impl ChainIndex for RemoteChainIndex {
    async fn server_info(&self) -> Result<ServerCapabilities, IndexerError> {
        let response = self
            .client()
            .await
            .server_info(Request::new(wallet::ServerInfoRequest {}))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let capabilities = response
            .capabilities
            .ok_or_else(|| IndexerError::malformed("capabilities", "field is missing"))?;
        ensure_network_name(self.network, &capabilities.network)?;
        Ok(capabilities)
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        let response = self
            .client()
            .await
            .latest_block(Request::new(wallet::LatestBlockRequest { at_epoch: None }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();

        chain_epoch_from_message_with_network(
            self.network,
            response
                .chain_epoch
                .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
        )
    }

    async fn latest_block(&self) -> Result<BlockId, IndexerError> {
        self.latest_block_from_epoch(None).await
    }

    async fn latest_block_at_epoch(&self, at_epoch: ChainEpoch) -> Result<BlockId, IndexerError> {
        self.latest_block_from_epoch(Some(at_epoch)).await
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.compact_block_from_epoch(height, None).await
    }

    async fn compact_block_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        self.compact_block_from_epoch(height, Some(at_epoch)).await
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
        self.tree_state_from_epoch(height, None).await
    }

    async fn tree_state_at_epoch(
        &self,
        height: BlockHeight,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.tree_state_from_epoch(height, Some(at_epoch)).await
    }

    async fn latest_tree_state(&self) -> Result<TreeStateArtifact, IndexerError> {
        self.latest_tree_state_from_epoch(None).await
    }

    async fn latest_tree_state_at_epoch(
        &self,
        at_epoch: ChainEpoch,
    ) -> Result<TreeStateArtifact, IndexerError> {
        self.latest_tree_state_from_epoch(Some(at_epoch)).await
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
        let response = self
            .client()
            .await
            .broadcast_transaction(Request::new(wallet::BroadcastTransactionRequest {
                raw_transaction: raw_transaction.as_slice().to_vec(),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        transaction_broadcast_result_from_message(response)
    }

    async fn chain_events_for_family(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEventStream, IndexerError> {
        let response = self
            .client()
            .await
            .chain_events(Request::new(wallet::ChainEventsRequest {
                from_cursor: from_cursor.map_or_else(Vec::new, |cursor| cursor.as_bytes().to_vec()),
                family: chain_event_stream_family_to_message(family) as i32,
            }))
            .await
            .map_err(IndexerError::from_status)?;
        let expected_network = self.network;
        let stream = response.into_inner().map(move |event_result| {
            let event = event_result.map_err(IndexerError::from_status)?;
            chain_event_envelope_from_message(expected_network, event)
        });

        Ok(Box::pin(stream))
    }

    async fn mempool_snapshot(
        &self,
        request: MempoolSnapshotRequest,
    ) -> Result<MempoolSnapshotView, IndexerError> {
        let from_cursor = request
            .from_cursor
            .map(|cursor| cursor.as_bytes().to_vec())
            .unwrap_or_default();
        let response = self
            .client()
            .await
            .mempool_snapshot(Request::new(wallet::MempoolSnapshotRequest {
                max_entries: request.max_entries,
                from_cursor,
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        mempool_snapshot_view_from_message(response)
    }

    async fn mempool_events(
        &self,
        from_cursor: Option<MempoolEventCursor>,
    ) -> Result<MempoolEventStream, IndexerError> {
        let response = self
            .client()
            .await
            .mempool_events(Request::new(wallet::MempoolEventsRequest {
                from_cursor: from_cursor.map_or_else(Vec::new, |cursor| cursor.as_bytes().to_vec()),
                family: wallet::MempoolEventStreamFamily::Mempool as i32,
            }))
            .await
            .map_err(IndexerError::from_status)?;
        let stream = response.into_inner().map(move |event_result| {
            let envelope_message = event_result.map_err(IndexerError::from_status)?;
            mempool_event_envelope_from_message(envelope_message)
        });
        Ok(Box::pin(stream))
    }

    async fn is_in_mempool(&self, transaction_id: TransactionId) -> Result<bool, IndexerError> {
        // Defers to `transaction_by_id`, which the canonical chain consults
        // first and falls through to the mempool when no mined record
        // exists. This avoids a second round-trip and reuses the writer's
        // typed `TxStatus::InMempool` answer.
        let outcome = self.transaction_by_id(transaction_id).await?;
        Ok(matches!(outcome, TxStatus::InMempool(_)))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: TransparentMempoolOutputsRequest,
    ) -> Result<Vec<TransparentMempoolOutput>, IndexerError> {
        let max_outputs = u32_to_usize(request.max_entries);
        let mut outputs: Vec<TransparentMempoolOutput> = Vec::new();
        self.for_each_mempool_entry(|entry| {
            for output in &entry.transparent_outputs {
                if output.address_script_hash == request.address_script_hash {
                    outputs.push(output.clone());
                    if outputs.len() >= max_outputs {
                        return ControlFlow::Break(());
                    }
                }
            }
            ControlFlow::Continue(())
        })
        .await?;
        Ok(outputs)
    }

    async fn transparent_mempool_spend_by_outpoint(
        &self,
        outpoint: TransparentOutPoint,
    ) -> Result<Option<TransparentMempoolSpend>, IndexerError> {
        let mut found_spend: Option<TransparentMempoolSpend> = None;
        self.for_each_mempool_entry(|entry| {
            for spend in &entry.transparent_spends {
                if spend.spent_outpoint == outpoint {
                    found_spend = Some(*spend);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        })
        .await?;
        Ok(found_spend)
    }
}

impl RemoteChainIndex {
    async fn latest_block_from_epoch(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
            .await
            .latest_block(Request::new(wallet::LatestBlockRequest {
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let latest_block = response
            .latest_block
            .ok_or_else(|| IndexerError::malformed("latest_block", "field is missing"))?;

        Ok(BlockId {
            height: BlockHeight::new(latest_block.height),
            hash: block_hash_from_bytes("latest_block.block_hash", latest_block.block_hash)?,
        })
    }

    async fn compact_block_from_epoch(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        let response = self
            .client()
            .await
            .compact_block(Request::new(wallet::CompactBlockRequest {
                height: height.value(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        compact_block_from_message(
            response
                .compact_block
                .ok_or_else(|| IndexerError::malformed("compact_block", "field is missing"))?,
        )
    }

    async fn compact_blocks_in_range_from_epoch(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        let response = self
            .client()
            .await
            .compact_block_range(Request::new(wallet::CompactBlockRangeRequest {
                start_height: block_range.start.value(),
                end_height: block_range.end.value(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?;
        let stream = response.into_inner().map(|chunk_result| {
            let chunk = chunk_result.map_err(IndexerError::from_status)?;
            compact_block_from_message(
                chunk
                    .compact_block
                    .ok_or_else(|| IndexerError::malformed("compact_block", "field is missing"))?,
            )
        });

        Ok(Box::pin(stream))
    }

    async fn tree_state_from_epoch(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .await
            .tree_state(Request::new(wallet::TreeStateRequest {
                height: height.value(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn latest_tree_state_from_epoch(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .await
            .latest_tree_state(Request::new(wallet::LatestTreeStateRequest {
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn subtree_roots_in_range_from_epoch(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        let response = self
            .client()
            .await
            .subtree_roots(Request::new(wallet::SubtreeRootsRequest {
                shielded_protocol: shielded_protocol_to_message(subtree_root_range.protocol)?
                    as i32,
                start_index: subtree_root_range.start_index.value(),
                max_entries: subtree_root_range.max_entries.get(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let protocol = shielded_protocol_from_message(response.shielded_protocol)?;
        response
            .subtree_roots
            .into_iter()
            .map(|root| subtree_root_from_message(protocol, root))
            .collect()
    }

    async fn transaction_by_id_from_epoch(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError> {
        let response = match self
            .client()
            .await
            .transaction(Request::new(wallet::TransactionRequest {
                transaction_id: transaction_id.as_bytes().to_vec(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => {
                if at_epoch.is_some() {
                    // Caller bound the read to a chain epoch. Mempool
                    // state is not part of any chain epoch; return the
                    // canonical NotFound answer without a mempool
                    // round-trip.
                    return Ok(TxStatus::NotFound);
                }
                return self.lookup_in_mempool(transaction_id).await;
            }
            Err(status) => return Err(IndexerError::from_status(status)),
        };
        let transaction = transaction_from_message(
            response
                .transaction
                .ok_or_else(|| IndexerError::malformed("transaction", "field is missing"))?,
        )?;
        Ok(TxStatus::Mined(transaction))
    }

    async fn lookup_in_mempool(
        &self,
        transaction_id: TransactionId,
    ) -> Result<TxStatus, IndexerError> {
        let mut found_entry: Option<MempoolEntry> = None;
        self.for_each_mempool_entry(|entry| {
            if entry.transaction_id == transaction_id {
                found_entry = Some(entry);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await?;
        Ok(found_entry.map_or(TxStatus::NotFound, TxStatus::InMempool))
    }

    /// Walks the live mempool one server-bounded page at a time, applying
    /// `visitor` to every entry until the visitor returns
    /// [`ControlFlow::Break`] or the writer returns no `next_cursor`.
    ///
    /// Each page is bounded by the server's `MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE`
    /// (1024 entries today). For typical mempool sizes this is one
    /// round-trip; for soak workloads it remains O(mempool-size). A
    /// dedicated point-lookup gRPC primitive is filed as
    /// [`docs/architecture/wallet-data-plane.md`] follow-up.
    async fn for_each_mempool_entry<Visitor>(
        &self,
        mut visitor: Visitor,
    ) -> Result<(), IndexerError>
    where
        Visitor: FnMut(MempoolEntry) -> ControlFlow<()>,
    {
        let mut from_cursor: Option<MempoolSnapshotCursor> = None;
        loop {
            let snapshot = self
                .mempool_snapshot(MempoolSnapshotRequest {
                    // 0 asks the writer for its default page size; the
                    // writer caps at MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE.
                    max_entries: 0,
                    from_cursor,
                })
                .await?;
            for entry in snapshot.entries {
                if visitor(entry) == ControlFlow::Break(()) {
                    return Ok(());
                }
            }
            match snapshot.next_cursor {
                Some(cursor) => from_cursor = Some(cursor),
                None => return Ok(()),
            }
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}

fn ensure_network_name(expected: Network, actual_name: &str) -> Result<(), IndexerError> {
    let Some(actual) = Network::from_name(actual_name) else {
        return Err(IndexerError::NetworkMismatch {
            expected,
            actual: actual_name.to_owned(),
        });
    };
    if actual != expected {
        return Err(IndexerError::NetworkMismatch {
            expected,
            actual: actual_name.to_owned(),
        });
    }
    Ok(())
}

fn chain_epoch_from_message_with_network(
    expected_network: Network,
    message: wallet::ChainEpoch,
) -> Result<ChainEpoch, IndexerError> {
    ensure_network_name(expected_network, &message.network_name)?;
    chain_epoch_from_message(message).map_err(decode_error_to_indexer_error)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Used as a Result::map_err callback so the value-passing signature is required."
)]
fn decode_error_to_indexer_error(error: MempoolDecodeError) -> IndexerError {
    IndexerError::malformed(error.field(), error.to_string())
}

fn chain_epoch_to_message(chain_epoch: ChainEpoch) -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: chain_epoch.id.value(),
        network_name: chain_epoch.network.name().to_owned(),
        tip_height: chain_epoch.tip_height.value(),
        tip_hash: chain_epoch.tip_hash.as_bytes().to_vec(),
        finalized_height: chain_epoch.finalized_height.value(),
        finalized_hash: chain_epoch.finalized_hash.as_bytes().to_vec(),
        artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
        created_at_millis: chain_epoch.created_at.value(),
        sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
        orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
    }
}

fn compact_block_from_message(
    message: wallet::CompactBlock,
) -> Result<CompactBlockArtifact, IndexerError> {
    Ok(CompactBlockArtifact::new(
        BlockHeight::new(message.height),
        block_hash_from_bytes("compact_block.block_hash", message.block_hash)?,
        message.payload_bytes,
    ))
}

fn tree_state_from_response(
    response: wallet::TreeStateResponse,
) -> Result<TreeStateArtifact, IndexerError> {
    Ok(TreeStateArtifact::new(
        BlockHeight::new(response.height),
        block_hash_from_bytes("tree_state.block_hash", response.block_hash)?,
        response.payload_bytes,
    ))
}

fn subtree_root_from_message(
    protocol: ShieldedProtocol,
    message: wallet::SubtreeRoot,
) -> Result<SubtreeRootArtifact, IndexerError> {
    Ok(SubtreeRootArtifact::new(
        protocol,
        SubtreeRootIndex::new(message.subtree_index),
        subtree_root_hash_from_bytes("subtree_root.root_hash", message.root_hash)?,
        BlockHeight::new(message.completing_block_height),
        block_hash_from_bytes(
            "subtree_root.completing_block_hash",
            message.completing_block_hash,
        )?,
    ))
}

fn transaction_from_message(
    message: wallet::Transaction,
) -> Result<TransactionArtifact, IndexerError> {
    Ok(TransactionArtifact::new(
        transaction_id_from_bytes("transaction.transaction_id", message.transaction_id)?,
        BlockHeight::new(message.block_height),
        block_hash_from_bytes("transaction.block_hash", message.block_hash)?,
        message.payload_bytes,
    ))
}

fn transaction_broadcast_result_from_message(
    response: wallet::BroadcastTransactionResponse,
) -> Result<TransactionBroadcastResult, IndexerError> {
    use wallet::broadcast_transaction_response::Outcome;
    use zinder_core::{
        BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected,
        BroadcastUnknown,
    };

    let outcome = response
        .outcome
        .ok_or_else(|| IndexerError::malformed("outcome", "field is missing"))?;
    match outcome {
        Outcome::Accepted(accepted) => {
            Ok(TransactionBroadcastResult::Accepted(BroadcastAccepted {
                transaction_id: transaction_id_from_bytes(
                    "accepted.transaction_id",
                    accepted.transaction_id,
                )?,
            }))
        }
        Outcome::Duplicate(duplicate) => {
            Ok(TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
                error_code: duplicate.error_code,
                message: duplicate.message,
            }))
        }
        Outcome::InvalidEncoding(invalid_encoding) => Ok(
            TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
                error_code: invalid_encoding.error_code,
                message: invalid_encoding.message,
            }),
        ),
        Outcome::Rejected(rejected) => {
            Ok(TransactionBroadcastResult::Rejected(BroadcastRejected {
                error_code: rejected.error_code,
                message: rejected.message,
            }))
        }
        Outcome::Unknown(unknown) => Ok(TransactionBroadcastResult::Unknown(BroadcastUnknown {
            error_code: unknown.error_code,
            message: unknown.message,
        })),
    }
}

fn chain_event_envelope_from_message(
    expected_network: Network,
    message: wallet::ChainEventEnvelope,
) -> Result<ChainEventEnvelope, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    let event = match message
        .event
        .ok_or_else(|| IndexerError::malformed("event", "field is missing"))?
    {
        wallet::chain_event_envelope::Event::Committed(committed) => ChainEvent::ChainCommitted {
            committed: chain_epoch_committed_from_message(
                expected_network,
                committed.committed.ok_or_else(|| {
                    IndexerError::malformed("committed.committed", "field is missing")
                })?,
            )?,
        },
        wallet::chain_event_envelope::Event::Reorged(reorged) => ChainEvent::ChainReorged {
            reverted: chain_range_reverted_from_message(
                expected_network,
                reorged.reverted.ok_or_else(|| {
                    IndexerError::malformed("reorged.reverted", "field is missing")
                })?,
            )?,
            committed: chain_epoch_committed_from_message(
                expected_network,
                reorged.committed.ok_or_else(|| {
                    IndexerError::malformed("reorged.committed", "field is missing")
                })?,
            )?,
        },
    };

    Ok(ChainEventEnvelope {
        cursor: ChainEventCursor::from_bytes(message.cursor),
        event_sequence: message.event_sequence,
        chain_epoch,
        finalized_height: BlockHeight::new(message.finalized_height),
        event,
    })
}

fn chain_epoch_committed_from_message(
    expected_network: Network,
    message: wallet::ChainEpochCommitted,
) -> Result<ChainEpochCommitted, IndexerError> {
    Ok(ChainEpochCommitted {
        chain_epoch: chain_epoch_from_message_with_network(
            expected_network,
            message
                .chain_epoch
                .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
        )?,
        block_range: BlockHeightRange::inclusive(
            BlockHeight::new(message.start_height),
            BlockHeight::new(message.end_height),
        ),
    })
}

fn chain_range_reverted_from_message(
    expected_network: Network,
    message: wallet::ChainRangeReverted,
) -> Result<ChainRangeReverted, IndexerError> {
    Ok(ChainRangeReverted {
        chain_epoch: chain_epoch_from_message_with_network(
            expected_network,
            message
                .chain_epoch
                .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
        )?,
        block_range: BlockHeightRange::inclusive(
            BlockHeight::new(message.start_height),
            BlockHeight::new(message.end_height),
        ),
    })
}

fn shielded_protocol_to_message(
    protocol: ShieldedProtocol,
) -> Result<wallet::ShieldedProtocol, IndexerError> {
    match protocol {
        ShieldedProtocol::Sapling => Ok(wallet::ShieldedProtocol::Sapling),
        ShieldedProtocol::Orchard => Ok(wallet::ShieldedProtocol::Orchard),
        _ => Err(IndexerError::invalid_request(
            "shielded protocol is unsupported by the native wallet protocol",
        )),
    }
}

fn shielded_protocol_from_message(protocol: i32) -> Result<ShieldedProtocol, IndexerError> {
    match wallet::ShieldedProtocol::try_from(protocol) {
        Ok(wallet::ShieldedProtocol::Sapling) => Ok(ShieldedProtocol::Sapling),
        Ok(wallet::ShieldedProtocol::Orchard) => Ok(ShieldedProtocol::Orchard),
        Ok(wallet::ShieldedProtocol::Unspecified) => Err(IndexerError::malformed(
            "shielded_protocol",
            "protocol is unspecified",
        )),
        Err(_) => Err(IndexerError::malformed(
            "shielded_protocol",
            "protocol is unknown",
        )),
    }
}

fn chain_event_stream_family_to_message(
    family: ChainEventStreamFamily,
) -> wallet::ChainEventStreamFamily {
    match family {
        ChainEventStreamFamily::Tip => wallet::ChainEventStreamFamily::Tip,
        ChainEventStreamFamily::Finalized => wallet::ChainEventStreamFamily::Finalized,
    }
}

fn block_hash_from_bytes(field: &'static str, bytes: Vec<u8>) -> Result<BlockHash, IndexerError> {
    let bytes = fixed_32_bytes(field, bytes)?;
    Ok(BlockHash::from_bytes(bytes))
}

fn subtree_root_hash_from_bytes(
    field: &'static str,
    bytes: Vec<u8>,
) -> Result<SubtreeRootHash, IndexerError> {
    let bytes = fixed_32_bytes(field, bytes)?;
    Ok(SubtreeRootHash::from_bytes(bytes))
}

fn transaction_id_from_bytes(
    field: &'static str,
    bytes: Vec<u8>,
) -> Result<TransactionId, IndexerError> {
    let bytes = fixed_32_bytes(field, bytes)?;
    Ok(TransactionId::from_bytes(bytes))
}

fn fixed_32_bytes(field: &'static str, bytes: Vec<u8>) -> Result<[u8; 32], IndexerError> {
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| IndexerError::malformed(field, format!("expected 32 bytes, got {len} bytes")))
}

fn mempool_snapshot_view_from_message(
    message: wallet::MempoolSnapshotResponse,
) -> Result<MempoolSnapshotView, IndexerError> {
    let chain_epoch_message = message
        .chain_epoch
        .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?;
    let chain_epoch =
        chain_epoch_from_message(chain_epoch_message).map_err(decode_error_to_indexer_error)?;
    let entries = message
        .entries
        .into_iter()
        .map(|entry| mempool_entry_from_message(entry).map_err(decode_error_to_indexer_error))
        .collect::<Result<Vec<MempoolEntry>, IndexerError>>()?;
    let next_cursor = if message.next_cursor.is_empty() {
        None
    } else {
        Some(MempoolSnapshotCursor::from_bytes(message.next_cursor))
    };
    Ok(MempoolSnapshotView {
        chain_epoch,
        snapshot_sequence: message.snapshot_sequence,
        snapshot_age_millis: message.snapshot_age_millis,
        entries,
        next_cursor,
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "zinder_store::MempoolEvent is non_exhaustive; the client mirrors its known variants and fails closed for any future variant."
)]
fn mempool_event_envelope_from_message(
    message: wallet::MempoolEventEnvelope,
) -> Result<MempoolEventEnvelope, IndexerError> {
    let store_envelope = mempool_event_envelope_from_message_shared(message)
        .map_err(decode_error_to_indexer_error)?;
    let event = match store_envelope.event {
        zinder_store::MempoolEvent::Added { entry } => MempoolEvent::Added { entry },
        zinder_store::MempoolEvent::Invalidated {
            transaction_id,
            reason,
        } => MempoolEvent::Invalidated {
            transaction_id,
            reason,
        },
        zinder_store::MempoolEvent::Mined {
            transaction_id,
            mined_height,
        } => MempoolEvent::Mined {
            transaction_id,
            mined_height,
        },
        _ => {
            return Err(IndexerError::malformed(
                "mempool_event",
                "store yielded a variant unknown to the client",
            ));
        }
    };
    Ok(MempoolEventEnvelope {
        cursor: MempoolEventCursor::from_bytes(store_envelope.cursor.as_bytes().to_vec()),
        event_sequence: store_envelope.event_sequence,
        source_observed_unix_millis: store_envelope.source_observed_unix_millis,
        event,
    })
}
