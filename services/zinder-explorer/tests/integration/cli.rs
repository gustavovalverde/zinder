#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    fmt::Write as _,
    fs,
    net::SocketAddr,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use tempfile::tempdir;
use tokio::process::Command as TokioCommand;
use tonic::transport::{Channel, Endpoint};
use zinder_core::Network;
use zinder_proto::{
    capabilities::{
        EXPLORER_BLOCK_SUMMARY_V2, EXPLORER_OVERVIEW_SNAPSHOT_V1, EXPLORER_TRANSACTION_DETAIL_V4,
        EXPLORER_TRANSACTION_FEES_V1,
    },
    v1::explorer::{ServerInfoRequest, explorer_query_client::ExplorerQueryClient},
};
use zinder_testkit::StoreFixture;

#[test]
fn print_config_accepts_explorer_bearer_token_path() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("materialized-view-print-config-store");
    let secondary_path = tempdir
        .path()
        .join("materialized-view-print-config-secondary");
    let token_path = tempdir.path().join("materialized-view-explorer.token");
    fs::write(&token_path, "expected-token\n")?;

    let (stdout, stderr) =
        run_print_config_with_explorer_bearer_token(&storage_path, &secondary_path, &token_path)?;
    assert_printed_explorer_paths_and_token_redaction(
        &stdout,
        &stderr,
        &storage_path,
        &secondary_path,
        &token_path,
    )?;
    assert_printed_explorer_rocksdb_budget(&stdout)?;
    assert_printed_explorer_omits_inapplicable_storage_and_node_settings(&stdout);
    Ok(())
}

fn run_print_config_with_explorer_bearer_token(
    storage_path: &Path,
    secondary_path: &Path,
    token_path: &Path,
) -> eyre::Result<(String, String)> {
    let output = zinder_explorer_command()
        .envs([
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__BLOCK_CACHE_BYTES",
                "104857600",
            ),
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__MAX_OPEN_FILES",
                "37",
            ),
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__WRITE_BUFFER_BYTES",
                "8388608",
            ),
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__MAX_WRITE_BUFFER_COUNT",
                "3",
            ),
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__MEMTABLE_BUDGET_BYTES",
                "33554432",
            ),
            (
                "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__STATISTICS_LEVEL",
                "full",
            ),
        ])
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
    Ok((stdout, stderr))
}

fn assert_printed_explorer_paths_and_token_redaction(
    stdout: &str,
    stderr: &str,
    storage_path: &Path,
    secondary_path: &Path,
    token_path: &Path,
) -> eyre::Result<()> {
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
    assert!(
        stdout.contains(&format!("path = \"{}\"", path_str(&storage_path)?)),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "secondary_path = \"{}\"",
            path_str(&secondary_path)?
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("[storage.materialized_views.rocksdb]"),
        "{stdout}"
    );
    Ok(())
}

fn assert_printed_explorer_rocksdb_budget(stdout: &str) -> eyre::Result<()> {
    let rendered: toml::Value = toml::from_str(&stdout)?;
    let rocksdb = rendered
        .get("storage")
        .and_then(|storage| storage.get("materialized_views"))
        .and_then(|materialized_views| materialized_views.get("rocksdb"))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| eyre::eyre!("printed config omitted the explorer RocksDB budget"))?;
    assert_eq!(rocksdb.len(), 6, "{rocksdb:?}");
    assert_eq!(
        rocksdb
            .get("block_cache_bytes")
            .and_then(toml::Value::as_integer),
        Some(104_857_600)
    );
    assert_eq!(
        rocksdb
            .get("max_open_files")
            .and_then(toml::Value::as_integer),
        Some(37)
    );
    assert_eq!(
        rocksdb
            .get("write_buffer_bytes")
            .and_then(toml::Value::as_integer),
        Some(8_388_608)
    );
    assert_eq!(
        rocksdb
            .get("max_write_buffer_count")
            .and_then(toml::Value::as_integer),
        Some(3)
    );
    assert_eq!(
        rocksdb
            .get("memtable_budget_bytes")
            .and_then(toml::Value::as_integer),
        Some(33_554_432)
    );
    assert_eq!(
        rocksdb
            .get("statistics_level")
            .and_then(toml::Value::as_str),
        Some("full")
    );
    assert!(!rocksdb.contains_key("max_wal_bytes"), "{rocksdb:?}");
    assert!(!rocksdb.contains_key("max_background_jobs"), "{rocksdb:?}");
    Ok(())
}

