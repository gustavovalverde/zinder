//! Native gRPC adapter for wallet query reads.

use std::{collections::HashMap, num::NonZeroU32, pin::Pin, sync::Arc, time::Instant};

use tokio::sync::{OnceCell, mpsc};
use tokio_stream::{self as stream, Stream, StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use zinder_core::wire::{
    decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex, decode_zinder_native_chain_name,
    encode_rpc_transaction_id_hex,
};
use zinder_core::{
    BlockHeight, BlockHeightRange, BlockSelector, ChainEpochId,
    MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootIndex, SubtreeRootRange, TransactionId, TransparentOutPoint,
};
use zinder_proto::capabilities::WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1;
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, wallet_query_server},
};
use zinder_runtime::{AuthenticatedChannel, BearerToken, connect_zinder_grpc};

use crate::record_proxy_outcome;
use zinder_store::{
    StreamCursorTokenV1, chain_event_stream_family_from_message, stream_cursor_from_message_bytes,
};

type AuthenticatedIngestControlClient = IngestControlClient<AuthenticatedChannel>;

use crate::{
    TransparentAddressTxIdsInRangeRequest, TransparentAddressUnspentOutputsRequest, WalletQueryApi,
};

use super::chain_events::{decode_address_filter, spawn_filtered_stream};
use super::native::{
    ServerInfoSettings, address_lookup_to_script_hash, block_header_by_selector_response,
    block_id_by_selector_response, broadcast_transaction_response, build_chain_view_message,
    build_compact_block_message, build_full_block_message, build_transparent_address_tx_ids_chunk,
    build_transparent_address_tx_ids_header, build_transparent_unspent_output_message,
    build_transparent_unspent_outputs_header, build_wallet_server_info, compact_block_response,
    full_block_response, latest_block_response, latest_safe_block_response,
    latest_tree_state_checkpoint_response, subtree_roots_response, transaction_response,
    transparent_address_unspent_outputs_response, transparent_outputs_by_outpoint_response,
    transparent_spends_by_outpoint_response, transparent_unspent_outputs_by_outpoint_response,
    transparent_utxo_set_summary_response, tree_state_at_response,
};
use super::status_from_query_error;

type WalletGrpcStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = WalletGrpcStream<wallet::ChainEventEnvelope>;
type MempoolEventsStream = WalletGrpcStream<wallet::MempoolEventEnvelope>;
type TransparentUnspentOutputsStream = WalletGrpcStream<wallet::TransparentUnspentOutputsChunk>;
type TransparentAddressTxIdsStream = WalletGrpcStream<wallet::TransparentAddressTxIdsChunk>;
type CompactBlocksInRangeStream = WalletGrpcStream<wallet::CompactBlocksInRangeChunk>;
type FullBlocksInRangeStream = WalletGrpcStream<wallet::FullBlocksInRangeChunk>;

/// gRPC adapter for a [`WalletQueryApi`] implementation.
///
/// Wallet queries that touch in-process state owned by the ingest writer
/// (`ChainEvents` retained replay, the live mempool index, and the mempool
/// event log) are proxied through the colocated `IngestControl` private
/// gRPC endpoint when one is wired. Direct (in-process) handling remains
/// available for development/test deployments that compose ingest and
/// query in one binary.
#[derive(Clone, Debug)]
pub struct WalletQueryGrpcAdapter<QueryApi> {
    query_api: QueryApi,
    server_info: ServerInfoSettings,
    ingest_control_proxy_endpoint: Option<String>,
    ingest_control_bearer_token: Option<BearerToken>,
    /// One cached HTTP/2 channel to the ingest-control writer, dialed lazily
    /// on the first proxied request. Clones of the adapter share the cache
    /// through `Arc<OnceCell<_>>` so concurrent RPCs never race to open
    /// duplicate connections.
    ingest_control_channel: Arc<OnceCell<AuthenticatedChannel>>,
}

impl<QueryApi> WalletQueryGrpcAdapter<QueryApi> {
    /// Creates a gRPC adapter over a wallet query API with the deployment's
    /// `ServerCapabilities` descriptor.
    #[must_use]
    pub fn new(query_api: QueryApi, server_info: ServerInfoSettings) -> Self {
        Self {
            query_api,
            server_info,
            ingest_control_proxy_endpoint: None,
            ingest_control_bearer_token: None,
            ingest_control_channel: Arc::new(OnceCell::new()),
        }
    }

