//! Per-consumer derive schema versioning: rebuild scope is one consumer.

use std::{collections::BTreeSet, path::Path};

use eyre::Result;
use tempfile::TempDir;
use zinder_derive::{
    DeriveConsumerName, DeriveConsumerSchema, DeriveStore, DeriveStoreError, DeriveStoreOptions,
    DeriveStoreTable,
};
use zinder_store::RocksDbResourceBudget;

const CONSUMER_A: DeriveConsumerName = DeriveConsumerName::from_static("consumer_a");
const CONSUMER_B: DeriveConsumerName = DeriveConsumerName::from_static("consumer_b");
const CONSUMER_C: DeriveConsumerName = DeriveConsumerName::from_static("consumer_c");
const CONSUMER_A_CF: &str = "consumer_a_cf";
const CONSUMER_B_CF: &str = "consumer_b_cf";

const CONSUMER_A_V1: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 1, &[CONSUMER_A_CF]);
const CONSUMER_A_V2: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 2, &[CONSUMER_A_CF]);
const CONSUMER_B_V1: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_B, 1, &[CONSUMER_B_CF]);
const CONSUMER_B_ON_A_CF: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_B, 1, &[CONSUMER_A_CF]);
const CONSUMER_C_ON_A_CF: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_C, 1, &[CONSUMER_A_CF]);
const CONSUMER_A_ON_RESERVED_CF: DeriveConsumerSchema = DeriveConsumerSchema::new(
    CONSUMER_A,
    1,
    &[DeriveStoreTable::ChainEventCursor.column_family_name()],
);

const BOTH_V1: &[DeriveConsumerSchema] = &[CONSUMER_A_V1, CONSUMER_B_V1];
const A_V2_B_V1: &[DeriveConsumerSchema] = &[CONSUMER_A_V2, CONSUMER_B_V1];
const ONLY_A_V1: &[DeriveConsumerSchema] = &[CONSUMER_A_V1];
const ONLY_A_V2: &[DeriveConsumerSchema] = &[CONSUMER_A_V2];
const DUPLICATE_CF: &[DeriveConsumerSchema] = &[CONSUMER_A_V1, CONSUMER_B_ON_A_CF];
const RESERVED_CF: &[DeriveConsumerSchema] = &[CONSUMER_A_ON_RESERVED_CF];
const ONLY_C_ON_A_CF: &[DeriveConsumerSchema] = &[CONSUMER_C_ON_A_CF];

fn options(consumers: &'static [DeriveConsumerSchema]) -> DeriveStoreOptions {
    DeriveStoreOptions {
        sync_writes: false,
        consumers,
        rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
    }
}

fn open(path: &Path, consumers: &'static [DeriveConsumerSchema]) -> Result<DeriveStore> {
    Ok(DeriveStore::open(path, options(consumers))?)
}

fn column_family_set(path: &Path) -> BTreeSet<String> {
    rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), path)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

#[test]
fn bumping_one_consumer_version_rebuilds_only_its_column_families() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
        store.put_chain_event_cursor(CONSUMER_B, b"cursor-b")?;
    }

    let store = open(tempdir.path(), A_V2_B_V1)?;
    assert_eq!(store.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    assert_eq!(store.get_chain_event_cursor(CONSUMER_A)?, None);
    assert_eq!(
        store.get_consumer(CONSUMER_B_CF, b"key-b")?,
        Some(b"value-b".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(CONSUMER_B)?,
        Some(b"cursor-b".to_vec())
    );
    Ok(())
}

#[test]
fn unregistering_a_consumer_clears_its_column_families() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
        store.put_chain_event_cursor(CONSUMER_B, b"cursor-b")?;
    }

    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        assert_eq!(
            store.get_consumer(CONSUMER_A_CF, b"key-a")?,
            Some(b"value-a".to_vec())
        );
    }

    // Re-registering B at its original version finds an empty column family,
    // which proves the earlier reopen cleared its rows rather than leaving
    // stale rows behind an unregistered name.
    let store = open(tempdir.path(), BOTH_V1)?;
    assert_eq!(store.get_consumer(CONSUMER_B_CF, b"key-b")?, None);
    assert_eq!(store.get_chain_event_cursor(CONSUMER_B)?, None);
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    Ok(())
}

#[test]
fn fresh_store_records_every_declared_consumer_version() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
    }

    // Reopening at the same versions must not rebuild either consumer, which
    // it only avoids if the fresh open recorded both declared versions.
    let store = open(tempdir.path(), BOTH_V1)?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    assert_eq!(
        store.get_consumer(CONSUMER_B_CF, b"key-b")?,
        Some(b"value-b".to_vec())
    );
    Ok(())
}

#[test]
fn declaring_one_column_family_from_two_consumers_is_rejected() -> Result<()> {
    let tempdir = TempDir::new()?;
    let outcome = DeriveStore::open(tempdir.path(), options(DUPLICATE_CF));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerColumnFamilyConflict { name }) if name == CONSUMER_A_CF
    ));
    Ok(())
}

