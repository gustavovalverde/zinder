//! Resolved upstream node endpoint shared by production binaries and live tests.
//!
//! `NodeTarget` is the canonical "what does it take to talk to a node" type:
//! every production binary's resolved `Config` embeds one, and live tests read
//! the same shape directly from environment variables. The `[node]` TOML block
//! deserializes into [`NodeSection`], and [`NodeTarget::resolve`] turns that
//! raw section into the resolved type.
//!
//! See [Public interfaces §Configuration Conventions](../../../docs/architecture/public-interfaces.md#configuration-conventions)
//! for the canonical TOML schema. The env-var keys (after `config-rs`'s `__`
//! flattening) are:
//!
//! | Env var | Field |
//! | ------- | ----- |
//! | `ZINDER_NETWORK` | resolved separately (each binary owns its `[network]`) |
//! | `ZINDER_NODE__JSON_RPC_ADDR` | [`NodeTarget::json_rpc_addr`] |
//! | `ZINDER_NODE__INDEXER_GRPC_ADDR` | [`NodeTarget::indexer_grpc_addr`] |
//! | `ZINDER_NODE__AUTH__METHOD` | `none` / `basic` / `cookie` |
//! | `ZINDER_NODE__AUTH__USERNAME` | Basic-auth username |
//! | `ZINDER_NODE__AUTH__PASSWORD` | Basic-auth password |
//! | `ZINDER_NODE__AUTH__PATH` | Cookie-auth file path |
//! | `ZINDER_NODE__AUTH__COOKIE` | Cookie-auth inline credentials |
//! | `ZINDER_NODE__REQUEST_TIMEOUT_SECS` | [`NodeTarget::request_timeout`] |
//! | `ZINDER_NODE__MAX_RESPONSE_BYTES` | [`NodeTarget::max_response_bytes`] |
//!
//! Cookie credentials may be supplied either by file path or inline (but not
//! both). Inline credentials let `PaaS`-style deployments inject the secret
//! directly through the environment without writing a shim that materializes
//! a cookie file.

use std::{num::NonZeroU64, path::PathBuf, time::Duration};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_core::Network;
use zinder_core::wire::decode_zinder_native_chain_name;

use crate::{CookieSource, DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth};

/// Default per-RPC node request timeout when the configuration omits one.
pub const DEFAULT_NODE_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Resolved upstream node endpoint shared across production binaries and live tests.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct NodeTarget {
    /// Network the node answers for.
    pub network: Network,
    /// Node JSON-RPC base URL.
    pub json_rpc_addr: String,
    /// Optional Zebra indexer gRPC endpoint URL.
    ///
    /// When set, enables the streaming
    /// [`ZebraIndexerMempoolSource`](crate::ZebraIndexerMempoolSource)
    /// backend instead of the polling fallback. Zebra exposes this port
    /// when started with `ZEBRA_RPC__INDEXER_LISTEN_ADDR`.
    pub indexer_grpc_addr: Option<String>,
    /// Node authentication.
    pub node_auth: NodeAuth,
    /// Per-RPC request timeout.
    pub request_timeout: Duration,
    /// Maximum JSON-RPC response body size accepted from the node.
    pub max_response_bytes: NonZeroU64,
}

impl NodeTarget {
    /// Builds a [`NodeTarget`] from already-resolved required fields. The
    /// optional [`NodeTarget::indexer_grpc_addr`] defaults to `None`; opt
    /// in to streaming with [`NodeTarget::with_indexer_grpc_addr`].
    #[must_use]
    pub const fn new(
        network: Network,
        json_rpc_addr: String,
        node_auth: NodeAuth,
        request_timeout: Duration,
        max_response_bytes: NonZeroU64,
    ) -> Self {
        Self {
            network,
            json_rpc_addr,
            indexer_grpc_addr: None,
            node_auth,
            request_timeout,
            max_response_bytes,
        }
    }

    /// Returns a new [`NodeTarget`] with [`NodeTarget::indexer_grpc_addr`]
    /// replaced by `indexer_grpc_addr`.
    #[must_use]
    pub fn with_indexer_grpc_addr(mut self, indexer_grpc_addr: Option<String>) -> Self {
        self.indexer_grpc_addr = indexer_grpc_addr;
        self
    }

