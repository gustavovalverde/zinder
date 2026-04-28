//! Shared configuration loading helpers used by every Zinder service binary.
//!
//! Every binary follows the same `defaults -> file -> ZINDER_* environment ->
//! CLI overrides` precedence and the same rules for filtering the test-only
//! prefix (`ZINDER_TEST_`) and rejecting sensitive leaves (`password`,
//! `secret`, `token`, `cookie`, `private_key`). This module owns that policy
//! so the rules cannot drift between binaries.
//!
//! Live tests reuse the production env-var schema directly; the
//! `ZINDER_TEST_LIVE` gate (and other `ZINDER_TEST_*` knobs like
//! `ZINDER_STORE_CRASH_*`) are stripped here so test-only acknowledgements
//! cannot leak into a production binary's config (per
//! [ADR-0012](../../../docs/adrs/0012-test-tiers-and-live-config.md)).

use std::{collections::HashMap, path::PathBuf};

use ::config::{ConfigError as InnerConfigError, Environment};
use thiserror::Error;

const ENV_PREFIX: &str = "ZINDER_";
const TEST_ENV_PREFIXES: &[&str] = &["ZINDER_TEST_"];
const SENSITIVE_ENV_LEAF_MARKERS: &[&str] =
    &["password", "secret", "token", "cookie", "private_key"];

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

    /// A sensitive leaf was provided through a production environment variable.
    ///
    /// Sensitive values must come from the TOML config file or a secret
    /// manager so they never appear in process environment.
    #[error(
        "environment variable {variable} targets sensitive field {field}; use the config file or secret manager instead"
    )]
    SensitiveEnvironmentOverride {
        /// Environment variable name.
        variable: String,
        /// Sensitive leaf field name.
        field: String,
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

/// Builds the standard Zinder environment source for `config-rs`.
///
/// Strips the `ZINDER_TEST_` prefix (used by `ZINDER_TEST_LIVE` and crash-
/// recovery harness vars) so test-only acknowledgements cannot leak into
/// production config, and rejects any `ZINDER_*` variable whose leaf matches
/// a sensitive marker.
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

        if let Some(field) = sensitive_env_field(config_key) {
            return Err(ConfigError::SensitiveEnvironmentOverride { variable, field });
        }

        filtered_env.insert(config_key.to_owned(), env_value);
    }

    Ok(Environment::default()
        .separator("__")
        .try_parsing(true)
        .source(Some(filtered_env)))
}

/// Returns the sensitive leaf field name if `config_key` targets one.
fn sensitive_env_field(config_key: &str) -> Option<String> {
    let leaf = config_key
        .rsplit("__")
        .next()
        .unwrap_or(config_key)
        .to_ascii_lowercase();

    SENSITIVE_ENV_LEAF_MARKERS
        .iter()
        .any(|marker| leaf.contains(marker))
        .then_some(leaf)
}

/// Returns `field_value` or a [`ConfigError::MissingField`] error pointing at
/// `field`.
pub fn require_string(
    field_value: Option<String>,
    field: &'static str,
) -> Result<String, ConfigError> {
    field_value.ok_or(ConfigError::MissingField { field })
}

/// Converts `path` to a UTF-8 string suitable for the TOML-shaped config
/// layer, returning [`ConfigError::NonUnicodePath`] if the path is not valid
/// UTF-8.
pub fn path_to_config_string(path: PathBuf, field: &'static str) -> Result<String, ConfigError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| ConfigError::NonUnicodePath { field })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_env_field_detects_password_leaf() {
        assert_eq!(
            sensitive_env_field("NODE__AUTH__PASSWORD"),
            Some("password".to_owned())
        );
    }

    #[test]
    fn sensitive_env_field_passes_innocuous_leaf() {
        assert!(sensitive_env_field("NODE__JSON_RPC_ADDR").is_none());
    }

    #[test]
    fn require_string_returns_missing_field_for_none() {
        let result = require_string(None, "ingest.commit_batch_blocks");
        assert!(matches!(
            result,
            Err(ConfigError::MissingField {
                field: "ingest.commit_batch_blocks"
            })
        ));
    }

    #[test]
    fn require_string_passes_through_value() -> Result<(), ConfigError> {
        let value = require_string(Some("hello".to_owned()), "x")?;
        assert_eq!(value, "hello");
        Ok(())
    }
}
