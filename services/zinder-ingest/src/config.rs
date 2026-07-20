//! Configuration loading for the `zinder-ingest` binary.
//!
//! [`IngestCommandConfig`] resolves the unified loop's input
//! (`zinder-ingest --config X` default invocation and `zinder-ingest probe`).

use std::{
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::BlockHeight;
use zinder_ingest::{
    CanonicalConstructionSettings, CanonicalFollowSettings, CanonicalPipelineLimits,
    CanonicalRunOverrides, ConventionalFeeDistributionBackfillConfig,
    DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
    DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, IngestError, IngestRuntimeConfig,
    MaterializedViewReplayConfig, MaterializedViewReplayPolicy, NodeSourceKind,
    PhaseClassificationConfig, RawBlobPolicy, TransactionComponentBackfillConfig,
    container_memory_budget_bytes,
};
use zinder_materialized_views::ProjectionPreset;
use zinder_runtime::{
    ConfigError, ConfigLoader, IngestControlSection, IngestControlWriterToml, NetworkSection,
    NetworkToml, NodeToml, OpsSection, OpsToml, PrimaryStorageSection, ResolvedIngestControlWriter,
    ResolvedPrimaryStorage, ResolvedRetention, RetentionSection, RetentionToml, SecuritySection,
    SecurityToml, ServiceIdentifier, StorageRoleSection, StorageRoleToml, duration_as_millis_u64,
    guard_optional_serving_bind, require_field, resolve_allow_public_bind,
    resolve_canonical_reader_rocksdb_budget, resolve_ingest_control_writer,
    resolve_ops_listen_addr, resolve_primary_storage, resolve_retention,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::RocksDbResourceBudget;

use crate::cli::parse::{
    parse_canonical_batch_max_blocks, parse_node_source, parse_poll_interval_ms,
    parse_reorg_window_blocks,
};

const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const DEFAULT_CANONICAL_BATCH_MAX_BLOCKS: u32 = 1_000;
const DEFAULT_CANONICAL_BATCH_MAX_ARTIFACT_BYTES: u64 = 536_870_912;
const FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES: u64 = 536_870_912; // 512 MiB

// Floor and divisor for the container-aware default. The divisor
// expresses how thinly the pipeline carves up the container's memory
// budget. Source-fetch and preparation limits use the same policy through
// `CanonicalPipelineLimits`; this helper remains for the canonical write and
// commit-reassembly bounds owned by the runtime configuration.
// At `/ 64` each bound claims about 1.6% of the
// container, leaving the remaining ~94% for RocksDB working set,
// per-batch write-batch amplification, allocator overhead beyond the
// pipeline's admission estimates, in-flight commit futures, and the
// query / explorer / multiplexer planes that share the same container.
// The 7x amplification observed on mainnet around blocks 297-298k
// (`estimated_write_bytes=510 MB` correlated with 22.7 GB resident
// memory at a 24 GB cap) is what set this divisor; tighter divisors
// trade catchup throughput for headroom.
//
// `MIN_PIPELINE_QUEUE_BYTES` keeps very small containers (single-digit
// GB hosts the team occasionally runs locally) from collapsing the
// queue to a value smaller than a single mainnet block can produce.
const MIN_PIPELINE_QUEUE_BYTES: u64 = 134_217_728; // 128 MiB
const PIPELINE_QUEUE_DIVISOR: u64 = 64;

fn default_pipeline_queue_bytes(fallback_bytes: u64) -> u64 {
    default_pipeline_queue_bytes_from_budget(container_memory_budget_bytes(), fallback_bytes)
}

fn default_pipeline_queue_bytes_from_budget(
    container_budget: Option<u64>,
    fallback_bytes: u64,
) -> u64 {
    container_budget.map_or(fallback_bytes, |budget| {
        (budget / PIPELINE_QUEUE_DIVISOR).clamp(MIN_PIPELINE_QUEUE_BYTES, fallback_bytes)
    })
}
const DEFAULT_FLUSH_INTERVAL_EPOCHS: u32 = 5;
const DEFAULT_MATERIALIZED_VIEW_REPLAY_BATCH_BLOCKS: u32 = 100;
const DEFAULT_MATERIALIZED_VIEW_REPLAY_MIN_BATCH_BLOCKS: u32 = 10;
const DEFAULT_MATERIALIZED_VIEW_STARTUP_HANDOFF_LAG_BLOCKS: u32 = 1_000;
const DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_DEGRADE_RATIO: f64 = 0.90;
const DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_PAUSE_RATIO: f64 = 0.99;
const DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_RESUME_RATIO: f64 = 0.80;
const DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_ALLOW_NEAR_TIP_FINALIZE: bool = false;
const DEFAULT_CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL_ENABLED: bool = true;
const DEFAULT_CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL_BATCH_BLOCKS: u32 = 256;
const DEFAULT_TRANSACTION_COMPONENT_BACKFILL_ENABLED: bool = true;
const DEFAULT_TRANSACTION_COMPONENT_BACKFILL_BATCH_BLOCKS: u32 = 256;
const DEFAULT_INGEST_COVERAGE: IngestCoverage = IngestCoverage::Explicit;
const DEFAULT_RAW_BLOB_POLICY: RawBlobPolicy = RawBlobPolicy::None;
const DEFAULT_PROJECTION_PRESET: ProjectionPreset = ProjectionPreset::Wallet;

/// Fully loaded command configuration for the unified `zinder-ingest`
/// run (no subcommand and the `probe` subcommand both consume this).
#[derive(Debug)]
pub(crate) struct IngestCommandConfig {
    pub(crate) runtime_config: IngestRuntimeConfig,
    pub(crate) projection_preset: ProjectionPreset,
    pub(crate) conventional_fee_distribution_backfill: ConventionalFeeDistributionBackfillConfig,
    pub(crate) transaction_component_backfill: TransactionComponentBackfillConfig,
    pub(crate) coverage: IngestCoverage,
    pub(crate) ingest_control_listen_addr: Option<SocketAddr>,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_checkpoint_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<zinder_runtime::BearerToken>,
    pub(crate) ingest_control_checkpoint_bearer_token: Option<zinder_runtime::BearerToken>,
    pub(crate) ingest_control_checkpoint_staging_root: PathBuf,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) allow_public_bind: bool,
    pub(crate) retention: ResolvedRetention,
}

/// Coverage policy applied to the [`CanonicalRunOverrides`] bootstrap path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IngestCoverage {
    /// Use explicitly supplied modifier heights as-is.
    Explicit,
    /// Derive the historical floor needed by lightwalletd-compatible
    /// wallets. The unified loop looks up the checkpoint against the
    /// upstream node before entering the first phase.
    WalletServing,
}

