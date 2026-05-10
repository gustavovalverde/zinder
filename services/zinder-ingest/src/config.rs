//! Configuration loading for the `zinder-ingest` command.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::BlockHeight;
use zinder_ingest::{
    BackfillConfig, ChainEventRetentionConfig, DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
    IngestError, MempoolEventRetentionWorkerConfig, NodeSourceKind, TipFollowConfig,
};
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, NetworkSection, NetworkToml,
    NodeToml, duration_as_millis_u64, require_field,
};
use zinder_store::MempoolEventRetentionConfig;

use crate::cli::parse::{
    parse_commit_batch_blocks, parse_node_source, parse_poll_interval_ms, parse_reorg_window_blocks,
};
use zinder_source::{NodeAuthSection, NodeSection, NodeTarget, SourceChainCheckpoint};

const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const DEFAULT_COMMIT_BATCH_BLOCKS: u32 = 1000;
const DEFAULT_ALLOW_NEAR_TIP_FINALIZE: bool = false;
const DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_INGEST_CONTROL_LISTEN_ADDR: &str = "127.0.0.1:9100";
const DEFAULT_CHAIN_EVENT_RETENTION_HOURS: u64 = 168;
const DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS: u64 = 60_000;
const DEFAULT_CURSOR_AT_RISK_WARNING_HOURS: u64 = 24;
const DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES: u64 = 60;
const DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS: u64 = 24;
const DEFAULT_MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS: u64 = 30_000;
// Default warning fires at 80% of the shorter retention window.
// 20% of `DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES` (60) is 12 minutes.
const DEFAULT_MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES: u64 = 12;

/// Fully loaded command configuration for `zinder-ingest backfill`.
#[derive(Debug)]
pub(crate) struct BackfillCommandConfig {
    pub(crate) node: NodeTarget,
    pub(crate) node_source: NodeSourceKind,
    pub(crate) storage_path: PathBuf,
    pub(crate) from_height: Option<BlockHeight>,
    pub(crate) to_height: BlockHeight,
    pub(crate) commit_batch_blocks: std::num::NonZeroU32,
    pub(crate) allow_near_tip_finalize: bool,
    pub(crate) checkpoint_height: Option<BlockHeight>,
    pub(crate) coverage: BackfillCoverage,
}

impl BackfillCommandConfig {
    pub(crate) fn resolved_backfill_config(
        &self,
        checkpoint: Option<SourceChainCheckpoint>,
    ) -> Result<BackfillConfig, IngestConfigError> {
        let from_height = self
            .from_height
            .ok_or_else(|| ConfigError::missing_field("backfill.from_height"))?;

        Ok(BackfillConfig {
            node: self.node.clone(),
            node_source: self.node_source,
            storage_path: self.storage_path.clone(),
            from_height,
            to_height: self.to_height,
            commit_batch_blocks: self.commit_batch_blocks,
            allow_near_tip_finalize: self.allow_near_tip_finalize,
            checkpoint,
        })
    }
}

/// Backfill coverage policy selected by an operator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BackfillCoverage {
    /// Use explicitly supplied heights.
    Explicit,
    /// Derive the historical floor needed by lightwalletd-compatible wallets.
    WalletServing,
}

impl BackfillCoverage {
    const fn as_kebab_case(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::WalletServing => "wallet-serving",
        }
    }
}

/// Fully loaded command configuration for `zinder-ingest tip-follow`.
#[derive(Debug)]
pub(crate) struct TipFollowCommandConfig {
    pub(crate) tip_follow: TipFollowConfig,
    pub(crate) ingest_control_listen_addr: SocketAddr,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) chain_event_retention: ChainEventRetentionConfig,
    pub(crate) mempool_event_retention: MempoolEventRetentionWorkerConfig,
}

/// Fully loaded command configuration for `zinder-ingest backup`.
#[derive(Debug)]
pub(crate) struct BackupCommandConfig {
    pub(crate) network: zinder_core::Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) to_path: PathBuf,
}

