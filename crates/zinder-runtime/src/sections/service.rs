//! Service identity used by shared section helpers to pick per-service
//! defaults without each binary repeating the table.

use std::fmt;

/// Identity of a deployable Zinder runtime.
///
/// Variants enumerate every binary the workspace ships; adding a new
/// variant forces a compile error in every section's default table so
/// nothing silently defaults to a placeholder. The string returned by
/// [`Self::binary_name`] is stable across releases and used as the
/// `service` label in Prometheus metrics.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServiceIdentifier {
    /// `zinder-ingest`: writer plane.
    Ingest,
    /// `zinder-query`: native query reader plane.
    Query,
    /// `zinder-compat-lightwalletd`: lightwalletd-protocol compat reader.
    CompatLightwalletd,
    /// `zinder-explorer`: explorer-plane derive reader.
    Explorer,
}

impl ServiceIdentifier {
    /// Canonical binary name used in metrics labels, log targets, and
    /// docs. Stable across releases.
    #[must_use]
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::Ingest => "zinder-ingest",
            Self::Query => "zinder-query",
            Self::CompatLightwalletd => "zinder-compat-lightwalletd",
            Self::Explorer => "zinder-explorer",
        }
    }
}

impl fmt::Display for ServiceIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.binary_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_matches_display() {
        for service in [
            ServiceIdentifier::Ingest,
            ServiceIdentifier::Query,
            ServiceIdentifier::CompatLightwalletd,
            ServiceIdentifier::Explorer,
        ] {
            assert_eq!(service.binary_name(), service.to_string());
        }
    }
}
