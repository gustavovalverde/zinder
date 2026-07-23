#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;
use std::{
    convert::Infallible,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use arc_swap::ArcSwap;
use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::{
    Request, Response, Status,
    body::Body as TonicBody,
    codegen::{Body, BoxFuture, Service, StdError, http},
    server::{NamedService, ServerStreamingService},
    transport::Server,
};
use zinder_client::{
    BlockHeight, BlockHeightRange, Capability, CapabilityDescriptor, ChainEvent, ChainIndex,
    ConsensusBranchId, EndpointBackedIndex, EventStreamStart, IndexerError, Network,
    NetworkUpgradeActivation, NetworkUpgradeActivations, RawTransactionBytes, RemoteChainIndex,
    RemoteOpenOptions, RetryPolicy, TransactionBroadcastOutcome, TransactionId,
};
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_proto::v1::wallet;
use zinder_query::{
    ServerInfoSettings, WalletCapabilityProfile, WalletQuery, WalletQueryGrpcAdapter,
    WalletServingQuery, WalletServingReadPair,
};
use zinder_testkit::{
    ChainFixture, MockTransactionBroadcaster, StoreFixture, WalletServingStoreFixture,
    sample_regtest_upgrade_activations,
};

#[tokio::test]
async fn remote_chain_index_round_trips_chain_index_calls_over_grpc() -> eyre::Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let transaction_id = TransactionId::from_bytes([0x66; 32]);
    let broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            transaction_broadcast_enabled: true,
            ..ServerInfoSettings::default()
        },
    );
    let endpoint = spawn_wallet_query(grpc_adapter).await?;
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;

    let server_info = chain_index.server_info().await?;
    let current_epoch = chain_index.current_epoch().await?;
    let compact_block = chain_index
        .compact_block_at(BlockHeight::new(1), None)
        .await?;
    let mut compact_blocks = chain_index
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;
    let mut compact_block_count = 0;
    while let Some(compact_block_result) = compact_blocks.next().await {
        compact_block_result?;
        compact_block_count += 1;
    }
    let broadcast_outcome = chain_index
        .broadcast_transaction(RawTransactionBytes::new([0x01, 0x02]))
        .await?;
    let mut events = chain_index
        .chain_events(EventStreamStart::EarliestRetained)
        .await?;
    let first_event = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await?
        .ok_or_else(|| eyre!("chain-events stream closed before first event"))??;

    let server_common = server_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("server_info missing common ops.ServerInfo"))?;
    assert_eq!(
        server_common.network,
        encode_zinder_native_chain_name(Network::ZcashRegtest)
    );
    assert!(server_info.supports(Capability::Broadcast));
    assert_eq!(current_epoch.visible_tip_height, BlockHeight::new(2));
    assert_eq!(compact_block.height(), BlockHeight::new(1));
    assert_eq!(compact_block_count, 2);
    assert_eq!(
        broadcast_outcome,
        TransactionBroadcastOutcome::Accepted(zinder_client::BroadcastAccepted { transaction_id })
    );
    assert!(matches!(
        first_event.event,
        ChainEvent::ChainCommitted { committed }
            if committed.block_range.start == BlockHeight::new(1)
    ));
    assert!(!first_event.cursor.as_bytes().is_empty());

    Ok(())
}

#[tokio::test]
async fn remote_chain_index_returns_typed_network_upgrade_activations() -> eyre::Result<()> {
    let expected = sample_regtest_upgrade_activations();
    let activations = Arc::new(expected.clone());
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let mut store_fixture =
        WalletServingStoreFixture::from_chain(&chain_fixture, activations.as_ref())?;
    let (canonical_reader, wallet_reader) = store_fixture.take_readers()?;
    let serving_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(canonical_reader),
        Arc::new(wallet_reader),
    )?);
    let serving_pair_slot = Arc::new(ArcSwap::from(serving_pair));
    let wallet_query =
        WalletServingQuery::from_serving_pair_slot(serving_pair_slot, (), activations);
    let endpoint = spawn_wallet_query(WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            capability_profile: WalletCapabilityProfile::ExactPair,
            ..ServerInfoSettings::default()
        },
    ))
    .await?;
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;

    let activations = chain_index.network_upgrade_activations().await?;
    let erased: &dyn ChainIndex = &chain_index;
    let erased_activations = erased.network_upgrade_activations().await?;

    assert_eq!(activations, expected);
    assert_eq!(erased_activations, expected);
    let first = activations
        .activations()
        .first()
        .ok_or_else(|| eyre!("fixture activation table must not be empty"))?;
    let expected_first = NetworkUpgradeActivation {
        branch_id: ConsensusBranchId::new(0x5BA8_1B19),
        activation_height: BlockHeight::new(1),
        name: "Overwinter".to_owned(),
    };
    assert_eq!(first, &expected_first);
    Ok(())
}

