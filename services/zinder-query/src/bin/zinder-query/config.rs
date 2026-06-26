//! Configuration loading for the `zinder-query` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_derive::DeriveStoreError;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, IngestControlReaderToml,
    IngestControlSection, NetworkSection, NetworkToml, NodeToml, OpsSection, OpsToml,
    ResolvedIngestControlReader, ResolvedRetention, ResolvedSecondaryStorage, RetentionSection,
    RetentionToml, SecondaryStorageSection, SecondaryStorageToml, SecuritySection, SecurityToml,
    ServiceIdentifier, guard_optional_serving_bind, guard_serving_bind, parse_socket_addr,
    require_field, resolve_allow_public_bind, resolve_ingest_control_reader,
    resolve_ops_listen_addr, resolve_retention, resolve_secondary_storage,
};
use zinder_source::{NodeSection, NodeTarget};
use zinder_store::StoreError;

/// Resolved query runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct QueryConfig {
    pub(crate) network: Network,
    pub(crate) storage: ResolvedSecondaryStorage,
    pub(crate) ingest_control_addr: String,
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) ingest_control_bearer_token: Option<BearerToken>,
    pub(crate) retention: ResolvedRetention,
    pub(crate) listen_addr: SocketAddr,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) allow_public_bind: bool,
    pub(crate) grpc: QueryGrpcConfig,
    /// Whether `TransparentUtxoSetSummary` folds the `LtHash16` UTXO-set
    /// commitment. Operator opt-in; the fold has per-output CPU cost.
    pub(crate) utxo_set_commitment_enabled: bool,
    /// Optional node broadcaster. Network must match `QueryConfig.network`
    /// when present; the resolver enforces this.
    pub(crate) broadcaster: Option<NodeTarget>,
}

impl QueryConfig {
    pub(crate) fn chain_event_retention_seconds(&self) -> u64 {
        self.retention
            .chain_event_retention_hours
            .saturating_mul(3_600)
    }

    pub(crate) fn mempool_mined_retention_seconds(&self) -> u64 {
        self.retention
            .mempool_mined_retention_minutes
            .saturating_mul(60)
    }

    pub(crate) fn mempool_invalidated_retention_seconds(&self) -> u64 {
        self.retention
            .mempool_invalidated_retention_hours
            .saturating_mul(3_600)
    }
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
    pub(crate) ingest_control_bearer_token_path: Option<PathBuf>,
    pub(crate) chain_event_retention_hours: Option<u64>,
    pub(crate) mempool_mined_retention_minutes: Option<u64>,
    pub(crate) mempool_invalidated_retention_hours: Option<u64>,
    pub(crate) listen_addr: Option<SocketAddr>,
    pub(crate) ops_listen_addr: Option<SocketAddr>,
    pub(crate) node_json_rpc_addr: Option<String>,
}

