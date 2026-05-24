#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;
use std::{net::SocketAddr, num::NonZeroU32, pin::Pin};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{Stream, StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, transport::Server};
use tonic_types::StatusExt;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{
    ChainEpoch, ChainTipMetadata, CompactBlockArtifact, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootHash, SubtreeRootIndex, TransactionId, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_proto::capabilities::{
    EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1, WALLET_BROADCAST_TRANSACTION_V1,
    WALLET_EVENTS_CHAIN_V1,
};
use zinder_proto::v1::{
    explorer::explorer_query_client::ExplorerQueryClient,
    ingest::{
        WriterPhase, WriterStatusRequest, WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    wallet::{self, wallet_query_server::WalletQuery as WalletQueryService},
};
use zinder_query::{
    DeriveProxy, DeriveProxyConfig, DeriveReadinessGauge, ServerInfoSettings, WalletQuery,
    WalletQueryGrpcAdapter, WalletQueryOptions,
};
use zinder_store::{ChainEpochArtifacts, PrimaryChainStore};
use zinder_testkit::{
    MockTransactionBroadcaster, StoreFixture, sample_regtest_upgrade_activations,
};

use crate::common::{compact_block_with_tree_sizes, synthetic_chain_epoch};

#[tokio::test]
async fn native_grpc_service_returns_wallet_reads_from_stored_artifacts() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_artifacts = commit_wallet_artifacts(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let grpc_responses = read_wallet_grpc_responses(&grpc_adapter).await?;

    assert_wallet_grpc_response_epochs(&grpc_responses, stored_artifacts.chain_epoch.id.value());
    assert_eq!(
        grpc_responses
            .latest_block
            .latest_block
            .ok_or_else(|| eyre!("missing latest block"))?
            .height,
        1
    );
    assert_eq!(
        grpc_responses
            .compact_block_range
            .first()
            .ok_or_else(|| eyre!("missing compact block"))?
            .compact_block
            .as_ref()
            .ok_or_else(|| eyre!("missing compact block"))?
            .payload_bytes,
        stored_artifacts.compact_block.payload_bytes
    );
    assert_eq!(
        grpc_responses.explicit_tree_state.payload_bytes,
        stored_artifacts.tree_state.payload_bytes
    );
    assert_eq!(
        grpc_responses.latest_tree_state_checkpoint.payload_bytes,
        stored_artifacts.tree_state.payload_bytes
    );
    assert_eq!(
        grpc_responses
            .subtree_roots
            .subtree_roots
            .first()
            .ok_or_else(|| eyre!("missing subtree root"))?
            .root_hash,
        stored_artifacts.subtree_root.root_hash.as_bytes()
    );

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_checks_range_limit_before_opening_reader() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let wallet_query = WalletQuery::with_options(
        store,
        (),
        Arc::new(sample_regtest_upgrade_activations()),
        WalletQueryOptions {
            max_compact_block_range: NonZeroU32::new(1)
                .ok_or_else(|| eyre!("invalid range limit"))?,
        },
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let status = match WalletQueryService::compact_block_range(
        &grpc_adapter,
        Request::new(wallet::CompactBlockRangeRequest {
            start_height: 1,
            end_height: 2,
            at_epoch: None,
        }),
    )
    .await
    {
        Ok(_response) => return Err(eyre!("expected range error, got success")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_maps_missing_artifacts_to_not_found() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 65_536, 0);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let tree_state_status = match WalletQueryService::tree_state_checkpoint(
        &grpc_adapter,
        Request::new(wallet::TreeStateCheckpointRequest {
            max_height: 1,
            at_epoch: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected tree-state error, got {response:?}")),
        Err(status) => status,
    };
    let subtree_roots_status = match WalletQueryService::subtree_roots(
        &grpc_adapter,
        Request::new(wallet::SubtreeRootsRequest {
            shielded_protocol: wallet::ShieldedProtocol::Sapling as i32,
            start_index: 0,
            max_entries: 1,
            at_epoch: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected subtree-root error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(tree_state_status.code(), Code::NotFound);
    assert_eq!(subtree_roots_status.code(), Code::NotFound);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_returns_not_found_when_transaction_missing() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_wallet_artifacts(&store)?;
    let requested_transaction_id = TransactionId::from_bytes([0x45; 32]);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let status = match WalletQueryService::transaction(
        &grpc_adapter,
        Request::new(wallet::TransactionRequest {
            transaction_id: requested_transaction_id.as_bytes().to_vec(),
            at_epoch: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected transaction error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);
    assert!(
        status.message().contains("not visible"),
        "expected wire message to describe visibility, got {:?}",
        status.message()
    );

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_broadcasts_raw_transactions() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let transaction_id = TransactionId::from_bytes([0x9a; 32]);
    let broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster.clone(),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::broadcast_transaction(
        &grpc_adapter,
        Request::new(wallet::BroadcastTransactionRequest {
            raw_transaction: vec![0x01, 0x02],
        }),
    )
    .await?
    .into_inner();

    assert!(matches!(
        response.outcome,
        Some(wallet::broadcast_transaction_response::Outcome::Accepted(accepted))
            if accepted.transaction_id == transaction_id.as_bytes().to_vec()
    ));
    assert_eq!(broadcaster.call_count(), 1);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_streams_chain_events_from_the_store() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_artifacts = commit_wallet_artifacts(&store)?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            from_cursor: Vec::new(),
            family: wallet::ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let first_event = event_stream
        .next()
        .await
        .ok_or_else(|| eyre!("chain-events stream closed before first event"))??;

    assert_eq!(first_event.event_sequence, 1);
    assert_eq!(
        first_event
            .chain_epoch
            .ok_or_else(|| eyre!("missing chain epoch"))?
            .chain_epoch_id,
        stored_artifacts.chain_epoch.id.value()
    );
    assert!(matches!(
        first_event.event,
        Some(wallet::chain_event_envelope::Event::ChainCommitted(committed))
            if committed.committed.as_ref().is_some_and(|inner| {
                inner.start_height == 1 && inner.end_height == 1
            })
    ));
    assert!(!first_event.cursor.is_empty());

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_expires_pruned_chain_event_cursors() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let mut first_cursor = Vec::new();

    for height in 1..=3 {
        let (chain_epoch, block, compact_block) = synthetic_chain_epoch(u64::from(height), height);
        let commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
        if height == 1 {
            first_cursor = commit.event_envelope.cursor.as_bytes().to_vec();
        }
    }
    store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_300_003))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            from_cursor: first_cursor,
            family: wallet::ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let status = match event_stream
        .next()
        .await
        .ok_or_else(|| eyre!("chain-events stream closed before cursor error"))?
    {
        Ok(event) => return Err(eyre!("expected cursor expiry, got event {event:?}")),
        Err(status) => status,
    };
    let details = status.get_error_details();
    let violation = details
        .precondition_failure()
        .and_then(|failure| failure.violations.first())
        .cloned();

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(matches!(
        violation,
        Some(violation)
            if violation.r#type == "CHAIN_EVENT_CURSOR_EXPIRED"
                && violation.subject == "chain_event:1"
                && violation.description.contains('3')
    ));

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_honors_request_epoch_pin() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let response = WalletQueryService::latest_block(
        &grpc_adapter,
        Request::new(wallet::LatestBlockRequest {
            at_epoch: Some(chain_epoch_message(first_epoch)),
        }),
    )
    .await?
    .into_inner();
    let response_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let latest_block = response
        .latest_block
        .ok_or_else(|| eyre!("missing latest block"))?;

    assert_eq!(response_epoch.chain_epoch_id, first_epoch.id.value());
    assert_eq!(latest_block.height, 1);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_proxies_chain_events_to_ingest_control() -> eyre::Result<()> {
    let proxied_event = wallet::ChainEventEnvelope {
        cursor: vec![0x01, 0x02],
        event_sequence: 77,
        chain_epoch: Some(wallet::ChainEpoch {
            chain_epoch_id: 11,
            network_name: "zcash-regtest".to_owned(),
            tip_height: 5,
            tip_hash: vec![0x05; 32],
            finalized_height: 4,
            finalized_hash: vec![0x04; 32],
            artifact_schema_version: 1,
            created_at_millis: 123,
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
        }),
        finalized_height: 4,
        event: Some(wallet::chain_event_envelope::Event::ChainCommitted(
            wallet::ChainCommitted {
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(wallet::ChainEpoch {
                        chain_epoch_id: 11,
                        network_name: "zcash-regtest".to_owned(),
                        tip_height: 5,
                        tip_hash: vec![0x05; 32],
                        finalized_height: 4,
                        finalized_hash: vec![0x04; 32],
                        artifact_schema_version: 1,
                        created_at_millis: 123,
                        sapling_commitment_tree_size: 0,
                        orchard_commitment_tree_size: 0,
                    }),
                    start_height: 5,
                    end_height: 5,
                }),
            },
        )),
    };
    let (ingest_control_addr, cancel, server_handle) =
        spawn_ingest_control_server(StaticIngestControl::new(proxied_event.clone())).await?;
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        ServerInfoSettings::default(),
        format!("http://{ingest_control_addr}"),
    );

    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            from_cursor: Vec::new(),
            family: wallet::ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let first_event = event_stream
        .next()
        .await
        .ok_or_else(|| eyre!("chain-events proxy stream closed before first event"))??;

    assert_eq!(first_event, proxied_event);

    drop(event_stream);
    cancel.cancel();
    server_handle.await??;

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_advertises_only_configured_capabilities() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let read_only_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let read_only_adapter =
        WalletQueryGrpcAdapter::new(read_only_query, ServerInfoSettings::default());
    let read_only_info = WalletQueryService::server_info(
        &read_only_adapter,
        Request::new(wallet::ServerInfoRequest {}),
    )
    .await?
    .into_inner()
    .info
    .ok_or_else(|| eyre!("missing read-only info"))?;

    assert!(has_capability(&read_only_info, WALLET_EVENTS_CHAIN_V1));
    assert!(!has_capability(
        &read_only_info,
        WALLET_BROADCAST_TRANSACTION_V1
    ));

    let broadcaster = MockTransactionBroadcaster::accepted(TransactionId::from_bytes([0x33; 32]));
    let broadcast_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let broadcast_adapter = WalletQueryGrpcAdapter::new(
        broadcast_query,
        ServerInfoSettings {
            transaction_broadcast_enabled: true,
            ..ServerInfoSettings::default()
        },
    );
    let broadcast_info = WalletQueryService::server_info(
        &broadcast_adapter,
        Request::new(wallet::ServerInfoRequest {}),
    )
    .await?
    .into_inner()
    .info
    .ok_or_else(|| eyre!("missing broadcast info"))?;

    assert!(has_capability(
        &broadcast_info,
        WALLET_BROADCAST_TRANSACTION_V1
    ));

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_gates_federated_derive_capability_on_readiness() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let readiness = DeriveReadinessGauge::new();
    let explorer_proxy = DeriveProxy::with_readiness(
        DeriveProxyConfig {
            endpoint: "http://127.0.0.1:0".to_owned(),
            bearer_token: None,
            capability: EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
        },
        readiness.clone(),
        ExplorerQueryClient::new,
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default())
        .with_explorer_proxy(explorer_proxy);

    let initial_capabilities = wallet_server_info(&grpc_adapter).await?;
    assert!(
        !has_capability(
            &initial_capabilities,
            EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1
        ),
        "explorer capability must be omitted before the readiness probe succeeds"
    );

    readiness.mark_ready();
    let ready_capabilities = wallet_server_info(&grpc_adapter).await?;
    assert!(has_capability(
        &ready_capabilities,
        EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1
    ));

    readiness.mark_not_ready();
    let not_ready_capabilities = wallet_server_info(&grpc_adapter).await?;
    assert!(
        !has_capability(
            &not_ready_capabilities,
            EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1
        ),
        "explorer capability must be removed after the readiness probe fails"
    );

    Ok(())
}

struct StoredWalletArtifacts {
    chain_epoch: ChainEpoch,
    compact_block: CompactBlockArtifact,
    tree_state: TreeStateArtifact,
    subtree_root: SubtreeRootArtifact,
}

struct WalletGrpcResponses {
    latest_block: wallet::LatestBlockResponse,
    compact_block_range: Vec<wallet::CompactBlockRangeChunk>,
    explicit_tree_state: wallet::TreeStateResponse,
    latest_tree_state_checkpoint: wallet::TreeStateResponse,
    subtree_roots: wallet::SubtreeRootsResponse,
}

fn commit_wallet_artifacts(store: &PrimaryChainStore) -> eyre::Result<StoredWalletArtifacts> {
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 65_536, 0);
    let tree_state =
        TreeStateArtifact::new(block.height, block.block_hash, b"tree-state-1".to_vec());
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x71; 32]),
        block.height,
        block.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block.clone()])
            .with_tree_states(vec![tree_state.clone()])
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    Ok(StoredWalletArtifacts {
        chain_epoch,
        compact_block,
        tree_state,
        subtree_root,
    })
}

async fn read_wallet_grpc_responses(
    grpc_adapter: &WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore>>,
) -> Result<WalletGrpcResponses, tonic::Status> {
    let latest_block = WalletQueryService::latest_block(
        grpc_adapter,
        Request::new(wallet::LatestBlockRequest { at_epoch: None }),
    )
    .await?
    .into_inner();
    let mut compact_block_stream = WalletQueryService::compact_block_range(
        grpc_adapter,
        Request::new(wallet::CompactBlockRangeRequest {
            start_height: 1,
            end_height: 1,
            at_epoch: None,
        }),
    )
    .await?
    .into_inner();
    let mut compact_block_range = Vec::new();
    while let Some(compact_block_chunk) = compact_block_stream.next().await {
        compact_block_range.push(compact_block_chunk?);
    }
    let explicit_tree_state = WalletQueryService::tree_state_checkpoint(
        grpc_adapter,
        Request::new(wallet::TreeStateCheckpointRequest {
            max_height: 1,
            at_epoch: None,
        }),
    )
    .await?
    .into_inner();
    let latest_tree_state_checkpoint = WalletQueryService::latest_tree_state_checkpoint(
        grpc_adapter,
        Request::new(wallet::LatestTreeStateCheckpointRequest { at_epoch: None }),
    )
    .await?
    .into_inner();
    let subtree_roots = WalletQueryService::subtree_roots(
        grpc_adapter,
        Request::new(wallet::SubtreeRootsRequest {
            shielded_protocol: wallet::ShieldedProtocol::Sapling as i32,
            start_index: 0,
            max_entries: 1,
            at_epoch: None,
        }),
    )
    .await?
    .into_inner();

    Ok(WalletGrpcResponses {
        latest_block,
        compact_block_range,
        explicit_tree_state,
        latest_tree_state_checkpoint,
        subtree_roots,
    })
}

fn assert_wallet_grpc_response_epochs(responses: &WalletGrpcResponses, chain_epoch_id: u64) {
    assert_eq!(
        response_chain_epoch_id(&responses.latest_block),
        chain_epoch_id
    );
    for compact_block_chunk in &responses.compact_block_range {
        assert_eq!(response_chain_epoch_id(compact_block_chunk), chain_epoch_id);
    }
    assert_eq!(
        response_chain_epoch_id(&responses.explicit_tree_state),
        chain_epoch_id
    );
    assert_eq!(
        response_chain_epoch_id(&responses.latest_tree_state_checkpoint),
        chain_epoch_id
    );
    assert_eq!(
        response_chain_epoch_id(&responses.subtree_roots),
        chain_epoch_id
    );
}

trait HasChainEpoch {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch>;
}

impl HasChainEpoch for wallet::LatestBlockResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasChainEpoch for wallet::CompactBlockRangeChunk {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasChainEpoch for wallet::TreeStateResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

impl HasChainEpoch for wallet::SubtreeRootsResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_epoch.as_ref()
    }
}

fn response_chain_epoch_id(response: &impl HasChainEpoch) -> u64 {
    response
        .chain_epoch()
        .map_or(0, |chain_epoch| chain_epoch.chain_epoch_id)
}

fn has_capability(wallet_info: &wallet::WalletServerInfo, capability: &str) -> bool {
    wallet_info.common.as_ref().is_some_and(|common| {
        common
            .capabilities
            .iter()
            .any(|advertised| advertised == capability)
    })
}

async fn wallet_server_info(
    grpc_adapter: &WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore>>,
) -> eyre::Result<wallet::WalletServerInfo> {
    WalletQueryService::server_info(grpc_adapter, Request::new(wallet::ServerInfoRequest {}))
        .await?
        .into_inner()
        .info
        .ok_or_else(|| eyre!("missing wallet server info"))
}

fn chain_epoch_message(chain_epoch: ChainEpoch) -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: chain_epoch.id.value(),
        network_name: encode_zinder_native_chain_name(chain_epoch.network).to_owned(),
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

type StaticChainEventsStream =
    Pin<Box<dyn Stream<Item = Result<wallet::ChainEventEnvelope, Status>> + Send>>;

#[derive(Clone)]
struct StaticIngestControl {
    event: wallet::ChainEventEnvelope,
}

impl StaticIngestControl {
    fn new(event: wallet::ChainEventEnvelope) -> Self {
        Self { event }
    }
}

#[tonic::async_trait]
impl IngestControl for StaticIngestControl {
    type ChainEventsStream = StaticChainEventsStream;
    type MempoolEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<wallet::MempoolEventEnvelope, Status>> + Send>,
    >;

    async fn server_info(
        &self,
        _request: Request<zinder_proto::v1::ingest::ServerInfoRequest>,
    ) -> Result<Response<zinder_proto::v1::ingest::ServerInfoResponse>, Status> {
        Ok(Response::new(
            zinder_proto::v1::ingest::ServerInfoResponse { server_info: None },
        ))
    }

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        Ok(Response::new(WriterStatusResponse {
            network_name: "zcash-regtest".to_owned(),
            latest_writer_chain_epoch_id: Some(11),
            latest_writer_tip_height: Some(5),
            latest_writer_finalized_height: Some(4),
            phase: WriterPhase::FollowingTip.into(),
            gap_blocks: Some(0),
            upstream_not_ready: None,
        }))
    }

    async fn chain_events(
        &self,
        _request: Request<wallet::ChainEventsRequest>,
    ) -> Result<Response<Self::ChainEventsStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::iter([Ok(self
            .event
            .clone())]))))
    }

    async fn mempool_snapshot(
        &self,
        _request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub MempoolSnapshot",
        ))
    }

    async fn mempool_events(
        &self,
        _request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub MempoolEvents",
        ))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub TransparentMempoolOutputsByAddress",
        ))
    }

    async fn transparent_mempool_spend_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolSpendByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendByOutpointResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub TransparentMempoolSpendByOutpoint",
        ))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub TransparentMempoolOutputsByOutpoint",
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub ChainValuePoolsAtTip",
        ))
    }
}

async fn spawn_ingest_control_server(
    ingest_control: StaticIngestControl,
) -> eyre::Result<(
    SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(IngestControlServer::new(ingest_control))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    Ok((listen_addr, cancel, server))
}
