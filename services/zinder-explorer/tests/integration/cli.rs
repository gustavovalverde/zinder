#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

#[test]
fn print_config_accepts_explorer_bearer_token_path() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("derive-print-config-store");
    let token_path = tempdir.path().join("derive-explorer.token");
    fs::write(&token_path, "expected-token\n")?;

    let output = zinder_explorer_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
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
    let storage_path = tempdir.path().join("derive-invalid-token-store");
    let token_path = tempdir.path().join("derive-empty.token");
    fs::write(&token_path, "\n")?;

    let output = zinder_explorer_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
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

fn zinder_explorer_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-explorer"));
    command.env_clear();
    command
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
