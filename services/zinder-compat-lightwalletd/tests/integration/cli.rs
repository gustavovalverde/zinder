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
    let wallet_path = tempdir.path().join("compat-print-config-wallet");
    let wallet_secondary_path = wallet_path.with_extension("secondary");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_toml(&storage_path, &secondary_path, &wallet_path)?,
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
    assert!(stdout.contains("reorg_window_blocks = 100"), "{stdout}");
    assert!(
        stdout.contains("pair_convergence_attempts = 12"),
        "{stdout}"
    );
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
    assert!(stdout.contains("[storage.derive.rocksdb]"), "{stdout}");
    assert!(stdout.contains("[wallet]"), "{stdout}");
    assert!(
        stdout.contains(&format!("path = \"{}\"", path_str(&wallet_path)?)),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "secondary_path = \"{}\"",
            path_str(&wallet_secondary_path)?
        )),
        "{stdout}"
    );
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
fn zero_reorg_window_is_rejected_before_storage_open() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-zero-window-store");
    let secondary_path = tempdir.path().join("compat-zero-window-secondary");
    let wallet_path = tempdir.path().join("compat-zero-window-wallet");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_toml(&storage_path, &secondary_path, &wallet_path)?,
    )?;

    let output = zinder_compat_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--reorg-window-blocks",
            "0",
        ])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("canonical build reorg window must be greater than zero"),
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
fn missing_wallet_secondary_path_is_rejected_before_binding() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-missing-wallet-secondary-store");
    let secondary_path = tempdir
        .path()
        .join("compat-missing-wallet-secondary-canonical");
    let wallet_path = tempdir
        .path()
        .join("compat-missing-wallet-secondary-wallet");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        format!(
            r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[wallet]
path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"
"#,
            path_str(&storage_path)?,
            path_str(&secondary_path)?,
            path_str(&wallet_path)?,
        ),
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("missing configuration field: wallet.secondary_path"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn overlapping_primary_and_secondary_roots_are_rejected_before_binding() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-overlapping-store");
    let secondary_path = tempdir.path().join("compat-overlapping-secondary");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_toml(&storage_path, &secondary_path, &storage_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("storage.path, storage.secondary_path, wallet.path, and wallet.secondary_path must be disjoint roots"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn nested_or_lexically_aliased_storage_roots_are_rejected_before_binding() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-root-alias-store");
    let canonical_secondary_path = storage_path.join("secondary");
    let wallet_path = storage_path.join("..").join(
        storage_path
            .file_name()
            .ok_or_else(|| eyre::eyre!("missing file name"))?,
    );
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_toml(&storage_path, &canonical_secondary_path, &wallet_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("storage.path, storage.secondary_path, wallet.path, and wallet.secondary_path must be disjoint roots"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn legacy_storage_path_override_is_rejected() -> eyre::Result<()> {
    let output = zinder_compat_command()
        .args([
            "--print-config",
            "--storage-path",
            "/tmp/zinder-compat-legacy",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("unexpected argument '--storage-path'"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn ingest_only_section_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-node-source-store");
    let secondary_path = tempdir.path().join("compat-node-source-secondary");
    let wallet_path = tempdir.path().join("compat-node-source-wallet");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_with_ingest_section_toml(&storage_path, &secondary_path, &wallet_path)?,
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
fn derive_storage_section_is_accepted() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("compat-derive-store");
    let secondary_path = tempdir.path().join("compat-derive-secondary");
    let wallet_path = tempdir.path().join("compat-derive-wallet");
    let config_path = tempdir.path().join("zinder-compat.toml");
    fs::write(
        &config_path,
        compat_config_with_derive_storage_toml(&storage_path, &secondary_path, &wallet_path)?,
    )?;

    let output = zinder_compat_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("[storage.derive.rocksdb]"), "{stdout}");
    assert!(stdout.contains("block_cache_bytes = 134217728"), "{stdout}");
    assert!(!stderr.contains("ERROR"), "{stderr}");

    Ok(())
}

fn compat_config_toml(
    storage_path: &Path,
    secondary_path: &Path,
    wallet_path: &Path,
) -> eyre::Result<String> {
    let wallet_secondary_path = wallet_path.with_extension("secondary");
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[wallet]
path = "{}"
secondary_path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
        path_str(wallet_path)?,
        path_str(&wallet_secondary_path)?,
    ))
}

fn compat_config_with_derive_storage_toml(
    storage_path: &Path,
    secondary_path: &Path,
    wallet_path: &Path,
) -> eyre::Result<String> {
    let wallet_secondary_path = wallet_path.with_extension("secondary");
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[wallet]
path = "{}"
secondary_path = "{}"

[storage.derive.rocksdb]
block_cache_bytes = 134217728

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
        path_str(wallet_path)?,
        path_str(&wallet_secondary_path)?,
    ))
}

fn compat_config_with_ingest_section_toml(
    storage_path: &Path,
    secondary_path: &Path,
    wallet_path: &Path,
) -> eyre::Result<String> {
    let wallet_secondary_path = wallet_path.with_extension("secondary");
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[wallet]
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
        path_str(wallet_path)?,
        path_str(&wallet_secondary_path)?,
    ))
}

fn compat_config_without_secondary_toml(storage_path: &Path) -> eyre::Result<String> {
    let wallet_path = storage_path.with_extension("wallet");
    let wallet_secondary_path = storage_path.with_extension("wallet-secondary");
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"

[wallet]
path = "{}"
secondary_path = "{}"

[compat]
listen_addr = "127.0.0.1:9067"
"#,
        path_str(storage_path)?,
        path_str(&wallet_path)?,
        path_str(&wallet_secondary_path)?,
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
