use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::{Mutex, MutexGuard};
use rust_rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DB, FlushOptions, Options, SliceTransform,
    Snapshot, WriteBatch, WriteBufferManager, WriteOptions, checkpoint::Checkpoint,
    statistics::Ticker,
};

use crate::{RocksDbResourceBudget, StoreError, format::StoreKey, kv::StorageTable};

type PrefixScanVisitor<'visitor> =
    &'visitor mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>;

const DIRECT_IO_COMPACTION_READAHEAD_BYTES: usize = 2 * 1024 * 1024;
const ROCKSDB_DEFAULT_COLUMN_FAMILY: &str = "default";

/// Filesystem I/O mode resolved while opening a `RocksDB` instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RocksDbIoMode {
    /// `RocksDB` opened with direct reads, and primary stores also opened
    /// flush and compaction work with direct I/O.
    Direct,
    /// `RocksDB` opened with buffered filesystem I/O after direct I/O was
    /// rejected by the filesystem or platform.
    Buffered,
}

impl RocksDbIoMode {
    /// Stable label used in logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Buffered => "buffered",
        }
    }
}

/// Pipeline stage that issued a canonical-store read.
///
/// Carried by a read view or snapshot at construction so
/// `zinder_store_read_duration_seconds` and the `multi_get` key counters
/// attribute I/O to the stage that drove it. The set is fixed so the metric
/// label stays bounded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreReadCaller {
    /// Public read paths (`zinder-query`, `zinder-explorer`) and any read not
    /// attributed to a more specific stage.
    Query,
    /// Bulk-catchup block-prepare spent-transparent-output prefetch.
    BlockPrefetch,
    /// Writer-commit spend-fact resolution and reorg-window projection repairs.
    CommitFallback,
    /// Safe-tip retention sweep scans.
    RetentionSweep,
    /// Derive-replay spend-fact and block/transaction hydration.
    DeriveHydration,
}

impl StoreReadCaller {
    /// Stable label used in metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::BlockPrefetch => "block_prefetch",
            Self::CommitFallback => "commit_fallback",
            Self::RetentionSweep => "retention_sweep",
            Self::DeriveHydration => "derive_hydration",
        }
    }
}

/// Store domain and open posture used to label `RocksDB` resource gauges.
///
/// The canonical chain store and the derive store share one process, so the
/// cache, memtable, WAL, and per-CF gauges carry a `store_role` label to keep
/// their resident footprints distinguishable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreRole {
    /// Canonical chain store opened as the primary writer.
    CanonicalPrimary,
    /// Canonical chain store opened as a secondary reader.
    CanonicalSecondary,
    /// Derive store opened as the primary writer.
    DerivePrimary,
    /// Derive store opened as a secondary reader.
    DeriveSecondary,
}

impl StoreRole {
    /// Stable label used in metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalPrimary => "canonical_primary",
            Self::CanonicalSecondary => "canonical_secondary",
            Self::DerivePrimary => "derive_primary",
            Self::DeriveSecondary => "derive_secondary",
        }
    }
}

/// Open role and filesystem paths for a bounded `RocksDB` instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RocksDbOpenRole<'path> {
    /// Primary writer opening its owned store path.
    Primary {
        /// Primary store path.
        path: &'path Path,
    },
    /// Secondary reader opening a writer-owned primary path plus its own
    /// reader-local metadata path.
    Secondary {
        /// Writer-owned primary store path.
        primary_path: &'path Path,
        /// Reader-local secondary metadata path.
        secondary_path: &'path Path,
    },
}

impl<'path> RocksDbOpenRole<'path> {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Primary { .. } => "primary",
            Self::Secondary { .. } => "secondary",
        }
    }

    const fn store_path(self) -> &'path Path {
        match self {
            Self::Primary { path } => path,
            Self::Secondary { primary_path, .. } => primary_path,
        }
    }

    const fn is_primary(self) -> bool {
        matches!(self, Self::Primary { .. })
    }
}

/// Opened `RocksDB` handle plus the resource handles that must outlive it.
pub struct BoundedRocksDbOpen {
    /// Opened database handle.
    pub db: DB,
    /// Shared block cache retained for the database lifetime.
    pub block_cache: Cache,
    /// Shared write-buffer manager retained for the database lifetime.
    pub write_buffer_manager: WriteBufferManager,
    /// Resolved filesystem I/O mode.
    pub io_mode: RocksDbIoMode,
    /// Open options retained so the DB-wide statistics tickers stay readable
    /// for metric export.
    pub statistics: Arc<Options>,
}

/// Opens a bounded `RocksDB` instance with direct I/O when the platform supports it.
///
/// The column-family descriptors are rebuilt for each open attempt because
/// their options hold references to the helper-owned block cache. On direct
/// I/O failure, the function logs the unsupported mode and retries with the
/// same bounded resource budget under buffered I/O.
pub fn open_bounded_rocksdb(
    role: RocksDbOpenRole<'_>,
    rocksdb_resource_budget: RocksDbResourceBudget,
    build_column_families: impl Fn(&Cache, RocksDbResourceBudget) -> Vec<ColumnFamilyDescriptor>,
) -> Result<BoundedRocksDbOpen, rust_rocksdb::Error> {
    let block_cache = build_block_cache(rocksdb_resource_budget.block_cache_bytes);
    let write_buffer_manager =
        build_write_buffer_manager(rocksdb_resource_budget.memtable_budget_bytes);
    let open_attempt = BoundedRocksDbOpenAttempt {
        role,
        rocksdb_resource_budget,
        block_cache: &block_cache,
        write_buffer_manager: &write_buffer_manager,
        build_column_families: &build_column_families,
    };

    match open_bounded_rocksdb_once(&open_attempt, RocksDbIoMode::Direct) {
        Ok((db, statistics)) => {
            record_rocksdb_io_mode(role, RocksDbIoMode::Direct);
            Ok(BoundedRocksDbOpen {
                db,
                block_cache,
                write_buffer_manager,
                io_mode: RocksDbIoMode::Direct,
                statistics: Arc::new(statistics),
            })
        }
        Err(direct_error) => {
            tracing::warn!(
                target: "zinder::store",
                event = "rocksdb_direct_io_unsupported",
                store_path = %role.store_path().display(),
                role = role.as_str(),
                error = %direct_error,
                "RocksDB direct I/O open failed; retrying with buffered I/O"
            );
            let (db, statistics) =
                open_bounded_rocksdb_once(&open_attempt, RocksDbIoMode::Buffered)?;
            record_rocksdb_io_mode(role, RocksDbIoMode::Buffered);
            Ok(BoundedRocksDbOpen {
                db,
                block_cache,
                write_buffer_manager,
                io_mode: RocksDbIoMode::Buffered,
                statistics: Arc::new(statistics),
            })
        }
    }
}

