//! Physical snapshot operator workflow.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, Subcommand};
use eyre::{Context, Result, eyre};
use futures_util::{StreamExt as _, TryStreamExt as _};
use reqwest::Url;
use sha2::{Digest, Sha256};
use zinder_core::{Network, wire::decode_zinder_native_chain_name};
use zinder_projector::{
    recovery_archive::{
        MAX_RECOVERY_ARCHIVE_MANIFEST_BYTES, RECOVERY_ARCHIVE_MANIFEST_FILE_NAME,
        RecoveryArchiveFile, RecoveryArchiveManifest, admit_recovery_archive,
        cold_admit_recovery_archive, package_completed_recovery_archive, restore_recovery_archive,
    },
    state_bundle::{
        CANONICAL_CHECKPOINT_DIRECTORY_NAME, StateBundleRecoveryAdmissionConfig,
        WALLET_CHECKPOINT_DIRECTORY_NAME,
    },
};
use zinder_proto::v1::ingest::{
    CreateStateBundleCaptureRequest, projector_control_client::ProjectorControlClient,
};
use zinder_runtime::{connect_zinder_grpc, load_bearer_token};
use zinder_store::RocksDbResourceBudget;
use zinder_wallet_rocksdb::WalletRecoveryAdmissionConfig;

use crate::{absolute_path, existing_operator_directory};

const DEFAULT_VALIDATION_SORT_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_VALIDATION_TEMPORARY_BYTES: u64 = 256 * 1024 * 1024 * 1024;
const DEFAULT_VALIDATION_REORG_UNDO_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_DOWNLOAD_CONCURRENCY: u32 = 8;
const MAX_DOWNLOAD_CONCURRENCY: u32 = 64;

#[derive(Subcommand)]
pub(crate) enum SnapshotCommand {
    /// Ask the running projector for a coherent capture and package it.
    Create(CreateArgs),
    /// Print the sealed outer manifest after byte admission.
    Inspect(ArchiveArgs),
    /// Verify every byte and cold-admit both checkpoint stores.
    Verify(VerifyArgs),
    /// Download a manifest-first snapshot from static HTTPS or an R2 public URL.
    Pull(PullArgs),
    /// Restore a verified snapshot into fresh canonical and wallet paths.
    Restore(RestoreArgs),
}

#[derive(Args)]
pub(crate) struct CreateArgs {
    /// Exact Zinder-native network name.
    #[arg(long)]
    network: String,
    /// Projector owner-control endpoint, including the HTTP scheme.
    #[arg(long = "projector-control-addr")]
    projector_control_addr: String,
    /// Optional file containing the projector-control bearer token.
    #[arg(long = "projector-control-token-path")]
    projector_control_token_path: Option<PathBuf>,
    /// Shared projector/ingest state-bundle staging root.
    #[arg(long = "staging-root")]
    staging_root: PathBuf,
    /// Existing root that will receive the portable candidate directory.
    #[arg(long = "archive-root")]
    archive_root: PathBuf,
    /// Opaque lowercase candidate identifier.
    #[arg(long)]
    candidate: String,
}

#[derive(Args)]
pub(crate) struct ArchiveArgs {
    /// Existing root containing candidate directories.
    #[arg(long = "archive-root")]
    archive_root: PathBuf,
    /// Opaque candidate identifier.
    #[arg(long)]
    candidate: String,
    /// Exact expected Zinder-native network name.
    #[arg(long)]
    network: String,
}

#[derive(Args)]
pub(crate) struct VerifyArgs {
    #[command(flatten)]
    archive: ArchiveArgs,
    #[command(flatten)]
    validation: ValidationArgs,
}

