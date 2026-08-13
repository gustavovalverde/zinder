//! Client-owned wallet endpoint metadata.

use zinder_core::{ArtifactSchemaVersion, Network};

use crate::{Capability, CapabilityDescriptor};

/// Validated structural claim naming one canonical construction sidecar.
///
/// This value is descriptive evidence only. A composition root that owns an
/// admitted canonical reader must exact-compare it with that authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalConstructionManifestBinding {
    format_version: u16,
    sha256: [u8; 32],
}

impl CanonicalConstructionManifestBinding {
    pub(crate) const fn from_validated_fields(format_version: u16, sha256: [u8; 32]) -> Self {
        Self {
            format_version,
            sha256,
        }
    }

    /// Returns the exact immutable sidecar format version.
    #[must_use]
    pub const fn format_version(self) -> u16 {
        self.format_version
    }

    /// Returns the exact immutable sidecar digest.
    #[must_use]
    pub const fn sha256(self) -> [u8; 32] {
        self.sha256
    }
}

/// Upstream node metadata captured by the query endpoint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct NodeServerInfo {
    /// Node-reported semantic version, when available.
    pub version: Option<String>,
    /// Exact upstream-node capability strings.
    pub capabilities: Vec<String>,
}

/// Wallet endpoint identity, capability, and schema metadata.
///
/// This is a client-owned representation. Generated protobuf messages remain
/// private to the remote transport conversion.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ServerInfo {
    /// Network served by the endpoint.
    pub network: Network,
    /// Stable service identifier, such as `zinder-query`.
    pub service_name: String,
    /// Service semantic version.
    pub service_version: String,
    /// Wallet capabilities, including unknown additive values.
    pub capabilities: Vec<Capability>,
    /// Monotonic native contract revision.
    pub contract_revision: u32,
    /// Closed materialized-view preset, when a view store is attached.
    pub materialized_view_preset: Option<String>,
    /// Stable materialized-view identities selected by the preset.
    pub materialized_view_identities: Vec<String>,
    /// Full source commit embedded by the release build.
    pub build_git_commit: String,
    /// Canonical artifact schema version.
    pub schema_version: ArtifactSchemaVersion,
    /// Configured canonical reorg window depth in blocks.
    pub reorg_window_blocks: u32,
    /// Validated canonical construction claim, when the endpoint can prove one.
    pub canonical_construction_manifest_binding: Option<CanonicalConstructionManifestBinding>,
    /// Upstream node metadata, when the endpoint has a source snapshot.
    pub node: Option<NodeServerInfo>,
}

impl CapabilityDescriptor for ServerInfo {
    fn has(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|advertised| advertised.as_str() == capability)
    }
}