#[tokio::test]
async fn remote_chain_index_maps_released_serving_query_stale_pin_to_refresh() -> eyre::Result<()> {
    let mut delayed_activations = sample_regtest_upgrade_activations().activations().to_vec();
    for activation in &mut delayed_activations {
        if matches!(activation.name.as_str(), "Sapling" | "NU5" | "NU6.3") {
            activation.activation_height = BlockHeight::new(100);
        }
    }
    let activations = Arc::new(NetworkUpgradeActivations::new(
        Network::ZcashRegtest,
        delayed_activations,
    )?);
    let initial_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let replacement_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let mut initial_store =
        WalletServingStoreFixture::from_chain(&initial_chain, activations.as_ref())?;
    let (initial_canonical, initial_wallet) = initial_store.take_readers()?;
    let initial_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(initial_canonical),
        Arc::new(initial_wallet),
    )?);
    let initial_epoch_id = initial_pair.canonical_fence().chain_epoch_id();
    let serving_slot = Arc::new(ArcSwap::from(initial_pair));
    let query = WalletServingQuery::from_serving_pair_slot(
        Arc::clone(&serving_slot),
        (),
        Arc::clone(&activations),
    );
    let endpoint = spawn_wallet_query(WalletQueryGrpcAdapter::new(
        query,
        ServerInfoSettings::default(),
    ))
    .await?;
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;

    let snapshot = chain_index.snapshot().await?;
    assert_eq!(snapshot.chain_epoch().id, initial_epoch_id);
    let initial_tip = snapshot.visible_tip_block().await?;

    let mut replacement_store = WalletServingStoreFixture::from_chain_after_live_append(
        &replacement_chain,
        activations.as_ref(),
    )?;
    let (replacement_canonical, replacement_wallet) = replacement_store.take_readers()?;
    let replacement_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(replacement_canonical),
        Arc::new(replacement_wallet),
    )?);
    let replacement_epoch_id = replacement_pair.canonical_fence().chain_epoch_id();
    assert!(replacement_epoch_id.value() > initial_epoch_id.value());
    serving_slot.store(replacement_pair);
    assert_eq!(chain_index.current_epoch().await?.id, replacement_epoch_id);

    let stale_read = snapshot.visible_tip_block().await;
    let Err(error) = stale_read else {
        return Err(eyre!(
            "the released serving query served a stale snapshot after pair publication"
        ));
    };

    assert!(matches!(error, IndexerError::ChainEpochPinUnavailable));
    assert_eq!(error.retry_policy(), RetryPolicy::RefreshChainEpoch);
    let stale_artifact_read = snapshot.compact_block_at(initial_tip.height).await;
    let Err(artifact_error) = stale_artifact_read else {
        return Err(eyre!(
            "the released serving query served a stale snapshot artifact"
        ));
    };
    assert!(matches!(
        artifact_error,
        IndexerError::ChainEpochPinUnavailable
    ));
    assert_eq!(
        artifact_error.retry_policy(),
        RetryPolicy::RefreshChainEpoch
    );
    assert_ne!(chain_index.visible_tip_block(None).await?, initial_tip);
    Ok(())
}

#[tokio::test]
async fn remote_chain_event_stream_rejects_duplicate_sequence_from_the_wire() -> eyre::Result<()> {
    let chain_epoch = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(2)
        .chain_epoch_artifacts(zinder_client::ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture must be non-empty"))?
        .chain_epoch;
    let event = wallet::ChainEventEnvelope {
        cursor: vec![1],
        event_sequence: 1,
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(zinder_proto::wire::chain_epoch_message(chain_epoch)),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }),
        event: Some(wallet::chain_event_envelope::Event::ChainCommitted(
            wallet::ChainCommitted {
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(zinder_proto::wire::chain_epoch_message(chain_epoch)),
                    start_height: 1,
                    end_height: 2,
                }),
            },
        )),
    };
    let endpoint = spawn_malformed_chain_event_service(vec![event.clone(), event]).await?;
    let client = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;
    let mut stream = client
        .chain_events(EventStreamStart::EarliestRetained)
        .await?;

    assert!(matches!(stream.next().await, Some(Ok(_))));
    assert!(matches!(
        stream.next().await,
        Some(Err(zinder_client::IndexerError::MalformedResponse {
            field: "event_sequence",
            ..
        }))
    ));
    Ok(())
}

