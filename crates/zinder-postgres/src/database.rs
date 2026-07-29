use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_postgres::{
    Client, Config, NoTls,
    config::{Host, SslMode},
};
use tokio_postgres_rustls::MakeRustlsConnect;
use zinder_runtime::PostgresTlsPolicy;

const MAX_CONNECTION_URI_BYTES: usize = 16 * 1024;
const MAX_ROOT_CERTIFICATE_BYTES: usize = 4 * 1024 * 1024;

/// Shared connection and identity input for one Zinder `PostgreSQL` database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseConfig {
    pub(crate) connection_uri_path: PathBuf,
    pub(crate) tls_policy: PostgresTlsPolicy,
    pub(crate) network: zinder_core::Network,
    pub(crate) application_name: &'static str,
}

impl DatabaseConfig {
    /// Creates database input from a secret-file URI, explicit TLS policy,
    /// and the one Zcash network this database represents.
    #[must_use]
    pub const fn new(
        connection_uri_path: PathBuf,
        tls_policy: PostgresTlsPolicy,
        network: zinder_core::Network,
        application_name: &'static str,
    ) -> Self {
        Self {
            connection_uri_path,
            tls_policy,
            network,
            application_name,
        }
    }
}

/// `PostgreSQL` data-plane failure with connection secrets omitted.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DatabaseError {
    /// The connection URI file could not be read.
    #[error("database connection URI file is unavailable")]
    ConnectionUriFileUnavailable {
        /// Secret-file path retained for structured diagnosis.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The connection URI secret is empty or unreasonably large.
    #[error("database connection URI file must contain 1..={max_bytes} bytes")]
    InvalidConnectionUriFile {
        /// Secret-file path retained for structured diagnosis.
        path: PathBuf,
        /// Maximum admitted secret size.
        max_bytes: usize,
    },
    /// The connection URI could not be parsed.
    #[error("database connection URI file contains an invalid PostgreSQL URI")]
    InvalidConnectionUri {
        /// Secret-file path retained for structured diagnosis.
        path: PathBuf,
    },
    /// A plaintext posture named a host that is not explicitly local.
    #[error("loopback plaintext database transport requires only loopback hosts")]
    PlaintextHostNotLoopback,
    /// The configured root certificate file could not be read.
    #[error("database root certificate file {path:?} is unavailable")]
    RootCertificateFileUnavailable {
        /// Root certificate path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The configured root certificate file is empty or unreasonably large.
    #[error("database root certificate file {path:?} must contain 1..={max_bytes} bytes")]
    InvalidRootCertificateFile {
        /// Root certificate path.
        path: PathBuf,
        /// Maximum admitted certificate bundle size.
        max_bytes: usize,
    },
    /// The configured root certificate file is not valid PEM.
    #[error("database root certificate file {path:?} contains invalid PEM")]
    InvalidRootCertificatePem {
        /// Root certificate path.
        path: PathBuf,
        /// Underlying PEM decoding failure.
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    /// A certificate in the configured trust roots is not a valid trust anchor.
    #[error("database root certificate file {path:?} contains an invalid trust anchor")]
    InvalidRootCertificate {
        /// Root certificate path.
        path: PathBuf,
        /// Underlying certificate validation failure.
        #[source]
        source: rustls::Error,
    },
    /// The `PostgreSQL` operation failed.
    #[error("database operation {operation} failed (SQLSTATE {sqlstate})")]
    Operation {
        /// Stable operation label without SQL or connection material.
        operation: &'static str,
        /// `PostgreSQL` SQLSTATE, or `unavailable` for transport failures.
        sqlstate: String,
        /// Underlying driver failure retained as an error source.
        #[source]
        source: tokio_postgres::Error,
    },
    /// The asynchronous `PostgreSQL` connection task could not be joined.
    #[error("database connection task failed")]
    ConnectionTask {
        /// Underlying task failure.
        #[source]
        source: tokio::task::JoinError,
    },
}

pub(crate) struct DatabaseConnection {
    pub(crate) client: Client,
    connection_task: JoinHandle<Result<(), tokio_postgres::Error>>,
}

impl DatabaseConnection {
    pub(crate) async fn connect(database_config: &DatabaseConfig) -> Result<Self, DatabaseError> {
        let mut config = read_connection_config(&database_config.connection_uri_path)?;
        config
            .application_name(database_config.application_name)
            .connect_timeout(Duration::from_secs(5));
        match &database_config.tls_policy {
            PostgresTlsPolicy::VerifyFull {
                root_certificate_path,
            } => {
                let root_certificates = read_root_certificates(root_certificate_path)?;
                let tls_config = ClientConfig::builder()
                    .with_root_certificates(root_certificates)
                    .with_no_client_auth();
                config.ssl_mode(SslMode::Require);
                let (client, connection) = config
                    .connect(MakeRustlsConnect::new(tls_config))
                    .await
                    .map_err(|source| DatabaseError::operation("connect", source))?;
                let connection_task = tokio::spawn(connection);
                Ok(Self {
                    client,
                    connection_task,
                })
            }
            PostgresTlsPolicy::LoopbackPlaintext => {
                validate_loopback_hosts(&config)?;
                config.ssl_mode(SslMode::Disable);
                let (client, connection) = config
                    .connect(NoTls)
                    .await
                    .map_err(|source| DatabaseError::operation("connect", source))?;
                let connection_task = tokio::spawn(connection);
                Ok(Self {
                    client,
                    connection_task,
                })
            }
        }
    }

    pub(crate) async fn close(self) -> Result<(), DatabaseError> {
        let Self {
            client,
            connection_task,
        } = self;
        drop(client);
        connection_task
            .await
            .map_err(|source| DatabaseError::ConnectionTask { source })?
            .map_err(|source| DatabaseError::operation("connection close", source))
    }
}

impl DatabaseError {
    pub(crate) fn operation(operation: &'static str, source: tokio_postgres::Error) -> Self {
        let sqlstate = source
            .code()
            .map_or_else(|| "unavailable".to_owned(), |code| code.code().to_owned());
        Self::Operation {
            operation,
            sqlstate,
            source,
        }
    }
}

fn read_connection_config(connection_uri_path: &Path) -> Result<Config, DatabaseError> {
    let connection_uri = fs::read_to_string(connection_uri_path).map_err(|source| {
        DatabaseError::ConnectionUriFileUnavailable {
            path: connection_uri_path.to_path_buf(),
            source,
        }
    })?;
    let connection_uri = connection_uri.trim();
    if connection_uri.is_empty() || connection_uri.len() > MAX_CONNECTION_URI_BYTES {
        return Err(DatabaseError::InvalidConnectionUriFile {
            path: connection_uri_path.to_path_buf(),
            max_bytes: MAX_CONNECTION_URI_BYTES,
        });
    }
    Config::from_str(connection_uri).map_err(|_| DatabaseError::InvalidConnectionUri {
        path: connection_uri_path.to_path_buf(),
    })
}

fn read_root_certificates(path: &Path) -> Result<RootCertStore, DatabaseError> {
    let pem = fs::read(path).map_err(|source| DatabaseError::RootCertificateFileUnavailable {
        path: path.to_path_buf(),
        source,
    })?;
    if pem.is_empty() || pem.len() > MAX_ROOT_CERTIFICATE_BYTES {
        return Err(DatabaseError::InvalidRootCertificateFile {
            path: path.to_path_buf(),
            max_bytes: MAX_ROOT_CERTIFICATE_BYTES,
        });
    }
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| DatabaseError::InvalidRootCertificatePem {
            path: path.to_path_buf(),
            source,
        })?;
    if certificates.is_empty() {
        return Err(DatabaseError::InvalidRootCertificateFile {
            path: path.to_path_buf(),
            max_bytes: MAX_ROOT_CERTIFICATE_BYTES,
        });
    }
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|source| DatabaseError::InvalidRootCertificate {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(roots)
}

fn validate_loopback_hosts(config: &Config) -> Result<(), DatabaseError> {
    let all_hosts_are_loopback = !config.get_hosts().is_empty()
        && config.get_hosts().iter().all(|host| match host {
            Host::Tcp(host) => {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }
            #[cfg(unix)]
            Host::Unix(_) => true,
        });
    let all_host_addresses_are_loopback = config
        .get_hostaddrs()
        .iter()
        .all(std::net::IpAddr::is_loopback);
    if all_hosts_are_loopback && all_host_addresses_are_loopback {
        Ok(())
    } else {
        Err(DatabaseError::PlaintextHostNotLoopback)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        DatabaseError, read_connection_config, read_root_certificates, validate_loopback_hosts,
    };

    #[test]
    fn connection_uri_parse_errors_do_not_expose_secret_contents()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let path = tempdir.path().join("database-uri");
        let secret = "not a valid URI containing super-secret-value";
        fs::write(&path, secret)?;

        let Err(error) = read_connection_config(&path) else {
            return Err("invalid URI was accepted".into());
        };
        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains("invalid PostgreSQL URI"));
        Ok(())
    }

    #[test]
    fn plaintext_transport_rejects_non_loopback_hosts() -> Result<(), Box<dyn std::error::Error>> {
        let loopback = read_config("postgresql://user:secret@127.0.0.1/database")?;
        validate_loopback_hosts(&loopback)?;

        let remote = read_config("postgresql://user:secret@database.example/database")?;
        assert!(matches!(
            validate_loopback_hosts(&remote),
            Err(DatabaseError::PlaintextHostNotLoopback)
        ));
        let remote_host_address =
            read_config("postgresql://user:secret@localhost/database?hostaddr=203.0.113.10")?;
        assert!(matches!(
            validate_loopback_hosts(&remote_host_address),
            Err(DatabaseError::PlaintextHostNotLoopback)
        ));
        Ok(())
    }

    #[test]
    fn verified_transport_rejects_empty_root_certificate_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let path = tempdir.path().join("root-certificate.pem");
        fs::write(&path, [])?;

        assert!(matches!(
            read_root_certificates(&path),
            Err(DatabaseError::InvalidRootCertificateFile { .. })
        ));
        Ok(())
    }

    fn read_config(connection_uri: &str) -> Result<tokio_postgres::Config, DatabaseError> {
        let tempdir = tempdir().map_err(|source| DatabaseError::ConnectionUriFileUnavailable {
            path: "temporary-directory".into(),
            source,
        })?;
        let path = tempdir.path().join("database-uri");
        fs::write(&path, connection_uri).map_err(|source| {
            DatabaseError::ConnectionUriFileUnavailable {
                path: path.clone(),
                source,
            }
        })?;
        read_connection_config(&path)
    }
}
