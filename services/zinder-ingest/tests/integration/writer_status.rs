#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{borrow::Cow, sync::Arc, time::Duration};

use eyre::Result;
use prost::Message as _;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use zinder_core::Network;
use zinder_ingest::{IngestControlGrpcAdapter, RocksDbMaterializedViewStatusReader};
use zinder_materialized_views::{
    MaterializedViewStore, MaterializedViewStoreOptions, ProjectionPreset,
};
use zinder_proto::v1::{
    ingest::{
        WriterPhase, WriterStatusRequest, ingest_control_client::IngestControlClient,
        ingest_control_server::IngestControl as IngestControlService,
    },
    wallet::{self, ChainEventStreamFamily, ChainEventsRequest},
};
use zinder_runtime::{
    IngestPhase, MAX_DECODING_MESSAGE_BYTES, Readiness, ReadinessState, UpstreamHealth,
    UpstreamNotReadyDetail,
};
use zinder_store::{EventStreamStartPosition, RocksDbResourceBudget, event_stream_start_message};
use zinder_testkit::StoreFixture;

fn bulk_catchup_readiness(current_height: u32, upstream_height: u32) -> Readiness {
    Readiness::new(
        ReadinessState::syncing(
            Some(u64::from(upstream_height.saturating_sub(current_height))),
            Some(current_height),
            Some(upstream_height),
        )
        .with_phase(IngestPhase::BulkCatchup),
    )
}

fn open_materialized_view_store(store_fixture: &StoreFixture) -> Result<MaterializedViewStore> {
    Ok(MaterializedViewStore::open_with_projection_preset(
        MaterializedViewStore::path_for_canonical(store_fixture.tempdir_path()),
        ProjectionPreset::Explorer,
        MaterializedViewStoreOptions {
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            ..MaterializedViewStoreOptions::default()
        },
    )?)
}

fn materialized_view_store_with_status(
    store_fixture: &StoreFixture,
    materialized_view_status: wallet::MaterializedViewStatus,
) -> Result<MaterializedViewStore> {
    let materialized_view_store = open_materialized_view_store(store_fixture)?;
    materialized_view_store
        .put_materialized_view_status(&materialized_view_status.encode_to_vec())?;
    Ok(materialized_view_store)
}

#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "the integration test constructs and verifies one complete writer status response"
)]
async fn writer_status_reports_latest_primary_chain_epoch() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let expected_chain_epoch = store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let expected_visible_height = expected_chain_epoch.visible_tip_height.value();
    let upstream_height = expected_visible_height
        .checked_add(2)
        .ok_or_else(|| eyre::eyre!("fixture height cannot represent a two-block gap"))?;
    let readiness = bulk_catchup_readiness(expected_visible_height, upstream_height);
    let expected_materialized_view_status = wallet::MaterializedViewStatus {
        health: wallet::MaterializedViewHealth::Live.into(),
        indexed_height: expected_visible_height,
        lag_blocks: 0,
        observed_at_millis: 1_752_588_000_000,
    };
    let materialized_view_store =
        materialized_view_store_with_status(&store_fixture, expected_materialized_view_status)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        readiness,
    )
    .with_materialized_view_status_reader(Arc::new(RocksDbMaterializedViewStatusReader::new(
        materialized_view_store,
    )));
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let response = client
        .writer_status(WriterStatusRequest {})
        .await?
        .into_inner();

    assert_eq!(response.network_name, "zcash-regtest");
    assert_eq!(response.phase(), WriterPhase::BulkCatchup);
    assert_eq!(response.gap_blocks, Some(2));
    let chain_view = response
        .chain_view
        .ok_or_else(|| eyre::eyre!("writer status response missing chain view"))?;
    assert_eq!(
        chain_view.upstream_tip,
        Some(wallet::UpstreamTip {
            committed_height: Some(upstream_height),
            estimated_height: None,
        })
    );
    assert_eq!(
        chain_view.materialized_views,
        Some(expected_materialized_view_status)
    );
    let response_chain_epoch = chain_view
        .chain_epoch
        .ok_or_else(|| eyre::eyre!("writer status response missing chain epoch"))?;
    assert_eq!(
        response_chain_epoch.chain_epoch_id,
        expected_chain_epoch.id.value()
    );
    assert_eq!(
        response_chain_epoch
            .visible_tip
            .ok_or_else(|| eyre::eyre!("writer status response missing visible tip"))?
            .height,
        expected_chain_epoch.visible_tip_height.value()
    );
    assert_eq!(
        response_chain_epoch
            .settled_tip
            .ok_or_else(|| eyre::eyre!("writer status response missing settled tip"))?
            .height,
        expected_chain_epoch.settled_tip_height.value()
    );

    cancel.cancel();
    server.await??;

    Ok(())
}

#[tokio::test]
async fn writer_status_preserves_following_tip_ready_gap() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let visible_height = store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?
        .visible_tip_height
        .value();
    let upstream_height = visible_height
        .checked_add(5)
        .ok_or_else(|| eyre::eyre!("fixture height cannot represent a five-block gap"))?;
    let readiness = Readiness::new(
        ReadinessState::ready_with_target(Some(visible_height), Some(upstream_height))
            .with_phase(IngestPhase::FollowingTip),
    );
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        readiness,
    );

    let response =
        IngestControlService::writer_status(&adapter, tonic::Request::new(WriterStatusRequest {}))
            .await?
            .into_inner();

    assert_eq!(response.phase(), WriterPhase::FollowingTip);
    assert_eq!(response.gap_blocks, Some(5));
    assert_eq!(
        response
            .chain_view
            .and_then(|chain_view| chain_view.upstream_tip),
        Some(wallet::UpstreamTip {
            committed_height: Some(upstream_height),
            estimated_height: None,
        })
    );
    Ok(())
}