    /// Creates a gRPC adapter that proxies in-process ingest-owned reads
    /// through `IngestControl`.
    ///
    /// The same endpoint serves `ChainEvents`, `MempoolSnapshot`,
    /// `MempoolEvents`, and the live mempool overlay on
    /// `TransparentAddressBalance`; secondary readers cannot observe the
    /// live writer state otherwise.
    #[must_use]
    pub fn with_ingest_control_proxy(
        query_api: QueryApi,
        server_info: ServerInfoSettings,
        ingest_control_proxy_endpoint: String,
    ) -> Self {
        Self {
            query_api,
            server_info,
            ingest_control_proxy_endpoint: Some(ingest_control_proxy_endpoint),
            ingest_control_bearer_token: None,
            ingest_control_channel: Arc::new(OnceCell::new()),
        }
    }

    /// Attaches a shared-secret bearer token to every proxied request.
    /// Required when the `IngestControl` writer is configured with a token;
    /// no-op when the writer is open.
    #[must_use]
    pub fn with_ingest_control_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.ingest_control_bearer_token = Some(bearer_token);
        self
    }

    /// Wraps this adapter in the generated tonic server type.
    #[must_use]
    pub fn into_server(self) -> wallet_query_server::WalletQueryServer<Self>
    where
        Self: wallet_query_server::WalletQuery,
    {
        wallet_query_server::WalletQueryServer::new(self)
    }
}

