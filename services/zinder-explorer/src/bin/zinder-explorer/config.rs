//! Configuration loading for the `zinder-explorer` binary.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_runtime::{
    BearerToken, BearerTokenError, ConfigError, ConfigLoader, InvalidZinderGrpcEndpoint,
    NetworkSection, NetworkToml, NodeAuthToml, NodeHealthToml, OpsSection, OpsServerError, OpsToml,
    RocksDbResourceBudgetSection, RuntimeService, SecuritySection, SecurityToml,
    guard_optional_serving_bind, guard_serving_bind, load_bearer_token, parse_socket_addr,
    require_field, resolve_allow_public_bind, resolve_materialized_view_reader_rocksdb_budget,
    resolve_ops_listen_addr, validate_zinder_grpc_endpoint,
};
use zinder_source::{NodeAuthSection, NodeHealthSection, NodeSection, NodeTarget};
use zinder_store::RocksDbResourceBudget;

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:9068";

/// Resolved explorer-plane runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerConfig {
    pub(crate) network: Network,
    pub(crate) storage: ExplorerMaterializedViewStorage,
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

/// Resolved storage opened by the Explorer materialized-view secondary.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerMaterializedViewStorage {
    /// Canonical root containing the ingest-owned materialized-view primary.
    pub(crate) canonical_root_path: PathBuf,
    /// Explorer-owned root for materialized-view secondary metadata.
    pub(crate) secondary_root_path: PathBuf,
    /// Effective reader budget for the materialized-view secondary.
    pub(crate) rocksdb_budget: RocksDbResourceBudget,
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

    #[error("failed to bind ExplorerQuery listener at {listen_addr}: {source}")]
    GrpcBind {
        listen_addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

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
    storage: ExplorerStorageSection,
    explorer: ExplorerSection,
    ops: OpsSection,
    node: ExplorerNodeSection,
    security: SecuritySection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerStorageSection {
    path: Option<PathBuf>,
    secondary_path: Option<PathBuf>,
    materialized_views: ExplorerMaterializedViewSecondarySection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerMaterializedViewSecondarySection {
    rocksdb: ExplorerMaterializedViewSecondaryRocksDbSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerMaterializedViewSecondaryRocksDbSection {
    block_cache_bytes: Option<u64>,
    max_open_files: Option<i32>,
    write_buffer_bytes: Option<u64>,
    max_write_buffer_count: Option<i32>,
    memtable_budget_bytes: Option<u64>,
    statistics_level: Option<String>,
}

impl ExplorerMaterializedViewSecondaryRocksDbSection {
    fn into_resource_budget_section(self) -> RocksDbResourceBudgetSection {
        RocksDbResourceBudgetSection {
            block_cache_bytes: self.block_cache_bytes,
            max_wal_bytes: None,
            max_open_files: self.max_open_files,
            write_buffer_bytes: self.write_buffer_bytes,
            max_write_buffer_count: self.max_write_buffer_count,
            max_background_jobs: None,
            memtable_budget_bytes: self.memtable_budget_bytes,
            statistics_level: self.statistics_level,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExplorerNodeSection {
    json_rpc_addr: Option<String>,
    request_timeout_secs: Option<u64>,
    max_response_bytes: Option<u64>,
    auth: NodeAuthSection,
    health: NodeHealthSection,
}

impl ExplorerNodeSection {
    fn has_configured_leaf(&self) -> bool {
        self.json_rpc_addr.is_some()
            || self.request_timeout_secs.is_some()
            || self.max_response_bytes.is_some()
            || self.auth.method.is_some()
            || self.auth.username.is_some()
            || self.auth.password.is_some()
            || self.auth.path.is_some()
            || self.auth.cookie.is_some()
            || self.health.addr.is_some()
            || self.health.poll_interval_ms.is_some()
            || self.health.verification_progress_floor.is_some()
            || self.health.estimated_gap_floor_blocks.is_some()
    }

    fn into_shared_section(self) -> NodeSection {
        NodeSection {
            json_rpc_addr: self.json_rpc_addr,
            indexer_grpc_addr: None,
            request_timeout_secs: self.request_timeout_secs,
            max_response_bytes: self.max_response_bytes,
            broadcast_timeout_secs: None,
            auth: self.auth,
            health: self.health,
        }
    }
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
    storage: ExplorerStorageToml,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<ExplorerNodeToml>,
    explorer: ExplorerToml,
}

#[derive(Debug, Serialize)]
struct ExplorerStorageToml {
    path: String,
    secondary_path: String,
    materialized_views: ExplorerMaterializedViewSecondaryToml,
}

#[derive(Debug, Serialize)]
struct ExplorerMaterializedViewSecondaryToml {
    rocksdb: ExplorerMaterializedViewSecondaryRocksDbToml,
}

#[derive(Debug, Serialize)]
struct ExplorerMaterializedViewSecondaryRocksDbToml {
    block_cache_bytes: u64,
    max_open_files: i32,
    write_buffer_bytes: u64,
    max_write_buffer_count: i32,
    memtable_budget_bytes: u64,
    statistics_level: &'static str,
}

impl ExplorerMaterializedViewSecondaryRocksDbToml {
    const fn from_resolved(budget: RocksDbResourceBudget) -> Self {
        Self {
            block_cache_bytes: budget.block_cache_bytes,
            max_open_files: budget.max_open_files,
            write_buffer_bytes: budget.write_buffer_bytes,
            max_write_buffer_count: budget.max_write_buffer_count,
            memtable_budget_bytes: budget.memtable_budget_bytes,
            statistics_level: budget.statistics_level.as_str(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ExplorerNodeToml {
    json_rpc_addr: String,
    request_timeout_secs: u64,
    max_response_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<NodeHealthToml>,
    auth: NodeAuthToml,
}

impl ExplorerNodeToml {
    fn from_resolved(node: &NodeTarget) -> Self {
        Self {
            json_rpc_addr: node.json_rpc_addr.clone(),
            request_timeout_secs: node.request_timeout.as_secs(),
            max_response_bytes: node.max_response_bytes.get(),
            health: node
                .health
                .as_ref()
                .map(NodeHealthToml::from_node_health_config),
            auth: NodeAuthToml::from_node_auth(&node.node_auth),
        }
    }
}

impl ExplorerStorageToml {
    fn from_resolved(storage: &ExplorerMaterializedViewStorage) -> Self {
        Self {
            path: storage.canonical_root_path.display().to_string(),
            secondary_path: storage.secondary_root_path.display().to_string(),
            materialized_views: ExplorerMaterializedViewSecondaryToml {
                rocksdb: ExplorerMaterializedViewSecondaryRocksDbToml::from_resolved(
                    storage.rocksdb_budget,
                ),
            },
        }
    }
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
            storage: ExplorerStorageToml::from_resolved(&config.storage),
            node: config.node.as_ref().map(ExplorerNodeToml::from_resolved),
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
    let storage = resolve_explorer_storage(raw.storage)?;
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
    let node = resolve_explorer_node(network, raw.node)?;
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

fn resolve_explorer_node(
    network: Network,
    node: ExplorerNodeSection,
) -> Result<Option<NodeTarget>, zinder_source::NodeConfigError> {
    if node.json_rpc_addr.is_none() && node.has_configured_leaf() {
        return Err(zinder_source::NodeConfigError::MissingField {
            field: "node.json_rpc_addr",
        });
    }
    if node.health.addr.is_none()
        && (node.health.poll_interval_ms.is_some()
            || node.health.verification_progress_floor.is_some()
            || node.health.estimated_gap_floor_blocks.is_some())
    {
        return Err(zinder_source::NodeConfigError::Invalid {
            reason: "node.health.addr is required when explorer node.health probe settings are configured",
        });
    }
    reject_none_auth_credentials(&node.auth)?;
    NodeTarget::resolve_optional(network, node.into_shared_section())
}

fn reject_none_auth_credentials(
    auth: &NodeAuthSection,
) -> Result<(), zinder_source::NodeConfigError> {
    if auth.method.as_deref().unwrap_or("none") != "none" {
        return Ok(());
    }
    for (present, field) in [
        (auth.username.is_some(), "node.auth.username"),
        (auth.password.is_some(), "node.auth.password"),
        (auth.path.is_some(), "node.auth.path"),
        (auth.cookie.is_some(), "node.auth.cookie"),
    ] {
        if present {
            return Err(zinder_source::NodeConfigError::AuthFieldNotApplicable {
                field,
                method: "none",
            });
        }
    }
    Ok(())
}

fn resolve_explorer_storage(
    storage: ExplorerStorageSection,
) -> Result<ExplorerMaterializedViewStorage, ConfigError> {
    let canonical_root_path = require_field(storage.path, "storage.path")?;
    let secondary_root_path = require_field(storage.secondary_path, "storage.secondary_path")?;
    let rocksdb_budget = resolve_materialized_view_reader_rocksdb_budget(
        storage
            .materialized_views
            .rocksdb
            .into_resource_budget_section(),
    )?;
    Ok(ExplorerMaterializedViewStorage {
        canonical_root_path,
        secondary_root_path,
        rocksdb_budget,
    })
}
