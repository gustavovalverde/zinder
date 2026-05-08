#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! Smoke test that boots an `ExplorerQueryGrpcAdapter` against an in-process
//! tonic server and verifies `ServerInfo` returns the expected capability set.

use std::time::Duration;

use eyre::{Result, eyre};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Channel;
use zinder_derive::{
    DERIVE_EXPLORER_READY_CAPABILITY, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
use zinder_proto::v1::explorer::{ServerInfoRequest, explorer_query_client::ExplorerQueryClient};

#[tokio::test]
async fn explorer_query_server_info_advertises_ready_capability() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
        network: "zcash-regtest".to_owned(),
    });
    let server_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let channel = await_with_retry(server_addr).await?;
    let mut client = ExplorerQueryClient::new(channel);
    let response = client.server_info(ServerInfoRequest {}).await?.into_inner();
    let capabilities = response
        .capabilities
        .ok_or_else(|| eyre!("server info response missing capabilities envelope"))?;

    assert_eq!(capabilities.vendor, "Zinder");
    assert_eq!(capabilities.network, "zcash-regtest");
    assert!(
        capabilities
            .capabilities
            .iter()
            .any(|advertised| { advertised == DERIVE_EXPLORER_READY_CAPABILITY })
    );

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

async fn await_with_retry(addr: std::net::SocketAddr) -> Result<Channel> {
    let endpoint = format!("http://{addr}");
    for _ in 0..20 {
        if let Ok(channel) = Channel::from_shared(endpoint.clone())?.connect().await {
            return Ok(channel);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre!(
        "explorer query gRPC server did not accept connections"
    ))
}