fn assert_printed_explorer_omits_inapplicable_storage_and_node_settings(stdout: &str) {
    assert!(!stdout.contains("[node]"), "{stdout}");
    for removed_key in [
        "[storage.canonical]",
        "secondary_catchup_interval_ms",
        "initial_catchup_timeout_ms",
        "secondary_replica_lag_threshold_chain_epochs",
    ] {
        assert!(!stdout.contains(removed_key), "{stdout}");
    }
}

#[test]
fn print_config_renders_only_effective_explorer_node_fields_and_redacts_auth() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("node-print-config-store");
    let secondary_path = tempdir.path().join("node-print-config-secondary");
    let config_path = tempdir.path().join("node-print-config.toml");
    let listen_addr = unused_loopback_addr()?;
    let mut config = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;
    config.push_str(
        r#"
[node]
json_rpc_addr = "http://127.0.0.1:18232"
request_timeout_secs = 7
max_response_bytes = 123456

[node.auth]
method = "basic"
username = "explorer-user"
password = "node-secret"

[node.health]
addr = "http://127.0.0.1:18233/ready"
poll_interval_ms = 2500
verification_progress_floor = 0.95
estimated_gap_floor_blocks = 12
"#,
    );
    fs::write(&config_path, config)?;

    let output = zinder_explorer_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    for expected in [
        "[node]",
        "json_rpc_addr = \"http://127.0.0.1:18232\"",
        "request_timeout_secs = 7",
        "max_response_bytes = 123456",
        "[node.auth]",
        "method = \"basic\"",
        "username = \"explorer-user\"",
        "password = \"[REDACTED]\"",
        "[node.health]",
        "addr = \"http://127.0.0.1:18233/ready\"",
        "poll_interval_ms = 2500",
        "verification_progress_floor = 0.95",
        "estimated_gap_floor_blocks = 12",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(!stdout.contains("node-secret"), "{stdout}");
    assert!(!stderr.contains("node-secret"), "{stderr}");
    assert!(!stdout.contains("indexer_grpc_addr"), "{stdout}");
    assert!(!stdout.contains("broadcast_timeout_secs"), "{stdout}");
    Ok(())
}

#[test]
fn unsupported_explorer_node_fields_are_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("unsupported-node-store");
    let secondary_path = tempdir.path().join("unsupported-node-secondary");
    let listen_addr = unused_loopback_addr()?;
    let base = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;

    for (name, unsupported) in [
        ("indexer", "indexer_grpc_addr = \"http://127.0.0.1:18234\""),
        ("broadcast", "broadcast_timeout_secs = 7"),
    ] {
        let config_path = tempdir.path().join(format!("{name}.toml"));
        fs::write(
            &config_path,
            format!("{base}\n[node]\njson_rpc_addr = \"http://127.0.0.1:18232\"\n{unsupported}\n"),
        )?;
        let output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;

        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("unknown field"), "{stderr}");
    }
    Ok(())
}

