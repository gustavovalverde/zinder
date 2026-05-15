//! Shared configuration loading helpers used by every Zinder service binary.
//!
//! Every binary follows the same `defaults -> file -> ZINDER_* environment ->
//! CLI overrides` precedence. This module owns that policy so the rules
//! cannot drift between binaries.
//!
//! Live tests reuse the production env-var schema directly; the
//! `ZINDER_TEST_LIVE` gate (and other `ZINDER_TEST_*` knobs like
//! `ZINDER_STORE_CRASH_*`) are stripped here so test-only acknowledgements
//! cannot leak into a production binary's config.
//!
//! Secret hygiene is handled at emit time rather than at load time. Secrets
//! pass through this loader unchanged; redaction happens in
//! [`NodeAuthToml::from_node_auth`] (used by `--print-config`) and in the
//! manual `Debug` impls on [`zinder_source::NodeAuth`] and
//! [`crate::auth::BearerToken`]. Per-surface file-only constraints (see
//! [ADR-0006](../../../docs/adrs/0006-ingest-control-transport-security.md))
//! remain enforced at their respective config types.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use ::config::{
    Config, ConfigBuilder, ConfigError as InnerConfigError, Environment, File, FileFormat, Value,
    builder::DefaultState,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use zinder_core::Network;
use zinder_core::wire::{decode_zinder_native_chain_name, encode_zinder_native_chain_name};
use zinder_source::{NodeAuth, NodeConfigError, NodeTarget};

const ENV_PREFIX: &str = "ZINDER_";
const TEST_ENV_PREFIXES: &[&str] = &["ZINDER_TEST_"];

/// Error returned while resolving Zinder service configuration from defaults,
/// file, environment, and CLI overrides.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// Loading or deserializing layered configuration failed.
    #[error("failed to load configuration: {source}")]
    Load {
        /// Underlying `config-rs` error.
        #[source]
        source: InnerConfigError,
    },

    /// Rendering effective configuration to TOML failed.
    #[error("failed to render configuration: {source}")]
    Render {
        /// Underlying TOML serialization error.
        #[source]
        source: toml::ser::Error,
    },

    /// A required configuration field is missing after all layers are merged.
    #[error("missing configuration field: {field}")]
    MissingField {
        /// Public configuration field path, such as `node.json_rpc_addr`.
        field: &'static str,
    },

    /// Configuration contains an invalid combination or value.
    #[error("invalid configuration: {reason}")]
    Invalid {
        /// Validation failure reason.
        reason: String,
    },

    /// A CLI-supplied path is not valid UTF-8 and cannot be carried through
    /// the TOML-shaped configuration layer.
    #[error("configuration path field {field} is not valid UTF-8")]
    NonUnicodePath {
        /// Public configuration field path.
        field: &'static str,
    },
}

impl ConfigError {
    /// Builds a [`ConfigError::Load`] from a `config-rs` error.
    #[must_use]
    pub fn load(source: InnerConfigError) -> Self {
        Self::Load { source }
    }

    /// Builds a [`ConfigError::Invalid`] from a free-form reason.
    #[must_use]
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self::Invalid {
            reason: reason.into(),
        }
    }

    /// Builds a [`ConfigError::MissingField`] for the given field path.
    #[must_use]
    pub const fn missing_field(field: &'static str) -> Self {
        Self::MissingField { field }
    }
}

impl From<NodeConfigError> for ConfigError {
    fn from(error: NodeConfigError) -> Self {
        match error {
            NodeConfigError::MissingField { field } => Self::MissingField { field },
            NodeConfigError::Invalid { reason } => Self::invalid(reason),
            NodeConfigError::AuthFieldNotApplicable { field, method } => Self::invalid(format!(
                "{field} is not valid when node.auth.method is {method}"
            )),
            NodeConfigError::UnknownAuthMethod { method } => {
                Self::invalid(format!("unknown node.auth.method: {method}"))
            }
            NodeConfigError::EnvParseFailed { field, reason } => Self::invalid(format!(
                "failed to parse environment variable for {field}: {reason}"
            )),
            other => Self::invalid(other.to_string()),
        }
    }
}

