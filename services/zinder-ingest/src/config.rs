//! Configuration loading for the `zinder-ingest` binary.
//!
//! The ingest binary has two top-level config shapes:
//!
//! - [`IngestCommandConfig`] resolves the unified loop's input
//!   (`zinder-ingest --config X` default invocation, and `zinder-ingest
//!   probe`).
//! - [`BackupCommandConfig`] resolves the `backup` subcommand. Backup
//!   keeps its own command config because it does not run the loop.

use std::{
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::BlockHeight;
use zinder_ingest::{
    BulkCatchupConfig, ChainEventRetentionConfig,
    DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
    DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
    DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS, DeriveReplayPolicy, IngestDeriveConfig, IngestError,
    IngestLoopConfig, IngestModifiers, MempoolEventRetentionWorkerConfig, NodeSourceKind,
    PhasesConfig, RawBlobPolicy, TipFollowPhaseConfig, container_memory_budget_bytes,
};
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, IngestControlSection,
    IngestControlWriterToml, NetworkSection, NetworkToml, NodeToml, OpsSection, OpsToml,
    PrimaryStorageSection, PrimaryStorageToml, ResolvedIngestControlWriter, ResolvedPrimaryStorage,
    ResolvedRetention, RetentionSection, RetentionToml, ServiceIdentifier, StorageRoleSection,
    StorageRoleToml, duration_as_millis_u64, require_field, resolve_ingest_control_writer,
    resolve_ops_listen_addr, resolve_primary_storage, resolve_retention,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::{MempoolEventRetentionConfig, RocksDbResourceBudget};

use crate::cli::parse::{
    parse_canonical_batch_max_blocks, parse_node_source, parse_poll_interval_ms,
    parse_reorg_window_blocks,
};

const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const DEFAULT_CANONICAL_BATCH_MAX_BLOCKS: u32 = 1_000;
const DEFAULT_CANONICAL_BATCH_MAX_ARTIFACT_BYTES: u64 = 536_870_912;
const DEFAULT_SOURCE_SEGMENT_MAX_BLOCKS: u32 = 16;
const DEFAULT_SOURCE_SEGMENT_TARGET_RESPONSE_BYTES: u64 = 33_554_432;
const DEFAULT_SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS: u32 = 12;

// Bulk-catchup pipeline queue caps fall through to these constants when
// the container memory budget can't be detected (dev hosts, macOS, older
// Linux without cgroup v2). On a container runtime (Railway, Fly, ECS,
// k8s, plain Docker) the actual defaults come from `default_pipeline_queue_bytes`,
// which sizes each queue as `container_memory / PIPELINE_QUEUE_DIVISOR`
// so the total in-flight watermark budget stays well under the kernel's
// memory.max. Operator env-var overrides
// (`ZINDER_INGEST__BULK_CATCHUP__*_BYTES`) still win when set. See
// [ADR-0022](../../../docs/adrs/0022-resource-budgeted-bulk-catchup.md).
const FALLBACK_SOURCE_FETCH_MAX_IN_FLIGHT_BYTES: u64 = 402_653_184; // 384 MiB
const FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES: u64 = 536_870_912; // 512 MiB
const FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES: u64 = 536_870_912; // 512 MiB

// Floor and divisor for the container-aware default. The divisor
// expresses how thinly the pipeline carves up the container's memory
// budget: at `/ 64` the four queues together claim ~6% of the
// container, leaving the remaining ~94% for RocksDB working set,
// per-batch decoded-artifact amplification (a watermark counts
// "estimated artifact bytes"; the actual Rust struct memory is several
// times larger for dense ranges), in-flight commit futures, and the
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
const DEFAULT_DERIVE_REPLAY_BATCH_BLOCKS: u32 = 100;
const DEFAULT_DERIVE_REPLAY_MIN_BATCH_BLOCKS: u32 = 10;
const DEFAULT_DERIVE_REPLAY_MEMORY_DEGRADE_RATIO: f64 = 0.90;
const DEFAULT_DERIVE_REPLAY_MEMORY_PAUSE_RATIO: f64 = 0.99;
const DEFAULT_DERIVE_REPLAY_MEMORY_RESUME_RATIO: f64 = 0.80;
const BLOCK_PREPARE_CONCURRENCY_CEILING: u32 = 16;
const DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_ALLOW_NEAR_TIP_FINALIZE: bool = false;
const DEFAULT_INGEST_COVERAGE: IngestCoverage = IngestCoverage::Explicit;
const DEFAULT_RAW_BLOB_POLICY: RawBlobPolicy = RawBlobPolicy::None;

/// Fully loaded command configuration for the unified `zinder-ingest`
/// run (no subcommand and the `probe` subcommand both consume this).
#[derive(Debug)]
pub(crate) struct IngestCommandConfig {
    pub(crate) loop_config: IngestLoopConfig,
    pub(crate) coverage: IngestCoverage,
    pub(crate) ingest_control_listen_addr: Option<SocketAddr>,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) retention: ResolvedRetention,
}

