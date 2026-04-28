#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{error::Error, fs, path::Path, process::Command};

use tempfile::tempdir;
use zinder_core::{ChainEpochId, Network};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::ChainFixture;

#[test]
fn print_config_validates_and_redacts_basic_auth() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("print-config-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
            "--json-rpc-addr",
            "http://127.0.0.1:40000",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[node.auth]"));
    assert!(stdout.contains("method = \"basic\""));
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(stdout.contains("json_rpc_addr = \"http://127.0.0.1:40000\""));
    assert!(!stdout.contains("file-secret"));

    Ok(())
}

#[test]
fn print_config_accepts_zebra_cookie_auth() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("cookie-auth-store");
    let cookie_path = tempdir.path().join("zebra-cookie");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        cookie_backfill_config_toml(&storage_path, &cookie_path, 1, 1)?,
    )?;
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[node.auth]"));
    assert!(stdout.contains("method = \"cookie\""));
    assert!(stdout.contains("path = \"[REDACTED]\""));
    assert!(!stdout.contains("zebra-cookie"));

    Ok(())
}

#[test]
fn print_config_loads_backfill_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("configured-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("name = \"zcash-regtest\""));
    assert!(stdout.contains("json_rpc_addr = \"http://127.0.0.1:39232\""));
    assert!(stdout.contains("from_height = 1"));
    assert!(stdout.contains("to_height = 1"));
    assert!(stdout.contains("allow_near_tip_finalize = false"));
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(!stdout.contains("file-secret"));

    Ok(())
}

#[test]
fn tip_follow_print_config_loads_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("tip-follow-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "tip-follow",
            "--reorg-window-blocks",
            "12",
            "--poll-interval-ms",
            "250",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[tip_follow]"));
    assert!(stdout.contains("reorg_window_blocks = 12"));
    assert!(stdout.contains("poll_interval_ms = 250"));
    assert!(stdout.contains("[ingest.control]"));
    assert!(stdout.contains("listen_addr = \"127.0.0.1:9100\""));
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(!stdout.contains("file-secret"));

    Ok(())
}

#[test]
fn backup_print_config_loads_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("backup-print-config-store");
    let backup_path = tempdir.path().join("backup-print-config-checkpoint");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        backup_config_toml(&storage_path, &backup_path)?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backup",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[backup]"));
    assert!(stdout.contains(&format!("to_path = \"{}\"", path_str(&backup_path)?)));

    Ok(())
}

#[test]
fn backup_creates_checkpoint_from_primary_store() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("backup-source-store");
    let checkpoint_path = tempdir.path().join("backup-checkpoint");
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let artifacts = chain_fixture
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or("chain fixture unexpectedly empty")?;
    let expected_chain_epoch = artifacts.chain_epoch;

    {
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        store.commit_chain_epoch(artifacts)?;
    }

    let output = zinder_ingest_command()
        .args([
            "backup",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
            "--to",
            path_str(&checkpoint_path)?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let checkpoint = PrimaryChainStore::open(
        &checkpoint_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    assert_eq!(
        checkpoint.current_chain_epoch()?,
        Some(expected_chain_epoch)
    );

    Ok(())
}

#[test]
fn print_config_shows_explicit_near_tip_finalize_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("allow-near-tip-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
            "--allow-near-tip-finalize",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("allow_near_tip_finalize = true"));

    Ok(())
}

#[test]
fn cli_overrides_environment_and_environment_overrides_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("precedence-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .env("ZINDER_NODE__REQUEST_TIMEOUT_SECS", "45")
        .env("ZINDER_BACKFILL__TO_HEIGHT", "2")
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
            "--json-rpc-addr",
            "http://127.0.0.1:40000",
            "--to-height",
            "3",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("json_rpc_addr = \"http://127.0.0.1:40000\""));
    assert!(stdout.contains("request_timeout_secs = 45"));
    assert!(stdout.contains("to_height = 3"));

    Ok(())
}

#[test]
fn checkpoint_height_uses_standard_config_precedence() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("checkpoint-precedence-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let config_toml = format!(
        "{}checkpoint_height = 5\n",
        backfill_config_toml(&storage_path, 6, 10)?
    );
    fs::write(&config_path, config_toml)?;

    let output = zinder_ingest_command()
        .env("ZINDER_BACKFILL__CHECKPOINT_HEIGHT", "6")
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
            "--checkpoint-height",
            "7",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("checkpoint_height = 7"));
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(!stdout.contains("file-secret"));

    Ok(())
}

#[test]
fn wallet_serving_backfill_print_config_omits_node_derived_floor() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        wallet_serving_backfill_config_toml(&storage_path, 2_000_000)?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("coverage = \"wallet-serving\""));
    assert!(!stdout.contains("from_height = 0"));
    assert!(!stdout.contains("checkpoint_height = 0"));

    Ok(())
}