#[derive(Args)]
pub(crate) struct PullArgs {
    /// URL of the remote candidate directory.
    #[arg(long)]
    url: String,
    /// Existing local root that will receive the candidate directory.
    #[arg(long = "archive-root")]
    archive_root: PathBuf,
    /// Trusted SHA-256 of the exact remote outer-manifest bytes.
    #[arg(long = "expected-manifest-sha256")]
    expected_manifest_sha256: String,
    /// Maximum simultaneous payload-object downloads.
    #[arg(
        long = "download-concurrency",
        default_value_t = DEFAULT_DOWNLOAD_CONCURRENCY
    )]
    download_concurrency: u32,
}

#[derive(Args)]
pub(crate) struct RestoreArgs {
    #[command(flatten)]
    archive: ArchiveArgs,
    /// Absent final path for the canonical READY store.
    #[arg(long = "canonical-target")]
    canonical_target: PathBuf,
    /// Absent final path for the wallet READY store.
    #[arg(long = "wallet-target")]
    wallet_target: PathBuf,
    #[command(flatten)]
    validation: ValidationArgs,
}

#[derive(Args)]
pub(crate) struct ValidationArgs {
    /// Existing private directory for wallet validation sort runs.
    #[arg(long = "validation-staging")]
    staging: PathBuf,
    /// Per-sorter in-memory validation ceiling.
    #[arg(
        long = "validation-sort-memory-bytes",
        default_value_t = DEFAULT_VALIDATION_SORT_MEMORY_BYTES
    )]
    sort_memory_bytes: u64,
    /// Per-sorter temporary-file ceiling.
    #[arg(
        long = "validation-temporary-bytes",
        default_value_t = DEFAULT_VALIDATION_TEMPORARY_BYTES
    )]
    temporary_bytes: u64,
    /// Accounted ceiling for retained reorg-undo reconstruction.
    #[arg(
        long = "validation-reorg-undo-bytes",
        default_value_t = DEFAULT_VALIDATION_REORG_UNDO_BYTES
    )]
    reorg_undo_bytes: u64,
}

pub(crate) async fn run(command: SnapshotCommand) -> Result<()> {
    match command {
        SnapshotCommand::Create(args) => create(args).await,
        SnapshotCommand::Inspect(args) => inspect(args),
        SnapshotCommand::Verify(args) => verify(args),
        SnapshotCommand::Pull(args) => pull(args).await,
        SnapshotCommand::Restore(args) => restore(args),
    }
}

async fn create(args: CreateArgs) -> Result<()> {
    let network = parse_network(&args.network)?;
    let staging_root = absolute_path(args.staging_root, "staging root")?;
    let archive_root = absolute_path(args.archive_root, "archive root")?;
    let token_path = args
        .projector_control_token_path
        .map(|path| absolute_path(path, "projector control token path"))
        .transpose()?;
    let token = load_bearer_token(token_path.as_deref())?;
    let channel = connect_zinder_grpc(&args.projector_control_addr, token.as_ref()).await?;
    let mut client = ProjectorControlClient::new(channel);
    let response = client
        .create_state_bundle_capture(CreateStateBundleCaptureRequest {
            candidate_id: args.candidate.clone(),
        })
        .await?
        .into_inner();
    if response.candidate_id != args.candidate {
        return Err(eyre!("projector returned a different capture candidate"));
    }
    let archive =
        package_completed_recovery_archive(archive_root, staging_root, &args.candidate, network)?;
    print_manifest(archive.manifest())
}

fn inspect(args: ArchiveArgs) -> Result<()> {
    let network = parse_network(&args.network)?;
    let archive_root = absolute_path(args.archive_root, "archive root")?;
    let archive = admit_recovery_archive(archive_root, &args.candidate, network)?;
    print_manifest(archive.manifest())
}

fn verify(args: VerifyArgs) -> Result<()> {
    let network = parse_network(&args.archive.network)?;
    let archive_root = absolute_path(args.archive.archive_root, "archive root")?;
    let validation_staging = absolute_path(args.validation.staging.clone(), "validation staging")?;
    let archive = admit_recovery_archive(archive_root, &args.archive.candidate, network)?;
    cold_admit_recovery_archive(
        &archive,
        recovery_admission_config(&args.validation, &validation_staging),
    )?;
    print_manifest(archive.manifest())
}

