//! Remote gRPC implementation of the chain-index contract.

use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use tokio_stream::StreamExt as _;
use tonic::{
    Request,
    transport::{Channel, Endpoint},
};
use tonic_types::StatusExt as _;
use tracing::warn;
use zinder_core::wire::{
    decode_rpc_block_hash_hex, decode_rpc_merkle_root_hex, decode_rpc_transaction_id_hex,
    decode_zinder_native_chain_name, encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex,
};
use zinder_core::{
    BlockHash, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockSelector, ChainEpoch,
    ChainEpochId, ChainValuePool, ChainValuePoolsAtTip, CompactBlockArtifact, ConsensusBranchId,
    MempoolEntry, MinedDetails, MinedTransaction, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange,
    TransactionBroadcastResult, TransactionId, TransactionLocation, TransparentAddressBalance,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TransparentMempoolOutput,
    TransparentMempoolOutputsRequest, TransparentMempoolSpend, TransparentOutPoint,
    TransparentOutputsByOutpointResponse, TransparentSpendEntry,
    TransparentSpendsByOutpointResponse, TransparentUnspentOutput, TreeStateArtifact, TxStatus,
    UnixTimestampMillis,
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

use crate::error::ZINDER_ERROR_DOMAIN;
use crate::{
    BlockId, ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope,
    ChainEventStream, ChainIndex, ChainRangeReverted, EndpointBackedIndex, IndexStream,
    IndexerError, MempoolEvent, MempoolEventCursor, MempoolEventEnvelope, MempoolEventStream,
    MempoolSnapshotCursor, MempoolSnapshotRequest, MempoolSnapshotView,
    TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem,
    TransparentAddressUnspentOutputsQuery, TransparentAddressUnspentOutputsStream,
    TransparentHistoryCursor, TransparentUnspentOutputStreamItem,
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
/// one HTTP/2 connection.
///
/// The channel is held behind an [`ArcSwap`] so the index can self-heal from
/// the tonic 0.14 failure mode where a stream-level h2 error leaves the
/// internal multiplexer in a state where every subsequent call returns an
/// empty `Code::Unavailable` without re-attempting the TCP/h2 handshake.
/// When a call surfaces that exact signature (Unavailable with no
/// `zinder.dev` `ErrorInfo` trailer), the index atomically swaps in a fresh
/// lazy channel built from the saved [`Endpoint`]; the next call dials a new
/// connection. Cloning the type via [`Clone`] is cheap and intentionally
/// shares the swappable channel slot.
#[derive(Clone)]
pub struct RemoteChainIndex {
    client: Arc<ArcSwap<WalletQueryClient<Channel>>>,
    endpoint: Endpoint,
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
        // URI scheme is the TLS signal: `https://` negotiates rustls with system roots.
        let use_tls = options.endpoint.starts_with("https://");
        let mut endpoint = Channel::from_shared(options.endpoint)
            .map_err(|error| IndexerError::invalid_request(error.to_string()))?;
        if use_tls {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|error| IndexerError::invalid_request(error.to_string()))?;
        }
        let endpoint = endpoint
            .keep_alive_while_idle(true)
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(Some(TCP_KEEPALIVE));
        let channel = endpoint.connect_lazy();

        Ok(Self {
            client: Arc::new(ArcSwap::new(Arc::new(WalletQueryClient::new(channel)))),
            endpoint,
            network: options.network,
        })
    }

    /// Returns a per-call `WalletQueryClient` clone backed by the current
    /// swappable channel slot. Cloning the client is cheap and does not
    /// allocate a new connection; concurrent callers share the underlying
    /// HTTP/2 multiplexer until [`Self::rebuild_channel`] swaps it out.
    fn client(&self) -> WalletQueryClient<Channel> {
        (**self.client.load()).clone()
    }

    /// Maps a tonic [`Status`](tonic::Status) into the typed [`IndexerError`]
    /// and rebuilds the underlying gRPC channel when the status carries the
    /// "poisoned multiplexer" signature, i.e. `Code::Unavailable` (or
    /// `Code::Unknown`, which tonic uses for some transport-level failures)
    /// with no `zinder.dev` `ErrorInfo` trailer. Application-level errors
    /// like `Code::InvalidArgument` always preserve the channel even when
    /// the upstream omitted the trailer, since those are caller bugs that
    /// reconnecting would not fix.
    fn handle_status(&self, status: tonic::Status) -> IndexerError {
        let poisoned_transport = matches!(
            status.code(),
            tonic::Code::Unavailable | tonic::Code::Unknown
        ) && status
            .get_error_details()
            .error_info()
            .is_none_or(|error_info| error_info.domain != ZINDER_ERROR_DOMAIN);
        let err = IndexerError::from_status(status);
        if poisoned_transport {
            self.rebuild_channel();
        }
        err
    }

    fn rebuild_channel(&self) {
        let fresh = self.endpoint.connect_lazy();
        self.client.store(Arc::new(WalletQueryClient::new(fresh)));
        warn!(
            target: "zinder_client",
            event = "remote_channel_rebuilt",
            "rebuilt remote chain index gRPC channel after poisoned-transport failure"
        );
    }
}