/// Builds the standard Zinder environment source for `config-rs`.
///
/// Strips the `ZINDER_TEST_` prefix (used by `ZINDER_TEST_LIVE` and crash-
/// recovery harness vars) so test-only acknowledgements cannot leak into
/// production config. Secret values pass through unchanged; redaction is
/// applied at emit boundaries (see [`NodeAuthToml`] and the `Debug` impls on
/// [`NodeAuth`] and [`crate::auth::BearerToken`]).
pub fn zinder_environment_source() -> Result<Environment, ConfigError> {
    let mut filtered_env = HashMap::new();

    for (variable, env_value) in std::env::vars() {
        if TEST_ENV_PREFIXES
            .iter()
            .any(|test_prefix| variable.starts_with(test_prefix))
        {
            continue;
        }

        let Some(config_key) = variable.strip_prefix(ENV_PREFIX) else {
            continue;
        };

        filtered_env.insert(config_key.to_owned(), env_value);
    }

    Ok(Environment::default()
        .separator("__")
        .try_parsing(true)
        .source(Some(filtered_env)))
}

/// Returns `field_value` or a [`ConfigError::MissingField`] error pointing at
/// `field`.
pub fn require_field<T>(field_value: Option<T>, field: &'static str) -> Result<T, ConfigError> {
    field_value.ok_or(ConfigError::MissingField { field })
}

/// Converts `path` to a UTF-8 string suitable for the TOML-shaped config
/// layer, returning [`ConfigError::NonUnicodePath`] if the path is not valid
/// UTF-8.
fn path_to_config_string(path: PathBuf, field: &'static str) -> Result<String, ConfigError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| ConfigError::NonUnicodePath { field })
}

/// Converts a [`Duration`] to whole milliseconds as `u64`.
///
/// Saturates at [`u64::MAX`] for durations that exceed the representable
/// range. Used by every binary's `--print-config` renderer; the saturating
/// fallback keeps the cast lint-clean without panicking on absurd
/// configurations.
#[must_use]
pub fn duration_as_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Raw `[network]` config section shared by every Zinder service binary.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSection {
    /// Network name: `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`.
    pub name: Option<String>,
}

impl NetworkSection {
    /// Resolves the configured [`Network`] or returns
    /// [`ConfigError::MissingField`] / [`ConfigError::Invalid`].
    pub fn resolve(self) -> Result<Network, ConfigError> {
        let name = require_field(self.name, "network.name")?;
        decode_zinder_native_chain_name(&name).ok().ok_or_else(|| {
            ConfigError::invalid(format!(
                "unknown network: {name}; expected zcash-mainnet, zcash-testnet, or zcash-regtest"
            ))
        })
    }
}

/// Fluent layered configuration loader.
///
/// Encapsulates the canonical Zinder precedence: `defaults -> file ->
/// ZINDER_* environment -> CLI overrides`. Each binary builds the same shape
/// by chaining [`ConfigLoader::with_default`], [`ConfigLoader::with_file`],
/// [`ConfigLoader::with_zinder_env`], and [`ConfigLoader::with_override_if`]
/// before calling [`ConfigLoader::load`] to deserialize.
#[must_use]
pub struct ConfigLoader {
    builder: ConfigBuilder<DefaultState>,
}

impl ConfigLoader {
    /// Starts a new loader with no sources or defaults.
    pub fn new() -> Self {
        Self {
            builder: Config::builder(),
        }
    }

    /// Records a default for `key`. Defaults sit at the lowest precedence
    /// regardless of when they are recorded.
    pub fn with_default<V>(mut self, key: &str, default_for_key: V) -> Result<Self, ConfigError>
    where
        V: Into<Value>,
    {
        self.builder = self
            .builder
            .set_default(key, default_for_key)
            .map_err(ConfigError::load)?;
        Ok(self)
    }

    /// Adds an optional TOML file source. When `path` is `None` the loader
    /// is unchanged; when `Some`, the file is required to exist.
    pub fn with_file(mut self, path: Option<PathBuf>) -> Self {
        if let Some(path) = path {
            self.builder = self
                .builder
                .add_source(File::from(path).format(FileFormat::Toml).required(true));
        }
        self
    }

    /// Adds the standard Zinder environment source (sensitive-leaf rejection
    /// + `ZINDER_TEST_*` stripping).
    pub fn with_zinder_env(mut self) -> Result<Self, ConfigError> {
        self.builder = self.builder.add_source(zinder_environment_source()?);
        Ok(self)
    }

    /// Records an override for `key` when `override_for_key` is `Some`.
    /// Overrides sit at the highest precedence regardless of when they are
    /// recorded.
    pub fn with_override_if<V>(
        mut self,
        key: &str,
        override_for_key: Option<V>,
    ) -> Result<Self, ConfigError>
    where
        V: Into<Value>,
    {
        if let Some(override_for_key) = override_for_key {
            self.builder = self
                .builder
                .set_override(key, override_for_key)
                .map_err(ConfigError::load)?;
        }
        Ok(self)
    }