async fn pull(args: PullArgs) -> Result<()> {
    let archive_root = existing_operator_directory(args.archive_root, "archive root")?;
    let base_url = parse_remote_candidate_url(&args.url)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_mins(1))
        .build()?;
    let manifest_url = base_url.join(RECOVERY_ARCHIVE_MANIFEST_FILE_NAME)?;
    let manifest_bytes = download_bounded_manifest(&client, manifest_url).await?;
    require_digest(
        &args.expected_manifest_sha256,
        &manifest_bytes,
        "outer manifest",
    )?;
    let manifest = RecoveryArchiveManifest::decode(&manifest_bytes)?;
    let candidate_root = archive_root.join(manifest.candidate_id());
    require_absent(&candidate_root)?;
    if !(1..=MAX_DOWNLOAD_CONCURRENCY).contains(&args.download_concurrency) {
        return Err(eyre!(
            "download concurrency must be between 1 and {MAX_DOWNLOAD_CONCURRENCY}"
        ));
    }
    let download_concurrency =
        usize::try_from(args.download_concurrency).map_err(|_| eyre!("invalid concurrency"))?;
    fs::create_dir(&candidate_root)
        .wrap_err_with(|| format!("could not create {}", candidate_root.display()))?;
    fs::create_dir(candidate_root.join(CANONICAL_CHECKPOINT_DIRECTORY_NAME))?;
    fs::create_dir(candidate_root.join(WALLET_CHECKPOINT_DIRECTORY_NAME))?;
    futures_util::stream::iter(manifest.payload_files())
        .map(|file| download_payload_file(&client, &base_url, &candidate_root, file))
        .buffer_unordered(download_concurrency)
        .try_collect::<Vec<_>>()
        .await?;
    let manifest_path = candidate_root.join(RECOVERY_ARCHIVE_MANIFEST_FILE_NAME);
    let temporary_manifest_path = candidate_root.join(".recovery-archive.json.incomplete");
    write_new_synced_file(&temporary_manifest_path, &manifest_bytes)?;
    fs::rename(&temporary_manifest_path, &manifest_path)?;
    sync_directory(&candidate_root)?;
    let archive =
        admit_recovery_archive(&archive_root, manifest.candidate_id(), manifest.network()?)?;
    print_manifest(archive.manifest())
}

fn restore(args: RestoreArgs) -> Result<()> {
    let network = parse_network(&args.archive.network)?;
    let archive_root = absolute_path(args.archive.archive_root, "archive root")?;
    let canonical_target = absolute_path(args.canonical_target, "canonical target")?;
    let wallet_target = absolute_path(args.wallet_target, "wallet target")?;
    let validation_staging = absolute_path(args.validation.staging.clone(), "validation staging")?;
    let archive = admit_recovery_archive(archive_root, &args.archive.candidate, network)?;
    let restored = restore_recovery_archive(
        &archive,
        canonical_target,
        wallet_target,
        recovery_admission_config(&args.validation, &validation_staging),
    )?;
    print_json(&serde_json::json!({
        "candidate_id": archive.candidate_id(),
        "network": args.archive.network,
        "canonical_path": restored.canonical(),
        "wallet_path": restored.wallet(),
    }))
}

fn recovery_admission_config<'staging>(
    args: &ValidationArgs,
    validation_staging: &'staging Path,
) -> StateBundleRecoveryAdmissionConfig<'staging> {
    StateBundleRecoveryAdmissionConfig {
        canonical_resource_budget: RocksDbResourceBudget::canonical_reader_defaults(),
        wallet: WalletRecoveryAdmissionConfig {
            resource_budget: RocksDbResourceBudget::wallet_projection_reader_defaults(),
            staging_path: validation_staging,
            max_sort_memory_bytes_per_sorter: args.sort_memory_bytes,
            max_temporary_file_bytes_per_sorter: args.temporary_bytes,
            max_accounted_reorg_undo_bytes: args.reorg_undo_bytes,
        },
    }
}

