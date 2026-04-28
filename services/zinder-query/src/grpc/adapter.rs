//! Native gRPC adapter for wallet query reads.

use std::{num::NonZeroU32, pin::Pin};

use tokio::sync::mpsc;
use tokio_stream::{self as stream, Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, Network, RawTransactionBytes, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange, TransactionId, UnixTimestampMillis,
};
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, wallet_query_server},
};
use zinder_store::{ChainEventStreamFamily, StreamCursorTokenV1, run_chain_event_stream};

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

/// gRPC adapter for a [`WalletQueryApi`] implementation.
#[derive(Clone, Debug)]
pub struct WalletQueryGrpcAdapter<QueryApi> {
    query_api: QueryApi,
    server_info: ServerInfoSettings,
    chain_events_proxy_endpoint: Option<String>,
}

impl<QueryApi> WalletQueryGrpcAdapter<QueryApi> {
    /// Creates a gRPC adapter over a wallet query API with the deployment's
    /// `ServerCapabilities` descriptor.
    #[must_use]
    pub const fn new(query_api: QueryApi, server_info: ServerInfoSettings) -> Self {
        Self {
            query_api,
            server_info,
            chain_events_proxy_endpoint: None,
        }
    }

    /// Creates a gRPC adapter that proxies `ChainEvents` to the ingest-owned
    /// private control endpoint.
    #[must_use]
    pub fn with_chain_events_proxy(
        query_api: QueryApi,
        server_info: ServerInfoSettings,
        chain_events_proxy_endpoint: String,
    ) -> Self {
        Self {
            query_api,
            server_info,
            chain_events_proxy_endpoint: Some(chain_events_proxy_endpoint),
        }
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
        if let Some(endpoint) = &self.chain_events_proxy_endpoint {
            return proxy_chain_events(endpoint.clone(), request).await;
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
    request: Request<wallet::ChainEventsRequest>,
) -> Result<Response<ChainEventsStream>, Status> {
    let mut client = IngestControlClient::connect(endpoint)
        .await
        .map_err(|error| Status::unavailable(error.to_string()))?;
    let response = client.chain_events(request).await?;

    Ok(Response::new(Box::pin(response.into_inner())))
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
    at_epoch.map(chain_epoch_from_message).transpose()
}

fn chain_epoch_from_message(message: wallet::ChainEpoch) -> Result<ChainEpoch, Status> {
    let Some(network) = Network::from_name(&message.network_name) else {
        return Err(Status::invalid_argument("at_epoch.network_name is unknown"));
    };
    let artifact_schema_version = u16::try_from(message.artifact_schema_version)
        .map_err(|_| Status::invalid_argument("at_epoch.artifact_schema_version exceeds u16"))?;

    Ok(ChainEpoch {
        id: ChainEpochId::new(message.chain_epoch_id),
        network,
        tip_height: BlockHeight::new(message.tip_height),
        tip_hash: block_hash_from_message("at_epoch.tip_hash", message.tip_hash)?,
        finalized_height: BlockHeight::new(message.finalized_height),
        finalized_hash: block_hash_from_message("at_epoch.finalized_hash", message.finalized_hash)?,
        artifact_schema_version: ArtifactSchemaVersion::new(artifact_schema_version),
        created_at: UnixTimestampMillis::new(message.created_at_millis),
        tip_metadata: ChainTipMetadata::new(
            message.sapling_commitment_tree_size,
            message.orchard_commitment_tree_size,
        ),
    })
}

fn block_hash_from_message(field: &'static str, bytes: Vec<u8>) -> Result<BlockHash, Status> {
    let len = bytes.len();
    let bytes = bytes
        .try_into()
        .map_err(|_| Status::invalid_argument(format!("{field} must be 32 bytes, got {len}")))?;
    Ok(BlockHash::from_bytes(bytes))
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
