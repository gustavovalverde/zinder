//! Configuration loading for the `zinder-query` binary.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, NetworkSection, NetworkToml,
    NodeToml, duration_as_millis_u64, require_field,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::StoreError;

const DEFAULT_INGEST_CONTROL_ADDR: &str = "http://127.0.0.1:9100";
const DEFAULT_CHAIN_EVENT_RETENTION_HOURS: u64 = 168;
const DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES: u64 = 60;
const DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS: u64 = 24;

/// Resolved query runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct QueryConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) secondary_path: PathBuf,
    pub(crate) secondary_catchup_interval: Duration,
    pub(crate) secondary_replica_lag_threshold_chain_epochs: u64,
    pub(crate) ingest_control_addr: String,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) chain_event_retention_seconds: u64,
    pub(crate) mempool_mined_retention_seconds: u64,
    pub(crate) mempool_invalidated_retention_seconds: u64,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) grpc: QueryGrpcConfig,
    pub(crate) explorer_derive: Option<ExplorerDeriveConfig>,
    /// Optional node broadcaster. Network must match `QueryConfig.network`
    /// when present; the resolver enforces this.
    pub(crate) broadcaster: Option<NodeTarget>,
}

/// Resolved derive-plane explorer proxy configuration.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerDeriveConfig {
    pub(crate) endpoint: String,
    pub(crate) token_path: Option<PathBuf>,
    pub(crate) bearer_token: Option<BearerToken>,
    pub(crate) probe_interval: Duration,
}

/// Resolved gRPC runtime options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QueryGrpcConfig {
    pub(crate) enable_reflection: bool,
    pub(crate) enable_health: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionAdvertisementSeconds {
    chain_event: u64,
    mempool_mined: u64,
    mempool_invalidated: u64,
}

/// Command-line overrides for the query command.
#[derive(Debug, Default)]
pub(crate) struct QueryConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) ingest_control_addr: Option<String>,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
    pub(crate) chain_event_retention_hours: Option<u64>,
    pub(crate) mempool_mined_retention_minutes: Option<u64>,
    pub(crate) mempool_invalidated_retention_hours: Option<u64>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) node_json_rpc_addr: Option<String>,
    pub(crate) derive_explorer_endpoint: Option<String>,
    pub(crate) derive_explorer_token_path: Option<PathBuf>,
    pub(crate) derive_explorer_probe_interval_ms: Option<u64>,
}

/// Error returned while resolving query configuration or running the gRPC server.
#[derive(Debug, Error)]
pub(crate) enum QueryConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("node source initialization failed: {0}")]
    Source(Box<zinder_source::SourceError>),

    #[error("gRPC transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC reflection initialization failed: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),

    #[error("invalid ingest-control bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
}

