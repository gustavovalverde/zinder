//! Configuration loading for the `zinder-derive` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, NetworkSection, NetworkToml,
    require_field,
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
    pub(crate) token_path: Option<PathBuf>,
    pub(crate) bearer_token: Option<BearerToken>,
    /// Wallet-query endpoint that backs the Shape C balance read path.
    /// Empty string means the federated balance method is unavailable; the
    /// `derive.explorer.transparent_balance_v1` capability is omitted.
    pub(crate) wallet_query_endpoint: Option<String>,
}

/// Command-line overrides applied on top of the layered configuration.
#[derive(Debug, Default)]
pub(crate) struct DeriveConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) token_path: Option<PathBuf>,
    pub(crate) wallet_query_endpoint: Option<String>,
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

    #[error("invalid derive explorer bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
}

/// Loads and validates derive configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_derive_config(
    config_path: Option<PathBuf>,
    overrides: DeriveConfigOverrides,
) -> Result<DeriveConfig, DeriveConfigError> {
    let raw: DeriveRawConfig = ConfigLoader::new()
        .with_default("derive.explorer.listen_addr", DEFAULT_LISTEN_ADDR)?
        .with_default("ops.listen_addr", DEFAULT_OPS_LISTEN_ADDR)?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("derive.explorer.storage_path", overrides.storage_path)?
        .with_override_if(
            "derive.explorer.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_path_if("derive.explorer.token_path", overrides.token_path)?
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if(
            "derive.explorer.wallet_query_endpoint",
            overrides.wallet_query_endpoint,
        )?
        .load()?;
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
struct DeriveSection {
    explorer: DeriveExplorerSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeriveExplorerSection {
    listen_addr: Option<String>,
    storage_path: Option<String>,
    token_path: Option<PathBuf>,
    wallet_query_endpoint: Option<String>,
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
struct DeriveToml {
    explorer: DeriveExplorerToml,
}

#[derive(Debug, Serialize)]
struct DeriveExplorerToml {
    listen_addr: String,
    storage_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_path: Option<String>,
    wallet_query_endpoint: String,
}

#[derive(Debug, Serialize)]
struct OpsToml {
    listen_addr: String,
}

impl DeriveConfigToml {
    fn from_resolved(config: &DeriveConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            derive: DeriveToml {
                explorer: DeriveExplorerToml {
                    listen_addr: config.listen_addr.to_string(),
                    storage_path: config.storage_path.to_string_lossy().into_owned(),
                    token_path: config
                        .token_path
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    wallet_query_endpoint: config.wallet_query_endpoint.clone().unwrap_or_default(),
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

fn resolve_derive_config(raw: DeriveRawConfig) -> Result<DeriveConfig, DeriveConfigError> {
    let network = raw.network.resolve()?;
    let listen_addr_text = require_field(
        raw.derive.explorer.listen_addr,
        "derive.explorer.listen_addr",
    )?;
    let listen_addr = listen_addr_text
        .parse::<SocketAddr>()
        .map_err(|error| ConfigError::invalid(error.to_string()))?;
    let storage_path_text = require_field(
        raw.derive.explorer.storage_path,
        "derive.explorer.storage_path",
    )?;
    let storage_path = PathBuf::from(storage_path_text);
    let token_path = raw.derive.explorer.token_path;
    let bearer_token = token_path
        .as_deref()
        .map(BearerToken::from_file)
        .transpose()?;
    let ops_listen_addr = match raw.ops.listen_addr.as_deref() {
        Some(text) if !text.is_empty() => Some(
            text.parse::<SocketAddr>()
                .map_err(|error| ConfigError::invalid(error.to_string()))?,
        ),
        _ => None,
    };
    let wallet_query_endpoint = raw
        .derive
        .explorer
        .wallet_query_endpoint
        .filter(|endpoint| !endpoint.is_empty());
    Ok(DeriveConfig {
        network,
        storage_path,
        listen_addr,
        ops_listen_addr,
        token_path,
        bearer_token,
        wallet_query_endpoint,
    })
}