/// Command-line overrides for the backfill command.
#[derive(Debug, Default)]
pub(crate) struct BackfillConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) node_source: Option<String>,
    pub(crate) json_rpc_addr: Option<String>,
    pub(crate) node_auth_method: Option<String>,
    pub(crate) node_auth_username: Option<String>,
    pub(crate) node_auth_path: Option<PathBuf>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) from_height: Option<u32>,
    pub(crate) to_height: Option<u32>,
    pub(crate) request_timeout_secs: Option<u64>,
    pub(crate) max_response_bytes: Option<u64>,
    pub(crate) commit_batch_blocks: Option<u32>,
    pub(crate) allow_near_tip_finalize: Option<bool>,
    pub(crate) checkpoint_height: Option<u32>,
    pub(crate) wallet_serving: Option<bool>,
}

/// Command-line overrides for the tip-follow command.
#[derive(Debug, Default)]
pub(crate) struct TipFollowConfigOverrides {
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
    pub(crate) commit_batch_blocks: Option<u32>,
    pub(crate) poll_interval_ms: Option<u64>,
    pub(crate) lag_threshold_blocks: Option<u64>,
    pub(crate) ingest_control_listen_addr: Option<SocketAddr>,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
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
    /// Shared configuration error (load, render, missing field, invalid value,
    /// sensitive-env override, non-UTF-8 path).
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Ingestion runtime validation failed.
    #[error(transparent)]
    Ingest(#[from] IngestError),

    /// Ingest-control endpoint failed to bind.
    #[error("failed to bind ingest-control endpoint at {listen_addr}: {source}")]
    IngestControlBind {
        /// Address that failed to bind.
        listen_addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Ingest-control endpoint transport failed.
    #[error("ingest-control endpoint failed: {source}")]
    IngestControlTransport {
        /// Underlying tonic transport error.
        #[source]
        source: tonic::transport::Error,
    },

    /// Loading the ingest-control bearer token failed.
    #[error("invalid ingest-control bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
}

/// Loads and validates backfill configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_backfill_config(
    config_path: Option<PathBuf>,
    overrides: BackfillConfigOverrides,
) -> Result<BackfillCommandConfig, IngestConfigError> {
    let raw_config: IngestConfig = ConfigLoader::new()
        .with_default("ingest.commit_batch_blocks", DEFAULT_COMMIT_BATCH_BLOCKS)?
        .with_default(
            "backfill.allow_near_tip_finalize",
            DEFAULT_ALLOW_NEAR_TIP_FINALIZE,
        )?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_if("node.source", overrides.node_source)?
        .with_override_if("node.json_rpc_addr", overrides.json_rpc_addr)?
        .with_override_if("node.auth.method", overrides.node_auth_method)?
        .with_override_if("node.auth.username", overrides.node_auth_username)?
        .with_override_path_if("node.auth.path", overrides.node_auth_path)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_if("node.request_timeout_secs", overrides.request_timeout_secs)?
        .with_override_if("node.max_response_bytes", overrides.max_response_bytes)?
        .with_override_if("ingest.commit_batch_blocks", overrides.commit_batch_blocks)?
        .with_override_if("backfill.from_height", overrides.from_height)?
        .with_override_if("backfill.to_height", overrides.to_height)?
        .with_override_if(
            "backfill.allow_near_tip_finalize",
            overrides.allow_near_tip_finalize,
        )?
        .with_override_if("backfill.checkpoint_height", overrides.checkpoint_height)?
        .with_override_if(
            "backfill.coverage",
            (overrides.wallet_serving == Some(true))
                .then_some(BackfillCoverage::WalletServing.as_kebab_case()),
        )?
        .load()?;

    resolve_backfill_config(raw_config)
}

/// Loads and validates tip-follow configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_tip_follow_config(
    config_path: Option<PathBuf>,
    overrides: TipFollowConfigOverrides,
) -> Result<TipFollowCommandConfig, IngestConfigError> {
    let raw_config: IngestConfig = ConfigLoader::new()
        .with_default("ingest.reorg_window_blocks", DEFAULT_REORG_WINDOW_BLOCKS)?
        .with_default("ingest.commit_batch_blocks", DEFAULT_COMMIT_BATCH_BLOCKS)?
        .with_default(
            "tip_follow.poll_interval_ms",
            DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS,
        )?
        .with_default(
            "tip_follow.lag_threshold_blocks",
            DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        )?
        .with_default(
            "ingest.retention.chain_event_retention_hours",
            DEFAULT_CHAIN_EVENT_RETENTION_HOURS,
        )?
        .with_default(
            "ingest.retention.chain_event_retention_check_interval_ms",
            DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS,
        )?
        .with_default(
            "ingest.retention.cursor_at_risk_warning_hours",
            DEFAULT_CURSOR_AT_RISK_WARNING_HOURS,
        )?
        .with_default(
            "ingest.retention.mempool_mined_retention_minutes",
            DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES,
        )?
        .with_default(
            "ingest.retention.mempool_invalidated_retention_hours",
            DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS,
        )?
        .with_default(
            "ingest.retention.mempool_event_retention_check_interval_ms",
            DEFAULT_MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS,
        )?
        .with_default(
            "ingest.retention.mempool_cursor_at_risk_warning_minutes",
            DEFAULT_MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES,
        )?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_if("node.source", overrides.node_source)?
        .with_override_if("node.json_rpc_addr", overrides.json_rpc_addr)?
        .with_override_if("node.auth.method", overrides.node_auth_method)?
        .with_override_if("node.auth.username", overrides.node_auth_username)?
        .with_override_path_if("node.auth.path", overrides.node_auth_path)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_if("node.request_timeout_secs", overrides.request_timeout_secs)?
        .with_override_if("node.max_response_bytes", overrides.max_response_bytes)?
        .with_override_if("ingest.reorg_window_blocks", overrides.reorg_window_blocks)?
        .with_override_if("ingest.commit_batch_blocks", overrides.commit_batch_blocks)?
        .with_override_if("tip_follow.poll_interval_ms", overrides.poll_interval_ms)?
        .with_override_if(
            "tip_follow.lag_threshold_blocks",
            overrides.lag_threshold_blocks,
        )?
        .with_override_if(
            "ingest.control.listen_addr",
            overrides
                .ingest_control_listen_addr
                .map(|addr| addr.to_string()),
        )?
        .with_override_path_if(
            "ingest.control.token_path",
            overrides.ingest_control_token_path,
        )?
        .load()?;

    resolve_tip_follow_config(raw_config)
}

