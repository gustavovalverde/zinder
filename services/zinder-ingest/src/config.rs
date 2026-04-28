//! Configuration loading for the `zinder-ingest` command.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use ::config::{Config, File, FileFormat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::BlockHeight;
use zinder_ingest::{
    BackfillConfig, ChainEventRetentionConfig, DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
    IngestError, NodeSourceKind, TipFollowConfig,
};
use zinder_runtime::{
    ConfigError, path_to_config_string, require_string, zinder_environment_source,
};

use crate::cli::parse::{
    parse_commit_batch_blocks, parse_max_response_bytes, parse_network, parse_node_source,
    parse_poll_interval_ms, parse_reorg_window_blocks,
};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeTarget, SourceChainCheckpoint,
};

const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES.get();
const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const DEFAULT_COMMIT_BATCH_BLOCKS: u32 = 1000;
const DEFAULT_ALLOW_NEAR_TIP_FINALIZE: bool = false;
const DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_INGEST_CONTROL_LISTEN_ADDR: &str = "127.0.0.1:9100";
const DEFAULT_CHAIN_EVENT_RETENTION_HOURS: u64 = 168;
const DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS: u64 = 60_000;
const DEFAULT_CURSOR_AT_RISK_WARNING_HOURS: u64 = 24;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackfillCoverage {
    /// Use explicitly supplied heights.
    Explicit,
    /// Derive the historical floor needed by lightwalletd-compatible wallets.
    WalletServing,
}

impl BackfillCoverage {
    const fn as_str(self) -> &'static str {
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
    pub(crate) chain_event_retention: ChainEventRetentionConfig,
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
}

/// Loads and validates backfill configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_backfill_config(
    config_path: Option<PathBuf>,
    overrides: BackfillConfigOverrides,
) -> Result<BackfillCommandConfig, IngestConfigError> {
    let mut builder = Config::builder()
        .set_default("node.request_timeout_secs", DEFAULT_REQUEST_TIMEOUT_SECS)
        .map_err(ConfigError::load)?
        .set_default("node.max_response_bytes", DEFAULT_MAX_RESPONSE_BYTES)
        .map_err(ConfigError::load)?
        .set_default("ingest.commit_batch_blocks", DEFAULT_COMMIT_BATCH_BLOCKS)
        .map_err(ConfigError::load)?
        .set_default(
            "backfill.allow_near_tip_finalize",
            DEFAULT_ALLOW_NEAR_TIP_FINALIZE,
        )
        .map_err(ConfigError::load)?;

    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }

    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_backfill_overrides(builder, overrides)?;

    let raw_config: IngestConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;

    resolve_backfill_config(raw_config)
}

/// Loads and validates tip-follow configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_tip_follow_config(
    config_path: Option<PathBuf>,
    overrides: TipFollowConfigOverrides,
) -> Result<TipFollowCommandConfig, IngestConfigError> {
    let mut builder = Config::builder()
        .set_default("node.request_timeout_secs", DEFAULT_REQUEST_TIMEOUT_SECS)
        .map_err(ConfigError::load)?
        .set_default("node.max_response_bytes", DEFAULT_MAX_RESPONSE_BYTES)
        .map_err(ConfigError::load)?
        .set_default("ingest.reorg_window_blocks", DEFAULT_REORG_WINDOW_BLOCKS)
        .map_err(ConfigError::load)?
        .set_default("ingest.commit_batch_blocks", DEFAULT_COMMIT_BATCH_BLOCKS)
        .map_err(ConfigError::load)?
        .set_default(
            "tip_follow.poll_interval_ms",
            DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS,
        )
        .map_err(ConfigError::load)?
        .set_default(
            "tip_follow.lag_threshold_blocks",
            DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        )
        .map_err(ConfigError::load)?
        .set_default(
            "ingest.retention.chain_event_retention_hours",
            DEFAULT_CHAIN_EVENT_RETENTION_HOURS,
        )
        .map_err(ConfigError::load)?
        .set_default(
            "ingest.retention.chain_event_retention_check_interval_ms",
            DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS,
        )
        .map_err(ConfigError::load)?
        .set_default(
            "ingest.retention.cursor_at_risk_warning_hours",
            DEFAULT_CURSOR_AT_RISK_WARNING_HOURS,
        )
        .map_err(ConfigError::load)?;

    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }

    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_tip_follow_overrides(builder, overrides)?;

    let raw_config: IngestConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;

    resolve_tip_follow_config(raw_config)
}

