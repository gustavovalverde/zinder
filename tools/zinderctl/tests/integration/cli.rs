use std::{error::Error, process::Command};

#[test]
fn debug_trace_reports_build_identity() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_zinderctl"))
        .env("RUST_LOG", "zinderctl=debug")
        .arg("--version")
        .output()?;
    assert!(output.status.success(), "zinderctl --version failed");

    let stderr = String::from_utf8(output.stderr)?;
    let build_git_commit = option_env!("ZINDER_BUILD_GIT_COMMIT").unwrap_or("unknown");
    assert!(
        stderr.contains("zinderctl build identity"),
        "zinderctl did not report its build identity: {stderr}"
    );
    assert!(
        stderr.contains(build_git_commit),
        "zinderctl did not report build commit {build_git_commit}: {stderr}"
    );
    Ok(())
}
