//! Projection preset storage contracts.

use eyre::Result;
use tempfile::TempDir;
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, DeriveConsumerName, DeriveConsumerSchema, DeriveStore,
    DeriveStoreError, DeriveStoreOptions, ProjectionPreset,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY, bundled_projection_definitions,
};
use zinder_store::RocksDbResourceBudget;

const FOREIGN_CONSUMER_NAME: DeriveConsumerName =
    DeriveConsumerName::from_static("foreign_projection");
const FOREIGN_COLUMN_FAMILY: &str = "foreign_projection_rows";
const FOREIGN_SCHEMA: DeriveConsumerSchema =
    DeriveConsumerSchema::new(FOREIGN_CONSUMER_NAME, 1, &[FOREIGN_COLUMN_FAMILY]);

fn options() -> DeriveStoreOptions {
    DeriveStoreOptions {
        rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
        ..DeriveStoreOptions::default()
    }
}

#[test]
fn wallet_preset_persists_only_wallet_projection_schemas() -> Result<()> {
    let primary = TempDir::new()?;
    let secondary = TempDir::new()?;

    {
        let store = DeriveStore::open_with_projection_preset(
            primary.path(),
            ProjectionPreset::Wallet,
            options(),
        )?;
        store.consumer_column_family(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY)?;
        store.consumer_column_family(TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILY)?;
        assert!(matches!(
            store.consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY),
            Err(DeriveStoreError::ConsumerColumnFamilyMissing { name })
                if name == BLOCK_SUMMARY_COLUMN_FAMILY
        ));
    }

    DeriveStore::open_secondary_with_projection_preset(
        primary.path(),
        secondary.path(),
        ProjectionPreset::Wallet,
        options(),
    )?;
    Ok(())
}

#[test]
fn persisted_workload_detection_distinguishes_wallet_complete_and_missing_paths() -> Result<()> {
    let wallet = TempDir::new()?;
    let complete = TempDir::new()?;
    let missing = TempDir::new()?;
    DeriveStore::open_with_projection_preset(wallet.path(), ProjectionPreset::Wallet, options())?;
    DeriveStore::open_with_projection_preset(
        complete.path(),
        ProjectionPreset::Complete,
        options(),
    )?;

    assert_eq!(
        DeriveStore::detect_projection_preset_at_path(wallet.path())?,
        Some(ProjectionPreset::Wallet)
    );
    assert_eq!(
        DeriveStore::detect_projection_preset_at_path(complete.path())?,
        Some(ProjectionPreset::Complete)
    );
    assert_eq!(
        DeriveStore::detect_projection_preset_at_path(missing.path())?,
        None
    );
    Ok(())
}

#[test]
fn changing_a_persisted_projection_preset_fails_before_manifest_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        DeriveStore::open_with_projection_preset(
            primary.path(),
            ProjectionPreset::Wallet,
            options(),
        )?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome = DeriveStore::open_with_projection_preset(
        primary.path(),
        ProjectionPreset::Complete,
        options(),
    );
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ProjectionPresetRequiresFreshStore {
            requested: "complete",
        })
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);

    let store = DeriveStore::open_with_projection_preset(
        primary.path(),
        ProjectionPreset::Wallet,
        options(),
    )?;
    assert!(matches!(
        store.consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY),
        Err(DeriveStoreError::ConsumerColumnFamilyMissing { name })
            if name == BLOCK_SUMMARY_COLUMN_FAMILY
    ));
    Ok(())
}

#[test]
fn reducing_complete_to_wallet_fails_before_column_family_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        DeriveStore::open_with_projection_preset(
            primary.path(),
            ProjectionPreset::Complete,
            options(),
        )?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome = DeriveStore::open_with_projection_preset(
        primary.path(),
        ProjectionPreset::Wallet,
        options(),
    );
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ProjectionPresetRequiresFreshStore {
            requested: "wallet",
        })
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);
    Ok(())
}

#[test]
fn complete_preflight_rejects_a_foreign_consumer_without_mutation() -> Result<()> {
    let primary = TempDir::new()?;
    {
        let store = DeriveStore::open(
            primary.path(),
            DeriveStoreOptions {
                consumers: &[FOREIGN_SCHEMA],
                ..options()
            },
        )?;
        store.put_consumer(FOREIGN_COLUMN_FAMILY, b"key", b"value")?;
        store.put_chain_event_cursor(FOREIGN_CONSUMER_NAME, b"cursor")?;
    }
    let column_families_before =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;

    let outcome =
        DeriveStore::inspect_projection_store_at_path(primary.path(), ProjectionPreset::Complete);
    assert!(matches!(
        outcome,
        Err(DeriveStoreError::ConsumerNotDeclared {
            consumer,
            persisted_schema_version: 1,
        }) if consumer == FOREIGN_CONSUMER_NAME.as_str()
    ));
    let column_families_after =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), primary.path())?;
    assert_eq!(column_families_after, column_families_before);

    let store = DeriveStore::open(
        primary.path(),
        DeriveStoreOptions {
            consumers: &[FOREIGN_SCHEMA],
            ..options()
        },
    )?;
    assert_eq!(
        store.get_consumer(FOREIGN_COLUMN_FAMILY, b"key")?,
        Some(b"value".to_vec())
    );
    assert_eq!(
        store.get_chain_event_cursor(FOREIGN_CONSUMER_NAME)?,
        Some(b"cursor".to_vec())
    );
    Ok(())
}

#[test]
fn projection_catalog_declares_every_complete_and_wallet_identity_once() {
    let definitions = bundled_projection_definitions();
    assert_eq!(
        definitions.len(),
        ProjectionPreset::Complete.consumer_schemas().len()
    );
    for schema in ProjectionPreset::Complete.consumer_schemas() {
        assert_eq!(
            definitions
                .iter()
                .filter(|definition| definition.schema.name == schema.name)
                .count(),
            1,
            "projection {} must have exactly one product declaration",
            schema.name.as_str()
        );
    }
    let wallet_identities = definitions
        .iter()
        .filter(|definition| definition.included_in(ProjectionPreset::Wallet))
        .map(|definition| definition.schema.name)
        .collect::<Vec<_>>();
    assert_eq!(wallet_identities.len(), 2);
    for schema in ProjectionPreset::Wallet.consumer_schemas() {
        assert!(wallet_identities.contains(&schema.name));
    }
}
