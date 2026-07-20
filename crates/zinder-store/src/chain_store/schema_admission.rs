//! Primary-open store schema admission.
//!
//! Only the current canonical layout is admitted. Noncurrent layouts may omit
//! source facts required by complete replay or encode unsupported event kinds, so
//! primary open fails closed and requires a fresh canonical rebuild.

use std::path::Path;

use rust_rocksdb::{DB, IteratorMode, Options};

use crate::{
    StoreError,
    format::StoreKey,
    kv::{RocksChainStore, StorageTable, StoreReadCaller},
};

use super::{CURRENT_STORE_SCHEMA_VERSION, decode_store_metadata};

/// Verifies that an existing store has durable metadata before a primary open
/// can create column families or write initialization records.
///
/// Returns `true` only for a fresh path or a database with no persisted rows.
pub(super) fn preflight_primary_store_schema(path: &Path) -> Result<bool, StoreError> {
    let Ok(column_families) = DB::list_cf(&Options::default(), path) else {
        return Ok(true);
    };
    let database = DB::open_cf_for_read_only(&Options::default(), path, &column_families, false)
        .map_err(StoreError::storage_unavailable)?;
    let metadata_key = StoreKey::store_metadata();
    let metadata = match database.cf_handle(StorageTable::StorageControl.column_family_name()) {
        Some(family) => database
            .get_cf(&family, metadata_key.as_bytes())
            .map_err(StoreError::storage_unavailable)?,
        None => None,
    };

    if let Some(metadata_bytes) = metadata {
        validate_store_metadata(&metadata_key, &metadata_bytes)?;
        return Ok(false);
    }

    for column_family in &column_families {
        let family = database
            .cf_handle(column_family)
            .ok_or_else(|| StoreError::StoreMetadataMissing)?;
        if database
            .iterator_cf(&family, IteratorMode::Start)
            .next()
            .transpose()
            .map_err(StoreError::storage_unavailable)?
            .is_some()
        {
            return Err(StoreError::StoreMetadataMissing);
        }
    }

    Ok(true)
}

pub(super) fn validate_primary_store_schema(
    inner: &RocksChainStore,
    initialized_from_empty_store: bool,
) -> Result<(), StoreError> {
    let key = StoreKey::store_metadata();
    let Some(metadata_bytes) =
        inner.get(StoreReadCaller::Query, StorageTable::StorageControl, &key)?
    else {
        return if initialized_from_empty_store {
            Ok(())
        } else {
            Err(StoreError::StoreMetadataMissing)
        };
    };
    validate_store_metadata(&key, &metadata_bytes)
}

fn validate_store_metadata(key: &StoreKey, metadata_bytes: &[u8]) -> Result<(), StoreError> {
    let metadata = decode_store_metadata(key, metadata_bytes)?;
    if metadata.schema_version == CURRENT_STORE_SCHEMA_VERSION {
        return Ok(());
    }

    Err(StoreError::SchemaMismatch {
        persisted_version: metadata.schema_version,
        expected_version: CURRENT_STORE_SCHEMA_VERSION,
    })
}
