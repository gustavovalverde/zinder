//! Materialized-view preset storage contracts.

use std::path::Path;

use super::construction_identity;
use eyre::Result;
use tempfile::TempDir;
use zinder_materialized_views::{
    BLOCK_SUMMARY_COLUMN_FAMILY, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
    MaterializedViewPreset, MaterializedViewStore, MaterializedViewStoreError,
    MaterializedViewStoreOptions, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY, bundled_materialized_view_consumer_definitions,
};
use zinder_store::RocksDbResourceBudget;

const FOREIGN_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("foreign_materialized_view");
const FOREIGN_COLUMN_FAMILY: &str = "foreign_consumer_rows";
const FOREIGN_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(FOREIGN_CONSUMER_NAME, 1, &[FOREIGN_COLUMN_FAMILY]);

fn options() -> MaterializedViewStoreOptions {
    MaterializedViewStoreOptions {
        rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
        ..MaterializedViewStoreOptions::default()
    }
}

fn open_preset(
    path: &Path,
    preset: MaterializedViewPreset,
) -> std::result::Result<MaterializedViewStore, MaterializedViewStoreError> {
    MaterializedViewStore::open_with_materialized_view_preset(
        path,
        construction_identity()?,
        preset,
        options(),
    )
}

fn open_with_options(
    path: &Path,
    options: MaterializedViewStoreOptions,
) -> Result<MaterializedViewStore> {
    Ok(MaterializedViewStore::open(
        path,
        construction_identity()?,
        options,
    )?)
}

#[test]
fn wallet_preset_persists_only_wallet_materialized_view_schemas() -> Result<()> {
    let primary = TempDir::new()?;
    let secondary = TempDir::new()?;

    {
        let store = open_preset(primary.path(), MaterializedViewPreset::Wallet)?;
        store.consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        store.consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY)?;
        assert!(matches!(
            store.consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY),
            Err(MaterializedViewStoreError::ConsumerColumnFamilyMissing { name })
                if name == BLOCK_SUMMARY_COLUMN_FAMILY
        ));
    }

    MaterializedViewStore::open_secondary_with_materialized_view_preset(
        primary.path(),
        secondary.path(),
        MaterializedViewPreset::Wallet,
        options(),
    )?;
    Ok(())
}

#[test]
fn persisted_workload_detection_distinguishes_wallet_explorer_and_missing_paths() -> Result<()> {
    let wallet = TempDir::new()?;
    let explorer = TempDir::new()?;
    let missing = TempDir::new()?;
    open_preset(wallet.path(), MaterializedViewPreset::Wallet)?;
    open_preset(explorer.path(), MaterializedViewPreset::Explorer)?;

    assert_eq!(
        MaterializedViewStore::detect_materialized_view_preset_at_path(wallet.path())?,
        Some(MaterializedViewPreset::Wallet)
    );
    assert_eq!(
        MaterializedViewStore::detect_materialized_view_preset_at_path(explorer.path())?,
        Some(MaterializedViewPreset::Explorer)
    );
    assert_eq!(
        MaterializedViewStore::detect_materialized_view_preset_at_path(missing.path())?,
        None
    );
    Ok(())
}

#[test]
fn changing_a_persisted_materialized_view_preset_fails_before_manifest_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        open_preset(primary.path(), MaterializedViewPreset::Wallet)?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome = open_preset(primary.path(), MaterializedViewPreset::Explorer);
    assert!(matches!(
        outcome,
        Err(
            MaterializedViewStoreError::MaterializedViewPresetRequiresFreshStore {
                requested: "explorer",
            }
        )
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);

    let store = open_preset(primary.path(), MaterializedViewPreset::Wallet)?;
    assert!(matches!(
        store.consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY),
        Err(MaterializedViewStoreError::ConsumerColumnFamilyMissing { name })
            if name == BLOCK_SUMMARY_COLUMN_FAMILY
    ));
    Ok(())
}

#[test]
fn reducing_explorer_to_wallet_fails_before_column_family_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        open_preset(primary.path(), MaterializedViewPreset::Explorer)?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome = open_preset(primary.path(), MaterializedViewPreset::Wallet);
    assert!(matches!(
        outcome,
        Err(
            MaterializedViewStoreError::MaterializedViewPresetRequiresFreshStore {
                requested: "wallet",
            }
        )
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);
    Ok(())
}

#[test]
fn explorer_preflight_rejects_a_foreign_consumer_without_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        let store = open_with_options(
            primary.path(),
            MaterializedViewStoreOptions {
                consumers: &[FOREIGN_SCHEMA],
                ..options()
            },
        )?;
        store.put_consumer(FOREIGN_COLUMN_FAMILY, b"key", b"value")?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome = MaterializedViewStore::detect_materialized_view_preset_at_path(primary.path());
    assert!(matches!(
        outcome,
        Err(MaterializedViewStoreError::ConsumerNotDeclared {
            consumer,
            persisted_schema_version: 1,
        }) if consumer == FOREIGN_CONSUMER_NAME.as_str()
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);

    let store = open_with_options(
        primary.path(),
        MaterializedViewStoreOptions {
            consumers: &[FOREIGN_SCHEMA],
            ..options()
        },
    )?;
    assert_eq!(
        store.get_consumer(FOREIGN_COLUMN_FAMILY, b"key")?,
        Some(b"value".to_vec())
    );
    Ok(())
}

#[test]
fn consumer_catalog_declares_every_explorer_and_wallet_identity_once() {
    let definitions = bundled_materialized_view_consumer_definitions();
    assert_eq!(
        definitions.len(),
        MaterializedViewPreset::Explorer.consumer_schemas().len()
    );
    for schema in MaterializedViewPreset::Explorer.consumer_schemas() {
        assert_eq!(
            definitions
                .iter()
                .filter(|definition| definition.schema.name == schema.name)
                .count(),
            1,
            "consumer {} must have exactly one product declaration",
            schema.name.as_str()
        );
    }
    let wallet_identities = definitions
        .iter()
        .filter(|definition| definition.included_in(MaterializedViewPreset::Wallet))
        .map(|definition| definition.schema.name)
        .collect::<Vec<_>>();
    assert_eq!(wallet_identities.len(), 2);
    for schema in MaterializedViewPreset::Wallet.consumer_schemas() {
        assert!(wallet_identities.contains(&schema.name));
    }
}