#[tokio::test]
async fn writer_status_reports_upstream_not_ready_snapshot() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let visible_height = store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?
        .visible_tip_height
        .value();
    let committed_height = visible_height
        .checked_add(4)
        .ok_or_else(|| eyre::eyre!("fixture height cannot represent a four-block gap"))?;
    let estimated_height = committed_height
        .checked_add(6)
        .ok_or_else(|| eyre::eyre!("fixture height cannot represent an estimated tip"))?;
    let readiness = Readiness::new(
        ReadinessState::upstream_not_ready_with_detail(
            UpstreamNotReadyDetail {
                upstream_committed_height: Some(committed_height),
                upstream_estimated_height: Some(estimated_height),
                upstream_verification_progress: Some(0.75),
                upstream_health: UpstreamHealth {
                    source: "zebra_ready_endpoint",
                    reason: Cow::Borrowed("syncing"),
                },
            },
            Some(visible_height),
        )
        .with_phase(IngestPhase::AwaitingUpstream),
    );
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        readiness,
    );

    let response =
        IngestControlService::writer_status(&adapter, tonic::Request::new(WriterStatusRequest {}))
            .await?
            .into_inner();

    assert_eq!(response.phase(), WriterPhase::AwaitingUpstream);
    assert_eq!(response.gap_blocks, Some(4));
    assert_eq!(
        response.upstream_not_ready,
        Some(zinder_proto::v1::ops::UpstreamNotReadyDetail {
            upstream_committed_height: Some(committed_height),
            upstream_estimated_height: Some(estimated_height),
            upstream_verification_progress: Some(0.75),
            upstream_health_source: "zebra_ready_endpoint".to_owned(),
            upstream_health_reason: "syncing".to_owned(),
        })
    );
    assert_eq!(
        response
            .chain_view
            .and_then(|chain_view| chain_view.upstream_tip),
        Some(wallet::UpstreamTip {
            committed_height: Some(committed_height),
            estimated_height: Some(estimated_height),
        })
    );

    Ok(())
}

#[tokio::test]
async fn malformed_materialized_view_status_does_not_hide_canonical_writer_status() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let materialized_view_store = open_materialized_view_store(&store_fixture)?;
    materialized_view_store.put_materialized_view_status(&[0xff])?;
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        Readiness::default(),
    )
    .with_materialized_view_status_reader(Arc::new(RocksDbMaterializedViewStatusReader::new(
        materialized_view_store,
    )));

    let response =
        IngestControlService::writer_status(&adapter, tonic::Request::new(WriterStatusRequest {}))
            .await?
            .into_inner();

    let chain_view = response
        .chain_view
        .ok_or_else(|| eyre::eyre!("writer status response missing canonical chain view"))?;
    assert!(chain_view.chain_epoch.is_some());
    assert_eq!(chain_view.materialized_views, None);
    Ok(())
}

#[tokio::test]
async fn semantically_invalid_materialized_view_status_does_not_hide_canonical_writer_status()
-> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let materialized_view_store = materialized_view_store_with_status(
        &store_fixture,
        wallet::MaterializedViewStatus {
            health: 99,
            indexed_height: 1,
            lag_blocks: 0,
            observed_at_millis: 1_752_588_000_000,
        },
    )?;
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        Readiness::default(),
    )
    .with_materialized_view_status_reader(Arc::new(RocksDbMaterializedViewStatusReader::new(
        materialized_view_store,
    )));

    let response =
        IngestControlService::writer_status(&adapter, tonic::Request::new(WriterStatusRequest {}))
            .await?
            .into_inner();

    let chain_view = response
        .chain_view
        .ok_or_else(|| eyre::eyre!("writer status response missing canonical chain view"))?;
    assert!(chain_view.chain_epoch.is_some());
    assert_eq!(chain_view.materialized_views, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_streams_chain_events_from_primary_store() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    );
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::EarliestRetained,
            )),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();
    let first_event = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await?
        .ok_or_else(|| eyre::eyre!("chain event stream closed before first event"))??;

    assert_eq!(first_event.event_sequence, 1);
    assert!(!first_event.cursor.is_empty());

    drop(stream);
    drop(client);
    cancel.cancel();
    server.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_rejects_request_frame_exceeding_decoding_cap() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store_fixture.chain_store().clone(),
        zinder_runtime::Readiness::default(),
    );
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let oversized_cursor = vec![0u8; MAX_DECODING_MESSAGE_BYTES + 1];
    let outcome = client
        .chain_events(ChainEventsRequest {
            start: Some(wallet::EventStreamStart {
                position: Some(wallet::event_stream_start::Position::AfterCursor(
                    oversized_cursor,
                )),
            }),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await;

    let status = outcome.err().ok_or_else(|| {
        eyre::eyre!("server accepted a request frame larger than the decoding cap")
    })?;
    assert_eq!(status.code(), tonic::Code::OutOfRange);

    cancel.cancel();
    server.await??;

    Ok(())
}
