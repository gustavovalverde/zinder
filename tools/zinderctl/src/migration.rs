//! Cross-schema logical replay workflow.

use std::{
    fs,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use clap::{Args, Subcommand};
use eyre::{Result, eyre};
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, wire::encode_zinder_native_chain_name};
use zinder_ingest::{
    CanonicalConstructionConfig, CanonicalFollowConfig, CanonicalPipelineLimits,
    CanonicalWriterConfig, run_canonical_writer,
};
use zinder_projector::recovery_archive::{admit_recovery_archive, read_admitted_state_bundle};
use zinder_runtime::Readiness;
use zinder_source::{DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth};
use zinder_store::{CANONICAL_STORE_SCHEMA_VERSION, RocksDbResourceBudget};

use crate::{
    absent_operator_target, absolute_path, existing_operator_directory,
    incomplete_operator_sibling,
    migration_archive::{MigrationArchiveManifest, MigrationArchiveSource},
    migration_capture::{CaptureConfig, capture_fixed_range},
    snapshot::parse_network,
    sync_operator_parent,
};

const DEFAULT_SEGMENT_BLOCKS: u32 = 1_000;
const DEFAULT_FETCH_CONCURRENCY: u32 = 8;
const DEFAULT_PREPARE_CONCURRENCY: u32 = 8;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 60;
const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const MAX_SEGMENT_BLOCKS: u32 = 10_000;
const MAX_CAPTURE_CONCURRENCY: u32 = 64;

#[derive(Subcommand)]
pub(crate) enum MigrationCommand {
    /// Describe how a logical archive maps to this binary's current schema.
    Plan(PlanArgs),
    /// Export a schema-independent raw source replay archive.
    Export(ExportArgs),
    /// Rebuild a current canonical store from a logical archive.
    Import(ImportArgs),
}

#[derive(Args)]
pub(crate) struct PlanArgs {
    /// Logical migration archive directory.
    #[arg(long)]
    archive: PathBuf,
}

#[derive(Args)]
pub(crate) struct ExportArgs {
    /// Root containing the exact-schema snapshot used as the export fence.
    #[arg(long = "snapshot-root")]
    snapshot_root: PathBuf,
    /// Snapshot candidate identifier.
    #[arg(long = "snapshot-candidate")]
    snapshot_candidate: String,
    /// Exact expected Zinder-native network name.
    #[arg(long)]
    network: String,
    /// Zebra JSON-RPC endpoint.
    #[arg(long = "node-json-rpc-addr")]
    node_json_rpc_addr: String,
    /// Optional Zebra cookie file.
    #[arg(long = "node-cookie-path")]
    node_cookie_path: Option<PathBuf>,
    /// Absent logical archive directory.
    #[arg(long)]
    output: PathBuf,
    /// Blocks per immutable segment object.
    #[arg(long = "segment-blocks", default_value_t = DEFAULT_SEGMENT_BLOCKS)]
    segment_blocks: u32,
    /// Concurrent Zebra block fetches.
    #[arg(long = "fetch-concurrency", default_value_t = DEFAULT_FETCH_CONCURRENCY)]
    fetch_concurrency: u32,
    /// Concurrent block-fact preparations.
    #[arg(long = "prepare-concurrency", default_value_t = DEFAULT_PREPARE_CONCURRENCY)]
    prepare_concurrency: u32,
}

#[derive(Args)]
pub(crate) struct ImportArgs {
    /// Logical migration archive directory.
    #[arg(long)]
    archive: PathBuf,
    /// Trusted SHA-256 of the exact logical manifest bytes.
    #[arg(long = "expected-manifest-sha256")]
    expected_manifest_sha256: String,
    /// Absent final canonical store path.
    #[arg(long = "canonical-target")]
    canonical_target: PathBuf,
    /// Canonical replacement window for the rebuilt store.
    #[arg(
        long = "reorg-window-blocks",
        default_value_t = DEFAULT_REORG_WINDOW_BLOCKS
    )]
    reorg_window_blocks: u32,
}

