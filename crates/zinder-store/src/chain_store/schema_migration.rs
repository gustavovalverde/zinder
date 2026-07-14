//! Primary-open store schema admission.
//!
//! Schema 13 makes block-local transparent inputs and resolved spend replay
//! facts durable. Older schemas stored only outpoints in the block index and may already have
//! deleted the corresponding point facts through retention, so the missing
//! facts cannot be reconstructed reliably in place. Primary open therefore
//! fails closed and requires a fresh canonical rebuild.

use crate::{
    StoreError,
    format::StoreKey,
    kv::{RocksChainStore, StorageTable, StoreReadCaller},
};

use super::{STORE_SCHEMA_VERSION, decode_store_metadata};

pub(super) fn migrate_primary_store_schema(inner: &RocksChainStore) -> Result<(), StoreError> {
    let key = StoreKey::store_metadata();
    let Some(metadata_bytes) =
        inner.get(StoreReadCaller::Query, StorageTable::StorageControl, &key)?
    else {
        return Ok(());
    };
    let metadata = decode_store_metadata(&key, &metadata_bytes)?;
    if metadata.schema_version == STORE_SCHEMA_VERSION {
        return Ok(());
    }

    Err(StoreError::SchemaMismatch {
        persisted_version: metadata.schema_version,
        expected_version: STORE_SCHEMA_VERSION,
    })
}