impl IngestCoverage {
    pub(crate) const fn as_kebab_case(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::WalletServing => "wallet-serving",
        }
    }
}

impl Default for IngestCoverage {
    fn default() -> Self {
        DEFAULT_INGEST_COVERAGE
    }
}

/// Fully loaded command configuration for
/// `zinder-ingest verify-canonical-replay`.
#[derive(Debug)]
pub(crate) struct CanonicalReplayVerificationCommandConfig {
    pub(crate) network: zinder_core::Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) secondary_path: PathBuf,
    pub(crate) canonical_rocksdb_budget: RocksDbResourceBudget,
}

/// Command-line overrides for the unified ingest invocation.
#[derive(Debug, Default)]
pub(crate) struct IngestConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) node_source: Option<String>,
    pub(crate) json_rpc_addr: Option<String>,
    pub(crate) node_auth_method: Option<String>,
    pub(crate) node_auth_username: Option<String>,
    pub(crate) node_auth_path: Option<PathBuf>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) projection_preset: Option<String>,
    pub(crate) request_timeout_secs: Option<u64>,
    pub(crate) max_response_bytes: Option<u64>,
    pub(crate) reorg_window_blocks: Option<u32>,
    pub(crate) catchup_threshold_blocks: Option<u32>,
    pub(crate) canonical_batch_max_blocks: Option<u32>,
    pub(crate) canonical_batch_max_artifact_bytes: Option<u64>,
    pub(crate) canonical_batch_max_estimated_write_bytes: Option<u64>,
    pub(crate) canonical_batch_min_blocks_before_estimated_write_close: Option<u32>,
    pub(crate) source_segment_max_blocks: Option<u32>,
    pub(crate) source_segment_target_response_bytes: Option<u64>,
    pub(crate) source_fetch_max_in_flight_requests: Option<u32>,
    pub(crate) source_fetch_max_in_flight_bytes: Option<u64>,
    pub(crate) block_prepare_concurrency: Option<u32>,
    pub(crate) poll_interval_ms: Option<u64>,
    pub(crate) lag_threshold_blocks: Option<u64>,
    pub(crate) target_height: Option<u32>,
    pub(crate) checkpoint_height: Option<u32>,
    pub(crate) allow_near_tip_finalize: Option<bool>,
    pub(crate) wallet_serving: Option<bool>,
    pub(crate) ingest_control_listen_addr: Option<SocketAddr>,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_checkpoint_bearer_token_path: Option<PathBuf>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
}

/// Command-line overrides for canonical replay verification.
#[derive(Debug, Default)]
pub(crate) struct CanonicalReplayVerificationConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
}

/// Error returned while resolving command configuration.
#[derive(Debug, Error)]
pub(crate) enum IngestConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Ingest(#[from] IngestError),

    #[error(transparent)]
    CanonicalWriter(#[from] zinder_ingest::CanonicalWriterError),

    #[error("canonical writer requires ingest.projection_preset=wallet")]
    CanonicalWriterRequiresWallet,

    #[error(transparent)]
    CanonicalReplayVerification(
        #[from] crate::replay_verification::CanonicalReplayVerificationError,
    ),
}

/// Loads and validates the unified ingest configuration.
#[allow(
    clippy::too_many_lines,
    reason = "the override chain is one auditable list of TOML keys; splitting it into helpers would scatter the precedence contract across multiple sites."
)]
pub(crate) fn load_ingest_config(
    config_path: Option<PathBuf>,
    overrides: IngestConfigOverrides,
) -> Result<IngestCommandConfig, IngestConfigError> {
    let raw_config: IngestConfig = ConfigLoader::new()
        // Storage default matches the canonical Zinder layout. The writer's
        // primary store lives at `/var/lib/zinder/store`. Operators on
        // non-PaaS hosts override via `ZINDER_STORAGE__PATH` env var or the
        // `--storage-path` CLI flag.
        .with_default("storage.path", "/var/lib/zinder/store")?
        .with_default(
            "ingest.projection_preset",
            DEFAULT_PROJECTION_PRESET.as_str(),
        )?
        .with_default("ingest.reorg_window_blocks", DEFAULT_REORG_WINDOW_BLOCKS)?
        .with_default(
            "ingest.construction.canonical_batch_max_blocks",
            DEFAULT_CANONICAL_BATCH_MAX_BLOCKS,
        )?
        .with_default(
            "ingest.construction.canonical_batch_max_artifact_bytes",
            DEFAULT_CANONICAL_BATCH_MAX_ARTIFACT_BYTES,
        )?
        .with_default(
            "ingest.construction.canonical_batch_max_estimated_write_bytes",
            default_pipeline_queue_bytes(DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES),
        )?
        .with_default(
            "ingest.construction.canonical_batch_min_blocks_before_estimated_write_close",
            DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )?
        .with_default(
            "ingest.construction.commit_reassembly_max_queued_artifact_bytes",
            default_pipeline_queue_bytes(FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES),
        )?
        .with_default(
            "ingest.materialized_views.replay_batch_blocks",
            DEFAULT_MATERIALIZED_VIEW_REPLAY_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.materialized_views.replay_policy",
            MaterializedViewReplayPolicy::DEFAULT.as_kebab_case(),
        )?
        .with_default(
            "ingest.materialized_views.memory_degrade_ratio",
            DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_DEGRADE_RATIO,
        )?
        .with_default(
            "ingest.materialized_views.memory_pause_ratio",
            DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_PAUSE_RATIO,
        )?
        .with_default(
            "ingest.materialized_views.memory_resume_ratio",
            DEFAULT_MATERIALIZED_VIEW_REPLAY_MEMORY_RESUME_RATIO,
        )?
        .with_default(
            "ingest.materialized_views.min_replay_batch_blocks",
            DEFAULT_MATERIALIZED_VIEW_REPLAY_MIN_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.materialized_views.startup_handoff_lag_blocks",
            DEFAULT_MATERIALIZED_VIEW_STARTUP_HANDOFF_LAG_BLOCKS,
        )?
        .with_default(
            "ingest.conventional_fee_distribution_backfill.enabled",
            DEFAULT_CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL_ENABLED,
        )?
        .with_default(
            "ingest.conventional_fee_distribution_backfill.batch_blocks",
            DEFAULT_CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.transaction_component_backfill.enabled",
            DEFAULT_TRANSACTION_COMPONENT_BACKFILL_ENABLED,
        )?
        .with_default(
            "ingest.transaction_component_backfill.batch_blocks",
            DEFAULT_TRANSACTION_COMPONENT_BACKFILL_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.construction.flush_interval_epochs",
            DEFAULT_FLUSH_INTERVAL_EPOCHS,
        )?
        .with_default(
            "ingest.follow.poll_interval_ms",
            DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS,
        )?
        .with_default(
            "ingest.follow.lag_threshold_blocks",
            DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        )?
        .with_default(
            "ingest.run_overrides.allow_near_tip_finalize",
            DEFAULT_ALLOW_NEAR_TIP_FINALIZE,
        )?
        .with_default(
            "ingest.run_overrides.coverage",
            DEFAULT_INGEST_COVERAGE.as_kebab_case(),
        )?
        .with_ops_section(ServiceIdentifier::Ingest)?
        .with_security_section()?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_if("ingest.source", overrides.node_source)?
        .with_override_if("node.json_rpc_addr", overrides.json_rpc_addr)?
        .with_override_if("node.auth.method", overrides.node_auth_method)?
        .with_override_if("node.auth.username", overrides.node_auth_username)?
        .with_override_path_if("node.auth.path", overrides.node_auth_path)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_if("ingest.projection_preset", overrides.projection_preset)?
        .with_override_if("node.request_timeout_secs", overrides.request_timeout_secs)?
        .with_override_if("node.max_response_bytes", overrides.max_response_bytes)?
        .with_override_if("ingest.reorg_window_blocks", overrides.reorg_window_blocks)?
        .with_override_if(
            "ingest.phase_classification.catchup_threshold_blocks",
            overrides.catchup_threshold_blocks,
        )?
        .with_override_if(
            "ingest.construction.canonical_batch_max_blocks",
            overrides.canonical_batch_max_blocks,
        )?
        .with_override_if(
            "ingest.construction.canonical_batch_max_artifact_bytes",
            overrides.canonical_batch_max_artifact_bytes,
        )?
        .with_override_if(
            "ingest.construction.canonical_batch_max_estimated_write_bytes",
            overrides.canonical_batch_max_estimated_write_bytes,
        )?
        .with_override_if(
            "ingest.construction.canonical_batch_min_blocks_before_estimated_write_close",
            overrides.canonical_batch_min_blocks_before_estimated_write_close,
        )?
        .with_override_if(
            "ingest.construction.source_segment_max_blocks",
            overrides.source_segment_max_blocks,
        )?
        .with_override_if(
            "ingest.construction.source_segment_target_response_bytes",
            overrides.source_segment_target_response_bytes,
        )?
        .with_override_if(
            "ingest.construction.source_fetch_max_in_flight_requests",
            overrides.source_fetch_max_in_flight_requests,
        )?
        .with_override_if(
            "ingest.construction.source_fetch_max_in_flight_bytes",
            overrides.source_fetch_max_in_flight_bytes,
        )?
        .with_override_if(
            "ingest.construction.block_prepare_concurrency",
            overrides.block_prepare_concurrency,
        )?
        .with_override_if("ingest.follow.poll_interval_ms", overrides.poll_interval_ms)?
        .with_override_if(
            "ingest.follow.lag_threshold_blocks",
            overrides.lag_threshold_blocks,
        )?
        .with_override_if(
            "ingest.run_overrides.target_height",
            overrides.target_height,
        )?
        .with_override_if(
            "ingest.run_overrides.checkpoint_height",
            overrides.checkpoint_height,
        )?
        .with_override_if(
            "ingest.run_overrides.allow_near_tip_finalize",
            overrides.allow_near_tip_finalize,
        )?
        .with_override_if(
            "ingest.run_overrides.coverage",
            (overrides.wallet_serving == Some(true))
                .then_some(IngestCoverage::WalletServing.as_kebab_case()),
        )?
        .with_override_if(
            "ingest_control.listen_addr",
            overrides
                .ingest_control_listen_addr
                .map(|addr| addr.to_string()),
        )?
        .with_override_path_if(
            "ingest_control.bearer_token_path",
            overrides.ingest_control_bearer_token_path,
        )?
        .with_override_path_if(
            "ingest_control.checkpoint_bearer_token_path",
            overrides.ingest_control_checkpoint_bearer_token_path,
        )?
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
        )?
        .load()?;

    resolve_ingest_config(raw_config)
}

