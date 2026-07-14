//! Shared `[storage]` config section.
//!
//! Two shapes ship to mirror the writer/reader split documented in
//! [ADR-0003](../../../../docs/adrs/0003-canonical-storage-access-boundary.md):
//!
//! - [`PrimaryStorageSection`] for the writer (`zinder-ingest`); a single
//!   storage path.
//! - [`SecondaryStorageSection`] for readers that open both canonical and
//!   derive stores (`zinder-query`, `zinder-explorer`).
//! - [`CanonicalSecondaryStorageSection`] for readers that open only the
//!   canonical store (`zinder-compat-lightwalletd`).

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use zinder_store::{RocksDbResourceBudget, RocksDbStatisticsLevel};

use crate::{
    ConfigError, canonical_reader_block_cache_bytes, canonical_reader_max_open_files,
    config::duration_as_millis_u64,
};

const DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS: u64 = 1_000;
const DEFAULT_INITIAL_CATCHUP_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS: u64 = 4;

/// Raw role-scoped `rocksdb` sub-section.
///
/// Surfaces the bounded `RocksDB` resource budget described in
/// [ADR-0020](../../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
/// Operators override any subset of fields; unspecified fields fall through
/// to the role default selected by the resolver.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RocksDbResourceBudgetSection {
    /// Override [`RocksDbResourceBudget::block_cache_bytes`].
    pub block_cache_bytes: Option<u64>,
    /// Override [`RocksDbResourceBudget::max_wal_bytes`].
    pub max_wal_bytes: Option<u64>,
    /// Override [`RocksDbResourceBudget::max_open_files`].
    pub max_open_files: Option<i32>,
    /// Override [`RocksDbResourceBudget::write_buffer_bytes`].
    pub write_buffer_bytes: Option<u64>,
    /// Override [`RocksDbResourceBudget::max_write_buffer_count`].
    pub max_write_buffer_count: Option<i32>,
    /// Override the primary-writer
    /// [`RocksDbResourceBudget::max_background_jobs`] limit. Secondary opens
    /// retain this field in their uniform budget but do not apply it.
    pub max_background_jobs: Option<i32>,
    /// Override [`RocksDbResourceBudget::memtable_budget_bytes`].
    pub memtable_budget_bytes: Option<u64>,
    /// Override [`RocksDbResourceBudget::statistics_level`]: `off`,
    /// `tickers`, or `full`.
    pub statistics_level: Option<String>,
}

impl RocksDbResourceBudgetSection {
    /// Merges any `Some` overrides onto `defaults`. Use writer or reader
    /// defaults for the selected store posture.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] when `statistics_level` is set to a
    /// value other than `off`, `tickers`, or `full`.
    pub fn apply_to(
        self,
        mut defaults: RocksDbResourceBudget,
        path: &'static str,
    ) -> Result<RocksDbResourceBudget, ConfigError> {
        if let Some(bytes) = self.block_cache_bytes {
            defaults.block_cache_bytes = bytes;
        }
        if let Some(bytes) = self.max_wal_bytes {
            defaults.max_wal_bytes = bytes;
        }
        if let Some(files) = self.max_open_files {
            defaults.max_open_files = files;
        }
        if let Some(bytes) = self.write_buffer_bytes {
            defaults.write_buffer_bytes = bytes;
        }
        if let Some(count) = self.max_write_buffer_count {
            defaults.max_write_buffer_count = count;
        }
        if let Some(jobs) = self.max_background_jobs {
            defaults.max_background_jobs = jobs;
        }
        if let Some(bytes) = self.memtable_budget_bytes {
            defaults.memtable_budget_bytes = bytes;
        }
        if let Some(level_text) = self.statistics_level {
            defaults.statistics_level = RocksDbStatisticsLevel::parse(&level_text)
                .ok_or_else(|| {
                    ConfigError::invalid(format!(
                        "{path}.statistics_level must be one of off, tickers, full; got {level_text:?}"
                    ))
                })?;
        }
        Ok(defaults)
    }
}