/// Loads and validates backup configuration from defaults, file, environment, and CLI overrides.
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

/// Renders the effective backfill configuration in the accepted TOML shape.
pub(crate) fn redacted_backfill_config_toml(
    config: &BackfillCommandConfig,
) -> Result<String, IngestConfigError> {
    let rendered = toml::to_string(&RedactedBackfillConfigToml::from_backfill_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
}

/// Renders the effective tip-follow configuration in the accepted TOML shape.
pub(crate) fn redacted_tip_follow_config_toml(
    config: &TipFollowCommandConfig,
) -> Result<String, IngestConfigError> {
    let rendered = toml::to_string(&RedactedTipFollowConfigToml::from_tip_follow_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
}

/// Renders the effective backup configuration in the accepted TOML shape.
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
    node: IngestNodeConfig,
    storage: StorageConfig,
    ingest: IngestSectionConfig,
    backfill: BackfillSectionConfig,
    tip_follow: TipFollowSectionConfig,
    backup: BackupSectionConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestNodeConfig {
    source: Option<String>,
    json_rpc_addr: Option<String>,
    indexer_grpc_addr: Option<String>,
    request_timeout_secs: Option<u64>,
    max_response_bytes: Option<u64>,
    auth: NodeAuthSection,
}

impl IngestNodeConfig {
    fn into_node_section(self) -> NodeSection {
        NodeSection {
            json_rpc_addr: self.json_rpc_addr,
            indexer_grpc_addr: self.indexer_grpc_addr,
            request_timeout_secs: self.request_timeout_secs,
            max_response_bytes: self.max_response_bytes,
            auth: self.auth,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageConfig {
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestSectionConfig {
    reorg_window_blocks: Option<u32>,
    commit_batch_blocks: Option<u32>,
    control: IngestControlSectionConfig,
    retention: IngestRetentionSectionConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestControlSectionConfig {
    listen_addr: Option<String>,
    token_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestRetentionSectionConfig {
    chain_event_retention_hours: Option<u64>,
    chain_event_retention_check_interval_ms: Option<u64>,
    cursor_at_risk_warning_hours: Option<u64>,
    mempool_mined_retention_minutes: Option<u64>,
    mempool_invalidated_retention_hours: Option<u64>,
    mempool_event_retention_check_interval_ms: Option<u64>,
    mempool_cursor_at_risk_warning_minutes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackfillSectionConfig {
    from_height: Option<u32>,
    to_height: Option<u32>,
    allow_near_tip_finalize: Option<bool>,
    checkpoint_height: Option<u32>,
    coverage: Option<BackfillCoverage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TipFollowSectionConfig {
    poll_interval_ms: Option<u64>,
    lag_threshold_blocks: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackupSectionConfig {
    to_path: Option<PathBuf>,
}

const fn node_source_name(node_source: NodeSourceKind) -> &'static str {
    match node_source {
        NodeSourceKind::ZebraJsonRpc => "zebra-json-rpc",
    }
}

fn resolve_backfill_config(
    config: IngestConfig,
) -> Result<BackfillCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let node_source_text = require_field(config.node.source.clone(), "node.source")?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let coverage = config
        .backfill
        .coverage
        .unwrap_or(BackfillCoverage::Explicit);
    let from_height = resolve_backfill_from_height(config.backfill.from_height, coverage)?;
    let to_height =
        require_field(config.backfill.to_height, "backfill.to_height").map(BlockHeight::new)?;
    if matches!(coverage, BackfillCoverage::WalletServing)
        && config.backfill.checkpoint_height.is_some()
    {
        return Err(ConfigError::invalid(
            "backfill.coverage = \"wallet-serving\" derives checkpoint_height from the node; remove backfill.checkpoint_height",
        )
        .into());
    }
    let commit_batch_blocks = require_field(
        config.ingest.commit_batch_blocks,
        "ingest.commit_batch_blocks",
    )?;
    let allow_near_tip_finalize = require_field(
        config.backfill.allow_near_tip_finalize,
        "backfill.allow_near_tip_finalize",
    )?;
    if matches!(coverage, BackfillCoverage::WalletServing) && allow_near_tip_finalize {
        return Err(ConfigError::invalid(
            "backfill.coverage = \"wallet-serving\" cannot be combined with backfill.allow_near_tip_finalize = true; serving stores must stop outside the reorg window",
        )
        .into());
    }
    let node_source = parse_node_source(&node_source_text)?;
    let node_target =
        NodeTarget::resolve(network, config.node.into_node_section()).map_err(ConfigError::from)?;

    Ok(BackfillCommandConfig {
        node: node_target,
        node_source,
        storage_path,
        from_height,
        to_height,
        commit_batch_blocks: parse_commit_batch_blocks(commit_batch_blocks)?,
        allow_near_tip_finalize,
        checkpoint_height: config.backfill.checkpoint_height.map(BlockHeight::new),
        coverage,
    })
}

fn resolve_backfill_from_height(
    from_height: Option<u32>,
    coverage: BackfillCoverage,
) -> Result<Option<BlockHeight>, IngestConfigError> {
    match (from_height, coverage) {
        (Some(from_height), BackfillCoverage::Explicit) => Ok(Some(BlockHeight::new(from_height))),
        (None, BackfillCoverage::Explicit) => {
            Err(ConfigError::missing_field("backfill.from_height").into())
        }
        (Some(_), BackfillCoverage::WalletServing) => Err(ConfigError::invalid(
            "backfill.coverage = \"wallet-serving\" derives from_height from the node; remove backfill.from_height",
        )
        .into()),
        (None, BackfillCoverage::WalletServing) => Ok(None),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Tip-follow config validation intentionally stays in one resolver so field dependencies and error ordering are auditable."
)]
fn resolve_tip_follow_config(
    config: IngestConfig,
) -> Result<TipFollowCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let node_source_text = require_field(config.node.source.clone(), "node.source")?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let reorg_window_blocks = require_field(
        config.ingest.reorg_window_blocks,
        "ingest.reorg_window_blocks",
    )?;
    let commit_batch_blocks = require_field(
        config.ingest.commit_batch_blocks,
        "ingest.commit_batch_blocks",
    )?;
    let poll_interval_ms = require_field(
        config.tip_follow.poll_interval_ms,
        "tip_follow.poll_interval_ms",
    )?;
    let lag_threshold_blocks = require_field(
        config.tip_follow.lag_threshold_blocks,
        "tip_follow.lag_threshold_blocks",
    )?;
    let ingest_control_listen_addr_string = config
        .ingest
        .control
        .listen_addr
        .unwrap_or_else(|| DEFAULT_INGEST_CONTROL_LISTEN_ADDR.to_owned());
    let ingest_control_listen_addr =
        ingest_control_listen_addr_string
            .parse::<SocketAddr>()
            .map_err(|source| {
                ConfigError::invalid(format!(
                    "ingest.control.listen_addr {ingest_control_listen_addr_string} is not a socket address: {source}"
                ))
            })?;
    let ingest_control_token_path = config.ingest.control.token_path.clone();
    let ingest_control_bearer_token = ingest_control_token_path
        .as_deref()
        .map(BearerToken::from_file)
        .transpose()?;
    let chain_event_retention_hours = require_field(
        config.ingest.retention.chain_event_retention_hours,
        "ingest.retention.chain_event_retention_hours",
    )?;
    let chain_event_retention_check_interval_ms = require_field(
        config
            .ingest
            .retention
            .chain_event_retention_check_interval_ms,
        "ingest.retention.chain_event_retention_check_interval_ms",
    )?;
    let cursor_at_risk_warning_hours = require_field(
        config.ingest.retention.cursor_at_risk_warning_hours,
        "ingest.retention.cursor_at_risk_warning_hours",
    )?;
    let mempool_mined_retention_minutes = require_field(
        config.ingest.retention.mempool_mined_retention_minutes,
        "ingest.retention.mempool_mined_retention_minutes",
    )?;
    let mempool_invalidated_retention_hours = require_field(
        config.ingest.retention.mempool_invalidated_retention_hours,
        "ingest.retention.mempool_invalidated_retention_hours",
    )?;
    let mempool_event_retention_check_interval_ms = require_field(
        config
            .ingest
            .retention
            .mempool_event_retention_check_interval_ms,
        "ingest.retention.mempool_event_retention_check_interval_ms",
    )?;
    let mempool_cursor_at_risk_warning_minutes = require_field(
        config
            .ingest
            .retention
            .mempool_cursor_at_risk_warning_minutes,
        "ingest.retention.mempool_cursor_at_risk_warning_minutes",
    )?;
    if chain_event_retention_check_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "ingest.retention.chain_event_retention_check_interval_ms must be greater than zero",
        )
        .into());
    }
    if mempool_event_retention_check_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "ingest.retention.mempool_event_retention_check_interval_ms must be greater than zero",
        )
        .into());
    }
    if chain_event_retention_hours > 0 && cursor_at_risk_warning_hours > chain_event_retention_hours
    {
        return Err(ConfigError::invalid(
            "ingest.retention.cursor_at_risk_warning_hours must be less than or equal to ingest.retention.chain_event_retention_hours",
        )
        .into());
    }
    let shortest_mempool_window_minutes = shortest_mempool_window_minutes_from(
        mempool_mined_retention_minutes,
        mempool_invalidated_retention_hours,
    );
    if let Some(shortest) = shortest_mempool_window_minutes
        && mempool_cursor_at_risk_warning_minutes > shortest
    {
        return Err(ConfigError::invalid(
            "ingest.retention.mempool_cursor_at_risk_warning_minutes must be less than or equal to the shortest configured mempool retention window",
        )
        .into());
    }
    let node_source = parse_node_source(&node_source_text)?;
    let node_target =
        NodeTarget::resolve(network, config.node.into_node_section()).map_err(ConfigError::from)?;

    Ok(TipFollowCommandConfig {
        tip_follow: TipFollowConfig {
            node: node_target,
            node_source,
            storage_path,
            reorg_window_blocks: parse_reorg_window_blocks(reorg_window_blocks)?,
            commit_batch_blocks: parse_commit_batch_blocks(commit_batch_blocks)?,
            poll_interval: parse_poll_interval_ms(poll_interval_ms)?,
            lag_threshold_blocks,
        },
        ingest_control_listen_addr,
        ingest_control_token_path,
        ingest_control_bearer_token,
        chain_event_retention: ChainEventRetentionConfig {
            retention_window: (chain_event_retention_hours > 0)
                .then(|| Duration::from_secs(chain_event_retention_hours.saturating_mul(3_600))),
            check_interval: Duration::from_millis(chain_event_retention_check_interval_ms),
            cursor_at_risk_warning: Duration::from_secs(
                cursor_at_risk_warning_hours.saturating_mul(3_600),
            ),
        },
        mempool_event_retention: MempoolEventRetentionWorkerConfig {
            retention: MempoolEventRetentionConfig::new(
                (mempool_mined_retention_minutes > 0).then(|| {
                    Duration::from_secs(mempool_mined_retention_minutes.saturating_mul(60))
                }),
                (mempool_invalidated_retention_hours > 0).then(|| {
                    Duration::from_secs(mempool_invalidated_retention_hours.saturating_mul(3_600))
                }),
            ),
            check_interval: Duration::from_millis(mempool_event_retention_check_interval_ms),
            cursor_at_risk_warning: Duration::from_secs(
                mempool_cursor_at_risk_warning_minutes.saturating_mul(60),
            ),
        },
    })
}

fn shortest_mempool_window_minutes_from(mined_minutes: u64, invalidated_hours: u64) -> Option<u64> {
    let mined = (mined_minutes > 0).then_some(mined_minutes);
    let invalidated = (invalidated_hours > 0).then_some(invalidated_hours.saturating_mul(60));
    match (mined, invalidated) {
        (Some(mined), Some(invalidated)) => Some(mined.min(invalidated)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn resolve_backup_config(config: IngestConfig) -> Result<BackupCommandConfig, IngestConfigError> {
    let network = config.network.resolve()?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let to_path = config
        .backup
        .to_path
        .ok_or_else(|| ConfigError::missing_field("backup.to_path"))?;

    Ok(BackupCommandConfig {
        network,
        storage_path,
        to_path,
    })
}

#[derive(Serialize)]
struct RedactedBackfillConfigToml {
    network: NetworkToml,
    node: IngestNodeToml,
    storage: StorageToml,
    ingest: IngestToml,
    backfill: BackfillToml,
}

#[derive(Serialize)]
struct RedactedTipFollowConfigToml {
    network: NetworkToml,
    node: IngestNodeToml,
    storage: StorageToml,
    ingest: IngestToml,
    tip_follow: TipFollowToml,
}

#[derive(Serialize)]
struct RedactedBackupConfigToml {
    network: NetworkToml,
    storage: StorageToml,
    backup: BackupToml,
}

#[derive(Serialize)]
struct IngestNodeToml {
    source: &'static str,
    #[serde(flatten)]
    base: NodeToml,
}

impl IngestNodeToml {
    fn from_target(node_source: NodeSourceKind, target: &NodeTarget) -> Self {
        Self {
            source: node_source_name(node_source),
            base: NodeToml::from_node_target(target),
        }
    }
}

impl RedactedBackfillConfigToml {
    fn from_backfill_config(config: &BackfillCommandConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.node.network),
            node: IngestNodeToml::from_target(config.node_source, &config.node),
            storage: StorageToml {
                path: config.storage_path.display().to_string(),
            },
            ingest: IngestToml {
                reorg_window_blocks: None,
                commit_batch_blocks: config.commit_batch_blocks.get(),
                control: None,
                retention: None,
            },
            backfill: BackfillToml {
                from_height: if matches!(config.coverage, BackfillCoverage::Explicit) {
                    config.from_height.map(BlockHeight::value)
                } else {
                    None
                },
                to_height: config.to_height.value(),
                allow_near_tip_finalize: config.allow_near_tip_finalize,
                checkpoint_height: config.checkpoint_height.map(BlockHeight::value),
                coverage: config.coverage,
            },
        }
    }
}

impl RedactedTipFollowConfigToml {
    fn from_tip_follow_config(config: &TipFollowCommandConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.tip_follow.node.network),
            node: IngestNodeToml::from_target(
                config.tip_follow.node_source,
                &config.tip_follow.node,
            ),
            storage: StorageToml {
                path: config.tip_follow.storage_path.display().to_string(),
            },
            ingest: IngestToml {
                reorg_window_blocks: Some(config.tip_follow.reorg_window_blocks),
                commit_batch_blocks: config.tip_follow.commit_batch_blocks.get(),
                control: Some(IngestControlToml {
                    listen_addr: config.ingest_control_listen_addr.to_string(),
                    token_path: config
                        .ingest_control_token_path
                        .as_ref()
                        .map(|path| path.display().to_string()),
                }),
                retention: Some(IngestRetentionToml::from_retention(
                    config.chain_event_retention,
                    config.mempool_event_retention,
                )),
            },
            tip_follow: TipFollowToml {
                poll_interval_ms: duration_as_millis_u64(config.tip_follow.poll_interval),
                lag_threshold_blocks: config.tip_follow.lag_threshold_blocks,
            },
        }
    }
}

impl RedactedBackupConfigToml {
    fn from_backup_config(config: &BackupCommandConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            storage: StorageToml {
                path: config.storage_path.display().to_string(),
            },
            backup: BackupToml {
                to_path: config.to_path.display().to_string(),
            },
        }
    }
}

