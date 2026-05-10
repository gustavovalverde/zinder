//! Injects the short git commit hash into the binary as `ZINDER_GIT_COMMIT` so
//! `lightd_info` can report it without runtime git access. Empty string when
//! the build runs outside a git checkout.

use std::process::Command;

fn main() {
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().to_owned())
        .unwrap_or_default();
    println!("cargo:rustc-env=ZINDER_GIT_COMMIT={git_hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
