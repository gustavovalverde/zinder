//! Configuration loading for the `zinder-derive` binary.

use std::{net::SocketAddr, path::PathBuf};

use ::config::{Config, File, FileFormat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    ConfigError, path_to_config_string, require_string, zinder_environment_source,
};

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9068";
const DEFAULT_OPS_LISTEN_ADDR: &str = "127.0.0.1:9069";

/// Resolved derive runtime configuration for the explorer consumer.
#[derive(Clone, Debug)]
pub(crate) struct DeriveConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
}

/// Command-line overrides applied on top of the layered configuration.
#[derive(Debug, Default)]
pub(crate) struct DeriveConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
}

/// Error returned while resolving derive configuration or running the binary.
#[derive(Debug, Error)]
pub(crate) enum DeriveConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Store(#[from] zinder_derive::DeriveStoreError),

    #[error("derive runtime failed: {0}")]
    Runtime(#[from] zinder_derive::DeriveError),

    #[error("gRPC transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC reflection initialization failed: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),
}

/// Loads and validates derive configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_derive_config(
    config_path: Option<PathBuf>,
    overrides: DeriveConfigOverrides,
) -> Result<DeriveConfig, DeriveConfigError> {
    let mut builder = Config::builder()
        .set_default("derive.explorer.listen_addr", DEFAULT_LISTEN_ADDR)
        .map_err(ConfigError::load)?
        .set_default("ops.listen_addr", DEFAULT_OPS_LISTEN_ADDR)
        .map_err(ConfigError::load)?;
    if let Some(path) = config_path {
        builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(true));
    }
    builder = builder.add_source(zinder_environment_source()?);
    builder = apply_derive_overrides(builder, overrides)?;
    let raw: DeriveRawConfig = builder
        .build()
        .map_err(ConfigError::load)?
        .try_deserialize()
        .map_err(ConfigError::load)?;
    resolve_derive_config(raw)
}

/// Renders a resolved derive configuration as TOML for `--print-config`.
pub(crate) fn derive_config_toml(config: &DeriveConfig) -> Result<String, DeriveConfigError> {
    let toml_text = toml::to_string(&DeriveConfigToml::from_resolved(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(toml_text)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeriveRawConfig {
    network: NetworkSection,
    derive: DeriveSection,
    ops: OpsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NetworkSection {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeriveSection {
    explorer: DeriveExplorerSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeriveExplorerSection {
    listen_addr: Option<String>,
    storage_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpsSection {
    listen_addr: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeriveConfigToml {
    network: NetworkToml,
    derive: DeriveToml,
    ops: OpsToml,
}

#[derive(Debug, Serialize)]
struct NetworkToml {
    name: String,
}

#[derive(Debug, Serialize)]
struct DeriveToml {
    explorer: DeriveExplorerToml,
}

#[derive(Debug, Serialize)]
struct DeriveExplorerToml {
    listen_addr: String,
    storage_path: String,
}

#[derive(Debug, Serialize)]
struct OpsToml {
    listen_addr: String,
}

impl DeriveConfigToml {
    fn from_resolved(config: &DeriveConfig) -> Self {
        Self {
            network: NetworkToml {
                name: config.network.name().to_owned(),
            },
            derive: DeriveToml {
                explorer: DeriveExplorerToml {
                    listen_addr: config.listen_addr.to_string(),
                    storage_path: config.storage_path.to_string_lossy().into_owned(),
                },
            },
            ops: OpsToml {
                listen_addr: config
                    .ops_listen_addr
                    .map(|addr| addr.to_string())
                    .unwrap_or_default(),
            },
        }
    }
}

fn apply_derive_overrides(
    builder: ::config::ConfigBuilder<::config::builder::DefaultState>,
    overrides: DeriveConfigOverrides,
) -> Result<::config::ConfigBuilder<::config::builder::DefaultState>, DeriveConfigError> {
    let mut builder = builder;
    if let Some(network) = overrides.network {
        builder = builder
            .set_override("network.name", network)
            .map_err(ConfigError::load)?;
    }
    if let Some(storage_path) = overrides.storage_path {
        builder = builder
            .set_override(
                "derive.explorer.storage_path",
                path_to_config_string(storage_path, "derive.explorer.storage_path")?,
            )
            .map_err(ConfigError::load)?;
    }
    if let Some(listen_addr) = overrides.listen_addr {
        builder = builder
            .set_override("derive.explorer.listen_addr", listen_addr.to_string())
            .map_err(ConfigError::load)?;
    }
    if let Some(ops_listen_addr) = overrides.ops_listen_addr {
        builder = builder
            .set_override("ops.listen_addr", ops_listen_addr.to_string())
            .map_err(ConfigError::load)?;
    }
    Ok(builder)
}

fn resolve_derive_config(raw: DeriveRawConfig) -> Result<DeriveConfig, DeriveConfigError> {
    let network_name = require_string(raw.network.name, "network.name")?;
    let network = Network::from_name(&network_name)
        .ok_or_else(|| ConfigError::invalid(format!("unsupported network name: {network_name}")))?;
    let listen_addr_text = require_string(
        raw.derive.explorer.listen_addr,
        "derive.explorer.listen_addr",
    )?;
    let listen_addr = listen_addr_text
        .parse::<SocketAddr>()
        .map_err(|error| ConfigError::invalid(error.to_string()))?;
    let storage_path_text = require_string(
        raw.derive.explorer.storage_path,
        "derive.explorer.storage_path",
    )?;
    let storage_path = PathBuf::from(storage_path_text);
    let ops_listen_addr = match raw.ops.listen_addr.as_deref() {
        Some(text) if !text.is_empty() => Some(
            text.parse::<SocketAddr>()
                .map_err(|error| ConfigError::invalid(error.to_string()))?,
        ),
        _ => None,
    };
    Ok(DeriveConfig {
        network,
        storage_path,
        listen_addr,
        ops_listen_addr,
    })
}