impl IngestCommandConfig {
    pub(crate) fn chain_event_retention(&self) -> ChainEventRetentionConfig {
        ChainEventRetentionConfig {
            retention_window: self.retention.chain_event_window(),
            check_interval: self.retention.chain_event_check_interval(),
            cursor_at_risk_warning: self.retention.cursor_at_risk_warning(),
        }
    }

    pub(crate) fn mempool_event_retention(&self) -> MempoolEventRetentionWorkerConfig {
        MempoolEventRetentionWorkerConfig {
            retention: MempoolEventRetentionConfig::new(
                self.retention.mempool_mined_window(),
                self.retention.mempool_invalidated_window(),
            ),
            check_interval: self.retention.mempool_check_interval(),
            cursor_at_risk_warning: self.retention.mempool_cursor_at_risk_warning(),
        }
    }
}

/// Coverage policy applied to the [`IngestModifiers`] bootstrap path.
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

/// Fully loaded command configuration for `zinder-ingest backup`.
#[derive(Debug)]
pub(crate) struct BackupCommandConfig {
    pub(crate) network: zinder_core::Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) canonical_rocksdb_budget: RocksDbResourceBudget,
    pub(crate) derive_rocksdb_budget: RocksDbResourceBudget,
    pub(crate) to_path: PathBuf,
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
    pub(crate) ops_listen_addr: Option<SocketAddr>,
}

/// Command-line overrides for the backup command.
#[derive(Debug, Default)]
pub(crate) struct BackupConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) to_path: Option<PathBuf>,
}

