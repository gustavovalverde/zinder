#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    fs,
    net::SocketAddr,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::process::Command as TokioCommand;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Endpoint};
use zinder_core::Network;
use zinder_proto::{
    capabilities::{
        EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_OVERVIEW_SNAPSHOT_V1, EXPLORER_TRANSACTION_DETAIL_V4,
        EXPLORER_TRANSACTION_FEES_V1,
    },
    v1::explorer::{ServerInfoRequest, explorer_query_client::ExplorerQueryClient},
};
use zinder_query::{
    ServerInfoSettings, WalletCapabilityProfile, WalletQuery, WalletQueryGrpcAdapter,
};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

#[test]
fn print_config_omits_legacy_storage_when_no_paths_are_configured() -> eyre::Result<()> {
    let output = zinder_explorer_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--listen-addr",
            "127.0.0.1:9068",
            "--wallet-query-endpoint",
            "http://127.0.0.1:9067",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(!stdout.contains("[storage]"), "{stdout}");
    assert!(!stdout.contains("/var/lib/zinder/store"), "{stdout}");
    assert!(
        !stdout.contains("/var/lib/zinder/explorer-secondary"),
        "{stdout}"
    );
    Ok(())
}

#[test]
fn print_config_accepts_explorer_bearer_token_path() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("materialized-view-print-config-store");
    let secondary_path = tempdir
        .path()
        .join("materialized-view-print-config-secondary");
    let token_path = tempdir.path().join("materialized-view-explorer.token");
    fs::write(&token_path, "expected-token\n")?;

    let output = zinder_explorer_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
            "--secondary-path",
            path_str(&secondary_path)?,
            "--listen-addr",
            "127.0.0.1:9068",
            "--bearer-token-path",
            path_str(&token_path)?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("[explorer]"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "bearer_token_path = \"{}\"",
            path_str(&token_path)?
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("expected-token"), "{stdout}");
    assert!(!stderr.contains("expected-token"), "{stderr}");

    Ok(())
}

#[test]
fn invalid_explorer_bearer_token_path_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("materialized-view-invalid-token-store");
    let secondary_path = tempdir
        .path()
        .join("materialized-view-invalid-token-secondary");
    let token_path = tempdir.path().join("materialized-view-empty.token");
    fs::write(&token_path, "\n")?;

    let output = zinder_explorer_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
            "--secondary-path",
            path_str(&secondary_path)?,
            "--listen-addr",
            "127.0.0.1:9068",
            "--bearer-token-path",
            path_str(&token_path)?,
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("bearer token must not be empty"),
        "{stderr}"
    );

    Ok(())
}

#[tokio::test]
async fn runtime_starts_stateless_after_wallet_contract_admission_and_omits_store_capabilities()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let (wallet_query_addr, wallet_query_handle) =
        spawn_admitted_wallet_query(&store_fixture).await?;
    let tempdir = tempdir()?;
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let listen_addr = unused_loopback_addr()?;
    fs::write(
        &config_path,
        stateless_explorer_config_toml(wallet_query_addr, listen_addr),
    )?;

    let mut child = zinder_explorer_tokio_command()
        .args(["--config", path_str(&config_path)?])
        .spawn()?;

    let channel = await_explorer_channel(listen_addr).await;
    if channel.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let channel = channel?;
    let mut client = ExplorerQueryClient::new(channel);
    let server_info = tokio::time::timeout(
        Duration::from_secs(5),
        client.server_info(ServerInfoRequest {}),
    )
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    wallet_query_handle.abort();
    let _ = wallet_query_handle.await;

    let explorer_info = server_info
        .map_err(|_| eyre::eyre!("explorer ServerInfo request timed out"))??
        .into_inner()
        .info
        .ok_or_else(|| eyre::eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .ok_or_else(|| eyre::eyre!("server info missing common envelope"))?;

    assert!(
        common
            .capabilities
            .iter()
            .any(|capability| capability == EXPLORER_TRANSACTION_DETAIL_V4),
        "{:?}",
        common.capabilities
    );
    assert!(
        !common
            .capabilities
            .iter()
            .any(|capability| capability == EXPLORER_BLOCK_SUMMARY_V1),
        "{:?}",
        common.capabilities
    );
    assert!(
        !common
            .capabilities
            .iter()
            .any(|capability| capability == EXPLORER_OVERVIEW_SNAPSHOT_V1),
        "{:?}",
        common.capabilities
    );
    assert!(
        !common
            .capabilities
            .iter()
            .any(|capability| capability == EXPLORER_TRANSACTION_FEES_V1),
        "{:?}",
        common.capabilities
    );

    Ok(())
}

async fn spawn_admitted_wallet_query(
    store_fixture: &StoreFixture,
) -> eyre::Result<(
    SocketAddr,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            network: "zcash-regtest".to_owned(),
            transaction_blobs_retained: true,
            transparent_outpoint_spend_available: true,
            capability_profile: WalletCapabilityProfile::ExactPair,
            ..ServerInfoSettings::default()
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    Ok((addr, handle))
}

fn zinder_explorer_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
    command.env_clear();
    command
}

fn zinder_explorer_tokio_command() -> TokioCommand {
    let mut command = TokioCommand::new(env!("CARGO_BIN_EXE_zinder-explorer"));
    command.env_clear();
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn stateless_explorer_config_toml(
    wallet_query_addr: SocketAddr,
    listen_addr: SocketAddr,
) -> String {
    format!(
        r#"[network]
name = "zcash-regtest"

[explorer]
listen_addr = "{listen_addr}"
wallet_query_endpoint = "http://{wallet_query_addr}"

[ops]
listen_addr = ""
"#,
    )
}

fn unused_loopback_addr() -> eyre::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

async fn await_explorer_channel(listen_addr: SocketAddr) -> eyre::Result<Channel> {
    let endpoint = format!("http://{listen_addr}");
    for _ in 0..100 {
        match Endpoint::from_shared(endpoint.clone())?.connect().await {
            Ok(channel) => return Ok(channel),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    Err(eyre::eyre!(
        "explorer did not accept gRPC connections at {listen_addr}"
    ))
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
