//! Role-scoped `RocksDB` resource budget.
//!
//! `RocksDbResourceBudget` separates the two halves of `RocksDB` option setting that
//! [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md)
//! identifies:
//!
//! - **Architectural invariants** (WAL on, point-in-time recovery, atomic
//!   flush, ordered writes, and direct-I/O fallback) live in the bounded
//!   `RocksDB` open path. They are not tunable; touching them breaks the
//!   per-epoch commit contract.
//! - **Bounded resource budget** lives here. The numbers below cap the
//!   block-cache, table-cache, WAL, per-column-family write buffers, and total
//!   memtable memory surfaces so a crash-replay open or bulk write does not
//!   OOM the host. The defaults target a mainnet-sized store and are documented in
//!   [the OOM-recovery runbook](../../../docs/runbooks/bulk-catchup-oom-recovery.md).
//!
//! Construct one with writer defaults for primary stores and reader defaults
//! for `RocksDB` secondaries. Operators may override individual fields through
//! `[storage.canonical.rocksdb]` and `[storage.derive.rocksdb]` in their TOML.

/// Bounded `RocksDB` resource budget applied to one DB instance at open.
///
/// The fields together cap the resident-memory peak at roughly
/// `block_cache_bytes + max_wal_bytes + memtable_budget_bytes` regardless of
/// store size. Each field has a single concrete effect:
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
/// - `write_buffer_bytes` caps each column family's mutable memtable before it
///   rotates to an immutable memtable and flushes to an `SST`.
/// - `max_write_buffer_count` caps how many mutable plus immutable memtables a
///   column family may hold before writes stall.
/// - `memtable_budget_bytes` caps total memtable memory across column
///   families via `RocksDB`'s write-buffer manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RocksDbResourceBudget {
    /// Bounded LRU cache size for data, index, and filter blocks.
    pub block_cache_bytes: u64,
    /// Total live WAL size ceiling across all column families.
    pub max_wal_bytes: u64,
    /// Open `SST` file handle limit. `RocksDB` takes `i32` natively;
    /// negative values mean "unbounded".
    pub max_open_files: i32,
    /// Per-column-family mutable memtable size.
    pub write_buffer_bytes: u64,
    /// Per-column-family mutable plus immutable memtable count.
    pub max_write_buffer_count: i32,
    /// Total mutable and immutable memtable memory budget across column families.
    pub memtable_budget_bytes: u64,
}

impl RocksDbResourceBudget {
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

    /// Smallest accepted [`Self::write_buffer_bytes`]. Smaller values create
    /// flush churn and make high-throughput catch-up compaction-bound.
    pub const MIN_WRITE_BUFFER_BYTES: u64 = 4 * MIB;

    /// Smallest accepted [`Self::max_write_buffer_count`]. One buffer cannot
    /// absorb writes while another buffer flushes.
    pub const MIN_MAX_WRITE_BUFFER_COUNT: i32 = 2;

    /// Smallest accepted [`Self::memtable_budget_bytes`]. Smaller values
    /// force constant flush churn and do not leave enough room for one
    /// bounded column-family memtable to rotate cleanly.
    pub const MIN_MEMTABLE_BUDGET_BYTES: u64 = 4 * MIB;

    /// Canonical-store writer defaults sized for a mainnet-shaped deployment.
    ///
    /// `512 MiB` block cache, `256 MiB` WAL ceiling, `512` open file handles,
    /// `16 MiB x 2` write buffers per column family, and `256 MiB` total
    /// memtable budget. See ADR-0020 for the budget derivation.
    #[must_use]
    pub const fn canonical_writer_defaults() -> Self {
        Self {
            block_cache_bytes: 512 * MIB,
            max_wal_bytes: 256 * MIB,
            max_open_files: 512,
            write_buffer_bytes: 16 * MIB,
            max_write_buffer_count: 2,
            memtable_budget_bytes: 256 * MIB,
        }
    }

