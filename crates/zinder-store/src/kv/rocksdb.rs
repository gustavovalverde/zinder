use std::{path::Path, sync::Arc, time::Instant};

use parking_lot::{Mutex, MutexGuard};
use rust_rocksdb::{
    ColumnFamilyDescriptor, DB, Options, SliceTransform, Snapshot, WriteBatch, WriteOptions,
    checkpoint::Checkpoint,
};

use crate::{StoreError, format::StoreKey, kv::StorageTable};

type PrefixScanVisitor<'visitor> =
    &'visitor mut dyn FnMut(&[u8], &[u8]) -> Result<PrefixScanControl, StoreError>;

#[derive(Clone)]
pub(crate) struct RocksChainStore {
    db: Arc<DB>,
    control_lock: Arc<Mutex<()>>,
    sync_writes: bool,
}

impl RocksChainStore {
    pub(crate) fn open_primary(
        path: impl AsRef<Path>,
        sync_writes: bool,
    ) -> Result<Self, StoreError> {
        let db_options = primary_db_options();
        let column_families = StorageTable::all()
            .into_iter()
            .map(|table| {
                ColumnFamilyDescriptor::new(
                    table.column_family_name(),
                    column_family_options(table),
                )
            })
            .collect::<Vec<_>>();

        let db = DB::open_cf_descriptors(&db_options, path.as_ref(), column_families)
            .map_err(|source| StoreError::primary_open_failed(path.as_ref(), source))?;

        let store = Self {
            db: Arc::new(db),
            control_lock: Arc::new(Mutex::new(())),
            sync_writes,
        };
        store.record_rocksdb_properties();

        Ok(store)
    }

    pub(crate) fn open_secondary(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        sync_writes: bool,
    ) -> Result<Self, StoreError> {
        let db_options = secondary_db_options();
        let column_families = StorageTable::all()
            .into_iter()
            .map(|table| {
                ColumnFamilyDescriptor::new(
                    table.column_family_name(),
                    column_family_options(table),
                )
            })
            .collect::<Vec<_>>();

        let db = DB::open_cf_descriptors_as_secondary(
            &db_options,
            primary_path.as_ref(),
            secondary_path.as_ref(),
            column_families,
        )
        .map_err(StoreError::storage_unavailable)?;

        let store = Self {
            db: Arc::new(db),
            control_lock: Arc::new(Mutex::new(())),
            sync_writes,
        };
        store.record_rocksdb_properties();

        Ok(store)
    }

    pub(crate) fn lock_control(&self) -> MutexGuard<'_, ()> {
        self.control_lock.lock()
    }

    pub(crate) fn get(
        &self,
        table: StorageTable,
        key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let column_family = self.column_family(table)?;
        let started_at = Instant::now();
        let read_outcome = self
            .db
            .get_cf(&column_family, key.as_bytes())
            .map_err(StoreError::storage_unavailable);
        record_store_read_outcome("get", table, started_at, &read_outcome);

        read_outcome
    }

    pub(crate) fn snapshot(&self) -> RocksChainStoreSnapshot<'_> {
        RocksChainStoreSnapshot {
            store: self,
            snapshot: Snapshot::new(self.db.as_ref()),
        }
    }

    pub(crate) fn direct_read_view(&self) -> RocksChainStoreReadView<'_> {
        RocksChainStoreReadView::Direct(self)
    }

    pub(crate) fn snapshot_read_view(&self) -> RocksChainStoreReadView<'_> {
        RocksChainStoreReadView::Snapshot(self.snapshot())
    }

    pub(crate) fn multi_get(
        &self,
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
        record_store_multi_get_outcome(table, started_at, keys.len(), &read_outcome);

        read_outcome
    }

    pub(crate) fn get_previous_by_prefix(
        &self,
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
        record_store_read_outcome("seek_for_prev", table, started_at, &read_outcome);

        read_outcome
    }

    pub(crate) fn scan_prefix(
        &self,
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
        record_store_scan_outcome(table, started_at, &scan_outcome);

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
        if write_outcome.is_ok() {
            self.record_rocksdb_properties();
        }

        write_outcome
    }

    pub(crate) fn try_catch_up_with_primary(&self) -> Result<(), StoreError> {
        self.db
            .try_catch_up_with_primary()
            .map_err(StoreError::secondary_catchup_failed)
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
        for table in StorageTable::all() {
            let Ok(column_family) = self.column_family(table) else {
                continue;
            };
            for property in ROCKSDB_INT_PROPERTIES {
                let Ok(Some(property_sample)) =
                    self.db.property_int_value_cf(&column_family, property)
                else {
                    continue;
                };
                metrics::gauge!(
                    "zinder_store_rocksdb_property",
                    "property" => property,
                    "cf" => table.column_family_name()
                )
                .set(u64_to_f64(property_sample));
            }
        }
    }
}

