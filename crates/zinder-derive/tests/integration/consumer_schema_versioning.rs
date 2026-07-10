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
const CONSUMER_A_V2_ROW_COMPATIBLE_WITH_V1: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 2, &[CONSUMER_A_CF]).with_row_compatible_versions(&[1]);
const CONSUMER_A_V2_ROW_COMPATIBLE_WITH_V1_ON_B_CF: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 2, &[CONSUMER_B_CF]).with_row_compatible_versions(&[1]);
const CONSUMER_A_V3_ROW_COMPATIBLE_WITH_V2: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 3, &[CONSUMER_A_CF]).with_row_compatible_versions(&[2]);
const CONSUMER_A_V3_ROW_COMPATIBLE_WITH_V1_V2: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 3, &[CONSUMER_A_CF])
        .with_row_compatible_versions(&[1, 2]);
const CONSUMER_A_TWO_CFS_V1: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 1, &[CONSUMER_A_CF, CONSUMER_B_CF]);
const CONSUMER_A_TWO_CFS_REORDERED_V1: DeriveConsumerSchema =
    DeriveConsumerSchema::new(CONSUMER_A, 1, &[CONSUMER_B_CF, CONSUMER_A_CF]);
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
const ONLY_A_V2_ROW_COMPATIBLE_WITH_V1: &[DeriveConsumerSchema] =
    &[CONSUMER_A_V2_ROW_COMPATIBLE_WITH_V1];
const ONLY_A_V2_ROW_COMPATIBLE_WITH_V1_ON_B_CF: &[DeriveConsumerSchema] =
    &[CONSUMER_A_V2_ROW_COMPATIBLE_WITH_V1_ON_B_CF];
const ONLY_A_V3_ROW_COMPATIBLE_WITH_V2: &[DeriveConsumerSchema] =
    &[CONSUMER_A_V3_ROW_COMPATIBLE_WITH_V2];
const ONLY_A_V3_ROW_COMPATIBLE_WITH_V1_V2: &[DeriveConsumerSchema] =
    &[CONSUMER_A_V3_ROW_COMPATIBLE_WITH_V1_V2];
const ONLY_A_TWO_CFS_V1: &[DeriveConsumerSchema] = &[CONSUMER_A_TWO_CFS_V1];
const ONLY_A_TWO_CFS_REORDERED_V1: &[DeriveConsumerSchema] = &[CONSUMER_A_TWO_CFS_REORDERED_V1];
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
fn row_compatible_consumer_upgrade_preserves_rows_and_cursor() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    {
        let store = open(tempdir.path(), ONLY_A_V2_ROW_COMPATIBLE_WITH_V1)?;
        assert_eq!(
            store.get_consumer(CONSUMER_A_CF, b"key-a")?,
            Some(b"value-a".to_vec())
        );
        assert_eq!(
            store.get_chain_event_cursor(CONSUMER_A)?,
            Some(b"cursor-a".to_vec())
        );
    }

    // The manifest records that version-1 rows still exist, so readers must
    // keep declaring compatibility with version 1 after the writer advances.
    let secondary = TempDir::new()?;
    let reader = DeriveStore::open_secondary(
        tempdir.path(),
        secondary.path(),
        options(ONLY_A_V2_ROW_COMPATIBLE_WITH_V1),
    )?;
    assert_eq!(
        reader.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    Ok(())
}

#[test]
fn row_compatibility_is_cumulative_across_multiple_upgrades() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }
    {
        let store = open(tempdir.path(), ONLY_A_V2_ROW_COMPATIBLE_WITH_V1)?;
        assert_eq!(
            store.get_consumer(CONSUMER_A_CF, b"key-a")?,
            Some(b"value-a".to_vec())
        );
    }

    let incomplete = DeriveStore::open(tempdir.path(), options(ONLY_A_V3_ROW_COMPATIBLE_WITH_V2));
    assert!(matches!(
        incomplete,
        Err(DeriveStoreError::ConsumerSchemaMismatch {
            consumer,
            persisted: Some(2),
            running: 3,
        }) if consumer == CONSUMER_A.as_str()
    ));

    let store = open(tempdir.path(), ONLY_A_V3_ROW_COMPATIBLE_WITH_V1_V2)?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(CONSUMER_A)?,
        Some(b"cursor-a".to_vec())
    );
    Ok(())
}

#[test]
fn reordering_the_same_column_family_set_preserves_rows_and_cursor() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_TWO_CFS_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    let store = open(tempdir.path(), ONLY_A_TWO_CFS_REORDERED_V1)?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    assert_eq!(
        store.get_consumer(CONSUMER_B_CF, b"key-b")?,
        Some(b"value-b".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(CONSUMER_A)?,
        Some(b"cursor-a".to_vec())
    );
    Ok(())
}