#[test]
fn declaring_a_reserved_store_table_column_family_is_rejected() -> Result<()> {
    let tempdir = TempDir::new()?;
    let outcome = DeriveStore::open(tempdir.path(), options(RESERVED_CF));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerColumnFamilyConflict { name })
            if name == DeriveStoreTable::ChainEventCursor.column_family_name()
    ));
    Ok(())
}

#[test]
fn declaring_a_new_consumer_over_an_existing_column_family_clears_its_rows() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
    }

    // consumer_a is unregistered while consumer_c now declares its column
    // family. consumer_c starts from an empty projection: the prior owner's
    // rows are cleared rather than adopted behind consumer_c's fresh cursor,
    // and the family stays usable for consumer_c's own writes.
    let store = open(tempdir.path(), ONLY_C_ON_A_CF)?;
    assert_eq!(store.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    store.put_consumer(CONSUMER_A_CF, b"key-c", b"value-c")?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-c")?,
        Some(b"value-c".to_vec())
    );
    Ok(())
}

#[test]
fn bumping_a_consumer_version_sweeps_keys_beyond_the_clear_range_upper_bound() -> Result<()> {
    let tempdir = TempDir::new()?;
    let above_upper_bound = vec![0xff; 600];
    let below_upper_bound = vec![0xff; 500];
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, &above_upper_bound, b"residue")?;
        store.put_consumer(CONSUMER_A_CF, &below_upper_bound, b"tombstoned")?;
    }

    // The range tombstone covers keys below the [0xff; 512] exclusive upper
    // bound; the point-delete sweep is the only thing that removes a key at or
    // above it. Both rows must be gone after the rebuild, and the column
    // family must scan empty.
    let store = open(tempdir.path(), ONLY_A_V2)?;
    assert_eq!(store.get_consumer(CONSUMER_A_CF, &above_upper_bound)?, None);
    assert_eq!(store.get_consumer(CONSUMER_A_CF, &below_upper_bound)?, None);
    assert_eq!(store.last_consumer_key(CONSUMER_A_CF)?, None);
    Ok(())
}

#[test]
fn open_secondary_rejects_a_consumer_declared_at_a_newer_version() -> Result<()> {
    let primary = TempDir::new()?;
    let secondary = TempDir::new()?;
    {
        let store = open(primary.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
    }

    let outcome = DeriveStore::open_secondary(primary.path(), secondary.path(), options(ONLY_A_V2));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerSchemaMismatch {
            consumer,
            persisted,
            running,
        }) if consumer == CONSUMER_A.as_str() && persisted == Some(1) && running == 2
    ));
    Ok(())
}

#[test]
fn open_secondary_accepts_matching_consumer_versions() -> Result<()> {
    let primary = TempDir::new()?;
    let secondary = TempDir::new()?;
    {
        let store = open(primary.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
    }

    let outcome = DeriveStore::open_secondary(primary.path(), secondary.path(), options(ONLY_A_V1));
    assert!(outcome.is_ok());
    Ok(())
}

#[test]
fn a_secondary_open_across_a_scoped_rebuild_catches_up_without_crashing() -> Result<()> {
    // Models the live container-bump path: a query/explorer process holds the
    // derive store open as a secondary while the ingest primary reconciles a
    // bumped consumer schema. Reconciliation must clear rows with range
    // tombstones, not drop the column family, or the secondary crashes replaying
    // the drop edit during catch-up.
    let primary = TempDir::new()?;
    let reader_scratch = TempDir::new()?;
    {
        let store = open(primary.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    let reader =
        DeriveStore::open_secondary(primary.path(), reader_scratch.path(), options(ONLY_A_V1))?;
    reader.try_catch_up()?;
    assert_eq!(
        reader.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );

    {
        let store = open(primary.path(), ONLY_A_V2)?;
        assert_eq!(store.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    }

    reader.try_catch_up()?;
    assert_eq!(reader.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    assert!(reader.get_chain_event_cursor(CONSUMER_A)?.is_none());
    Ok(())
}

#[test]
fn a_scoped_rebuild_leaves_the_on_disk_column_family_set_unchanged() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
    }

    let before = column_family_set(tempdir.path());
    assert!(before.contains(CONSUMER_A_CF));
    {
        let store = open(tempdir.path(), A_V2_B_V1)?;
        assert_eq!(store.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    }
    let after = column_family_set(tempdir.path());

    assert_eq!(before, after);
    Ok(())
}

#[test]
fn unregistering_a_consumer_keeps_its_column_family_on_disk_as_an_empty_orphan() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
    }

    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        drop(store);
    }

    // consumer_b is no longer declared. Its rows are cleared, but its column
    // family stays on disk as an emptied orphan so an attached secondary never
    // replays a column-family drop; the physical family is reclaimed only by a
    // container-format wipe.
    assert!(column_family_set(tempdir.path()).contains(CONSUMER_B_CF));
    Ok(())
}
