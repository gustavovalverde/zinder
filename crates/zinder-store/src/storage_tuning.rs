//! Operator-tunable `RocksDB` resource budget.
//!
//! `StorageTuning` separates the two halves of `RocksDB` option setting that
//! [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md)
//! identifies:
//!
//! - **Architectural invariants** (WAL on, point-in-time recovery, atomic
//!   flush, ordered writes) live in [`crate::kv::build_primary_db_options`]
//!   and [`crate::kv::build_secondary_db_options`]. They are not tunable;
//!   touching them breaks the per-epoch commit contract.
//! - **Bounded resource budget** lives here. The three numbers below cap the
//!   open-time RAM peak so a crash-replay open does not OOM the host. The
//!   defaults target a mainnet-sized store and are documented in
//!   [the OOM-recovery runbook](../../../docs/runbooks/bulk-catchup-oom-recovery.md).
//!
//! Construct one with [`StorageTuning::canonical_defaults`] for the writer
//! and reader replicas of the canonical chain store, or
//! [`StorageTuning::derive_defaults`] for the derive plane's own `RocksDB`
//! instance. Operators may override individual fields through
//! `[storage.tuning]` in their TOML.

/// Bounded `RocksDB` resource budget applied to one DB instance at open.
///
/// Three knobs together cap the resident-memory peak at roughly
/// `block_cache_bytes + max_wal_bytes + active_memtables` regardless of
/// store size. Each knob has a single concrete effect:
///
/// - `block_cache_bytes` is the size of the bounded LRU cache shared by
///   data blocks, index blocks, and bloom filter blocks. Without a bounded
///   cache, `RocksDB` pins index and bloom blocks per-SST in resident memory,
///   which scales with store size.
/// - `max_wal_bytes` is the ceiling for the live WAL across all column
///   families. Crossing it triggers a memtable flush so the WAL truncates.
///   The default of 0 (`RocksDB`'s own) means "never trigger from WAL size",
///   which is the bug the OOM-recovery runbook documents.
/// - `max_open_files` caps the number of `SST` file handles `RocksDB` keeps
///   open. The default of -1 (`RocksDB`'s own) means "open every `SST` and
///   pin its metadata", which is what makes a mainnet-sized store's
///   open-time RSS scale with store size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageTuning {
    /// Bounded LRU cache size for data, index, and filter blocks.
    pub block_cache_bytes: u64,
    /// Total live WAL size ceiling across all column families.
    pub max_wal_bytes: u64,
    /// Open `SST` file handle limit. `RocksDB` takes `i32` natively;
    /// negative values mean "unbounded".
    pub max_open_files: i32,
}

impl StorageTuning {
    /// Smallest accepted [`Self::block_cache_bytes`]. Anything smaller
    /// degenerates into per-`SST` pinning, which is the regression
    /// [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md)
    /// closes.
    pub const MIN_BLOCK_CACHE_BYTES: u64 = 4 * MIB;

    /// Smallest accepted [`Self::max_wal_bytes`]. Zero specifically would
    /// disable `RocksDB`'s WAL-size flush trigger and reopen the
    /// bulk-catchup OOM trap; smaller-but-nonzero values would force the
    /// writer to flush on every batch.
    pub const MIN_MAX_WAL_BYTES: u64 = 4 * MIB;

    /// Smallest accepted [`Self::max_open_files`]. Negative values
    /// (`RocksDB`'s "unbounded") pin every `SST`'s metadata in resident
    /// memory.
    pub const MIN_MAX_OPEN_FILES: i32 = 32;

    /// Canonical-store defaults sized for a mainnet-shaped deployment.
    ///
    /// `512 MiB` block cache, `256 MiB` WAL ceiling, `512` open file
    /// handles. Yields a roughly `~1 GiB` open-time RAM peak even when a
    /// crash leaves a full WAL behind. See ADR-0020 for the budget
    /// derivation.
    #[must_use]
    pub const fn canonical_defaults() -> Self {
        Self {
            block_cache_bytes: 512 * MIB,
            max_wal_bytes: 256 * MIB,
            max_open_files: 512,
        }
    }

    /// Derive-plane defaults sized for the smaller projected stores managed
    /// by `zinder-explorer`.
    ///
    /// `128 MiB` block cache, `64 MiB` WAL ceiling, `256` open file
    /// handles. Halves the canonical budget because derive stores hold
    /// strictly smaller working sets than the canonical chain store.
    #[must_use]
    pub const fn derive_defaults() -> Self {
        Self {
            block_cache_bytes: 128 * MIB,
            max_wal_bytes: 64 * MIB,
            max_open_files: 256,
        }
    }

    /// Defaults for throwaway local test stores.
    ///
    /// `32 MiB` block cache, `16 MiB` WAL ceiling, `64` open file handles.
    /// Keeps unit-test memory footprint negligible while still exercising
    /// the bounded code path.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            block_cache_bytes: 32 * MIB,
            max_wal_bytes: 16 * MIB,
            max_open_files: 64,
        }
    }

    /// Validates that the budget keeps the bounded-resource invariants
    /// closed before the caller builds `RocksDB` options from it.
    pub const fn validate(self) -> Result<(), &'static str> {
        if self.block_cache_bytes < Self::MIN_BLOCK_CACHE_BYTES {
            return Err("storage.tuning.block_cache_bytes must be at least 4 MiB");
        }
        if self.max_wal_bytes < Self::MIN_MAX_WAL_BYTES {
            return Err(
                "storage.tuning.max_wal_bytes must be at least 4 MiB; zero disables RocksDB's WAL-size flush trigger",
            );
        }
        if self.max_open_files < Self::MIN_MAX_OPEN_FILES {
            return Err(
                "storage.tuning.max_open_files must be at least 32; negative values pin every SST's metadata in resident memory",
            );
        }
        Ok(())
    }
}

const MIB: u64 = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_defaults_match_mainnet_envelope() {
        let tuning = StorageTuning::canonical_defaults();
        assert_eq!(tuning.block_cache_bytes, 512 * MIB);
        assert_eq!(tuning.max_wal_bytes, 256 * MIB);
        assert_eq!(tuning.max_open_files, 512);
    }

    #[test]
    fn derive_defaults_are_half_canonical_budget() {
        let canonical = StorageTuning::canonical_defaults();
        let derive = StorageTuning::derive_defaults();
        assert_eq!(derive.block_cache_bytes * 4, canonical.block_cache_bytes);
        assert_eq!(derive.max_wal_bytes * 4, canonical.max_wal_bytes);
    }

    #[test]
    fn local_test_defaults_fit_in_megabytes() {
        let tuning = StorageTuning::for_local_tests();
        assert!(tuning.block_cache_bytes <= 64 * MIB);
        assert!(tuning.max_wal_bytes <= 32 * MIB);
        assert!(tuning.max_open_files <= 128);
    }

    #[test]
    fn zero_wal_budget_is_rejected() {
        let mut tuning = StorageTuning::for_local_tests();
        tuning.max_wal_bytes = 0;
        assert!(matches!(tuning.validate(), Err(reason) if reason.contains("max_wal_bytes")));
    }

    #[test]
    fn negative_open_file_budget_is_rejected() {
        let mut tuning = StorageTuning::for_local_tests();
        tuning.max_open_files = -1;
        assert!(matches!(tuning.validate(), Err(reason) if reason.contains("max_open_files")));
    }
}