pub(crate) async fn run(command: MigrationCommand) -> Result<()> {
    match command {
        MigrationCommand::Plan(args) => plan(args),
        MigrationCommand::Export(args) => export(args).await,
        MigrationCommand::Import(args) => import(args).await,
    }
}

fn plan(args: PlanArgs) -> Result<()> {
    let archive = existing_operator_directory(args.archive, "migration archive")?;
    let (manifest, manifest_sha256) = MigrationArchiveManifest::read_with_sha256(&archive)?;
    print_json(&serde_json::json!({
        "artifact_identity": manifest.contract_identity,
        "artifact_format_version": manifest.archive_format_version,
        "manifest_sha256": manifest_sha256,
        "network": manifest.network,
        "source_canonical_schema_version": manifest.source_canonical_schema_version,
        "destination_canonical_schema_version": CANONICAL_STORE_SCHEMA_VERSION,
        "first_rebuilt_height": 1,
        "tip_height": manifest.to_height,
        "action": "rebuild-canonical-through-ingest",
        "wallet_action": "rebuild-with-zinder-projector",
        "materialized_view_action": "rebuild-from-canonical-events",
        "mempool_action": "rehydrate-from-node",
    }))
}

async fn export(args: ExportArgs) -> Result<()> {
    let network = parse_network(&args.network)?;
    let snapshot_root = existing_operator_directory(args.snapshot_root, "snapshot root")?;
    let output = absent_operator_target(args.output, "migration output")?;
    let staging_output =
        incomplete_operator_sibling(&output, "migration-export", "migration staging output")?;
    let node_cookie_path = args
        .node_cookie_path
        .map(|path| absolute_path(path, "node cookie path"))
        .transpose()?;
    let snapshot = admit_recovery_archive(snapshot_root, &args.snapshot_candidate, network)?;
    let state_bundle = read_admitted_state_bundle(&snapshot)?;
    if state_bundle.canonical_first_retained_height() != 1 {
        return Err(eyre!(
            "logical export requires a complete canonical history beginning at height 1"
        ));
    }
    let segment_blocks =
        bounded_nonzero(args.segment_blocks, MAX_SEGMENT_BLOCKS, "segment blocks")?;
    let fetch_concurrency = bounded_nonzero(
        args.fetch_concurrency,
        MAX_CAPTURE_CONCURRENCY,
        "fetch concurrency",
    )?;
    let prepare_concurrency = bounded_nonzero(
        args.prepare_concurrency,
        MAX_CAPTURE_CONCURRENCY,
        "prepare concurrency",
    )?;
    let node_auth = node_cookie_path.map_or(NodeAuth::None, NodeAuth::cookie_file);
    let manifest = capture_fixed_range(CaptureConfig {
        network,
        json_rpc_addr: args.node_json_rpc_addr,
        node_auth,
        from_height: BlockHeight::new(0),
        to_height: BlockHeight::new(state_bundle.fence().visible_tip_height()),
        source_canonical_schema_version: state_bundle.canonical_schema_version(),
        segment_blocks,
        fetch_concurrency,
        prepare_concurrency,
        request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS),
        max_response_bytes: DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        output_directory: staging_output.clone(),
    })
    .await?;
    let digest = &manifest.canonical_block_facts_digest_evidence;
    if manifest.tip_hash_hex != state_bundle.fence().visible_tip_hash()
        || digest.block_count != state_bundle.fence().visible_block_count()
        || digest.sequence_digest_sha256 != state_bundle.fence().sequence_digest_sha256()
    {
        return Err(eyre!(
            "captured logical source does not match the snapshot canonical fence"
        ));
    }
    fs::rename(&staging_output, &output)?;
    sync_operator_parent(&output)?;
    print_json(&serde_json::to_value(manifest)?)
}