struct BoundedRocksDbOpenAttempt<'attempt, 'path, BuildColumnFamilies>
where
    BuildColumnFamilies: Fn(&Cache, RocksDbResourceBudget) -> Vec<ColumnFamilyDescriptor>,
{
    role: RocksDbOpenRole<'path>,
    rocksdb_resource_budget: RocksDbResourceBudget,
    block_cache: &'attempt Cache,
    write_buffer_manager: &'attempt WriteBufferManager,
    build_column_families: &'attempt BuildColumnFamilies,
}

fn open_bounded_rocksdb_once<BuildColumnFamilies>(
    open_attempt: &BoundedRocksDbOpenAttempt<'_, '_, BuildColumnFamilies>,
    io_mode: RocksDbIoMode,
) -> Result<(DB, Options), rust_rocksdb::Error>
where
    BuildColumnFamilies: Fn(&Cache, RocksDbResourceBudget) -> Vec<ColumnFamilyDescriptor>,
{
    let mut db_options = match open_attempt.role {
        RocksDbOpenRole::Primary { .. } => build_primary_db_options(
            open_attempt.rocksdb_resource_budget,
            open_attempt.block_cache,
        ),
        RocksDbOpenRole::Secondary { .. } => build_secondary_db_options(
            open_attempt.rocksdb_resource_budget,
            open_attempt.block_cache,
        ),
    };
    db_options.set_write_buffer_manager(open_attempt.write_buffer_manager);
    if io_mode == RocksDbIoMode::Direct {
        db_options.set_use_direct_reads(true);
        if open_attempt.role.is_primary() {
            db_options.set_use_direct_io_for_flush_and_compaction(true);
            db_options.set_compaction_readahead_size(DIRECT_IO_COMPACTION_READAHEAD_BYTES);
        }
    }
    let column_families = (open_attempt.build_column_families)(
        open_attempt.block_cache,
        open_attempt.rocksdb_resource_budget,
    );

    let db = match open_attempt.role {
        RocksDbOpenRole::Primary { path } => {
            DB::open_cf_descriptors(&db_options, path, column_families)
        }
        RocksDbOpenRole::Secondary {
            primary_path,
            secondary_path,
        } => DB::open_cf_descriptors_as_secondary(
            &db_options,
            primary_path,
            secondary_path,
            column_families,
        ),
    }?;
    Ok((db, db_options))
}

fn record_rocksdb_io_mode(role: RocksDbOpenRole<'_>, io_mode: RocksDbIoMode) {
    tracing::info!(
        target: "zinder::store",
        event = "rocksdb_io_mode",
        store_path = %role.store_path().display(),
        role = role.as_str(),
        mode = io_mode.as_str(),
        "RocksDB I/O mode resolved"
    );
}

fn storage_column_family_descriptors(
    block_cache: &Cache,
    rocksdb_resource_budget: RocksDbResourceBudget,
    existing_column_families: &[String],
) -> Vec<ColumnFamilyDescriptor> {
    let mut opened_names = BTreeSet::new();
    let mut descriptors = StorageTable::all()
        .into_iter()
        .map(|table| {
            opened_names.insert(table.column_family_name().to_owned());
            ColumnFamilyDescriptor::new(
                table.column_family_name(),
                column_family_options(table, block_cache, rocksdb_resource_budget),
            )
        })
        .collect::<Vec<_>>();

    for name in existing_column_families {
        if name == ROCKSDB_DEFAULT_COLUMN_FAMILY || opened_names.contains(name) {
            continue;
        }
        descriptors.push(ColumnFamilyDescriptor::new(
            name,
            extra_column_family_options(block_cache, rocksdb_resource_budget),
        ));
    }

    descriptors
}

fn existing_column_family_names(path: &Path) -> Vec<String> {
    DB::list_cf(&Options::default(), path).unwrap_or_default()
}

#[derive(Clone)]
pub(crate) struct RocksChainStore {
    db: Arc<DB>,
    control_lock: Arc<Mutex<()>>,
    sync_writes: bool,
    block_cache: Cache,
    write_buffer_manager: WriteBufferManager,
    statistics: Arc<Options>,
    io_mode: RocksDbIoMode,
    resource_budget: RocksDbResourceBudget,
    store_role: StoreRole,
    resource_gauge_throttle: Arc<ResourceGaugeThrottle>,
}

impl RocksChainStore {
    pub(crate) fn open_primary(
        path: impl AsRef<Path>,
        sync_writes: bool,
        rocksdb_resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, StoreError> {
        let existing_column_families = existing_column_family_names(path.as_ref());
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary {
                path: path.as_ref(),
            },
            rocksdb_resource_budget,
            |cache, resource_budget| {
                storage_column_family_descriptors(cache, resource_budget, &existing_column_families)
            },
        )
        .map_err(|source| StoreError::primary_open_failed(path.as_ref(), source))?;

        let store = Self {
            db: Arc::new(bounded_open.db),
            control_lock: Arc::new(Mutex::new(())),
            sync_writes,
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_budget: rocksdb_resource_budget,
            store_role: StoreRole::CanonicalPrimary,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
        };
        store.record_rocksdb_properties();

        Ok(store)
    }

    pub(crate) fn open_secondary(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        sync_writes: bool,
        rocksdb_resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, StoreError> {
        let existing_column_families = existing_column_family_names(primary_path.as_ref());
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Secondary {
                primary_path: primary_path.as_ref(),
                secondary_path: secondary_path.as_ref(),
            },
            rocksdb_resource_budget,
            |cache, resource_budget| {
                storage_column_family_descriptors(cache, resource_budget, &existing_column_families)
            },
        )
        .map_err(StoreError::storage_unavailable)?;

        let store = Self {
            db: Arc::new(bounded_open.db),
            control_lock: Arc::new(Mutex::new(())),
            sync_writes,
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_budget: rocksdb_resource_budget,
            store_role: StoreRole::CanonicalSecondary,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
        };
        store.record_rocksdb_properties();

        Ok(store)
    }

