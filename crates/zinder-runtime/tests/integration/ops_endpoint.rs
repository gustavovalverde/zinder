use std::{net::SocketAddr, time::Duration};

use eyre::Result;
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::net::TcpListener;
use zinder_runtime::{OpsServer, Readiness, ReadinessCause, ReadinessState, spawn_ops_endpoint};

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

    let client = Client::builder(TokioExecutor::new()).build_http::<String>();

    let healthz = client
        .get(format!("http://{listen_addr}/healthz").parse()?)
        .await?;
    assert_eq!(healthz.status(), 200);

    let readyz = client
        .get(format!("http://{listen_addr}/readyz").parse()?)
        .await?;
    assert_eq!(readyz.status(), 200);
    let readyz_body = readyz.into_body().collect().await?.to_bytes();
    let readyz_value: serde_json::Value = serde_json::from_slice(&readyz_body)?;
    assert_eq!(readyz_value["status"], "ready");
    assert_eq!(readyz_value["cause"], "ready");
    assert_eq!(readyz_value["current_height"], 7);

    readiness.set(ReadinessState::not_ready(ReadinessCause::NodeUnavailable));
    let readyz = client
        .get(format!("http://{listen_addr}/readyz").parse()?)
        .await?;
    assert_eq!(readyz.status(), 503);
    let readyz_body = readyz.into_body().collect().await?.to_bytes();
    let readyz_value: serde_json::Value = serde_json::from_slice(&readyz_body)?;
    assert_eq!(readyz_value["status"], "not_ready");
    assert_eq!(readyz_value["cause"], "node_unavailable");

    let metrics = client
        .get(format!("http://{listen_addr}/metrics").parse()?)
        .await?;
    assert_eq!(metrics.status(), 200);
    let metrics_body = metrics.into_body().collect().await?.to_bytes();
    let metrics_text = String::from_utf8(metrics_body.to_vec())?;
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

    server_handle.shutdown().await;

    Ok(())
}
