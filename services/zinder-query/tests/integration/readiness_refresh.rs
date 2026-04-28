#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{pin::Pin, sync::Arc, time::Duration};

use eyre::eyre;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, transport::Server};
use zinder_core::Network;
use zinder_proto::v1::{
    ingest::{
        WriterStatusRequest, WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    wallet,
};
use zinder_query::{WriterStatusConfig, spawn_readiness_refresh, spawn_secondary_catchup};
use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
use zinder_store::{
    ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore, SecondaryChainStore,
};
use zinder_testkit::StoreFixture;

use crate::common::synthetic_chain_epoch;

const REFRESH_INTERVAL: Duration = Duration::from_millis(20);
const TICK_BUDGET: Duration = Duration::from_millis(500);

#[tokio::test(flavor = "multi_thread")]
async fn refresh_advances_readiness_when_chain_epoch_advances() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let readiness = Readiness::new(ReadinessState::ready(None));
    let cancel = CancellationToken::new();

    let handle = spawn_readiness_refresh(
        store.clone(),
        readiness.clone(),
        REFRESH_INTERVAL,
        cancel.clone(),
    );

    commit_epoch(&store, 1, 1)?;
    wait_for_visible_height(&readiness, Some(1)).await?;

    commit_epoch(&store, 2, 2)?;
    wait_for_visible_height(&readiness, Some(2)).await?;

    cancel.cancel();
    handle.await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_preserves_startup_snapshot_when_store_is_empty() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let readiness = Readiness::new(ReadinessState::ready(Some(42)));
    let cancel = CancellationToken::new();

    let handle =
        spawn_readiness_refresh(store, readiness.clone(), REFRESH_INTERVAL, cancel.clone());

    tokio::time::sleep(REFRESH_INTERVAL * 4).await;

    let report = readiness.report();
    assert!(matches!(report.cause, ReadinessCause::Ready));
    assert_eq!(report.current_height, Some(42));

    cancel.cancel();
    handle.await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn secondary_catchup_refreshes_before_first_interval() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let primary = store_fixture.chain_store().clone();
    commit_epoch(&primary, 1, 1)?;
    let secondary_path = store_fixture
        .tempdir_path()
        .join("query-secondary-immediate");
    let secondary = SecondaryChainStore::open(
        store_fixture.tempdir_path(),
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;

    let readiness = Readiness::new(ReadinessState::syncing(None, None, None));
    let cancel = CancellationToken::new();
    let handle = spawn_secondary_catchup(
        secondary,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: Duration::from_secs(60),
            lag_threshold_chain_epochs: 1,
            writer_status: None,
        },
        cancel.clone(),
    );

    wait_for_ready_height(&readiness, Some(1)).await?;

    cancel.cancel();
    handle.await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn secondary_catchup_marks_replica_lagging_from_writer_status() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let primary = store_fixture.chain_store().clone();
    commit_epoch(&primary, 1, 1)?;
    let secondary_path = store_fixture.tempdir_path().join("query-secondary");
    let secondary = SecondaryChainStore::open(
        store_fixture.tempdir_path(),
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    secondary.try_catch_up()?;

    let writer_status = SharedWriterStatus::new(WriterStatusResponse {
        network_name: "zcash-regtest".to_owned(),
        latest_writer_chain_epoch_id: Some(7),
        latest_writer_tip_height: Some(7),
        latest_writer_finalized_height: Some(7),
    });
    let (writer_status_addr, writer_status_cancel, writer_status_handle) =
        spawn_writer_status_server(writer_status.clone()).await?;
    let readiness = Readiness::new(ReadinessState::ready(Some(1)));
    let cancel = CancellationToken::new();
    let handle = spawn_secondary_catchup(
        secondary,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: REFRESH_INTERVAL,
            lag_threshold_chain_epochs: 1,
            writer_status: Some(WriterStatusConfig {
                endpoint: format!("http://{writer_status_addr}"),
                network: Network::ZcashRegtest,
            }),
        },
        cancel.clone(),
    );

    wait_for_replica_lagging(&readiness, 6).await?;
    writer_status.set(WriterStatusResponse {
        network_name: "zcash-regtest".to_owned(),
        latest_writer_chain_epoch_id: Some(1),
        latest_writer_tip_height: Some(1),
        latest_writer_finalized_height: Some(1),
    });
    wait_for_ready_height(&readiness, Some(1)).await?;

    cancel.cancel();
    handle.await?;
    writer_status_cancel.cancel();
    writer_status_handle.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn secondary_catchup_marks_writer_status_unavailable_when_method_is_unimplemented()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let primary = store_fixture.chain_store().clone();
    commit_epoch(&primary, 1, 1)?;
    let secondary_path = store_fixture
        .tempdir_path()
        .join("wrong-writer-status-secondary");
    let secondary = SecondaryChainStore::open(
        store_fixture.tempdir_path(),
        &secondary_path,
        ChainStoreOptions::for_local_tests(),
    )?;
    secondary.try_catch_up()?;

    let (wrong_service_addr, wrong_service_cancel, wrong_service_handle) =
        spawn_wrong_service_server().await?;
    let readiness = Readiness::new(ReadinessState::ready(Some(1)));
    let cancel = CancellationToken::new();
    let handle = spawn_secondary_catchup(
        secondary,
        readiness.clone(),
        zinder_query::SecondaryCatchupOptions {
            interval: REFRESH_INTERVAL,
            lag_threshold_chain_epochs: 1,
            writer_status: Some(WriterStatusConfig {
                endpoint: format!("http://{wrong_service_addr}"),
                network: Network::ZcashRegtest,
            }),
        },
        cancel.clone(),
    );

    wait_for_writer_status_unavailable(&readiness).await?;

    cancel.cancel();
    handle.await?;
    wrong_service_cancel.cancel();
    wrong_service_handle.await??;

    Ok(())
}