#[tokio::test]
async fn remote_chain_event_stream_rejects_a_nonadjacent_repeated_cursor_from_the_wire()
-> eyre::Result<()> {
    let chain_epoch = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(2)
        .chain_epoch_artifacts(zinder_client::ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture must be non-empty"))?
        .chain_epoch;
    let mut first = chain_event_message(chain_epoch, 1, vec![1]);
    let second = chain_event_message(chain_epoch, 2, vec![2]);
    let mut repeated = chain_event_message(chain_epoch, 3, vec![1]);
    // Keep the event payloads independent so the regression specifically
    // exercises cursor identity across nonadjacent stream items.
    first.event_sequence = 1;
    repeated.event_sequence = 3;

    let endpoint = spawn_malformed_chain_event_service(vec![first, second, repeated]).await?;
    let client = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;
    let mut stream = client
        .chain_events(EventStreamStart::EarliestRetained)
        .await?;

    assert!(matches!(stream.next().await, Some(Ok(_))));
    assert!(matches!(stream.next().await, Some(Ok(_))));
    assert!(matches!(
        stream.next().await,
        Some(Err(zinder_client::IndexerError::MalformedResponse {
            field: "cursor",
            ..
        }))
    ));
    Ok(())
}

fn chain_event_message(
    chain_epoch: zinder_client::ChainEpoch,
    event_sequence: u64,
    cursor: Vec<u8>,
) -> wallet::ChainEventEnvelope {
    wallet::ChainEventEnvelope {
        cursor,
        event_sequence,
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(zinder_proto::wire::chain_epoch_message(chain_epoch)),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }),
        event: Some(wallet::chain_event_envelope::Event::ChainCommitted(
            wallet::ChainCommitted {
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(zinder_proto::wire::chain_epoch_message(chain_epoch)),
                    start_height: 1,
                    end_height: 2,
                }),
            },
        )),
    }
}

#[derive(Clone)]
struct MalformedChainEventService {
    events: Arc<Vec<wallet::ChainEventEnvelope>>,
}

impl NamedService for MalformedChainEventService {
    const NAME: &'static str = "zinder.v1.wallet.WalletQuery";
}

impl<B> Service<http::Request<B>> for MalformedChainEventService
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<TonicBody>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if request.uri().path() != "/zinder.v1.wallet.WalletQuery/ChainEvents" {
            return Box::pin(async move {
                let mut response = http::Response::new(TonicBody::default());
                response.headers_mut().insert(
                    Status::GRPC_STATUS,
                    (tonic::Code::Unimplemented as i32).into(),
                );
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    tonic::metadata::GRPC_CONTENT_TYPE,
                );
                Ok(response)
            });
        }

        let events = Arc::clone(&self.events);
        Box::pin(async move {
            struct ChainEventsMethod(Arc<Vec<wallet::ChainEventEnvelope>>);

            impl ServerStreamingService<wallet::ChainEventsRequest> for ChainEventsMethod {
                type Response = wallet::ChainEventEnvelope;
                type ResponseStream = Pin<
                    Box<dyn tokio_stream::Stream<Item = Result<Self::Response, Status>> + Send>,
                >;
                type Future = BoxFuture<Response<Self::ResponseStream>, Status>;

                fn call(&mut self, _request: Request<wallet::ChainEventsRequest>) -> Self::Future {
                    let events = Arc::clone(&self.0);
                    Box::pin(async move {
                        let stream =
                            tokio_stream::iter(events.as_ref().clone().into_iter().map(Ok));
                        Ok(Response::new(Box::pin(stream) as Self::ResponseStream))
                    })
                }
            }

            let codec = tonic_prost::ProstCodec::default();
            let mut grpc = tonic::server::Grpc::new(codec);
            Ok(grpc
                .server_streaming(ChainEventsMethod(events), request)
                .await)
        })
    }
}

async fn spawn_malformed_chain_event_service(
    events: Vec<wallet::ChainEventEnvelope>,
) -> eyre::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        let _server_result = Server::builder()
            .add_service(MalformedChainEventService {
                events: Arc::new(events),
            })
            .serve_with_incoming(incoming)
            .await;
    });
    Ok(format!("http://{addr}"))
}

async fn spawn_wallet_query<QueryApi>(
    grpc_adapter: WalletQueryGrpcAdapter<QueryApi>,
) -> eyre::Result<String>
where
    WalletQueryGrpcAdapter<QueryApi>: zinder_proto::v1::wallet::wallet_query_server::WalletQuery,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        let _server_result = Server::builder()
            .add_service(grpc_adapter.into_server())
            .serve_with_incoming(incoming)
            .await;
    });

    Ok(format!("http://{addr}"))
}
