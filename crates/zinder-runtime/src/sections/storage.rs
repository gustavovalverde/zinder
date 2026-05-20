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
use zinder_store::StorageTuning;

use crate::{ConfigError, config::duration_as_millis_u64};

const DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS: u64 = 250;
const DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS: u64 = 4;

/// Raw `[storage.tuning]` sub-section.
///
/// Surfaces the bounded `RocksDB` resource budget described in
/// [ADR-0020](../../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
/// Operators override any subset of fields; unspecified fields fall through
/// to [`StorageTuning::canonical_defaults`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageTuningSection {
    /// Override [`StorageTuning::block_cache_bytes`].
    pub block_cache_bytes: Option<u64>,
    /// Override [`StorageTuning::max_wal_bytes`].
    pub max_wal_bytes: Option<u64>,
    /// Override [`StorageTuning::max_open_files`].
    pub max_open_files: Option<i32>,
}

impl StorageTuningSection {
    /// Merges any `Some` overrides onto `defaults`. Use
    /// [`StorageTuning::canonical_defaults`] for the canonical store and
    /// [`StorageTuning::derive_defaults`] for the derive store.
    #[must_use]
    pub const fn apply_to(self, mut defaults: StorageTuning) -> StorageTuning {
        if let Some(bytes) = self.block_cache_bytes {
            defaults.block_cache_bytes = bytes;
        }
        if let Some(bytes) = self.max_wal_bytes {
            defaults.max_wal_bytes = bytes;
        }
        if let Some(files) = self.max_open_files {
            defaults.max_open_files = files;
        }
        defaults
    }
}

/// Raw `[storage]` section consumed by the writer binary.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrimaryStorageSection {
    /// Filesystem path of the canonical (primary) `RocksDB` instance.
    pub path: Option<PathBuf>,
    /// `[storage.tuning]` overrides for the bounded `RocksDB` resource budget.
    pub tuning: StorageTuningSection,
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
    /// `[storage.tuning]` overrides for the bounded `RocksDB` resource budget.
    pub tuning: StorageTuningSection,
}

/// Resolved primary storage location.
#[derive(Clone, Debug)]
pub struct ResolvedPrimaryStorage {
    /// Filesystem path of the canonical instance.
    pub path: PathBuf,
    /// Bounded `RocksDB` resource budget applied at open time.
    pub tuning: StorageTuning,
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
    /// Bounded `RocksDB` resource budget applied at open time.
    pub tuning: StorageTuning,
}

/// Merges `section` overrides onto [`StorageTuning::canonical_defaults`].
#[must_use]
pub const fn resolve_storage_tuning(section: StorageTuningSection) -> StorageTuning {
    section.apply_to(StorageTuning::canonical_defaults())
}

/// Validates and resolves a [`PrimaryStorageSection`].
pub fn resolve_primary_storage(
    section: PrimaryStorageSection,
) -> Result<ResolvedPrimaryStorage, ConfigError> {
    let path = section
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    Ok(ResolvedPrimaryStorage {
        path,
        tuning: resolve_storage_tuning(section.tuning),
    })
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
        tuning: resolve_storage_tuning(section.tuning),
    })
}

/// Redacted TOML projection of `[storage.tuning]`.
#[derive(Debug, Serialize)]
pub struct StorageTuningToml {
    /// Resolved [`StorageTuning::block_cache_bytes`].
    pub block_cache_bytes: u64,
    /// Resolved [`StorageTuning::max_wal_bytes`].
    pub max_wal_bytes: u64,
    /// Resolved [`StorageTuning::max_open_files`].
    pub max_open_files: i32,
}

impl StorageTuningToml {
    /// Builds a [`StorageTuningToml`] from a resolved tuning value.
    #[must_use]
    pub const fn from_resolved(tuning: StorageTuning) -> Self {
        Self {
            block_cache_bytes: tuning.block_cache_bytes,
            max_wal_bytes: tuning.max_wal_bytes,
            max_open_files: tuning.max_open_files,
        }
    }
}

/// Redacted TOML projection of the writer-side `[storage]` section.
#[derive(Debug, Serialize)]
pub struct PrimaryStorageToml {
    /// Filesystem path of the canonical instance.
    pub path: String,
    /// Resolved tuning projection.
    pub tuning: StorageTuningToml,
}

impl PrimaryStorageToml {
    /// Builds a [`PrimaryStorageToml`] from a resolved storage location.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedPrimaryStorage) -> Self {
        Self {
            path: resolved.path.display().to_string(),
            tuning: StorageTuningToml::from_resolved(resolved.tuning),
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
    /// Resolved tuning projection.
    pub tuning: StorageTuningToml,
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
            tuning: StorageTuningToml::from_resolved(resolved.tuning),
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
            tuning: StorageTuningSection::default(),
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

    #[test]
    fn tuning_resolution_falls_through_to_canonical_defaults() {
        let resolved = resolve_storage_tuning(StorageTuningSection::default());
        assert_eq!(resolved, StorageTuning::canonical_defaults());
    }

    #[test]
    fn tuning_resolution_applies_individual_overrides() {
        let resolved = resolve_storage_tuning(StorageTuningSection {
            block_cache_bytes: Some(1024),
            max_wal_bytes: None,
            max_open_files: Some(256),
        });
        let defaults = StorageTuning::canonical_defaults();
        assert_eq!(resolved.block_cache_bytes, 1024);
        assert_eq!(resolved.max_wal_bytes, defaults.max_wal_bytes);
        assert_eq!(resolved.max_open_files, 256);
    }

    #[test]
    fn primary_storage_carries_resolved_tuning() -> Result<(), ConfigError> {
        let resolved = resolve_primary_storage(PrimaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            tuning: StorageTuningSection {
                max_wal_bytes: Some(123 * 1024 * 1024),
                ..StorageTuningSection::default()
            },
        })?;
        assert_eq!(resolved.tuning.max_wal_bytes, 123 * 1024 * 1024);
        Ok(())
    }
}
