//! Configuration loading for the `zinder-query` binary.

use std::{net::SocketAddr, num::NonZeroU64, path::PathBuf, time::Duration};

use ::config::{Config, File, FileFormat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    ConfigError, path_to_config_string, require_string, zinder_environment_source,
};
use zinder_source::{DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeTarget};
use zinder_store::StoreError;

const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES.get();
const DEFAULT_INGEST_CONTROL_ADDR: &str = "http://127.0.0.1:9100";

/// Resolved query runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct QueryConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) secondary_path: PathBuf,
    pub(crate) secondary_catchup_interval: Duration,
    pub(crate) secondary_replica_lag_threshold_chain_epochs: u64,
    pub(crate) ingest_control_addr: String,
    pub(crate) chain_event_retention_seconds: u64,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) grpc: QueryGrpcConfig,
    /// Optional node broadcaster. Network must match `QueryConfig.network`
    /// when present; the resolver enforces this.
    pub(crate) broadcaster: Option<NodeTarget>,
}

/// Resolved gRPC runtime options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QueryGrpcConfig {
    pub(crate) enable_reflection: bool,
    pub(crate) enable_health: bool,
}

/// Command-line overrides for the query command.
#[derive(Debug, Default)]
pub(crate) struct QueryConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) ingest_control_addr: Option<String>,
    pub(crate) chain_event_retention_hours: Option<u64>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) node_json_rpc_addr: Option<String>,
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
}