    /// Resolves an optional [`NodeTarget`] from a deserialized [`NodeSection`].
    ///
    /// Returns `Ok(None)` when [`NodeSection::json_rpc_addr`] is unset, so a
    /// missing `[node]` section means "no node target," not "validation
    /// failure." Once the address is present every other required field is
    /// validated through [`NodeTarget::resolve`]. Used by reader binaries
    /// where the broadcaster is optional.
    pub fn resolve_optional(
        network: Network,
        section: NodeSection,
    ) -> Result<Option<Self>, NodeConfigError> {
        if section.json_rpc_addr.is_none() {
            return Ok(None);
        }
        Self::resolve(network, section).map(Some)
    }

    /// Resolves a [`NodeTarget`] from a deserialized [`NodeSection`].
    ///
    /// Each production binary calls this after deserializing its raw config
    /// through `config-rs`. Live tests call [`NodeTarget::from_environment`].
    pub fn resolve(network: Network, section: NodeSection) -> Result<Self, NodeConfigError> {
        let json_rpc_addr = section.json_rpc_addr.ok_or(NodeConfigError::MissingField {
            field: "node.json_rpc_addr",
        })?;
        let request_timeout = Duration::from_secs(
            section
                .request_timeout_secs
                .unwrap_or(DEFAULT_NODE_REQUEST_TIMEOUT_SECS),
        );
        let max_response_bytes_value = section
            .max_response_bytes
            .unwrap_or_else(|| DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES.get());
        let max_response_bytes =
            NonZeroU64::new(max_response_bytes_value).ok_or(NodeConfigError::Invalid {
                reason: "node.max_response_bytes must be greater than zero",
            })?;
        let node_auth = resolve_node_auth(section.auth)?;

        Ok(Self::new(
            network,
            json_rpc_addr,
            node_auth,
            request_timeout,
            max_response_bytes,
        )
        .with_indexer_grpc_addr(section.indexer_grpc_addr))
    }

    /// Resolves a [`NodeTarget`] directly from the unified env-var schema.
    ///
    /// Reads `ZINDER_NETWORK` and `ZINDER_NODE__*` from `std::env` without
    /// going through the production loader, which rejects sensitive leaves.
    /// Used by live tests via `zinder_testkit::live::LiveTestEnv` so the same
    /// env-var schema serves production and tests.
    pub fn from_environment() -> Result<Self, NodeConfigError> {
        let network_name = read_required("ZINDER_NETWORK")?;
        let network = decode_zinder_native_chain_name(&network_name).ok().ok_or(
            NodeConfigError::Invalid {
                reason: "ZINDER_NETWORK must be zcash-mainnet, zcash-testnet, or zcash-regtest",
            },
        )?;

        let section = NodeSection {
            json_rpc_addr: read_optional("ZINDER_NODE__JSON_RPC_ADDR"),
            indexer_grpc_addr: read_optional("ZINDER_NODE__INDEXER_GRPC_ADDR"),
            request_timeout_secs: read_optional_parsed::<u64>(
                "ZINDER_NODE__REQUEST_TIMEOUT_SECS",
                "node.request_timeout_secs",
            )?,
            max_response_bytes: read_optional_parsed::<u64>(
                "ZINDER_NODE__MAX_RESPONSE_BYTES",
                "node.max_response_bytes",
            )?,
            auth: NodeAuthSection {
                method: read_optional("ZINDER_NODE__AUTH__METHOD"),
                username: read_optional("ZINDER_NODE__AUTH__USERNAME"),
                password: read_optional("ZINDER_NODE__AUTH__PASSWORD"),
                path: read_optional("ZINDER_NODE__AUTH__PATH").map(PathBuf::from),
                cookie: read_optional("ZINDER_NODE__AUTH__COOKIE"),
            },
        };

        Self::resolve(network, section)
    }
}

/// Raw `[node]` config section. Each binary's typed `Config` embeds one of
/// these and passes it to [`NodeTarget::resolve`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeSection {
    /// Node JSON-RPC base URL.
    pub json_rpc_addr: Option<String>,
    /// Zebra indexer gRPC endpoint URL.
    ///
    /// When set, enables the streaming mempool source backend and chain-tip
    /// wakeups. Omit to fall back to JSON-RPC polling.
    pub indexer_grpc_addr: Option<String>,
    /// Per-RPC request timeout in seconds.
    pub request_timeout_secs: Option<u64>,
    /// Maximum JSON-RPC response body size accepted from the node.
    pub max_response_bytes: Option<u64>,
    /// Authentication subsection.
    pub auth: NodeAuthSection,
}

/// Raw `[node.auth]` config section.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeAuthSection {
    /// Auth method: `none`, `basic`, or `cookie`. Defaults to `none`.
    pub method: Option<String>,
    /// Basic-auth username.
    pub username: Option<String>,
    /// Basic-auth password.
    pub password: Option<String>,
    /// Cookie-auth file path. Mutually exclusive with [`Self::cookie`].
    pub path: Option<PathBuf>,
    /// Cookie-auth inline credentials. Used by `PaaS` deployments that inject
    /// the secret as a configuration value. Mutually exclusive with
    /// [`Self::path`].
    pub cookie: Option<String>,
}

