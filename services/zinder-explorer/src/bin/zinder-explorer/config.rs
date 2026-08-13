//! Configuration loading for the `zinder-explorer` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, InvalidZinderGrpcEndpoint,
    NetworkSection, NetworkToml, OpsSection, OpsServerError, OpsToml, ResolvedSecondaryStorage,
    RuntimeService, SecondaryStorageSection, SecondaryStorageToml, SecuritySection, SecurityToml,
    guard_optional_serving_bind, guard_serving_bind, load_bearer_token, parse_socket_addr,
    require_field, resolve_allow_public_bind, resolve_ops_listen_addr, resolve_secondary_storage,
    validate_zinder_grpc_endpoint,
};
use zinder_source::{NodeSection, NodeTarget};

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9068";

/// Resolved explorer-plane runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerConfig {
    pub(crate) network: Network,
    pub(crate) storage: ResolvedSecondaryStorage,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) allow_public_bind: bool,
    pub(crate) bearer_token_path: Option<PathBuf>,
    pub(crate) bearer_token: Option<BearerToken>,
    /// Admitted Wallet-query endpoint that anchors the Explorer construction.
    pub(crate) wallet_query_endpoint: String,
    /// Optional bearer token sent to the admitted Wallet-query endpoint.
    pub(crate) wallet_query_bearer_token_path: Option<PathBuf>,
    /// Loaded optional bearer token sent to the admitted Wallet-query endpoint.
    pub(crate) wallet_query_bearer_token: Option<BearerToken>,
    /// Resolved upstream node target used only for freshness observation.
    /// `None` leaves `ExplorerFreshness.chain_view.upstream_tip` unset.
    pub(crate) node: Option<NodeTarget>,
}

/// Command-line overrides applied on top of the layered configuration.
#[derive(Debug, Default)]
pub(crate) struct ExplorerConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) bearer_token_path: Option<PathBuf>,
    pub(crate) wallet_query_endpoint: Option<String>,
    pub(crate) wallet_query_bearer_token_path: Option<PathBuf>,
}

