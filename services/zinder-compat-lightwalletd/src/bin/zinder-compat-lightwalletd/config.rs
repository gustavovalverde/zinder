//! Configuration loading for the `zinder-compat-lightwalletd` binary.

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

/// Resolved lightwalletd compatibility runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct LightwalletdConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) secondary_path: PathBuf,
    pub(crate) secondary_catchup_interval: Duration,
    pub(crate) secondary_replica_lag_threshold_chain_epochs: u64,
    pub(crate) ingest_control_addr: String,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) broadcaster: Option<NodeTarget>,
}

/// Command-line overrides for the lightwalletd compat command.
#[derive(Debug, Default)]
pub(crate) struct LightwalletdConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) ingest_control_addr: Option<String>,
    pub(crate) ingest_control_token_path: Option<PathBuf>,
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

    #[error("invalid ingest-control bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
}

/// Loads and validates lightwalletd compat configuration.
pub(crate) fn load_lightwalletd_config(
    config_path: Option<PathBuf>,
    overrides: LightwalletdConfigOverrides,
) -> Result<LightwalletdConfig, LightwalletdConfigError> {
    let raw_config: LightwalletdRawConfig = ConfigLoader::new()
        .with_default("compat.listen_addr", "127.0.0.1:9067")?
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
            "compat.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if("node.json_rpc_addr", overrides.node_json_rpc_addr)?
        .load()?;

    resolve_lightwalletd_config(raw_config)
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
struct StorageSection {
    path: Option<PathBuf>,
    secondary_path: Option<PathBuf>,
    secondary_catchup_interval_ms: Option<u64>,
    secondary_replica_lag_threshold_chain_epochs: Option<u64>,
    ingest_control_addr: Option<String>,
    ingest_control_token_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CompatSection {
    listen_addr: Option<String>,
}

fn resolve_lightwalletd_config(
    config: LightwalletdRawConfig,
) -> Result<LightwalletdConfig, LightwalletdConfigError> {
    let network = config.network.resolve()?;
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
    let listen_addr_string = require_field(config.compat.listen_addr, "compat.listen_addr")?;
    let listen_addr = listen_addr_string.parse::<SocketAddr>().map_err(|source| {
        ConfigError::invalid(format!(
            "compat.listen_addr {listen_addr_string} is not a socket address: {source}"
        ))
    })?;
    let broadcaster =
        NodeTarget::resolve_optional(network, config.node).map_err(ConfigError::from)?;

    Ok(LightwalletdConfig {
        network,
        storage_path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(secondary_catchup_interval_ms),
        secondary_replica_lag_threshold_chain_epochs,
        ingest_control_addr,
        ingest_control_token_path,
        ingest_control_bearer_token,
        listen_addr,
        broadcaster,
    })
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
            },
            compat: CompatToml {
                listen_addr: config.listen_addr.to_string(),
            },
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
}

#[derive(Serialize)]
struct CompatToml {
    listen_addr: String,
}
