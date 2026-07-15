use std::{fs, path::Path};

use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, DBCompressionType, DEFAULT_COLUMN_FAMILY_NAME, IteratorMode,
    Options, WriteBatch, WriteOptions,
};
use zinder_core::{CanonicalHistoryBounds, Network};

use crate::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget,
    build_block_based_table_factory, open_bounded_rocksdb,
};

use super::{
    CanonicalStoreBuildState, CanonicalStoreError, CanonicalStoreWorkload,
    control::{DecodedStoreControl, decode_store_control, encode_building_store_control},
};

const STORE_CONTROL_KEY: &[u8] = b"store_control";
const BLOCK_HEADER_COLUMN_FAMILY: &str = "block_header";
const BLOCK_HASH_INDEX_COLUMN_FAMILY: &str = "block_hash_index";
const BLOCK_REPLAY_COLUMN_FAMILY: &str = "block_replay";
const BLOCK_VALUE_POOL_BALANCES_COLUMN_FAMILY: &str = "block_value_pool_balances";
const TRANSACTION_LOCATION_COLUMN_FAMILY: &str = "transaction_location";
const COMPACT_BLOCK_COLUMN_FAMILY: &str = "compact_block";
const TREE_STATE_COLUMN_FAMILY: &str = "tree_state";
const FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY: &str = "final_note_commitment_roots";
const SUBTREE_ROOT_COLUMN_FAMILY: &str = "subtree_root";
const CHAIN_EVENT_COLUMN_FAMILY: &str = "chain_event";
const MEMPOOL_EVENT_COLUMN_FAMILY: &str = "mempool_event";
const DISPLACED_BLOCK_FACTS_COLUMN_FAMILY: &str = "displaced_block_facts";
const BLOCK_BLOB_COLUMN_FAMILY: &str = "block_blob";
const TRANSACTION_BLOB_COLUMN_FAMILY: &str = "transaction_blob";

const CANONICAL_DATA_COLUMN_FAMILIES: [&str; 14] = [
    BLOCK_HEADER_COLUMN_FAMILY,
    BLOCK_HASH_INDEX_COLUMN_FAMILY,
    BLOCK_REPLAY_COLUMN_FAMILY,
    BLOCK_VALUE_POOL_BALANCES_COLUMN_FAMILY,
    TRANSACTION_LOCATION_COLUMN_FAMILY,
    COMPACT_BLOCK_COLUMN_FAMILY,
    TREE_STATE_COLUMN_FAMILY,
    FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
    SUBTREE_ROOT_COLUMN_FAMILY,
    CHAIN_EVENT_COLUMN_FAMILY,
    MEMPOOL_EVENT_COLUMN_FAMILY,
    DISPLACED_BLOCK_FACTS_COLUMN_FAMILY,
    BLOCK_BLOB_COLUMN_FAMILY,
    TRANSACTION_BLOB_COLUMN_FAMILY,
];

/// One admitted, clean version-1 `RocksDB` canonical store.
///
/// This type currently owns layout creation and admission. Bulk construction
/// and live commits will be added against these already-fixed data families;
/// the diagnostic benchmark candidate is intentionally not reused here.
pub struct RocksDbCanonicalStore {
    bounded_open: BoundedRocksDbOpen,
    network: Network,
    workload: CanonicalStoreWorkload,
    history_bounds: CanonicalHistoryBounds,
    _cursor_auth_key: [u8; 32],
    build_state: CanonicalStoreBuildState,
}

