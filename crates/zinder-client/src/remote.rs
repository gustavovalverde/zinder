//! Remote gRPC implementation of the chain-index contract.

use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::time::Duration;

use async_trait::async_trait;
use tokio_stream::StreamExt as _;
use tonic::{Request, transport::Channel};
use zinder_core::wire::{
    decode_zinder_native_chain_name, encode_internal_block_hash, encode_internal_transaction_id,
    encode_zinder_native_chain_name,
};
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockSelector,
    ChainEpoch, ChainValuePool, ChainValuePoolsAtTip, CompactBlockArtifact, ConsensusBranchId,
    MempoolEntry, MinedDetails, MinedTransaction, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange, TransactionArtifact,
    TransactionBroadcastResult, TransactionId, TransparentAddressBalance,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentAddressUtxoArtifact, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend, TransparentOutPoint, TransparentPrevoutsResponse, TreeStateArtifact,
    TxStatus, UnixTimestampMillis,
};
use zinder_proto::v1::wallet::{self, WalletServerInfo, wallet_query_client::WalletQueryClient};
use zinder_store::{
    self, ChainEventStreamFamily, MempoolDecodeError, chain_epoch_from_message,
    mempool_entry_from_message,
    mempool_event_envelope_from_message as mempool_event_envelope_from_message_shared,
    outpoint_message,
    transparent_mempool_output_from_message as transparent_mempool_output_from_message_shared,
    transparent_mempool_spend_from_message as transparent_mempool_spend_from_message_shared,
};