/// Loads and validates query configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_query_config(
    config_path: Option<PathBuf>,
    overrides: QueryConfigOverrides,
) -> Result<QueryConfig, QueryConfigError> {
    let mut builder = Config::builder()
        .set_default("query.listen_addr", "127.0.0.1:9101")
        .map_err(ConfigError::load)?
        .set_default("query.grpc.enable_reflection", true)
        .map_err(ConfigError::load)?
        .set_default("query.grpc.enable_health", true)
        .map_err(ConfigError::load)?
        .set_default("node.request_timeout_secs", DEFAULT_REQUEST_TIMEOUT_SECS)
        .map_err(ConfigError::load)?
        .set_default("node.max_response_bytes", DEFAULT_MAX_RESPONSE_BYTES)
        .map_err(ConfigError::load)?
        .set_default("storage.chain_event_retention_hours", 168_u64)
        .map_err(ConfigError::load)?;

    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }

    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_query_overrides(builder, overrides)?;

    let raw_config: QueryRawConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;

    Ok(resolve_query_config(raw_config)?)
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NetworkSection {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageSection {
    path: Option<PathBuf>,
    secondary_path: Option<PathBuf>,
    secondary_catchup_interval_ms: Option<u64>,
    secondary_replica_lag_threshold_chain_epochs: Option<u64>,
    ingest_control_addr: Option<String>,
    chain_event_retention_hours: Option<u64>,
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
struct NodeSection {
    json_rpc_addr: Option<String>,
    request_timeout_secs: Option<u64>,
    max_response_bytes: Option<u64>,
    auth: NodeAuthSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NodeAuthSection {
    method: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

fn apply_query_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: QueryConfigOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, ConfigError> {
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
    if let Some(secondary_path) = overrides.secondary_path {
        builder = builder
            .set_override(
                "storage.secondary_path",
                path_to_config_string(secondary_path, "storage.secondary_path")?,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(ingest_control_addr) = overrides.ingest_control_addr {
        builder = builder
            .set_override("storage.ingest_control_addr", ingest_control_addr)
            .map_err(ConfigError::load)?;
    }
    if let Some(chain_event_retention_hours) = overrides.chain_event_retention_hours {
        builder = builder
            .set_override(
                "storage.chain_event_retention_hours",
                chain_event_retention_hours,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(listen_addr) = overrides.listen_addr {
        builder = builder
            .set_override("query.listen_addr", listen_addr.to_string())
            .map_err(ConfigError::load)?;
    }
    if let Some(json_rpc_addr) = overrides.node_json_rpc_addr {
        builder = builder
            .set_override("node.json_rpc_addr", json_rpc_addr)
            .map_err(ConfigError::load)?;
    }

    Ok(builder)
}

fn resolve_query_config(config: QueryRawConfig) -> Result<QueryConfig, ConfigError> {
    let network_name = require_string(config.network.name, "network.name")?;
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
        ));
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
    let chain_event_retention_hours = config.storage.chain_event_retention_hours.unwrap_or(168);
    let chain_event_retention_seconds = chain_event_retention_hours.saturating_mul(3_600);
    let listen_addr_string = require_string(config.query.listen_addr, "query.listen_addr")?;
    let network = Network::from_name(&network_name)
        .ok_or_else(|| ConfigError::invalid(format!("unknown network: {network_name}")))?;
    let listen_addr = listen_addr_string.parse::<SocketAddr>().map_err(|source| {
        ConfigError::invalid(format!(
            "query.listen_addr {listen_addr_string} is not a socket address: {source}"
        ))
    })?;
    let enable_reflection = config
        .query
        .grpc
        .enable_reflection
        .ok_or_else(|| ConfigError::missing_field("query.grpc.enable_reflection"))?;
    let enable_health = config
        .query
        .grpc
        .enable_health
        .ok_or_else(|| ConfigError::missing_field("query.grpc.enable_health"))?;
    let broadcaster = resolve_broadcaster_config(network, config.node)?;

    Ok(QueryConfig {
        network,
        storage_path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(secondary_catchup_interval_ms),
        secondary_replica_lag_threshold_chain_epochs,
        ingest_control_addr,
        chain_event_retention_seconds,
        listen_addr,
        grpc: QueryGrpcConfig {
            enable_reflection,
            enable_health,
        },
        broadcaster,
    })
}

fn resolve_broadcaster_config(
    network: Network,
    section: NodeSection,
) -> Result<Option<NodeTarget>, ConfigError> {
    let Some(json_rpc_addr) = section.json_rpc_addr else {
        return Ok(None);
    };

    let request_timeout = section.request_timeout_secs.map_or_else(
        || Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        Duration::from_secs,
    );
    let max_response_bytes = section
        .max_response_bytes
        .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES);
    let max_response_bytes = NonZeroU64::new(max_response_bytes)
        .ok_or_else(|| ConfigError::invalid("node.max_response_bytes must be greater than zero"))?;
    let node_auth = resolve_node_auth(section.auth)?;

    Ok(Some(NodeTarget::new(
        network,
        json_rpc_addr,
        node_auth,
        request_timeout,
        max_response_bytes,
    )))
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "configuration durations are rejected by operators long before u64 millis overflow"
)]
fn millis_u64(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

fn resolve_node_auth(auth: NodeAuthSection) -> Result<NodeAuth, ConfigError> {
    let method = auth.method.unwrap_or_else(|| "none".to_string());
    match method.as_str() {
        "none" => Ok(NodeAuth::None),
        "basic" => {
            let username = require_string(auth.username, "node.auth.username")?;
            let password = require_string(auth.password, "node.auth.password")?;
            Ok(NodeAuth::basic(username, password))
        }
        other => Err(ConfigError::invalid(format!(
            "node.auth.method {other} must be one of: none, basic"
        ))),
    }
}

#[derive(Serialize)]
struct QueryConfigToml {
    network: NetworkToml,
    storage: StorageToml,
    query: QueryToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeToml>,
}

impl QueryConfigToml {
    fn from_query_config(config: &QueryConfig) -> Self {
        Self {
            network: NetworkToml {
                name: config.network.name(),
            },
            storage: StorageToml {
                path: config.storage_path.display().to_string(),
                secondary_path: config.secondary_path.display().to_string(),
                secondary_catchup_interval_ms: millis_u64(config.secondary_catchup_interval),
                secondary_replica_lag_threshold_chain_epochs: config
                    .secondary_replica_lag_threshold_chain_epochs,
                ingest_control_addr: config.ingest_control_addr.clone(),
                chain_event_retention_hours: config.chain_event_retention_seconds / 3_600,
            },
            query: QueryToml {
                listen_addr: config.listen_addr.to_string(),
                grpc: QueryGrpcToml {
                    enable_reflection: config.grpc.enable_reflection,
                    enable_health: config.grpc.enable_health,
                },
            },
            node: config.broadcaster.as_ref().map(NodeToml::from),
        }
    }
}

#[derive(Serialize)]
struct NetworkToml {
    name: &'static str,
}

#[derive(Serialize)]
struct StorageToml {
    path: String,
    secondary_path: String,
    secondary_catchup_interval_ms: u64,
    secondary_replica_lag_threshold_chain_epochs: u64,
    ingest_control_addr: String,
    chain_event_retention_hours: u64,
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
struct NodeToml {
    json_rpc_addr: String,
    request_timeout_secs: u64,
    max_response_bytes: u64,
    auth: NodeAuthToml,
}

impl From<&NodeTarget> for NodeToml {
    fn from(target: &NodeTarget) -> Self {
        Self {
            json_rpc_addr: target.json_rpc_addr.clone(),
            request_timeout_secs: target.request_timeout.as_secs(),
            max_response_bytes: target.max_response_bytes.get(),
            auth: NodeAuthToml::from(&target.node_auth),
        }
    }
}

#[derive(Serialize)]
struct NodeAuthToml {
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'static str>,
}

impl From<&NodeAuth> for NodeAuthToml {
    fn from(auth: &NodeAuth) -> Self {
        match auth {
            NodeAuth::None => Self {
                method: "none",
                username: None,
                password: None,
            },
            NodeAuth::Basic { username, .. } => Self {
                method: "basic",
                username: Some(username.clone()),
                password: Some("[REDACTED]"),
            },
            NodeAuth::Cookie { .. } => Self {
                method: "cookie",
                username: None,
                password: Some("[REDACTED]"),
            },
        }
    }
}