fn commit_epoch(store: &PrimaryChainStore, chain_epoch_id: u64, height: u32) -> eyre::Result<()> {
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(chain_epoch_id, height);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    Ok(())
}

async fn wait_for_visible_height(readiness: &Readiness, expected: Option<u32>) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        if readiness.report().current_height == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre!(
                "readiness did not reach height {expected:?} within {TICK_BUDGET:?}; last report was {:?}",
                readiness.report()
            ));
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

async fn wait_for_ready_height(readiness: &Readiness, expected: Option<u32>) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        let report = readiness.report();
        if matches!(report.cause, ReadinessCause::Ready) && report.current_height == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre!(
                "readiness did not become ready at height {expected:?} within {TICK_BUDGET:?}; last report was {report:?}"
            ));
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

async fn wait_for_replica_lagging(readiness: &Readiness, expected_lag: u64) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        let report = readiness.report();
        if matches!(
            report.cause,
            ReadinessCause::ReplicaLagging { lag_chain_epochs } if lag_chain_epochs == expected_lag
        ) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre!(
                "readiness did not become replica_lagging({expected_lag}) within {TICK_BUDGET:?}; last report was {report:?}"
            ));
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

async fn wait_for_writer_status_unavailable(readiness: &Readiness) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + TICK_BUDGET;
    loop {
        let report = readiness.report();
        if matches!(report.cause, ReadinessCause::WriterStatusUnavailable) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre!(
                "readiness did not become writer_status_unavailable within {TICK_BUDGET:?}; last report was {report:?}"
            ));
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

#[derive(Clone)]
struct SharedWriterStatus {
    response: Arc<Mutex<WriterStatusResponse>>,
}

impl SharedWriterStatus {
    fn new(response: WriterStatusResponse) -> Self {
        Self {
            response: Arc::new(Mutex::new(response)),
        }
    }

    fn set(&self, response: WriterStatusResponse) {
        *self.response.lock() = response;
    }
}

#[tonic::async_trait]
impl IngestControl for SharedWriterStatus {
    type ChainEventsStream =
        Pin<Box<dyn Stream<Item = Result<wallet::ChainEventEnvelope, Status>> + Send>>;

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        Ok(Response::new(self.response.lock().clone()))
    }

    async fn chain_events(
        &self,
        _request: Request<wallet::ChainEventsRequest>,
    ) -> Result<Response<Self::ChainEventsStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::empty())))
    }
}

async fn spawn_writer_status_server(
    writer_status: SharedWriterStatus,
) -> eyre::Result<(
    std::net::SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(IngestControlServer::new(writer_status))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    Ok((listen_addr, cancel, server))
}

async fn spawn_wrong_service_server() -> eyre::Result<(
    std::net::SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let (_health_reporter, health_service) = tonic_health::server::health_reporter();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(health_service)
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    Ok((listen_addr, cancel, server))
}