    pub(crate) fn lock_control(&self) -> MutexGuard<'_, ()> {
        self.control_lock.lock()
    }

    pub(crate) fn get(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let column_family = self.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .db
            .get_cf(&column_family, key.as_bytes())
            .map_err(StoreError::storage_unavailable);
        record_store_read_outcome("get", caller, table, started_at, &read_outcome);

        read_outcome
    }

    pub(crate) fn snapshot(&self, caller: StoreReadCaller) -> RocksChainStoreSnapshot<'_> {
        RocksChainStoreSnapshot {
            store: self,
            snapshot: Snapshot::new(self.db.as_ref()),
            caller,
        }
    }

    pub(crate) fn direct_read_view_for(
        &self,
        caller: StoreReadCaller,
    ) -> RocksChainStoreReadView<'_> {
        RocksChainStoreReadView::Direct {
            store: self,
            caller,
        }
    }

    pub(crate) fn snapshot_read_view_for(
        &self,
        caller: StoreReadCaller,
    ) -> RocksChainStoreReadView<'_> {
        RocksChainStoreReadView::Snapshot(self.snapshot(caller))
    }

    pub(crate) fn multi_get(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let column_family = self.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .db
            .multi_get_cf(keys.iter().map(|key| (&column_family, key.as_bytes())))
            .into_iter()
            .map(|rocksdb_result| rocksdb_result.map_err(StoreError::storage_unavailable))
            .collect();
        record_store_multi_get_outcome(caller, table, started_at, keys.len(), &read_outcome);

        read_outcome
    }

    pub(crate) fn sorted_multi_get(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let column_family = self.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .db
            .batched_multi_get_cf_slice(&column_family, keys.iter().map(StoreKey::as_bytes), true)
            .into_iter()
            .map(|rocksdb_result| {
                rocksdb_result
                    .map(|maybe_slice| maybe_slice.map(|slice| slice.to_vec()))
                    .map_err(StoreError::storage_unavailable)
            })
            .collect();
        record_store_multi_get_outcome(caller, table, started_at, keys.len(), &read_outcome);

        read_outcome
    }

    pub(crate) fn get_previous_by_prefix(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let started_at = Instant::now();
        let read_outcome = (|| {
            let column_family = self.column_family(table)?;
            let mut iterator = self.db.raw_iterator_cf(&column_family);

            iterator.seek_for_prev(seek_key.as_bytes());
            if !iterator.valid() {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(None);
            }

            let Some((key, index_value)) = iterator.item() else {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(None);
            };

            if !key.starts_with(prefix.as_bytes()) {
                return Ok(None);
            }

            Ok(Some(index_value.to_vec()))
        })();
        record_store_read_outcome("seek_for_prev", caller, table, started_at, &read_outcome);

        read_outcome
    }

    pub(crate) fn scan_prefix(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.column_family(table)?;
            let mut iterator = self.db.raw_iterator_cf(&column_family);

            iterator.seek(prefix.as_bytes());
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.next();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(caller, table, started_at, &scan_outcome);

        scan_outcome
    }

    pub(crate) fn scan_prefix_reverse(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.column_family(table)?;
            let mut iterator = self.db.raw_iterator_cf(&column_family);
            if let Some(upper_bound) = exclusive_prefix_upper_bound(prefix.as_bytes()) {
                iterator.seek(&upper_bound);
                if iterator.valid() {
                    iterator.prev();
                } else {
                    iterator.seek_to_last();
                }
            } else {
                iterator.seek_to_last();
            }
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.prev();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(caller, table, started_at, &scan_outcome);

        scan_outcome
    }

    pub(crate) fn scan_prefix_reverse_before(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        before: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let column_family = self.column_family(table)?;
        let mut iterator = self.db.raw_iterator_cf(&column_family);
        iterator.seek_for_prev(before.as_bytes());
        if iterator.key() == Some(before.as_bytes()) {
            iterator.prev();
        }
        while iterator.valid() {
            let Some((key, row_value)) = iterator.item() else {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(());
            };
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                break;
            }
            iterator.prev();
        }
        iterator.status().map_err(StoreError::storage_unavailable)
    }

    pub(crate) fn scan_forward(
        &self,
        caller: StoreReadCaller,
        table: StorageTable,
        start_key: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.column_family(table)?;
            let mut iterator = self.db.raw_iterator_cf(&column_family);

            iterator.seek(start_key.as_bytes());
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.next();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(caller, table, started_at, &scan_outcome);

        scan_outcome
    }

    pub(crate) fn write(&self, puts: Vec<StoragePut>) -> Result<(), StoreError> {
        self.write_batch(puts, Vec::new())
    }

    pub(crate) fn write_batch(
        &self,
        puts: Vec<StoragePut>,
        deletes: Vec<StorageDelete>,
    ) -> Result<(), StoreError> {
        record_write_batch_inputs(&puts, &deletes);
        let started_at = Instant::now();
        let write_outcome = (|| {
            let mut batch = WriteBatch::default();

            for put in puts {
                let column_family = self.column_family(put.table)?;
                batch.put_cf(&column_family, put.key.as_bytes(), put.value);
            }
            for delete in deletes {
                let column_family = self.column_family(delete.table)?;
                batch.delete_cf(&column_family, delete.key.as_bytes());
            }

            let mut write_options = WriteOptions::default();
            write_options.set_sync(self.sync_writes);

            self.db
                .write_opt(&batch, &write_options)
                .map_err(StoreError::storage_unavailable)
        })();
        record_write_batch_outcome(started_at, &write_outcome);
        if write_outcome.is_ok() && self.resource_gauge_throttle.should_sample() {
            self.record_rocksdb_properties();
        }

        write_outcome
    }

    pub(crate) fn try_catch_up_with_primary(&self) -> Result<(), StoreError> {
        self.db
            .try_catch_up_with_primary()
            .map_err(StoreError::secondary_catchup_failed)
    }

    /// Drops `table`'s column family and recreates it empty with the same
    /// bounded options, reclaiming its disk space immediately.
    ///
    /// Primary-only schema-migration primitive: a secondary cannot replay a
    /// column-family drop and must reopen after the primary migrates.
    pub(crate) fn recreate_column_family(&self, table: StorageTable) -> Result<(), StoreError> {
        let name = table.column_family_name();
        if self.db.cf_handle(name).is_some() {
            self.db
                .drop_cf(name)
                .map_err(StoreError::storage_unavailable)?;
        }
        let options = column_family_options(table, &self.block_cache, self.resource_budget);
        self.db
            .create_cf(name, &options)
            .map_err(StoreError::storage_unavailable)
    }

    /// Drops an extra column family opened only for schema migration.
    pub(crate) fn drop_column_family(&self, name: &'static str) -> Result<(), StoreError> {
        if self.db.cf_handle(name).is_some() {
            self.db
                .drop_cf(name)
                .map_err(StoreError::storage_unavailable)?;
        }
        Ok(())
    }

    /// Walks two column families in one ordered pass, pairing rows whose
    /// keys are identical after the two-byte `StoreKey` header.
    ///
    /// Both families must share the same post-header key layout. Writes
    /// issued from inside the visitor are safe: each raw iterator pins its
    /// own consistent view at creation.
    pub(crate) fn scan_tables_merged_by_key_suffix(
        &self,
        left_table: StorageTable,
        right_table: StorageTable,
        visit: &mut dyn FnMut(MergedTableRow<'_>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let left_column_family = self.column_family(left_table)?;
        let right_column_family = self.column_family(right_table)?;
        let mut left = self.db.raw_iterator_cf(&left_column_family);
        let mut right = self.db.raw_iterator_cf(&right_column_family);
        left.seek_to_first();
        right.seek_to_first();

        loop {
            let left_item = if left.valid() { left.item() } else { None };
            let right_item = if right.valid() { right.item() } else { None };
            match (left_item, right_item) {
                (None, None) => break,
                (Some((left_key, left_value)), None) => {
                    visit(MergedTableRow::LeftOnly {
                        key: left_key,
                        value: left_value,
                    })?;
                    left.next();
                }
                (None, Some((right_key, right_value))) => {
                    visit(MergedTableRow::RightOnly {
                        key: right_key,
                        value: right_value,
                    })?;
                    right.next();
                }
                (Some((left_key, left_value)), Some((right_key, right_value))) => {
                    match key_suffix(left_key).cmp(key_suffix(right_key)) {
                        std::cmp::Ordering::Less => {
                            visit(MergedTableRow::LeftOnly {
                                key: left_key,
                                value: left_value,
                            })?;
                            left.next();
                        }
                        std::cmp::Ordering::Greater => {
                            visit(MergedTableRow::RightOnly {
                                key: right_key,
                                value: right_value,
                            })?;
                            right.next();
                        }
                        std::cmp::Ordering::Equal => {
                            visit(MergedTableRow::Matched {
                                left_key,
                                left_value,
                                right_value,
                            })?;
                            left.next();
                            right.next();
                        }
                    }
                }
            }
        }
        left.status().map_err(StoreError::storage_unavailable)?;
        right.status().map_err(StoreError::storage_unavailable)?;

        Ok(())
    }

    /// Forces every column family's active memtable to flush to `SST`
    /// and truncates the WAL.
    ///
    /// Used by `zinder-ingest` between `BulkCatchup` batches to keep the
    /// live WAL bounded by writer cadence instead of by `RocksDB`'s
    /// WAL-size trigger alone. Atomic-flush is enabled at open, and
    /// `flush_cfs_opt` names every column family that participates in the
    /// per-epoch commit contract documented in ADR-0001.
    pub(crate) fn flush(&self) -> Result<(), StoreError> {
        let column_families = StorageTable::all()
            .into_iter()
            .map(|table| self.column_family(table))
            .collect::<Result<Vec<_>, _>>()?;
        let column_family_refs = column_families.iter().collect::<Vec<_>>();
        self.db
            .flush_cfs_opt(&column_family_refs, &FlushOptions::default())
            .map_err(StoreError::storage_unavailable)
    }

    pub(crate) fn create_checkpoint(&self, path: impl AsRef<Path>) -> Result<(), StoreError> {
        let checkpoint =
            Checkpoint::new(self.db.as_ref()).map_err(StoreError::storage_unavailable)?;
        checkpoint
            .create_checkpoint(path.as_ref())
            .map_err(|source| StoreError::checkpoint_unavailable(path.as_ref(), source))
    }

    #[cfg(test)]
    pub(crate) fn delete(&self, table: StorageTable, key: &StoreKey) -> Result<(), StoreError> {
        let column_family = self.column_family(table)?;
        let mut write_options = WriteOptions::default();
        write_options.set_sync(self.sync_writes);

        self.db
            .delete_cf_opt(&column_family, key.as_bytes(), &write_options)
            .map_err(StoreError::storage_unavailable)
    }

    fn column_family(
        &self,
        table: StorageTable,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, StoreError> {
        self.db
            .cf_handle(table.column_family_name())
            .ok_or(StoreError::Unsupported {
                feature: table.column_family_name(),
            })
    }

    fn record_rocksdb_properties(&self) {
        let column_family_names = StorageTable::all()
            .into_iter()
            .map(StorageTable::column_family_name)
            .collect::<Vec<_>>();
        record_rocksdb_resource_gauges(&RocksDbResourceGaugeInputs {
            db: &self.db,
            store_role: self.store_role,
            column_family_names: &column_family_names,
            block_cache: &self.block_cache,
            write_buffer_manager: &self.write_buffer_manager,
            statistics: &self.statistics,
            io_mode: self.io_mode,
            resource_budget: self.resource_budget,
        });
    }
}

/// Inputs for [`record_rocksdb_resource_gauges`].
pub struct RocksDbResourceGaugeInputs<'sample> {
    /// Opened database whose properties are sampled.
    pub db: &'sample DB,
    /// Store domain and open posture, emitted as the `store_role` label.
    pub store_role: StoreRole,
    /// Column families to probe for the per-CF integer properties.
    pub column_family_names: &'sample [&'static str],
    /// Shared block cache retained for the database lifetime.
    pub block_cache: &'sample Cache,
    /// Shared write-buffer manager retained for the database lifetime.
    pub write_buffer_manager: &'sample WriteBufferManager,
    /// Statistics-bearing open options whose DB-wide tickers are exported.
    pub statistics: &'sample Options,
    /// Resolved filesystem I/O mode.
    pub io_mode: RocksDbIoMode,
    /// Bounded resource budget applied at open.
    pub resource_budget: RocksDbResourceBudget,
}