    /// Derive-store writer defaults sized for smaller rebuildable artifacts.
    ///
    /// `128 MiB` block cache, `64 MiB` WAL ceiling, `256` open file handles,
    /// `8 MiB x 2` write buffers per column family, and `64 MiB` total
    /// memtable budget. Derive stores hold smaller working sets than the
    /// canonical chain store and are rebuildable from retained chain events.
    #[must_use]
    pub const fn derive_writer_defaults() -> Self {
        Self {
            block_cache_bytes: 128 * MIB,
            max_wal_bytes: 64 * MIB,
            max_open_files: 256,
            write_buffer_bytes: 8 * MIB,
            max_write_buffer_count: 2,
            memtable_budget_bytes: 64 * MIB,
        }
    }

    /// Canonical-store reader defaults for secondary processes.
    ///
    /// `128 MiB` block cache, `32 MiB` WAL ceiling, `128` open file handles,
    /// `8 MiB x 2` write buffers per column family, and `16 MiB` total
    /// memtable budget. Secondary readers replay the writer's manifest and
    /// serve public traffic; their memory posture must stay below the writer
    /// so readers cannot starve clean sync.
    #[must_use]
    pub const fn canonical_reader_defaults() -> Self {
        Self {
            block_cache_bytes: 128 * MIB,
            max_wal_bytes: 32 * MIB,
            max_open_files: 128,
            write_buffer_bytes: 8 * MIB,
            max_write_buffer_count: 2,
            memtable_budget_bytes: 16 * MIB,
        }
    }

    /// Derive-store reader defaults for secondary processes.
    ///
    /// `64 MiB` block cache, `16 MiB` WAL ceiling, `64` open file handles,
    /// `4 MiB x 2` write buffers per column family, and `16 MiB` total
    /// memtable budget. Derive reader stores are rebuildable and subordinate
    /// to the canonical reader path.
    #[must_use]
    pub const fn derive_reader_defaults() -> Self {
        Self {
            block_cache_bytes: 64 * MIB,
            max_wal_bytes: 16 * MIB,
            max_open_files: 64,
            write_buffer_bytes: 4 * MIB,
            max_write_buffer_count: 2,
            memtable_budget_bytes: 16 * MIB,
        }
    }

    /// Defaults for throwaway local test stores.
    ///
    /// `32 MiB` block cache, `16 MiB` WAL ceiling, `64` open file handles,
    /// `4 MiB x 2` write buffers per column family, and `8 MiB` total
    /// memtable budget. Keeps unit-test memory footprint negligible while
    /// still exercising the bounded code path.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            block_cache_bytes: 32 * MIB,
            max_wal_bytes: 16 * MIB,
            max_open_files: 64,
            write_buffer_bytes: 4 * MIB,
            max_write_buffer_count: 2,
            memtable_budget_bytes: 8 * MIB,
        }
    }

    /// Validates that the budget keeps the bounded-resource invariants
    /// closed before the caller builds `RocksDB` options from it.
    pub const fn validate(self) -> Result<(), &'static str> {
        if self.block_cache_bytes < Self::MIN_BLOCK_CACHE_BYTES {
            return Err("rocksdb_resource_budget.block_cache_bytes must be at least 4 MiB");
        }
        if self.max_wal_bytes < Self::MIN_MAX_WAL_BYTES {
            return Err(
                "rocksdb_resource_budget.max_wal_bytes must be at least 4 MiB; zero disables RocksDB's WAL-size flush trigger",
            );
        }
        if self.max_open_files < Self::MIN_MAX_OPEN_FILES {
            return Err(
                "rocksdb_resource_budget.max_open_files must be at least 32; negative values pin every SST's metadata in resident memory",
            );
        }
        if self.write_buffer_bytes < Self::MIN_WRITE_BUFFER_BYTES {
            return Err("rocksdb_resource_budget.write_buffer_bytes must be at least 4 MiB");
        }
        if self.max_write_buffer_count < Self::MIN_MAX_WRITE_BUFFER_COUNT {
            return Err("rocksdb_resource_budget.max_write_buffer_count must be at least 2");
        }
        if self.memtable_budget_bytes < Self::MIN_MEMTABLE_BUDGET_BYTES {
            return Err("rocksdb_resource_budget.memtable_budget_bytes must be at least 4 MiB");
        }
        Ok(())
    }
}