/// Raw storage-role section consumed as `[storage.canonical]` and
/// `[storage.derive]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageRoleSection {
    /// `RocksDB` resource budget overrides for this storage role.
    pub rocksdb: RocksDbResourceBudgetSection,
}

/// Raw `[storage]` section consumed by the writer binary.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrimaryStorageSection {
    /// Filesystem path of the canonical (primary) `RocksDB` instance.
    pub path: Option<PathBuf>,
    /// Canonical store role budget.
    pub canonical: StorageRoleSection,
    /// Derive store role budget.
    pub derive: StorageRoleSection,
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
    /// Maximum startup catchup duration in milliseconds before the reader
    /// proceeds with the opened secondary and lets readiness report lag.
    pub initial_catchup_timeout_ms: Option<u64>,
    /// Replica-lag threshold in chain epochs. Crossing this threshold
    /// flips readiness to [`crate::ReadinessCause::ReplicaLagging`].
    pub secondary_replica_lag_threshold_chain_epochs: Option<u64>,
    /// Canonical store role budget.
    pub canonical: StorageRoleSection,
    /// Derive store role budget.
    pub derive: StorageRoleSection,
}

/// Raw `[storage]` section consumed by reader binaries that only open the
/// canonical store as a `RocksDB` secondary.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CanonicalSecondaryStorageSection {
    /// Filesystem path of the canonical primary instance the reader opens
    /// as a `RocksDB` secondary.
    pub path: Option<PathBuf>,
    /// Reader-local secondary metadata path; must be unique per reader
    /// process per ADR-0003.
    pub secondary_path: Option<PathBuf>,
    /// Catchup tick cadence in milliseconds.
    pub secondary_catchup_interval_ms: Option<u64>,
    /// Maximum startup catchup duration in milliseconds before the reader
    /// proceeds with the opened secondary and lets readiness report lag.
    pub initial_catchup_timeout_ms: Option<u64>,
    /// Replica-lag threshold in chain epochs. Crossing this threshold
    /// flips readiness to [`crate::ReadinessCause::ReplicaLagging`].
    pub secondary_replica_lag_threshold_chain_epochs: Option<u64>,
    /// Canonical store role budget.
    pub canonical: StorageRoleSection,
}

/// Resolved primary storage location.
#[derive(Clone, Debug)]
pub struct ResolvedPrimaryStorage {
    /// Filesystem path of the canonical instance.
    pub path: PathBuf,
    /// Bounded `RocksDB` resource budget for the canonical store.
    pub canonical_rocksdb_budget: RocksDbResourceBudget,
    /// Bounded `RocksDB` resource budget for the derive store.
    pub derive_rocksdb_budget: RocksDbResourceBudget,
}

/// Resolved secondary storage location with catchup cadence.
#[derive(Clone, Debug)]
pub struct ResolvedSecondaryStorage {
    /// Canonical primary path the reader opens as secondary.
    pub path: PathBuf,
    /// Reader-local secondary metadata path.
    pub secondary_path: PathBuf,
    /// Catchup tick cadence.
    pub secondary_catchup_interval: Duration,
    /// Maximum startup catchup duration before serving with the opened secondary.
    pub initial_catchup_timeout: Duration,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
    /// Bounded `RocksDB` resource budget for the canonical store.
    pub canonical_rocksdb_budget: RocksDbResourceBudget,
    /// Bounded `RocksDB` resource budget for the derive store.
    pub derive_rocksdb_budget: RocksDbResourceBudget,
}

