#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs, net::SocketAddr, path::Path, process::Command};

use tempfile::tempdir;
use zinder_core::Network;
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
            "http://127.0.0.1:1",
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

#[test]
fn print_config_exposes_only_effective_materialized_view_secondary_controls() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let config_path = tempdir.path().join("zinder-explorer.toml");
    fs::write(
        &config_path,
        format!(
            r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[storage.materialized_views.rocksdb]
block_cache_bytes = 4194304
max_open_files = 32
write_buffer_bytes = 4194304
max_write_buffer_count = 3
memtable_budget_bytes = 16777216
statistics_level = "off"

[explorer]
listen_addr = "127.0.0.1:9068"
wallet_query_endpoint = "http://127.0.0.1:1"
"#,
            path_str(&tempdir.path().join("canonical"))?,
            path_str(&tempdir.path().join("secondary"))?,
        ),
    )?;

    let output = zinder_explorer_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    for expected in [
        "block_cache_bytes = 4194304",
        "max_open_files = 32",
        "write_buffer_bytes = 4194304",
        "max_write_buffer_count = 3",
        "memtable_budget_bytes = 16777216",
        "statistics_level = \"off\"",
    ] {
        assert!(stdout.contains(expected), "{stdout}");
    }
    for obsolete in [
        "max_wal_bytes",
        "max_background_jobs",
        "secondary_catchup_interval_ms",
        "initial_catchup_timeout_ms",
        "secondary_replica_lag_threshold_chain_epochs",
        "[storage.canonical]",
    ] {
        assert!(!stdout.contains(obsolete), "{stdout}");
    }
    Ok(())
}

#[test]
fn obsolete_explorer_storage_controls_are_rejected() -> eyre::Result<()> {
    for obsolete_line in [
        "initial_catchup_timeout_ms = 1",
        "secondary_catchup_interval_ms = 1",
        "secondary_replica_lag_threshold_chain_epochs = 1",
        "canonical = {}",
        "materialized_views = { rocksdb = { max_wal_bytes = 1 } }",
        "materialized_views = { rocksdb = { max_background_jobs = 1 } }",
    ] {
        let tempdir = tempdir()?;
        let config_path = tempdir.path().join("zinder-explorer.toml");
        fs::write(
            &config_path,
            format!(
                r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"
{obsolete_line}

[explorer]
listen_addr = "127.0.0.1:9068"
wallet_query_endpoint = "http://127.0.0.1:1"
"#,
                path_str(&tempdir.path().join("canonical"))?,
                path_str(&tempdir.path().join("secondary"))?,
            ),
        )?;
        let output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;
        assert!(!output.status.success(), "accepted {obsolete_line}");
    }
    Ok(())
}

#[test]
fn explorer_node_rejects_orphaned_and_none_auth_credentials() -> eyre::Result<()> {
    for node_section in [
        "request_timeout_secs = 1",
        "json_rpc_addr = \"http://127.0.0.1:8232\"\n[node.auth]\nmethod = \"none\"\nusername = \"ignored\"",
        "json_rpc_addr = \"http://127.0.0.1:8232\"\nindexer_grpc_addr = \"http://127.0.0.1:8233\"",
        "json_rpc_addr = \"http://127.0.0.1:8232\"\nbroadcast_timeout_secs = 1",
    ] {
        let tempdir = tempdir()?;
        let config_path = tempdir.path().join("zinder-explorer.toml");
        fs::write(
            &config_path,
            format!(
                r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[explorer]
listen_addr = "127.0.0.1:9068"
wallet_query_endpoint = "http://127.0.0.1:1"

[node]
{node_section}
"#,
                path_str(&tempdir.path().join("canonical"))?,
                path_str(&tempdir.path().join("secondary"))?,
            ),
        )?;
        let output = zinder_explorer_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .output()?;
        assert!(!output.status.success(), "accepted {node_section}");
    }
    Ok(())
}

#[test]
fn runtime_refuses_to_bind_without_an_admitted_materialized_view_and_wallet_pair()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let tempdir = tempdir()?;
    let secondary_path = tempdir.path().join("explorer-secondary");
    let config_path = tempdir.path().join("zinder-explorer.toml");
    let listen_addr = unused_loopback_addr()?;
    fs::write(
        &config_path,
        explorer_config_toml(store_fixture.tempdir_path(), &secondary_path, listen_addr)?,
    )?;

    let output = zinder_explorer_command()
        .args(["--config", path_str(&config_path)?])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("materialized-view") || stderr.contains("wallet"),
        "{stderr}"
    );

    Ok(())
}

fn zinder_explorer_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
    command.env_clear();
    command
}

fn explorer_config_toml(
    storage_path: &Path,
    secondary_path: &Path,
    listen_addr: SocketAddr,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[explorer]
listen_addr = "{}"
wallet_query_endpoint = "http://127.0.0.1:1"

[ops]
listen_addr = ""
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
        listen_addr,
    ))
}

fn unused_loopback_addr() -> eyre::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
