use std::{net::SocketAddr, time::Duration};

use eyre::Result;
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::net::TcpListener;
use zinder_runtime::{OpsServer, Readiness, ReadinessState, spawn_ops_endpoint};

async fn get_json(listen_addr: SocketAddr, path: &str) -> Result<(u16, serde_json::Value)> {
    let client = Client::builder(TokioExecutor::new()).build_http::<String>();
    let response = client
        .get(format!("http://{listen_addr}{path}").parse()?)
        .await?;
    let status = response.status().as_u16();
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, serde_json::from_slice(&body)?))
}

async fn get_text(listen_addr: SocketAddr, path: &str) -> Result<(u16, String)> {
    let client = Client::builder(TokioExecutor::new()).build_http::<String>();
    let response = client
        .get(format!("http://{listen_addr}{path}").parse()?)
        .await?;
    let status = response.status().as_u16();
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8(body.to_vec())?))
}

#[tokio::test]
async fn ops_endpoint_serves_health_readiness_and_metrics() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr: SocketAddr = listener.local_addr()?;
    drop(listener);

    let readiness = Readiness::new(ReadinessState::ready(Some(7)));
    let server_handle = spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-test",
            service_version: "0.0.0",
            network_name: "zcash-regtest",
        },
        readiness.clone(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, _healthz_value) = get_json(listen_addr, "/healthz").await?;
    assert_eq!(status, 200);

    let (status, readyz_value) = get_json(listen_addr, "/readyz").await?;
    assert_eq!(status, 200);
    assert_eq!(readyz_value["status"], "ready");
    assert_eq!(readyz_value["cause"], "ready");
    assert_eq!(readyz_value["current_height"], 7);

    readiness.set(ReadinessState::node_unavailable_with_detail(
        zinder_runtime::NodeUnavailableDetail::first_iteration(
            "node_unreachable",
            "synthetic test outage",
        ),
        Some(7),
    ));
    let (status, readyz_value) = get_json(listen_addr, "/readyz").await?;
    assert_eq!(status, 503);
    assert_eq!(readyz_value["status"], "not_ready");
    assert_eq!(
        readyz_value["cause"]["node_unavailable"]["failure_class"],
        "node_unreachable"
    );

    let (status, metrics_text) = get_text(listen_addr, "/metrics").await?;
    assert_eq!(status, 200);
    assert!(
        metrics_text.contains("zinder_build_info{"),
        "{metrics_text}"
    );
    assert!(
        metrics_text.contains("service=\"zinder-test\""),
        "{metrics_text}"
    );
    assert!(metrics_text.contains("version=\"0.0.0\""), "{metrics_text}");
    assert!(
        metrics_text.contains("network=\"zcash-regtest\""),
        "{metrics_text}"
    );
    assert!(
        metrics_text.contains("zinder_readiness_state{"),
        "{metrics_text}"
    );
    assert!(
        metrics_text.contains("cause=\"node_unavailable\""),
        "{metrics_text}"
    );
    assert!(
        metrics_text.contains("zinder_readiness_sync_lag_blocks"),
        "{metrics_text}"
    );
    assert!(
        metrics_text.contains("zinder_readiness_replica_lag_chain_epochs"),
        "{metrics_text}"
    );

    readiness.set(ReadinessState::mempool_cursor_at_risk(50, 60, Some(7)));
    let (status, readyz_value) = get_json(listen_addr, "/readyz").await?;
    assert_eq!(status, 200);
    assert_eq!(readyz_value["status"], "ready");
    assert_eq!(
        readyz_value["cause"]["mempool_cursor_at_risk"]["oldest_retained_age_minutes"],
        50
    );
    assert_eq!(
        readyz_value["cause"]["mempool_cursor_at_risk"]["retention_minutes"],
        60
    );

    server_handle.shutdown().await;

    Ok(())
}