/// Loads and validates query configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_query_config(
    config_path: Option<PathBuf>,
    overrides: QueryConfigOverrides,
) -> Result<QueryConfig, QueryConfigError> {
    let raw_config: QueryRawConfig = ConfigLoader::new()
        .with_default("query.listen_addr", "127.0.0.1:9101")?
        .with_default("query.grpc.enable_reflection", true)?
        .with_default("query.grpc.enable_health", true)?
        .with_default(
            "storage.chain_event_retention_hours",
            DEFAULT_CHAIN_EVENT_RETENTION_HOURS,
        )?
        .with_default(
            "storage.mempool_mined_retention_minutes",
            DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES,
        )?
        .with_default(
            "storage.mempool_invalidated_retention_hours",
            DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS,
        )?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_path_if("storage.secondary_path", overrides.secondary_path)?
        .with_override_if("storage.ingest_control_addr", overrides.ingest_control_addr)?
        .with_override_path_if(
            "storage.ingest_control_token_path",
            overrides.ingest_control_token_path,
        )?
        .with_override_if(
            "storage.chain_event_retention_hours",
            overrides.chain_event_retention_hours,
        )?
        .with_override_if(
            "storage.mempool_mined_retention_minutes",
            overrides.mempool_mined_retention_minutes,
        )?
        .with_override_if(
            "storage.mempool_invalidated_retention_hours",
            overrides.mempool_invalidated_retention_hours,
        )?
        .with_override_if(
            "query.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if("node.json_rpc_addr", overrides.node_json_rpc_addr)?
        .with_override_if(
            "derive.explorer.endpoint",
            overrides.derive_explorer_endpoint,
        )?
        .with_override_path_if(
            "derive.explorer.token_path",
            overrides.derive_explorer_token_path,
        )?
        .with_override_if(
            "derive.explorer.probe_interval_ms",
            overrides.derive_explorer_probe_interval_ms,
        )?
        .load()?;

    resolve_query_config(raw_config)
}

/// Renders the effective query configuration in the accepted TOML shape.
pub(crate) fn query_config_toml(config: &QueryConfig) -> Result<String, QueryConfigError> {
    let rendered = toml::to_string(&QueryConfigToml::from_query_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueryRawConfig {
    network: NetworkSection,
    storage: StorageSection,
    query: QuerySection,
    node: NodeSection,
    derive: QueryDeriveSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageSection {
    path: Option<PathBuf>,
    secondary_path: Option<PathBuf>,
    secondary_catchup_interval_ms: Option<u64>,
    secondary_replica_lag_threshold_chain_epochs: Option<u64>,
    ingest_control_addr: Option<String>,
    ingest_control_token_path: Option<PathBuf>,
    chain_event_retention_hours: Option<u64>,
    mempool_mined_retention_minutes: Option<u64>,
    mempool_invalidated_retention_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QuerySection {
    listen_addr: Option<String>,
    grpc: QueryGrpcSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueryGrpcSection {
    enable_reflection: Option<bool>,
    enable_health: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueryDeriveSection {
    explorer: QueryDeriveExplorerSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueryDeriveExplorerSection {
    endpoint: Option<String>,
    token_path: Option<PathBuf>,
    probe_interval_ms: Option<u64>,
}

fn resolve_query_config(config: QueryRawConfig) -> Result<QueryConfig, QueryConfigError> {
    let network = config.network.resolve()?;
    let retention_advertisement = resolve_retention_advertisement(&config.storage);
    let storage_path = config
        .storage
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let secondary_path = config
        .storage
        .secondary_path
        .ok_or_else(|| ConfigError::missing_field("storage.secondary_path"))?;
    let secondary_catchup_interval_ms = config.storage.secondary_catchup_interval_ms.unwrap_or(250);
    if secondary_catchup_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "storage.secondary_catchup_interval_ms must be greater than zero",
        )
        .into());
    }
    let secondary_replica_lag_threshold_chain_epochs = config
        .storage
        .secondary_replica_lag_threshold_chain_epochs
        .unwrap_or(4);
    let ingest_control_addr = config
        .storage
        .ingest_control_addr
        .unwrap_or_else(|| DEFAULT_INGEST_CONTROL_ADDR.to_owned());
    tonic::transport::Endpoint::from_shared(ingest_control_addr.clone()).map_err(|source| {
        ConfigError::invalid(format!(
            "storage.ingest_control_addr {ingest_control_addr} is not a tonic endpoint: {source}"
        ))
    })?;
    let ingest_control_token_path = config.storage.ingest_control_token_path;
    let ingest_control_bearer_token = ingest_control_token_path
        .as_deref()
        .map(BearerToken::from_file)
        .transpose()?;
    let listen_addr_string = require_field(config.query.listen_addr, "query.listen_addr")?;
    let listen_addr = listen_addr_string.parse::<SocketAddr>().map_err(|source| {
        ConfigError::invalid(format!(
            "query.listen_addr {listen_addr_string} is not a socket address: {source}"
        ))
    })?;
    let enable_reflection = require_field(
        config.query.grpc.enable_reflection,
        "query.grpc.enable_reflection",
    )?;
    let enable_health = require_field(config.query.grpc.enable_health, "query.grpc.enable_health")?;
    let broadcaster =
        NodeTarget::resolve_optional(network, config.node).map_err(ConfigError::from)?;
    let explorer_derive = resolve_explorer_derive_config(config.derive.explorer)?;

    Ok(QueryConfig {
        network,
        storage_path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(secondary_catchup_interval_ms),
        secondary_replica_lag_threshold_chain_epochs,
        ingest_control_addr,
        ingest_control_token_path,
        ingest_control_bearer_token,
        chain_event_retention_seconds: retention_advertisement.chain_event,
        mempool_mined_retention_seconds: retention_advertisement.mempool_mined,
        mempool_invalidated_retention_seconds: retention_advertisement.mempool_invalidated,
        listen_addr,
        grpc: QueryGrpcConfig {
            enable_reflection,
            enable_health,
        },
        explorer_derive,
        broadcaster,
    })
}

fn resolve_explorer_derive_config(
    config: QueryDeriveExplorerSection,
) -> Result<Option<ExplorerDeriveConfig>, QueryConfigError> {
    let Some(endpoint) = config.endpoint else {
        return Ok(None);
    };
    tonic::transport::Endpoint::from_shared(endpoint.clone()).map_err(|source| {
        ConfigError::invalid(format!(
            "derive.explorer.endpoint {endpoint} is not a tonic endpoint: {source}"
        ))
    })?;
    let probe_interval_ms = config.probe_interval_ms.unwrap_or_else(|| {
        u64::try_from(zinder_query::DEFAULT_DERIVE_PROBE_INTERVAL.as_millis()).unwrap_or(u64::MAX)
    });
    if probe_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "derive.explorer.probe_interval_ms must be greater than zero",
        )
        .into());
    }
    let token_path = config.token_path;
    let bearer_token = token_path
        .as_deref()
        .map(BearerToken::from_file)
        .transpose()?;

    Ok(Some(ExplorerDeriveConfig {
        endpoint,
        token_path,
        bearer_token,
        probe_interval: Duration::from_millis(probe_interval_ms),
    }))
}

fn resolve_retention_advertisement(storage: &StorageSection) -> RetentionAdvertisementSeconds {
    let chain_event_retention_hours = storage
        .chain_event_retention_hours
        .unwrap_or(DEFAULT_CHAIN_EVENT_RETENTION_HOURS);
    let mempool_mined_retention_minutes = storage
        .mempool_mined_retention_minutes
        .unwrap_or(DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES);
    let mempool_invalidated_retention_hours = storage
        .mempool_invalidated_retention_hours
        .unwrap_or(DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS);

    RetentionAdvertisementSeconds {
        chain_event: chain_event_retention_hours.saturating_mul(3_600),
        mempool_mined: mempool_mined_retention_minutes.saturating_mul(60),
        mempool_invalidated: mempool_invalidated_retention_hours.saturating_mul(3_600),
    }
}

#[derive(Serialize)]
struct QueryConfigToml {
    network: NetworkToml,
    storage: StorageToml,
    query: QueryToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    derive: Option<QueryDeriveToml>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeToml>,
}

impl QueryConfigToml {
    fn from_query_config(config: &QueryConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            storage: StorageToml {
                path: config.storage_path.display().to_string(),
                secondary_path: config.secondary_path.display().to_string(),
                secondary_catchup_interval_ms: duration_as_millis_u64(
                    config.secondary_catchup_interval,
                ),
                secondary_replica_lag_threshold_chain_epochs: config
                    .secondary_replica_lag_threshold_chain_epochs,
                ingest_control_addr: config.ingest_control_addr.clone(),
                ingest_control_token_path: config
                    .ingest_control_token_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                chain_event_retention_hours: config.chain_event_retention_seconds / 3_600,
                mempool_mined_retention_minutes: config.mempool_mined_retention_seconds / 60,
                mempool_invalidated_retention_hours: config.mempool_invalidated_retention_seconds
                    / 3_600,
            },
            query: QueryToml {
                listen_addr: config.listen_addr.to_string(),
                grpc: QueryGrpcToml {
                    enable_reflection: config.grpc.enable_reflection,
                    enable_health: config.grpc.enable_health,
                },
            },
            derive: config
                .explorer_derive
                .as_ref()
                .map(QueryDeriveToml::from_explorer_config),
            node: config.broadcaster.as_ref().map(NodeToml::from_node_target),
        }
    }
}

#[derive(Serialize)]
struct StorageToml {
    path: String,
    secondary_path: String,
    secondary_catchup_interval_ms: u64,
    secondary_replica_lag_threshold_chain_epochs: u64,
    ingest_control_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ingest_control_token_path: Option<String>,
    chain_event_retention_hours: u64,
    mempool_mined_retention_minutes: u64,
    mempool_invalidated_retention_hours: u64,
}

#[derive(Serialize)]
struct QueryToml {
    listen_addr: String,
    grpc: QueryGrpcToml,
}

#[derive(Serialize)]
struct QueryGrpcToml {
    enable_reflection: bool,
    enable_health: bool,
}

#[derive(Serialize)]
struct QueryDeriveToml {
    explorer: QueryDeriveExplorerToml,
}

impl QueryDeriveToml {
    fn from_explorer_config(config: &ExplorerDeriveConfig) -> Self {
        Self {
            explorer: QueryDeriveExplorerToml {
                endpoint: config.endpoint.clone(),
                token_path: config
                    .token_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                probe_interval_ms: duration_as_millis_u64(config.probe_interval),
            },
        }
    }
}

#[derive(Serialize)]
struct QueryDeriveExplorerToml {
    endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_path: Option<String>,
    probe_interval_ms: u64,
}
