//! Configuration loading for the `zinder-compat-lightwalletd` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, CanonicalSecondaryStorageSection, CanonicalSecondaryStorageToml,
    ConfigError, ConfigLoader, IngestControlReaderToml, IngestControlSection, NetworkSection,
    NetworkToml, NodeToml, OpsSection, OpsToml, ResolvedCanonicalSecondaryStorage,
    ResolvedIngestControlReader, SecuritySection, SecurityToml, ServiceIdentifier,
    guard_optional_serving_bind, guard_serving_bind, parse_socket_addr, require_field,
    resolve_allow_public_bind, resolve_canonical_secondary_storage, resolve_ingest_control_reader,
    resolve_ops_listen_addr,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::StoreError;

/// Resolved lightwalletd compatibility runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct LightwalletdConfig {
    pub(crate) network: Network,
    pub(crate) storage: ResolvedCanonicalSecondaryStorage,
    pub(crate) ingest_control_addr: String,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) allow_public_bind: bool,
    pub(crate) broadcaster: Option<NodeTarget>,
}

/// Command-line overrides for the lightwalletd compat command.
#[derive(Debug, Default)]
pub(crate) struct LightwalletdConfigOverrides {
    pub(crate) network: Option<String>,
    pub(crate) storage_path: Option<PathBuf>,
    pub(crate) secondary_path: Option<PathBuf>,
    pub(crate) ingest_control_addr: Option<String>,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
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

    #[error("gRPC reflection service build failed: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),
}

/// Loads and validates lightwalletd compat configuration.
pub(crate) fn load_lightwalletd_config(
    config_path: Option<PathBuf>,
    overrides: LightwalletdConfigOverrides,
) -> Result<LightwalletdConfig, LightwalletdConfigError> {
    let raw_config: LightwalletdRawConfig = ConfigLoader::new()
        .with_default("compat.listen_addr", "127.0.0.1:9067")?
        .with_ops_section(ServiceIdentifier::CompatLightwalletd)?
        .with_security_section()?
        .with_file(config_path)
        .with_zinder_env()?
        .with_override_if("network.name", overrides.network)?
        .with_override_path_if("storage.path", overrides.storage_path)?
        .with_override_path_if("storage.secondary_path", overrides.secondary_path)?
        .with_override_if("ingest_control.addr", overrides.ingest_control_addr)?
        .with_override_path_if(
            "ingest_control.bearer_token_path",
            overrides.ingest_control_bearer_token_path,
        )?
        .with_override_if(
            "compat.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
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
    ops: OpsSection,
    storage: CanonicalSecondaryStorageSection,
    ingest_control: IngestControlSection,
    compat: CompatSection,
    node: NodeSection,
    security: SecuritySection,
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
    let storage = resolve_canonical_secondary_storage(config.storage)?;
    let ResolvedIngestControlReader {
        addr: ingest_control_addr,
        bearer_token_path: ingest_control_bearer_token_path,
        bearer_token: ingest_control_bearer_token,
    } = resolve_ingest_control_reader(config.ingest_control)?;
    let listen_addr_string = require_field(config.compat.listen_addr, "compat.listen_addr")?;
    let listen_addr = parse_socket_addr("compat.listen_addr", &listen_addr_string)?;
    let ops_listen_addr = resolve_ops_listen_addr(config.ops)?;
    let allow_public_bind = resolve_allow_public_bind(config.security)?;
    guard_serving_bind("compat.listen_addr", listen_addr, allow_public_bind)?;
    guard_optional_serving_bind("ops.listen_addr", ops_listen_addr, allow_public_bind)?;
    let broadcaster =
        NodeTarget::resolve_optional(network, config.node).map_err(ConfigError::from)?;

    Ok(LightwalletdConfig {
        network,
        storage,
        ingest_control_addr,
        ingest_control_bearer_token_path,
        ingest_control_bearer_token,
        listen_addr,
        ops_listen_addr,
        allow_public_bind,
        broadcaster,
    })
}

#[derive(Serialize)]
struct LightwalletdConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    security: SecurityToml,
    storage: CanonicalSecondaryStorageToml,
    ingest_control: IngestControlReaderToml,
    compat: CompatToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeToml>,
}

impl LightwalletdConfigToml {
    fn from_lightwalletd_config(config: &LightwalletdConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            security: SecurityToml::from_resolved(config.allow_public_bind),
            storage: CanonicalSecondaryStorageToml::from_resolved(&config.storage),
            ingest_control: IngestControlReaderToml::from_resolved(
                config.ingest_control_addr.clone(),
                config.ingest_control_bearer_token_path.as_deref(),
            ),
            compat: CompatToml {
                listen_addr: config.listen_addr.to_string(),
            },
            node: config.broadcaster.as_ref().map(NodeToml::from_node_target),
        }
    }
}

#[derive(Serialize)]
struct CompatToml {
    listen_addr: String,
}
