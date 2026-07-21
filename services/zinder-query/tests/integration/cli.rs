//! Public CLI contract for the native wallet-query runtime.

use std::process::Command;

use tempfile::TempDir;

fn query_command(temporary: &TempDir, canonical_secondary_root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-query"));
    command.args([
        "--print-config",
        "--network",
        "zcash-regtest",
        "--canonical-primary-path",
    ]);
    command.arg(temporary.path().join("canonical-primary"));
    command.arg("--canonical-secondary-root");
    command.arg(canonical_secondary_root);
    command.arg("--wallet-primary-path");
    command.arg(temporary.path().join("wallet-primary"));
    command.arg("--wallet-secondary-root");
    command.arg(temporary.path().join("wallet-secondary"));
    command.args([
        "--ingest-control-addr",
        "http://127.0.0.1:9100",
        "--node-json-rpc-addr",
        "http://127.0.0.1:29232",
    ]);
    command
}

#[test]
fn print_config_uses_native_ports_and_dedicated_secondary_roots() -> eyre::Result<()> {
    let temporary = TempDir::new()?;
    let output =
        query_command(&temporary, &temporary.path().join("canonical-secondary")).output()?;
    assert!(
        output.status.success(),
        "print-config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("listen_addr = \"127.0.0.1:9102\""));
    assert!(stdout.contains("listen_addr = \"127.0.0.1:9106\""));
    assert!(stdout.contains(&format!(
        "secondary_path = \"{}\"",
        temporary.path().join("canonical-secondary").display()
    )));
    assert!(stdout.contains(&format!(
        "secondary_path = \"{}\"",
        temporary.path().join("wallet-secondary").display()
    )));
    assert!(!stdout.contains("9101"));
    Ok(())
}

#[test]
fn nested_secondary_roots_are_rejected_before_storage_open() -> eyre::Result<()> {
    let temporary = TempDir::new()?;
    let canonical_primary = temporary.path().join("canonical-primary");
    let nested_secondary = canonical_primary.join("query-secondary");
    let output = query_command(&temporary, &nested_secondary).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be disjoint roots"));
    Ok(())
}

#[test]
fn public_bind_requires_an_explicit_security_opt_in() -> eyre::Result<()> {
    let temporary = TempDir::new()?;
    let mut command = query_command(&temporary, &temporary.path().join("canonical-secondary"));
    command.args(["--listen-addr", "0.0.0.0:9102"]);
    let output = command.output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("public"));
    Ok(())
}