#[test]
fn explorer_node_leaf_requires_json_rpc_addr() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("partial-node-store");
    let secondary_path = tempdir.path().join("partial-node-secondary");
    let listen_addr = unused_loopback_addr()?;
    let base = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;

    for (name, partial) in [
        ("timeout", "[node]\nrequest_timeout_secs = 7\n"),
        ("auth", "[node.auth]\nmethod = \"none\"\n"),
        ("health", "[node.health]\npoll_interval_ms = 2500\n"),
    ] {
        let config_path = tempdir.path().join(format!("{name}.toml"));
        fs::write(&config_path, format!("{base}\n{partial}"))?;
        let output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;

        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("missing required configuration field: node.json_rpc_addr"),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn explorer_health_settings_require_health_addr() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("partial-health-store");
    let secondary_path = tempdir.path().join("partial-health-secondary");
    let config_path = tempdir.path().join("partial-health.toml");
    let listen_addr = unused_loopback_addr()?;
    let base = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;
    fs::write(
        &config_path,
        format!(
            "{base}\n[node]\njson_rpc_addr = \"http://127.0.0.1:18232\"\n\n[node.health]\npoll_interval_ms = 2500\n"
        ),
    )?;

    let output = zinder_explorer_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "node.health.addr is required when explorer node.health probe settings are configured"
        ),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn removed_explorer_storage_keys_are_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("removed-storage-key-store");
    let secondary_path = tempdir.path().join("removed-storage-key-secondary");
    let listen_addr = unused_loopback_addr()?;
    let base = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;

    for (name, removed_setting) in [
        ("catchup", "secondary_catchup_interval_ms = 250".to_owned()),
        (
            "canonical-budget",
            "canonical = { rocksdb = { max_open_files = 32 } }".to_owned(),
        ),
    ] {
        let config_path = tempdir.path().join(format!("{name}.toml"));
        fs::write(
            &config_path,
            base.replace("[storage]\n", &format!("[storage]\n{removed_setting}\n")),
        )?;
        let output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;

        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("unknown field"), "{stderr}");
    }
    Ok(())
}

#[test]
fn primary_only_materialized_view_budget_fields_are_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("primary-only-budget-store");
    let secondary_path = tempdir.path().join("primary-only-budget-secondary");
    let listen_addr = unused_loopback_addr()?;
    let base = explorer_config_toml(&storage_path, &secondary_path, listen_addr, None)?;

    for (field, env_var) in [
        (
            "max_wal_bytes",
            "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__MAX_WAL_BYTES",
        ),
        (
            "max_background_jobs",
            "ZINDER_STORAGE__MATERIALIZED_VIEWS__ROCKSDB__MAX_BACKGROUND_JOBS",
        ),
    ] {
        let config_path = tempdir.path().join(format!("{field}.toml"));
        fs::write(
            &config_path,
            format!("{base}\n[storage.materialized_views.rocksdb]\n{field} = 4194304\n"),
        )?;
        let file_output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;

        assert!(!file_output.status.success(), "{file_output:?}");
        let file_stderr = String::from_utf8(file_output.stderr)?;
        assert!(file_stderr.contains("unknown field"), "{file_stderr}");
        assert!(file_stderr.contains(field), "{file_stderr}");

        fs::write(&config_path, &base)?;
        let env_output = zinder_explorer_command()
            .env(env_var, "4194304")
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;

        assert!(!env_output.status.success(), "{env_output:?}");
        let env_stderr = String::from_utf8(env_output.stderr)?;
        assert!(env_stderr.contains("unknown field"), "{env_stderr}");
        assert!(env_stderr.contains(field), "{env_stderr}");
    }
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

#[test]
fn print_config_accepts_wallet_query_bearer_token_path_without_rendering_the_secret()
-> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-query-print-config-store");
    let secondary_path = tempdir.path().join("wallet-query-print-config-secondary");
    let token_path = tempdir.path().join("wallet-query.token");
    fs::write(&token_path, "outbound-secret\n")?;

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
            "--wallet-query-endpoint",
            "http://127.0.0.1:9067",
            "--wallet-query-bearer-token-path",
            path_str(&token_path)?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stdout.contains(&format!(
            "wallet_query_bearer_token_path = \"{}\"",
            path_str(&token_path)?
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("outbound-secret"), "{stdout}");
    assert!(!stderr.contains("outbound-secret"), "{stderr}");
    Ok(())
}

