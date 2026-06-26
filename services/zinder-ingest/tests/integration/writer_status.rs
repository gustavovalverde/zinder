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
use zinder_runtime::MAX_DECODING_MESSAGE_BYTES;
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
    let response_chain_epoch = response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
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
    let oversized_cursor = vec![0u8; MAX_DECODING_MESSAGE_BYTES + 1];
    let outcome = client
        .chain_events(ChainEventsRequest {
            from_cursor: oversized_cursor,
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
