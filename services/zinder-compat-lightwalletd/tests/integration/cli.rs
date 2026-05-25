#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

#[test]
fn print_config_renders_resolved_toml_to_stdout() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-print-config-store");
    let secondary_path = tempdir.path().join("compat-print-config-secondary");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("[network]"), "{stdout}");
    assert!(stdout.contains("name = \"zcash-regtest\""), "{stdout}");
    assert!(stdout.contains("[compat]"), "{stdout}");
    assert!(
        stdout.contains("listen_addr = \"127.0.0.1:9067\""),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "secondary_path = \"{}\"",
            path_str(&secondary_path)?
        )),
        "{stdout}"
    );
    assert!(stdout.contains("[storage.canonical.rocksdb]"), "{stdout}");
    assert!(!stdout.contains("[storage.derive.rocksdb]"), "{stdout}");
    assert!(stdout.contains("[ingest_control]"), "{stdout}");
    assert!(
        stdout.contains("addr = \"http://127.0.0.1:9100\""),
        "{stdout}"
    );
    assert!(!stderr.contains("ERROR"), "{stderr}");

    Ok(())
}

#[test]
fn missing_storage_path_is_rejected_before_binding() -> eyre::Result<()> {
    let output = zinder_compat_command()
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
    let storage_path = tempdir.path().join("compat-missing-secondary-store");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_without_secondary_toml(&storage_path)?,
    )?;

    let output = zinder_compat_command()
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
fn ingest_only_section_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-node-source-store");
    let secondary_path = tempdir.path().join("compat-node-source-secondary");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_with_ingest_section_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown field `ingest`"), "{stderr}");

    Ok(())
}

#[test]
fn derive_storage_section_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-derive-store");
    let secondary_path = tempdir.path().join("compat-derive-secondary");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_with_derive_storage_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown field `derive`"), "{stderr}");

    Ok(())
}

fn compat_config_toml(storage_path: &Path, secondary_path: &Path) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn compat_config_with_derive_storage_toml(
    storage_path: &Path,
    secondary_path: &Path,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[storage.derive.rocksdb]
block_cache_bytes = 134217728

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn compat_config_with_ingest_section_toml(
    storage_path: &Path,
    secondary_path: &Path,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"

[node]
json_rpc_addr = "http://127.0.0.1:18232"

[ingest]
source = "zebra-json-rpc"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn compat_config_without_secondary_toml(storage_path: &Path) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
    ))
}

fn zinder_compat_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-compat-lightwalletd"));
    command.env_clear();
    command
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
