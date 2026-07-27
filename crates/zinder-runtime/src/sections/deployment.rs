//! Shared `[deployment]` config section.
//!
//! The deployment topology names an operator-selected production shape. It
//! does not replace the storage configuration owned by each service; it makes
//! the process topology explicit so composition roots can validate that their
//! storage and coordination settings match the selected shape.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, require_field};

/// Stable deployment-shape catalog.
///
/// The config names are stable operator-facing identifiers. Adding a topology
/// requires a new explicit variant rather than inferring it from incidental
/// storage settings. Presence in this catalog does not by itself make a
/// topology production-supported; release documentation owns that claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DeploymentTopology {
    /// One service set backed by local `RocksDB` storage on a single host.
    #[serde(rename = "rocksdb-single-host")]
    RocksDbSingleHost,
    /// Reserved horizontal service shape coordinated through `PostgreSQL`.
    ///
    /// This name is exercised by an unreleased tracer while the complete
    /// topology proceeds through production certification.
    PostgresHorizontal,
}

impl DeploymentTopology {
    /// Returns the stable config name for this deployment topology.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RocksDbSingleHost => "rocksdb-single-host",
            Self::PostgresHorizontal => "postgres-horizontal",
        }
    }

    /// Parses a stable operator-facing topology name.
    #[must_use]
    pub fn parse_config_name(name: &str) -> Option<Self> {
        match name {
            "rocksdb-single-host" => Some(Self::RocksDbSingleHost),
            "postgres-horizontal" => Some(Self::PostgresHorizontal),
            _ => None,
        }
    }
}

impl fmt::Display for DeploymentTopology {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Raw `[deployment]` config section.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeploymentSection {
    /// Operator-selected deployment shape.
    pub topology: Option<DeploymentTopology>,
}

impl DeploymentSection {
    /// Resolves the configured topology.
    ///
    /// Composition roots may apply a service-specific default before calling
    /// this method. An absent value fails closed when no default was applied.
    pub fn resolve(self) -> Result<DeploymentTopology, ConfigError> {
        require_field(self.topology, "deployment.topology")
    }
}

/// TOML projection of `[deployment]` for `--print-config`.
#[derive(Debug, Serialize)]
pub struct DeploymentToml {
    /// Resolved deployment shape.
    pub topology: DeploymentTopology,
}

impl DeploymentToml {
    /// Builds a [`DeploymentToml`] from a resolved topology.
    #[must_use]
    pub const fn from_resolved(topology: DeploymentTopology) -> Self {
        Self { topology }
    }
}