impl RocksDbCanonicalStore {
    /// Creates a new version-1 canonical store at a path that does not exist.
    ///
    /// Existing paths are refused before `RocksDB` is opened. This builder
    /// never adopts, deletes, migrates, or repairs another store layout.
    pub fn create_fresh(
        path: impl AsRef<Path>,
        network: Network,
        workload: CanonicalStoreWorkload,
        history_bounds: CanonicalHistoryBounds,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        let path = path.as_ref();
        validate_resource_budget(resource_budget)?;
        let mut cursor_auth_key = [0; 32];
        getrandom::fill(&mut cursor_auth_key)
            .map_err(|source| CanonicalStoreError::EntropyUnavailable { source })?;
        create_fresh_directory(path)?;
        initialize_store_identity(path, network, workload, history_bounds, cursor_auth_key)?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary { path },
            resource_budget,
            canonical_column_family_descriptors,
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "create",
            source,
        })?;
        Ok(Self {
            bounded_open,
            network,
            workload,
            history_bounds,
            _cursor_auth_key: cursor_auth_key,
            build_state: CanonicalStoreBuildState::Building,
        })
    }

    /// Reopens an existing version-1 canonical store after exact admission.
    ///
    /// Admission validates the complete column-family set, singleton control
    /// key, identity, schema, build-state encoding, and network before opening
    /// a writer that is forbidden from creating a database or data family.
    pub fn reopen(
        path: impl AsRef<Path>,
        expected_network: Network,
        expected_workload: CanonicalStoreWorkload,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        let path = path.as_ref();
        validate_resource_budget(resource_budget)?;
        let (admitted_database_identity, admitted_control) =
            admit_existing_store(path, expected_network, expected_workload)?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::ExistingPrimary { path },
            resource_budget,
            canonical_column_family_descriptors,
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "reopen",
            source,
        })?;
        let opened_database_identity = bounded_open.db.get_db_identity().map_err(|source| {
            CanonicalStoreError::RocksDbOperation {
                operation: "database identity read",
                source,
            }
        })?;
        if opened_database_identity != admitted_database_identity {
            return Err(CanonicalStoreError::admission(
                path,
                "database identity changed during admission",
            ));
        }
        let opened_control = validate_open_store_control(
            &bounded_open.db,
            path,
            expected_network,
            expected_workload,
        )?;
        if opened_control != admitted_control {
            return Err(CanonicalStoreError::admission(
                path,
                "store control changed during admission",
            ));
        }
        Ok(Self {
            bounded_open,
            network: expected_network,
            workload: expected_workload,
            history_bounds: opened_control.history_bounds,
            _cursor_auth_key: opened_control.cursor_auth_key,
            build_state: opened_control.build_state,
        })
    }

    /// Returns the immutable network persisted by the store control record.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Returns the immutable canonical workload persisted by the store.
    #[must_use]
    pub const fn workload(&self) -> CanonicalStoreWorkload {
        self.workload
    }

    /// Returns the durable boundary of intentionally retained history.
    #[must_use]
    pub const fn history_bounds(&self) -> CanonicalHistoryBounds {
        self.history_bounds
    }

    /// Returns whether this store is still building or has published evidence.
    #[must_use]
    pub const fn build_state(&self) -> CanonicalStoreBuildState {
        self.build_state
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

fn canonical_column_family_descriptors(
    block_cache: &Cache,
    resource_budget: RocksDbResourceBudget,
) -> Vec<ColumnFamilyDescriptor> {
    CANONICAL_DATA_COLUMN_FAMILIES
        .into_iter()
        .map(|name| {
            let mut options = Options::default();
            options.set_compression_type(DBCompressionType::Snappy);
            options.set_block_based_table_factory(&build_block_based_table_factory(block_cache));
            options.set_write_buffer_size(
                usize::try_from(resource_budget.write_buffer_bytes).unwrap_or(usize::MAX),
            );
            options.set_max_write_buffer_number(resource_budget.max_write_buffer_count);
            ColumnFamilyDescriptor::new(name, options)
        })
        .collect()
}

fn validate_resource_budget(
    resource_budget: RocksDbResourceBudget,
) -> Result<(), CanonicalStoreError> {
    resource_budget
        .validate()
        .map_err(|reason| CanonicalStoreError::InvalidResourceBudget { reason })
}

fn create_fresh_directory(path: &Path) -> Result<(), CanonicalStoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| CanonicalStoreError::PathUnavailable {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CanonicalStoreError::PathNotFresh {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(CanonicalStoreError::PathUnavailable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn initialize_store_identity(
    path: &Path,
    network: Network,
    workload: CanonicalStoreWorkload,
    history_bounds: CanonicalHistoryBounds,
    cursor_auth_key: [u8; 32],
) -> Result<(), CanonicalStoreError> {
    let mut database_options = Options::default();
    database_options.create_if_missing(true);
    let db = DB::open(&database_options, path).map_err(|source| {
        CanonicalStoreError::RocksDbOperation {
            operation: "identity open",
            source,
        }
    })?;
    let mut batch = WriteBatch::default();
    batch.put(
        STORE_CONTROL_KEY,
        encode_building_store_control(network, workload, history_bounds, cursor_auth_key),
    );
    let mut options = WriteOptions::default();
    options.disable_wal(false);
    options.set_sync(true);
    db.write_opt(&batch, &options)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "identity initialization",
            source,
        })
}

fn admit_existing_store(
    path: &Path,
    expected_network: Network,
    expected_workload: CanonicalStoreWorkload,
) -> Result<(Vec<u8>, DecodedStoreControl), CanonicalStoreError> {
    let column_families = DB::list_cf(&Options::default(), path).map_err(|source| {
        CanonicalStoreError::admission(path, format!("column-family discovery failed: {source}"))
    })?;
    validate_exact_column_families(path, &column_families)?;
    let db = DB::open_cf_for_read_only(&Options::default(), path, &column_families, false)
        .map_err(|source| {
            CanonicalStoreError::admission(path, format!("read-only open failed: {source}"))
        })?;
    let control = validate_open_store_control(&db, path, expected_network, expected_workload)?;
    let database_identity = db.get_db_identity().map_err(|source| {
        CanonicalStoreError::admission(path, format!("database identity read failed: {source}"))
    })?;
    Ok((database_identity, control))
}

fn validate_exact_column_families(
    path: &Path,
    observed: &[String],
) -> Result<(), CanonicalStoreError> {
    let mut observed = observed.to_vec();
    observed.sort_unstable();
    let mut expected = CANONICAL_DATA_COLUMN_FAMILIES
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    expected.push(DEFAULT_COLUMN_FAMILY_NAME.to_owned());
    expected.sort_unstable();
    if observed != expected {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "column families {observed:?} do not exactly match required canonical version-1 set {expected:?}"
            ),
        ));
    }
    Ok(())
}