/// Error returned while resolving explorer configuration or running the binary.
#[derive(Debug, Error)]
pub(crate) enum ExplorerConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    OpsServer(#[from] OpsServerError),

    #[error(transparent)]
    Store(#[from] zinder_explorer::MaterializedViewStoreError),

    #[error("explorer requires an initialized materialized-view store")]
    RequiredMaterializedViewStore,

    #[error("explorer runtime failed: {0}")]
    Runtime(#[from] zinder_explorer::MaterializedViewError),

    #[error("materialized-view catch-up task failed to join: {0}")]
    MaterializedViewCatchupTask(#[source] tokio::task::JoinError),

    #[error("gRPC transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC reflection initialization failed: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),

    #[error("invalid explorer bearer token: {0}")]
    BearerToken(#[from] BearerTokenError),

    #[error("invalid wallet-query bearer token: {0}")]
    WalletQueryBearerToken(#[source] BearerTokenError),

    #[error(transparent)]
    InvalidWalletQueryEndpoint(#[from] InvalidZinderGrpcEndpoint),

    #[error(transparent)]
    EndpointAdmission(#[from] zinder_explorer::ExplorerEndpointAdmissionError),

    #[error("invalid [node] configuration: {0}")]
    Node(#[from] zinder_source::NodeConfigError),

    #[error("failed to build upstream node source: {0}")]
    NodeSource(#[from] zinder_source::SourceError),
}

/// Loads and validates explorer configuration from defaults, file, environment, and CLI overrides.
pub(crate) fn load_explorer_config(
    config_path: Option<PathBuf>,
    overrides: ExplorerConfigOverrides,
) -> Result<ExplorerConfig, ExplorerConfigError> {
    let raw: ExplorerRawConfig = ConfigLoader::new()
        // Storage defaults match the canonical Zinder layout. The explorer
        // locates the materialized-view primary beneath `storage.path` and
        // keeps its process-owned secondary metadata beneath
        // `storage.secondary_path`; it does not open the canonical primary.
        // Operators override via env vars (`ZINDER_STORAGE__PATH`,
        // `ZINDER_STORAGE__SECONDARY_PATH`) or CLI flags.
        .with_default("storage.path", "/var/lib/zinder/store")?
        .with_default(
            "storage.secondary_path",
            "/var/lib/zinder/explorer-secondary",
        )?
        .with_default("explorer.listen_addr", DEFAULT_LISTEN_ADDR)?
        .with_ops_section(RuntimeService::Explorer)?
        .with_security_section()?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_path_if("storage.secondary_path", overrides.secondary_path)?
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
        .with_override_path_if(
            "explorer.wallet_query_bearer_token_path",
            overrides.wallet_query_bearer_token_path,
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
    storage: SecondaryStorageSection,
    explorer: ExplorerSection,
    ops: OpsSection,
    node: NodeSection,
    security: SecuritySection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerSection {
    listen_addr: Option<String>,
    bearer_token_path: Option<PathBuf>,
    wallet_query_endpoint: Option<String>,
    wallet_query_bearer_token_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ExplorerConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    security: SecurityToml,
    storage: SecondaryStorageToml,
    explorer: ExplorerToml,
}

#[derive(Debug, Serialize)]
struct ExplorerToml {
    listen_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer_token_path: Option<String>,
    wallet_query_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet_query_bearer_token_path: Option<String>,
}

impl ExplorerConfigToml {
    fn from_resolved(config: &ExplorerConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            security: SecurityToml::from_resolved(config.allow_public_bind),
            storage: SecondaryStorageToml::from_resolved(&config.storage),
            explorer: ExplorerToml {
                listen_addr: config.listen_addr.to_string(),
                bearer_token_path: config
                    .bearer_token_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                wallet_query_endpoint: config.wallet_query_endpoint.clone(),
                wallet_query_bearer_token_path: config
                    .wallet_query_bearer_token_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
            },
        }
    }
}

fn resolve_explorer_config(raw: ExplorerRawConfig) -> Result<ExplorerConfig, ExplorerConfigError> {
    let network = raw.network.resolve()?;
    let storage = resolve_secondary_storage(raw.storage)?;
    let listen_addr_text = require_field(raw.explorer.listen_addr, "explorer.listen_addr")?;
    let listen_addr = parse_socket_addr("explorer.listen_addr", &listen_addr_text)?;
    let bearer_token_path = raw.explorer.bearer_token_path;
    let bearer_token = load_bearer_token(bearer_token_path.as_deref())?;
    let ops_listen_addr = resolve_ops_listen_addr(raw.ops)?;
    let allow_public_bind = resolve_allow_public_bind(raw.security)?;
    guard_serving_bind("explorer.listen_addr", listen_addr, allow_public_bind)?;
    guard_optional_serving_bind("ops.listen_addr", ops_listen_addr, allow_public_bind)?;
    let wallet_query_endpoint = require_field(
        raw.explorer
            .wallet_query_endpoint
            .filter(|endpoint| !endpoint.trim().is_empty()),
        "explorer.wallet_query_endpoint",
    )?;
    validate_zinder_grpc_endpoint(&wallet_query_endpoint)?;
    let wallet_query_bearer_token_path = raw.explorer.wallet_query_bearer_token_path;
    let wallet_query_bearer_token = wallet_query_bearer_token_path
        .as_deref()
        .map(BearerToken::from_file)
        .transpose()
        .map_err(ExplorerConfigError::WalletQueryBearerToken)?;
    let node = NodeTarget::resolve_optional(network, raw.node)?;
    Ok(ExplorerConfig {
        network,
        storage,
        listen_addr,
        ops_listen_addr,
        allow_public_bind,
        bearer_token_path,
        bearer_token,
        wallet_query_endpoint,
        wallet_query_bearer_token_path,
        wallet_query_bearer_token,
        node,
    })
}