#[test]
fn wallet_serving_backfill_rejects_explicit_checkpoint_height() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-checkpoint-store");
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--to-height",
            "10",
            "--wallet-serving",
            "--checkpoint-height",
            "5",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("wallet-serving"));
    assert!(stderr.contains("checkpoint_height"));

    Ok(())
}

#[test]
fn wallet_serving_backfill_rejects_near_tip_finalize_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-near-tip-store");
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--to-height",
            "10",
            "--wallet-serving",
            "--allow-near-tip-finalize",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("wallet-serving"));
    assert!(stderr.contains("allow_near_tip_finalize"));

    Ok(())
}

#[test]
fn max_response_bytes_can_be_overridden_from_cli() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("response-cap-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
            "--max-response-bytes",
            "33554432",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("max_response_bytes = 33554432"));

    Ok(())
}

#[test]
fn sensitive_password_environment_override_is_rejected() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("sensitive-env-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .env("ZINDER_NODE__AUTH__PASSWORD", "env-secret")
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("sensitive field password"));
    assert!(!stderr.contains("env-secret"));

    Ok(())
}

#[test]
fn sensitive_password_hint_environment_override_is_rejected() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("sensitive-env-hint-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, backfill_config_toml(&storage_path, 1, 1)?)?;

    let output = zinder_ingest_command()
        .env("ZINDER_NODE__AUTH__PASSWORD_HINT", "env-secret")
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backfill",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("sensitive field password_hint"));
    assert!(!stderr.contains("env-secret"));

    Ok(())
}

#[test]
fn zero_commit_batch_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-batch-store");
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--from-height",
            "1",
            "--to-height",
            "1",
            "--commit-batch-blocks",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid commit batch size"));

    Ok(())
}

#[test]
fn zero_max_response_bytes_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-response-cap-store");
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--from-height",
            "1",
            "--to-height",
            "1",
            "--max-response-bytes",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid JSON-RPC response byte limit"));

    Ok(())
}

#[test]
fn zero_tip_follow_poll_interval_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-poll-store");
    let output = zinder_ingest_command()
        .args([
            "tip-follow",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--poll-interval-ms",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid tip-follow poll interval"));

    Ok(())
}

#[test]
fn missing_storage_path_is_rejected_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--from-height",
            "1",
            "--to-height",
            "1",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("missing configuration field: storage.path"));

    Ok(())
}

#[test]
fn unknown_network_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("unknown-network-store");
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "unknown",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--from-height",
            "1",
            "--to-height",
            "1",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown network"));

    Ok(())
}

#[test]
fn inverted_range_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("inverted-range-store");
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--from-height",
            "2",
            "--to-height",
            "1",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid backfill range"));

    Ok(())
}

#[test]
fn partial_basic_auth_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("partial-auth-store");
    let output = zinder_ingest_command()
        .args([
            "backfill",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--node-auth-method",
            "basic",
            "--node-auth-username",
            "zebra",
            "--storage-path",
            path_str(&storage_path)?,
            "--from-height",
            "1",
            "--to-height",
            "1",
        ])
        .output()?;

    assert!(!output.status.success());
    assert!(!storage_path.exists());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("missing configuration field: node.auth.password"));

    Ok(())
}

fn zinder_ingest_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-ingest"));
    command.env_clear();
    command
}

fn backfill_config_toml(
    storage_path: &Path,
    from_height: u32,
    to_height: u32,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "basic"
username = "zebra"
password = "file-secret"

[storage]
path = "{}"

[ingest]
commit_batch_blocks = 1000

[backfill]
from_height = {}
to_height = {}
allow_near_tip_finalize = false
"#,
        path_str(storage_path)?,
        from_height,
        to_height
    ))
}

fn wallet_serving_backfill_config_toml(
    storage_path: &Path,
    to_height: u32,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "basic"
username = "zebra"
password = "file-secret"

[storage]
path = "{}"

[ingest]
commit_batch_blocks = 1000

[backfill]
to_height = {}
allow_near_tip_finalize = false
coverage = "wallet-serving"
"#,
        path_str(storage_path)?,
        to_height
    ))
}

fn cookie_backfill_config_toml(
    storage_path: &Path,
    cookie_path: &Path,
    from_height: u32,
    to_height: u32,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "cookie"
path = "{}"

[storage]
path = "{}"

[ingest]
commit_batch_blocks = 1000

[backfill]
from_height = {}
to_height = {}
allow_near_tip_finalize = false
"#,
        path_str(cookie_path)?,
        path_str(storage_path)?,
        from_height,
        to_height
    ))
}

fn backup_config_toml(storage_path: &Path, backup_path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"

[backup]
to_path = "{}"
"#,
        path_str(storage_path)?,
        path_str(backup_path)?
    ))
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}
