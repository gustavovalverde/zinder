//! Native gRPC adapter for wallet query reads.

use std::{num::NonZeroU32, pin::Pin};

use tokio::sync::mpsc;
use tokio_stream::{self as stream, Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use zinder_core::wire::{decode_zinder_native_chain_name, encode_internal_transaction_id};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockSelector, ChainEpoch,
    MAX_TRANSPARENT_PREVOUTS_PER_REQUEST, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootIndex, SubtreeRootRange, TransactionId, TransparentOutPoint,
};
use zinder_proto::capabilities::{
    EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1, WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
};
use zinder_proto::v1::{
    explorer::explorer_query_client::ExplorerQueryClient,
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, wallet_query_server},
};
use zinder_runtime::{AuthenticatedChannel, BearerToken, connect_authenticated_channel};

use crate::{DeriveProxy, derive_proxy::DeriveReadinessGauge};
use zinder_store::{
    StreamCursorTokenV1, chain_epoch_from_message, chain_event_stream_family_from_message,
    stream_cursor_from_message_bytes,
};

type AuthenticatedIngestControlClient = IngestControlClient<AuthenticatedChannel>;

use crate::{
    TransparentAddressTxIdsInRangeRequest, TransparentAddressUtxosRequest, WalletQueryApi,
};

use super::chain_events::{decode_address_filter, spawn_filtered_stream};
use super::native::{
    ServerInfoSettings, address_lookup_to_script_hash, block_header_by_selector_response,
    block_id_by_selector_response, broadcast_transaction_response, build_chain_epoch_message,
    build_compact_block_message, build_transparent_address_tx_ids_chunk,
    build_transparent_address_utxos_stream_chunk, build_wallet_server_info, compact_block_response,
    full_block_response, latest_block_response, latest_tree_state_response, subtree_roots_response,
    transaction_response, transparent_address_confirmed_balance_response,
    transparent_address_utxos_response, transparent_prevouts_response, tree_state_response,
};
use super::status_from_query_error;

type WalletGrpcStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = WalletGrpcStream<wallet::ChainEventEnvelope>;
type MempoolEventsStream = WalletGrpcStream<wallet::MempoolEventEnvelope>;
type TransparentAddressUtxosStream = WalletGrpcStream<wallet::TransparentAddressUtxosStreamChunk>;
type TransparentAddressTxIdsStream = WalletGrpcStream<wallet::TransparentAddressTxIdsChunk>;

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
    explorer_proxy: Option<DeriveProxy<ExplorerQueryClient<AuthenticatedChannel>>>,
}

impl<QueryApi> WalletQueryGrpcAdapter<QueryApi> {
    /// Creates a gRPC adapter over a wallet query API with the deployment's
    /// `ServerCapabilities` descriptor.
    #[must_use]
    pub const fn new(query_api: QueryApi, server_info: ServerInfoSettings) -> Self {
        Self {
            query_api,
            server_info,
            ingest_control_proxy_endpoint: None,
            ingest_control_bearer_token: None,
            explorer_proxy: None,
        }
    }

