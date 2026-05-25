//! Injects the short git commit hash into the binary so `lightd_info` can
//! report it without runtime git access. Empty string when the build runs
//! outside a git checkout.

use std::process::Command;

fn main() {
    let git_hash = git_command(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=LIGHTWALLETD_COMPAT_BUILD_GIT_COMMIT={git_hash}");

    // Watch the files git updates as commits land or refs repack so the
    // recorded hash stays current. `.git/HEAD` flips on checkout, the
    // per-branch ref file moves on every commit, and `packed-refs` is what
    // git fetch/push update when refs are compacted.
    if let Some(git_dir) = git_command(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/packed-refs");
        if let Some(symbolic_ref) = git_command(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={git_dir}/{symbolic_ref}");
        }
    }
}

fn git_command(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
