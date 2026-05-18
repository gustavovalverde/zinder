//! `RocksDB` wrapper for the derive plane.
//!
//! `DeriveStore` is intentionally separate from `zinder_store::PrimaryChainStore`:
//! it lives in its own filesystem path, has its own column families, and uses
//! its own schema version. The two stores never share keys.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rust_rocksdb::{ColumnFamilyDescriptor, DB, IteratorMode, Options, WriteBatch, WriteOptions};

use crate::{
    consumer::DeriveConsumerName,
    error::{DeriveStoreColumnFamily, DeriveStoreError},
};

/// On-disk schema version used by the derive plane.
///
/// Bumped by the binary when the column-family layout, key schema, or
/// metadata payload format changes in a backwards-incompatible way. The
/// version is persisted in the `consumer_metadata` column family on first
/// open and validated on subsequent opens.
pub const DERIVE_SCHEMA_VERSION: u16 = 1;

const SCHEMA_VERSION_KEY: &[u8] = b"\x00\x01schema_version";

/// Per-column-family options the derive plane tunes at open time.
fn column_family_options() -> Options {
    let mut options = Options::default();
    options.set_max_write_buffer_number(2);
    options
}

fn primary_db_options() -> Options {
    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    options
}

/// Logical column-family identifier.
///
/// Mirrors `DeriveStoreColumnFamily` but lives on the public store surface
/// because callers reference column families when issuing reads. Operator
/// errors carry the same enum so the two halves stay in sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeriveStoreTable {
    /// `cursor` column family: per-consumer cursor persistence.
    Cursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl DeriveStoreTable {
    /// Returns the canonical `RocksDB` column-family name for the variant.
    #[must_use]
    pub const fn column_family_name(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::ConsumerMetadata => "consumer_metadata",
        }
    }

    fn error_family(self) -> DeriveStoreColumnFamily {
        match self {
            Self::Cursor => DeriveStoreColumnFamily::Cursor,
            Self::ConsumerMetadata => DeriveStoreColumnFamily::ConsumerMetadata,
        }
    }

    fn all() -> [Self; 2] {
        [Self::Cursor, Self::ConsumerMetadata]
    }
}

/// Configurable knobs the binary applies before opening the database.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeriveStoreOptions {
    /// When set, every write is flushed to the OS page cache before returning.
    /// Default `false` matches the canonical store's tunable so operators can
    /// trade durability for throughput in development environments.
    pub sync_writes: bool,
    /// Consumer-owned column families to register at open time. Each entry is
    /// the canonical column-family name a consumer reads and writes through
    /// [`DeriveStore::consumer_column_family`].
    pub consumer_column_families: &'static [&'static str],
}

/// Owned `(key, payload)` pair returned by
/// [`DeriveStore::range_iterate_consumer`]. Both halves are RocksDB-owned
/// bytes copied out of the iterator's borrowed buffers.
pub type ConsumerEntry = (Vec<u8>, Vec<u8>);

/// Cursor entry observed by `DeriveStore::get_cursor`.
///
/// Carries the raw cursor bytes and a copy of the consumer name the caller
/// queried with so callers can match cursors to their owning consumer when
/// processing batches of reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeriveCursorEntry {
    /// Consumer the cursor was persisted for.
    pub consumer: DeriveConsumerName,
    /// Opaque cursor bytes the consumer last persisted.
    pub cursor_bytes: Vec<u8>,
}

/// `RocksDB`-backed durable storage for the derive plane.
///
/// Operations are atomic at the `RocksDB` `WriteBatch` granularity. Cursor
/// writes always go in a single batch with the consumer's data writes so a
/// crash mid-write never advances the cursor without persisting the
/// underlying state.
#[derive(Clone, Debug)]
pub struct DeriveStore {
    db: Arc<DB>,
    sync_writes: bool,
    storage_path: PathBuf,
    consumer_column_families: &'static [&'static str],
}