/// Walks the on-disk WAL files and returns their combined size in bytes.
///
/// Returns `None` when the store path is unreadable; the metric scrape treats
/// that as "no sample" rather than failing closed.
fn wal_size_bytes(db: &DB) -> Option<u64> {
    let entries = fs::read_dir(db.path()).ok()?;
    let mut total_bytes = 0_u64;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Some(extension) = entry_path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !extension.eq_ignore_ascii_case("log") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        total_bytes = total_bytes.saturating_add(metadata.len());
    }
    Some(total_bytes)
}

/// Publishes the bounded-`RocksDB` resource gauges under a `store_role` label.
///
/// Shared by the canonical chain store and the derive store so both instances
/// contribute distinguishable cache, memtable, WAL, and per-CF footprints to
/// the same series. `store_role` keeps the aggregate resident set attributable
/// to each instance.
pub fn record_rocksdb_resource_gauges(inputs: &RocksDbResourceGaugeInputs<'_>) {
    let store_role = inputs.store_role.as_str();
    for cf_name in inputs.column_family_names {
        let Some(column_family) = inputs.db.cf_handle(cf_name) else {
            continue;
        };
        for property in PER_CF_INT_PROPERTIES {
            let Ok(Some(property_sample)) =
                inputs.db.property_int_value_cf(&column_family, property)
            else {
                continue;
            };
            metrics::gauge!(
                "zinder_store_rocksdb_property",
                "property" => property,
                "cf" => *cf_name,
                "store_role" => store_role
            )
            .set(u64_to_f64(property_sample));
        }
    }
    for property in DB_INT_PROPERTIES {
        let Ok(Some(property_sample)) = inputs.db.property_int_value(property) else {
            continue;
        };
        metrics::gauge!(
            "zinder_store_rocksdb_property",
            "property" => property,
            "cf" => DB_LEVEL_PROPERTY_CF,
            "store_role" => store_role
        )
        .set(u64_to_f64(property_sample));
    }
    if let Some(wal_bytes) = wal_size_bytes(inputs.db) {
        metrics::gauge!("zinder_store_wal_bytes", "store_role" => store_role)
            .set(u64_to_f64(wal_bytes));
    }
    metrics::gauge!("zinder_store_wal_bytes_limit", "store_role" => store_role)
        .set(u64_to_f64(inputs.resource_budget.max_wal_bytes));
    metrics::gauge!("zinder_store_block_cache_capacity_bytes", "store_role" => store_role)
        .set(u64_to_f64(inputs.resource_budget.block_cache_bytes));
    metrics::gauge!("zinder_store_block_cache_usage_bytes", "store_role" => store_role)
        .set(usize_to_f64(inputs.block_cache.get_usage()));
    metrics::gauge!("zinder_store_block_cache_pinned_usage_bytes", "store_role" => store_role)
        .set(usize_to_f64(inputs.block_cache.get_pinned_usage()));
    record_rocksdb_ticker_gauges(inputs.statistics, store_role);
    metrics::gauge!("zinder_store_memtable_budget_bytes", "store_role" => store_role)
        .set(usize_to_f64(inputs.write_buffer_manager.get_buffer_size()));
    metrics::gauge!("zinder_store_memtable_budget_usage_bytes", "store_role" => store_role)
        .set(usize_to_f64(inputs.write_buffer_manager.get_usage()));
    metrics::gauge!(
        "zinder_store_rocksdb_io_mode",
        "mode" => inputs.io_mode.as_str(),
        "store_role" => store_role
    )
    .set(1.0);
}