/// Resolved canonical-secondary storage location with catchup cadence.
#[derive(Clone, Debug)]
pub struct ResolvedCanonicalSecondaryStorage {
    /// Canonical primary path the reader opens as secondary.
    pub path: PathBuf,
    /// Reader-local secondary metadata path.
    pub secondary_path: PathBuf,
    /// Catchup tick cadence.
    pub secondary_catchup_interval: Duration,
    /// Maximum startup catchup duration before serving with the opened secondary.
    pub initial_catchup_timeout: Duration,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
    /// Bounded `RocksDB` resource budget for the canonical store.
    pub canonical_rocksdb_budget: RocksDbResourceBudget,
}

fn resolve_rocksdb_resource_budget(
    section: RocksDbResourceBudgetSection,
    defaults: RocksDbResourceBudget,
    path: &'static str,
) -> Result<RocksDbResourceBudget, ConfigError> {
    let budget = section.apply_to(defaults, path)?;
    budget
        .validate()
        .map_err(|reason| ConfigError::invalid(format!("{path}: {reason}")))?;
    Ok(budget)
}

/// Merges canonical-role overrides onto
/// [`RocksDbResourceBudget::canonical_writer_defaults`].
pub fn resolve_canonical_writer_rocksdb_budget(
    section: RocksDbResourceBudgetSection,
) -> Result<RocksDbResourceBudget, ConfigError> {
    resolve_rocksdb_resource_budget(
        section,
        RocksDbResourceBudget::canonical_writer_defaults(),
        "storage.canonical.rocksdb",
    )
}

/// Merges derive-role overrides onto [`RocksDbResourceBudget::derive_writer_defaults`].
pub fn resolve_derive_writer_rocksdb_budget(
    section: RocksDbResourceBudgetSection,
) -> Result<RocksDbResourceBudget, ConfigError> {
    resolve_rocksdb_resource_budget(
        section,
        RocksDbResourceBudget::derive_writer_defaults(),
        "storage.derive.rocksdb",
    )
}

/// Merges canonical-role overrides onto container-aware reader defaults.
///
/// The base defaults derive `block_cache_bytes` as `min(container_memory / 8, 512 MiB)`
/// (floor 128 MiB when no cgroup limit is detectable) and `max_open_files`
/// proportionally. All other fields come from
/// [`RocksDbResourceBudget::canonical_reader_defaults`]. Explicit overrides in
/// `section` win over the derived values.
pub fn resolve_canonical_reader_rocksdb_budget(
    section: RocksDbResourceBudgetSection,
) -> Result<RocksDbResourceBudget, ConfigError> {
    let mut defaults = RocksDbResourceBudget::canonical_reader_defaults();
    defaults.block_cache_bytes = canonical_reader_block_cache_bytes();
    defaults.max_open_files = canonical_reader_max_open_files();
    resolve_rocksdb_resource_budget(section, defaults, "storage.canonical.rocksdb")
}

/// Merges derive-role overrides onto [`RocksDbResourceBudget::derive_reader_defaults`].
pub fn resolve_derive_reader_rocksdb_budget(
    section: RocksDbResourceBudgetSection,
) -> Result<RocksDbResourceBudget, ConfigError> {
    resolve_rocksdb_resource_budget(
        section,
        RocksDbResourceBudget::derive_reader_defaults(),
        "storage.derive.rocksdb",
    )
}

