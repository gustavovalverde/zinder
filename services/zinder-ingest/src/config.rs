//! Configuration loading for the `zinder-ingest` binary.
//!
//! The ingest binary has two top-level config shapes:
//!
//! - [`IngestCommandConfig`] resolves the unified loop's input
//!   (`zinder-ingest --config X` default invocation, and `zinder-ingest
//!   probe`).
//! - [`BackupCommandConfig`] resolves the `backup` subcommand. Backup
//!   keeps its own command config because it does not run the loop.

use std::{net::SocketAddr, num::NonZeroU32, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::BlockHeight;
use zinder_ingest::{
    BulkCatchupConfig, ChainEventRetentionConfig, DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
    IngestDeriveConfig, IngestError, IngestLoopConfig, IngestModifiers,
    MempoolEventRetentionWorkerConfig, NodeSourceKind, PhasesConfig, TipFollowPhaseConfig,
};
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, IngestControlSection,
    IngestControlWriterToml, NetworkSection, NetworkToml, NodeToml, OpsSection, OpsToml,
    PrimaryStorageSection, PrimaryStorageToml, ResolvedIngestControlWriter, ResolvedPrimaryStorage,
    ResolvedRetention, RetentionSection, RetentionToml, ServiceIdentifier, StorageTuningToml,
    duration_as_millis_u64, require_field, resolve_ingest_control_writer, resolve_ops_listen_addr,
    resolve_primary_storage, resolve_retention,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::{MempoolEventRetentionConfig, StorageTuning};

use crate::cli::parse::{
    parse_commit_batch_blocks, parse_node_source, parse_poll_interval_ms, parse_reorg_window_blocks,
};

const DEFAULT_REORG_WINDOW_BLOCKS: u32 = 100;
const DEFAULT_COMMIT_BATCH_BLOCKS: u32 = 1_000;
const DEFAULT_MAX_TRANSPARENT_PREVOUT_STORE_LOOKUPS_PER_BATCH: u32 = 250_000;
const DEFAULT_FETCH_CONCURRENCY: u32 = 32;
const DEFAULT_FLUSH_INTERVAL_EPOCHS: u32 = 5;
const DERIVE_CONCURRENCY_FLOOR: u32 = 4;
const DERIVE_CONCURRENCY_CEILING: u32 = 32;
const DEFAULT_TIP_FOLLOW_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_ALLOW_NEAR_TIP_FINALIZE: bool = false;
const DEFAULT_INGEST_COVERAGE: IngestCoverage = IngestCoverage::Explicit;

/// Fully loaded command configuration for the unified `zinder-ingest`
/// run (no subcommand and the `probe` subcommand both consume this).
#[derive(Debug)]
pub(crate) struct IngestCommandConfig {
    pub(crate) loop_config: IngestLoopConfig,
    pub(crate) coverage: IngestCoverage,
    pub(crate) ingest_control_listen_addr: SocketAddr,
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
    pub(crate) storage_tuning: StorageTuning,
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
    pub(crate) commit_batch_blocks: Option<u32>,
    pub(crate) max_transparent_prevout_store_lookups_per_batch: Option<u32>,
    pub(crate) fetch_concurrency: Option<u32>,
    pub(crate) derive_concurrency: Option<u32>,
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
        .with_default("ingest.reorg_window_blocks", DEFAULT_REORG_WINDOW_BLOCKS)?
        .with_default(
            "ingest.bulk_catchup.commit_batch_blocks",
            DEFAULT_COMMIT_BATCH_BLOCKS,
        )?
        .with_default(
            "ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch",
            DEFAULT_MAX_TRANSPARENT_PREVOUT_STORE_LOOKUPS_PER_BATCH,
        )?
        .with_default(
            "ingest.bulk_catchup.fetch_concurrency",
            DEFAULT_FETCH_CONCURRENCY,
        )?
        .with_default("ingest.derive.concurrency", default_derive_concurrency())?
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
            "ingest.bulk_catchup.commit_batch_blocks",
            overrides.commit_batch_blocks,
        )?
        .with_override_if(
            "ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch",
            overrides.max_transparent_prevout_store_lookups_per_batch,
        )?
        .with_override_if(
            "ingest.bulk_catchup.fetch_concurrency",
            overrides.fetch_concurrency,
        )?
        .with_override_if("ingest.derive.concurrency", overrides.derive_concurrency)?
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
    storage: PrimaryStorageSection,
    ingest: IngestSection,
    ingest_control: IngestControlSection,
    retention: RetentionSection,
    backup: BackupSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestSection {
    /// Source-adapter selector. Today only `zebra-json-rpc` is
    /// implemented; [ADR-0016](../../../docs/adrs/0016-source-streaming-pipeline.md)
    /// reserves `auto`, `zebra-indexer-grpc`, and `zebra-in-process`.
    source: Option<String>,
    /// Chain-truth invariant: how deep into the upstream tip the
    /// finalized cliff sits. Defaults to `100`.
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
    concurrency: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IngestBulkCatchupSection {
    commit_batch_blocks: Option<u32>,
    max_transparent_prevout_store_lookups_per_batch: Option<u32>,
    fetch_concurrency: Option<u32>,
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
    let ResolvedPrimaryStorage {
        path: storage_path,
        tuning: storage_tuning,
    } = resolve_primary_storage(config.storage)?;

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

    let commit_batch_blocks_raw = require_field(
        config.ingest.bulk_catchup.commit_batch_blocks,
        "ingest.bulk_catchup.commit_batch_blocks",
    )?;
    let commit_batch_blocks = parse_commit_batch_blocks(commit_batch_blocks_raw)?;

    let max_transparent_prevout_store_lookups_per_batch_raw = require_field(
        config
            .ingest
            .bulk_catchup
            .max_transparent_prevout_store_lookups_per_batch,
        "ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch",
    )?;
    let max_transparent_prevout_store_lookups_per_batch =
        NonZeroU32::new(max_transparent_prevout_store_lookups_per_batch_raw).ok_or_else(|| {
            ConfigError::invalid(
                "ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch must be greater than zero",
            )
        })?;

    let fetch_concurrency_raw = require_field(
        config.ingest.bulk_catchup.fetch_concurrency,
        "ingest.bulk_catchup.fetch_concurrency",
    )?;
    let fetch_concurrency = NonZeroU32::new(fetch_concurrency_raw).ok_or_else(|| {
        ConfigError::invalid("ingest.bulk_catchup.fetch_concurrency must be greater than zero")
    })?;

    let derive_concurrency_raw = require_field(
        config.ingest.derive.concurrency,
        "ingest.derive.concurrency",
    )?;
    let derive_concurrency = NonZeroU32::new(derive_concurrency_raw).ok_or_else(|| {
        ConfigError::invalid("ingest.derive.concurrency must be greater than zero")
    })?;

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
        listen_addr: ingest_control_listen_addr_opt,
        bearer_token_path: ingest_control_bearer_token_path,
        bearer_token: ingest_control_bearer_token,
    } = resolve_ingest_control_writer(config.ingest_control)?;
    let ingest_control_listen_addr = ingest_control_listen_addr_opt
        .ok_or_else(|| ConfigError::missing_field("ingest_control.listen_addr"))?;
    let retention = resolve_retention(config.retention)?;
    let ops_listen_addr = resolve_ops_listen_addr(config.ops)?;
    let node_target = NodeTarget::resolve(network, config.node).map_err(ConfigError::from)?;

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
        storage_tuning,
        reorg_window_blocks,
        phases: PhasesConfig {
            catchup_threshold_blocks,
        },
        derive: IngestDeriveConfig {
            concurrency: derive_concurrency,
        },
        bulk_catchup: BulkCatchupConfig {
            commit_batch_blocks,
            max_transparent_prevout_store_lookups_per_batch,
            fetch_concurrency,
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
        tuning: storage_tuning,
    } = resolve_primary_storage(config.storage)?;
    let to_path = config
        .backup
        .to_path
        .ok_or_else(|| ConfigError::missing_field("backup.to_path"))?;

    Ok(BackupCommandConfig {
        network,
        storage_path,
        storage_tuning,
        to_path,
    })
}

#[derive(Serialize)]
struct RedactedIngestConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    node: NodeToml,
    storage: PrimaryStorageToml,
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
    fn from_ingest_config(config: &IngestCommandConfig) -> Self {
        let loop_config = &config.loop_config;
        Self {
            network: NetworkToml::from_network(loop_config.node.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            node: NodeToml::from_node_target(&loop_config.node),
            storage: PrimaryStorageToml {
                path: loop_config.storage_path.display().to_string(),
                tuning: StorageTuningToml::from_resolved(loop_config.storage_tuning),
            },
            ingest: IngestToml {
                source: node_source_name(loop_config.node_source),
                reorg_window_blocks: loop_config.reorg_window_blocks,
                phases: IngestPhasesToml {
                    catchup_threshold_blocks: loop_config.phases.catchup_threshold_blocks,
                },
                derive: IngestDeriveToml {
                    concurrency: loop_config.derive.concurrency.get(),
                },
                bulk_catchup: IngestBulkCatchupToml {
                    commit_batch_blocks: loop_config.bulk_catchup.commit_batch_blocks.get(),
                    max_transparent_prevout_store_lookups_per_batch: loop_config
                        .bulk_catchup
                        .max_transparent_prevout_store_lookups_per_batch
                        .get(),
                    fetch_concurrency: loop_config.bulk_catchup.fetch_concurrency.get(),
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
                Some(config.ingest_control_listen_addr),
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
                tuning: StorageTuningToml::from_resolved(config.storage_tuning),
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
struct IngestPhasesToml {
    catchup_threshold_blocks: u32,
}

#[derive(Serialize)]
struct IngestDeriveToml {
    concurrency: u32,
}

#[derive(Serialize)]
struct IngestBulkCatchupToml {
    commit_batch_blocks: u32,
    max_transparent_prevout_store_lookups_per_batch: u32,
    fetch_concurrency: u32,
    flush_interval_epochs: u32,
}

/// Computes the default `ingest.derive.concurrency` from available
/// logical cores.
///
/// Subtracts one to leave a CPU for the Tokio reactor and other ingest
/// work, then clamps into `[4, 32]`. The ceiling mirrors Zebra's
/// `full_verify_concurrency_limit` precedent. See
/// [ADR-0021](../../../docs/adrs/0021-parallel-block-derivation.md).
fn default_derive_concurrency() -> u32 {
    let logical_cores =
        u32::try_from(std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get))
            .unwrap_or(8);
    logical_cores
        .saturating_sub(1)
        .clamp(DERIVE_CONCURRENCY_FLOOR, DERIVE_CONCURRENCY_CEILING)
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