fn parse_remote_candidate_url(encoded: &str) -> Result<Url> {
    let mut encoded = encoded.trim_end_matches('/').to_owned();
    encoded.push('/');
    let url = Url::parse(&encoded)?;
    let loopback_http = url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !loopback_http {
        return Err(eyre!(
            "remote candidate URL must use HTTPS; HTTP is allowed only for loopback testing"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(eyre!(
            "remote candidate URL must be an object prefix without a query or fragment"
        ));
    }
    Ok(url)
}

async fn download_bounded_manifest(client: &reqwest::Client, url: Url) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RECOVERY_ARCHIVE_MANIFEST_BYTES)
    {
        return Err(eyre!("remote outer manifest exceeds the fixed byte limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if u64::try_from(bytes.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            > MAX_RECOVERY_ARCHIVE_MANIFEST_BYTES
        {
            return Err(eyre!("remote outer manifest exceeds the fixed byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn download_payload_file(
    client: &reqwest::Client,
    base_url: &Url,
    candidate_root: &Path,
    expected: &RecoveryArchiveFile,
) -> Result<()> {
    let target = candidate_root.join(expected.path());
    let parent = target
        .parent()
        .ok_or_else(|| eyre!("download target has no parent"))?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(eyre!("download target parent must be a real directory"));
    }
    let url = base_url.join(expected.path())?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length != expected.byte_length())
    {
        return Err(eyre!("remote object length does not match its manifest"));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)?;
    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    while let Some(chunk) = response.chunk().await? {
        byte_length = byte_length
            .checked_add(u64::try_from(chunk.len()).map_err(|_| eyre!("chunk is too large"))?)
            .ok_or_else(|| eyre!("download byte length overflowed u64"))?;
        if byte_length > expected.byte_length() {
            return Err(eyre!("remote object exceeds its manifest byte length"));
        }
        output.write_all(&chunk)?;
        digest.update(&chunk);
    }
    output.sync_all()?;
    if byte_length != expected.byte_length() || hex::encode(digest.finalize()) != expected.sha256()
    {
        return Err(eyre!("remote object bytes do not match their manifest"));
    }
    Ok(())
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(eyre!("download target already exists: {}", path.display())),
        Err(source) => Err(source.into()),
    }
}

fn require_digest(expected: &str, bytes: &[u8], field: &str) -> Result<()> {
    let decoded = hex::decode(expected)
        .map_err(|_| eyre!("{field} SHA-256 must be lowercase 32-byte hexadecimal"))?;
    if decoded.len() != 32 || hex::encode(&decoded) != expected {
        return Err(eyre!(
            "{field} SHA-256 must be lowercase 32-byte hexadecimal"
        ));
    }
    let observed = hex::encode(Sha256::digest(bytes));
    if observed == expected {
        Ok(())
    } else {
        Err(eyre!("{field} SHA-256 does not match"))
    }
}

fn write_new_synced_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub(crate) fn parse_network(encoded: &str) -> Result<Network> {
    decode_zinder_native_chain_name(encoded)
        .map_err(|_| eyre!("unknown network {encoded:?}; use an exact Zinder-native name"))
}

fn print_manifest(manifest: &RecoveryArchiveManifest) -> Result<()> {
    print_json(&serde_json::to_value(manifest)?)
}

#[allow(
    clippy::print_stdout,
    reason = "zinderctl emits its requested machine-readable command result on stdout"
)]
fn print_json(document: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(document)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_candidate_url_requires_https_outside_loopback() {
        assert!(parse_remote_candidate_url("https://state.example.net/candidate").is_ok());
        assert!(parse_remote_candidate_url("http://127.0.0.1:8080/candidate").is_ok());
        assert!(parse_remote_candidate_url("http://state.example.net/candidate").is_err());
        assert!(
            parse_remote_candidate_url("https://state.example.net/candidate?token=secret").is_err()
        );
    }
}