const MIB: u64 = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_writer_defaults_match_mainnet_envelope() {
        let budget = RocksDbResourceBudget::canonical_writer_defaults();
        assert_eq!(budget.block_cache_bytes, 512 * MIB);
        assert_eq!(budget.max_wal_bytes, 256 * MIB);
        assert_eq!(budget.max_open_files, 512);
        assert_eq!(budget.write_buffer_bytes, 16 * MIB);
        assert_eq!(budget.max_write_buffer_count, 2);
        assert_eq!(budget.memtable_budget_bytes, 256 * MIB);
    }

    #[test]
    fn derive_writer_defaults_are_smaller_than_canonical_writer_budget() {
        let canonical = RocksDbResourceBudget::canonical_writer_defaults();
        let derive = RocksDbResourceBudget::derive_writer_defaults();
        assert_eq!(derive.block_cache_bytes * 4, canonical.block_cache_bytes);
        assert_eq!(derive.max_wal_bytes * 4, canonical.max_wal_bytes);
        assert_eq!(derive.write_buffer_bytes * 2, canonical.write_buffer_bytes);
        assert_eq!(
            derive.memtable_budget_bytes * 4,
            canonical.memtable_budget_bytes
        );
    }

    #[test]
    fn reader_defaults_are_smaller_than_writer_defaults() {
        assert!(
            RocksDbResourceBudget::canonical_reader_defaults().block_cache_bytes
                < RocksDbResourceBudget::canonical_writer_defaults().block_cache_bytes
        );
        assert!(
            RocksDbResourceBudget::derive_reader_defaults().block_cache_bytes
                < RocksDbResourceBudget::derive_writer_defaults().block_cache_bytes
        );
    }

    #[test]
    fn local_test_defaults_fit_in_megabytes() {
        let budget = RocksDbResourceBudget::for_local_tests();
        assert!(budget.block_cache_bytes <= 64 * MIB);
        assert!(budget.max_wal_bytes <= 32 * MIB);
        assert!(budget.max_open_files <= 128);
        assert!(budget.write_buffer_bytes <= 8 * MIB);
        assert!(budget.memtable_budget_bytes <= 8 * MIB);
    }

    #[test]
    fn zero_wal_budget_is_rejected() {
        let mut budget = RocksDbResourceBudget::for_local_tests();
        budget.max_wal_bytes = 0;
        assert!(matches!(budget.validate(), Err(reason) if reason.contains("max_wal_bytes")));
    }

    #[test]
    fn negative_open_file_budget_is_rejected() {
        let mut budget = RocksDbResourceBudget::for_local_tests();
        budget.max_open_files = -1;
        assert!(matches!(budget.validate(), Err(reason) if reason.contains("max_open_files")));
    }

    #[test]
    fn undersized_write_buffer_budget_is_rejected() {
        let mut budget = RocksDbResourceBudget::for_local_tests();
        budget.write_buffer_bytes = MIB;
        assert!(matches!(budget.validate(), Err(reason) if reason.contains("write_buffer_bytes")));
    }

    #[test]
    fn single_write_buffer_is_rejected() {
        let mut budget = RocksDbResourceBudget::for_local_tests();
        budget.max_write_buffer_count = 1;
        assert!(
            matches!(budget.validate(), Err(reason) if reason.contains("max_write_buffer_count"))
        );
    }

    #[test]
    fn undersized_memtable_budget_is_rejected() {
        let mut budget = RocksDbResourceBudget::for_local_tests();
        budget.memtable_budget_bytes = MIB;
        assert!(
            matches!(budget.validate(), Err(reason) if reason.contains("memtable_budget_bytes"))
        );
    }
}