/// DB-wide `RocksDB` statistics tickers exported as monotonic gauges.
///
/// Every ticker is aggregated across all column families of one store; the
/// bloom entries reflect only `transparent_output` because it is the sole
/// column family with a filter policy.
const EXPORTED_TICKERS: &[Ticker] = &[
    Ticker::BloomFilterUseful,
    Ticker::BloomFilterFullPositive,
    Ticker::BloomFilterFullTruePositive,
    Ticker::BlockCacheDataHit,
    Ticker::BlockCacheDataMiss,
    Ticker::BlockCacheIndexHit,
    Ticker::BlockCacheIndexMiss,
    Ticker::BlockCacheFilterHit,
    Ticker::BlockCacheFilterMiss,
    Ticker::NumberMultigetCalls,
    Ticker::NumberMultigetKeysRead,
    Ticker::NumberMultigetBytesRead,
    Ticker::BytesRead,
    Ticker::BytesWritten,
    Ticker::StallMicros,
    Ticker::CompactReadBytes,
    Ticker::CompactWriteBytes,
];

/// Publishes [`EXPORTED_TICKERS`] under `zinder_store_rocksdb_ticker`, keyed by
/// the upstream `RocksDB` ticker name and the store role.
fn record_rocksdb_ticker_gauges(statistics: &Options, store_role: &'static str) {
    for ticker in EXPORTED_TICKERS {
        metrics::gauge!(
            "zinder_store_rocksdb_ticker",
            "ticker" => ticker.name(),
            "store_role" => store_role
        )
        .set(u64_to_f64(statistics.get_ticker_count(*ticker)));
    }
}

/// Sampling cadence for the resource-gauge sweep on the write path.
///
/// Sits below the 15s Prometheus scrape interval so scrapes always read a
/// fresh sample while a commit burst probes the sweep at most once per second.
const RESOURCE_GAUGE_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Rate-limits the `RocksDB` resource-gauge sweep so a burst of small commits
/// probes every property at most once per interval instead of once per write.
///
/// The sweep in [`record_rocksdb_resource_gauges`] acquires the `RocksDB` DB
/// mutex for per-column-family properties and reads the WAL directory, so
/// running it per commit couples that cost to write rate on a store whose
/// smallest write unit is a single mempool event.
pub struct ResourceGaugeThrottle {
    interval: Duration,
    last_sampled_at: Mutex<Option<Instant>>,
}

impl ResourceGaugeThrottle {
    /// Builds a throttle that admits one sample per `interval`.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sampled_at: Mutex::new(None),
        }
    }

    /// Returns `true` when a sweep is due, recording the sample instant.
    ///
    /// The first call always admits; a later call admits only once `interval`
    /// has elapsed since the last admitted sample.
    pub fn should_sample(&self) -> bool {
        let now = Instant::now();
        let mut last_sampled_at = self.last_sampled_at.lock();
        if last_sampled_at.is_some_and(|previous| now.duration_since(previous) < self.interval) {
            return false;
        }
        *last_sampled_at = Some(now);
        true
    }
}

impl Default for ResourceGaugeThrottle {
    fn default() -> Self {
        Self::new(RESOURCE_GAUGE_SAMPLE_INTERVAL)
    }
}

/// Builds `RocksDB` options for any writer-posture instance in the workspace.
///
/// The canonical chain store and the derive store both route through this
/// factory; ADR-0020 describes the bounded resource budget applied here.
///
/// Locked invariants applied here are non-tunable: write-ahead logging on
/// (`disable_wal=false`), point-in-time recovery on (`RocksDB` default),
/// atomic cross-CF flush on (`set_atomic_flush(true)`), and ordered
/// writes on (`unordered_write=false`). See
/// [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md)
/// for the full design.
#[must_use]
fn build_primary_db_options(
    rocksdb_resource_budget: RocksDbResourceBudget,
    cache: &Cache,
) -> Options {
    let mut db_options = Options::default();
    db_options.create_if_missing(true);
    db_options.create_missing_column_families(true);
    db_options.enable_statistics();
    db_options.set_max_total_wal_size(rocksdb_resource_budget.max_wal_bytes);
    db_options.set_max_open_files(rocksdb_resource_budget.max_open_files);
    db_options.set_atomic_flush(true);
    db_options.set_block_based_table_factory(&build_block_based_table_factory(cache));
    db_options
}