/// Loads and validates backup configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_backup_config(
    config_path: Option<PathBuf>,
    overrides: BackupConfigOverrides,
) -> Result<BackupCommandConfig, IngestConfigError> {
    let mut builder = Config::builder();

    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }

    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_backup_overrides(builder, overrides)?;

    let raw_config: IngestConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;

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
    network: NetworkConfig,
    node: NodeConfig,
    storage: StorageConfig,
    ingest: IngestSectionConfig,
    backfill: BackfillSectionConfig,
    tip_follow: TipFollowSectionConfig,
    backup: BackupSectionConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NetworkConfig {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NodeConfig {
    source: Option<String>,
    json_rpc_addr: Option<String>,
    request_timeout_secs: Option<u64>,
    max_response_bytes: Option<u64>,
    auth: NodeAuthConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NodeAuthConfig {
    method: Option<String>,
    username: Option<String>,
    password: Option<String>,
    path: Option<PathBuf>,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestRetentionSectionConfig {
    chain_event_retention_hours: Option<u64>,
    chain_event_retention_check_interval_ms: Option<u64>,
    cursor_at_risk_warning_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackfillSectionConfig {
    from_height: Option<u32>,
    to_height: Option<u32>,
    allow_near_tip_finalize: Option<bool>,
    checkpoint_height: Option<u32>,
    coverage: Option<String>,
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

struct CommonNodeOverrides {
    network: Option<String>,
    node_source: Option<String>,
    json_rpc_addr: Option<String>,
    node_auth_method: Option<String>,
    node_auth_username: Option<String>,
    node_auth_path: Option<PathBuf>,
    storage_path: Option<PathBuf>,
    request_timeout_secs: Option<u64>,
    max_response_bytes: Option<u64>,
    commit_batch_blocks: Option<u32>,
}

fn apply_common_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: CommonNodeOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, IngestConfigError> {
    if let Some(network) = overrides.network {
        builder = builder
            .set_override("network.name", network)
            .map_err(ConfigError::load)?;
    }
    if let Some(node_source) = overrides.node_source {
        builder = builder
            .set_override("node.source", node_source)
            .map_err(ConfigError::load)?;
    }
    if let Some(json_rpc_addr) = overrides.json_rpc_addr {
        builder = builder
            .set_override("node.json_rpc_addr", json_rpc_addr)
            .map_err(ConfigError::load)?;
    }
    if let Some(node_auth_method) = overrides.node_auth_method {
        builder = builder
            .set_override("node.auth.method", node_auth_method)
            .map_err(ConfigError::load)?;
    }
    if let Some(node_auth_username) = overrides.node_auth_username {
        builder = builder
            .set_override("node.auth.username", node_auth_username)
            .map_err(ConfigError::load)?;
    }
    if let Some(node_auth_path) = overrides.node_auth_path {
        builder = builder
            .set_override(
                "node.auth.path",
                path_to_config_string(node_auth_path, "node.auth.path")?,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(storage_path) = overrides.storage_path {
        builder = builder
            .set_override(
                "storage.path",
                path_to_config_string(storage_path, "storage.path")?,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(request_timeout_secs) = overrides.request_timeout_secs {
        builder = builder
            .set_override("node.request_timeout_secs", request_timeout_secs)
            .map_err(ConfigError::load)?;
    }
    if let Some(max_response_bytes) = overrides.max_response_bytes {
        builder = builder
            .set_override("node.max_response_bytes", max_response_bytes)
            .map_err(ConfigError::load)?;
    }
    if let Some(commit_batch_blocks) = overrides.commit_batch_blocks {
        builder = builder
            .set_override("ingest.commit_batch_blocks", commit_batch_blocks)
            .map_err(ConfigError::load)?;
    }

    Ok(builder)
}

fn apply_backfill_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: BackfillConfigOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, IngestConfigError> {
    builder = apply_common_overrides(
        builder,
        CommonNodeOverrides {
            network: overrides.network,
            node_source: overrides.node_source,
            json_rpc_addr: overrides.json_rpc_addr,
            node_auth_method: overrides.node_auth_method,
            node_auth_username: overrides.node_auth_username,
            node_auth_path: overrides.node_auth_path,
            storage_path: overrides.storage_path,
            request_timeout_secs: overrides.request_timeout_secs,
            max_response_bytes: overrides.max_response_bytes,
            commit_batch_blocks: overrides.commit_batch_blocks,
        },
    )?;

    if let Some(from_height) = overrides.from_height {
        builder = builder
            .set_override("backfill.from_height", from_height)
            .map_err(ConfigError::load)?;
    }
    if let Some(to_height) = overrides.to_height {
        builder = builder
            .set_override("backfill.to_height", to_height)
            .map_err(ConfigError::load)?;
    }
    if let Some(allow_near_tip_finalize) = overrides.allow_near_tip_finalize {
        builder = builder
            .set_override("backfill.allow_near_tip_finalize", allow_near_tip_finalize)
            .map_err(ConfigError::load)?;
    }
    if let Some(checkpoint_height) = overrides.checkpoint_height {
        builder = builder
            .set_override("backfill.checkpoint_height", checkpoint_height)
            .map_err(ConfigError::load)?;
    }
    if overrides.wallet_serving == Some(true) {
        builder = builder
            .set_override(
                "backfill.coverage",
                BackfillCoverage::WalletServing.as_str(),
            )
            .map_err(ConfigError::load)?;
    }

    Ok(builder)
}

fn apply_tip_follow_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: TipFollowConfigOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, IngestConfigError> {
    builder = apply_common_overrides(
        builder,
        CommonNodeOverrides {
            network: overrides.network,
            node_source: overrides.node_source,
            json_rpc_addr: overrides.json_rpc_addr,
            node_auth_method: overrides.node_auth_method,
            node_auth_username: overrides.node_auth_username,
            node_auth_path: overrides.node_auth_path,
            storage_path: overrides.storage_path,
            request_timeout_secs: overrides.request_timeout_secs,
            max_response_bytes: overrides.max_response_bytes,
            commit_batch_blocks: overrides.commit_batch_blocks,
        },
    )?;

    if let Some(reorg_window_blocks) = overrides.reorg_window_blocks {
        builder = builder
            .set_override("ingest.reorg_window_blocks", reorg_window_blocks)
            .map_err(ConfigError::load)?;
    }
    if let Some(poll_interval_ms) = overrides.poll_interval_ms {
        builder = builder
            .set_override("tip_follow.poll_interval_ms", poll_interval_ms)
            .map_err(ConfigError::load)?;
    }
    if let Some(lag_threshold_blocks) = overrides.lag_threshold_blocks {
        builder = builder
            .set_override("tip_follow.lag_threshold_blocks", lag_threshold_blocks)
            .map_err(ConfigError::load)?;
    }
    if let Some(ingest_control_listen_addr) = overrides.ingest_control_listen_addr {
        builder = builder
            .set_override(
                "ingest.control.listen_addr",
                ingest_control_listen_addr.to_string(),
            )
            .map_err(ConfigError::load)?;
    }
    Ok(builder)
}

fn apply_backup_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: BackupConfigOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, IngestConfigError> {
    if let Some(network) = overrides.network {
        builder = builder
            .set_override("network.name", network)
            .map_err(ConfigError::load)?;
    }
    if let Some(storage_path) = overrides.storage_path {
        builder = builder
            .set_override(
                "storage.path",
                path_to_config_string(storage_path, "storage.path")?,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(to_path) = overrides.to_path {
        builder = builder
            .set_override(
                "backup.to_path",
                path_to_config_string(to_path, "backup.to_path")?,
            )
            .map_err(ConfigError::load)?;
    }

    Ok(builder)
}

const fn node_source_name(node_source: NodeSourceKind) -> &'static str {
    match node_source {
        NodeSourceKind::ZebraJsonRpc => "zebra-json-rpc",
    }
}

fn resolve_backfill_config(
    config: IngestConfig,
) -> Result<BackfillCommandConfig, IngestConfigError> {
    let network = require_string(config.network.name, "network.name")?;
    let node_source = require_string(config.node.source, "node.source")?;
    let json_rpc_addr = require_string(config.node.json_rpc_addr, "node.json_rpc_addr")?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let coverage = parse_backfill_coverage(config.backfill.coverage.as_deref())?;
    let from_height = resolve_backfill_from_height(config.backfill.from_height, coverage)?;
    let to_height =
        require_u32(config.backfill.to_height, "backfill.to_height").map(BlockHeight::new)?;
    if matches!(coverage, BackfillCoverage::WalletServing)
        && config.backfill.checkpoint_height.is_some()
    {
        return Err(ConfigError::invalid(
            "backfill.coverage = \"wallet-serving\" derives checkpoint_height from the node; remove backfill.checkpoint_height",
        )
        .into());
    }
    let request_timeout_secs = require_u64(
        config.node.request_timeout_secs,
        "node.request_timeout_secs",
    )?;
    let max_response_bytes =
        require_u64(config.node.max_response_bytes, "node.max_response_bytes")?;
    let commit_batch_blocks = require_u32(
        config.ingest.commit_batch_blocks,
        "ingest.commit_batch_blocks",
    )?;
    let allow_near_tip_finalize = require_bool(
        config.backfill.allow_near_tip_finalize,
        "backfill.allow_near_tip_finalize",
    )?;
    if matches!(coverage, BackfillCoverage::WalletServing) && allow_near_tip_finalize {
        return Err(ConfigError::invalid(
            "backfill.coverage = \"wallet-serving\" cannot be combined with backfill.allow_near_tip_finalize = true; serving stores must stop outside the reorg window",
        )
        .into());
    }
    let node_source = parse_node_source(&node_source)?;
    let node_auth = resolve_node_auth(config.node.auth)?;

    Ok(BackfillCommandConfig {
        node: NodeTarget::new(
            parse_network(&network)?,
            json_rpc_addr,
            node_auth,
            Duration::from_secs(request_timeout_secs),
            parse_max_response_bytes(max_response_bytes)?,
        ),
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

fn parse_backfill_coverage(coverage: Option<&str>) -> Result<BackfillCoverage, IngestConfigError> {
    match coverage.unwrap_or("explicit") {
        "explicit" => Ok(BackfillCoverage::Explicit),
        "wallet-serving" => Ok(BackfillCoverage::WalletServing),
        other => Err(ConfigError::invalid(format!(
            "unknown backfill.coverage: {other}; expected explicit or wallet-serving"
        ))
        .into()),
    }
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
    let network = require_string(config.network.name, "network.name")?;
    let node_source = require_string(config.node.source, "node.source")?;
    let json_rpc_addr = require_string(config.node.json_rpc_addr, "node.json_rpc_addr")?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let request_timeout_secs = require_u64(
        config.node.request_timeout_secs,
        "node.request_timeout_secs",
    )?;
    let max_response_bytes =
        require_u64(config.node.max_response_bytes, "node.max_response_bytes")?;
    let reorg_window_blocks = require_u32(
        config.ingest.reorg_window_blocks,
        "ingest.reorg_window_blocks",
    )?;
    let commit_batch_blocks = require_u32(
        config.ingest.commit_batch_blocks,
        "ingest.commit_batch_blocks",
    )?;
    let poll_interval_ms = require_u64(
        config.tip_follow.poll_interval_ms,
        "tip_follow.poll_interval_ms",
    )?;
    let lag_threshold_blocks = require_u64(
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
    let chain_event_retention_hours = require_u64(
        config.ingest.retention.chain_event_retention_hours,
        "ingest.retention.chain_event_retention_hours",
    )?;
    let chain_event_retention_check_interval_ms = require_u64(
        config
            .ingest
            .retention
            .chain_event_retention_check_interval_ms,
        "ingest.retention.chain_event_retention_check_interval_ms",
    )?;
    let cursor_at_risk_warning_hours = require_u64(
        config.ingest.retention.cursor_at_risk_warning_hours,
        "ingest.retention.cursor_at_risk_warning_hours",
    )?;
    if chain_event_retention_check_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "ingest.retention.chain_event_retention_check_interval_ms must be greater than zero",
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
    let node_source = parse_node_source(&node_source)?;
    let node_auth = resolve_node_auth(config.node.auth)?;

    Ok(TipFollowCommandConfig {
        tip_follow: TipFollowConfig {
            node: NodeTarget::new(
                parse_network(&network)?,
                json_rpc_addr,
                node_auth,
                Duration::from_secs(request_timeout_secs),
                parse_max_response_bytes(max_response_bytes)?,
            ),
            node_source,
            storage_path,
            reorg_window_blocks: parse_reorg_window_blocks(reorg_window_blocks)?,
            commit_batch_blocks: parse_commit_batch_blocks(commit_batch_blocks)?,
            poll_interval: parse_poll_interval_ms(poll_interval_ms)?,
            lag_threshold_blocks,
        },
        ingest_control_listen_addr,
        chain_event_retention: ChainEventRetentionConfig {
            retention_window: (chain_event_retention_hours > 0)
                .then(|| Duration::from_secs(chain_event_retention_hours.saturating_mul(3_600))),
            check_interval: Duration::from_millis(chain_event_retention_check_interval_ms),
            cursor_at_risk_warning: Duration::from_secs(
                cursor_at_risk_warning_hours.saturating_mul(3_600),
            ),
        },
    })
}

fn resolve_backup_config(config: IngestConfig) -> Result<BackupCommandConfig, IngestConfigError> {
    let network = require_string(config.network.name, "network.name")?;
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let to_path = config
        .backup
        .to_path
        .ok_or_else(|| ConfigError::missing_field("backup.to_path"))?;

    Ok(BackupCommandConfig {
        network: parse_network(&network)?,
        storage_path,
        to_path,
    })
}

fn resolve_node_auth(auth: NodeAuthConfig) -> Result<NodeAuth, IngestConfigError> {
    let method = auth.method.as_deref().unwrap_or("none");

    match method {
        "none" => {
            reject_present(auth.username.is_some(), "node.auth.username", "none")?;
            reject_present(auth.password.is_some(), "node.auth.password", "none")?;
            reject_present(auth.path.is_some(), "node.auth.path", "none")?;
            Ok(NodeAuth::None)
        }
        "basic" => {
            reject_present(auth.path.is_some(), "node.auth.path", "basic")?;
            let username = require_string(auth.username, "node.auth.username")?;
            let password = require_string(auth.password, "node.auth.password")?;
            Ok(NodeAuth::basic(username, password))
        }
        "cookie" => {
            reject_present(auth.username.is_some(), "node.auth.username", "cookie")?;
            reject_present(auth.password.is_some(), "node.auth.password", "cookie")?;
            let path = auth
                .path
                .ok_or_else(|| ConfigError::missing_field("node.auth.path"))?;

            Ok(NodeAuth::Cookie { path })
        }
        _ => Err(ConfigError::invalid(format!("unknown node auth method: {method}")).into()),
    }
}

fn reject_present(
    is_field_present: bool,
    field: &'static str,
    method: &'static str,
) -> Result<(), ConfigError> {
    if is_field_present {
        return Err(ConfigError::invalid(format!(
            "{field} is not valid when node.auth.method is {method}"
        )));
    }

    Ok(())
}

fn require_u32(field_value: Option<u32>, field: &'static str) -> Result<u32, ConfigError> {
    field_value.ok_or_else(|| ConfigError::missing_field(field))
}

fn require_u64(field_value: Option<u64>, field: &'static str) -> Result<u64, ConfigError> {
    field_value.ok_or_else(|| ConfigError::missing_field(field))
}

fn require_bool(field_value: Option<bool>, field: &'static str) -> Result<bool, ConfigError> {
    field_value.ok_or_else(|| ConfigError::missing_field(field))
}

#[derive(Serialize)]
struct RedactedBackfillConfigToml {
    network: NetworkToml,
    node: NodeToml,
    storage: StorageToml,
    ingest: IngestToml,
    backfill: BackfillToml,
}

#[derive(Serialize)]
struct RedactedTipFollowConfigToml {
    network: NetworkToml,
    node: NodeToml,
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

impl RedactedBackfillConfigToml {
    fn from_backfill_config(config: &BackfillCommandConfig) -> Self {
        Self {
            network: NetworkToml {
                name: config.node.network.name(),
            },
            node: NodeToml {
                source: node_source_name(config.node_source),
                json_rpc_addr: config.node.json_rpc_addr.clone(),
                request_timeout_secs: config.node.request_timeout.as_secs(),
                max_response_bytes: config.node.max_response_bytes.get(),
                auth: NodeAuthToml::from_node_auth(&config.node.node_auth),
            },
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
                coverage: config.coverage.as_str(),
            },
        }
    }
}

impl RedactedTipFollowConfigToml {
    fn from_tip_follow_config(config: &TipFollowCommandConfig) -> Self {
        Self {
            network: NetworkToml {
                name: config.tip_follow.node.network.name(),
            },
            node: NodeToml {
                source: node_source_name(config.tip_follow.node_source),
                json_rpc_addr: config.tip_follow.node.json_rpc_addr.clone(),
                request_timeout_secs: config.tip_follow.node.request_timeout.as_secs(),
                max_response_bytes: config.tip_follow.node.max_response_bytes.get(),
                auth: NodeAuthToml::from_node_auth(&config.tip_follow.node.node_auth),
            },
            storage: StorageToml {
                path: config.tip_follow.storage_path.display().to_string(),
            },
            ingest: IngestToml {
                reorg_window_blocks: Some(config.tip_follow.reorg_window_blocks),
                commit_batch_blocks: config.tip_follow.commit_batch_blocks.get(),
                control: Some(IngestControlToml {
                    listen_addr: config.ingest_control_listen_addr.to_string(),
                }),
                retention: Some(IngestRetentionToml::from_chain_event_retention(
                    config.chain_event_retention,
                )),
            },
            tip_follow: TipFollowToml {
                poll_interval_ms: duration_millis(config.tip_follow.poll_interval),
                lag_threshold_blocks: config.tip_follow.lag_threshold_blocks,
            },
        }
    }
}

impl RedactedBackupConfigToml {
    fn from_backup_config(config: &BackupCommandConfig) -> Self {
        Self {
            network: NetworkToml {
                name: config.network.name(),
            },
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
struct NetworkToml {
    name: &'static str,
}

#[derive(Serialize)]
struct NodeToml {
    source: &'static str,
    json_rpc_addr: String,
    request_timeout_secs: u64,
    max_response_bytes: u64,
    auth: NodeAuthToml,
}

#[derive(Serialize)]
struct NodeAuthToml {
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'static str>,
}

impl NodeAuthToml {
    fn from_node_auth(node_auth: &NodeAuth) -> Self {
        match node_auth {
            NodeAuth::None => Self {
                method: "none",
                username: None,
                password: None,
                path: None,
            },
            NodeAuth::Cookie { .. } => Self {
                method: "cookie",
                username: None,
                password: None,
                path: Some("[REDACTED]"),
            },
            NodeAuth::Basic { username, .. } => Self {
                method: "basic",
                username: Some(username.clone()),
                password: Some("[REDACTED]"),
                path: None,
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
}

#[derive(Serialize)]
struct IngestRetentionToml {
    chain_event_retention_hours: u64,
    chain_event_retention_check_interval_ms: u64,
    cursor_at_risk_warning_hours: u64,
}

impl IngestRetentionToml {
    fn from_chain_event_retention(config: ChainEventRetentionConfig) -> Self {
        Self {
            chain_event_retention_hours: config.retention_window.map_or(0, duration_hours),
            chain_event_retention_check_interval_ms: duration_millis(config.check_interval),
            cursor_at_risk_warning_hours: duration_hours(config.cursor_at_risk_warning),
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
    coverage: &'static str,
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

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn duration_hours(duration: Duration) -> u64 {
    duration.as_secs() / 3_600
}
