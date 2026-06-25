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
    assert!(stdout.contains("[ingest_control]"), "{stdout}");
    assert!(
        stdout.contains("addr = \"http://127.0.0.1:9100\""),
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
fn storage_path_default_resolves_to_canonical_zinder_layout() -> eyre::Result<()> {
    // The binary's default for `storage.path` matches the canonical Zinder
    // layout under `/var/lib/zinder/store`. The default exists so the
    // single-container Docker image works with env-only configuration and
    // no `--config` argument. Operators on other deployment shapes override
    // via `ZINDER_STORAGE__PATH` or the `--storage-path` flag.
    let output = zinder_query_command()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("path = \"/var/lib/zinder/store\""),
        "stdout does not carry the canonical storage.path default:\n{stdout}"
    );

    Ok(())
}

#[test]
fn secondary_path_default_resolves_to_canonical_zinder_layout() -> eyre::Result<()> {
    // Same rationale as `storage_path_default_resolves_to_canonical_zinder_layout`:
    // the wallet-query reader opens its RocksDB secondary at
    // `/var/lib/zinder/secondary` by default. Operators on shared-store
    // deployments override via `ZINDER_STORAGE__SECONDARY_PATH` or the
    // `--secondary-path` flag.
    let output = zinder_query_command()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("secondary_path = \"/var/lib/zinder/secondary\""),
        "stdout does not carry the canonical storage.secondary_path default:\n{stdout}"
    );

    Ok(())
}

#[test]
fn ingest_only_section_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-node-source-store");
    let secondary_path = tempdir.path().join("query-node-source-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_with_ingest_section_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown field `ingest`"), "{stderr}");

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

fn query_config_with_ingest_section_toml(
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
json_rpc_addr = "http://127.0.0.1:18232"

[ingest]
source = "zebra-json-rpc"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
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
