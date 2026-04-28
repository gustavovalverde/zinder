#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::Result;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use zinder_core::Network;
use zinder_ingest::IngestControlGrpcAdapter;
use zinder_proto::v1::{
    ingest::{WriterStatusRequest, ingest_control_client::IngestControlClient},
    wallet::{ChainEventStreamFamily, ChainEventsRequest},
};
use zinder_testkit::StoreFixture;

#[tokio::test(flavor = "multi_thread")]
async fn writer_status_reports_latest_primary_chain_epoch() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let expected_chain_epoch = store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let adapter =
        IngestControlGrpcAdapter::new(Network::ZcashRegtest, store_fixture.chain_store().clone());
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
    assert_eq!(
        response.latest_writer_chain_epoch_id,
        Some(expected_chain_epoch.id.value())
    );
    assert_eq!(
        response.latest_writer_tip_height,
        Some(expected_chain_epoch.tip_height.value())
    );
    assert_eq!(
        response.latest_writer_finalized_height,
        Some(expected_chain_epoch.finalized_height.value())
    );

    cancel.cancel();
    server.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_streams_chain_events_from_primary_store() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let adapter =
        IngestControlGrpcAdapter::new(Network::ZcashRegtest, store_fixture.chain_store().clone());
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
            from_cursor: Vec::new(),
            family: ChainEventStreamFamily::Tip as i32,
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
