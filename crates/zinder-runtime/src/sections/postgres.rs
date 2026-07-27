//! Shared `[storage.postgres]` configuration contract.
//!
//! PostgreSQL-backed services consume the same secret-file and transport
//! vocabulary so migration, ingest, and future reader composition roots
//! cannot drift into incompatible operator contracts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, require_field};

/// Raw `[storage.postgres]` config section.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostgresStorageSection {
    database_url_path: Option<PathBuf>,
    tls: Option<PostgresTlsMode>,
    tls_root_certificate_path: Option<PathBuf>,
}

impl PostgresStorageSection {
    /// Resolves the secret-file and transport contract, rejecting incoherent
    /// combinations before a process attempts a database connection.
    pub fn resolve(self) -> Result<PostgresStorageConfig, ConfigError> {
        let database_url_path =
            require_field(self.database_url_path, "storage.postgres.database_url_path")?;
        let tls = match require_field(self.tls, "storage.postgres.tls")? {
            PostgresTlsMode::VerifyFull => PostgresTlsPolicy::VerifyFull {
                root_certificate_path: require_field(
                    self.tls_root_certificate_path,
                    "storage.postgres.tls_root_certificate_path",
                )?,
            },
            PostgresTlsMode::LoopbackPlaintext => {
                if self.tls_root_certificate_path.is_some() {
                    return Err(ConfigError::invalid(
                        "storage.postgres.tls_root_certificate_path is only valid when storage.postgres.tls = \"verify-full\"",
                    ));
                }
                PostgresTlsPolicy::LoopbackPlaintext
            }
        };
        Ok(PostgresStorageConfig {
            database_url_path,
            tls,
        })
    }
}

/// Resolved shared `PostgreSQL` storage configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresStorageConfig {
    database_url_path: PathBuf,
    tls: PostgresTlsPolicy,
}

impl PostgresStorageConfig {
    /// Returns the secret file containing the `PostgreSQL` connection URI.
    #[must_use]
    pub fn database_url_path(&self) -> &Path {
        &self.database_url_path
    }

    /// Returns the admitted database transport posture.
    #[must_use]
    pub const fn tls(&self) -> &PostgresTlsPolicy {
        &self.tls
    }

    /// Consumes the config into the values required by a database client.
    #[must_use]
    pub fn into_parts(self) -> (PathBuf, PostgresTlsPolicy) {
        (self.database_url_path, self.tls)
    }
}

/// Stable operator-facing `PostgreSQL` TLS mode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostgresTlsMode {
    /// Require encryption plus certificate-chain and hostname verification.
    VerifyFull,
    /// Allow plaintext only when every database host is loopback-local.
    LoopbackPlaintext,
}

/// Resolved `PostgreSQL` transport policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PostgresTlsPolicy {
    /// Require encryption and verify the server certificate and hostname.
    VerifyFull {
        /// Path to a PEM file containing the trusted root certificate chain.
        root_certificate_path: PathBuf,
    },
    /// Explicit unencrypted transport restricted to loopback tests.
    LoopbackPlaintext,
}

/// Redacted TOML projection of `[storage.postgres]` for `--print-config`.
#[derive(Debug, Serialize)]
pub struct PostgresStorageToml {
    database_url_path: &'static str,
    tls: PostgresTlsMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_root_certificate_path: Option<String>,
}

impl PostgresStorageToml {
    /// Builds a redacted rendering without exposing the connection secret path.
    #[must_use]
    pub fn redacted(config: &PostgresStorageConfig) -> Self {
        let (tls, tls_root_certificate_path) = match config.tls() {
            PostgresTlsPolicy::VerifyFull {
                root_certificate_path,
            } => (
                PostgresTlsMode::VerifyFull,
                Some(root_certificate_path.display().to_string()),
            ),
            PostgresTlsPolicy::LoopbackPlaintext => (PostgresTlsMode::LoopbackPlaintext, None),
        };
        Self {
            database_url_path: "[REDACTED]",
            tls,
            tls_root_certificate_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PostgresStorageConfig, PostgresStorageSection, PostgresStorageToml, PostgresTlsPolicy,
    };

    #[test]
    fn verify_full_requires_and_preserves_the_root_certificate_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let section: PostgresStorageSection = toml::from_str(
            r#"
database_url_path = "/run/secrets/database-url"
tls = "verify-full"
tls_root_certificate_path = "/run/config/postgres-ca.pem"
"#,
        )?;

        assert_eq!(
            section.resolve()?,
            PostgresStorageConfig {
                database_url_path: "/run/secrets/database-url".into(),
                tls: PostgresTlsPolicy::VerifyFull {
                    root_certificate_path: "/run/config/postgres-ca.pem".into(),
                },
            }
        );
        Ok(())
    }

    #[test]
    fn loopback_plaintext_rejects_a_root_certificate_path() -> Result<(), Box<dyn std::error::Error>>
    {
        let section: PostgresStorageSection = toml::from_str(
            r#"
database_url_path = "/run/secrets/database-url"
tls = "loopback-plaintext"
tls_root_certificate_path = "/run/config/postgres-ca.pem"
"#,
        )?;

        let error = section.resolve().err().ok_or("invalid TLS config passed")?;
        assert!(error.to_string().contains(
            "tls_root_certificate_path is only valid when storage.postgres.tls = \"verify-full\""
        ));
        Ok(())
    }

    #[test]
    fn unknown_postgres_storage_fields_fail_closed() {
        let result = toml::from_str::<PostgresStorageSection>(
            r#"
database_url_path = "/run/secrets/database-url"
tls = "loopback-plaintext"
pool_size = 100
"#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn tls_mode_names_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let verify_full = PostgresStorageConfig {
            database_url_path: "/run/secrets/database-url".into(),
            tls: PostgresTlsPolicy::VerifyFull {
                root_certificate_path: "/run/config/postgres-ca.pem".into(),
            },
        };
        let loopback_plaintext = PostgresStorageConfig {
            database_url_path: "/run/secrets/database-url".into(),
            tls: PostgresTlsPolicy::LoopbackPlaintext,
        };

        assert!(
            toml::to_string(&PostgresStorageToml::redacted(&verify_full))?
                .contains("tls = \"verify-full\"")
        );
        assert!(
            toml::to_string(&PostgresStorageToml::redacted(&loopback_plaintext))?
                .contains("tls = \"loopback-plaintext\"")
        );
        Ok(())
    }
}