impl DeriveStore {
    /// Opens or creates a derive store at `path`.
    ///
    /// On a fresh path the schema version is written immediately. On an
    /// existing path the persisted schema version is validated against
    /// [`DERIVE_SCHEMA_VERSION`].
    pub fn open(
        path: impl AsRef<Path>,
        options: DeriveStoreOptions,
    ) -> Result<Self, DeriveStoreError> {
        let path = path.as_ref();
        let db_options = primary_db_options();
        let sdk_families = DeriveStoreTable::all().into_iter().map(|table| {
            ColumnFamilyDescriptor::new(table.column_family_name(), column_family_options())
        });
        let consumer_families = options
            .consumer_column_families
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(*name, column_family_options()));
        let column_families = sdk_families.chain(consumer_families).collect::<Vec<_>>();
        let db = DB::open_cf_descriptors(&db_options, path, column_families).map_err(|source| {
            DeriveStoreError::Open {
                path: path.to_path_buf(),
                source,
            }
        })?;
        let store = Self {
            db: Arc::new(db),
            sync_writes: options.sync_writes,
            storage_path: path.to_path_buf(),
            consumer_column_families: options.consumer_column_families,
        };
        store.validate_or_initialize_schema_version()?;
        Ok(store)
    }

    /// Returns the filesystem path the store opened from.
    #[must_use]
    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }

    /// Reads a consumer's persisted cursor bytes, when present.
    pub fn get_cursor(
        &self,
        consumer: DeriveConsumerName,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        self.get(DeriveStoreTable::Cursor, consumer.as_str().as_bytes())
    }

    /// Atomically persists `cursor_bytes` for `consumer`.
    ///
    /// Each call commits its own `WriteBatch`. Consumers that need to bundle
    /// cursor advances with their own data writes use [`Self::write_batch`]
    /// instead.
    pub fn put_cursor(
        &self,
        consumer: DeriveConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let mut batch = WriteBatch::default();
        let column_family = self.column_family(DeriveStoreTable::Cursor)?;
        batch.put_cf(&column_family, consumer.as_str().as_bytes(), cursor_bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put_cursor",
                column_family: DeriveStoreColumnFamily::Cursor,
                source,
            })
    }

    /// Returns the persisted schema version recorded under
    /// `consumer_metadata`.
    pub fn schema_version(&self) -> Result<u16, DeriveStoreError> {
        let Some(bytes) = self.get(DeriveStoreTable::ConsumerMetadata, SCHEMA_VERSION_KEY)? else {
            return Err(DeriveStoreError::SchemaMismatch {
                persisted: 0,
                running: DERIVE_SCHEMA_VERSION,
            });
        };
        decode_schema_version(&bytes).map_err(|reason| DeriveStoreError::Decode {
            column_family: DeriveStoreColumnFamily::ConsumerMetadata,
            reason,
        })
    }

    /// Commits a prepared `WriteBatch` to the database.
    ///
    /// Consumers use this to bundle a cursor write together with their own
    /// data writes so the persisted cursor never advances without the
    /// underlying state having reached durability.
    pub fn write_batch(&self, batch: &WriteBatch) -> Result<(), DeriveStoreError> {
        self.write(batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "write_batch",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Returns a column-family handle the caller can use when staging puts
    /// directly into a [`WriteBatch`].
    pub fn column_family(
        &self,
        table: DeriveStoreTable,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, DeriveStoreError> {
        self.db
            .cf_handle(table.column_family_name())
            .ok_or_else(|| DeriveStoreError::ColumnFamilyMissing {
                column_family: table.error_family(),
            })
    }

    /// Returns a handle for a consumer-owned column family registered through
    /// [`DeriveStoreOptions::consumer_column_families`]. Consumers stage puts
    /// and deletes by calling `batch.put_cf(handle, key, value)` on the
    /// returned handle and committing through [`Self::write_batch`].
    pub fn consumer_column_family(
        &self,
        name: &'static str,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, DeriveStoreError> {
        if !self.consumer_column_families.contains(&name) {
            return Err(DeriveStoreError::ConsumerColumnFamilyMissing { name });
        }
        self.db
            .cf_handle(name)
            .ok_or(DeriveStoreError::ConsumerColumnFamilyMissing { name })
    }

    /// Reads a single value from a consumer-owned column family.
    pub fn get_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        self.db
            .get_cf(&handle, key)
            .map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "get",
                name: column_family,
                source,
            })
    }

    /// Iterates a consumer-owned column family, returning every entry whose
    /// key lies in `[start_key, end_key_inclusive]` in ascending order. The
    /// returned `Vec` is bounded by the caller's range; the helper collects
    /// eagerly so the iterator's `RocksDB` snapshot is dropped before the
    /// helper returns.
    pub fn range_iterate_consumer(
        &self,
        column_family: &'static str,
        start_key: &[u8],
        end_key_inclusive: &[u8],
    ) -> Result<Vec<ConsumerEntry>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let iterator = self.db.iterator_cf(
            &handle,
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        let mut entries = Vec::new();
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "range_iterate",
                name: column_family,
                source,
            })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            entries.push((key.to_vec(), payload.to_vec()));
        }
        Ok(entries)
    }

    /// Returns the lexicographically last key in a consumer-owned column
    /// family, or `None` when the column family is empty.
    ///
    /// Uses `RocksDB`'s reverse iterator (`IteratorMode::End`) so the lookup
    /// is bounded by one seek plus one block read regardless of how many
    /// entries the column family holds. Callers that need the "highest"
    /// height-keyed materialized record use this instead of a full-table
    /// scan to compute derive-cursor lag at request time.
    pub fn last_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation: "last_key",
            name: column_family,
            source,
        })?;
        Ok(Some(key.to_vec()))
    }

    fn validate_or_initialize_schema_version(&self) -> Result<(), DeriveStoreError> {
        let Some(bytes) = self.get(DeriveStoreTable::ConsumerMetadata, SCHEMA_VERSION_KEY)? else {
            return self.put(
                DeriveStoreTable::ConsumerMetadata,
                SCHEMA_VERSION_KEY,
                &DERIVE_SCHEMA_VERSION.to_be_bytes(),
            );
        };
        let persisted =
            decode_schema_version(&bytes).map_err(|reason| DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason,
            })?;
        if persisted == DERIVE_SCHEMA_VERSION {
            Ok(())
        } else {
            Err(DeriveStoreError::SchemaMismatch {
                persisted,
                running: DERIVE_SCHEMA_VERSION,
            })
        }
    }

    fn get(
        &self,
        table: DeriveStoreTable,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let column_family = self.column_family(table)?;
        self.db
            .get_cf(&column_family, key)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "get",
                column_family: table.error_family(),
                source,
            })
    }

    fn put(
        &self,
        table: DeriveStoreTable,
        key: &[u8],
        bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let column_family = self.column_family(table)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&column_family, key, bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put",
                column_family: table.error_family(),
                source,
            })
    }

    fn write(&self, batch: &WriteBatch) -> Result<(), rust_rocksdb::Error> {
        let mut write_options = WriteOptions::default();
        write_options.set_sync(self.sync_writes);
        self.db.write_opt(batch, &write_options)
    }
}