    /// Creates a gRPC adapter that proxies in-process ingest-owned reads
    /// through `IngestControl`.
    ///
    /// The same endpoint serves `ChainEvents`, `MempoolSnapshot`, and
    /// `MempoolEvents`; secondary readers cannot observe the live writer
    /// state otherwise.
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
            explorer_proxy: None,
        }
    }

    /// Wires the explorer-plane proxy that federates
    /// `WalletQuery.TransparentAddressBalance` to
    /// `zinder-explorer`'s `ExplorerQuery`.
    ///
    /// Without an explorer proxy the federated balance method returns
    /// `UNAVAILABLE` and `ServerInfo` omits the corresponding capability
    /// string.
    #[must_use]
    pub fn with_explorer_proxy(
        mut self,
        explorer_proxy: DeriveProxy<ExplorerQueryClient<AuthenticatedChannel>>,
    ) -> Self {
        self.explorer_proxy = Some(explorer_proxy);
        self
    }

    /// Returns the readiness gauge of the configured explorer proxy.
    ///
    /// Operators wire the gauge into a [`crate::spawn_derive_readiness_probe`]
    /// task so the federated capability is gated on a live readiness probe.
    #[must_use]
    pub fn explorer_proxy_readiness(&self) -> Option<DeriveReadinessGauge> {
        self.explorer_proxy.as_ref().map(DeriveProxy::readiness)
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
    type CompactBlockRangeStream = WalletGrpcStream<wallet::CompactBlockRangeChunk>;
    type ChainEventsStream = ChainEventsStream;
    type MempoolEventsStream = MempoolEventsStream;
    type TransparentAddressUtxosStreamStream = TransparentAddressUtxosStream;
    type TransparentAddressTxIdsInRangeStream = TransparentAddressTxIdsStream;

    async fn latest_block(
        &self,
        request: Request<wallet::LatestBlockRequest>,
    ) -> Result<Response<wallet::LatestBlockResponse>, Status> {
        latest_block_response(
            &self.query_api,
            chain_epoch_from_request(request.into_inner().at_epoch)?,
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
            chain_epoch_from_request(request.at_epoch)?,
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
            BlockHeight::new(request.block_height),
            chain_epoch_from_request(request.at_epoch)?,
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn block_id_by_selector(
        &self,
        request: Request<wallet::BlockIdBySelectorRequest>,
    ) -> Result<Response<wallet::BlockIdResponse>, Status> {
        let request = request.into_inner();
        let selector = block_selector_from_request(request.selector)?;
        let at_epoch = chain_epoch_from_request(request.at_epoch)?;
        block_id_by_selector_response(&self.query_api, selector, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn block_header_by_selector(
        &self,
        request: Request<wallet::BlockIdBySelectorRequest>,
    ) -> Result<Response<wallet::BlockHeaderResponse>, Status> {
        let request = request.into_inner();
        let selector = block_selector_from_request(request.selector)?;
        let at_epoch = chain_epoch_from_request(request.at_epoch)?;
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
            chain_epoch_from_request(request.at_epoch)?,
        )
        .await
        .map_err(|error| status_from_query_error(&error))?
        .ok_or_else(|| {
            Status::not_found("transaction is not visible in the canonical chain or live mempool")
        })
        .map(Response::new)
    }

    async fn compact_block_range(
        &self,
        request: Request<wallet::CompactBlockRangeRequest>,
    ) -> Result<Response<Self::CompactBlockRangeStream>, Status> {
        let request = request.into_inner();
        let block_range = BlockHeightRange::inclusive(
            BlockHeight::new(request.start_height),
            BlockHeight::new(request.end_height),
        );
        let at_epoch = chain_epoch_from_request(request.at_epoch)?;

        let compact_block_range = self
            .query_api
            .compact_block_range(block_range, at_epoch)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let chain_epoch = build_chain_epoch_message(compact_block_range.chain_epoch);
        let compact_block_chunks =
            compact_block_range
                .compact_blocks
                .into_iter()
                .map(move |compact_block| {
                    Ok(wallet::CompactBlockRangeChunk {
                        chain_epoch: Some(chain_epoch.clone()),
                        compact_block: Some(build_compact_block_message(compact_block)),
                    })
                });

        Ok(Response::new(Box::pin(stream::iter(compact_block_chunks))))
    }

    async fn tree_state(
        &self,
        request: Request<wallet::TreeStateRequest>,
    ) -> Result<Response<wallet::TreeStateResponse>, Status> {
        let request = request.into_inner();
        tree_state_response(
            &self.query_api,
            BlockHeight::new(request.height),
            chain_epoch_from_request(request.at_epoch)?,
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn latest_tree_state(
        &self,
        request: Request<wallet::LatestTreeStateRequest>,
    ) -> Result<Response<wallet::TreeStateResponse>, Status> {
        latest_tree_state_response(
            &self.query_api,
            chain_epoch_from_request(request.into_inner().at_epoch)?,
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
        let at_epoch = chain_epoch_from_request(request.at_epoch)?;

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
        if let Some(endpoint) = &self.ingest_control_proxy_endpoint {
            return proxy_chain_events(
                endpoint.clone(),
                self.ingest_control_bearer_token.clone(),
                request,
            )
            .await;
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

        Ok(Response::new(Box::pin(ReceiverStream::new(event_receiver))))
    }

    async fn mempool_snapshot(
        &self,
        request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        let endpoint = self
            .ingest_control_proxy_endpoint
            .as_ref()
            .ok_or_else(|| {
                Status::unavailable(
                    "MempoolSnapshot requires the ingest-control proxy; configure the writer endpoint",
                )
            })?
            .clone();
        proxy_mempool_snapshot(endpoint, self.ingest_control_bearer_token.clone(), request).await
    }

    async fn mempool_events(
        &self,
        request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        let endpoint = self
            .ingest_control_proxy_endpoint
            .as_ref()
            .ok_or_else(|| {
                Status::unavailable(
                    "MempoolEvents requires the ingest-control proxy; configure the writer endpoint",
                )
            })?
            .clone();
        proxy_mempool_events(endpoint, self.ingest_control_bearer_token.clone(), request).await
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        let endpoint = self.require_ingest_control_proxy_endpoint(
            "TransparentMempoolOutputsByAddress requires the ingest-control proxy; \
             configure the writer endpoint",
        )?;
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
        let mut client =
            connect_authenticated_proxy(&endpoint, self.ingest_control_bearer_token.as_ref())
                .await?;
        client
            .transparent_mempool_outputs_by_address(Request::new(normalized_request))
            .await
    }

    async fn transparent_mempool_spend_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolSpendByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendByOutpointResponse>, Status> {
        let endpoint = self.require_ingest_control_proxy_endpoint(
            "TransparentMempoolSpendByOutpoint requires the ingest-control proxy; \
             configure the writer endpoint",
        )?;
        let mut client =
            connect_authenticated_proxy(&endpoint, self.ingest_control_bearer_token.as_ref())
                .await?;
        client.transparent_mempool_spend_by_outpoint(request).await
    }

    async fn transparent_prevouts(
        &self,
        request: Request<wallet::TransparentPrevoutsRequest>,
    ) -> Result<Response<wallet::TransparentPrevoutsResponse>, Status> {
        let request = request.into_inner();
        reject_coinbase_sentinels(&request.outpoints)?;
        let outpoints = transparent_outpoints_from_request(request.outpoints)?;
        let at_epoch = chain_epoch_from_request(request.at_epoch)?;
        transparent_prevouts_response(&self.query_api, outpoints, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transparent_mempool_prevouts(
        &self,
        request: Request<wallet::TransparentMempoolPrevoutsRequest>,
    ) -> Result<Response<wallet::TransparentPrevoutsResponse>, Status> {
        let request_inner = request.get_ref();
        reject_coinbase_sentinels(&request_inner.outpoints)?;
        let endpoint = self.require_ingest_control_proxy_endpoint(
            "TransparentMempoolPrevouts requires the ingest-control proxy; \
             configure the writer endpoint",
        )?;
        let mut client =
            connect_authenticated_proxy(&endpoint, self.ingest_control_bearer_token.as_ref())
                .await?;
        client.transparent_mempool_prevouts(request).await
    }

    async fn transparent_address_utxos(
        &self,
        request: Request<wallet::TransparentAddressUtxosRequest>,
    ) -> Result<Response<wallet::TransparentAddressUtxosResponse>, Status> {
        let request = request.into_inner();
        let at_epoch = chain_epoch_from_request(request.at_epoch.clone())?;
        let typed_request =
            transparent_address_utxos_request_from_message(request, self.server_info_network()?)?;

        transparent_address_utxos_response(&self.query_api, typed_request, at_epoch)
            .await
            .map(Response::new)
            .map_err(|error| status_from_query_error(&error))
    }

    async fn transparent_address_utxos_stream(
        &self,
        request: Request<wallet::TransparentAddressUtxosRequest>,
    ) -> Result<Response<Self::TransparentAddressUtxosStreamStream>, Status> {
        let request = request.into_inner();
        let at_epoch = chain_epoch_from_request(request.at_epoch.clone())?;
        let typed_request =
            transparent_address_utxos_request_from_message(request, self.server_info_network()?)?;
        let response = self
            .query_api
            .transparent_address_utxos(typed_request, at_epoch)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let chain_epoch = response.chain_epoch;
        let next_cursor_bytes = response
            .next_cursor
            .map(|cursor| cursor.as_bytes().to_vec())
            .unwrap_or_default();
        let last_index = response.utxos.len().saturating_sub(1);
        let chunk_iter = response
            .utxos
            .into_iter()
            .enumerate()
            .map(move |(index, utxo)| {
                let cursor_bytes = if index == last_index {
                    next_cursor_bytes.clone()
                } else {
                    Vec::new()
                };
                Ok(build_transparent_address_utxos_stream_chunk(
                    chain_epoch,
                    &utxo,
                    cursor_bytes,
                ))
            });

        Ok(Response::new(Box::pin(stream::iter(chunk_iter))))
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: Request<wallet::TransparentAddressTxIdsInRangeRequest>,
    ) -> Result<Response<Self::TransparentAddressTxIdsInRangeStream>, Status> {
        let request = request.into_inner();
        let at_epoch = chain_epoch_from_request(request.at_epoch.clone())?;
        let typed_request = transparent_address_tx_ids_in_range_request_from_message(
            request,
            self.server_info_network()?,
        )?;
        let response = self
            .query_api
            .transparent_address_tx_ids_in_range(typed_request, at_epoch)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let chain_epoch = response.chain_epoch;
        let next_cursor_bytes = response
            .next_cursor
            .map(|cursor| cursor.as_bytes().to_vec())
            .unwrap_or_default();
        let last_index = response.artifacts.len().saturating_sub(1);
        let chunks = response
            .artifacts
            .into_iter()
            .enumerate()
            .map(move |(index, artifact)| {
                let cursor = if index == last_index {
                    next_cursor_bytes.clone()
                } else {
                    Vec::new()
                };
                Ok(build_transparent_address_tx_ids_chunk(
                    chain_epoch,
                    &artifact,
                    cursor,
                ))
            });
        Ok(Response::new(Box::pin(stream::iter(chunks))))
    }

    async fn transparent_address_balance(
        &self,
        request: Request<wallet::TransparentAddressBalanceRequest>,
    ) -> Result<Response<wallet::TransparentAddressBalanceResponse>, Status> {
        // Prefer the federated explorer plane when configured and ready: it
        // adds the mempool overlay that canonical-only compute cannot supply.
        // Fall back to the always-on canonical-confirmed-balance path otherwise
        // so the RPC stays answerable without `zinder-explorer`.
        if let Some(proxy) = self
            .explorer_proxy
            .as_ref()
            .filter(|proxy| proxy.is_ready())
        {
            return proxy
                .forward(request, |mut client, request| async move {
                    client.transparent_address_balance(request).await
                })
                .await;
        }

        let network = self.server_info_network()?;
        let inner = request.into_inner();
        let at_epoch = inner
            .at_epoch
            .map(|message| {
                chain_epoch_from_message(message)
                    .map_err(|error| Status::invalid_argument(error.to_string()))
            })
            .transpose()?;
        transparent_address_confirmed_balance_response(
            &self.query_api,
            inner.addresses,
            network,
            at_epoch,
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
    }

    async fn server_info(
        &self,
        _request: Request<wallet::ServerInfoRequest>,
    ) -> Result<Response<wallet::ServerInfoResponse>, Status> {
        // Two coexisting capabilities advertise the same RPC under different
        // semantics:
        //   * `wallet.address.transparent_balance_v1` is always-on. It signals
        //     the canonical-confirmed-balance compute path that
        //     `WalletQuery.TransparentAddressBalance` answers when the explorer
        //     plane is unavailable.
        //   * `explorer.transparent_address.balance_v1` coexists when the
        //     explorer plane is configured and ready. It signals that the same
        //     response additionally carries the live mempool overlay in
        //     `unconfirmed_delta_zat`.
        let mut wallet_info = build_wallet_server_info(&self.server_info);
        let Some(common) = wallet_info.common.as_mut() else {
            return Err(Status::internal(
                "build_wallet_server_info must populate ops.ServerInfo",
            ));
        };
        let federated_capability = self
            .explorer_proxy
            .as_ref()
            .filter(|proxy| proxy.is_ready())
            .map(|proxy| proxy.capability().to_owned());
        if let Some(capability) = federated_capability {
            if !common.capabilities.contains(&capability) {
                common.capabilities.push(capability);
            }
        } else {
            common
                .capabilities
                .retain(|advertised| advertised != EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1);
        }
        if !common
            .capabilities
            .iter()
            .any(|advertised| advertised == WALLET_ADDRESS_TRANSPARENT_BALANCE_V1)
        {
            common
                .capabilities
                .push(WALLET_ADDRESS_TRANSPARENT_BALANCE_V1.to_owned());
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

fn transparent_address_utxos_request_from_message(
    request: wallet::TransparentAddressUtxosRequest,
    network: Network,
) -> Result<TransparentAddressUtxosRequest, Status> {
    let address_script_hash = address_lookup_to_script_hash(request.address, network)
        .map_err(|error| status_from_query_error(&error))?;
    let max_entries =
        optional_max_entries_from_u32(request.max_entries, DEFAULT_MAX_TRANSPARENT_ADDRESS_UTXOS);
    let from_cursor = if request.from_cursor.is_empty() {
        None
    } else {
        Some(StreamCursorTokenV1::from_bytes(request.from_cursor))
    };

    Ok(TransparentAddressUtxosRequest {
        address_script_hash,
        start_height: BlockHeight::new(request.start_height),
        max_entries,
        from_cursor,
    })
}

const DEFAULT_MAX_TRANSPARENT_ADDRESS_UTXOS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

fn optional_max_entries_from_u32(requested: Option<u32>, cap: NonZeroU32) -> NonZeroU32 {
    requested.map_or(cap, |max_entries| max_entries_from_u32(max_entries, cap))
}

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

async fn proxy_chain_events(
    endpoint: String,
    bearer_token: Option<BearerToken>,
    request: Request<wallet::ChainEventsRequest>,
) -> Result<Response<ChainEventsStream>, Status> {
    let mut client = connect_authenticated_proxy(&endpoint, bearer_token.as_ref()).await?;
    let response = client.chain_events(request).await?;

    Ok(Response::new(Box::pin(response.into_inner())))
}

async fn proxy_mempool_snapshot(
    endpoint: String,
    bearer_token: Option<BearerToken>,
    request: Request<wallet::MempoolSnapshotRequest>,
) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
    let mut client = connect_authenticated_proxy(&endpoint, bearer_token.as_ref()).await?;
    client.mempool_snapshot(request).await
}

async fn proxy_mempool_events(
    endpoint: String,
    bearer_token: Option<BearerToken>,
    request: Request<wallet::MempoolEventsRequest>,
) -> Result<Response<MempoolEventsStream>, Status> {
    let mut client = connect_authenticated_proxy(&endpoint, bearer_token.as_ref()).await?;
    let response = client.mempool_events(request).await?;
    Ok(Response::new(Box::pin(response.into_inner())))
}

async fn connect_authenticated_proxy(
    endpoint: &str,
    bearer_token: Option<&BearerToken>,
) -> Result<AuthenticatedIngestControlClient, Status> {
    let channel = connect_authenticated_channel(endpoint, bearer_token)
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    Ok(IngestControlClient::new(channel))
}

fn transaction_id_from_request(transaction_id_bytes: &[u8]) -> Result<TransactionId, Status> {
    let bytes: [u8; 32] = transaction_id_bytes
        .try_into()
        .map_err(|_| Status::invalid_argument("transaction_id must be 32 bytes"))?;
    Ok(TransactionId::from_bytes(bytes))
}

/// Rejects the coinbase sentinel outpoint with `INVALID_ARGUMENT` and a
/// `BadRequest`-shaped diagnostic naming the offending request index.
///
fn reject_coinbase_sentinels(outpoints: &[wallet::OutPoint]) -> Result<(), Status> {
    let sentinel = TransparentOutPoint::COINBASE_SENTINEL;
    let sentinel_transaction_id = encode_internal_transaction_id(sentinel.transaction_id);
    for (request_index, outpoint) in outpoints.iter().enumerate() {
        if outpoint.transaction_id.as_slice() == sentinel_transaction_id.as_slice()
            && outpoint.output_index == sentinel.output_index
        {
            return Err(Status::invalid_argument(format!(
                "outpoints[{request_index}] is the coinbase sentinel \
                 (transaction_id == [0u8; 32], output_index == 0xFFFFFFFF); \
                 filter coinbase inputs at the request boundary",
            )));
        }
    }
    Ok(())
}

/// Translates a wire outpoint list into typed `TransparentOutPoint`s,
/// silently truncating to [`MAX_TRANSPARENT_PREVOUTS_PER_REQUEST`].
fn transparent_outpoints_from_request(
    mut outpoints: Vec<wallet::OutPoint>,
) -> Result<Vec<TransparentOutPoint>, Status> {
    outpoints.truncate(MAX_TRANSPARENT_PREVOUTS_PER_REQUEST);
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

fn chain_epoch_from_request(
    at_epoch: Option<wallet::ChainEpoch>,
) -> Result<Option<ChainEpoch>, Status> {
    at_epoch
        .map(|message| {
            chain_epoch_from_message(message)
                .map_err(|error| Status::invalid_argument(error.to_string()))
        })
        .transpose()
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
        wallet::block_selector::Selector::Hash(hash_bytes) => {
            let bytes: [u8; 32] = hash_bytes
                .try_into()
                .map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
            Ok(BlockSelector::Hash(BlockHash::from_bytes(bytes)))
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
