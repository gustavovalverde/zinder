//! Zinder state portability operator CLI.

use std::{
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use eyre::Result;
use zinder_runtime::{
    BUILD_GIT_COMMIT, host_cpu_meets_compiled_baseline, install_tracing_subscriber,
};

mod migration;
mod migration_archive;
mod migration_capture;
mod migration_error;
mod snapshot;

#[derive(Parser)]
#[command(name = "zinderctl")]
#[command(about = "Export, verify, download, restore, and migrate Zinder state")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage exact-schema canonical and wallet snapshots.
    Snapshot {
        #[command(subcommand)]
        command: snapshot::SnapshotCommand,
    },
    /// Export and import schema-independent canonical state.
    Migrate {
        #[command(subcommand)]
        command: migration::MigrationCommand,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    install_tracing_subscriber();
    tracing::debug!(
        version = env!("CARGO_PKG_VERSION"),
        build_git_commit = BUILD_GIT_COMMIT,
        "zinderctl build identity"
    );
    if !host_cpu_meets_compiled_baseline() {
        return ExitCode::FAILURE;
    }
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "zinderctl command failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Snapshot { command } => snapshot::run(command).await,
        Command::Migrate { command } => migration::run(command).await,
    }
}

fn absolute_path(path: PathBuf, field: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(eyre::eyre!("{field} must be an absolute path"));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        return Err(eyre::eyre!(
            "{field} must not contain traversal or platform prefixes"
        ));
    }
    Ok(path)
}

fn existing_operator_directory(path: PathBuf, field: &str) -> Result<PathBuf> {
    let path = absolute_path(path, field)?;
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(eyre::eyre!("{field} must be a real directory"));
    }
    let resolved = fs::canonicalize(&path)?;
    let resolved_metadata = fs::symlink_metadata(&resolved)?;
    if resolved_metadata.file_type().is_symlink() || !resolved_metadata.is_dir() {
        return Err(eyre::eyre!("{field} must resolve to a real directory"));
    }
    Ok(resolved)
}

fn absent_operator_target(path: PathBuf, field: &str) -> Result<PathBuf> {
    let path = absolute_path(path, field)?;
    let name = path
        .file_name()
        .ok_or_else(|| eyre::eyre!("{field} must end in a normal file name"))?;
    if !matches!(path.components().next_back(), Some(Component::Normal(_))) {
        return Err(eyre::eyre!("{field} must end in a normal file name"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("{field} must have an existing parent"))?;
    let parent = existing_operator_directory(parent.to_path_buf(), field)?;
    let target = parent.join(name);
    require_absent_operator_path(&target, field)?;
    Ok(target)
}

fn incomplete_operator_sibling(target: &Path, operation: &str, field: &str) -> Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| eyre::eyre!("{field} must end in a normal file name"))?;
    let mut staging_name = OsString::from(".");
    staging_name.push(name);
    staging_name.push(format!(".zinder-{operation}.incomplete"));
    let staging = target.with_file_name(staging_name);
    require_absent_operator_path(&staging, field)?;
    Ok(staging)
}

fn require_absent_operator_path(path: &Path, field: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(eyre::eyre!("{field} must be absent: {}", path.display())),
        Err(source) => Err(source.into()),
    }
}

fn sync_operator_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("operator target has no parent directory"))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