/// Builds `RocksDB` options for a secondary replica with the same
/// bounded resource budget as the primary.
///
/// Secondaries replay the writer's WAL but do not generate one, so the WAL
/// ceiling and atomic-flush flag are not applied here. Per-DB resource caps
/// (block cache, open file handles) apply identically because the secondary's
/// open-time RAM peak grows with store size the same way the primary's does.
#[must_use]
fn build_secondary_db_options(
    rocksdb_resource_budget: RocksDbResourceBudget,
    cache: &Cache,
) -> Options {
    let mut db_options = Options::default();
    db_options.create_if_missing(false);
    db_options.create_missing_column_families(false);
    db_options.enable_statistics();
    db_options.set_max_open_files(rocksdb_resource_budget.max_open_files);
    db_options.set_block_based_table_factory(&build_block_based_table_factory(cache));
    db_options
}

/// Allocates the bounded LRU block cache shared by every column family.
///
/// The cache holds data, index, and bloom-filter blocks together so total
/// resident metadata stays bounded by `capacity_bytes`. Cloning the
/// returned handle shares the underlying allocation (`RocksDB`'s `Cache`
/// is reference counted), so callers needing the same cache across
/// multiple open paths pass `&` references rather than reconstructing.
#[must_use]
fn build_block_cache(capacity_bytes: u64) -> Cache {
    let capacity = usize::try_from(capacity_bytes).unwrap_or(usize::MAX);
    Cache::new_lru_cache(capacity)
}

fn build_write_buffer_manager(memtable_budget_bytes: u64) -> WriteBufferManager {
    let capacity = usize::try_from(memtable_budget_bytes).unwrap_or(usize::MAX);
    WriteBufferManager::new_write_buffer_manager(capacity, false)
}

/// Builds the shared `BlockBasedOptions` every column family routes through.
///
/// Index and filter blocks are accounted to `cache` so the at-rest
/// metadata budget stays bounded by [`RocksDbResourceBudget::block_cache_bytes`].
/// Public because the derive store re-uses the same factory; see
/// [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
#[must_use]
pub fn build_block_based_table_factory(cache: &Cache) -> BlockBasedOptions {
    let mut bbt = BlockBasedOptions::default();
    bbt.set_block_cache(cache);
    bbt.set_cache_index_and_filter_blocks(true);
    bbt.set_pin_l0_filter_and_index_blocks_in_cache(true);
    bbt
}

pub(crate) trait RocksChainStoreRead {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError>;

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError>;

    fn sorted_multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        self.multi_get(table, keys)
    }

    fn get_previous_by_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError>;

    fn scan_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError>;

    fn scan_prefix_reverse(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError>;

    fn scan_prefix_reverse_before(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        before: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError>;

    fn scan_forward(
        &self,
        table: StorageTable,
        start_key: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError>;
}

pub(crate) enum PrefixScanControl {
    Continue,
    Stop,
}

/// One step of a two-table ordered merge over a shared post-header key layout.
#[derive(Clone, Copy)]
pub(crate) enum MergedTableRow<'row> {
    /// Key present only in the left table.
    LeftOnly { key: &'row [u8], value: &'row [u8] },
    /// Key present only in the right table.
    RightOnly { key: &'row [u8], value: &'row [u8] },
    /// Key present in both tables.
    Matched {
        left_key: &'row [u8],
        left_value: &'row [u8],
        right_value: &'row [u8],
    },
}

fn key_suffix(key: &[u8]) -> &[u8] {
    key.get(2..).unwrap_or(key)
}

impl RocksChainStoreRead for RocksChainStore {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        Self::get(self, StoreReadCaller::Query, table, key)
    }

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        Self::multi_get(self, StoreReadCaller::Query, table, keys)
    }

    fn sorted_multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        Self::sorted_multi_get(self, StoreReadCaller::Query, table, keys)
    }

    fn get_previous_by_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        Self::get_previous_by_prefix(self, StoreReadCaller::Query, table, prefix, seek_key)
    }

    fn scan_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        Self::scan_prefix(self, StoreReadCaller::Query, table, prefix, visit)
    }

    fn scan_prefix_reverse(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        Self::scan_prefix_reverse(self, StoreReadCaller::Query, table, prefix, visit)
    }

    fn scan_prefix_reverse_before(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        before: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        Self::scan_prefix_reverse_before(self, table, prefix, before, visit)
    }

    fn scan_forward(
        &self,
        table: StorageTable,
        start_key: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        Self::scan_forward(self, StoreReadCaller::Query, table, start_key, visit)
    }
}

pub(crate) struct RocksChainStoreSnapshot<'store> {
    store: &'store RocksChainStore,
    snapshot: Snapshot<'store>,
    caller: StoreReadCaller,
}

pub(crate) enum RocksChainStoreReadView<'store> {
    Snapshot(RocksChainStoreSnapshot<'store>),
    Direct {
        store: &'store RocksChainStore,
        caller: StoreReadCaller,
    },
}

impl RocksChainStoreRead for RocksChainStoreReadView<'_> {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.get(table, key),
            Self::Direct { store, caller } => store.get(*caller, table, key),
        }
    }

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.multi_get(table, keys),
            Self::Direct { store, caller } => store.multi_get(*caller, table, keys),
        }
    }

    fn sorted_multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.multi_get(table, keys),
            Self::Direct { store, caller } => store.sorted_multi_get(*caller, table, keys),
        }
    }

    fn get_previous_by_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.get_previous_by_prefix(table, prefix, seek_key),
            Self::Direct { store, caller } => {
                store.get_previous_by_prefix(*caller, table, prefix, seek_key)
            }
        }
    }

    fn scan_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.scan_prefix(table, prefix, visit),
            Self::Direct { store, caller } => store.scan_prefix(*caller, table, prefix, visit),
        }
    }

    fn scan_prefix_reverse(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.scan_prefix_reverse(table, prefix, visit),
            Self::Direct { store, caller } => {
                store.scan_prefix_reverse(*caller, table, prefix, visit)
            }
        }
    }

    fn scan_prefix_reverse_before(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        before: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        match self {
            Self::Snapshot(snapshot) => {
                snapshot.scan_prefix_reverse_before(table, prefix, before, visit)
            }
            Self::Direct { store, caller: _ } => {
                store.scan_prefix_reverse_before(table, prefix, before, visit)
            }
        }
    }

    fn scan_forward(
        &self,
        table: StorageTable,
        start_key: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.scan_forward(table, start_key, visit),
            Self::Direct { store, caller } => store.scan_forward(*caller, table, start_key, visit),
        }
    }
}