#[derive(Serialize)]
struct StorageToml {
    path: String,
}

#[derive(Serialize)]
struct IngestToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    reorg_window_blocks: Option<u32>,
    commit_batch_blocks: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    control: Option<IngestControlToml>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retention: Option<IngestRetentionToml>,
}

#[derive(Serialize)]
struct IngestControlToml {
    listen_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_path: Option<String>,
}

#[derive(Serialize)]
struct IngestRetentionToml {
    chain_event_retention_hours: u64,
    chain_event_retention_check_interval_ms: u64,
    cursor_at_risk_warning_hours: u64,
    mempool_mined_retention_minutes: u64,
    mempool_invalidated_retention_hours: u64,
    mempool_event_retention_check_interval_ms: u64,
    mempool_cursor_at_risk_warning_minutes: u64,
}

impl IngestRetentionToml {
    fn from_retention(
        chain: ChainEventRetentionConfig,
        mempool: MempoolEventRetentionWorkerConfig,
    ) -> Self {
        Self {
            chain_event_retention_hours: chain.retention_window.map_or(0, duration_hours),
            chain_event_retention_check_interval_ms: duration_as_millis_u64(chain.check_interval),
            cursor_at_risk_warning_hours: duration_hours(chain.cursor_at_risk_warning),
            mempool_mined_retention_minutes: mempool
                .retention
                .mined_retention
                .map_or(0, duration_minutes),
            mempool_invalidated_retention_hours: mempool
                .retention
                .invalidated_retention
                .map_or(0, duration_hours),
            mempool_event_retention_check_interval_ms: duration_as_millis_u64(
                mempool.check_interval,
            ),
            mempool_cursor_at_risk_warning_minutes: duration_minutes(
                mempool.cursor_at_risk_warning,
            ),
        }
    }
}

#[derive(Serialize)]
struct BackfillToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    from_height: Option<u32>,
    to_height: u32,
    allow_near_tip_finalize: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint_height: Option<u32>,
    coverage: BackfillCoverage,
}

#[derive(Serialize)]
struct TipFollowToml {
    poll_interval_ms: u64,
    lag_threshold_blocks: u64,
}

#[derive(Serialize)]
struct BackupToml {
    to_path: String,
}

fn duration_hours(duration: Duration) -> u64 {
    duration.as_secs() / 3_600
}

fn duration_minutes(duration: Duration) -> u64 {
    duration.as_secs() / 60
}
