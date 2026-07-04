//! Derive-store fixtures for tests that exercise materialized projections.

use std::{collections::BTreeMap, path::Path};

use thiserror::Error;
use zinder_core::{
    BlockHeight, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentSpendFact,
    wire::{encode_height_key_ascending, encode_height_key_descending, encode_outpoint_key},
};
use zinder_derive::{
    DeriveStore, DeriveStoreOptions, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
    encode_transparent_spend_row_value,
};

const ADDRESS_HASH_LEN: usize = 32;
const HEIGHT_LEN: usize = 4;
const POSITION_LEN: usize = 4;
const HISTORY_KEY_LEN: usize = ADDRESS_HASH_LEN + HEIGHT_LEN + POSITION_LEN;
const HISTORY_VALUE_LEN: usize = 64;

/// Failure returned while preparing a derive-store fixture.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeriveFixtureError {
    /// Derive store could not be opened or written.
    #[error(transparent)]
    Store(#[from] zinder_derive::DeriveStoreError),
}

/// Opens the derive primary paired with `canonical_path` for tests.
///
/// Production processes use `zinder-ingest` as the derive writer. Tests that
/// construct canonical rows directly use this helper to keep the paired derive
/// projection explicit.
///
/// # Errors
///
/// Returns [`DeriveFixtureError`] when the derive store cannot be opened.
pub fn open_test_derive_store_for_canonical(
    canonical_path: &Path,
) -> Result<DeriveStore, DeriveFixtureError> {
    Ok(DeriveStore::open(
        DeriveStore::path_for_canonical(canonical_path),
        DeriveStoreOptions {
            sync_writes: false,
            consumers: DeriveStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?)
}

/// Seeds transparent-address transaction-history rows in a derive store.
///
/// This writes the final projection shape directly. It is intended for query
/// and client tests that do not run the ingest derive tailer.
///
/// # Errors
///
/// Returns [`DeriveFixtureError`] when any derive-store write fails.
pub fn seed_transparent_address_transaction_history(
    derive_store: &DeriveStore,
    artifacts: &[TransparentAddressTxIndexArtifact],
) -> Result<(), DeriveFixtureError> {
    let mut index_payloads_by_height = BTreeMap::<BlockHeight, Vec<u8>>::new();
    for artifact in artifacts {
        let ascending_key = history_key(
            artifact.address_script_hash,
            artifact.block_height,
            artifact.tx_index_in_block,
            false,
        );
        let descending_key = history_key(
            artifact.address_script_hash,
            artifact.block_height,
            artifact.tx_index_in_block,
            true,
        );
        let history_payload = history_value(artifact);
        derive_store.put_consumer(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
            &ascending_key,
            &history_payload,
        )?;
        derive_store.put_consumer(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
            &descending_key,
            &history_payload,
        )?;
        let index_payload = index_payloads_by_height
            .entry(artifact.block_height)
            .or_default();
        index_payload.extend_from_slice(&ascending_key);
        index_payload.extend_from_slice(&descending_key);
    }

    for (height, index_payload) in index_payloads_by_height {
        derive_store.put_consumer(
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            &encode_height_key_ascending(height),
            &index_payload,
        )?;
    }

    Ok(())
}

/// Seeds durable transparent-outpoint-spend rows in a derive store.
///
/// Writes the primary spend rows keyed by spent outpoint plus the per-height
/// index rows the projection uses for rewind and freshness. Intended for query
/// and client tests that populate the projection without running the ingest
/// derive tailer. Row values reuse the consumer's
/// [`encode_transparent_spend_row_value`] so the seeded bytes never diverge
/// from what the consumer writes.
///
/// # Errors
///
/// Returns [`DeriveFixtureError`] when any derive-store write fails.
pub fn seed_transparent_outpoint_spends(
    derive_store: &DeriveStore,
    spends: &[TransparentSpendFact],
) -> Result<(), DeriveFixtureError> {
    let mut index_payloads_by_height = BTreeMap::<BlockHeight, Vec<u8>>::new();
    for spend in spends {
        let key = encode_outpoint_key(spend.spent_outpoint);
        derive_store.put_consumer(
            TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY,
            &key,
            &encode_transparent_spend_row_value(spend),
        )?;
        index_payloads_by_height
            .entry(spend.block_height)
            .or_default()
            .extend_from_slice(&key);
    }
    for (height, index_payload) in index_payloads_by_height {
        derive_store.put_consumer(
            TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            &encode_height_key_ascending(height),
            &index_payload,
        )?;
    }
    Ok(())
}

fn history_key(
    address_script_hash: TransparentAddressScriptHash,
    height: BlockHeight,
    tx_index_in_block: u32,
    descending: bool,
) -> [u8; HISTORY_KEY_LEN] {
    let mut key = [0u8; HISTORY_KEY_LEN];
    key[..ADDRESS_HASH_LEN].copy_from_slice(&address_script_hash.as_bytes());
    let height_key = if descending {
        encode_height_key_descending(height)
    } else {
        encode_height_key_ascending(height)
    };
    key[ADDRESS_HASH_LEN..ADDRESS_HASH_LEN + HEIGHT_LEN].copy_from_slice(&height_key);
    let position_key = if descending {
        u32::MAX - tx_index_in_block
    } else {
        tx_index_in_block
    };
    key[ADDRESS_HASH_LEN + HEIGHT_LEN..].copy_from_slice(&position_key.to_be_bytes());
    key
}

fn history_value(artifact: &TransparentAddressTxIndexArtifact) -> [u8; HISTORY_VALUE_LEN] {
    let mut encoded = [0u8; HISTORY_VALUE_LEN];
    encoded[..32].copy_from_slice(&artifact.transaction_id.as_bytes());
    encoded[32..].copy_from_slice(&artifact.block_hash.as_bytes());
    encoded
}