/// Error returned while resolving node configuration.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NodeConfigError {
    /// A required field is missing.
    #[error("missing required configuration field: {field}")]
    MissingField {
        /// Configuration field path.
        field: &'static str,
    },

    /// A field value is invalid for the chosen mode.
    #[error("invalid node configuration: {reason}")]
    Invalid {
        /// Human-readable reason describing the validation failure.
        reason: &'static str,
    },

    /// A field is not valid in combination with the selected auth method.
    #[error("{field} is not valid when node.auth.method is {method}")]
    AuthFieldNotApplicable {
        /// Conflicting field path.
        field: &'static str,
        /// Selected auth method.
        method: &'static str,
    },

    /// Auth method string is not recognized.
    #[error("unknown node.auth.method: {method}")]
    UnknownAuthMethod {
        /// Unrecognized method string.
        method: String,
    },

    /// An environment variable could not be parsed as the expected type.
    #[error("failed to parse environment variable for {field}: {reason}")]
    EnvParseFailed {
        /// Configuration field path.
        field: &'static str,
        /// Parse failure reason.
        reason: String,
    },
}

fn resolve_node_auth(section: NodeAuthSection) -> Result<NodeAuth, NodeConfigError> {
    let method = section.method.as_deref().unwrap_or("none");

    match method {
        "none" => {
            reject_present(section.username.is_some(), "node.auth.username", "none")?;
            reject_present(section.password.is_some(), "node.auth.password", "none")?;
            reject_present(section.path.is_some(), "node.auth.path", "none")?;
            reject_present(section.cookie.is_some(), "node.auth.cookie", "none")?;
            Ok(NodeAuth::None)
        }
        "basic" => {
            reject_present(section.path.is_some(), "node.auth.path", "basic")?;
            reject_present(section.cookie.is_some(), "node.auth.cookie", "basic")?;
            let username = section.username.ok_or(NodeConfigError::MissingField {
                field: "node.auth.username",
            })?;
            let password = section.password.ok_or(NodeConfigError::MissingField {
                field: "node.auth.password",
            })?;
            Ok(NodeAuth::basic(username, password))
        }
        "cookie" => {
            reject_present(section.username.is_some(), "node.auth.username", "cookie")?;
            reject_present(section.password.is_some(), "node.auth.password", "cookie")?;
            let source = match (section.path, section.cookie) {
                (Some(_), Some(_)) => {
                    return Err(NodeConfigError::Invalid {
                        reason: "node.auth.path and node.auth.cookie are mutually exclusive",
                    });
                }
                (Some(path), None) => CookieSource::File(path),
                (None, Some(content)) => CookieSource::Inline(SecretString::from(content)),
                (None, None) => {
                    return Err(NodeConfigError::MissingField {
                        field: "node.auth.path or node.auth.cookie",
                    });
                }
            };
            Ok(NodeAuth::Cookie(source))
        }
        other => Err(NodeConfigError::UnknownAuthMethod {
            method: other.to_owned(),
        }),
    }
}

fn reject_present(
    is_field_present: bool,
    field: &'static str,
    method: &'static str,
) -> Result<(), NodeConfigError> {
    if is_field_present {
        return Err(NodeConfigError::AuthFieldNotApplicable { field, method });
    }
    Ok(())
}

fn read_required(env_var: &'static str) -> Result<String, NodeConfigError> {
    std::env::var(env_var).map_err(|_| NodeConfigError::MissingField { field: env_var })
}

fn read_optional(env_var: &'static str) -> Option<String> {
    std::env::var(env_var).ok()
}