/// Error returned while resolving query configuration or running the gRPC server.
#[derive(Debug, Error)]
pub(crate) enum QueryConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    DeriveStore(#[from] DeriveStoreError),

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
        // Storage defaults match the canonical Zinder layout (`/var/lib/zinder/store`
        // for the writer's primary, `/var/lib/zinder/secondary` for this reader's
        // RocksDB secondary). Operators override via env vars
        // (`ZINDER_STORAGE__PATH`, `ZINDER_STORAGE__SECONDARY_PATH`) or `--storage-path`
        // / `--secondary-path` CLI flags; non-Railway deployments that mount
        // volumes elsewhere set the env vars.
        .with_default("storage.path", "/var/lib/zinder/store")?
        .with_default("storage.secondary_path", "/var/lib/zinder/secondary")?
        // The single-container image runs the writer in the same PID namespace,
        // so the reader connects to ingest's control endpoint over loopback.
        // Cross-host topologies override with `ZINDER_INGEST_CONTROL__ADDR` or
        // `--ingest-control-addr`.
        .with_default("ingest_control.addr", "http://127.0.0.1:9100")?
        .with_default("query.listen_addr", "127.0.0.1:9101")?
        .with_default("query.grpc.enable_reflection", true)?
        .with_default("query.grpc.enable_health", true)?
        .with_default("query.utxo_set_commitment_enabled", false)?
        .with_ops_section(ServiceIdentifier::Query)?
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
            "retention.chain_event_retention_hours",
            overrides.chain_event_retention_hours,
        )?
        .with_override_if(
            "retention.mempool_mined_retention_minutes",
            overrides.mempool_mined_retention_minutes,
        )?
        .with_override_if(
            "retention.mempool_invalidated_retention_hours",
            overrides.mempool_invalidated_retention_hours,
        )?
        .with_override_if(
            "query.listen_addr",
            overrides.listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if(
            "ops.listen_addr",
            overrides.ops_listen_addr.map(|addr| addr.to_string()),
        )?
        .with_override_if("node.json_rpc_addr", overrides.node_json_rpc_addr)?
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
    ops: OpsSection,
    storage: SecondaryStorageSection,
    retention: RetentionSection,
    ingest_control: IngestControlSection,
    query: QuerySection,
    node: NodeSection,
    security: SecuritySection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QuerySection {
    listen_addr: Option<String>,
    grpc: QueryGrpcSection,
    utxo_set_commitment_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueryGrpcSection {
    enable_reflection: Option<bool>,
    enable_health: Option<bool>,
}

fn resolve_query_config(config: QueryRawConfig) -> Result<QueryConfig, QueryConfigError> {
    let network = config.network.resolve()?;
    let storage = resolve_secondary_storage(config.storage)?;
    let retention = resolve_retention(config.retention)?;
    let ResolvedIngestControlReader {
        addr: ingest_control_addr,
        bearer_token_path: ingest_control_bearer_token_path,
        bearer_token: ingest_control_bearer_token,
    } = resolve_ingest_control_reader(config.ingest_control)?;
    let listen_addr_string = require_field(config.query.listen_addr, "query.listen_addr")?;
    let listen_addr = parse_socket_addr("query.listen_addr", &listen_addr_string)?;
    let enable_reflection = require_field(
        config.query.grpc.enable_reflection,
        "query.grpc.enable_reflection",
    )?;
    let enable_health = require_field(config.query.grpc.enable_health, "query.grpc.enable_health")?;
    let utxo_set_commitment_enabled = require_field(
        config.query.utxo_set_commitment_enabled,
        "query.utxo_set_commitment_enabled",
    )?;
    let ops_listen_addr = resolve_ops_listen_addr(config.ops)?;
    let allow_public_bind = resolve_allow_public_bind(config.security)?;
    guard_serving_bind("query.listen_addr", listen_addr, allow_public_bind)?;
    guard_optional_serving_bind("ops.listen_addr", ops_listen_addr, allow_public_bind)?;
    let broadcaster =
        NodeTarget::resolve_optional(network, config.node).map_err(ConfigError::from)?;

    Ok(QueryConfig {
        network,
        storage,
        ingest_control_addr,
        ingest_control_bearer_token_path,
        ingest_control_bearer_token,
        retention,
        listen_addr,
        ops_listen_addr,
        allow_public_bind,
        grpc: QueryGrpcConfig {
            enable_reflection,
            enable_health,
        },
        utxo_set_commitment_enabled,
        broadcaster,
    })
}

#[derive(Serialize)]
struct QueryConfigToml {
    network: NetworkToml,
    ops: OpsToml,
    security: SecurityToml,
    storage: SecondaryStorageToml,
    retention: RetentionToml,
    ingest_control: IngestControlReaderToml,
    query: QueryToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeToml>,
}

impl QueryConfigToml {
    fn from_query_config(config: &QueryConfig) -> Self {
        Self {
            network: NetworkToml::from_network(config.network),
            ops: OpsToml::from_resolved(config.ops_listen_addr),
            security: SecurityToml::from_resolved(config.allow_public_bind),
            storage: SecondaryStorageToml::from_resolved(&config.storage),
            retention: RetentionToml::from_resolved(config.retention),
            ingest_control: IngestControlReaderToml::from_resolved(
                config.ingest_control_addr.clone(),
                config.ingest_control_bearer_token_path.as_deref(),
            ),
            query: QueryToml {
                listen_addr: config.listen_addr.to_string(),
                grpc: QueryGrpcToml {
                    enable_reflection: config.grpc.enable_reflection,
                    enable_health: config.grpc.enable_health,
                },
                utxo_set_commitment_enabled: config.utxo_set_commitment_enabled,
            },
            node: config.broadcaster.as_ref().map(NodeToml::from_node_target),
        }
    }
}

#[derive(Serialize)]
struct QueryToml {
    listen_addr: String,
    grpc: QueryGrpcToml,
    utxo_set_commitment_enabled: bool,
}

#[derive(Serialize)]
struct QueryGrpcToml {
    enable_reflection: bool,
    enable_health: bool,
}