/// Error returned while resolving command configuration.
#[derive(Debug, Error)]
pub(crate) enum IngestConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Ingest(#[from] IngestError),

    #[error("failed to bind ingest-control endpoint at {listen_addr}: {source}")]
    IngestControlBind {
        listen_addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("ingest-control endpoint failed: {source}")]
    IngestControlTransport {
        #[source]
        source: tonic::transport::Error,
    },

    #[error("invalid ingest-control bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
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
        .with_default("ingest.reorg_window_blocks", DEFAULT_REORG_WINDOW_BLOCKS)?
        .with_default(
            "ingest.bulk_catchup.canonical_batch_max_blocks",
            DEFAULT_CANONICAL_BATCH_MAX_BLOCKS,
        )?
        .with_default(
            "ingest.bulk_catchup.canonical_batch_max_artifact_bytes",
            DEFAULT_CANONICAL_BATCH_MAX_ARTIFACT_BYTES,
        )?
        .with_default(
            "ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes",
            default_pipeline_queue_bytes(DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES),
        )?
        .with_default(
            "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close",
            DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )?
        .with_default(
            "ingest.bulk_catchup.source_segment_max_blocks",
            DEFAULT_SOURCE_SEGMENT_MAX_BLOCKS,
        )?
        .with_default(
            "ingest.bulk_catchup.source_segment_target_response_bytes",
            DEFAULT_SOURCE_SEGMENT_TARGET_RESPONSE_BYTES,
        )?
        .with_default(
            "ingest.bulk_catchup.source_fetch_max_in_flight_requests",
            DEFAULT_SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS,
        )?
        .with_default(
            "ingest.bulk_catchup.source_fetch_max_in_flight_bytes",
            default_pipeline_queue_bytes(FALLBACK_SOURCE_FETCH_MAX_IN_FLIGHT_BYTES),
        )?
        .with_default(
            "ingest.bulk_catchup.block_prepare_concurrency",
            default_block_prepare_concurrency(),
        )?
        .with_default(
            "ingest.bulk_catchup.block_prepare_max_in_flight_artifact_bytes",
            default_pipeline_queue_bytes(FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES),
        )?
        .with_default(
            "ingest.bulk_catchup.commit_reassembly_max_queued_artifact_bytes",
            default_pipeline_queue_bytes(FALLBACK_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES),
        )?
        .with_default(
            "ingest.derive.replay_batch_blocks",
            DEFAULT_DERIVE_REPLAY_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.derive.replay_policy",
            DeriveReplayPolicy::DEFAULT.as_kebab_case(),
        )?
        .with_default(
            "ingest.derive.memory_degrade_ratio",
            DEFAULT_DERIVE_REPLAY_MEMORY_DEGRADE_RATIO,
        )?
        .with_default(
            "ingest.derive.memory_pause_ratio",
            DEFAULT_DERIVE_REPLAY_MEMORY_PAUSE_RATIO,
        )?
        .with_default(
            "ingest.derive.memory_resume_ratio",
            DEFAULT_DERIVE_REPLAY_MEMORY_RESUME_RATIO,
        )?
        .with_default(
            "ingest.derive.min_replay_batch_blocks",
            DEFAULT_DERIVE_REPLAY_MIN_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.bulk_catchup.flush_interval_epochs",
            DEFAULT_FLUSH_INTERVAL_EPOCHS,
        )?
        .with_default(
            "ingest.tip_follow.poll_interval_ms",
            DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS,
        )?
        .with_default(
            "ingest.tip_follow.lag_threshold_blocks",
            DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        )?
        .with_default(
            "ingest.modifiers.allow_near_tip_finalize",
            DEFAULT_ALLOW_NEAR_TIP_FINALIZE,
        )?
        .with_default(
            "ingest.modifiers.coverage",
            DEFAULT_INGEST_COVERAGE.as_kebab_case(),
        )?
        .with_default(
            "storage.raw_blob_policy",
            DEFAULT_RAW_BLOB_POLICY.as_kebab_case(),
        )?
        .with_ops_section(ServiceIdentifier::Ingest)?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_if("ingest.source", overrides.node_source)?
        .with_override_if("node.json_rpc_addr", overrides.json_rpc_addr)?
        .with_override_if("node.auth.method", overrides.node_auth_method)?
        .with_override_if("node.auth.username", overrides.node_auth_username)?
        .with_override_path_if("node.auth.path", overrides.node_auth_path)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_if("node.request_timeout_secs", overrides.request_timeout_secs)?
        .with_override_if("node.max_response_bytes", overrides.max_response_bytes)?
        .with_override_if("ingest.reorg_window_blocks", overrides.reorg_window_blocks)?
        .with_override_if(
            "ingest.phases.catchup_threshold_blocks",
            overrides.catchup_threshold_blocks,
        )?
        .with_override_if(
            "ingest.bulk_catchup.canonical_batch_max_blocks",
            overrides.canonical_batch_max_blocks,
        )?
        .with_override_if(
            "ingest.bulk_catchup.canonical_batch_max_artifact_bytes",
            overrides.canonical_batch_max_artifact_bytes,
        )?
        .with_override_if(
            "ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes",
            overrides.canonical_batch_max_estimated_write_bytes,
        )?
        .with_override_if(
            "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close",
            overrides.canonical_batch_min_blocks_before_estimated_write_close,
        )?
        .with_override_if(
            "ingest.bulk_catchup.source_segment_max_blocks",
            overrides.source_segment_max_blocks,
        )?
        .with_override_if(
            "ingest.bulk_catchup.source_segment_target_response_bytes",
            overrides.source_segment_target_response_bytes,
        )?
        .with_override_if(
            "ingest.bulk_catchup.source_fetch_max_in_flight_requests",
            overrides.source_fetch_max_in_flight_requests,
        )?
        .with_override_if(
            "ingest.bulk_catchup.source_fetch_max_in_flight_bytes",
            overrides.source_fetch_max_in_flight_bytes,
        )?
        .with_override_if(
            "ingest.bulk_catchup.block_prepare_concurrency",
            overrides.block_prepare_concurrency,
        )?
        .with_override_if(
            "ingest.tip_follow.poll_interval_ms",
            overrides.poll_interval_ms,
        )?
        .with_override_if(
            "ingest.tip_follow.lag_threshold_blocks",
            overrides.lag_threshold_blocks,
        )?
        .with_override_if("ingest.modifiers.target_height", overrides.target_height)?
        .with_override_if(
            "ingest.modifiers.checkpoint_height",
            overrides.checkpoint_height,
        )?
        .with_override_if(
            "ingest.modifiers.allow_near_tip_finalize",
            overrides.allow_near_tip_finalize,
        )?
        .with_override_if(
            "ingest.modifiers.coverage",
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
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
        )?
        .load()?;

    resolve_ingest_config(raw_config)
}

/// Loads and validates backup configuration.
pub(crate) fn load_backup_config(
    config_path: Option<PathBuf>,
    overrides: BackupConfigOverrides,
) -> Result<BackupCommandConfig, IngestConfigError> {
    let raw_config: IngestConfig = ConfigLoader::new()
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_path_if("backup.to_path", overrides.to_path)?
        .load()?;

    resolve_backup_config(raw_config)
}

