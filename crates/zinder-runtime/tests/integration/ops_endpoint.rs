use std::{net::SocketAddr, sync::Arc};

use eyre::Result;
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::net::TcpListener;
use zinder_runtime::{OpsServer, OpsServerError, Readiness, ReadinessState, spawn_ops_endpoint};

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
#[allow(
    clippy::too_many_lines,
    reason = "end-to-end test covers healthz, readyz, and metrics in a single fixture"
)]
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
            advertised_capabilities: Arc::from([
                zinder_proto::capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1,
            ]),
        },
        readiness.clone(),
    )
    .await?;

    let (status, healthz_value) = get_json(listen_addr, "/healthz").await?;
    assert_eq!(status, 200);
    assert_eq!(healthz_value["version"], "0.0.0");
    assert_eq!(
        healthz_value["git_commit"],
        zinder_runtime::BUILD_GIT_COMMIT
    );

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
        metrics_text.contains(&format!(
            "git_commit=\"{}\"",
            zinder_runtime::BUILD_GIT_COMMIT
        )),
        "{metrics_text}"
    );
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

    server_handle.shutdown().await?;

    Ok(())
}

#[tokio::test]
async fn spawn_ops_endpoint_fails_before_returning_when_port_is_occupied() -> Result<()> {
    let occupied_listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = occupied_listener.local_addr()?;

    let spawn_result = spawn_ops_endpoint(
        listen_addr,
        OpsServer {
            service_name: "zinder-test",
            service_version: "0.0.0",
            network_name: "zcash-regtest",
            advertised_capabilities: Arc::from([]),
        },
        Readiness::default(),
    )
    .await;

    let Err(OpsServerError::Bind {
        listen_addr: failed_addr,
        ..
    }) = spawn_result
    else {
        return Err(eyre::eyre!(
            "occupied operational listen address must fail before a handle is returned"
        ));
    };
    assert_eq!(failed_addr, listen_addr);
    Ok(())
}
