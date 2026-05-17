//! Configuration loading for the `zinder-explorer` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, NetworkSection, NetworkToml,
    OpsSection, OpsToml, ServiceIdentifier, load_bearer_token, parse_socket_addr, require_field,
    resolve_ops_listen_addr,
};

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9068";

/// Resolved explorer-plane runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerConfig {
    pub(crate) network: Network,
    pub(crate) storage_path: PathBuf,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) bearer_token_path: Option<PathBuf>,
    pub(crate) bearer_token: Option<BearerToken>,
    /// Wallet-query endpoint that backs the Shape C balance read path.
    /// Empty string means the federated balance method is unavailable; the
    /// `explorer.transparent_address.balance_v1` capability is omitted.
    pub(crate) wallet_query_endpoint: Option<String>,
}

/// Command-line overrides applied on top of the layered configuration.
#[derive(Debug, Default)]
pub(crate) struct ExplorerConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) bearer_token_path: Option<PathBuf>,
    pub(crate) wallet_query_endpoint: Option<String>,
}

/// Error returned while resolving explorer configuration or running the binary.
#[derive(Debug, Error)]
pub(crate) enum ExplorerConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Store(#[from] zinder_explorer::DeriveStoreError),

    #[error("explorer runtime failed: {0}")]
    Runtime(#[from] zinder_explorer::DeriveError),

    #[error("gRPC transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC reflection initialization failed: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),

    #[error("invalid explorer bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),
}

/// Loads and validates explorer configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_explorer_config(
    config_path: Option<PathBuf>,
    overrides: ExplorerConfigOverrides,
) -> Result<ExplorerConfig, ExplorerConfigError> {
    let raw: ExplorerRawConfig = ConfigLoader::new()
        .with_default("explorer.listen_addr", DEFAULT_LISTEN_ADDR)?
        .with_ops_section(ServiceIdentifier::Explorer)?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("explorer.storage_path", overrides.storage_path)?
        .with_override_if(
            "explorer.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_path_if("explorer.bearer_token_path", overrides.bearer_token_path)?
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if(
            "explorer.wallet_query_endpoint",
            overrides.wallet_query_endpoint,
        )?
        .load()?;
    resolve_explorer_config(raw)
}

/// Renders a resolved explorer configuration as TOML for `--print-config`.
pub(crate) fn explorer_config_toml(config: &ExplorerConfig) -> Result<String, ExplorerConfigError> {
    let toml_text = toml::to_string(&ExplorerConfigToml::from_resolved(config))
        .map_err(|source| ConfigError::Render { source })?;
    Ok(toml_text)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerRawConfig {
    network: NetworkSection,
    explorer: ExplorerSection,
    ops: OpsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerSection {
    listen_addr: Option<String>,
    storage_path: Option<String>,
    bearer_token_path: Option<PathBuf>,
    wallet_query_endpoint: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExplorerConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    explorer: ExplorerToml,
}

#[derive(Debug, Serialize)]
struct ExplorerToml {
    listen_addr: String,
    storage_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer_token_path: Option<String>,
    wallet_query_endpoint: String,
}

impl ExplorerConfigToml {
    fn from_resolved(config: &ExplorerConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            explorer: ExplorerToml {
                listen_addr: config.listen_addr.to_string(),
                storage_path: config.storage_path.to_string_lossy().into_owned(),
                bearer_token_path: config
                    .bearer_token_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                wallet_query_endpoint: config.wallet_query_endpoint.clone().unwrap_or_default(),
            },
        }
    }
}

fn resolve_explorer_config(raw: ExplorerRawConfig) -> Result<ExplorerConfig, ExplorerConfigError> {
    let network = raw.network.resolve()?;
    let listen_addr_text = require_field(raw.explorer.listen_addr, "explorer.listen_addr")?;
    let listen_addr = parse_socket_addr("explorer.listen_addr", &listen_addr_text)?;
    let storage_path_text = require_field(raw.explorer.storage_path, "explorer.storage_path")?;
    let storage_path = PathBuf::from(storage_path_text);
    let bearer_token_path = raw.explorer.bearer_token_path;
    let bearer_token = load_bearer_token(bearer_token_path.as_deref())?;
    let ops_listen_addr = resolve_ops_listen_addr(raw.ops)?;
    let wallet_query_endpoint = raw
        .explorer
        .wallet_query_endpoint
        .filter(|endpoint| !endpoint.is_empty());
    Ok(ExplorerConfig {
        network,
        storage_path,
        listen_addr,
        ops_listen_addr,
        bearer_token_path,
        bearer_token,
        wallet_query_endpoint,
    })
}