fn validate_open_store_control(
    db: &DB,
    path: &Path,
    expected_network: Network,
    expected_workload: CanonicalStoreWorkload,
) -> Result<DecodedStoreControl, CanonicalStoreError> {
    let mut control = None;
    for row in db.iterator(IteratorMode::Start) {
        let (key, encoded_control) = row.map_err(|source| {
            CanonicalStoreError::admission(
                path,
                format!("store-control iteration failed: {source}"),
            )
        })?;
        match key.as_ref() {
            STORE_CONTROL_KEY => control = Some(encoded_control),
            unknown => {
                return Err(CanonicalStoreError::admission(
                    path,
                    format!("unexpected default-column-family key {unknown:?}"),
                ));
            }
        }
    }
    let control =
        control.ok_or_else(|| CanonicalStoreError::admission(path, "store identity is absent"))?;
    let persisted = decode_store_control(path, &control)?;
    if persisted.network != expected_network {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "persisted network {:?} does not equal requested network {expected_network:?}",
                persisted.network
            ),
        ));
    }
    if persisted.workload != expected_workload {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "persisted workload {} does not equal requested workload {}",
                persisted.workload.as_str(),
                expected_workload.as_str()
            ),
        ));
    }
    Ok(persisted)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::{CANONICAL_STORE_IDENTITY, CANONICAL_STORE_SCHEMA_VERSION};

    #[test]
    fn exact_version_one_layout_reopens_only_for_its_network()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("canonical");
        let store = RocksDbCanonicalStore::create_fresh(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            CanonicalHistoryBounds::complete(),
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(store.network(), Network::ZcashTestnet);
        assert_eq!(store.workload(), CanonicalStoreWorkload::Explorer);
        assert_eq!(store.history_bounds(), CanonicalHistoryBounds::complete());
        assert_eq!(store.build_state(), CanonicalStoreBuildState::Building);
        drop(store);

        let mut observed = DB::list_cf(&Options::default(), &path)?;
        observed.sort_unstable();
        let mut expected = CANONICAL_DATA_COLUMN_FAMILIES
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        expected.push(DEFAULT_COLUMN_FAMILY_NAME.to_owned());
        expected.sort_unstable();
        assert_eq!(observed, expected);

        let reopened = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(reopened.network(), Network::ZcashTestnet);
        drop(reopened);

        let control_before = read_control(&path)?;
        let error = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashMainnet,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("network mismatch should be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::AdmissionRefused { .. }
        ));
        assert_eq!(read_control(&path)?, control_before);

        let error = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Wallet,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("workload mismatch should be rejected")?;
        assert!(error.to_string().contains("persisted workload explorer"));
        assert_eq!(read_control(&path)?, control_before);
        Ok(())
    }

    #[test]
    fn old_looking_schema_one_path_is_refused_without_adoption()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("old-schema-one");
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = CANONICAL_DATA_COLUMN_FAMILIES
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
        let db = DB::open_cf_descriptors(&options, &path, descriptors)?;
        db.put(b"schema_version", 1_u16.to_le_bytes())?;
        drop(db);

        let error = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("old-looking schema should be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::AdmissionRefused { .. }
        ));

        let column_families = DB::list_cf(&Options::default(), &path)?;
        let db = DB::open_cf_for_read_only(&Options::default(), &path, &column_families, false)?;
        assert_eq!(
            db.get(b"schema_version")?,
            Some(1_u16.to_le_bytes().to_vec())
        );
        assert_eq!(db.get(STORE_CONTROL_KEY)?, None);
        Ok(())
    }

    #[test]
    fn another_schema_version_is_refused_without_rewriting_control()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("wrong-schema-version");
        let store = RocksDbCanonicalStore::create_fresh(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            CanonicalHistoryBounds::complete(),
            RocksDbResourceBudget::for_local_tests(),
        )?;
        drop(store);

        let column_families = DB::list_cf(&Options::default(), &path)?;
        let db = DB::open_cf(&Options::default(), &path, &column_families)?;
        let mut control = db.get(STORE_CONTROL_KEY)?.ok_or("control should exist")?;
        let schema_version_start = CANONICAL_STORE_IDENTITY.len();
        control[schema_version_start..schema_version_start + 2]
            .copy_from_slice(&2_u16.to_le_bytes());
        db.put(STORE_CONTROL_KEY, &control)?;
        drop(db);

        let error = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("another schema version should be rejected")?;
        assert!(error.to_string().contains("schema version 2"), "{error}");
        assert_eq!(read_control(&path)?, control);
        Ok(())
    }

    #[test]
    fn builder_refuses_every_existing_path() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let error = RocksDbCanonicalStore::create_fresh(
            temporary.path(),
            Network::ZcashRegtest,
            CanonicalStoreWorkload::Wallet,
            CanonicalHistoryBounds::complete(),
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("existing path should be rejected")?;
        assert!(matches!(error, CanonicalStoreError::PathNotFresh { .. }));
        Ok(())
    }

    #[test]
    fn reopen_does_not_recreate_a_missing_required_family() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("missing-family");
        let store = RocksDbCanonicalStore::create_fresh(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            CanonicalHistoryBounds::complete(),
            RocksDbResourceBudget::for_local_tests(),
        )?;
        drop(store);

        let column_families = DB::list_cf(&Options::default(), &path)?;
        let db = DB::open_cf(&Options::default(), &path, &column_families)?;
        db.drop_cf(TRANSACTION_BLOB_COLUMN_FAMILY)?;
        drop(db);
        let families_without_transaction_blobs = DB::list_cf(&Options::default(), &path)?;

        let error = RocksDbCanonicalStore::reopen(
            &path,
            Network::ZcashTestnet,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("missing required family should be rejected")?;
        assert!(matches!(
            error,
            CanonicalStoreError::AdmissionRefused { .. }
        ));
        assert_eq!(
            DB::list_cf(&Options::default(), &path)?,
            families_without_transaction_blobs
        );
        assert!(
            !families_without_transaction_blobs
                .iter()
                .any(|family| family == TRANSACTION_BLOB_COLUMN_FAMILY)
        );
        Ok(())
    }

    #[test]
    fn contract_identity_and_schema_are_exactly_version_one() {
        assert_eq!(CANONICAL_STORE_IDENTITY, "canonical");
        assert_eq!(CANONICAL_STORE_SCHEMA_VERSION, 1);
    }

    fn read_control(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let column_families = DB::list_cf(&Options::default(), path)?;
        let db = DB::open_cf_for_read_only(&Options::default(), path, &column_families, false)?;
        db.get(STORE_CONTROL_KEY)?
            .ok_or_else(|| "store control should exist".into())
    }
}