impl RocksChainStoreRead for RocksChainStoreSnapshot<'_> {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        let column_family = self.store.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .snapshot
            .get_cf(&column_family, key.as_bytes())
            .map_err(StoreError::storage_unavailable);
        record_store_read_outcome("get", self.caller, table, started_at, &read_outcome);

        read_outcome
    }

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let column_family = self.store.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .snapshot
            .multi_get_cf(keys.iter().map(|key| (&column_family, key.as_bytes())))
            .into_iter()
            .map(|rocksdb_result| rocksdb_result.map_err(StoreError::storage_unavailable))
            .collect();
        record_store_multi_get_outcome(self.caller, table, started_at, keys.len(), &read_outcome);

        read_outcome
    }

    fn get_previous_by_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let started_at = Instant::now();
        let read_outcome = (|| {
            let column_family = self.store.column_family(table)?;
            let mut iterator = self.snapshot.raw_iterator_cf(&column_family);

            iterator.seek_for_prev(seek_key.as_bytes());
            if !iterator.valid() {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(None);
            }

            let Some((key, index_value)) = iterator.item() else {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(None);
            };

            if !key.starts_with(prefix.as_bytes()) {
                return Ok(None);
            }

            Ok(Some(index_value.to_vec()))
        })();
        record_store_read_outcome(
            "seek_for_prev",
            self.caller,
            table,
            started_at,
            &read_outcome,
        );

        read_outcome
    }

    fn scan_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.store.column_family(table)?;
            let mut iterator = self.snapshot.raw_iterator_cf(&column_family);
            iterator.seek(prefix.as_bytes());
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.next();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(self.caller, table, started_at, &scan_outcome);

        scan_outcome
    }

    fn scan_prefix_reverse(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.store.column_family(table)?;
            let mut iterator = self.snapshot.raw_iterator_cf(&column_family);
            if let Some(upper_bound) = exclusive_prefix_upper_bound(prefix.as_bytes()) {
                iterator.seek(&upper_bound);
                if iterator.valid() {
                    iterator.prev();
                } else {
                    iterator.seek_to_last();
                }
            } else {
                iterator.seek_to_last();
            }
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.prev();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(self.caller, table, started_at, &scan_outcome);

        scan_outcome
    }

    fn scan_prefix_reverse_before(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        before: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let column_family = self.store.column_family(table)?;
        let mut iterator = self.snapshot.raw_iterator_cf(&column_family);
        iterator.seek_for_prev(before.as_bytes());
        if iterator.key() == Some(before.as_bytes()) {
            iterator.prev();
        }
        while iterator.valid() {
            let Some((key, row_value)) = iterator.item() else {
                iterator.status().map_err(StoreError::storage_unavailable)?;
                return Ok(());
            };
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                break;
            }
            iterator.prev();
        }
        iterator.status().map_err(StoreError::storage_unavailable)
    }

    fn scan_forward(
        &self,
        table: StorageTable,
        start_key: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        let started_at = Instant::now();
        let scan_outcome = (|| {
            let column_family = self.store.column_family(table)?;
            let mut iterator = self.snapshot.raw_iterator_cf(&column_family);
            iterator.seek(start_key.as_bytes());
            while iterator.valid() {
                let Some((key, row_value)) = iterator.item() else {
                    iterator.status().map_err(StoreError::storage_unavailable)?;
                    return Ok(());
                };
                if matches!(visit(key, row_value)?, PrefixScanControl::Stop) {
                    break;
                }
                iterator.next();
            }
            iterator.status().map_err(StoreError::storage_unavailable)?;
            Ok(())
        })();
        record_store_scan_outcome(self.caller, table, started_at, &scan_outcome);

        scan_outcome
    }
}

fn exclusive_prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper_bound = prefix.to_vec();
    let increment_index = upper_bound.iter().rposition(|byte| *byte != u8::MAX)?;
    upper_bound[increment_index] = upper_bound[increment_index].saturating_add(1);
    upper_bound.truncate(increment_index.saturating_add(1));
    Some(upper_bound)
}

const PER_CF_INT_PROPERTIES: [&str; 7] = [
    "rocksdb.estimate-live-data-size",
    "rocksdb.total-sst-files-size",
    "rocksdb.size-all-mem-tables",
    "rocksdb.cur-size-active-mem-table",
    "rocksdb.estimate-table-readers-mem",
    "rocksdb.estimate-pending-compaction-bytes",
    "rocksdb.num-running-compactions",
];

/// Write-controller properties reported by the whole database instance rather
/// than per column family.
const DB_INT_PROPERTIES: [&str; 2] = [
    "rocksdb.actual-delayed-write-rate",
    "rocksdb.is-write-stopped",
];

/// `cf` label value for DB-level properties, which have no owning column family.
const DB_LEVEL_PROPERTY_CF: &str = "__db__";

fn record_store_read_outcome(
    operation: &'static str,
    caller: StoreReadCaller,
    table: StorageTable,
    started_at: Instant,
    read_outcome: &Result<Option<Vec<u8>>, StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => operation,
        "table" => table.column_family_name(),
        "caller" => caller.as_str(),
        "status" => outcome_status(read_outcome)
    )
    .record(started_at.elapsed());

    if let Ok(Some(bytes)) = read_outcome {
        metrics::counter!(
            "zinder_store_read_bytes_total",
            "operation" => operation,
            "table" => table.column_family_name()
        )
        .increment(usize_to_u64(bytes.len()));
    }
}

fn record_store_multi_get_outcome(
    caller: StoreReadCaller,
    table: StorageTable,
    started_at: Instant,
    key_count: usize,
    read_outcome: &Result<Vec<Option<Vec<u8>>>, StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => "multi_get",
        "table" => table.column_family_name(),
        "caller" => caller.as_str(),
        "status" => outcome_status(read_outcome)
    )
    .record(started_at.elapsed());
    metrics::histogram!(
        "zinder_store_multi_get_key_count",
        "table" => table.column_family_name(),
        "status" => outcome_status(read_outcome)
    )
    .record(usize_to_u32_saturating(key_count));
    metrics::counter!(
        "zinder_store_multi_get_keys_total",
        "table" => table.column_family_name(),
        "caller" => caller.as_str()
    )
    .increment(usize_to_u64(key_count));

    if let Ok(read_items) = read_outcome {
        let resolved_count = read_items.iter().filter(|row| row.is_some()).count();
        metrics::counter!(
            "zinder_store_multi_get_resolved_total",
            "table" => table.column_family_name(),
            "caller" => caller.as_str()
        )
        .increment(usize_to_u64(resolved_count));
        let byte_count = read_items
            .iter()
            .flatten()
            .map(Vec::len)
            .fold(0_u64, |total, len| total.saturating_add(usize_to_u64(len)));
        metrics::counter!(
            "zinder_store_read_bytes_total",
            "operation" => "multi_get",
            "table" => table.column_family_name()
        )
        .increment(byte_count);
    }
}

