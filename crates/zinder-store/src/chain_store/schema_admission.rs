//! Primary-open store schema admission.
//!
//! Schema 14 makes complete canonical replay envelopes durable. Older schemas
//! never retained every source-local field, so the missing envelopes cannot be
//! reconstructed reliably in place. Primary open therefore fails closed and
//! requires a fresh canonical rebuild.

use crate::{
    StoreError,
    format::StoreKey,
    kv::{RocksChainStore, StorageTable, StoreReadCaller},
};

use super::{CURRENT_STORE_SCHEMA_VERSION, decode_store_metadata};

pub(super) fn validate_primary_store_schema(inner: &RocksChainStore) -> Result<(), StoreError> {
    let key = StoreKey::store_metadata();
    let Some(metadata_bytes) =
        inner.get(StoreReadCaller::Query, StorageTable::StorageControl, &key)?
    else {
        return Ok(());
    };
    let metadata = decode_store_metadata(&key, &metadata_bytes)?;
    if metadata.schema_version == CURRENT_STORE_SCHEMA_VERSION {
        return Ok(());
    }

    Err(StoreError::SchemaMismatch {
        persisted_version: metadata.schema_version,
        expected_version: CURRENT_STORE_SCHEMA_VERSION,
    })
}