async fn import(args: ImportArgs) -> Result<()> {
    let archive = existing_operator_directory(args.archive, "migration archive")?;
    let canonical_target = absent_operator_target(args.canonical_target, "canonical target")?;
    let canonical_staging =
        incomplete_operator_sibling(&canonical_target, "migration-import", "canonical staging")?;
    let (manifest, manifest_sha256) = MigrationArchiveManifest::read_with_sha256(&archive)?;
    require_manifest_digest(
        &args.expected_manifest_sha256,
        &manifest_sha256,
        "logical manifest",
    )?;
    let source = MigrationArchiveSource::open(&archive, &manifest)?;
    let activations = Arc::new(manifest.activations_typed()?);
    let tip = manifest.tip_id()?;
    let logical_core_count = std::thread::available_parallelism()
        .ok()
        .and_then(|count| u32::try_from(count.get()).ok())
        .and_then(NonZeroU32::new)
        .unwrap_or(NonZeroU32::MIN);
    let pipeline_limits = CanonicalPipelineLimits::resolve(
        None,
        logical_core_count,
        NonZeroU64::new(64 * 1024 * 1024).unwrap_or(NonZeroU64::MIN),
    );
    let request_timeout = Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS);
    let config = CanonicalWriterConfig {
        storage_path: canonical_staging.clone(),
        resource_budget: RocksDbResourceBudget::canonical_writer_defaults(),
        construction: CanonicalConstructionConfig {
            request_timeout,
            pipeline_limits,
            network_upgrade_activations: Arc::clone(&activations),
        },
        checkpoint_height: None,
        reorg_window_blocks: args.reorg_window_blocks,
        follow: CanonicalFollowConfig {
            request_timeout,
            poll_interval: Duration::from_secs(1),
            lag_threshold_blocks: 0,
            target_height: Some(tip.height),
            event_retention_window: None,
            event_retention_check_interval: Duration::from_mins(1),
            mempool_ready_gate: None,
        },
    };
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();
    let store = run_canonical_writer(&source, activations, config, &readiness, &cancel).await?;
    let fence = store.event_fence();
    if fence.visible_tip() != tip
        || fence.sequence_digest().block_count()
            != manifest.canonical_block_facts_digest_evidence.block_count
        || hex::encode(fence.sequence_digest().as_bytes())
            != manifest
                .canonical_block_facts_digest_evidence
                .sequence_digest_sha256
    {
        return Err(eyre!(
            "rebuilt canonical fence does not match the logical archive"
        ));
    }
    drop(store);
    fs::rename(&canonical_staging, &canonical_target)?;
    sync_operator_parent(&canonical_target)?;
    print_json(&serde_json::json!({
        "network": encode_zinder_native_chain_name(manifest.network_typed()?),
        "canonical_path": canonical_target,
        "canonical_schema_version": CANONICAL_STORE_SCHEMA_VERSION,
        "visible_tip_height": tip.height.value(),
        "wallet_next_step": "start zinder-projector against this canonical path to rebuild the wallet",
    }))
}

fn require_manifest_digest(expected: &str, observed: &str, field: &str) -> Result<()> {
    let decoded = hex::decode(expected)
        .map_err(|_| eyre!("{field} SHA-256 must be lowercase 32-byte hexadecimal"))?;
    if decoded.len() != 32 || hex::encode(decoded) != expected {
        return Err(eyre!(
            "{field} SHA-256 must be lowercase 32-byte hexadecimal"
        ));
    }
    if expected != observed {
        return Err(eyre!("{field} SHA-256 does not match"));
    }
    Ok(())
}

fn bounded_nonzero(count: u32, maximum: u32, field: &str) -> Result<NonZeroU32> {
    if !(1..=maximum).contains(&count) {
        return Err(eyre!("{field} must be between 1 and {maximum}"));
    }
    NonZeroU32::new(count).ok_or_else(|| eyre!("{field} must be nonzero"))
}

#[allow(
    clippy::print_stdout,
    reason = "zinderctl emits its requested machine-readable command result on stdout"
)]
fn print_json(document: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(document)?);
    Ok(())
}
