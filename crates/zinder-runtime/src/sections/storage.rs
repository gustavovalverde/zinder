//! Shared `[storage]` config section.
//!
//! Two shapes ship to mirror the writer/reader split documented in
//! [ADR-0003](../../../../docs/adrs/0003-canonical-storage-access-boundary.md):
//!
//! - [`PrimaryStorageSection`] for the writer (`zinder-ingest`); a single
//!   storage path.
//! - [`SecondaryStorageSection`] for the readers (`zinder-query`,
//!   `zinder-compat-lightwalletd`); the writer path plus the reader's
//!   local secondary path and catchup tuning.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, config::duration_as_millis_u64};

const DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS: u64 = 250;
const DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS: u64 = 4;

/// Raw `[storage]` section consumed by the writer binary.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrimaryStorageSection {
    /// Filesystem path of the canonical (primary) `RocksDB` instance.
    pub path: Option<PathBuf>,
}

/// Raw `[storage]` section consumed by reader binaries.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecondaryStorageSection {
    /// Filesystem path of the canonical primary instance the reader opens
    /// as a `RocksDB` secondary.
    pub path: Option<PathBuf>,
    /// Reader-local secondary metadata path; must be unique per reader
    /// process per ADR-0003.
    pub secondary_path: Option<PathBuf>,
    /// Catchup tick cadence in milliseconds.
    pub secondary_catchup_interval_ms: Option<u64>,
    /// Replica-lag threshold in chain epochs. Crossing this threshold
    /// flips readiness to [`crate::ReadinessCause::ReplicaLagging`].
    pub secondary_replica_lag_threshold_chain_epochs: Option<u64>,
}

/// Resolved primary storage location.
#[derive(Clone, Debug)]
pub struct ResolvedPrimaryStorage {
    /// Filesystem path of the canonical instance.
    pub path: PathBuf,
}

/// Resolved secondary storage location with catchup tuning.
#[derive(Clone, Debug)]
pub struct ResolvedSecondaryStorage {
    /// Canonical primary path the reader opens as secondary.
    pub path: PathBuf,
    /// Reader-local secondary metadata path.
    pub secondary_path: PathBuf,
    /// Catchup tick cadence.
    pub secondary_catchup_interval: Duration,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
}

/// Validates and resolves a [`PrimaryStorageSection`].
pub fn resolve_primary_storage(
    section: PrimaryStorageSection,
) -> Result<ResolvedPrimaryStorage, ConfigError> {
    let path = section
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    Ok(ResolvedPrimaryStorage { path })
}

/// Validates and resolves a [`SecondaryStorageSection`], applying
/// per-field defaults for `secondary_catchup_interval_ms` and
/// `secondary_replica_lag_threshold_chain_epochs`.
pub fn resolve_secondary_storage(
    section: SecondaryStorageSection,
) -> Result<ResolvedSecondaryStorage, ConfigError> {
    let path = section
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let secondary_path = section
        .secondary_path
        .ok_or_else(|| ConfigError::missing_field("storage.secondary_path"))?;
    let catchup_ms = section
        .secondary_catchup_interval_ms
        .unwrap_or(DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS);
    if catchup_ms == 0 {
        return Err(ConfigError::invalid(
            "storage.secondary_catchup_interval_ms must be greater than zero",
        ));
    }
    let secondary_replica_lag_threshold_chain_epochs = section
        .secondary_replica_lag_threshold_chain_epochs
        .unwrap_or(DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS);
    Ok(ResolvedSecondaryStorage {
        path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(catchup_ms),
        secondary_replica_lag_threshold_chain_epochs,
    })
}

/// Redacted TOML projection of the writer-side `[storage]` section.
#[derive(Debug, Serialize)]
pub struct PrimaryStorageToml {
    /// Filesystem path of the canonical instance.
    pub path: String,
}

impl PrimaryStorageToml {
    /// Builds a [`PrimaryStorageToml`] from a resolved storage location.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedPrimaryStorage) -> Self {
        Self {
            path: resolved.path.display().to_string(),
        }
    }
}

/// Redacted TOML projection of the reader-side `[storage]` section.
#[derive(Debug, Serialize)]
pub struct SecondaryStorageToml {
    /// Canonical primary path.
    pub path: String,
    /// Reader-local secondary metadata path.
    pub secondary_path: String,
    /// Catchup tick cadence in milliseconds.
    pub secondary_catchup_interval_ms: u64,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
}

impl SecondaryStorageToml {
    /// Builds a [`SecondaryStorageToml`] from a resolved reader storage
    /// configuration.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedSecondaryStorage) -> Self {
        Self {
            path: resolved.path.display().to_string(),
            secondary_path: resolved.secondary_path.display().to_string(),
            secondary_catchup_interval_ms: duration_as_millis_u64(
                resolved.secondary_catchup_interval,
            ),
            secondary_replica_lag_threshold_chain_epochs: resolved
                .secondary_replica_lag_threshold_chain_epochs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_storage_resolution_rejects_missing_path() {
        let outcome = resolve_primary_storage(PrimaryStorageSection::default());
        assert!(matches!(
            outcome,
            Err(ConfigError::MissingField {
                field: "storage.path"
            })
        ));
    }

    #[test]
    fn primary_storage_returns_resolved_path() -> Result<(), ConfigError> {
        let resolved = resolve_primary_storage(PrimaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
        })?;
        assert_eq!(resolved.path, PathBuf::from("/tmp/store"));
        Ok(())
    }

    #[test]
    fn secondary_storage_requires_both_paths() {
        let only_primary = SecondaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            ..SecondaryStorageSection::default()
        };
        assert!(matches!(
            resolve_secondary_storage(only_primary),
            Err(ConfigError::MissingField {
                field: "storage.secondary_path"
            })
        ));

        let only_secondary = SecondaryStorageSection {
            secondary_path: Some(PathBuf::from("/tmp/store-secondary")),
            ..SecondaryStorageSection::default()
        };
        assert!(matches!(
            resolve_secondary_storage(only_secondary),
            Err(ConfigError::MissingField {
                field: "storage.path"
            })
        ));
    }

    #[test]
    fn secondary_storage_applies_defaults() -> Result<(), ConfigError> {
        let resolved = resolve_secondary_storage(SecondaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            secondary_path: Some(PathBuf::from("/tmp/store-secondary")),
            ..SecondaryStorageSection::default()
        })?;
        assert_eq!(
            resolved.secondary_catchup_interval,
            Duration::from_millis(DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS)
        );
        assert_eq!(
            resolved.secondary_replica_lag_threshold_chain_epochs,
            DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS
        );
        Ok(())
    }

    #[test]
    fn secondary_storage_rejects_zero_catchup_interval() {
        let outcome = resolve_secondary_storage(SecondaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            secondary_path: Some(PathBuf::from("/tmp/store-secondary")),
            secondary_catchup_interval_ms: Some(0),
            ..SecondaryStorageSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }
}