#[test]
fn wallet_query_bearer_token_without_an_endpoint_fails_before_binding_explorer() -> eyre::Result<()>
{
    let tempdir = tempdir()?;
    let token_path = tempdir.path().join("wallet-query.token");
    let listen_addr = unused_loopback_addr()?;
    let listen_addr_text = listen_addr.to_string();
    fs::write(&token_path, "outbound-secret\n")?;

    let output = zinder_explorer_command()
        .args([
            "--network",
            "zcash-regtest",
            "--listen-addr",
            &listen_addr_text,
            "--wallet-query-bearer-token-path",
            path_str(&token_path)?,
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "explorer.wallet_query_bearer_token_path requires \
             explorer.wallet_query_endpoint"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("outbound-secret"), "{stderr}");
    let listener = std::net::TcpListener::bind(listen_addr)?;
    drop(listener);
    Ok(())
}

#[tokio::test]
async fn runtime_starts_without_materialized_view_store_and_omits_materialized_view_capabilities()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let tempdir = tempdir()?;
    let secondary_path = tempdir.path().join("explorer-secondary");
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let listen_addr = unused_loopback_addr()?;
    fs::write(
        &config_path,
        explorer_config_toml(
            store_fixture.tempdir_path(),
            &secondary_path,
            listen_addr,
            None,
        )?,
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

    let explorer_info = server_info
        .map_err(|_| eyre::eyre!("explorer ServerInfo request timed out"))??
        .into_inner()
        .info
        .ok_or_else(|| eyre::eyre!("server info missing info envelope"))?;
    let common = explorer_info
        .common
        .ok_or_else(|| eyre::eyre!("server info missing common envelope"))?;

    assert!(
        !common
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
            .any(|capability| capability == EXPLORER_BLOCK_SUMMARY_V2),
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

#[test]
fn configured_unreachable_wallet_query_endpoint_fails_before_binding_explorer() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let tempdir = tempdir()?;
    let secondary_path = tempdir.path().join("explorer-secondary");
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let listen_addr = unused_loopback_addr()?;
    let unreachable_addr = unused_loopback_addr()?;
    fs::write(
        &config_path,
        explorer_config_toml(
            store_fixture.tempdir_path(),
            &secondary_path,
            listen_addr,
            Some(unreachable_addr),
        )?,
    )?;

    let output = zinder_explorer_command()
        .args(["--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("wallet query endpoint is unreachable"),
        "{stderr}"
    );
    let listener = std::net::TcpListener::bind(listen_addr)?;
    drop(listener);
    Ok(())
}

#[test]
fn configured_unreachable_node_fails_before_binding_explorer() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let tempdir = tempdir()?;
    let secondary_path = tempdir.path().join("explorer-secondary");
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let listen_addr = unused_loopback_addr()?;
    let unreachable_addr = unused_loopback_addr()?;
    let mut config = explorer_config_toml(
        store_fixture.tempdir_path(),
        &secondary_path,
        listen_addr,
        None,
    )?;
    writeln!(
        &mut config,
        "\n[node]\njson_rpc_addr = \"http://{unreachable_addr}\"\nrequest_timeout_secs = 1"
    )?;
    fs::write(&config_path, config)?;

    let output = zinder_explorer_command()
        .args(["--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("failed to build upstream node source"),
        "{stderr}"
    );
    let listener = std::net::TcpListener::bind(listen_addr)?;
    drop(listener);
    Ok(())
}

#[test]
fn occupied_grpc_port_fails_before_operations_endpoint_binds() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let tempdir = tempdir()?;
    let secondary_path = tempdir.path().join("explorer-secondary");
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let occupied_grpc = std::net::TcpListener::bind("127.0.0.1:0")?;
    let listen_addr = occupied_grpc.local_addr()?;
    let ops_addr = unused_loopback_addr()?;
    let config = explorer_config_toml(
        store_fixture.tempdir_path(),
        &secondary_path,
        listen_addr,
        None,
    )?
    .replace(
        "[ops]\nlisten_addr = \"\"",
        &format!("[ops]\nlisten_addr = \"{ops_addr}\""),
    );
    fs::write(&config_path, config)?;

    let output = zinder_explorer_command()
        .args(["--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("failed to bind explorer gRPC listener"),
        "{stderr}"
    );
    let ops_listener = std::net::TcpListener::bind(ops_addr)?;
    drop(ops_listener);
    drop(occupied_grpc);
    Ok(())
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

fn explorer_config_toml(
    storage_path: &Path,
    secondary_path: &Path,
    listen_addr: SocketAddr,
    wallet_query_addr: Option<SocketAddr>,
) -> eyre::Result<String> {
    let wallet_query_endpoint = wallet_query_addr.map_or_else(String::new, |addr| {
        format!("wallet_query_endpoint = \"http://{addr}\"\n")
    });
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[explorer]
listen_addr = "{}"
{}

[ops]
listen_addr = ""
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
        listen_addr,
        wallet_query_endpoint,
    ))
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