#[tonic::async_trait]
impl<QueryApi> wallet_query_server::WalletQuery for WalletQueryGrpcAdapter<QueryApi>
where
    QueryApi: Clone + WalletQueryApi + Send + Sync + 'static,
{
    type CompactBlocksInRangeStream = CompactBlocksInRangeStream;
    type FullBlocksInRangeStream = FullBlocksInRangeStream;
    type ChainEventsStream = ChainEventsStream;
    type MempoolEventsStream = MempoolEventsStream;
    type TransparentAddressUnspentOutputsStream = TransparentUnspentOutputsStream;
    type TransparentAddressTxIdsInRangeStream = TransparentAddressTxIdsStream;

    async fn latest_block(
        &self,
        request: Request<wallet::LatestBlockRequest>,
    ) -> Result<Response<wallet::LatestBlockResponse>, Status> {
        latest_block_response(
            &self.query_api,
            chain_epoch_id_from_request(request.into_inner().at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn latest_safe_block(
        &self,
        request: Request<wallet::LatestSafeBlockRequest>,
    ) -> Result<Response<wallet::LatestSafeBlockResponse>, Status> {
        latest_safe_block_response(
            &self.query_api,
            chain_epoch_id_from_request(request.into_inner().at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn compact_block(
        &self,
        request: Request<wallet::CompactBlockRequest>,
    ) -> Result<Response<wallet::CompactBlockResponse>, Status> {
        let request = request.into_inner();
        compact_block_response(
            &self.query_api,
            BlockHeight::new(request.height),
            chain_epoch_id_from_request(request.at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn full_block(
        &self,
        request: Request<wallet::FullBlockRequest>,
    ) -> Result<Response<wallet::FullBlockResponse>, Status> {
        let request = request.into_inner();
        full_block_response(
            &self.query_api,
            BlockHeight::new(request.height),
            chain_epoch_id_from_request(request.at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn block_id_by_selector(
        &self,
        request: Request<wallet::BlockSelectorRequest>,
    ) -> Result<Response<wallet::BlockIdResponse>, Status> {
        let request = request.into_inner();
        let selector = block_selector_from_request(request.selector)?;
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);
        block_id_by_selector_response(&self.query_api, selector, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn block_header_by_selector(
        &self,
        request: Request<wallet::BlockSelectorRequest>,
    ) -> Result<Response<wallet::BlockHeaderResponse>, Status> {
        let request = request.into_inner();
        let selector = block_selector_from_request(request.selector)?;
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);
        block_header_by_selector_response(&self.query_api, selector, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transaction(
        &self,
        request: Request<wallet::TransactionRequest>,
    ) -> Result<Response<wallet::TransactionStatusResponse>, Status> {
        let request = request.into_inner();
        let transaction_id = transaction_id_from_request(&request.transaction_id)?;

        transaction_response(
            &self.query_api,
            transaction_id,
            chain_epoch_id_from_request(request.at_epoch_id),
        )
        .await
        .map_err(|error| status_from_query_error(&error))?
        .ok_or_else(|| {
            Status::not_found("transaction is not visible in the canonical chain or live mempool")
        })
        .map(Response::new)
    }

    async fn compact_blocks_in_range(
        &self,
        request: Request<wallet::CompactBlocksInRangeRequest>,
    ) -> Result<Response<Self::CompactBlocksInRangeStream>, Status> {
        let request = request.into_inner();
        let block_range = BlockHeightRange::inclusive(
            BlockHeight::new(request.start_height),
            BlockHeight::new(request.end_height),
        );
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);

        let compact_blocks_in_range = self
            .query_api
            .compact_blocks_in_range(block_range, at_epoch)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let chain_view = build_chain_view_message(compact_blocks_in_range.chain_epoch);
        let compact_block_chunks =
            compact_blocks_in_range
                .compact_blocks
                .into_iter()
                .map(move |compact_block| {
                    Ok(wallet::CompactBlocksInRangeChunk {
                        chain_view: Some(chain_view.clone()),
                        compact_block: Some(build_compact_block_message(compact_block)),
                    })
                });

        Ok(Response::new(Box::pin(stream::iter(compact_block_chunks))))
    }

    async fn full_blocks_in_range(
        &self,
        request: Request<wallet::FullBlocksInRangeRequest>,
    ) -> Result<Response<Self::FullBlocksInRangeStream>, Status> {
        let request = request.into_inner();
        let block_range = BlockHeightRange::inclusive(
            BlockHeight::new(request.start_height),
            BlockHeight::new(request.end_height),
        );
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);

        let full_blocks_in_range = self
            .query_api
            .full_blocks_in_range(block_range, at_epoch)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let chain_view = build_chain_view_message(full_blocks_in_range.chain_epoch);
        let full_block_chunks =
            full_blocks_in_range
                .block_blobs
                .into_iter()
                .map(move |block_blob| {
                    Ok(wallet::FullBlocksInRangeChunk {
                        chain_view: Some(chain_view.clone()),
                        full_block: Some(build_full_block_message(block_blob)),
                    })
                });

        Ok(Response::new(Box::pin(stream::iter(full_block_chunks))))
    }

    async fn tree_state_at_height(
        &self,
        request: Request<wallet::TreeStateAtHeightRequest>,
    ) -> Result<Response<wallet::TreeStateResponse>, Status> {
        let request = request.into_inner();
        tree_state_at_response(
            &self.query_api,
            BlockHeight::new(request.height),
            chain_epoch_id_from_request(request.at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn latest_tree_state_checkpoint(
        &self,
        request: Request<wallet::LatestTreeStateCheckpointRequest>,
    ) -> Result<Response<wallet::TreeStateResponse>, Status> {
        latest_tree_state_checkpoint_response(
            &self.query_api,
            chain_epoch_id_from_request(request.into_inner().at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn subtree_roots(
        &self,
        request: Request<wallet::SubtreeRootsRequest>,
    ) -> Result<Response<wallet::SubtreeRootsResponse>, Status> {
        let request = request.into_inner();
        let protocol = shielded_protocol_from_request(request.shielded_protocol)?;
        let max_entries = NonZeroU32::new(request.max_entries)
            .ok_or_else(|| Status::invalid_argument("max_entries must be non-zero"))?;
        let subtree_root_range = SubtreeRootRange::new(
            protocol,
            SubtreeRootIndex::new(request.start_index),
            max_entries,
        );
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);

        subtree_roots_response(&self.query_api, subtree_root_range, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn broadcast_transaction(
        &self,
        request: Request<wallet::BroadcastTransactionRequest>,
    ) -> Result<Response<wallet::BroadcastTransactionResponse>, Status> {
        let raw_transaction = RawTransactionBytes::new(request.into_inner().raw_transaction);

        broadcast_transaction_response(&self.query_api, raw_transaction)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn chain_events(
        &self,
        request: Request<wallet::ChainEventsRequest>,
    ) -> Result<Response<Self::ChainEventsStream>, Status> {
        let started_at = Instant::now();
        let outcome: Result<Response<ChainEventsStream>, Status> = async {
            if self.ingest_control_proxy_endpoint.is_some() {
                let mut client = self
                    .ingest_control_client(
                        "ChainEvents requires the ingest-control proxy; \
                         configure the writer endpoint",
                    )
                    .await?;
                let response = client.chain_events(request).await?;
                let stream: ChainEventsStream = Box::pin(response.into_inner());
                return Ok(Response::new(stream));
            }

            let request = request.into_inner();
            let from_cursor = stream_cursor_from_message_bytes(request.from_cursor);
            let family = chain_event_stream_family_from_message(request.family)
                .ok_or_else(|| Status::invalid_argument("chain-event stream family is unknown"))?;
            let network = self.server_info_network()?;
            let address_filter = decode_address_filter(request.address_filter, network)
                .map_err(|error| status_from_query_error(&error))?;
            let query_api = self.query_api.clone();
            let (event_sender, event_receiver) = mpsc::channel(16);
            spawn_filtered_stream(query_api, from_cursor, family, address_filter, event_sender);

            let stream: ChainEventsStream = Box::pin(ReceiverStream::new(event_receiver));
            Ok(Response::new(stream))
        }
        .await;
        record_proxy_outcome("chain_events", started_at, &outcome);
        outcome
    }

    async fn mempool_snapshot(
        &self,
        request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        let started_at = Instant::now();
        let outcome = async {
            let mut client = self
                .ingest_control_client(
                    "MempoolSnapshot requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            client.mempool_snapshot(request).await
        }
        .await;
        record_proxy_outcome("mempool_snapshot", started_at, &outcome);
        outcome
    }

    async fn mempool_events(
        &self,
        request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        let started_at = Instant::now();
        let outcome: Result<Response<MempoolEventsStream>, Status> = async {
            let mut client = self
                .ingest_control_client(
                    "MempoolEvents requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            let response = client.mempool_events(request).await?;
            let stream: MempoolEventsStream = Box::pin(response.into_inner());
            Ok(Response::new(stream))
        }
        .await;
        record_proxy_outcome("mempool_events", started_at, &outcome);
        outcome
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        let started_at = Instant::now();
        let outcome = async {
            let request = request.into_inner();
            let address_script_hash =
                address_lookup_to_script_hash(request.address.clone(), self.server_info_network()?)
                    .map_err(|error| status_from_query_error(&error))?;
            let normalized_request = wallet::TransparentMempoolOutputsByAddressRequest {
                address: Some(typed_script_hash_address_lookup(
                    &address_script_hash.as_bytes(),
                )),
                max_entries: request.max_entries,
            };
            let mut client = self
                .ingest_control_client(
                    "TransparentMempoolOutputsByAddress requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            client
                .transparent_mempool_outputs_by_address(Request::new(normalized_request))
                .await
        }
        .await;
        record_proxy_outcome(
            "transparent_mempool_outputs_by_address",
            started_at,
            &outcome,
        );
        outcome
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        let started_at = Instant::now();
        let outcome = async {
            let mut client = self
                .ingest_control_client(
                    "TransparentMempoolSpendsByOutpoint requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            client.transparent_mempool_spends_by_outpoint(request).await
        }
        .await;
        record_proxy_outcome(
            "transparent_mempool_spends_by_outpoint",
            started_at,
            &outcome,
        );
        outcome
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        request: Request<wallet::TransparentOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        let request = request.into_inner();
        reject_coinbase_sentinels(&request.outpoints)?;
        let outpoints = transparent_outpoints_from_request(request.outpoints)?;
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);
        transparent_outputs_by_outpoint_response(&self.query_api, outpoints, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transparent_spends_by_outpoint(
        &self,
        request: Request<wallet::TransparentSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentSpendsByOutpointResponse>, Status> {
        let request = request.into_inner();
        reject_coinbase_sentinels(&request.outpoints)?;
        let outpoints = transparent_outpoints_from_request(request.outpoints)?;
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);
        transparent_spends_by_outpoint_response(&self.query_api, outpoints, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        request: Request<wallet::TransparentUnspentOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentUnspentOutputsByOutpointResponse>, Status> {
        let request = request.into_inner();
        reject_coinbase_sentinels(&request.outpoints)?;
        let outpoints = transparent_outpoints_from_request(request.outpoints)?;
        let at_epoch = chain_epoch_id_from_request(request.at_epoch_id);
        transparent_unspent_outputs_by_outpoint_response(&self.query_api, outpoints, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        let started_at = Instant::now();
        let outcome = async {
            let request_inner = request.get_ref();
            reject_coinbase_sentinels(&request_inner.outpoints)?;
            let mut client = self
                .ingest_control_client(
                    "TransparentMempoolOutputsByOutpoint requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            client
                .transparent_mempool_outputs_by_outpoint(request)
                .await
        }
        .await;
        record_proxy_outcome(
            "transparent_mempool_outputs_by_outpoint",
            started_at,
            &outcome,
        );
        outcome
    }

    async fn chain_value_pools_at_tip(
        &self,
        request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        let started_at = Instant::now();
        let outcome = async {
            let mut client = self
                .ingest_control_client(
                    "ChainValuePoolsAtTip requires the ingest-control proxy; \
                     configure the writer endpoint",
                )
                .await?;
            client.chain_value_pools_at_tip(request).await
        }
        .await;
        record_proxy_outcome("chain_value_pools_at_tip", started_at, &outcome);
        outcome
    }

    async fn transparent_address_unspent_outputs(
        &self,
        request: Request<wallet::TransparentAddressUnspentOutputsRequest>,
    ) -> Result<Response<Self::TransparentAddressUnspentOutputsStream>, Status> {
        let request = request.into_inner();
        let address_script_hash =
            address_lookup_to_script_hash(request.address, self.server_info_network()?)
                .map_err(|error| status_from_query_error(&error))?;
        let typed_request = TransparentAddressUnspentOutputsRequest {
            address_script_hash,
            start_height: BlockHeight::new(request.start_height),
        };
        let response =
            transparent_address_unspent_outputs_response(&self.query_api, typed_request, None)
                .await
                .map_err(|error| status_from_query_error(&error))?;
        let header = Ok(build_transparent_unspent_outputs_header(
            response.chain_epoch,
        ));
        let items = response
            .outputs
            .into_iter()
            .map(|output| Ok(build_transparent_unspent_output_message(&output)));
        let messages = stream::once(header).chain(stream::iter(items));

        Ok(Response::new(Box::pin(messages)))
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: Request<wallet::TransparentAddressTxIdsInRangeRequest>,
    ) -> Result<Response<Self::TransparentAddressTxIdsInRangeStream>, Status> {
        let request = request.into_inner();
        let typed_request = transparent_address_tx_ids_in_range_request_from_message(
            request,
            self.server_info_network()?,
        )?;
        let response = self
            .query_api
            .transparent_address_tx_ids_in_range(typed_request)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let header = Ok(build_transparent_address_tx_ids_header(
            response.chain_epoch,
        ));
        let next_cursor_bytes = response
            .next_cursor
            .map(|cursor| cursor.as_bytes().to_vec())
            .unwrap_or_default();
        let last_index = response.artifacts.len().saturating_sub(1);
        let items = response
            .artifacts
            .into_iter()
            .enumerate()
            .map(move |(index, artifact)| {
                let cursor = if index == last_index {
                    next_cursor_bytes.clone()
                } else {
                    Vec::new()
                };
                Ok(build_transparent_address_tx_ids_chunk(&artifact, cursor))
            });
        let chunks = stream::once(header).chain(stream::iter(items));
        Ok(Response::new(Box::pin(chunks)))
    }

    async fn transparent_address_balance(
        &self,
        request: Request<wallet::TransparentAddressBalanceRequest>,
    ) -> Result<Response<wallet::TransparentAddressBalanceResponse>, Status> {
        let started_at = Instant::now();
        let outcome = self.compute_transparent_address_balance(request).await;
        record_proxy_outcome("transparent_address_balance", started_at, &outcome);
        outcome
    }

    async fn transparent_utxo_set_summary(
        &self,
        request: Request<wallet::TransparentUtxoSetSummaryRequest>,
    ) -> Result<Response<wallet::TransparentUtxoSetSummaryResponse>, Status> {
        transparent_utxo_set_summary_response(
            &self.query_api,
            chain_epoch_id_from_request(request.into_inner().at_epoch_id),
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn server_info(
        &self,
        _request: Request<wallet::ServerInfoRequest>,
    ) -> Result<Response<wallet::ServerInfoResponse>, Status> {
        let mut wallet_info = build_wallet_server_info(&self.server_info);
        let Some(common) = wallet_info.common.as_mut() else {
            return Err(Status::internal(
                "build_wallet_server_info must populate ops.ServerInfo",
            ));
        };
        if self.ingest_control_proxy_endpoint.is_none() {
            common
                .capabilities
                .retain(|advertised| advertised != WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1);
        }
        Ok(Response::new(wallet::ServerInfoResponse {
            info: Some(wallet_info),
        }))
    }
}

fn transparent_address_tx_ids_in_range_request_from_message(
    request: wallet::TransparentAddressTxIdsInRangeRequest,
    network: Network,
) -> Result<TransparentAddressTxIdsInRangeRequest, Status> {
    let address_script_hash = address_lookup_to_script_hash(request.address, network)
        .map_err(|error| status_from_query_error(&error))?;
    let max_entries =
        max_entries_from_u32(request.max_entries, DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES);
    let from_cursor = if request.from_cursor.is_empty() {
        None
    } else {
        Some(StreamCursorTokenV1::from_bytes(request.from_cursor))
    };

    Ok(TransparentAddressTxIdsInRangeRequest {
        address_script_hash,
        start_height: BlockHeight::new(request.start_height),
        end_height: BlockHeight::new(request.end_height),
        max_entries,
        from_cursor,
        descending: request.descending,
    })
}

const DEFAULT_MAX_TRANSPARENT_HISTORY_ENTRIES: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

fn max_entries_from_u32(requested: u32, cap: NonZeroU32) -> NonZeroU32 {
    NonZeroU32::new(requested).map_or(cap, |max_entries| clamp_max_entries(max_entries, cap))
}

fn clamp_max_entries(requested: NonZeroU32, cap: NonZeroU32) -> NonZeroU32 {
    if requested > cap { cap } else { requested }
}

impl<QueryApi> WalletQueryGrpcAdapter<QueryApi> {
    fn server_info_network(&self) -> Result<Network, Status> {
        decode_zinder_native_chain_name(&self.server_info.network)
            .ok()
            .ok_or_else(|| {
                Status::internal(format!(
                    "server network identifier {} is not recognized",
                    self.server_info.network
                ))
            })
    }

    fn require_ingest_control_proxy_endpoint(
        &self,
        unconfigured_message: &'static str,
    ) -> Result<String, Status> {
        self.ingest_control_proxy_endpoint
            .clone()
            .ok_or_else(|| Status::unavailable(unconfigured_message))
    }

    /// Returns an `IngestControl` client backed by the adapter's cached
    /// HTTP/2 channel.
    ///
    /// The first call to this method on a given adapter instance dials the
    /// writer; subsequent calls reuse the cached
    /// [`AuthenticatedChannel`] (cheap clone, transparent HTTP/2 reconnect).
    /// `unconfigured_message` is the operation-specific `UNAVAILABLE` text
    /// returned when no ingest-control endpoint has been configured.
    ///
    /// The `QueryApi: Sync` bound matches the bound on the `WalletQuery`
    /// trait impl: a `Sync` adapter is required for the returned future to
    /// be `Send`, which `tonic::async_trait` demands at the call site.
    async fn ingest_control_client(
        &self,
        unconfigured_message: &'static str,
    ) -> Result<AuthenticatedIngestControlClient, Status>
    where
        QueryApi: Sync,
    {
        let endpoint = self.require_ingest_control_proxy_endpoint(unconfigured_message)?;
        let bearer_token = self.ingest_control_bearer_token.clone();
        let channel = self
            .ingest_control_channel
            .get_or_try_init(|| async move {
                connect_zinder_grpc(&endpoint, bearer_token.as_ref())
                    .await
                    .map_err(|error| Status::unavailable(error.to_string()))
            })
            .await?;
        Ok(IngestControlClient::new(channel.clone()))
    }

    /// Computes the transparent-address balance: the canonical confirmed total
    /// summed in-process plus the live mempool overlay.
    ///
    /// The confirmed total, address cap, and chain-epoch pin are owned by
    /// [`WalletQueryApi::transparent_address_balance`]. The signed
    /// `unconfirmed_delta_zat` overlay is composed here from the live mempool
    /// surfaces reached through the ingest-control proxy; deployments without
    /// an ingest-control endpoint leave the delta at zero.
    async fn compute_transparent_address_balance(
        &self,
        request: Request<wallet::TransparentAddressBalanceRequest>,
    ) -> Result<Response<wallet::TransparentAddressBalanceResponse>, Status>
    where
        QueryApi: Clone + WalletQueryApi + Send + Sync + 'static,
    {
        let request = request.into_inner();
        if request.addresses.is_empty() {
            return Err(Status::invalid_argument("addresses list must not be empty"));
        }
        let network = self.server_info_network()?;
        let at_epoch_id = chain_epoch_id_from_request(request.at_epoch_id);
        let mut script_hashes = Vec::with_capacity(request.addresses.len());
        for address_lookup in request.addresses {
            let address_script_hash = address_lookup_to_script_hash(Some(address_lookup), network)
                .map_err(|error| status_from_query_error(&error))?;
            script_hashes.push(address_script_hash);
        }

        let confirmed = self
            .query_api
            .transparent_address_balance(script_hashes.clone(), at_epoch_id)
            .await
            .map_err(|error| status_from_query_error(&error))?;

        let unconfirmed_delta_zat = self
            .mempool_balance_overlay(&script_hashes, confirmed.chain_epoch.id)
            .await?;

        Ok(Response::new(wallet::TransparentAddressBalanceResponse {
            chain_view: Some(build_chain_view_message(confirmed.chain_epoch)),
            confirmed_zat: confirmed.confirmed_zat,
            unconfirmed_delta_zat,
            address_count: confirmed.address_count,
        }))
    }

    /// Sums the signed mempool delta for the requested addresses.
    ///
    /// Returns zero when no ingest-control endpoint is wired: the canonical
    /// confirmed total is the whole answer for storage-only deployments.
    /// Otherwise adds pending inflows (mempool outputs paid to the addresses)
    /// and subtracts pending outflows (mempool spends of the addresses'
    /// confirmed unspent set), both read from the live mempool index.
    async fn mempool_balance_overlay(
        &self,
        script_hashes: &[zinder_core::TransparentAddressScriptHash],
        chain_epoch_id: ChainEpochId,
    ) -> Result<i64, Status>
    where
        QueryApi: Clone + WalletQueryApi + Send + Sync + 'static,
    {
        if self.ingest_control_proxy_endpoint.is_none() {
            return Ok(0);
        }
        let mut client = self
            .ingest_control_client(
                "TransparentAddressBalance mempool overlay requires the ingest-control proxy; \
                 configure the writer endpoint",
            )
            .await?;

        let mut unconfirmed_delta_zat: i64 = 0;
        let mut spendable_value_by_outpoint: HashMap<(String, u32), u64> = HashMap::new();
        for address_script_hash in script_hashes {
            let mempool_outputs = client
                .transparent_mempool_outputs_by_address(Request::new(
                    wallet::TransparentMempoolOutputsByAddressRequest {
                        address: Some(typed_script_hash_address_lookup(
                            &address_script_hash.as_bytes(),
                        )),
                        max_entries: None,
                    },
                ))
                .await?
                .into_inner();
            for output in &mempool_outputs.outputs {
                unconfirmed_delta_zat =
                    unconfirmed_delta_zat.saturating_add(value_zat_to_signed(output.value_zat)?);
            }

            let unspent_outputs = self
                .query_api
                .transparent_address_unspent_outputs(
                    TransparentAddressUnspentOutputsRequest {
                        address_script_hash: *address_script_hash,
                        start_height: BlockHeight::new(0),
                    },
                    Some(chain_epoch_id),
                )
                .await
                .map_err(|error| status_from_query_error(&error))?;
            for output in &unspent_outputs.outputs {
                spendable_value_by_outpoint.insert(
                    (
                        encode_rpc_transaction_id_hex(output.outpoint.transaction_id),
                        output.outpoint.output_index,
                    ),
                    output.value_zat,
                );
            }
        }

        let pending_outflow_zat =
            mempool_pending_outflow_zat(&mut client, &spendable_value_by_outpoint).await?;
        Ok(unconfirmed_delta_zat.saturating_sub(pending_outflow_zat))
    }
}

/// Sums the value of confirmed unspent outputs that the live mempool already
/// spends, batched at the per-request outpoint cap.
async fn mempool_pending_outflow_zat(
    client: &mut AuthenticatedIngestControlClient,
    spendable_value_by_outpoint: &HashMap<(String, u32), u64>,
) -> Result<i64, Status> {
    let spendable_outpoints = spendable_value_by_outpoint
        .keys()
        .map(|(transaction_id, output_index)| wallet::OutPoint {
            transaction_id: transaction_id.clone(),
            output_index: *output_index,
        })
        .collect::<Vec<_>>();
    let mut pending_outflow_zat: i64 = 0;
    for outpoint_batch in spendable_outpoints.chunks(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST) {
        let spends = client
            .transparent_mempool_spends_by_outpoint(Request::new(
                wallet::TransparentMempoolSpendsByOutpointRequest {
                    outpoints: outpoint_batch.to_vec(),
                },
            ))
            .await?
            .into_inner();
        for spend in spends.spends {
            let spent_outpoint = spend.spent_outpoint.ok_or_else(|| {
                Status::data_loss(
                    "TransparentMempoolSpend.spent_outpoint missing in IngestControl response",
                )
            })?;
            if let Some(value_zat) = spendable_value_by_outpoint
                .get(&(spent_outpoint.transaction_id, spent_outpoint.output_index))
            {
                pending_outflow_zat =
                    pending_outflow_zat.saturating_add(value_zat_to_signed(*value_zat)?);
            }
        }
    }
    Ok(pending_outflow_zat)
}

/// Builds an `AddressLookup` whose only populated arm is the typed
/// script-hash bytes.
///
/// The native adapter parses public `AddressLookup` selectors at the
/// public boundary; this helper produces the normalized shape the
/// private ingest-control surface accepts.
fn typed_script_hash_address_lookup(script_hash_bytes: &[u8]) -> wallet::AddressLookup {
    wallet::AddressLookup {
        selector: Some(wallet::address_lookup::Selector::ScriptHash(
            script_hash_bytes.to_vec(),
        )),
    }
}

fn transaction_id_from_request(transaction_id_rpc_hex: &str) -> Result<TransactionId, Status> {
    decode_rpc_transaction_id_hex(transaction_id_rpc_hex)
        .map_err(|error| Status::invalid_argument(error.to_string()))
}

/// Rejects the coinbase sentinel outpoint with `INVALID_ARGUMENT` and a
/// `BadRequest`-shaped diagnostic naming the offending request index.
///
fn reject_coinbase_sentinels(outpoints: &[wallet::OutPoint]) -> Result<(), Status> {
    let sentinel = TransparentOutPoint::COINBASE_SENTINEL;
    let sentinel_transaction_id_rpc_hex = encode_rpc_transaction_id_hex(sentinel.transaction_id);
    for (request_index, outpoint) in outpoints.iter().enumerate() {
        if outpoint.transaction_id == sentinel_transaction_id_rpc_hex
            && outpoint.output_index == sentinel.output_index
        {
            return Err(Status::invalid_argument(format!(
                "outpoints[{request_index}] is the coinbase sentinel \
                 (transaction_id is the all-zero RPC-form hash, output_index == 0xFFFFFFFF); \
                 filter coinbase inputs at the request boundary",
            )));
        }
    }
    Ok(())
}

/// Translates a wire outpoint list into typed `TransparentOutPoint`s,
/// silently truncating to [`MAX_TRANSPARENT_OUTPUTS_PER_REQUEST`].
fn transparent_outpoints_from_request(
    mut outpoints: Vec<wallet::OutPoint>,
) -> Result<Vec<TransparentOutPoint>, Status> {
    outpoints.truncate(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);
    outpoints
        .into_iter()
        .map(|outpoint| {
            let transaction_id = transaction_id_from_request(&outpoint.transaction_id)?;
            Ok(TransparentOutPoint::new(
                transaction_id,
                outpoint.output_index,
            ))
        })
        .collect()
}

fn chain_epoch_id_from_request(at_epoch_id: Option<u64>) -> Option<ChainEpochId> {
    at_epoch_id.map(ChainEpochId::new)
}

/// Converts a wire `u64` Zatoshi value to the signed delta-accumulator width.
///
/// Zcash's hardcoded supply cap (`MAX_MONEY = 21,000,000 * 10^8` zat) fits
/// well inside `i64::MAX`, so a `u64` value that does not fit is upstream data
/// corruption and surfaces as `data_loss` rather than silent saturation.
fn value_zat_to_signed(value_zat: u64) -> Result<i64, Status> {
    i64::try_from(value_zat).map_err(|_| {
        Status::data_loss(format!(
            "mempool overlay value_zat {value_zat} exceeds i64::MAX"
        ))
    })
}

fn block_selector_from_request(
    selector: Option<wallet::BlockSelector>,
) -> Result<BlockSelector, Status> {
    let inner = selector
        .and_then(|message| message.selector)
        .ok_or_else(|| Status::invalid_argument("block selector must be specified"))?;
    match inner {
        wallet::block_selector::Selector::Height(height) => {
            Ok(BlockSelector::Height(BlockHeight::new(height)))
        }
        wallet::block_selector::Selector::Hash(hash_rpc_hex) => {
            let block_hash = decode_rpc_block_hash_hex(&hash_rpc_hex)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            Ok(BlockSelector::Hash(block_hash))
        }
    }
}

fn shielded_protocol_from_request(protocol: i32) -> Result<ShieldedProtocol, Status> {
    match wallet::ShieldedProtocol::try_from(protocol) {
        Ok(wallet::ShieldedProtocol::Sapling) => Ok(ShieldedProtocol::Sapling),
        Ok(wallet::ShieldedProtocol::Orchard) => Ok(ShieldedProtocol::Orchard),
        Ok(wallet::ShieldedProtocol::Unspecified) => Err(Status::invalid_argument(
            "shielded_protocol must be specified",
        )),
        Err(_) => Err(Status::invalid_argument("shielded_protocol is unknown")),
    }
}