fn read_optional_parsed<TargetType>(
    env_var: &'static str,
    field: &'static str,
) -> Result<Option<TargetType>, NodeConfigError>
where
    TargetType: std::str::FromStr,
    TargetType::Err: std::fmt::Display,
{
    std::env::var(env_var).map_or(Ok(None), |raw| {
        raw.parse::<TargetType>()
            .map(Some)
            .map_err(|error| NodeConfigError::EnvParseFailed {
                field,
                reason: error.to_string(),
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_basic_auth_round_trip() -> Result<(), NodeConfigError> {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: Some("http://127.0.0.1:8155".to_owned()),
            request_timeout_secs: Some(15),
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("basic".to_owned()),
                username: Some("zebra".to_owned()),
                password: Some("zebra".to_owned()),
                path: None,
                cookie: None,
            },
        };
        let target = NodeTarget::resolve(Network::ZcashRegtest, section)?;

        assert_eq!(target.network, Network::ZcashRegtest);
        assert_eq!(target.json_rpc_addr, "http://127.0.0.1:8232");
        assert_eq!(
            target.indexer_grpc_addr.as_deref(),
            Some("http://127.0.0.1:8155")
        );
        assert_eq!(target.request_timeout, Duration::from_secs(15));
        assert_eq!(target.node_auth.scheme_name(), "basic");
        Ok(())
    }

    #[test]
    fn resolve_omits_indexer_grpc_addr_when_unset() -> Result<(), NodeConfigError> {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("none".to_owned()),
                username: None,
                password: None,
                path: None,
                cookie: None,
            },
        };
        let target = NodeTarget::resolve(Network::ZcashRegtest, section)?;

        assert!(target.indexer_grpc_addr.is_none());
        Ok(())
    }

    #[test]
    fn resolve_rejects_missing_json_rpc_addr() {
        let section = NodeSection::default();
        let outcome = NodeTarget::resolve(Network::ZcashRegtest, section);

        assert!(matches!(
            outcome,
            Err(NodeConfigError::MissingField {
                field: "node.json_rpc_addr"
            })
        ));
    }

    #[test]
    fn resolve_rejects_basic_auth_with_path() {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("basic".to_owned()),
                username: Some("zebra".to_owned()),
                password: Some("zebra".to_owned()),
                path: Some(PathBuf::from("/etc/zebra/cookie")),
                cookie: None,
            },
        };
        let outcome = NodeTarget::resolve(Network::ZcashRegtest, section);

        assert!(matches!(
            outcome,
            Err(NodeConfigError::AuthFieldNotApplicable {
                field: "node.auth.path",
                method: "basic",
            })
        ));
    }

    #[test]
    fn resolve_cookie_auth_from_path() -> Result<(), NodeConfigError> {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("cookie".to_owned()),
                username: None,
                password: None,
                path: Some(PathBuf::from("/var/run/auth/.cookie")),
                cookie: None,
            },
        };
        let target = NodeTarget::resolve(Network::ZcashRegtest, section)?;

        assert!(matches!(
            target.node_auth,
            NodeAuth::Cookie(CookieSource::File(_))
        ));
        Ok(())
    }

    #[test]
    fn resolve_cookie_auth_from_inline() -> Result<(), NodeConfigError> {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("cookie".to_owned()),
                username: None,
                password: None,
                path: None,
                cookie: Some("user:cookie-secret".to_owned()),
            },
        };
        let target = NodeTarget::resolve(Network::ZcashRegtest, section)?;

        assert!(matches!(
            target.node_auth,
            NodeAuth::Cookie(CookieSource::Inline(_))
        ));
        Ok(())
    }

    #[test]
    fn resolve_rejects_cookie_auth_with_both_path_and_inline() {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("cookie".to_owned()),
                username: None,
                password: None,
                path: Some(PathBuf::from("/var/run/auth/.cookie")),
                cookie: Some("user:cookie-secret".to_owned()),
            },
        };
        let outcome = NodeTarget::resolve(Network::ZcashRegtest, section);

        assert!(matches!(
            outcome,
            Err(NodeConfigError::Invalid {
                reason: "node.auth.path and node.auth.cookie are mutually exclusive",
            })
        ));
    }

    #[test]
    fn resolve_rejects_cookie_auth_without_path_or_inline() {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("cookie".to_owned()),
                username: None,
                password: None,
                path: None,
                cookie: None,
            },
        };
        let outcome = NodeTarget::resolve(Network::ZcashRegtest, section);

        assert!(matches!(
            outcome,
            Err(NodeConfigError::MissingField {
                field: "node.auth.path or node.auth.cookie",
            })
        ));
    }

    #[test]
    fn resolve_rejects_unknown_auth_method() {
        let section = NodeSection {
            json_rpc_addr: Some("http://127.0.0.1:8232".to_owned()),
            indexer_grpc_addr: None,
            request_timeout_secs: None,
            max_response_bytes: None,
            auth: NodeAuthSection {
                method: Some("oauth".to_owned()),
                username: None,
                password: None,
                path: None,
                cookie: None,
            },
        };
        let outcome = NodeTarget::resolve(Network::ZcashRegtest, section);

        assert!(matches!(
            outcome,
            Err(NodeConfigError::UnknownAuthMethod { method }) if method == "oauth"
        ));
    }
}