fn primary_db_options() -> Options {
    let mut db_options = Options::default();
    db_options.create_if_missing(true);
    db_options.create_missing_column_families(true);
    db_options.enable_statistics();
    db_options
}

fn secondary_db_options() -> Options {
    let mut db_options = Options::default();
    db_options.create_if_missing(false);
    db_options.create_missing_column_families(false);
    db_options.set_max_open_files(-1);
    db_options.enable_statistics();
    db_options
}

pub(crate) trait RocksChainStoreRead {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError>;

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError>;

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
}

pub(crate) enum PrefixScanControl {
    Continue,
    Stop,
}

impl RocksChainStoreRead for RocksChainStore {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        Self::get(self, table, key)
    }

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        Self::multi_get(self, table, keys)
    }

    fn get_previous_by_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        seek_key: &StoreKey,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        Self::get_previous_by_prefix(self, table, prefix, seek_key)
    }

    fn scan_prefix(
        &self,
        table: StorageTable,
        prefix: &StoreKey,
        visit: PrefixScanVisitor<'_>,
    ) -> Result<(), StoreError> {
        Self::scan_prefix(self, table, prefix, visit)
    }
}

pub(crate) struct RocksChainStoreSnapshot<'store> {
    store: &'store RocksChainStore,
    snapshot: Snapshot<'store>,
}

pub(crate) enum RocksChainStoreReadView<'store> {
    Snapshot(RocksChainStoreSnapshot<'store>),
    Direct(&'store RocksChainStore),
}

impl RocksChainStoreRead for RocksChainStoreReadView<'_> {
    fn get(&self, table: StorageTable, key: &StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.get(table, key),
            Self::Direct(store) => store.get(table, key),
        }
    }

    fn multi_get(
        &self,
        table: StorageTable,
        keys: &[StoreKey],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.multi_get(table, keys),
            Self::Direct(store) => store.multi_get(table, keys),
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
            Self::Direct(store) => store.get_previous_by_prefix(table, prefix, seek_key),
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
            Self::Direct(store) => store.scan_prefix(table, prefix, visit),
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
        record_store_read_outcome("get", table, started_at, &read_outcome);

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
        record_store_multi_get_outcome(table, started_at, keys.len(), &read_outcome);

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
        record_store_read_outcome("seek_for_prev", table, started_at, &read_outcome);

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
        record_store_scan_outcome(table, started_at, &scan_outcome);

        scan_outcome
    }
}

const ROCKSDB_INT_PROPERTIES: [&str; 6] = [
    "rocksdb.estimate-live-data-size",
    "rocksdb.total-sst-files-size",
    "rocksdb.size-all-mem-tables",
    "rocksdb.estimate-table-readers-mem",
    "rocksdb.estimate-pending-compaction-bytes",
    "rocksdb.num-running-compactions",
];

fn record_store_read_outcome(
    operation: &'static str,
    table: StorageTable,
    started_at: Instant,
    read_outcome: &Result<Option<Vec<u8>>, StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => operation,
        "table" => table.column_family_name(),
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
    table: StorageTable,
    started_at: Instant,
    key_count: usize,
    read_outcome: &Result<Vec<Option<Vec<u8>>>, StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => "multi_get",
        "table" => table.column_family_name(),
        "status" => outcome_status(read_outcome)
    )
    .record(started_at.elapsed());
    metrics::histogram!(
        "zinder_store_multi_get_key_count",
        "table" => table.column_family_name(),
        "status" => outcome_status(read_outcome)
    )
    .record(usize_to_u32_saturating(key_count));

    if let Ok(read_items) = read_outcome {
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
    table: StorageTable,
    started_at: Instant,
    scan_outcome: &Result<(), StoreError>,
) {
    metrics::histogram!(
        "zinder_store_read_duration_seconds",
        "operation" => "scan_prefix",
        "table" => table.column_family_name(),
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

fn column_family_options(table: StorageTable) -> Options {
    let mut options = Options::default();

    if table == StorageTable::ReorgWindow {
        options.set_prefix_extractor(SliceTransform::create(
            "zinder_reorg_window_visibility_prefix",
            reorg_window_visibility_prefix,
            Some(is_reorg_window_visibility_key),
        ));
        options.set_memtable_prefix_bloom_ratio(0.2);
        options.set_optimize_filters_for_hits(true);
        options.set_write_buffer_size(SMALL_CF_WRITE_BUFFER_BYTES);
    }

    options
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
