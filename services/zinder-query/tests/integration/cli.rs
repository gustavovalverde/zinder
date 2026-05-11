#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

#[test]
fn print_config_renders_resolved_toml_to_stdout() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-print-config-store");
    let secondary_path = tempdir.path().join("query-print-config-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("[network]"), "{stdout}");
    assert!(stdout.contains("name = \"zcash-regtest\""), "{stdout}");
    assert!(stdout.contains("[query]"), "{stdout}");
    assert!(
        stdout.contains("listen_addr = \"127.0.0.1:9101\""),
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
        stdout.contains("ingest_control_addr = \"http://127.0.0.1:9100\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("mempool_mined_retention_minutes = 60"),
        "{stdout}"
    );
    assert!(
        stdout.contains("mempool_invalidated_retention_hours = 24"),
        "{stdout}"
    );
    assert!(!stderr.contains("ERROR"), "{stderr}");

    Ok(())
}

#[test]
fn missing_storage_path_is_rejected_before_binding() -> eyre::Result<()> {
    let output = zinder_query_command()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("missing configuration field: storage.path"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn missing_secondary_path_is_rejected_before_binding() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-missing-secondary-store");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_without_secondary_toml(&storage_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("missing configuration field: storage.secondary_path"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn sensitive_environment_override_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-sensitive-env-store");
    let secondary_path = tempdir.path().join("query-sensitive-env-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .env("ZINDER_QUERY__AUTH__PASSWORD", "env-secret")
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ZINDER_QUERY__AUTH__PASSWORD targets a sensitive field"),
        "{stderr}"
    );
    assert!(!stderr.contains("env-secret"), "{stderr}");

    Ok(())
}

#[test]
fn print_config_accepts_explorer_derive_proxy() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-derive-store");
    let secondary_path = tempdir.path().join("query-derive-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--derive-explorer-endpoint",
            "http://127.0.0.1:9068",
            "--derive-explorer-probe-interval-ms",
            "1000",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[derive.explorer]"), "{stdout}");
    assert!(
        stdout.contains("endpoint = \"http://127.0.0.1:9068\""),
        "{stdout}"
    );
    assert!(stdout.contains("probe_interval_ms = 1000"), "{stdout}");

    Ok(())
}

#[test]
fn zero_explorer_derive_probe_interval_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-derive-zero-store");
    let secondary_path = tempdir.path().join("query-derive-zero-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--derive-explorer-endpoint",
            "http://127.0.0.1:9068",
            "--derive-explorer-probe-interval-ms",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("derive.explorer.probe_interval_ms must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn ingest_only_node_source_config_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-node-source-store");
    let secondary_path = tempdir.path().join("query-node-source-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_with_node_source_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("unknown field `source`") && stderr.contains("json_rpc_addr"),
        "{stderr}"
    );

    Ok(())
}

fn query_config_toml(storage_path: &Path, secondary_path: &Path) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[query]
listen_addr = "127.0.0.1:9101"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn query_config_with_node_source_toml(
    storage_path: &Path,
    secondary_path: &Path,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[query]
listen_addr = "127.0.0.1:9101"

[node]
source = "zebra-json-rpc"
json_rpc_addr = "http://127.0.0.1:18232"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn query_config_without_secondary_toml(storage_path: &Path) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"

[query]
listen_addr = "127.0.0.1:9101"
"#,
        path_str(storage_path)?,
    ))
}

fn zinder_query_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-query"));
    command.env_clear();
    command
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
