//! Remote gRPC implementation of the chain-index contract.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_stream::StreamExt as _;
use tonic::{Request, transport::Channel};
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange, TransactionArtifact,
    TransactionBroadcastResult, TransactionId, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_proto::v1::wallet::{self, ServerCapabilities, wallet_query_client::WalletQueryClient};
use zinder_store::ChainEventStreamFamily;

use crate::{
    BlockId, ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope,
    ChainEventStream, ChainIndex, ChainRangeReverted, IndexStream, IndexerError, TxStatus,
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

        chain_epoch_from_message(
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
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(TxStatus::NotFound),
            Err(status) => return Err(IndexerError::from_status(status)),
        };
        let transaction = transaction_from_message(
            response
                .transaction
                .ok_or_else(|| IndexerError::malformed("transaction", "field is missing"))?,
        )?;
        Ok(TxStatus::Mined(transaction))
    }
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

fn chain_epoch_from_message(
    expected_network: Network,
    message: wallet::ChainEpoch,
) -> Result<ChainEpoch, IndexerError> {
    ensure_network_name(expected_network, &message.network_name)?;
    let schema_version = u16::try_from(message.artifact_schema_version).map_err(|_| {
        IndexerError::malformed(
            "chain_epoch.artifact_schema_version",
            "value exceeds u16 range",
        )
    })?;

    Ok(ChainEpoch {
        id: ChainEpochId::new(message.chain_epoch_id),
        network: expected_network,
        tip_height: BlockHeight::new(message.tip_height),
        tip_hash: block_hash_from_bytes("chain_epoch.tip_hash", message.tip_hash)?,
        finalized_height: BlockHeight::new(message.finalized_height),
        finalized_hash: block_hash_from_bytes(
            "chain_epoch.finalized_hash",
            message.finalized_hash,
        )?,
        artifact_schema_version: ArtifactSchemaVersion::new(schema_version),
        tip_metadata: ChainTipMetadata::new(
            message.sapling_commitment_tree_size,
            message.orchard_commitment_tree_size,
        ),
        created_at: UnixTimestampMillis::new(message.created_at_millis),
    })
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
    let chain_epoch = chain_epoch_from_message(
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
        chain_epoch: chain_epoch_from_message(
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
        chain_epoch: chain_epoch_from_message(
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