fn decode_schema_version(bytes: &[u8]) -> Result<u16, String> {
    let array: [u8; 2] = bytes
        .try_into()
        .map_err(|_| format!("schema version requires 2 bytes; got {}", bytes.len()))?;
    Ok(u16::from_be_bytes(array))
}

#[cfg(test)]
mod tests {
    use eyre::Result;
    use tempfile::tempdir;

    use super::*;

    const TEST_CONSUMER: DeriveConsumerName = DeriveConsumerName::from_static("test_consumer");

    #[test]
    fn opening_a_fresh_store_writes_the_schema_version() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        assert_eq!(store.schema_version()?, DERIVE_SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn cursor_round_trip_persists_and_retrieves_bytes() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        assert!(store.get_cursor(TEST_CONSUMER)?.is_none());
        store.put_cursor(TEST_CONSUMER, &[1, 2, 3])?;
        assert_eq!(store.get_cursor(TEST_CONSUMER)?, Some(vec![1, 2, 3]));
        store.put_cursor(TEST_CONSUMER, &[4, 5])?;
        assert_eq!(store.get_cursor(TEST_CONSUMER)?, Some(vec![4, 5]));
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_none_for_empty_column_family() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumer_column_families: &["test_cf"],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        assert_eq!(store.last_consumer_key("test_cf")?, None);
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_lexicographically_last_key() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumer_column_families: &["test_cf"],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        let handle = store.consumer_column_family("test_cf")?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&handle, 1_u32.to_be_bytes(), b"a");
        batch.put_cf(&handle, 42_u32.to_be_bytes(), b"b");
        batch.put_cf(&handle, 7_u32.to_be_bytes(), b"c");
        drop(handle);
        store.write_batch(&batch)?;
        assert_eq!(
            store.last_consumer_key("test_cf")?,
            Some(42_u32.to_be_bytes().to_vec())
        );
        Ok(())
    }

    #[test]
    fn reopening_a_store_with_an_advanced_schema_version_returns_mismatch() -> Result<()> {
        let tempdir = tempdir()?;
        {
            let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
            store.put(
                DeriveStoreTable::ConsumerMetadata,
                SCHEMA_VERSION_KEY,
                &(DERIVE_SCHEMA_VERSION + 1).to_be_bytes(),
            )?;
        }
        let outcome = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default());
        assert!(matches!(
            outcome,
            Err(DeriveStoreError::SchemaMismatch {
                persisted,
                running,
            }) if persisted == DERIVE_SCHEMA_VERSION + 1 && running == DERIVE_SCHEMA_VERSION
        ));
        Ok(())
    }
}