/// Renders the effective ingest configuration in the accepted TOML
/// shape.
pub(crate) fn redacted_ingest_config_toml(
    config: &IngestCommandConfig,
) -> Result<String, IngestConfigError> {
    let rendered = toml::to_string(&RedactedIngestConfigToml::from_ingest_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
}

/// Renders the effective backup configuration in the accepted TOML
/// shape.
pub(crate) fn redacted_backup_config_toml(
    config: &BackupCommandConfig,
) -> Result<String, IngestConfigError> {
    let rendered = toml::to_string(&RedactedBackupConfigToml::from_backup_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
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
    backup: BackupSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestPrimaryStorageSection {
    path: Option<PathBuf>,
    canonical: StorageRoleSection,
    derive: StorageRoleSection,
    raw_blob_policy: Option<RawBlobPolicy>,
}

impl IngestPrimaryStorageSection {
    fn into_primary_storage(self) -> PrimaryStorageSection {
        PrimaryStorageSection {
            path: self.path,
            canonical: self.canonical,
            derive: self.derive,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestSection {
    /// Source-adapter selector. Today only `zebra-json-rpc` is
    /// implemented; [ADR-0016](../../../docs/adrs/0016-source-streaming-pipeline.md)
    /// reserves `auto`, `zebra-indexer-grpc`, and `zebra-in-process`.
    source: Option<String>,
    /// Chain-truth invariant: how deep into the upstream tip the
    /// safe-tip cliff sits. Defaults to `100`.
    reorg_window_blocks: Option<u32>,
    /// Phase classifier knobs.
    phases: IngestPhasesSection,
    /// Shared derive execution knobs.
    derive: IngestDeriveSection,
    /// Pipelined-fetch knobs for bulk catch-up.
    bulk_catchup: IngestBulkCatchupSection,
    /// Serial-loop knobs for tip-follow.
    tip_follow: IngestTipFollowSection,
    /// One-shot modifiers for the unified loop.
    modifiers: IngestModifiersSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestPhasesSection {
    catchup_threshold_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestDeriveSection {
    #[serde(rename = "replay_batch_blocks")]
    batch_blocks: Option<u32>,
    #[serde(rename = "replay_policy")]
    policy: Option<String>,
    memory_budget_bytes: Option<u64>,
    memory_degrade_ratio: Option<f64>,
    memory_pause_ratio: Option<f64>,
    memory_resume_ratio: Option<f64>,
    min_replay_batch_blocks: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestBulkCatchupSection {
    canonical_batch_max_blocks: Option<u32>,
    canonical_batch_max_artifact_bytes: Option<u64>,
    canonical_batch_max_estimated_write_bytes: Option<u64>,
    canonical_batch_min_blocks_before_estimated_write_close: Option<u32>,
    source_segment_max_blocks: Option<u32>,
    source_segment_target_response_bytes: Option<u64>,
    source_fetch_max_in_flight_requests: Option<u32>,
    source_fetch_max_in_flight_bytes: Option<u64>,
    block_prepare_concurrency: Option<u32>,
    block_prepare_max_in_flight_artifact_bytes: Option<u64>,
    commit_reassembly_max_queued_artifact_bytes: Option<u64>,
    flush_interval_epochs: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestTipFollowSection {
    poll_interval_ms: Option<u64>,
    lag_threshold_blocks: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestModifiersSection {
    target_height: Option<u32>,
    checkpoint_height: Option<u32>,
    allow_near_tip_finalize: Option<bool>,
    coverage: Option<IngestCoverage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackupSection {
    to_path: Option<PathBuf>,
}

const fn node_source_name(node_source: NodeSourceKind) -> &'static str {
    match node_source {
        NodeSourceKind::ZebraJsonRpc => "zebra-json-rpc",
    }
}

fn parse_derive_replay_policy(policy_text: &str) -> Result<DeriveReplayPolicy, ConfigError> {
    match policy_text {
        "canonical-first" => Ok(DeriveReplayPolicy::CanonicalFirst),
        "continuous" => Ok(DeriveReplayPolicy::Continuous),
        _ => Err(ConfigError::invalid(
            "ingest.derive.replay_policy must be one of: canonical-first, continuous",
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
    let node_source_text = config
        .ingest
        .source
        .clone()
        .unwrap_or_else(|| node_source_name(NodeSourceKind::ZebraJsonRpc).to_owned());
    let node_source = parse_node_source(&node_source_text)?;
    let raw_blob_policy = config
        .storage
        .raw_blob_policy
        .unwrap_or(DEFAULT_RAW_BLOB_POLICY);
    let ResolvedPrimaryStorage {
        path: storage_path,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
    } = resolve_primary_storage(config.storage.into_primary_storage())?;

    let reorg_window_blocks = require_field(
        config.ingest.reorg_window_blocks,
        "ingest.reorg_window_blocks",
    )?;
    let reorg_window_blocks = parse_reorg_window_blocks(reorg_window_blocks)?;

    let catchup_threshold_blocks = config
        .ingest
        .phases
        .catchup_threshold_blocks
        .unwrap_or(reorg_window_blocks);

    let canonical_batch_max_blocks_raw = require_field(
        config.ingest.bulk_catchup.canonical_batch_max_blocks,
        "ingest.bulk_catchup.canonical_batch_max_blocks",
    )?;
    let canonical_batch_max_blocks =
        parse_canonical_batch_max_blocks(canonical_batch_max_blocks_raw)?;

    let canonical_batch_max_artifact_bytes = nonzero_u64_config(
        config
            .ingest
            .bulk_catchup
            .canonical_batch_max_artifact_bytes,
        "ingest.bulk_catchup.canonical_batch_max_artifact_bytes",
    )?;
    let canonical_batch_max_estimated_write_bytes = nonzero_u64_config(
        config
            .ingest
            .bulk_catchup
            .canonical_batch_max_estimated_write_bytes,
        "ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes",
    )?;
    let canonical_batch_min_blocks_before_estimated_write_close = nonzero_u32_config(
        config
            .ingest
            .bulk_catchup
            .canonical_batch_min_blocks_before_estimated_write_close,
        "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close",
    )?;
    if canonical_batch_min_blocks_before_estimated_write_close.get()
        > canonical_batch_max_blocks.get()
    {
        return Err(ConfigError::invalid(
            "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close must be less than or equal to ingest.bulk_catchup.canonical_batch_max_blocks",
        )
        .into());
    }

    let source_segment_max_blocks_raw = require_field(
        config.ingest.bulk_catchup.source_segment_max_blocks,
        "ingest.bulk_catchup.source_segment_max_blocks",
    )?;
    let source_segment_max_blocks =
        NonZeroU32::new(source_segment_max_blocks_raw).ok_or_else(|| {
            ConfigError::invalid(
                "ingest.bulk_catchup.source_segment_max_blocks must be greater than zero",
            )
        })?;

    let block_prepare_concurrency_raw = require_field(
        config.ingest.bulk_catchup.block_prepare_concurrency,
        "ingest.bulk_catchup.block_prepare_concurrency",
    )?;
    let block_prepare_concurrency =
        NonZeroU32::new(block_prepare_concurrency_raw).ok_or_else(|| {
            ConfigError::invalid(
                "ingest.bulk_catchup.block_prepare_concurrency must be greater than zero",
            )
        })?;
    let block_prepare_max_in_flight_artifact_bytes = nonzero_u64_config(
        config
            .ingest
            .bulk_catchup
            .block_prepare_max_in_flight_artifact_bytes,
        "ingest.bulk_catchup.block_prepare_max_in_flight_artifact_bytes",
    )?;
    let commit_reassembly_max_queued_artifact_bytes = nonzero_u64_config(
        config
            .ingest
            .bulk_catchup
            .commit_reassembly_max_queued_artifact_bytes,
        "ingest.bulk_catchup.commit_reassembly_max_queued_artifact_bytes",
    )?;

    let source_segment_target_response_bytes = nonzero_u64_config(
        config
            .ingest
            .bulk_catchup
            .source_segment_target_response_bytes,
        "ingest.bulk_catchup.source_segment_target_response_bytes",
    )?;
    let source_fetch_max_in_flight_requests = nonzero_u32_config(
        config
            .ingest
            .bulk_catchup
            .source_fetch_max_in_flight_requests,
        "ingest.bulk_catchup.source_fetch_max_in_flight_requests",
    )?;
    let source_fetch_max_in_flight_bytes = nonzero_u64_config(
        config.ingest.bulk_catchup.source_fetch_max_in_flight_bytes,
        "ingest.bulk_catchup.source_fetch_max_in_flight_bytes",
    )?;

    let replay_batch_blocks_raw = require_field(
        config.ingest.derive.batch_blocks,
        "ingest.derive.replay_batch_blocks",
    )?;
    let replay_batch_blocks = NonZeroU32::new(replay_batch_blocks_raw).ok_or_else(|| {
        ConfigError::invalid("ingest.derive.replay_batch_blocks must be greater than zero")
    })?;
    let replay_policy_raw =
        require_field(config.ingest.derive.policy, "ingest.derive.replay_policy")?;
    let replay_policy = parse_derive_replay_policy(&replay_policy_raw)?;
    let memory_budget_bytes = optional_nonzero_u64_config(
        config.ingest.derive.memory_budget_bytes,
        "ingest.derive.memory_budget_bytes",
    )?;
    let memory_degrade_ratio = ratio_config(
        config.ingest.derive.memory_degrade_ratio,
        "ingest.derive.memory_degrade_ratio",
    )?;
    let memory_pause_ratio = ratio_config(
        config.ingest.derive.memory_pause_ratio,
        "ingest.derive.memory_pause_ratio",
    )?;
    let memory_resume_ratio = ratio_config(
        config.ingest.derive.memory_resume_ratio,
        "ingest.derive.memory_resume_ratio",
    )?;
    if !(memory_resume_ratio < memory_degrade_ratio && memory_degrade_ratio < memory_pause_ratio) {
        return Err(ConfigError::invalid(
            "ingest.derive memory ratios must satisfy memory_resume_ratio < memory_degrade_ratio < memory_pause_ratio",
        )
        .into());
    }
    let min_replay_batch_blocks = nonzero_u32_config(
        config.ingest.derive.min_replay_batch_blocks,
        "ingest.derive.min_replay_batch_blocks",
    )?;
    if min_replay_batch_blocks > replay_batch_blocks {
        return Err(ConfigError::invalid(
            "ingest.derive.min_replay_batch_blocks must be less than or equal to ingest.derive.replay_batch_blocks",
        )
        .into());
    }

    let flush_interval_epochs_raw = require_field(
        config.ingest.bulk_catchup.flush_interval_epochs,
        "ingest.bulk_catchup.flush_interval_epochs",
    )?;
    let flush_interval_epochs = NonZeroU32::new(flush_interval_epochs_raw).ok_or_else(|| {
        ConfigError::invalid("ingest.bulk_catchup.flush_interval_epochs must be greater than zero")
    })?;

    let poll_interval_ms = require_field(
        config.ingest.tip_follow.poll_interval_ms,
        "ingest.tip_follow.poll_interval_ms",
    )?;
    let poll_interval = parse_poll_interval_ms(poll_interval_ms)?;

    let lag_threshold_blocks = require_field(
        config.ingest.tip_follow.lag_threshold_blocks,
        "ingest.tip_follow.lag_threshold_blocks",
    )?;

    let coverage = config.ingest.modifiers.coverage.unwrap_or_default();

    let allow_near_tip_finalize = require_field(
        config.ingest.modifiers.allow_near_tip_finalize,
        "ingest.modifiers.allow_near_tip_finalize",
    )?;
    if matches!(coverage, IngestCoverage::WalletServing) && allow_near_tip_finalize {
        return Err(ConfigError::invalid(
            "ingest.modifiers.coverage = \"wallet-serving\" cannot be combined with ingest.modifiers.allow_near_tip_finalize = true; serving stores must stop outside the reorg window",
        )
        .into());
    }
    if matches!(coverage, IngestCoverage::WalletServing)
        && config.ingest.modifiers.checkpoint_height.is_some()
    {
        return Err(ConfigError::invalid(
            "ingest.modifiers.coverage = \"wallet-serving\" derives checkpoint_height from the node; remove ingest.modifiers.checkpoint_height",
        )
        .into());
    }

    let ResolvedIngestControlWriter {
        listen_addr: ingest_control_listen_addr,
        bearer_token_path: ingest_control_bearer_token_path,
        bearer_token: ingest_control_bearer_token,
    } = resolve_ingest_control_writer(config.ingest_control)?;
    let retention = resolve_retention(config.retention)?;
    let ops_listen_addr = resolve_ops_listen_addr(config.ops)?;
    let node_target = NodeTarget::resolve(network, config.node).map_err(ConfigError::from)?;
    if source_segment_target_response_bytes > node_target.max_response_bytes {
        return Err(ConfigError::invalid(
            "ingest.bulk_catchup.source_segment_target_response_bytes must not exceed node.max_response_bytes",
        )
        .into());
    }
    if source_fetch_max_in_flight_bytes.get() < node_target.max_response_bytes.get() {
        return Err(ConfigError::invalid(
            "ingest.bulk_catchup.source_fetch_max_in_flight_bytes must be greater than or equal to node.max_response_bytes",
        )
        .into());
    }

    let modifiers = IngestModifiers {
        target_height: config.ingest.modifiers.target_height.map(BlockHeight::new),
        checkpoint_height: config
            .ingest
            .modifiers
            .checkpoint_height
            .map(BlockHeight::new),
        allow_near_tip_finalize,
        checkpoint: None,
    };

    let loop_config = IngestLoopConfig {
        node: node_target,
        node_source,
        storage_path,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
        raw_blob_policy,
        reorg_window_blocks,
        phases: PhasesConfig {
            catchup_threshold_blocks,
        },
        derive: IngestDeriveConfig {
            replay_batch_blocks,
            replay_policy,
            memory_budget_bytes,
            memory_degrade_ratio,
            memory_pause_ratio,
            memory_resume_ratio,
            min_replay_batch_blocks,
        },
        bulk_catchup: BulkCatchupConfig {
            canonical_batch_max_blocks,
            canonical_batch_max_artifact_bytes,
            canonical_batch_max_estimated_write_bytes,
            canonical_batch_min_blocks_before_estimated_write_close,
            source_segment_max_blocks,
            source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes,
            block_prepare_concurrency,
            block_prepare_max_in_flight_artifact_bytes,
            commit_reassembly_max_queued_artifact_bytes,
            flush_interval_epochs,
        },
        tip_follow: TipFollowPhaseConfig {
            poll_interval,
            lag_threshold_blocks,
        },
        modifiers,
    };

    Ok(IngestCommandConfig {
        loop_config,
        coverage,
        ingest_control_listen_addr,
        ingest_control_bearer_token_path,
        ingest_control_bearer_token,
        ops_listen_addr,
        retention,
    })
}

fn resolve_backup_config(config: IngestConfig) -> Result<BackupCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let ResolvedPrimaryStorage {
        path: storage_path,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
    } = resolve_primary_storage(config.storage.into_primary_storage())?;
    let to_path = config
        .backup
        .to_path
        .ok_or_else(|| ConfigError::missing_field("backup.to_path"))?;

    Ok(BackupCommandConfig {
        network,
        storage_path,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
        to_path,
    })
}

#[derive(Serialize)]
struct RedactedIngestConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    node: NodeToml,
    storage: IngestPrimaryStorageToml,
    ingest: IngestToml,
    ingest_control: IngestControlWriterToml,
    retention: RetentionToml,
}

#[derive(Serialize)]
struct RedactedBackupConfigToml {
    network: NetworkToml,
    storage: PrimaryStorageToml,
    backup: BackupToml,
}

impl RedactedIngestConfigToml {
    #[allow(
        clippy::too_many_lines,
        reason = "redacted print-config mirrors the resolved ingest TOML shape field by field"
    )]
    fn from_ingest_config(config: &IngestCommandConfig) -> Self {
        let loop_config = &config.loop_config;
        Self {
            network: NetworkToml::from_network(loop_config.node.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            node: NodeToml::from_node_target(&loop_config.node),
            storage: IngestPrimaryStorageToml {
                path: loop_config.storage_path.display().to_string(),
                canonical: StorageRoleToml::from_resolved(loop_config.canonical_rocksdb_budget),
                derive: StorageRoleToml::from_resolved(loop_config.derive_rocksdb_budget),
                raw_blob_policy: loop_config.raw_blob_policy,
            },
            ingest: IngestToml {
                source: node_source_name(loop_config.node_source),
                reorg_window_blocks: loop_config.reorg_window_blocks,
                phases: IngestPhasesToml {
                    catchup_threshold_blocks: loop_config.phases.catchup_threshold_blocks,
                },
                derive: IngestDeriveToml {
                    batch_blocks: loop_config.derive.replay_batch_blocks.get(),
                    policy: loop_config.derive.replay_policy.as_kebab_case(),
                    memory_budget_bytes: loop_config
                        .derive
                        .memory_budget_bytes
                        .map(NonZeroU64::get),
                    memory_degrade_ratio: loop_config.derive.memory_degrade_ratio,
                    memory_pause_ratio: loop_config.derive.memory_pause_ratio,
                    memory_resume_ratio: loop_config.derive.memory_resume_ratio,
                    min_replay_batch_blocks: loop_config.derive.min_replay_batch_blocks.get(),
                },
                bulk_catchup: IngestBulkCatchupToml {
                    canonical_batch_max_blocks: loop_config
                        .bulk_catchup
                        .canonical_batch_max_blocks
                        .get(),
                    canonical_batch_max_artifact_bytes: loop_config
                        .bulk_catchup
                        .canonical_batch_max_artifact_bytes
                        .get(),
                    canonical_batch_max_estimated_write_bytes: loop_config
                        .bulk_catchup
                        .canonical_batch_max_estimated_write_bytes
                        .get(),
                    canonical_batch_min_blocks_before_estimated_write_close: loop_config
                        .bulk_catchup
                        .canonical_batch_min_blocks_before_estimated_write_close
                        .get(),
                    source_segment_max_blocks: loop_config
                        .bulk_catchup
                        .source_segment_max_blocks
                        .get(),
                    source_segment_target_response_bytes: loop_config
                        .bulk_catchup
                        .source_segment_target_response_bytes
                        .get(),
                    source_fetch_max_in_flight_requests: loop_config
                        .bulk_catchup
                        .source_fetch_max_in_flight_requests
                        .get(),
                    source_fetch_max_in_flight_bytes: loop_config
                        .bulk_catchup
                        .source_fetch_max_in_flight_bytes
                        .get(),
                    block_prepare_concurrency: loop_config
                        .bulk_catchup
                        .block_prepare_concurrency
                        .get(),
                    block_prepare_max_in_flight_artifact_bytes: loop_config
                        .bulk_catchup
                        .block_prepare_max_in_flight_artifact_bytes
                        .get(),
                    commit_reassembly_max_queued_artifact_bytes: loop_config
                        .bulk_catchup
                        .commit_reassembly_max_queued_artifact_bytes
                        .get(),
                    flush_interval_epochs: loop_config.bulk_catchup.flush_interval_epochs.get(),
                },
                tip_follow: IngestTipFollowToml {
                    poll_interval_ms: duration_as_millis_u64(loop_config.tip_follow.poll_interval),
                    lag_threshold_blocks: loop_config.tip_follow.lag_threshold_blocks,
                },
                modifiers: IngestModifiersToml {
                    target_height: loop_config.modifiers.target_height.map(BlockHeight::value),
                    checkpoint_height: loop_config
                        .modifiers
                        .checkpoint_height
                        .map(BlockHeight::value),
                    allow_near_tip_finalize: loop_config.modifiers.allow_near_tip_finalize,
                    coverage: config.coverage,
                },
            },
            ingest_control: IngestControlWriterToml::from_resolved(
                config.ingest_control_listen_addr,
                config.ingest_control_bearer_token_path.as_deref(),
            ),
            retention: RetentionToml::from_resolved(config.retention),
        }
    }
}

impl RedactedBackupConfigToml {
    fn from_backup_config(config: &BackupCommandConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            storage: PrimaryStorageToml {
                path: config.storage_path.display().to_string(),
                canonical: StorageRoleToml::from_resolved(config.canonical_rocksdb_budget),
                derive: StorageRoleToml::from_resolved(config.derive_rocksdb_budget),
            },
            backup: BackupToml {
                to_path: config.to_path.display().to_string(),
            },
        }
    }
}

#[derive(Serialize)]
struct IngestToml {
    source: &'static str,
    reorg_window_blocks: u32,
    phases: IngestPhasesToml,
    derive: IngestDeriveToml,
    bulk_catchup: IngestBulkCatchupToml,
    tip_follow: IngestTipFollowToml,
    modifiers: IngestModifiersToml,
}

#[derive(Serialize)]
struct IngestPrimaryStorageToml {
    path: String,
    canonical: StorageRoleToml,
    derive: StorageRoleToml,
    raw_blob_policy: RawBlobPolicy,
}

#[derive(Serialize)]
struct IngestPhasesToml {
    catchup_threshold_blocks: u32,
}

#[derive(Serialize)]
struct IngestDeriveToml {
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
}

#[derive(Serialize)]
struct IngestBulkCatchupToml {
    canonical_batch_max_blocks: u32,
    canonical_batch_max_artifact_bytes: u64,
    canonical_batch_max_estimated_write_bytes: u64,
    canonical_batch_min_blocks_before_estimated_write_close: u32,
    source_segment_max_blocks: u32,
    source_segment_target_response_bytes: u64,
    source_fetch_max_in_flight_requests: u32,
    source_fetch_max_in_flight_bytes: u64,
    block_prepare_concurrency: u32,
    block_prepare_max_in_flight_artifact_bytes: u64,
    commit_reassembly_max_queued_artifact_bytes: u64,
    flush_interval_epochs: u32,
}

/// Computes the default block-prepare concurrency from available logical cores.
fn default_block_prepare_concurrency() -> u32 {
    let logical_cores =
        u32::try_from(std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get))
            .unwrap_or(8);
    logical_cores.clamp(1, BLOCK_PREPARE_CONCURRENCY_CEILING)
}

#[derive(Serialize)]
struct IngestTipFollowToml {
    poll_interval_ms: u64,
    lag_threshold_blocks: u64,
}

#[derive(Serialize)]
struct IngestModifiersToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    target_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint_height: Option<u32>,
    allow_near_tip_finalize: bool,
    coverage: IngestCoverage,
}

#[derive(Serialize)]
struct BackupToml {
    to_path: String,
}

/// Re-export used by the `Duration` field above; the helper is the
/// runtime crate's stable rendering for milliseconds.
#[allow(
    dead_code,
    reason = "kept for forward-compat with code that touched it during refactor"
)]
const _: fn(Duration) -> u64 = duration_as_millis_u64;

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn pipeline_queue_falls_back_when_cgroup_absent() {
        let fallback = FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES;
        assert_eq!(
            default_pipeline_queue_bytes_from_budget(None, fallback),
            fallback,
            "dev hosts without cgroup keep the pre-existing 512 MiB defaults"
        );
    }

    #[test]
    fn pipeline_queue_caps_at_fallback_on_fat_containers() {
        let fallback = FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES;
        // 64 GiB container -> raw computation would give 1 GiB per queue,
        // but the fallback caps at the previously hand-tuned 512 MiB.
        let result = default_pipeline_queue_bytes_from_budget(Some(64 * ONE_GIB), fallback);
        assert_eq!(result, fallback);
    }

    #[test]
    fn pipeline_queue_shrinks_for_railway_sized_containers() {
        let fallback = FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES;
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
        let fallback = FALLBACK_BLOCK_PREPARE_MAX_IN_FLIGHT_ARTIFACT_BYTES;
        // 4 GiB container -> raw computation gives 64 MiB per queue,
        // smaller than a single mainnet block can produce. Floor at
        // 128 MiB so the pipeline can always make forward progress.
        let result = default_pipeline_queue_bytes_from_budget(Some(4 * ONE_GIB), fallback);
        assert_eq!(result, MIN_PIPELINE_QUEUE_BYTES);
    }
}