use crate::{
    BlockId, ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope,
    ChainEventStream, ChainIndex, ChainRangeReverted, IndexStream, IndexerError, MempoolEvent,
    MempoolEventCursor, MempoolEventEnvelope, MempoolEventStream, MempoolSnapshotCursor,
    MempoolSnapshotRequest, MempoolSnapshotView, TransparentAddressTxIdsQuery,
    TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem, TransparentAddressUtxoStream,
    TransparentAddressUtxoStreamItem, TransparentAddressUtxosQuery, TransparentAddressUtxosView,
    TransparentHistoryCursor, TransparentUtxoCursor,
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
///
/// `RemoteChainIndex` is the recommended baseline for the Zallet-with-Zinder
/// operator recipe documented in
/// [Service operations §Zallet with Zinder](../../../docs/architecture/service-operations.md#zallet-with-zinder).
/// `LocalChainIndex` is the colocated optimization for advanced operators.
///
/// The underlying tonic `Channel` is configured with HTTP/2 keepalive and a
/// lazy connect: the connection is established on the first call and
/// re-established automatically after a transport failure. A half-open
/// connection is detected by the keepalive PING within
/// `KEEP_ALIVE_INTERVAL + KEEP_ALIVE_TIMEOUT`, so a stalled call errors out
/// instead of hanging forever. The channel multiplexes concurrent calls over
/// one HTTP/2 connection; the type is itself [`Clone`], and each clone
/// shares the same underlying connection.
#[derive(Clone)]
pub struct RemoteChainIndex {
    client: WalletQueryClient<Channel>,
    network: Network,
}

/// Cadence at which HTTP/2 PING frames are sent over the channel; with
/// [`KEEP_ALIVE_TIMEOUT`], this is what makes a half-open connection
/// detectable instead of hanging in-flight calls forever.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Time the channel waits for a PONG to a keepalive PING before deciding the
/// connection is dead and tearing it down so the next call re-dials.
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the initial TCP+HTTP/2 handshake when the channel
/// establishes (or re-establishes) the connection lazily.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// OS-level TCP keepalive idle interval. Belt-and-braces alongside the
/// application-level HTTP/2 PING: detects connections dropped silently by
/// intermediaries that don't surface the failure to userspace.
const TCP_KEEPALIVE: Duration = Duration::from_mins(1);

impl RemoteChainIndex {
    /// Builds a remote-chain-index handle pointed at a `WalletQuery` endpoint.
    ///
    /// The channel is constructed lazily: the TCP+HTTP/2 connection is
    /// established on the first gRPC call, not here. Only URI parsing can
    /// fail at this stage; transport errors surface at first use.
    pub fn connect(options: RemoteOpenOptions) -> Result<Self, IndexerError> {
        let channel = Channel::from_shared(options.endpoint)
            .map_err(|error| IndexerError::invalid_request(error.to_string()))?
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(Some(TCP_KEEPALIVE))
            .connect_lazy();

        Ok(Self {
            client: WalletQueryClient::new(channel),
            network: options.network,
        })
    }

    /// Returns a per-call `WalletQueryClient` clone. The underlying `Channel`
    /// is shared and multiplexes the request over one HTTP/2 connection;
    /// cloning the client is cheap and does not allocate a new connection.
    fn client(&self) -> WalletQueryClient<Channel> {
        self.client.clone()
    }
}

#[async_trait]
impl ChainIndex for RemoteChainIndex {
    async fn server_info(&self) -> Result<WalletServerInfo, IndexerError> {
        let response = self
            .client()
            .server_info(Request::new(wallet::ServerInfoRequest {}))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let wallet_info = response
            .info
            .ok_or_else(|| IndexerError::malformed("info", "field is missing"))?;
        let common = wallet_info
            .common
            .as_ref()
            .ok_or_else(|| IndexerError::malformed("info.common", "field is missing"))?;
        ensure_network_name(self.network, &common.network)?;
        Ok(wallet_info)
    }

    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        let response = self
            .client()
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

    async fn latest_block(&self, at_epoch: Option<ChainEpoch>) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
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

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
            .block_id_by_selector(Request::new(wallet::BlockIdBySelectorRequest {
                selector: Some(block_selector_to_message(selector)?),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        block_id_from_message(response.block_id)
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockHeaderInfo, IndexerError> {
        let response = self
            .client()
            .block_header_by_selector(Request::new(wallet::BlockIdBySelectorRequest {
                selector: Some(block_selector_to_message(selector)?),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let header_message = response
            .block_header
            .ok_or_else(|| IndexerError::malformed("block_header", "field is missing"))?;
        block_header_info_from_message(header_message)
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        let response = self
            .client()
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

    async fn full_block_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<BlockArtifact, IndexerError> {
        let response = self
            .client()
            .full_block(Request::new(wallet::FullBlockRequest {
                block_height: height.value(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        full_block_from_message(
            response
                .block
                .ok_or_else(|| IndexerError::malformed("full_block", "field is missing"))?,
        )
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        let response = self
            .client()
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

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .tree_state(Request::new(wallet::TreeStateRequest {
                height: height.value(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn latest_tree_state(
        &self,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .latest_tree_state(Request::new(wallet::LatestTreeStateRequest {
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn subtree_roots_in_range(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        let response = self
            .client()
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

    async fn chain_value_pools_at_tip(&self) -> Result<ChainValuePoolsAtTip, IndexerError> {
        let response = self
            .client()
            .chain_value_pools_at_tip(Request::new(wallet::ChainValuePoolsAtTipRequest {}))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        chain_value_pools_at_tip_from_message(self.network, response)
    }

    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TxStatus, IndexerError> {
        let response = match self
            .client()
            .transaction(Request::new(wallet::TransactionRequest {
                transaction_id: encode_internal_transaction_id(transaction_id).to_vec(),
                at_epoch: at_epoch.map(chain_epoch_to_message),
            }))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => {
                if at_epoch.is_some() {
                    return Ok(TxStatus::NotFound);
                }
                return self.lookup_in_mempool(transaction_id).await;
            }
            Err(status) => return Err(IndexerError::from_status(status)),
        };
        tx_status_from_message(response)
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, IndexerError> {
        let response = self
            .client()
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
        self.chain_events_with_filter(from_cursor, family, Vec::new())
            .await
    }

    async fn chain_events_with_filter(
        &self,
        from_cursor: Option<ChainEventCursor>,
        family: ChainEventStreamFamily,
        address_filter: Vec<String>,
    ) -> Result<ChainEventStream, IndexerError> {
        let response = self
            .client()
            .chain_events(Request::new(wallet::ChainEventsRequest {
                from_cursor: from_cursor.map_or_else(Vec::new, |cursor| cursor.as_bytes().to_vec()),
                family: chain_event_stream_family_to_message(family) as i32,
                address_filter,
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
        let outcome = self.transaction_by_id(transaction_id, None).await?;
        Ok(matches!(outcome, TxStatus::InMempool(_)))
    }

    async fn transparent_address_utxos(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxosView, IndexerError> {
        let request = transparent_address_utxos_request_message(&query, at_epoch);
        let response = self
            .client()
            .transparent_address_utxos(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        let chain_epoch = chain_epoch_from_message_with_network(
            self.network,
            response
                .chain_epoch
                .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
        )?;
        let utxos = response
            .utxos
            .into_iter()
            .map(transparent_address_utxo_from_message)
            .collect::<Result<Vec<_>, IndexerError>>()?;
        let next_cursor = if response.next_cursor.is_empty() {
            None
        } else {
            Some(TransparentUtxoCursor::from_bytes(response.next_cursor))
        };
        Ok(TransparentAddressUtxosView {
            chain_epoch,
            utxos,
            next_cursor,
        })
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressTxIdsStream, IndexerError> {
        let address_script_hash = query.address_script_hash;
        let request = wallet::TransparentAddressTxIdsInRangeRequest {
            address: Some(wallet::AddressLookup {
                selector: Some(wallet::address_lookup::Selector::ScriptHash(
                    address_script_hash.as_bytes().to_vec(),
                )),
            }),
            start_height: query.start_height.value(),
            end_height: query.end_height.value(),
            max_entries: query.max_entries.map(NonZeroU32::get).unwrap_or_default(),
            from_cursor: query
                .from_cursor
                .as_ref()
                .map(|cursor| cursor.as_bytes().to_vec())
                .unwrap_or_default(),
            at_epoch: at_epoch.map(chain_epoch_to_message),
            descending: query.descending,
        };
        let response = self
            .client()
            .transparent_address_tx_ids_in_range(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?;
        let expected_network = self.network;
        let stream = response.into_inner().map(move |chunk_result| {
            let chunk = chunk_result.map_err(IndexerError::from_status)?;
            transparent_address_tx_ids_chunk_from_message(
                expected_network,
                address_script_hash,
                chunk,
            )
        });
        Ok(Box::pin(stream))
    }

    async fn transparent_address_utxos_stream(
        &self,
        query: TransparentAddressUtxosQuery,
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressUtxoStream, IndexerError> {
        let request = transparent_address_utxos_request_message(&query, at_epoch);
        let response = self
            .client()
            .transparent_address_utxos_stream(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?;
        let expected_network = self.network;
        let stream = response.into_inner().map(move |chunk_result| {
            let chunk = chunk_result.map_err(IndexerError::from_status)?;
            transparent_address_utxos_stream_item_from_message(expected_network, chunk)
        });
        Ok(Box::pin(stream))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: TransparentMempoolOutputsRequest,
    ) -> Result<Vec<TransparentMempoolOutput>, IndexerError> {
        let wire_request = wallet::TransparentMempoolOutputsByAddressRequest {
            address: Some(wallet::AddressLookup {
                selector: Some(wallet::address_lookup::Selector::ScriptHash(
                    request.address_script_hash.as_bytes().to_vec(),
                )),
            }),
            max_entries: Some(request.max_entries),
        };
        let response = self
            .client()
            .transparent_mempool_outputs_by_address(Request::new(wire_request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        response
            .outputs
            .into_iter()
            .map(transparent_mempool_output_from_message)
            .collect::<Result<Vec<_>, IndexerError>>()
    }

    async fn transparent_mempool_spend_by_outpoint(
        &self,
        outpoint: TransparentOutPoint,
    ) -> Result<Option<TransparentMempoolSpend>, IndexerError> {
        let wire_request = wallet::TransparentMempoolSpendByOutpointRequest {
            outpoint: Some(outpoint_message(&outpoint)),
        };
        let response = self
            .client()
            .transparent_mempool_spend_by_outpoint(Request::new(wire_request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        response
            .spend
            .map(transparent_mempool_spend_from_message)
            .transpose()
    }

    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentAddressBalance, IndexerError> {
        let wire_addresses = addresses
            .iter()
            .map(|script_hash| wallet::AddressLookup {
                selector: Some(wallet::address_lookup::Selector::ScriptHash(
                    script_hash.as_bytes().to_vec(),
                )),
            })
            .collect();
        let request = wallet::TransparentAddressBalanceRequest {
            addresses: wire_addresses,
            at_epoch: at_epoch.map(chain_epoch_to_message),
        };
        let response = self
            .client()
            .transparent_address_balance(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        transparent_address_balance_from_message(self.network, response)
    }

    async fn transparent_prevouts(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch: Option<ChainEpoch>,
    ) -> Result<TransparentPrevoutsResponse, IndexerError> {
        let wire_outpoints = outpoints.iter().map(outpoint_message).collect();
        let request = wallet::TransparentPrevoutsRequest {
            outpoints: wire_outpoints,
            at_epoch: at_epoch.map(chain_epoch_to_message),
        };
        let response = self
            .client()
            .transparent_prevouts(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        transparent_prevouts_response_from_message(self.network, response)
    }

    async fn transparent_mempool_prevouts(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<TransparentPrevoutsResponse, IndexerError> {
        let wire_outpoints = outpoints.iter().map(outpoint_message).collect();
        let request = wallet::TransparentMempoolPrevoutsRequest {
            outpoints: wire_outpoints,
        };
        let response = self
            .client()
            .transparent_mempool_prevouts(Request::new(request))
            .await
            .map_err(IndexerError::from_status)?
            .into_inner();
        transparent_prevouts_response_from_message(self.network, response)
    }
}

fn transparent_prevouts_response_from_message(
    expected_network: Network,
    message: wallet::TransparentPrevoutsResponse,
) -> Result<TransparentPrevoutsResponse, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    let entries = message
        .entries
        .into_iter()
        .map(transparent_prevout_entry_from_message)
        .collect::<Result<Vec<_>, IndexerError>>()?;
    Ok(TransparentPrevoutsResponse {
        chain_epoch,
        entries,
    })
}

fn chain_value_pools_at_tip_from_message(
    expected_network: Network,
    message: wallet::ChainValuePoolsAtTipResponse,
) -> Result<ChainValuePoolsAtTip, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    let pools = message
        .pools
        .into_iter()
        .map(|pool| ChainValuePool::new(pool.id, pool.monitored, pool.chain_value_zat))
        .collect();
    Ok(ChainValuePoolsAtTip {
        chain_epoch,
        tip_height: BlockHeight::new(message.tip_height),
        pools,
    })
}

fn transparent_prevout_entry_from_message(
    message: wallet::TransparentPrevoutEntry,
) -> Result<zinder_core::TransparentPrevoutEntry, IndexerError> {
    let outpoint_message = message.outpoint.ok_or_else(|| {
        IndexerError::malformed("transparent_prevout_entry.outpoint", "field is missing")
    })?;
    let transaction_id = TransactionId::from_bytes(fixed_32_bytes(
        "transparent_prevout_entry.outpoint.transaction_id",
        outpoint_message.transaction_id,
    )?);
    let outpoint = TransparentOutPoint::new(transaction_id, outpoint_message.output_index);
    let prevout = message
        .prevout
        .map(|prevout_message| zinder_core::TransparentPrevout {
            value_zat: prevout_message.value_zat,
            script_pub_key: prevout_message.script_pub_key,
        });
    Ok(zinder_core::TransparentPrevoutEntry { outpoint, prevout })
}

fn transparent_address_balance_from_message(
    expected_network: Network,
    message: wallet::TransparentAddressBalanceResponse,
) -> Result<TransparentAddressBalance, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    Ok(TransparentAddressBalance {
        confirmed_zat: message.confirmed_zat,
        unconfirmed_delta_zat: message.unconfirmed_delta_zat,
        address_count: message.address_count,
        chain_epoch,
    })
}

fn transparent_mempool_output_from_message(
    message: wallet::TransparentMempoolOutput,
) -> Result<TransparentMempoolOutput, IndexerError> {
    transparent_mempool_output_from_message_shared(message).map_err(decode_error_to_indexer_error)
}

fn transparent_mempool_spend_from_message(
    message: wallet::TransparentMempoolSpend,
) -> Result<TransparentMempoolSpend, IndexerError> {
    transparent_mempool_spend_from_message_shared(message).map_err(decode_error_to_indexer_error)
}

impl RemoteChainIndex {
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
    /// round-trip; iterating callers pay O(mempool-size) by design, since
    /// `MempoolSnapshot` is the only public mempool enumeration surface.
    /// Per-txid presence checks should call [`ChainIndex::is_in_mempool`]
    /// rather than walk the snapshot.
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

fn ensure_network_name(expected: Network, actual_name: &str) -> Result<(), IndexerError> {
    let Some(actual) = decode_zinder_native_chain_name(actual_name).ok() else {
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
        network_name: encode_zinder_native_chain_name(chain_epoch.network).to_owned(),
        tip_height: chain_epoch.tip_height.value(),
        tip_hash: encode_internal_block_hash(chain_epoch.tip_hash).to_vec(),
        finalized_height: chain_epoch.finalized_height.value(),
        finalized_hash: encode_internal_block_hash(chain_epoch.finalized_hash).to_vec(),
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

fn full_block_from_message(message: wallet::FullBlock) -> Result<BlockArtifact, IndexerError> {
    Ok(BlockArtifact::new(
        BlockHeight::new(message.block_height),
        block_hash_from_bytes("full_block.block_hash", message.block_hash)?,
        block_hash_from_bytes("full_block.parent_block_hash", message.parent_block_hash)?,
        message.raw_block_bytes,
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

fn transparent_address_utxos_request_message(
    query: &TransparentAddressUtxosQuery,
    at_epoch: Option<ChainEpoch>,
) -> wallet::TransparentAddressUtxosRequest {
    wallet::TransparentAddressUtxosRequest {
        address: Some(wallet::AddressLookup {
            selector: Some(wallet::address_lookup::Selector::ScriptHash(
                query.address_script_hash.as_bytes().to_vec(),
            )),
        }),
        max_entries: query.max_entries.map(NonZeroU32::get),
        from_cursor: query
            .from_cursor
            .as_ref()
            .map(|cursor| cursor.as_bytes().to_vec())
            .unwrap_or_default(),
        at_epoch: at_epoch.map(chain_epoch_to_message),
        start_height: query.start_height.value(),
    }
}

fn transparent_address_tx_ids_chunk_from_message(
    expected_network: Network,
    address_script_hash: TransparentAddressScriptHash,
    message: wallet::TransparentAddressTxIdsChunk,
) -> Result<TransparentAddressTxIdsStreamItem, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    let transaction_id = transaction_id_from_bytes("transaction_id", message.transaction_id)?;
    let block_hash = block_hash_from_bytes("block_hash", message.block_hash)?;
    let artifact = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        BlockHeight::new(message.block_height),
        message.tx_index_in_block,
        transaction_id,
        block_hash,
    );
    let cursor = if message.cursor.is_empty() {
        None
    } else {
        Some(TransparentHistoryCursor::from_bytes(message.cursor))
    };
    Ok(TransparentAddressTxIdsStreamItem {
        chain_epoch,
        artifact,
        cursor,
    })
}

fn transparent_address_utxo_from_message(
    message: wallet::TransparentAddressUtxo,
) -> Result<TransparentAddressUtxoArtifact, IndexerError> {
    let address_script_hash_bytes = fixed_32_bytes(
        "transparent_address_utxo.address_script_hash",
        message.address_script_hash,
    )?;
    let outpoint_message = message.outpoint.ok_or_else(|| {
        IndexerError::malformed("transparent_address_utxo.outpoint", "field is missing")
    })?;
    let transaction_id_bytes = fixed_32_bytes(
        "transparent_address_utxo.outpoint.transaction_id",
        outpoint_message.transaction_id,
    )?;
    let block_hash =
        block_hash_from_bytes("transparent_address_utxo.block_hash", message.block_hash)?;
    Ok(TransparentAddressUtxoArtifact::new(
        TransparentAddressScriptHash::from_bytes(address_script_hash_bytes),
        message.script_pub_key,
        TransparentOutPoint::new(
            TransactionId::from_bytes(transaction_id_bytes),
            outpoint_message.output_index,
        ),
        message.value_zat,
        BlockHeight::new(message.block_height),
        block_hash,
    ))
}

fn transparent_address_utxos_stream_item_from_message(
    expected_network: Network,
    message: wallet::TransparentAddressUtxosStreamChunk,
) -> Result<TransparentAddressUtxoStreamItem, IndexerError> {
    let chain_epoch = chain_epoch_from_message_with_network(
        expected_network,
        message
            .chain_epoch
            .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?,
    )?;
    let utxo = transparent_address_utxo_from_message(
        message
            .utxo
            .ok_or_else(|| IndexerError::malformed("utxo", "field is missing"))?,
    )?;
    let cursor = if message.cursor.is_empty() {
        None
    } else {
        Some(TransparentUtxoCursor::from_bytes(message.cursor))
    };
    Ok(TransparentAddressUtxoStreamItem {
        chain_epoch,
        utxo,
        cursor,
    })
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
        wallet::chain_event_envelope::Event::ChainCommitted(chain_committed) => {
            ChainEvent::ChainCommitted {
                committed: chain_epoch_committed_from_message(
                    expected_network,
                    chain_committed.committed.ok_or_else(|| {
                        IndexerError::malformed("chain_committed.committed", "field is missing")
                    })?,
                )?,
            }
        }
        wallet::chain_event_envelope::Event::ChainReorged(chain_reorged) => {
            ChainEvent::ChainReorged {
                reverted: chain_range_reverted_from_message(
                    expected_network,
                    chain_reorged.reverted.ok_or_else(|| {
                        IndexerError::malformed("chain_reorged.reverted", "field is missing")
                    })?,
                )?,
                committed: chain_epoch_committed_from_message(
                    expected_network,
                    chain_reorged.committed.ok_or_else(|| {
                        IndexerError::malformed("chain_reorged.committed", "field is missing")
                    })?,
                )?,
            }
        }
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

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "BlockSelector is #[non_exhaustive]; new selector variants need wire opt-in before they can be sent"
)]
fn block_selector_to_message(
    selector: BlockSelector,
) -> Result<wallet::BlockSelector, IndexerError> {
    let inner = match selector {
        BlockSelector::Height(height) => wallet::block_selector::Selector::Height(height.value()),
        BlockSelector::Hash(hash) => {
            wallet::block_selector::Selector::Hash(encode_internal_block_hash(hash).to_vec())
        }
        _ => {
            return Err(IndexerError::invalid_request(
                "unsupported block selector variant",
            ));
        }
    };
    Ok(wallet::BlockSelector {
        selector: Some(inner),
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TransactionStatusResponse oneof is non-exhaustive in the protobuf-generated enum; new variants are a deliberate wire change."
)]
fn tx_status_from_message(
    response: wallet::TransactionStatusResponse,
) -> Result<TxStatus, IndexerError> {
    let chain_epoch_message = response
        .chain_epoch
        .as_ref()
        .ok_or_else(|| IndexerError::malformed("chain_epoch", "field is missing"))?;
    let status = response
        .status
        .ok_or_else(|| IndexerError::malformed("status", "field is missing"))?;
    match status {
        wallet::transaction_status_response::Status::Mined(mined) => {
            let artifact = transaction_from_message(mined.transaction.ok_or_else(|| {
                IndexerError::malformed("mined.transaction", "field is missing")
            })?)?;
            let details_message = mined
                .details
                .ok_or_else(|| IndexerError::malformed("mined.details", "field is missing"))?;
            let details = MinedDetails {
                consensus_branch_id: ConsensusBranchId::new(details_message.consensus_branch_id),
                block_time: details_message.block_time,
                confirmations: details_message.confirmations,
            };
            Ok(TxStatus::Mined(MinedTransaction::new(artifact, details)))
        }
        wallet::transaction_status_response::Status::InMempool(in_mempool) => {
            let chain_epoch = chain_epoch_from_message(chain_epoch_message.clone())
                .map_err(decode_error_to_indexer_error)?;
            let entry = MempoolEntry {
                transaction_id: TransactionId::from_bytes([0; 32]),
                auth_digest: None,
                raw_transaction_bytes: RawTransactionBytes::new(in_mempool.payload_bytes),
                compact_transaction_bytes: Vec::new(),
                first_seen_unix_millis: UnixTimestampMillis::new(
                    u64::try_from(in_mempool.first_seen_unix_seconds.saturating_mul(1000))
                        .unwrap_or(0),
                ),
                first_seen_chain_epoch: chain_epoch,
                transparent_outputs: Vec::new(),
                transparent_spends: Vec::new(),
            };
            Ok(TxStatus::InMempool(entry))
        }
        wallet::transaction_status_response::Status::Conflicting(_) => {
            Ok(TxStatus::ConflictingChain)
        }
    }
}

fn block_id_from_message(block_id: Option<wallet::BlockMetadata>) -> Result<BlockId, IndexerError> {
    let metadata =
        block_id.ok_or_else(|| IndexerError::malformed("block_id", "field is missing"))?;
    let block_hash = block_hash_from_bytes("block_id.block_hash", metadata.block_hash)?;
    Ok(BlockId::new(BlockHeight::new(metadata.height), block_hash))
}

fn block_header_info_from_message(
    message: wallet::BlockHeaderInfo,
) -> Result<BlockHeaderInfo, IndexerError> {
    let block_id = block_id_from_message(message.block_id)?;
    let previous_block_hash = block_hash_from_bytes(
        "block_header.previous_block_hash",
        message.previous_block_hash,
    )?;
    let merkle_root_hash =
        fixed_32_bytes("block_header.merkle_root_hash", message.merkle_root_hash)?;
    let commitment_bytes =
        fixed_32_bytes("block_header.commitment_bytes", message.commitment_bytes)?;
    let nonce = fixed_32_bytes("block_header.nonce", message.nonce)?;
    Ok(BlockHeaderInfo::new(
        block_id,
        previous_block_hash,
        merkle_root_hash,
        commitment_bytes,
        message.block_time,
        message.bits,
        nonce,
        message.version,
    ))
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
            block_hash,
        } => MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        },
        zinder_store::MempoolEvent::Suppressed { transaction_id } => {
            MempoolEvent::Suppressed { transaction_id }
        }
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
