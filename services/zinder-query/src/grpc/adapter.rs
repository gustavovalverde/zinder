//! Native gRPC adapter for wallet query reads.

use std::{num::NonZeroU32, pin::Pin};

use tokio::sync::mpsc;
use tokio_stream::{self as stream, Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootIndex, SubtreeRootRange, TransactionId,
};
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, wallet_query_server},
};
use zinder_runtime::{AuthenticatedChannel, BearerToken, connect_authenticated_channel};
use zinder_store::{
    ChainEventStreamFamily, StreamCursorTokenV1, chain_epoch_from_message, run_chain_event_stream,
};

type AuthenticatedIngestControlClient = IngestControlClient<AuthenticatedChannel>;

use crate::WalletQueryApi;

use super::native::{
    ServerInfoSettings, broadcast_transaction_response, build_chain_epoch_message,
    build_compact_block_message, build_server_capabilities_message, chain_events_response,
    compact_block_response, latest_block_response, latest_tree_state_response,
    subtree_roots_response, transaction_response, tree_state_response,
};
use super::status_from_query_error;

type WalletGrpcStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = WalletGrpcStream<wallet::ChainEventEnvelope>;
type MempoolEventsStream = WalletGrpcStream<wallet::MempoolEventEnvelope>;

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
    type CompactBlockRangeStream = WalletGrpcStream<wallet::CompactBlockRangeChunk>;
    type ChainEventsStream = ChainEventsStream;
    type MempoolEventsStream = MempoolEventsStream;

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

    async fn transaction(
        &self,
        request: Request<wallet::TransactionRequest>,
    ) -> Result<Response<wallet::TransactionResponse>, Status> {
        let request = request.into_inner();
        let transaction_id = transaction_id_from_request(&request.transaction_id)?;

        transaction_response(
            &self.query_api,
            transaction_id,
            chain_epoch_from_request(request.at_epoch)?,
        )
        .await
        .map(Response::new)
        .map_err(|error| status_from_query_error(&error))
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
            .compact_block_range_at_epoch(block_range, at_epoch)
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
        let from_cursor = cursor_from_request(request.from_cursor);
        let family = chain_event_stream_family_from_request(request.family)?;
        let query_api = self.query_api.clone();
        let (event_sender, event_receiver) = mpsc::channel(16);
        tokio::spawn(run_chain_event_stream(
            from_cursor,
            move |cursor| {
                let query_api = query_api.clone();
                async move {
                    chain_events_response(&query_api, cursor, family)
                        .await
                        .map_err(|error| status_from_query_error(&error))
                }
            },
            event_sender,
        ));

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

    async fn server_info(
        &self,
        _request: Request<wallet::ServerInfoRequest>,
    ) -> Result<Response<wallet::ServerInfoResponse>, Status> {
        Ok(Response::new(wallet::ServerInfoResponse {
            capabilities: Some(build_server_capabilities_message(&self.server_info)),
        }))
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

fn cursor_from_request(cursor_bytes: Vec<u8>) -> Option<StreamCursorTokenV1> {
    if cursor_bytes.is_empty() {
        None
    } else {
        Some(StreamCursorTokenV1::from_bytes(cursor_bytes))
    }
}

fn chain_event_stream_family_from_request(family: i32) -> Result<ChainEventStreamFamily, Status> {
    match wallet::ChainEventStreamFamily::try_from(family) {
        Ok(wallet::ChainEventStreamFamily::Tip) => Ok(ChainEventStreamFamily::Tip),
        Ok(wallet::ChainEventStreamFamily::Finalized) => Ok(ChainEventStreamFamily::Finalized),
        Err(_) => Err(Status::invalid_argument(
            "chain-event stream family is unknown",
        )),
    }
}