/// Validates and resolves a [`PrimaryStorageSection`].
pub fn resolve_primary_storage(
    section: PrimaryStorageSection,
) -> Result<ResolvedPrimaryStorage, ConfigError> {
    let path = section
        .path
        .ok_or_else(|| ConfigError::missing_field("storage.path"))?;
    let canonical_rocksdb_budget =
        resolve_canonical_writer_rocksdb_budget(section.canonical.rocksdb)?;
    let derive_rocksdb_budget = resolve_derive_writer_rocksdb_budget(section.derive.rocksdb)?;
    Ok(ResolvedPrimaryStorage {
        path,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
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
    let initial_catchup_timeout_ms = section
        .initial_catchup_timeout_ms
        .unwrap_or(DEFAULT_INITIAL_CATCHUP_TIMEOUT_MS);
    if initial_catchup_timeout_ms == 0 {
        return Err(ConfigError::invalid(
            "storage.initial_catchup_timeout_ms must be greater than zero",
        ));
    }
    let secondary_replica_lag_threshold_chain_epochs = section
        .secondary_replica_lag_threshold_chain_epochs
        .unwrap_or(DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS);
    let canonical_rocksdb_budget =
        resolve_canonical_reader_rocksdb_budget(section.canonical.rocksdb)?;
    let derive_rocksdb_budget = resolve_derive_reader_rocksdb_budget(section.derive.rocksdb)?;
    Ok(ResolvedSecondaryStorage {
        path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(catchup_ms),
        initial_catchup_timeout: Duration::from_millis(initial_catchup_timeout_ms),
        secondary_replica_lag_threshold_chain_epochs,
        canonical_rocksdb_budget,
        derive_rocksdb_budget,
    })
}

/// Validates and resolves a [`CanonicalSecondaryStorageSection`], applying
/// per-field defaults for `secondary_catchup_interval_ms` and
/// `secondary_replica_lag_threshold_chain_epochs`.
pub fn resolve_canonical_secondary_storage(
    section: CanonicalSecondaryStorageSection,
) -> Result<ResolvedCanonicalSecondaryStorage, ConfigError> {
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
    let initial_catchup_timeout_ms = section
        .initial_catchup_timeout_ms
        .unwrap_or(DEFAULT_INITIAL_CATCHUP_TIMEOUT_MS);
    if initial_catchup_timeout_ms == 0 {
        return Err(ConfigError::invalid(
            "storage.initial_catchup_timeout_ms must be greater than zero",
        ));
    }
    let secondary_replica_lag_threshold_chain_epochs = section
        .secondary_replica_lag_threshold_chain_epochs
        .unwrap_or(DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS);
    let canonical_rocksdb_budget =
        resolve_canonical_reader_rocksdb_budget(section.canonical.rocksdb)?;
    Ok(ResolvedCanonicalSecondaryStorage {
        path,
        secondary_path,
        secondary_catchup_interval: Duration::from_millis(catchup_ms),
        initial_catchup_timeout: Duration::from_millis(initial_catchup_timeout_ms),
        secondary_replica_lag_threshold_chain_epochs,
        canonical_rocksdb_budget,
    })
}

/// Redacted TOML projection of a role-scoped `rocksdb` resource budget.
#[derive(Debug, Serialize)]
pub struct RocksDbResourceBudgetToml {
    /// Resolved [`RocksDbResourceBudget::block_cache_bytes`].
    pub block_cache_bytes: u64,
    /// Resolved [`RocksDbResourceBudget::max_wal_bytes`].
    pub max_wal_bytes: u64,
    /// Resolved [`RocksDbResourceBudget::max_open_files`].
    pub max_open_files: i32,
    /// Resolved [`RocksDbResourceBudget::write_buffer_bytes`].
    pub write_buffer_bytes: u64,
    /// Resolved [`RocksDbResourceBudget::max_write_buffer_count`].
    pub max_write_buffer_count: i32,
    /// Resolved primary-writer [`RocksDbResourceBudget::max_background_jobs`]
    /// limit. Secondary opens do not apply it.
    pub max_background_jobs: i32,
    /// Resolved [`RocksDbResourceBudget::memtable_budget_bytes`].
    pub memtable_budget_bytes: u64,
    /// Resolved [`RocksDbResourceBudget::statistics_level`]: `off`,
    /// `tickers`, or `full`.
    pub statistics_level: &'static str,
}

impl RocksDbResourceBudgetToml {
    /// Builds a [`RocksDbResourceBudgetToml`] from a resolved budget.
    #[must_use]
    pub const fn from_resolved(budget: RocksDbResourceBudget) -> Self {
        Self {
            block_cache_bytes: budget.block_cache_bytes,
            max_wal_bytes: budget.max_wal_bytes,
            max_open_files: budget.max_open_files,
            write_buffer_bytes: budget.write_buffer_bytes,
            max_write_buffer_count: budget.max_write_buffer_count,
            max_background_jobs: budget.max_background_jobs,
            memtable_budget_bytes: budget.memtable_budget_bytes,
            statistics_level: budget.statistics_level.as_str(),
        }
    }
}

/// Redacted TOML projection of a storage role section.
#[derive(Debug, Serialize)]
pub struct StorageRoleToml {
    /// Resolved `RocksDB` budget projection.
    pub rocksdb: RocksDbResourceBudgetToml,
}

impl StorageRoleToml {
    /// Builds a [`StorageRoleToml`] from a resolved budget.
    #[must_use]
    pub const fn from_resolved(budget: RocksDbResourceBudget) -> Self {
        Self {
            rocksdb: RocksDbResourceBudgetToml::from_resolved(budget),
        }
    }
}

/// Redacted TOML projection of the writer-side `[storage]` section.
#[derive(Debug, Serialize)]
pub struct PrimaryStorageToml {
    /// Filesystem path of the canonical instance.
    pub path: String,
    /// Canonical store role projection.
    pub canonical: StorageRoleToml,
    /// Derive store role projection.
    pub derive: StorageRoleToml,
}

impl PrimaryStorageToml {
    /// Builds a [`PrimaryStorageToml`] from a resolved storage location.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedPrimaryStorage) -> Self {
        Self {
            path: resolved.path.display().to_string(),
            canonical: StorageRoleToml::from_resolved(resolved.canonical_rocksdb_budget),
            derive: StorageRoleToml::from_resolved(resolved.derive_rocksdb_budget),
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
    /// Initial catchup timeout in milliseconds.
    pub initial_catchup_timeout_ms: u64,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
    /// Canonical store role projection.
    pub canonical: StorageRoleToml,
    /// Derive store role projection.
    pub derive: StorageRoleToml,
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
            initial_catchup_timeout_ms: duration_as_millis_u64(resolved.initial_catchup_timeout),
            secondary_replica_lag_threshold_chain_epochs: resolved
                .secondary_replica_lag_threshold_chain_epochs,
            canonical: StorageRoleToml::from_resolved(resolved.canonical_rocksdb_budget),
            derive: StorageRoleToml::from_resolved(resolved.derive_rocksdb_budget),
        }
    }
}

/// Redacted TOML projection of a canonical-secondary `[storage]` section.
#[derive(Debug, Serialize)]
pub struct CanonicalSecondaryStorageToml {
    /// Canonical primary path.
    pub path: String,
    /// Reader-local secondary metadata path.
    pub secondary_path: String,
    /// Catchup tick cadence in milliseconds.
    pub secondary_catchup_interval_ms: u64,
    /// Initial catchup timeout in milliseconds.
    pub initial_catchup_timeout_ms: u64,
    /// Replica-lag threshold in chain epochs.
    pub secondary_replica_lag_threshold_chain_epochs: u64,
    /// Canonical store role projection.
    pub canonical: StorageRoleToml,
}

impl CanonicalSecondaryStorageToml {
    /// Builds a [`CanonicalSecondaryStorageToml`] from a resolved
    /// canonical-secondary storage configuration.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedCanonicalSecondaryStorage) -> Self {
        Self {
            path: resolved.path.display().to_string(),
            secondary_path: resolved.secondary_path.display().to_string(),
            secondary_catchup_interval_ms: duration_as_millis_u64(
                resolved.secondary_catchup_interval,
            ),
            initial_catchup_timeout_ms: duration_as_millis_u64(resolved.initial_catchup_timeout),
            secondary_replica_lag_threshold_chain_epochs: resolved
                .secondary_replica_lag_threshold_chain_epochs,
            canonical: StorageRoleToml::from_resolved(resolved.canonical_rocksdb_budget),
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
            canonical: StorageRoleSection::default(),
            derive: StorageRoleSection::default(),
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
            resolved.initial_catchup_timeout,
            Duration::from_millis(DEFAULT_INITIAL_CATCHUP_TIMEOUT_MS)
        );
        assert_eq!(
            resolved.secondary_replica_lag_threshold_chain_epochs,
            DEFAULT_SECONDARY_REPLICA_LAG_THRESHOLD_CHAIN_EPOCHS
        );
        assert_eq!(
            resolved.canonical_rocksdb_budget.block_cache_bytes,
            crate::canonical_reader_block_cache_bytes()
        );
        assert_eq!(
            resolved.canonical_rocksdb_budget.max_open_files,
            crate::canonical_reader_max_open_files()
        );
        assert_eq!(
            resolved.derive_rocksdb_budget,
            RocksDbResourceBudget::derive_reader_defaults()
        );
        Ok(())
    }

    #[test]
    fn canonical_secondary_storage_applies_canonical_budget_only() -> Result<(), ConfigError> {
        let resolved = resolve_canonical_secondary_storage(CanonicalSecondaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            secondary_path: Some(PathBuf::from("/tmp/store-secondary")),
            canonical: StorageRoleSection {
                rocksdb: RocksDbResourceBudgetSection {
                    max_open_files: Some(128),
                    ..RocksDbResourceBudgetSection::default()
                },
            },
            ..CanonicalSecondaryStorageSection::default()
        })?;
        assert_eq!(resolved.path, PathBuf::from("/tmp/store"));
        assert_eq!(
            resolved.secondary_path,
            PathBuf::from("/tmp/store-secondary")
        );
        assert_eq!(
            resolved.secondary_catchup_interval,
            Duration::from_millis(DEFAULT_SECONDARY_CATCHUP_INTERVAL_MS)
        );
        // max_open_files override (128) wins over the container-derived value.
        assert_eq!(resolved.canonical_rocksdb_budget.max_open_files, 128);
        // block_cache_bytes has no override, so it takes the container-derived value.
        assert_eq!(
            resolved.canonical_rocksdb_budget.block_cache_bytes,
            crate::canonical_reader_block_cache_bytes()
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
    fn secondary_storage_rejects_zero_initial_catchup_timeout() {
        let outcome = resolve_secondary_storage(SecondaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            secondary_path: Some(PathBuf::from("/tmp/store-secondary")),
            initial_catchup_timeout_ms: Some(0),
            ..SecondaryStorageSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn canonical_writer_budget_resolution_falls_through_to_writer_defaults()
    -> Result<(), ConfigError> {
        let resolved =
            resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection::default())?;
        assert_eq!(resolved, RocksDbResourceBudget::canonical_writer_defaults());
        Ok(())
    }

    #[test]
    fn derive_writer_budget_resolution_falls_through_to_writer_defaults() -> Result<(), ConfigError>
    {
        let resolved =
            resolve_derive_writer_rocksdb_budget(RocksDbResourceBudgetSection::default())?;
        assert_eq!(resolved, RocksDbResourceBudget::derive_writer_defaults());
        Ok(())
    }

    #[test]
    fn canonical_reader_budget_resolution_applies_container_aware_defaults()
    -> Result<(), ConfigError> {
        let resolved =
            resolve_canonical_reader_rocksdb_budget(RocksDbResourceBudgetSection::default())?;
        // block_cache and max_open_files come from the container budget helpers;
        // other fields fall through to canonical_reader_defaults().
        assert_eq!(
            resolved.block_cache_bytes,
            crate::canonical_reader_block_cache_bytes()
        );
        assert_eq!(
            resolved.max_open_files,
            crate::canonical_reader_max_open_files()
        );
        let static_defaults = RocksDbResourceBudget::canonical_reader_defaults();
        assert_eq!(resolved.max_wal_bytes, static_defaults.max_wal_bytes);
        assert_eq!(
            resolved.write_buffer_bytes,
            static_defaults.write_buffer_bytes
        );
        assert_eq!(
            resolved.memtable_budget_bytes,
            static_defaults.memtable_budget_bytes
        );
        Ok(())
    }

    #[test]
    fn derive_reader_budget_resolution_falls_through_to_reader_defaults() -> Result<(), ConfigError>
    {
        let resolved =
            resolve_derive_reader_rocksdb_budget(RocksDbResourceBudgetSection::default())?;
        assert_eq!(resolved, RocksDbResourceBudget::derive_reader_defaults());
        Ok(())
    }

    #[test]
    fn canonical_budget_resolution_applies_individual_overrides() -> Result<(), ConfigError> {
        let resolved = resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection {
            block_cache_bytes: Some(8 * 1024 * 1024),
            max_wal_bytes: None,
            max_open_files: Some(256),
            write_buffer_bytes: Some(8 * 1024 * 1024),
            max_write_buffer_count: Some(3),
            max_background_jobs: Some(6),
            memtable_budget_bytes: Some(16 * 1024 * 1024),
            statistics_level: Some("off".to_owned()),
        })?;
        let defaults = RocksDbResourceBudget::canonical_writer_defaults();
        assert_eq!(resolved.block_cache_bytes, 8 * 1024 * 1024);
        assert_eq!(resolved.max_wal_bytes, defaults.max_wal_bytes);
        assert_eq!(resolved.max_open_files, 256);
        assert_eq!(resolved.write_buffer_bytes, 8 * 1024 * 1024);
        assert_eq!(resolved.max_write_buffer_count, 3);
        assert_eq!(resolved.max_background_jobs, 6);
        assert_eq!(resolved.memtable_budget_bytes, 16 * 1024 * 1024);
        assert_eq!(resolved.statistics_level, RocksDbStatisticsLevel::Off);
        Ok(())
    }

    #[test]
    fn background_job_budget_rejects_single_job() {
        let outcome = resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection {
            max_background_jobs: Some(1),
            ..RocksDbResourceBudgetSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn statistics_level_defaults_to_tickers() -> Result<(), ConfigError> {
        let resolved =
            resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection::default())?;
        assert_eq!(resolved.statistics_level, RocksDbStatisticsLevel::Tickers);
        Ok(())
    }

    #[test]
    fn statistics_level_accepts_full() -> Result<(), ConfigError> {
        let resolved = resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection {
            statistics_level: Some("full".to_owned()),
            ..RocksDbResourceBudgetSection::default()
        })?;
        assert_eq!(resolved.statistics_level, RocksDbStatisticsLevel::Full);
        Ok(())
    }

    #[test]
    fn statistics_level_rejects_unknown_value() {
        let outcome = resolve_canonical_writer_rocksdb_budget(RocksDbResourceBudgetSection {
            statistics_level: Some("verbose".to_owned()),
            ..RocksDbResourceBudgetSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn primary_storage_carries_role_scoped_budgets() -> Result<(), ConfigError> {
        let resolved = resolve_primary_storage(PrimaryStorageSection {
            path: Some(PathBuf::from("/tmp/store")),
            canonical: StorageRoleSection {
                rocksdb: RocksDbResourceBudgetSection {
                    max_wal_bytes: Some(123 * 1024 * 1024),
                    ..RocksDbResourceBudgetSection::default()
                },
            },
            derive: StorageRoleSection {
                rocksdb: RocksDbResourceBudgetSection {
                    max_wal_bytes: Some(64 * 1024 * 1024),
                    ..RocksDbResourceBudgetSection::default()
                },
            },
        })?;
        assert_eq!(
            resolved.canonical_rocksdb_budget.max_wal_bytes,
            123 * 1024 * 1024
        );
        assert_eq!(
            resolved.derive_rocksdb_budget.max_wal_bytes,
            64 * 1024 * 1024
        );
        Ok(())
    }
}