/// Loads and validates canonical replay verification configuration.
pub(crate) fn load_canonical_replay_verification_config(
    config_path: Option<PathBuf>,
    overrides: CanonicalReplayVerificationConfigOverrides,
) -> Result<CanonicalReplayVerificationCommandConfig, IngestConfigError> {
    let raw_config: IngestConfig = ConfigLoader::new()
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_path_if("storage.secondary_path", overrides.secondary_path)?
        .load()?;

    resolve_canonical_replay_verification_config(raw_config)
}

/// Renders the effective ingest configuration in the accepted TOML
/// shape.
pub(crate) fn redacted_ingest_config_toml(
    config: &IngestCommandConfig,
) -> Result<String, IngestConfigError> {
    let rendered = toml::to_string(&RedactedIngestConfigToml::from_ingest_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    let effective_projection_identities = config
        .projection_preset
        .consumer_schemas()
        .iter()
        .map(|schema| format!("\"{}\"", schema.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "# effective_projection_identities = [{effective_projection_identities}]\n{rendered}"
    ))
}

/// Renders the effective canonical replay verification configuration in the
/// accepted TOML shape.
pub(crate) fn redacted_canonical_replay_verification_config_toml(
    config: &CanonicalReplayVerificationCommandConfig,
) -> Result<String, IngestConfigError> {
    toml::to_string(
        &RedactedCanonicalReplayVerificationConfigToml::from_verification_config(config),
    )
    .map_err(|source| ConfigError::Render { source }.into())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestConfig {
    network: NetworkSection,
    ops: OpsSection,
    node: NodeSection,
    storage: IngestPrimaryStorageSection,
    ingest: IngestSection,
    ingest_control: IngestControlSection,
    retention: RetentionSection,
    security: SecuritySection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestPrimaryStorageSection {
    path: Option<PathBuf>,
    secondary_path: Option<PathBuf>,
    canonical: StorageRoleSection,
    materialized_views: StorageRoleSection,
    raw_blob_policy: Option<RawBlobPolicy>,
}

impl IngestPrimaryStorageSection {
    fn into_primary_storage(self) -> PrimaryStorageSection {
        PrimaryStorageSection {
            path: self.path,
            canonical: self.canonical,
            materialized_views: self.materialized_views,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestSection {
    /// Canonical source-adapter selector. The supported value is
    /// `zebra-json-rpc`.
    source: Option<String>,
    /// Closed materialized-view workload. Omitted configuration defaults to `explorer`.
    projection_preset: Option<String>,
    /// Chain-truth invariant: how deep into the upstream tip the
    /// settled-tip cliff sits. Defaults to `100`.
    reorg_window_blocks: Option<u32>,
    /// Phase classifier knobs.
    phase_classification: IngestPhaseClassificationSection,
    /// Shared materialized-view replay knobs.
    materialized_views: IngestMaterializedViewsSection,
    /// Historical ZIP-317 conventional-fee distribution projection.
    conventional_fee_distribution_backfill: IngestConventionalFeeDistributionBackfillSection,
    /// Settled historical transaction-component projection.
    transaction_component_backfill: IngestTransactionComponentBackfillSection,
    /// Pipelined-fetch knobs for bulk catch-up.
    construction: IngestConstructionSection,
    /// Serial-loop knobs for tip-follow.
    follow: IngestFollowSection,
    /// One-shot `run_overrides` for the unified loop.
    run_overrides: IngestRunOverridesSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestPhaseClassificationSection {
    catchup_threshold_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestMaterializedViewsSection {
    #[serde(rename = "replay_batch_blocks")]
    batch_blocks: Option<u32>,
    #[serde(rename = "replay_policy")]
    policy: Option<String>,
    memory_budget_bytes: Option<u64>,
    memory_degrade_ratio: Option<f64>,
    memory_pause_ratio: Option<f64>,
    memory_resume_ratio: Option<f64>,
    min_replay_batch_blocks: Option<u32>,
    startup_handoff_lag_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestConventionalFeeDistributionBackfillSection {
    enabled: Option<bool>,
    batch_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestTransactionComponentBackfillSection {
    enabled: Option<bool>,
    batch_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestConstructionSection {
    canonical_batch_max_blocks: Option<u32>,
    canonical_batch_max_artifact_bytes: Option<u64>,
    canonical_batch_max_estimated_write_bytes: Option<u64>,
    canonical_batch_min_blocks_before_estimated_write_close: Option<u32>,
    source_segment_max_blocks: Option<u32>,
    source_segment_target_response_bytes: Option<u64>,
    source_fetch_max_in_flight_requests: Option<u32>,
    source_fetch_max_in_flight_bytes: Option<u64>,
    block_prepare_concurrency: Option<u32>,
    block_prepare_memory_watermark_bytes: Option<u64>,
    commit_reassembly_max_queued_artifact_bytes: Option<u64>,
    flush_interval_epochs: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestFollowSection {
    poll_interval_ms: Option<u64>,
    lag_threshold_blocks: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestRunOverridesSection {
    target_height: Option<u32>,
    checkpoint_height: Option<u32>,
    allow_near_tip_finalize: Option<bool>,
    coverage: Option<IngestCoverage>,
}

const fn node_source_name(node_source: NodeSourceKind) -> &'static str {
    match node_source {
        NodeSourceKind::ZebraJsonRpc => "zebra-json-rpc",
    }
}

fn parse_materialized_view_replay_policy(
    policy_text: &str,
) -> Result<MaterializedViewReplayPolicy, ConfigError> {
    match policy_text {
        "canonical-first" => Ok(MaterializedViewReplayPolicy::CanonicalFirst),
        "continuous" => Ok(MaterializedViewReplayPolicy::Continuous),
        _ => Err(ConfigError::invalid(
            "ingest.materialized_views.replay_policy must be one of: canonical-first, continuous",
        )),
    }
}

fn parse_projection_preset(preset_text: &str) -> Result<ProjectionPreset, ConfigError> {
    match preset_text {
        "wallet" => Ok(ProjectionPreset::Wallet),
        "explorer" => Ok(ProjectionPreset::Explorer),
        _ => Err(ConfigError::invalid(
            "ingest.projection_preset must be one of: wallet, explorer",
        )),
    }
}

fn nonzero_u32_config(amount: Option<u32>, path: &'static str) -> Result<NonZeroU32, ConfigError> {
    let amount = require_field(amount, path)?;
    NonZeroU32::new(amount)
        .ok_or_else(|| ConfigError::invalid(format!("{path} must be greater than zero")))
}

fn nonzero_u64_config(amount: Option<u64>, path: &'static str) -> Result<NonZeroU64, ConfigError> {
    let amount = require_field(amount, path)?;
    NonZeroU64::new(amount)
        .ok_or_else(|| ConfigError::invalid(format!("{path} must be greater than zero")))
}

fn optional_nonzero_u64_config(
    amount: Option<u64>,
    path: &'static str,
) -> Result<Option<NonZeroU64>, ConfigError> {
    amount
        .map(|amount| {
            NonZeroU64::new(amount)
                .ok_or_else(|| ConfigError::invalid(format!("{path} must be greater than zero")))
        })
        .transpose()
}

fn optional_nonzero_u32_config(
    amount: Option<u32>,
    path: &'static str,
) -> Result<Option<NonZeroU32>, ConfigError> {
    amount
        .map(|amount| {
            NonZeroU32::new(amount)
                .ok_or_else(|| ConfigError::invalid(format!("{path} must be greater than zero")))
        })
        .transpose()
}

fn available_logical_core_count() -> NonZeroU32 {
    let logical_core_count =
        std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get);
    NonZeroU32::new(u32::try_from(logical_core_count).unwrap_or(u32::MAX))
        .unwrap_or(NonZeroU32::MIN)
}

fn ratio_config(amount: Option<f64>, path: &'static str) -> Result<f64, ConfigError> {
    let amount = require_field(amount, path)?;
    if amount > 0.0 && amount <= 1.0 {
        return Ok(amount);
    }
    Err(ConfigError::invalid(format!(
        "{path} must be greater than zero and less than or equal to one"
    )))
}

#[allow(
    clippy::too_many_lines,
    reason = "the unified ingest resolver composes the network, source, storage, phase, bulk-catchup, tip-follow, and modifier knobs in one auditable validation sequence."
)]
fn resolve_ingest_config(config: IngestConfig) -> Result<IngestCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let node_target = NodeTarget::resolve(network, config.node).map_err(ConfigError::from)?;
    let resolved_pipeline_limits = CanonicalPipelineLimits::resolve(
        container_memory_budget_bytes().and_then(NonZeroU64::new),
        available_logical_core_count(),
        node_target.max_response_bytes,
    );
    let projection_preset_text = require_field(
        config.ingest.projection_preset.clone(),
        "ingest.projection_preset",
    )?;
    let projection_preset = parse_projection_preset(&projection_preset_text)?;
    let node_source_text = config
        .ingest
        .source
        .clone()
        .unwrap_or_else(|| node_source_name(NodeSourceKind::ZebraJsonRpc).to_owned());
    let node_source = parse_node_source(&node_source_text)?;
    let configured_raw_blob_policy = config.storage.raw_blob_policy;
    let ResolvedPrimaryStorage {
        path: storage_path,
        canonical_rocksdb_budget,
        materialized_view_rocksdb_budget,
    } = resolve_primary_storage(config.storage.into_primary_storage())?;

    let reorg_window_blocks = require_field(
        config.ingest.reorg_window_blocks,
        "ingest.reorg_window_blocks",
    )?;
    let reorg_window_blocks = parse_reorg_window_blocks(reorg_window_blocks)?;

    let catchup_threshold_blocks = config
        .ingest
        .phase_classification
        .catchup_threshold_blocks
        .unwrap_or(reorg_window_blocks);

    let canonical_batch_max_blocks_raw = require_field(
        config.ingest.construction.canonical_batch_max_blocks,
        "ingest.construction.canonical_batch_max_blocks",
    )?;
    let canonical_batch_max_blocks =
        parse_canonical_batch_max_blocks(canonical_batch_max_blocks_raw)?;

    let canonical_batch_max_artifact_bytes = nonzero_u64_config(
        config
            .ingest
            .construction
            .canonical_batch_max_artifact_bytes,
        "ingest.construction.canonical_batch_max_artifact_bytes",
    )?;
    let canonical_batch_max_estimated_write_bytes = nonzero_u64_config(
        config
            .ingest
            .construction
            .canonical_batch_max_estimated_write_bytes,
        "ingest.construction.canonical_batch_max_estimated_write_bytes",
    )?;
    let canonical_batch_min_blocks_before_estimated_write_close = nonzero_u32_config(
        config
            .ingest
            .construction
            .canonical_batch_min_blocks_before_estimated_write_close,
        "ingest.construction.canonical_batch_min_blocks_before_estimated_write_close",
    )?;
    if canonical_batch_min_blocks_before_estimated_write_close.get()
        > canonical_batch_max_blocks.get()
    {
        return Err(ConfigError::invalid(
            "ingest.construction.canonical_batch_min_blocks_before_estimated_write_close must be less than or equal to ingest.construction.canonical_batch_max_blocks",
        )
        .into());
    }

    let source_segment_max_blocks = optional_nonzero_u32_config(
        config.ingest.construction.source_segment_max_blocks,
        "ingest.construction.source_segment_max_blocks",
    )?
    .unwrap_or(resolved_pipeline_limits.source_segment_max_blocks);
    let block_prepare_concurrency = optional_nonzero_u32_config(
        config.ingest.construction.block_prepare_concurrency,
        "ingest.construction.block_prepare_concurrency",
    )?
    .unwrap_or(resolved_pipeline_limits.block_prepare_concurrency);
    let block_prepare_memory_watermark_bytes = optional_nonzero_u64_config(
        config
            .ingest
            .construction
            .block_prepare_memory_watermark_bytes,
        "ingest.construction.block_prepare_memory_watermark_bytes",
    )?
    .unwrap_or(resolved_pipeline_limits.block_prepare_memory_watermark_bytes);
    let commit_reassembly_max_queued_artifact_bytes = nonzero_u64_config(
        config
            .ingest
            .construction
            .commit_reassembly_max_queued_artifact_bytes,
        "ingest.construction.commit_reassembly_max_queued_artifact_bytes",
    )?;

    let source_segment_target_response_bytes = optional_nonzero_u64_config(
        config
            .ingest
            .construction
            .source_segment_target_response_bytes,
        "ingest.construction.source_segment_target_response_bytes",
    )?
    .unwrap_or(resolved_pipeline_limits.source_segment_target_response_bytes);
    let source_fetch_max_in_flight_requests = optional_nonzero_u32_config(
        config
            .ingest
            .construction
            .source_fetch_max_in_flight_requests,
        "ingest.construction.source_fetch_max_in_flight_requests",
    )?
    .unwrap_or(resolved_pipeline_limits.source_fetch_max_in_flight_requests);
    let source_fetch_max_in_flight_bytes = optional_nonzero_u64_config(
        config.ingest.construction.source_fetch_max_in_flight_bytes,
        "ingest.construction.source_fetch_max_in_flight_bytes",
    )?
    .unwrap_or(resolved_pipeline_limits.source_fetch_max_in_flight_bytes);
    let pipeline_limits = CanonicalPipelineLimits {
        max_response_bytes: node_target.max_response_bytes,
        source_segment_target_response_bytes,
        source_segment_max_blocks,
        source_fetch_max_in_flight_requests,
        source_fetch_max_in_flight_bytes,
        block_prepare_concurrency,
        block_prepare_memory_watermark_bytes,
    }
    .validate()
    .map_err(|error| {
        ConfigError::invalid(format!(
            "invalid ingest.construction pipeline limits: {error}"
        ))
    })?;

    let replay_batch_blocks_raw = require_field(
        config.ingest.materialized_views.batch_blocks,
        "ingest.materialized_views.replay_batch_blocks",
    )?;
    let replay_batch_blocks = NonZeroU32::new(replay_batch_blocks_raw).ok_or_else(|| {
        ConfigError::invalid(
            "ingest.materialized_views.replay_batch_blocks must be greater than zero",
        )
    })?;
    let replay_policy_raw = require_field(
        config.ingest.materialized_views.policy,
        "ingest.materialized_views.replay_policy",
    )?;
    let replay_policy = parse_materialized_view_replay_policy(&replay_policy_raw)?;
    let memory_budget_bytes = optional_nonzero_u64_config(
        config.ingest.materialized_views.memory_budget_bytes,
        "ingest.materialized_views.memory_budget_bytes",
    )?;
    let memory_degrade_ratio = ratio_config(
        config.ingest.materialized_views.memory_degrade_ratio,
        "ingest.materialized_views.memory_degrade_ratio",
    )?;
    let memory_pause_ratio = ratio_config(
        config.ingest.materialized_views.memory_pause_ratio,
        "ingest.materialized_views.memory_pause_ratio",
    )?;
    let memory_resume_ratio = ratio_config(
        config.ingest.materialized_views.memory_resume_ratio,
        "ingest.materialized_views.memory_resume_ratio",
    )?;
    if !(memory_resume_ratio < memory_degrade_ratio && memory_degrade_ratio < memory_pause_ratio) {
        return Err(ConfigError::invalid(
            "ingest.materialized_views memory ratios must satisfy memory_resume_ratio < memory_degrade_ratio < memory_pause_ratio",
        )
        .into());
    }
    let min_replay_batch_blocks = nonzero_u32_config(
        config.ingest.materialized_views.min_replay_batch_blocks,
        "ingest.materialized_views.min_replay_batch_blocks",
    )?;
    if min_replay_batch_blocks > replay_batch_blocks {
        return Err(ConfigError::invalid(
            "ingest.materialized_views.min_replay_batch_blocks must be less than or equal to ingest.materialized_views.replay_batch_blocks",
        )
        .into());
    }
    let startup_handoff_lag_blocks = u64::from(require_field(
        config.ingest.materialized_views.startup_handoff_lag_blocks,
        "ingest.materialized_views.startup_handoff_lag_blocks",
    )?);

    let conventional_fee_distribution_backfill = ConventionalFeeDistributionBackfillConfig {
        enabled: require_field(
            config.ingest.conventional_fee_distribution_backfill.enabled,
            "ingest.conventional_fee_distribution_backfill.enabled",
        )?,
        batch_blocks: nonzero_u32_config(
            config
                .ingest
                .conventional_fee_distribution_backfill
                .batch_blocks,
            "ingest.conventional_fee_distribution_backfill.batch_blocks",
        )?,
    };
    let transaction_component_backfill = TransactionComponentBackfillConfig {
        enabled: require_field(
            config.ingest.transaction_component_backfill.enabled,
            "ingest.transaction_component_backfill.enabled",
        )?,
        batch_blocks: nonzero_u32_config(
            config.ingest.transaction_component_backfill.batch_blocks,
            "ingest.transaction_component_backfill.batch_blocks",
        )?,
    };

    let flush_interval_epochs_raw = require_field(
        config.ingest.construction.flush_interval_epochs,
        "ingest.construction.flush_interval_epochs",
    )?;
    let flush_interval_epochs = NonZeroU32::new(flush_interval_epochs_raw).ok_or_else(|| {
        ConfigError::invalid("ingest.construction.flush_interval_epochs must be greater than zero")
    })?;

    let poll_interval_ms = require_field(
        config.ingest.follow.poll_interval_ms,
        "ingest.follow.poll_interval_ms",
    )?;
    let poll_interval = parse_poll_interval_ms(poll_interval_ms)?;

    let lag_threshold_blocks = require_field(
        config.ingest.follow.lag_threshold_blocks,
        "ingest.follow.lag_threshold_blocks",
    )?;

    let coverage = config.ingest.run_overrides.coverage.unwrap_or_default();
    let raw_blob_policy = resolve_raw_blob_policy(coverage, configured_raw_blob_policy)?;

    let allow_near_tip_finalize = require_field(
        config.ingest.run_overrides.allow_near_tip_finalize,
        "ingest.run_overrides.allow_near_tip_finalize",
    )?;
    if matches!(coverage, IngestCoverage::WalletServing) && allow_near_tip_finalize {
        return Err(ConfigError::invalid(
            "ingest.run_overrides.coverage = \"wallet-serving\" cannot be combined with ingest.run_overrides.allow_near_tip_finalize = true; serving stores must stop outside the reorg window",
        )
        .into());
    }
    if matches!(coverage, IngestCoverage::WalletServing)
        && config.ingest.run_overrides.checkpoint_height.is_some()
    {
        return Err(ConfigError::invalid(
            "ingest.run_overrides.coverage = \"wallet-serving\" derives checkpoint_height from the node; remove ingest.run_overrides.checkpoint_height",
        )
        .into());
    }

    let ResolvedIngestControlWriter {
        listen_addr: ingest_control_listen_addr,
        bearer_token_path: ingest_control_bearer_token_path,
        bearer_token: ingest_control_bearer_token,
        checkpoint_bearer_token: ingest_control_checkpoint_bearer_token,
        checkpoint_bearer_token_path: ingest_control_checkpoint_bearer_token_path,
        checkpoint_staging_root: ingest_control_checkpoint_staging_root,
    } = resolve_ingest_control_writer(config.ingest_control)?;
    let retention = resolve_retention(config.retention)?;
    let ops_listen_addr = resolve_ops_listen_addr(config.ops)?;
    let allow_public_bind = resolve_allow_public_bind(config.security)?;
    guard_optional_serving_bind(
        "ingest_control.listen_addr",
        ingest_control_listen_addr,
        allow_public_bind,
    )?;
    guard_ingest_control_bearer_token(
        ingest_control_listen_addr,
        ingest_control_bearer_token.as_ref(),
    )?;
    guard_optional_serving_bind("ops.listen_addr", ops_listen_addr, allow_public_bind)?;
    let run_overrides = CanonicalRunOverrides {
        target_height: config
            .ingest
            .run_overrides
            .target_height
            .map(BlockHeight::new),
        checkpoint_height: config
            .ingest
            .run_overrides
            .checkpoint_height
            .map(BlockHeight::new),
        allow_near_tip_finalize,
        checkpoint: None,
    };

    let runtime_config = IngestRuntimeConfig {
        node: node_target,
        node_source,
        storage_path,
        canonical_rocksdb_budget,
        materialized_view_rocksdb_budget,
        raw_blob_policy,
        reorg_window_blocks,
        phase_classification: PhaseClassificationConfig {
            catchup_threshold_blocks,
        },
        materialized_views: MaterializedViewReplayConfig {
            replay_batch_blocks,
            replay_policy,
            memory_budget_bytes,
            memory_degrade_ratio,
            memory_pause_ratio,
            memory_resume_ratio,
            min_replay_batch_blocks,
            startup_handoff_lag_blocks,
        },
        construction: CanonicalConstructionSettings {
            canonical_batch_max_blocks,
            canonical_batch_max_artifact_bytes,
            canonical_batch_max_estimated_write_bytes,
            canonical_batch_min_blocks_before_estimated_write_close,
            pipeline_limits,
            commit_reassembly_max_queued_artifact_bytes,
            flush_interval_epochs,
        },
        follow: CanonicalFollowSettings {
            poll_interval,
            lag_threshold_blocks,
        },
        run_overrides,
    };

    Ok(IngestCommandConfig {
        runtime_config,
        projection_preset,
        conventional_fee_distribution_backfill,
        transaction_component_backfill,
        coverage,
        ingest_control_listen_addr,
        ingest_control_bearer_token_path,
        ingest_control_checkpoint_bearer_token_path,
        ingest_control_bearer_token,
        ingest_control_checkpoint_bearer_token,
        ingest_control_checkpoint_staging_root,
        ops_listen_addr,
        allow_public_bind,
        retention,
    })
}

fn guard_ingest_control_bearer_token(
    listen_addr: Option<SocketAddr>,
    bearer_token: Option<&zinder_runtime::BearerToken>,
) -> Result<(), ConfigError> {
    if listen_addr.is_some_and(|address| !address.ip().is_loopback()) && bearer_token.is_none() {
        return Err(ConfigError::invalid(
            "ingest_control.listen_addr outside loopback requires ingest_control.bearer_token_path",
        ));
    }
    Ok(())
}

fn resolve_canonical_replay_verification_config(
    config: IngestConfig,
) -> Result<CanonicalReplayVerificationCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let secondary_path = config
        .storage
        .secondary_path
        .ok_or_else(|| ConfigError::missing_field("storage.secondary_path"))?;
    let canonical_rocksdb_budget =
        resolve_canonical_reader_rocksdb_budget(config.storage.canonical.rocksdb)?;

    Ok(CanonicalReplayVerificationCommandConfig {
        network,
        storage_path,
        secondary_path,
        canonical_rocksdb_budget,
    })
}

fn resolve_raw_blob_policy(
    coverage: IngestCoverage,
    configured_policy: Option<RawBlobPolicy>,
) -> Result<RawBlobPolicy, ConfigError> {
    match (coverage, configured_policy) {
        (IngestCoverage::WalletServing, None) => Ok(RawBlobPolicy::Transactions),
        (IngestCoverage::WalletServing, Some(RawBlobPolicy::None)) => Err(ConfigError::invalid(
            "ingest.run_overrides.coverage = \"wallet-serving\" requires storage.raw_blob_policy = \"transactions\" or \"all\"; remove storage.raw_blob_policy to use \"transactions\"",
        )),
        (_, Some(raw_blob_policy)) => Ok(raw_blob_policy),
        (_, None) => Ok(DEFAULT_RAW_BLOB_POLICY),
    }
}

#[derive(Serialize)]
struct RedactedIngestConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    security: SecurityToml,
    node: NodeToml,
    storage: IngestStorageToml,
    ingest: IngestToml,
    ingest_control: IngestControlWriterToml,
    retention: RetentionToml,
}

#[derive(Serialize)]
struct RedactedCanonicalReplayVerificationConfigToml {
    network: NetworkToml,
    storage: CanonicalReplayVerificationStorageToml,
}

impl RedactedIngestConfigToml {
    #[allow(
        clippy::too_many_lines,
        reason = "redacted print-config mirrors the resolved ingest TOML shape field by field"
    )]
    fn from_ingest_config(config: &IngestCommandConfig) -> Self {
        let runtime_config = &config.runtime_config;
        Self {
            network: NetworkToml::from_network(runtime_config.node.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            security: SecurityToml::from_resolved(config.allow_public_bind),
            node: NodeToml::from_node_target(&runtime_config.node),
            storage: IngestStorageToml {
                path: runtime_config.storage_path.display().to_string(),
                canonical: StorageRoleToml::from_resolved(runtime_config.canonical_rocksdb_budget),
                materialized_views: StorageRoleToml::from_resolved(
                    runtime_config.materialized_view_rocksdb_budget,
                ),
                raw_blob_policy: runtime_config.raw_blob_policy,
            },
            ingest: IngestToml {
                source: node_source_name(runtime_config.node_source),
                projection_preset: config.projection_preset.as_str(),
                reorg_window_blocks: runtime_config.reorg_window_blocks,
                phase_classification: IngestPhaseClassificationToml {
                    catchup_threshold_blocks: runtime_config
                        .phase_classification
                        .catchup_threshold_blocks,
                },
                materialized_views: IngestMaterializedViewsToml {
                    batch_blocks: runtime_config.materialized_views.replay_batch_blocks.get(),
                    policy: runtime_config
                        .materialized_views
                        .replay_policy
                        .as_kebab_case(),
                    memory_budget_bytes: runtime_config
                        .materialized_views
                        .memory_budget_bytes
                        .map(NonZeroU64::get),
                    memory_degrade_ratio: runtime_config.materialized_views.memory_degrade_ratio,
                    memory_pause_ratio: runtime_config.materialized_views.memory_pause_ratio,
                    memory_resume_ratio: runtime_config.materialized_views.memory_resume_ratio,
                    min_replay_batch_blocks: runtime_config
                        .materialized_views
                        .min_replay_batch_blocks
                        .get(),
                    startup_handoff_lag_blocks: runtime_config
                        .materialized_views
                        .startup_handoff_lag_blocks,
                },
                conventional_fee_distribution_backfill:
                    IngestConventionalFeeDistributionBackfillToml {
                        enabled: config.conventional_fee_distribution_backfill.enabled,
                        batch_blocks: config
                            .conventional_fee_distribution_backfill
                            .batch_blocks
                            .get(),
                    },
                transaction_component_backfill: IngestTransactionComponentBackfillToml {
                    enabled: config.transaction_component_backfill.enabled,
                    batch_blocks: config.transaction_component_backfill.batch_blocks.get(),
                },
                construction: IngestConstructionToml {
                    canonical_batch_max_blocks: runtime_config
                        .construction
                        .canonical_batch_max_blocks
                        .get(),
                    canonical_batch_max_artifact_bytes: runtime_config
                        .construction
                        .canonical_batch_max_artifact_bytes
                        .get(),
                    canonical_batch_max_estimated_write_bytes: runtime_config
                        .construction
                        .canonical_batch_max_estimated_write_bytes
                        .get(),
                    canonical_batch_min_blocks_before_estimated_write_close: runtime_config
                        .construction
                        .canonical_batch_min_blocks_before_estimated_write_close
                        .get(),
                    source_segment_max_blocks: runtime_config
                        .construction
                        .pipeline_limits
                        .source_segment_max_blocks
                        .get(),
                    source_segment_target_response_bytes: runtime_config
                        .construction
                        .pipeline_limits
                        .source_segment_target_response_bytes
                        .get(),
                    source_fetch_max_in_flight_requests: runtime_config
                        .construction
                        .pipeline_limits
                        .source_fetch_max_in_flight_requests
                        .get(),
                    source_fetch_max_in_flight_bytes: runtime_config
                        .construction
                        .pipeline_limits
                        .source_fetch_max_in_flight_bytes
                        .get(),
                    block_prepare_concurrency: runtime_config
                        .construction
                        .pipeline_limits
                        .block_prepare_concurrency
                        .get(),
                    block_prepare_memory_watermark_bytes: runtime_config
                        .construction
                        .pipeline_limits
                        .block_prepare_memory_watermark_bytes
                        .get(),
                    commit_reassembly_max_queued_artifact_bytes: runtime_config
                        .construction
                        .commit_reassembly_max_queued_artifact_bytes
                        .get(),
                    flush_interval_epochs: runtime_config.construction.flush_interval_epochs.get(),
                },
                follow: IngestFollowToml {
                    poll_interval_ms: duration_as_millis_u64(runtime_config.follow.poll_interval),
                    lag_threshold_blocks: runtime_config.follow.lag_threshold_blocks,
                },
                run_overrides: IngestRunOverridesToml {
                    target_height: runtime_config
                        .run_overrides
                        .target_height
                        .map(BlockHeight::value),
                    checkpoint_height: runtime_config
                        .run_overrides
                        .checkpoint_height
                        .map(BlockHeight::value),
                    allow_near_tip_finalize: runtime_config.run_overrides.allow_near_tip_finalize,
                    coverage: config.coverage,
                },
            },
            ingest_control: IngestControlWriterToml::from_resolved(
                config.ingest_control_listen_addr,
                config.ingest_control_bearer_token_path.as_deref(),
                config
                    .ingest_control_checkpoint_bearer_token_path
                    .as_deref(),
                &config.ingest_control_checkpoint_staging_root,
            ),
            retention: RetentionToml::from_resolved(config.retention),
        }
    }
}

impl RedactedCanonicalReplayVerificationConfigToml {
    fn from_verification_config(config: &CanonicalReplayVerificationCommandConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            storage: CanonicalReplayVerificationStorageToml {
                path: config.storage_path.display().to_string(),
                secondary_path: config.secondary_path.display().to_string(),
                canonical: StorageRoleToml::from_resolved(config.canonical_rocksdb_budget),
            },
        }
    }
}

#[derive(Serialize)]
struct IngestToml {
    source: &'static str,
    projection_preset: &'static str,
    reorg_window_blocks: u32,
    phase_classification: IngestPhaseClassificationToml,
    materialized_views: IngestMaterializedViewsToml,
    conventional_fee_distribution_backfill: IngestConventionalFeeDistributionBackfillToml,
    transaction_component_backfill: IngestTransactionComponentBackfillToml,
    construction: IngestConstructionToml,
    follow: IngestFollowToml,
    run_overrides: IngestRunOverridesToml,
}

#[derive(Serialize)]
struct IngestConventionalFeeDistributionBackfillToml {
    enabled: bool,
    batch_blocks: u32,
}

#[derive(Serialize)]
struct IngestTransactionComponentBackfillToml {
    enabled: bool,
    batch_blocks: u32,
}

#[derive(Serialize)]
struct IngestStorageToml {
    path: String,
    canonical: StorageRoleToml,
    materialized_views: StorageRoleToml,
    raw_blob_policy: RawBlobPolicy,
}

#[derive(Serialize)]
struct CanonicalReplayVerificationStorageToml {
    path: String,
    secondary_path: String,
    canonical: StorageRoleToml,
}

#[derive(Serialize)]
struct IngestPhaseClassificationToml {
    catchup_threshold_blocks: u32,
}

#[derive(Serialize)]
struct IngestMaterializedViewsToml {
    #[serde(rename = "replay_batch_blocks")]
    batch_blocks: u32,
    #[serde(rename = "replay_policy")]
    policy: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_budget_bytes: Option<u64>,
    memory_degrade_ratio: f64,
    memory_pause_ratio: f64,
    memory_resume_ratio: f64,
    min_replay_batch_blocks: u32,
    startup_handoff_lag_blocks: u64,
}

#[derive(Serialize)]
struct IngestConstructionToml {
    canonical_batch_max_blocks: u32,
    canonical_batch_max_artifact_bytes: u64,
    canonical_batch_max_estimated_write_bytes: u64,
    canonical_batch_min_blocks_before_estimated_write_close: u32,
    source_segment_max_blocks: u32,
    source_segment_target_response_bytes: u64,
    source_fetch_max_in_flight_requests: u32,
    source_fetch_max_in_flight_bytes: u64,
    block_prepare_concurrency: u32,
    block_prepare_memory_watermark_bytes: u64,
    commit_reassembly_max_queued_artifact_bytes: u64,
    flush_interval_epochs: u32,
}

#[derive(Serialize)]
struct IngestFollowToml {
    poll_interval_ms: u64,
    lag_threshold_blocks: u64,
}

#[derive(Serialize)]
struct IngestRunOverridesToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    target_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint_height: Option<u32>,
    allow_near_tip_finalize: bool,
    coverage: IngestCoverage,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn pipeline_queue_falls_back_when_cgroup_absent() {
        let fallback = FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES;
        assert_eq!(
            default_pipeline_queue_bytes_from_budget(None, fallback),
            fallback,
            "dev hosts without cgroup keep the pre-existing 512 MiB defaults"
        );
    }

    #[test]
    fn pipeline_queue_caps_at_fallback_on_fat_containers() {
        let fallback = FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES;
        // 64 GiB container -> raw computation would give 1 GiB per queue,
        // but the fallback caps at the previously hand-tuned 512 MiB.
        let result = default_pipeline_queue_bytes_from_budget(Some(64 * ONE_GIB), fallback);
        assert_eq!(result, fallback);
    }

    #[test]
    fn pipeline_queue_shrinks_for_railway_sized_containers() {
        let fallback = FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES;
        // The Railway zinder-mainnet incident: 24 GiB container cap,
        // OOM at the 512 MiB default. Container-aware default must
        // be smaller than the fallback to prevent recurrence.
        let result = default_pipeline_queue_bytes_from_budget(Some(24 * ONE_GIB), fallback);
        assert_eq!(result, 24 * ONE_GIB / PIPELINE_QUEUE_DIVISOR);
        assert!(
            result < fallback,
            "Railway-sized containers must shrink below the 512 MiB fallback"
        );
    }

    #[test]
    fn pipeline_queue_floors_at_min_for_tight_containers() {
        let fallback = FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES;
        // 4 GiB container -> raw computation gives 64 MiB per queue,
        // smaller than a single mainnet block can produce. Floor at
        // 128 MiB so the pipeline can always make forward progress.
        let result = default_pipeline_queue_bytes_from_budget(Some(4 * ONE_GIB), fallback);
        assert_eq!(result, MIN_PIPELINE_QUEUE_BYTES);
    }

    #[test]
    fn conventional_fee_distribution_backfill_batch_rejects_zero() {
        let error = nonzero_u32_config(
            Some(0),
            "ingest.conventional_fee_distribution_backfill.batch_blocks",
        )
        .err()
        .unwrap_or_else(|| ConfigError::invalid("zero batch size was accepted"));
        assert!(error.to_string().contains("must be greater than zero"));
    }

    #[test]
    fn transaction_component_backfill_batch_rejects_zero() {
        let error = nonzero_u32_config(
            Some(0),
            "ingest.transaction_component_backfill.batch_blocks",
        )
        .err()
        .unwrap_or_else(|| ConfigError::invalid("zero batch size was accepted"));
        assert!(error.to_string().contains("must be greater than zero"));
    }

    #[test]
    fn non_loopback_ingest_control_requires_bearer_token() {
        let error =
            guard_ingest_control_bearer_token(Some(SocketAddr::from(([0, 0, 0, 0], 8240))), None)
                .err()
                .unwrap_or_else(|| ConfigError::invalid("public ingest control was accepted"));

        assert!(
            error
                .to_string()
                .contains("outside loopback requires ingest_control.bearer_token_path")
        );
        assert!(guard_ingest_control_bearer_token(
            Some(SocketAddr::from(([127, 0, 0, 1], 8240))),
            None,
        )
        .is_ok());
    }
}