#[test]
fn row_compatible_secondary_can_open_before_primary_reconciliation() -> Result<()> {
    let primary = TempDir::new()?;
    let secondary = TempDir::new()?;
    {
        let store = open(primary.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
    }

    let reader = DeriveStore::open_secondary(
        primary.path(),
        secondary.path(),
        options(ONLY_A_V2_ROW_COMPATIBLE_WITH_V1),
    )?;
    assert_eq!(
        reader.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    Ok(())
}

#[test]
fn row_compatible_version_with_changed_column_families_rebuilds() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    let store = open(tempdir.path(), ONLY_A_V2_ROW_COMPATIBLE_WITH_V1_ON_B_CF)?;
    assert_eq!(store.get_chain_event_cursor(CONSUMER_A)?, None);
    assert_eq!(store.get_consumer(CONSUMER_B_CF, b"key-a")?, None);
    Ok(())
}

#[test]
fn older_consumer_binary_rejects_newer_manifest_without_clearing_rows() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V2)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    let outcome = DeriveStore::open(tempdir.path(), options(ONLY_A_V1));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerSchemaMismatch {
            consumer,
            persisted,
            running,
        }) if consumer == CONSUMER_A.as_str() && persisted == Some(2) && running == 1
    ));

    let store = open(tempdir.path(), ONLY_A_V2)?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(CONSUMER_A)?,
        Some(b"cursor-a".to_vec())
    );
    Ok(())
}

#[test]
fn unknown_manifest_consumer_fails_closed_without_clearing_rows() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
        store.put_chain_event_cursor(CONSUMER_B, b"cursor-b")?;
    }

    let outcome = DeriveStore::open(tempdir.path(), options(ONLY_A_V1));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerNotDeclared {
            consumer,
            persisted_schema_version: 1,
        }) if consumer == CONSUMER_B.as_str()
    ));

    let store = open(tempdir.path(), BOTH_V1)?;
    assert_eq!(
        store.get_consumer(CONSUMER_B_CF, b"key-b")?,
        Some(b"value-b".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(CONSUMER_B)?,
        Some(b"cursor-b".to_vec())
    );
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
fn transferring_a_column_family_to_a_new_consumer_fails_closed() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
    }

    let outcome = DeriveStore::open(tempdir.path(), options(ONLY_C_ON_A_CF));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerNotDeclared {
            consumer,
            persisted_schema_version: 1,
        }) if consumer == CONSUMER_A.as_str()
    ));

    let store = open(tempdir.path(), ONLY_A_V1)?;
    assert_eq!(
        store.get_consumer(CONSUMER_A_CF, b"key-a")?,
        Some(b"value-a".to_vec())
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
fn old_secondary_rejects_incompatible_schema_after_catch_up() -> Result<()> {
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

    let outcome = reader.try_catch_up();
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerSchemaMismatch {
            consumer,
            persisted: Some(2),
            running: 1,
        }) if consumer == CONSUMER_A.as_str()
    ));
    Ok(())
}

#[test]
fn upgraded_secondary_catches_up_across_scoped_rebuild() -> Result<()> {
    let primary = TempDir::new()?;
    let reader_scratch = TempDir::new()?;
    {
        let store = open(primary.path(), ONLY_A_V1)?;
        store.put_consumer(CONSUMER_A_CF, b"key-a", b"value-a")?;
        store.put_chain_event_cursor(CONSUMER_A, b"cursor-a")?;
    }

    let reader = DeriveStore::open_secondary(
        primary.path(),
        reader_scratch.path(),
        options(ONLY_A_V2_ROW_COMPATIBLE_WITH_V1),
    )?;
    {
        let store = open(primary.path(), ONLY_A_V2)?;
        assert_eq!(store.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    }

    reader.try_catch_up()?;
    assert_eq!(reader.get_consumer(CONSUMER_A_CF, b"key-a")?, None);
    assert_eq!(reader.get_chain_event_cursor(CONSUMER_A)?, None);
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
fn secondary_open_rejects_unknown_manifest_consumer() -> Result<()> {
    let tempdir = TempDir::new()?;
    {
        let store = open(tempdir.path(), BOTH_V1)?;
        store.put_consumer(CONSUMER_B_CF, b"key-b", b"value-b")?;
    }

    let secondary = TempDir::new()?;
    let outcome = DeriveStore::open_secondary(tempdir.path(), secondary.path(), options(ONLY_A_V1));
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerNotDeclared {
            consumer,
            persisted_schema_version: 1,
        }) if consumer == CONSUMER_B.as_str()
    ));

    let store = open(tempdir.path(), BOTH_V1)?;
    assert_eq!(
        store.get_consumer(CONSUMER_B_CF, b"key-b")?,
        Some(b"value-b".to_vec())
    );
    Ok(())
}