    /// Records an override for `key` from a [`PathBuf`] when present,
    /// converting to UTF-8 for the TOML-shaped config layer.
    pub fn with_override_path_if(
        mut self,
        key: &'static str,
        override_path: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        if let Some(path) = override_path {
            let path_string = path_to_config_string(path, key)?;
            self.builder = self
                .builder
                .set_override(key, path_string)
                .map_err(ConfigError::load)?;
        }
        Ok(self)
    }

    /// Builds the merged configuration and deserializes it as `T`.
    pub fn load<T>(self) -> Result<T, ConfigError>
    where
        T: DeserializeOwned,
    {
        self.builder
            .build()
            .map_err(ConfigError::load)?
            .try_deserialize::<T>()
            .map_err(ConfigError::load)
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Redacted TOML projection of `[network]` for `--print-config`.
#[derive(Debug, Serialize)]
pub struct NetworkToml {
    /// Network name in canonical form (e.g. `zcash-mainnet`).
    pub name: &'static str,
}

impl NetworkToml {
    /// Builds a [`NetworkToml`] from a resolved [`Network`].
    #[must_use]
    pub const fn from_network(network: Network) -> Self {
        Self {
            name: encode_zinder_native_chain_name(network),
        }
    }
}

/// Redacted TOML projection of `[node.auth]` for `--print-config`.
#[derive(Debug, Serialize)]
pub struct NodeAuthToml {
    /// Auth method: `none`, `basic`, or `cookie`.
    pub method: &'static str,
    /// Basic-auth username, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Basic-auth password placeholder. Always `[REDACTED]` when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<&'static str>,
    /// Cookie-auth path placeholder. Always `[REDACTED]` when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'static str>,
}

impl NodeAuthToml {
    /// Builds a redacted [`NodeAuthToml`] from a resolved [`NodeAuth`].
    ///
    /// Both cookie sources (file path and inline credentials) redact to
    /// `[REDACTED]` so an operator inspecting `--print-config` cannot tell
    /// the credential apart from the path. Distinguishing them is not
    /// useful for debugging and risks leaking metadata about how the secret
    /// was injected.
    #[must_use]
    pub fn from_node_auth(auth: &NodeAuth) -> Self {
        match auth {
            NodeAuth::None => Self {
                method: "none",
                username: None,
                password: None,
                path: None,
            },
            NodeAuth::Basic { username, .. } => Self {
                method: "basic",
                username: Some(username.clone()),
                password: Some("[REDACTED]"),
                path: None,
            },
            NodeAuth::Cookie(_) => Self {
                method: "cookie",
                username: None,
                password: None,
                path: Some("[REDACTED]"),
            },
        }
    }
}

/// Redacted TOML projection of `[node]` for `--print-config`.
#[derive(Debug, Serialize)]
pub struct NodeToml {
    /// Node JSON-RPC base URL.
    pub json_rpc_addr: String,
    /// Optional Zebra indexer gRPC endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexer_grpc_addr: Option<String>,
    /// Per-RPC request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Maximum JSON-RPC response body size accepted from the node.
    pub max_response_bytes: u64,
    /// Auth subsection.
    pub auth: NodeAuthToml,
}

impl NodeToml {
    /// Builds a redacted [`NodeToml`] from a resolved [`NodeTarget`].
    #[must_use]
    pub fn from_node_target(target: &NodeTarget) -> Self {
        Self {
            json_rpc_addr: target.json_rpc_addr.clone(),
            indexer_grpc_addr: target.indexer_grpc_addr.clone(),
            request_timeout_secs: target.request_timeout.as_secs(),
            max_response_bytes: target.max_response_bytes.get(),
            auth: NodeAuthToml::from_node_auth(&target.node_auth),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_field_returns_missing_field_for_none() {
        let outcome: Result<u32, _> = require_field(None, "ingest.commit_batch_blocks");
        assert!(matches!(
            outcome,
            Err(ConfigError::MissingField {
                field: "ingest.commit_batch_blocks"
            })
        ));
    }

    #[test]
    fn require_field_passes_through_value() -> Result<(), ConfigError> {
        let resolved: String = require_field(Some("hello".to_owned()), "x")?;
        assert_eq!(resolved, "hello");
        Ok(())
    }

    #[test]
    fn duration_as_millis_u64_saturates_on_overflow() {
        assert_eq!(duration_as_millis_u64(Duration::from_millis(1_500)), 1_500);
        assert_eq!(duration_as_millis_u64(Duration::MAX), u64::MAX);
    }
}