#[async_trait]
impl ChainIndex for RemoteChainIndex {
    async fn current_epoch(&self) -> Result<ChainEpoch, IndexerError> {
        let response = self
            .client()
            .latest_block(Request::new(wallet::LatestBlockRequest {
                at_epoch_id: None,
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();

        chain_epoch_from_chain_view_with_network(self.network, response.chain_view)
    }

    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
            .latest_block(Request::new(wallet::LatestBlockRequest {
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        let latest_block = response
            .latest_block
            .ok_or_else(|| IndexerError::malformed("latest_block", "field is missing"))?;

        Ok(BlockId {
            height: BlockHeight::new(latest_block.height),
            hash: block_hash_from_rpc_hex("latest_block.block_hash", &latest_block.block_hash)?,
        })
    }

    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
            .latest_safe_block(Request::new(wallet::LatestSafeBlockRequest {
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        let safe_tip_block = response
            .safe_tip_block
            .ok_or_else(|| IndexerError::malformed("safe_tip_block", "field is missing"))?;

        Ok(BlockId {
            height: BlockHeight::new(safe_tip_block.height),
            hash: block_hash_from_rpc_hex("safe_tip_block.block_hash", &safe_tip_block.block_hash)?,
        })
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockId, IndexerError> {
        let response = self
            .client()
            .block_id_by_selector(Request::new(wallet::BlockSelectorRequest {
                selector: Some(block_selector_to_message(selector)?),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        block_id_from_message(response.block_id)
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderInfo, IndexerError> {
        let response = self
            .client()
            .block_header_by_selector(Request::new(wallet::BlockSelectorRequest {
                selector: Some(block_selector_to_message(selector)?),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        let header_message = response
            .block_header
            .ok_or_else(|| IndexerError::malformed("block_header", "field is missing"))?;
        block_header_info_from_message(header_message)
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockArtifact, IndexerError> {
        let response = self
            .client()
            .compact_block(Request::new(wallet::CompactBlockRequest {
                height: height.value(),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        compact_block_from_message(
            response
                .compact_block
                .ok_or_else(|| IndexerError::malformed("compact_block", "field is missing"))?,
        )
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<IndexStream<CompactBlockArtifact>, IndexerError> {
        let response = self
            .client()
            .compact_blocks_in_range(Request::new(wallet::CompactBlocksInRangeRequest {
                start_height: block_range.start.value(),
                end_height: block_range.end.value(),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?;
        let recovery = self.clone();
        let stream = response.into_inner().map(move |chunk_result| {
            let chunk = chunk_result.map_err(|status| recovery.handle_status(status))?;
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
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .tree_state_at_height(Request::new(wallet::TreeStateAtHeightRequest {
                height: height.value(),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeStateArtifact, IndexerError> {
        let response = self
            .client()
            .latest_tree_state_checkpoint(Request::new(wallet::LatestTreeStateCheckpointRequest {
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        tree_state_from_response(response)
    }

    async fn subtree_roots_in_range(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Vec<SubtreeRootArtifact>, IndexerError> {
        let response = self
            .client()
            .subtree_roots(Request::new(wallet::SubtreeRootsRequest {
                shielded_protocol: shielded_protocol_to_message(subtree_root_range.protocol)?
                    as i32,
                start_index: subtree_root_range.start_index.value(),
                max_entries: subtree_root_range.max_entries.get(),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        let protocol = shielded_protocol_from_message(response.shielded_protocol)?;
        response
            .subtree_roots
            .into_iter()
            .map(|root| subtree_root_from_message(protocol, root))
            .collect()
    }

    async fn transaction_by_id(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TxStatus, IndexerError> {
        let response = match self
            .client()
            .transaction(Request::new(wallet::TransactionRequest {
                transaction_id: encode_rpc_transaction_id_hex(transaction_id),
                at_epoch_id: at_epoch_id.map(ChainEpochId::value),
            }))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == tonic::Code::NotFound => {
                if at_epoch_id.is_some() {
                    return Ok(TxStatus::NotFound);
                }
                return self.lookup_in_mempool(transaction_id).await;
            }
            Err(status) => return Err(self.handle_status(status)),
        };
        tx_status_from_message(response)
    }

    async fn transparent_address_unspent_outputs(
        &self,
        query: TransparentAddressUnspentOutputsQuery,
    ) -> Result<TransparentAddressUnspentOutputsStream, IndexerError> {
        let request = wallet::TransparentAddressUnspentOutputsRequest {
            address: Some(wallet::AddressLookup {
                selector: Some(wallet::address_lookup::Selector::ScriptHash(
                    query.address_script_hash.as_bytes().to_vec(),
                )),
            }),
            start_height: query.start_height.value(),
        };
        let response = self
            .client()
            .transparent_address_unspent_outputs(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?;
        let expected_network = self.network;
        let recovery = self.clone();
        // The leading header pins the chain epoch for the whole stream; the
        // closure captures it and drops the header (yielding no item).
        let mut pinned_chain_epoch: Option<ChainEpoch> = None;
        let stream = response.into_inner().filter_map(move |message_result| {
            message_result
                .map_err(|status| recovery.handle_status(status))
                .and_then(|message| {
                    transparent_unspent_output_stream_item(
                        expected_network,
                        &mut pinned_chain_epoch,
                        message,
                    )
                })
                .transpose()
        });
        Ok(Box::pin(stream))
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        query: TransparentAddressTxIdsQuery,
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
            descending: query.descending,
        };
        let response = self
            .client()
            .transparent_address_tx_ids_in_range(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?;
        let expected_network = self.network;
        let recovery = self.clone();
        // The leading header pins the chain epoch for the whole stream; the
        // closure captures it and drops the header (yielding no item).
        let mut pinned_chain_epoch: Option<ChainEpoch> = None;
        let stream = response.into_inner().filter_map(move |chunk_result| {
            chunk_result
                .map_err(|status| recovery.handle_status(status))
                .and_then(|chunk| {
                    transparent_address_tx_ids_stream_item(
                        expected_network,
                        address_script_hash,
                        &mut pinned_chain_epoch,
                        chunk,
                    )
                })
                .transpose()
        });
        Ok(Box::pin(stream))
    }

    async fn transparent_address_balance(
        &self,
        addresses: &[TransparentAddressScriptHash],
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
            at_epoch_id: None,
        };
        let response = self
            .client()
            .transparent_address_balance(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        transparent_address_balance_from_message(self.network, response)
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, IndexerError> {
        let wire_outpoints = outpoints.iter().map(outpoint_message).collect();
        let request = wallet::TransparentOutputsByOutpointRequest {
            outpoints: wire_outpoints,
            at_epoch_id: at_epoch_id.map(ChainEpochId::value),
        };
        let response = self
            .client()
            .transparent_outputs_by_outpoint(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        transparent_outputs_by_outpoint_response_from_message(self.network, response)
    }

    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, IndexerError> {
        let wire_outpoints = outpoints.iter().map(outpoint_message).collect();
        let request = wallet::TransparentSpendsByOutpointRequest {
            outpoints: wire_outpoints,
            at_epoch_id: at_epoch_id.map(ChainEpochId::value),
        };
        let response = self
            .client()
            .transparent_spends_by_outpoint(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        transparent_spends_by_outpoint_response_from_message(self.network, response)
    }
}

#[async_trait]
impl EndpointBackedIndex for RemoteChainIndex {
    async fn server_info(&self) -> Result<WalletServerInfo, IndexerError> {
        let response = self
            .client()
            .server_info(Request::new(wallet::ServerInfoRequest {}))
            .await
            .map_err(|status| self.handle_status(status))?
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

    async fn chain_value_pools_at_tip(&self) -> Result<ChainValuePoolsAtTip, IndexerError> {
        let response = self
            .client()
            .chain_value_pools_at_tip(Request::new(wallet::ChainValuePoolsAtTipRequest {}))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        chain_value_pools_at_tip_from_message(self.network, response)
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
            .map_err(|status| self.handle_status(status))?
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
            .map_err(|status| self.handle_status(status))?;
        let expected_network = self.network;
        let recovery = self.clone();
        let stream = response.into_inner().map(move |event_result| {
            let event = event_result.map_err(|status| recovery.handle_status(status))?;
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
            .map_err(|status| self.handle_status(status))?
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
            .map_err(|status| self.handle_status(status))?;
        let recovery = self.clone();
        let stream = response.into_inner().map(move |event_result| {
            let envelope_message = event_result.map_err(|status| recovery.handle_status(status))?;
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
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        response
            .outputs
            .into_iter()
            .map(transparent_mempool_output_from_message)
            .collect::<Result<Vec<_>, IndexerError>>()
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<Vec<TransparentMempoolSpend>, IndexerError> {
        let wire_request = wallet::TransparentMempoolSpendsByOutpointRequest {
            outpoints: outpoints.iter().map(outpoint_message).collect(),
        };
        let response = self
            .client()
            .transparent_mempool_spends_by_outpoint(Request::new(wire_request))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        response
            .spends
            .into_iter()
            .map(transparent_mempool_spend_from_message)
            .collect()
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<TransparentOutputsByOutpointResponse, IndexerError> {
        let wire_outpoints = outpoints.iter().map(outpoint_message).collect();
        let request = wallet::TransparentMempoolOutputsByOutpointRequest {
            outpoints: wire_outpoints,
        };
        let response = self
            .client()
            .transparent_mempool_outputs_by_outpoint(Request::new(request))
            .await
            .map_err(|status| self.handle_status(status))?
            .into_inner();
        transparent_outputs_by_outpoint_response_from_message(self.network, response)
    }
}

fn transparent_outputs_by_outpoint_response_from_message(
    expected_network: Network,
    message: wallet::TransparentOutputsByOutpointResponse,
) -> Result<TransparentOutputsByOutpointResponse, IndexerError> {
    let chain_epoch =
        chain_epoch_from_chain_view_with_network(expected_network, message.chain_view)?;
    let entries = message
        .entries
        .into_iter()
        .map(transparent_output_entry_from_message)
        .collect::<Result<Vec<_>, IndexerError>>()?;
    Ok(TransparentOutputsByOutpointResponse {
        chain_epoch,
        entries,
    })
}

fn transparent_spends_by_outpoint_response_from_message(
    expected_network: Network,
    message: wallet::TransparentSpendsByOutpointResponse,
) -> Result<TransparentSpendsByOutpointResponse, IndexerError> {
    let chain_epoch =
        chain_epoch_from_chain_view_with_network(expected_network, message.chain_view)?;
    let spends = message
        .spends
        .into_iter()
        .map(transparent_spend_from_message)
        .collect::<Result<Vec<_>, IndexerError>>()?;
    Ok(TransparentSpendsByOutpointResponse {
        chain_epoch,
        spends,
    })
}

fn transparent_spend_from_message(
    message: wallet::TransparentSpend,
) -> Result<TransparentSpendEntry, IndexerError> {
    let outpoint_message = message.spent_outpoint.ok_or_else(|| {
        IndexerError::malformed("transparent_spend.spent_outpoint", "field is missing")
    })?;
    let spent_transaction_id = transaction_id_from_rpc_hex(
        "transparent_spend.spent_outpoint.transaction_id",
        &outpoint_message.transaction_id,
    )?;
    let spent_outpoint =
        TransparentOutPoint::new(spent_transaction_id, outpoint_message.output_index);
    let spending_transaction_id = transaction_id_from_rpc_hex(
        "transparent_spend.spending_transaction_id",
        &message.spending_transaction_id,
    )?;
    let spending_block = message.spending_block.ok_or_else(|| {
        IndexerError::malformed("transparent_spend.spending_block", "field is missing")
    })?;
    let spending_block_hash = block_hash_from_rpc_hex(
        "transparent_spend.spending_block.hash",
        &spending_block.hash,
    )?;
    Ok(TransparentSpendEntry {
        spent_outpoint,
        spending_transaction_id,
        input_index: message.input_index,
        spending_block_height: BlockHeight::new(spending_block.height),
        spending_block_hash,
    })
}

fn chain_value_pools_at_tip_from_message(
    expected_network: Network,
    message: wallet::ChainValuePoolsAtTipResponse,
) -> Result<ChainValuePoolsAtTip, IndexerError> {
    let chain_epoch =
        chain_epoch_from_chain_view_with_network(expected_network, message.chain_view)?;
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

fn transparent_output_entry_from_message(
    message: wallet::TransparentOutputEntry,
) -> Result<zinder_core::TransparentOutputEntry, IndexerError> {
    let outpoint_message = message.outpoint.ok_or_else(|| {
        IndexerError::malformed("transparent_output_entry.outpoint", "field is missing")
    })?;
    let transaction_id = transaction_id_from_rpc_hex(
        "transparent_output_entry.outpoint.transaction_id",
        &outpoint_message.transaction_id,
    )?;
    let outpoint = TransparentOutPoint::new(transaction_id, outpoint_message.output_index);
    let prevout = message
        .output
        .map(|prevout_message| zinder_core::TransparentOutput {
            value_zat: prevout_message.value_zat,
            script_pub_key: prevout_message.script_pub_key,
        });
    Ok(zinder_core::TransparentOutputEntry {
        outpoint,
        output: prevout,
    })
}

fn transparent_address_balance_from_message(
    expected_network: Network,
    message: wallet::TransparentAddressBalanceResponse,
) -> Result<TransparentAddressBalance, IndexerError> {
    let chain_epoch =
        chain_epoch_from_chain_view_with_network(expected_network, message.chain_view)?;
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

/// Decodes the chain epoch from a response's [`wallet::ChainView`] envelope,
/// asserting the network matches the endpoint the client opened against.
fn chain_epoch_from_chain_view_with_network(
    expected_network: Network,
    chain_view: Option<wallet::ChainView>,
) -> Result<ChainEpoch, IndexerError> {
    let chain_view =
        chain_view.ok_or_else(|| IndexerError::malformed("chain_view", "field is missing"))?;
    let chain_epoch = chain_view
        .chain_epoch
        .ok_or_else(|| IndexerError::malformed("chain_view.chain_epoch", "field is missing"))?;
    chain_epoch_from_message_with_network(expected_network, chain_epoch)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Used as a Result::map_err callback so the value-passing signature is required."
)]
fn decode_error_to_indexer_error(error: MempoolDecodeError) -> IndexerError {
    IndexerError::malformed(error.field(), error.to_string())
}

fn compact_block_from_message(
    message: wallet::CompactBlock,
) -> Result<CompactBlockArtifact, IndexerError> {
    Ok(CompactBlockArtifact::new(
        BlockHeight::new(message.height),
        block_hash_from_rpc_hex("compact_block.block_hash", &message.block_hash)?,
        message.payload_bytes,
    ))
}

fn tree_state_from_response(
    response: wallet::TreeStateResponse,
) -> Result<TreeStateArtifact, IndexerError> {
    Ok(TreeStateArtifact::new(
        BlockHeight::new(response.height),
        block_hash_from_rpc_hex("tree_state.block_hash", &response.block_hash)?,
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
        block_hash_from_rpc_hex(
            "subtree_root.completing_block_hash",
            &message.completing_block_hash,
        )?,
    ))
}

/// Decodes one message of the `TransparentAddressTxIdsInRange` stream.
///
/// The leading header carries the chain epoch pinned for the whole stream; it
/// is captured in `pinned_chain_epoch` and yields no item (`Ok(None)`). Every
/// later item is bound to that captured epoch. An item before the header, or a
/// second header, is a protocol violation.
fn transparent_address_tx_ids_stream_item(
    expected_network: Network,
    address_script_hash: TransparentAddressScriptHash,
    pinned_chain_epoch: &mut Option<ChainEpoch>,
    message: wallet::TransparentAddressTxIdsChunk,
) -> Result<Option<TransparentAddressTxIdsStreamItem>, IndexerError> {
    match message.body.ok_or_else(|| {
        IndexerError::malformed("transparent_address_tx_ids_chunk.body", "field is missing")
    })? {
        wallet::transparent_address_tx_ids_chunk::Body::Header(chain_view) => {
            stream_header_chain_epoch(expected_network, pinned_chain_epoch, Some(chain_view))?;
            Ok(None)
        }
        wallet::transparent_address_tx_ids_chunk::Body::Item(entry) => {
            let chain_epoch = stream_item_chain_epoch(pinned_chain_epoch.as_ref())?;
            let transaction_id =
                transaction_id_from_rpc_hex("transaction_id", &entry.transaction_id)?;
            let block_hash = block_hash_from_rpc_hex("block_hash", &entry.block_hash)?;
            let artifact = TransparentAddressTxIndexArtifact::new(
                address_script_hash,
                BlockHeight::new(entry.block_height),
                entry.tx_index_in_block,
                transaction_id,
                block_hash,
            );
            let cursor = if entry.cursor.is_empty() {
                None
            } else {
                Some(TransparentHistoryCursor::from_bytes(entry.cursor))
            };
            Ok(Some(TransparentAddressTxIdsStreamItem {
                chain_epoch,
                artifact,
                cursor,
            }))
        }
    }
}

/// Decodes one message of the `TransparentAddressUnspentOutputs` stream.
///
/// The leading header carries the chain epoch pinned for the whole stream; it
/// is captured in `pinned_chain_epoch` and yields no item (`Ok(None)`). Every
/// later item is bound to that captured epoch. An item before the header, or a
/// second header, is a protocol violation.
fn transparent_unspent_output_stream_item(
    expected_network: Network,
    pinned_chain_epoch: &mut Option<ChainEpoch>,
    message: wallet::TransparentUnspentOutputsChunk,
) -> Result<Option<TransparentUnspentOutputStreamItem>, IndexerError> {
    match message.body.ok_or_else(|| {
        IndexerError::malformed("transparent_unspent_outputs_chunk.body", "field is missing")
    })? {
        wallet::transparent_unspent_outputs_chunk::Body::Header(chain_view) => {
            stream_header_chain_epoch(expected_network, pinned_chain_epoch, Some(chain_view))?;
            Ok(None)
        }
        wallet::transparent_unspent_outputs_chunk::Body::Item(output_message) => {
            let chain_epoch = stream_item_chain_epoch(pinned_chain_epoch.as_ref())?;
            let address_script_hash_bytes = fixed_32_bytes(
                "transparent_unspent_output.address_script_hash",
                output_message.address_script_hash,
            )?;
            let outpoint_message = output_message.outpoint.ok_or_else(|| {
                IndexerError::malformed("transparent_unspent_output.outpoint", "field is missing")
            })?;
            let transaction_id = transaction_id_from_rpc_hex(
                "transparent_unspent_output.outpoint.transaction_id",
                &outpoint_message.transaction_id,
            )?;
            let block_hash = block_hash_from_rpc_hex(
                "transparent_unspent_output.block_hash",
                &output_message.block_hash,
            )?;
            let output = TransparentUnspentOutput::new(
                TransparentAddressScriptHash::from_bytes(address_script_hash_bytes),
                output_message.script_pub_key,
                TransparentOutPoint::new(transaction_id, outpoint_message.output_index),
                output_message.value_zat,
                BlockHeight::new(output_message.block_height),
                block_hash,
            );
            Ok(Some(TransparentUnspentOutputStreamItem {
                chain_epoch,
                output,
            }))
        }
    }
}

/// Records the pinned chain epoch from a stream's leading header.
///
/// Rejects a second header: the stream-header contract sends exactly one.
fn stream_header_chain_epoch(
    expected_network: Network,
    pinned_chain_epoch: &mut Option<ChainEpoch>,
    chain_view: Option<wallet::ChainView>,
) -> Result<(), IndexerError> {
    if pinned_chain_epoch.is_some() {
        return Err(IndexerError::malformed(
            "stream.header",
            "stream sent more than one header",
        ));
    }
    let chain_epoch = chain_epoch_from_chain_view_with_network(expected_network, chain_view)?;
    *pinned_chain_epoch = Some(chain_epoch);
    Ok(())
}

/// Returns the chain epoch a stream item binds to, rejecting an item that
/// arrives before the leading header.
fn stream_item_chain_epoch(
    pinned_chain_epoch: Option<&ChainEpoch>,
) -> Result<ChainEpoch, IndexerError> {
    pinned_chain_epoch.copied().ok_or_else(|| {
        IndexerError::malformed("stream.item", "stream sent an item before its header")
    })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "MinedBlockLocation is a tiny POD; taking by value matches the symmetric encoder."
)]
fn mined_block_location_from_message(
    message: wallet::MinedBlockLocation,
) -> Result<TransactionLocation, IndexerError> {
    Ok(TransactionLocation::new(
        transaction_id_from_rpc_hex(
            "mined_block_location.transaction_id",
            &message.transaction_id,
        )?,
        BlockHeight::new(message.block_height),
        block_hash_from_rpc_hex("mined_block_location.block_hash", &message.block_hash)?,
        message.tx_index_in_block,
    ))
}

fn transaction_broadcast_result_from_message(
    response: wallet::BroadcastTransactionResponse,
) -> Result<TransactionBroadcastResult, IndexerError> {
    use wallet::broadcast_transaction_response::Outcome;
    use zinder_core::{
        BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
        BroadcastRejected, BroadcastUnknown,
    };

    let outcome = response
        .outcome
        .ok_or_else(|| IndexerError::malformed("outcome", "field is missing"))?;
    match outcome {
        Outcome::Accepted(accepted) => {
            Ok(TransactionBroadcastResult::Accepted(BroadcastAccepted {
                transaction_id: transaction_id_from_rpc_hex(
                    "accepted.transaction_id",
                    &accepted.transaction_id,
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
        Outcome::Queued(queued) => Ok(TransactionBroadcastResult::Queued(BroadcastQueued {
            message: queued.message,
        })),
        Outcome::Rejected(rejected) => {
            Ok(TransactionBroadcastResult::Rejected(BroadcastRejected {
                kind: broadcast_rejection_reason_from_message(rejected.kind),
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

fn broadcast_rejection_reason_from_message(code: i32) -> zinder_core::BroadcastRejectionReason {
    use zinder_core::BroadcastRejectionReason;

    match wallet::BroadcastRejectionReason::try_from(code) {
        Ok(wallet::BroadcastRejectionReason::InvalidSignature) => {
            BroadcastRejectionReason::InvalidSignature
        }
        Ok(wallet::BroadcastRejectionReason::BadExpiryHeight) => {
            BroadcastRejectionReason::BadExpiryHeight
        }
        Ok(wallet::BroadcastRejectionReason::BadConsensusBranch) => {
            BroadcastRejectionReason::BadConsensusBranch
        }
        Ok(wallet::BroadcastRejectionReason::MempoolFull) => BroadcastRejectionReason::MempoolFull,
        // Unspecified and Unknown both collapse to Unknown on the client side:
        // an old server that never sets the field is indistinguishable from a
        // server that explicitly reports an unclassified rejection.
        _ => BroadcastRejectionReason::Unknown,
    }
}

fn chain_event_envelope_from_message(
    expected_network: Network,
    message: wallet::ChainEventEnvelope,
) -> Result<ChainEventEnvelope, IndexerError> {
    let chain_epoch =
        chain_epoch_from_chain_view_with_network(expected_network, message.chain_view)?;
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
        safe_tip_height: chain_epoch.settled_tip_height,
        chain_epoch,
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
        ChainEventStreamFamily::Safe => wallet::ChainEventStreamFamily::Safe,
    }
}

fn block_hash_from_rpc_hex(field: &'static str, rpc_hex: &str) -> Result<BlockHash, IndexerError> {
    decode_rpc_block_hash_hex(rpc_hex)
        .map_err(|error| IndexerError::malformed(field, error.to_string()))
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
            wallet::block_selector::Selector::Hash(encode_rpc_block_hash_hex(hash))
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
    reason = "TransactionLocation oneof is non-exhaustive in the protobuf-generated enum; new variants are a deliberate wire change."
)]
fn tx_status_from_message(
    response: wallet::TransactionStatusResponse,
) -> Result<TxStatus, IndexerError> {
    let chain_epoch_message = response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| IndexerError::malformed("chain_view.chain_epoch", "field is missing"))?;
    let location = response
        .location
        .and_then(|location| location.location)
        .ok_or_else(|| IndexerError::malformed("location.location", "field is missing"))?;
    match location {
        wallet::transaction_location::Location::Mined(mined) => {
            let location =
                mined_block_location_from_message(mined.location.ok_or_else(|| {
                    IndexerError::malformed("mined.location", "field is missing")
                })?)?;
            let details_message = mined
                .details
                .ok_or_else(|| IndexerError::malformed("mined.details", "field is missing"))?;
            let details = MinedDetails {
                consensus_branch_id: ConsensusBranchId::new(details_message.consensus_branch_id),
                block_time: details_message.block_time,
                confirmations: details_message.confirmations,
            };
            Ok(TxStatus::Mined(MinedTransaction::new(
                location,
                details,
                mined.raw_transaction_bytes,
            )))
        }
        wallet::transaction_location::Location::InMempool(in_mempool) => {
            let chain_epoch = chain_epoch_from_message(chain_epoch_message)
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
        wallet::transaction_location::Location::Conflicting(_) => Ok(TxStatus::ConflictingChain),
    }
}

fn block_id_from_message(block_id: Option<wallet::BlockMetadata>) -> Result<BlockId, IndexerError> {
    let metadata =
        block_id.ok_or_else(|| IndexerError::malformed("block_id", "field is missing"))?;
    let block_hash = block_hash_from_rpc_hex("block_id.block_hash", &metadata.block_hash)?;
    Ok(BlockId::new(BlockHeight::new(metadata.height), block_hash))
}

fn block_header_info_from_message(
    message: wallet::BlockHeaderInfo,
) -> Result<BlockHeaderInfo, IndexerError> {
    let block_id = block_id_from_message(message.block_id)?;
    let previous_block_hash = block_hash_from_rpc_hex(
        "block_header.previous_block_hash",
        &message.previous_block_hash,
    )?;
    let merkle_root_hash =
        merkle_root_hash_from_rpc_hex("block_header.merkle_root_hash", &message.merkle_root_hash)?;
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

fn transaction_id_from_rpc_hex(
    field: &'static str,
    rpc_hex: &str,
) -> Result<TransactionId, IndexerError> {
    decode_rpc_transaction_id_hex(rpc_hex)
        .map_err(|error| IndexerError::malformed(field, error.to_string()))
}

fn merkle_root_hash_from_rpc_hex(
    field: &'static str,
    rpc_hex: &str,
) -> Result<[u8; 32], IndexerError> {
    decode_rpc_merkle_root_hex(rpc_hex)
        .map_err(|error| IndexerError::malformed(field, error.to_string()))
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
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| IndexerError::malformed("chain_view.chain_epoch", "field is missing"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IndexerError;
    use tonic::{Code, Status};

    #[allow(
        clippy::expect_used,
        reason = "syntactically valid URI; connect_lazy cannot fail here"
    )]
    fn build_index() -> RemoteChainIndex {
        RemoteChainIndex::connect(RemoteOpenOptions {
            endpoint: "http://127.0.0.1:1".to_owned(),
            network: Network::ZcashRegtest,
        })
        .expect("connect_lazy never errors for a syntactically valid URI")
    }

    fn current_client_ptr(index: &RemoteChainIndex) -> usize {
        Arc::as_ptr(&index.client.load_full()) as usize
    }

    #[tokio::test]
    async fn handle_status_swaps_channel_on_poisoned_unavailable() {
        let index = build_index();
        let before = current_client_ptr(&index);

        let err = index.handle_status(Status::new(Code::Unavailable, ""));

        let after = current_client_ptr(&index);
        assert!(
            matches!(
                &err,
                IndexerError::ServiceUnavailable { reason }
                    if reason.starts_with("missing zinder.dev ErrorInfo")
            ),
            "expected ServiceUnavailable with poisoned-transport reason, got {err:?}"
        );
        assert_ne!(
            before, after,
            "empty Code::Unavailable without zinder.dev ErrorInfo must swap the channel"
        );
    }

    #[tokio::test]
    async fn handle_status_keeps_channel_on_invalid_argument() {
        let index = build_index();
        let before = current_client_ptr(&index);

        let _ = index.handle_status(Status::new(Code::InvalidArgument, "bad cursor"));

        let after = current_client_ptr(&index);
        assert_eq!(
            before, after,
            "application-level errors with InvalidArgument must not rebuild the channel"
        );
    }
}
