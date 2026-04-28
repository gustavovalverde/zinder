//! Configuration loading for the `zinder-compat-lightwalletd` binary.

use std::{net::SocketAddr, num::NonZeroU64, path::PathBuf, time::Duration};

use ::config::{Config, File, FileFormat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    ConfigError, path_to_config_string, require_string, zinder_environment_source,
};
use zinder_source::{DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth};
use zinder_store::StoreError;

const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESPONSE_BYTES: u64 = DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES.get();
const DEFAULT_INGEST_CONTROL_ADDR: &str = "http://127.0.0.1:9100";

/// Resolved lightwalletd compatibility runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct LightwalletdConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) secondary_path: PathBuf,
    pub(crate) secondary_catchup_interval: Duration,
    pub(crate) secondary_replica_lag_threshold_chain_epochs: u64,
    pub(crate) ingest_control_addr: String,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) broadcaster: Option<BroadcasterConfig>,
}

/// Resolved node-broadcaster runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct BroadcasterConfig {
    pub(crate) json_rpc_addr: String,
    pub(crate) auth: NodeAuth,
    pub(crate) request_timeout: Duration,
    pub(crate) max_response_bytes: NonZeroU64,
}

/// Command-line overrides for the lightwalletd compat command.
#[derive(Debug, Default)]
pub(crate) struct LightwalletdConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) ingest_control_addr: Option<String>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) node_json_rpc_addr: Option<String>,
}

/// Error returned while resolving lightwalletd compat config or running its gRPC server.
#[derive(Debug, Error)]
pub(crate) enum LightwalletdConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("node source initialization failed: {0}")]
    Source(Box<zinder_source::SourceError>),

    #[error("gRPC transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// Loads and validates lightwalletd compat configuration.
pub(crate) fn load_lightwalletd_config(
    config_path: Option<PathBuf>,
    overrides: LightwalletdConfigOverrides,
) -> Result<LightwalletdConfig, LightwalletdConfigError> {
    let mut builder = Config::builder()
        .set_default("compat.listen_addr", "127.0.0.1:9067")
        .map_err(ConfigError::load)?
        .set_default("node.request_timeout_secs", DEFAULT_REQUEST_TIMEOUT_SECS)
        .map_err(ConfigError::load)?
        .set_default("node.max_response_bytes", DEFAULT_MAX_RESPONSE_BYTES)
        .map_err(ConfigError::load)?;

    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }

    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_overrides(builder, overrides)?;

    let raw_config: LightwalletdRawConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;

    Ok(resolve_lightwalletd_config(raw_config)?)
}

/// Renders the effective lightwalletd compat configuration in the accepted TOML shape.
pub(crate) fn lightwalletd_config_toml(
    config: &LightwalletdConfig,
) -> Result<String, LightwalletdConfigError> {
    let rendered = toml::to_string(&LightwalletdConfigToml::from_lightwalletd_config(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(rendered)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LightwalletdRawConfig {
    network: NetworkSection,
    storage: StorageSection,
    compat: CompatSection,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CompatSection {
    listen_addr: Option<String>,
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

fn apply_overrides(
    mut builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: LightwalletdConfigOverrides,
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
    if let Some(listen_addr) = overrides.listen_addr {
        builder = builder
            .set_override("compat.listen_addr", listen_addr.to_string())
            .map_err(ConfigError::load)?;
    }
    if let Some(json_rpc_addr) = overrides.node_json_rpc_addr {
        builder = builder
            .set_override("node.json_rpc_addr", json_rpc_addr)
            .map_err(ConfigError::load)?;
    }

    Ok(builder)
}

fn resolve_lightwalletd_config(
    config: LightwalletdRawConfig,
) -> Result<LightwalletdConfig, ConfigError> {
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
    let listen_addr_string = require_string(config.compat.listen_addr, "compat.listen_addr")?;
    let network = Network::from_name(&network_name)
        .ok_or_else(|| ConfigError::invalid(format!("unknown network: {network_name}")))?;
    let listen_addr = listen_addr_string.parse::<SocketAddr>().map_err(|source| {
        ConfigError::invalid(format!(
            "compat.listen_addr {listen_addr_string} is not a socket address: {source}"
        ))
    })?;
    let broadcaster = resolve_broadcaster_config(config.node)?;

    Ok(LightwalletdConfig {
        network,
        storage_path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(secondary_catchup_interval_ms),
        secondary_replica_lag_threshold_chain_epochs,
        ingest_control_addr,
        listen_addr,
        broadcaster,
    })
}

fn resolve_broadcaster_config(
    section: NodeSection,
) -> Result<Option<BroadcasterConfig>, ConfigError> {
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
    let auth = resolve_node_auth(section.auth)?;

    Ok(Some(BroadcasterConfig {
        json_rpc_addr,
        auth,
        request_timeout,
        max_response_bytes,
    }))
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
struct LightwalletdConfigToml {
    network: NetworkToml,
    storage: StorageToml,
    compat: CompatToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeToml>,
}

impl LightwalletdConfigToml {
    fn from_lightwalletd_config(config: &LightwalletdConfig) -> Self {
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
            },
            compat: CompatToml {
                listen_addr: config.listen_addr.to_string(),
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
}

#[derive(Serialize)]
struct CompatToml {
    listen_addr: String,
}

#[derive(Serialize)]
struct NodeToml {
    json_rpc_addr: String,
    request_timeout_secs: u64,
    max_response_bytes: u64,
    auth: NodeAuthToml,
}

impl From<&BroadcasterConfig> for NodeToml {
    fn from(config: &BroadcasterConfig) -> Self {
        Self {
            json_rpc_addr: config.json_rpc_addr.clone(),
            request_timeout_secs: config.request_timeout.as_secs(),
            max_response_bytes: config.max_response_bytes.get(),
            auth: NodeAuthToml::from(&config.auth),
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
