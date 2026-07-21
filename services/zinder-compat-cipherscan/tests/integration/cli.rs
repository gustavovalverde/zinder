//! Public CLI contract for the Cipherscan compatibility runtime.

use std::process::Command;

#[test]
fn print_config_uses_the_native_wallet_query_port() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_zinder-compat-cipherscan"))
        .env_clear()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("wallet_query_endpoint = \"http://127.0.0.1:9102\""),
        "{stdout}"
    );
    assert!(!stdout.contains("9101"), "{stdout}");
    Ok(())
}