fn record_store_scan_outcome(
    caller: StoreReadCaller,
    table: StorageTable,
    started_at: Instant,
    scan_outcome: &Result<(), StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => "scan_prefix",
        "table" => table.column_family_name(),
        "caller" => caller.as_str(),
        "status" => outcome_status(scan_outcome)
    )
    .record(started_at.elapsed());
}

fn record_write_batch_inputs(puts: &[StoragePut], deletes: &[StorageDelete]) {
    for put in puts {
        metrics::counter!(
            "zinder_store_write_batch_rows_total",
            "kind" => "put",
            "table" => put.table.column_family_name()
        )
        .increment(1);
        metrics::counter!(
            "zinder_store_write_batch_bytes_total",
            "kind" => "put",
            "table" => put.table.column_family_name()
        )
        .increment(usize_to_u64(put.value.len()));
    }

    for delete in deletes {
        metrics::counter!(
            "zinder_store_write_batch_rows_total",
            "kind" => "delete",
            "table" => delete.table.column_family_name()
        )
        .increment(1);
    }
}

fn record_write_batch_outcome(started_at: Instant, write_outcome: &Result<(), StoreError>) {
    metrics::histogram!(
        "zinder_store_write_batch_duration_seconds",
        "status" => outcome_status(write_outcome)
    )
    .record(started_at.elapsed());
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn usize_to_u64(amount: usize) -> u64 {
    u64::try_from(amount).map_or(u64::MAX, |converted| converted)
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).map_or(u32::MAX, |converted| converted)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; RocksDB property values are diagnostic magnitudes"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; block cache sizes are diagnostic magnitudes"
)]
fn usize_to_f64(sample: usize) -> f64 {
    sample as f64
}

/// Per-CF memtable budget for column families whose live data stays under a
/// few hundred KiB.
///
/// The rust-rocksdb default `write_buffer_size` is 64 MiB. With
/// `set_memtable_prefix_bloom_ratio(0.2)`, that reserves ~12.8 MiB for the
/// memtable's prefix-bloom filter alone, regardless of how many rows the CF
/// actually holds. Tip-only CFs that hold O(reorg-window) rows do not need
/// that budget; capping the buffer to 4 MiB keeps the bloom proportional and
/// shrinks the writer's resident memtable footprint by an order of magnitude.
const SMALL_CF_WRITE_BUFFER_BYTES: usize = 4 * 1024 * 1024;

fn column_family_options(
    table: StorageTable,
    cache: &Cache,
    rocksdb_resource_budget: RocksDbResourceBudget,
) -> Options {
    let mut options = Options::default();
    let mut table_factory = build_block_based_table_factory(cache);

    if table == StorageTable::TransparentOutput {
        table_factory.set_bloom_filter(10.0, false);
        options.set_memtable_batch_lookup_optimization(true);
    }

    options.set_block_based_table_factory(&table_factory);
    options.set_write_buffer_size(write_buffer_bytes_for_table(table, rocksdb_resource_budget));
    options.set_max_write_buffer_number(rocksdb_resource_budget.max_write_buffer_count);

    if table == StorageTable::ReorgWindow {
        options.set_prefix_extractor(SliceTransform::create(
            "zinder_reorg_window_visibility_prefix",
            reorg_window_visibility_prefix,
            Some(is_reorg_window_visibility_key),
        ));
        options.set_memtable_prefix_bloom_ratio(0.2);
        options.set_optimize_filters_for_hits(true);
    }

    options
}

fn extra_column_family_options(
    cache: &Cache,
    rocksdb_resource_budget: RocksDbResourceBudget,
) -> Options {
    let mut options = Options::default();
    options.set_block_based_table_factory(&build_block_based_table_factory(cache));
    options.set_write_buffer_size(
        usize::try_from(rocksdb_resource_budget.write_buffer_bytes).unwrap_or(usize::MAX),
    );
    options.set_max_write_buffer_number(rocksdb_resource_budget.max_write_buffer_count);
    options
}

fn write_buffer_bytes_for_table(
    table: StorageTable,
    rocksdb_resource_budget: RocksDbResourceBudget,
) -> usize {
    let budgeted_bytes =
        usize::try_from(rocksdb_resource_budget.write_buffer_bytes).unwrap_or(usize::MAX);
    if table == StorageTable::ReorgWindow {
        return budgeted_bytes.min(SMALL_CF_WRITE_BUFFER_BYTES);
    }
    budgeted_bytes
}

fn reorg_window_visibility_prefix(key: &[u8]) -> &[u8] {
    let prefix_len = StoreKey::reorg_window_prefix_len(key).unwrap_or(key.len());
    &key[..prefix_len]
}

fn is_reorg_window_visibility_key(key: &[u8]) -> bool {
    StoreKey::reorg_window_prefix_len(key).is_some()
}

pub(crate) struct StoragePut {
    pub(crate) table: StorageTable,
    pub(crate) key: StoreKey,
    pub(crate) value: Vec<u8>,
}

pub(crate) struct StorageDelete {
    pub(crate) table: StorageTable,
    pub(crate) key: StoreKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_gauge_throttle_admits_first_sample_then_holds_within_interval() {
        let throttle = ResourceGaugeThrottle::new(Duration::from_hours(1));
        assert!(throttle.should_sample());
        assert!(!throttle.should_sample());
        assert!(!throttle.should_sample());
    }

    #[test]
    fn resource_gauge_throttle_readmits_after_interval() {
        let throttle = ResourceGaugeThrottle::new(Duration::ZERO);
        assert!(throttle.should_sample());
        assert!(throttle.should_sample());
    }

    #[test]
    fn transparent_output_cf_enables_memtable_batch_lookup() {
        let budget = RocksDbResourceBudget::for_local_tests();
        let cache = build_block_cache(budget.block_cache_bytes);

        let transparent_output_options =
            column_family_options(StorageTable::TransparentOutput, &cache, budget);
        assert!(transparent_output_options.get_memtable_batch_lookup_optimization());

        for table in StorageTable::all() {
            if table == StorageTable::TransparentOutput {
                continue;
            }
            let options = column_family_options(table, &cache, budget);
            assert!(
                !options.get_memtable_batch_lookup_optimization(),
                "{} must not enable memtable batch lookup",
                table.column_family_name()
            );
        }
    }

    #[test]
    fn ticker_export_runs_on_an_open_store() -> Result<(), StoreError> {
        let store_dir = tempfile::tempdir().map_err(StoreError::storage_unavailable)?;
        let store = RocksChainStore::open_primary(
            store_dir.path(),
            false,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        store.record_rocksdb_properties();
        Ok(())
    }
}
