use std::{fs, path::Path};

use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, DBCompressionType, DEFAULT_COLUMN_FAMILY_NAME, IteratorMode,
    Options, WriteBatch, WriteOptions,
};
use zinder_core::{
    CanonicalHistoryBounds, Network, NetworkUpgradeActivations,
    NetworkUpgradeActivationsFingerprint, NetworkUpgradeActivationsFingerprintVersion,
};

use crate::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget,
    build_block_based_table_factory, open_bounded_rocksdb,
};

use super::{
    CanonicalStoreBuildState, CanonicalStoreError, CanonicalStoreReadyEvidence,
    CanonicalStoreWorkload,
    block_replay::BLOCK_REPLAY_COLUMN_FAMILY,
    control::{DecodedStoreControl, decode_store_control, encode_building_store_control},
};

pub(super) const STORE_CONTROL_KEY: &[u8] = b"store_control";
pub(super) const BLOCK_HEADER_COLUMN_FAMILY: &str = "block_header";
pub(super) const BLOCK_HASH_INDEX_COLUMN_FAMILY: &str = "block_hash_index";
const DAILY_VALUE_POOL_BALANCE_COLUMN_FAMILY: &str = "daily_value_pool_balance";
pub(super) const TRANSACTION_LOCATION_COLUMN_FAMILY: &str = "transaction_location";
pub(super) const COMPACT_BLOCK_COLUMN_FAMILY: &str = "compact_block";
pub(super) const TREE_STATE_CHECKPOINT_COLUMN_FAMILY: &str = "tree_state_checkpoint";
pub(super) const BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY: &str =
    "block_final_note_commitment_roots";
const SUBTREE_ROOT_COLUMN_FAMILY: &str = "subtree_root";
const CHAIN_EPOCH_COLUMN_FAMILY: &str = "chain_epoch";
const CHAIN_EVENT_COLUMN_FAMILY: &str = "chain_event";
const MEMPOOL_EVENT_COLUMN_FAMILY: &str = "mempool_event";
const DISPLACED_BLOCK_FACTS_COLUMN_FAMILY: &str = "displaced_block_facts";
pub(super) const BLOCK_BLOB_COLUMN_FAMILY: &str = "block_blob";
pub(super) const TRANSACTION_BLOB_COLUMN_FAMILY: &str = "transaction_blob";

pub(super) const CANONICAL_DATA_COLUMN_FAMILIES: [&str; 15] = [
    BLOCK_HEADER_COLUMN_FAMILY,
    BLOCK_HASH_INDEX_COLUMN_FAMILY,
    BLOCK_REPLAY_COLUMN_FAMILY,
    DAILY_VALUE_POOL_BALANCE_COLUMN_FAMILY,
    TRANSACTION_LOCATION_COLUMN_FAMILY,
    COMPACT_BLOCK_COLUMN_FAMILY,
    TREE_STATE_CHECKPOINT_COLUMN_FAMILY,
    BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY,
    SUBTREE_ROOT_COLUMN_FAMILY,
    CHAIN_EPOCH_COLUMN_FAMILY,
    CHAIN_EVENT_COLUMN_FAMILY,
    MEMPOOL_EVENT_COLUMN_FAMILY,
    DISPLACED_BLOCK_FACTS_COLUMN_FAMILY,
    BLOCK_BLOB_COLUMN_FAMILY,
    TRANSACTION_BLOB_COLUMN_FAMILY,
];

/// One admitted READY canonical version-1 `RocksDB` store.
///
/// Construction is owned exclusively by [`super::RocksDbCanonicalBuilder`].
/// This serving type cannot represent or reopen an unpublished BUILDING store.
pub struct RocksDbCanonicalStore {
    bounded_open: BoundedRocksDbOpen,
    workload: CanonicalStoreWorkload,
    build_plan: super::CanonicalStoreBuildPlan,
    _cursor_auth_key: [u8; 32],
    ready_evidence: CanonicalStoreReadyEvidence,
}

impl RocksDbCanonicalStore {
    /// Opens an existing READY version-1 canonical store after exact admission.
    ///
    /// Admission validates the complete column-family set, singleton control
    /// key, identity, schema, exact network-upgrade activation table, workload,
    /// source range, and readiness evidence before opening a writer that cannot
    /// create data families.
    pub fn open_ready(
        path: impl AsRef<Path>,
        expected_network_upgrade_activations: &NetworkUpgradeActivations,
        expected_workload: CanonicalStoreWorkload,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        let path = path.as_ref();
        let expected_network = expected_network_upgrade_activations.network();
        let expected_activations_fingerprint = expected_network_upgrade_activations
            .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        validate_resource_budget(resource_budget)?;
        let store_path = canonical_store_path(path)?;
        let (admitted_database_identity, admitted_control) = admit_existing_store(
            &store_path,
            expected_network,
            expected_activations_fingerprint,
            expected_workload,
        )?;
        let CanonicalStoreBuildState::Ready(admitted_ready_evidence) = admitted_control.build_state
        else {
            return Err(CanonicalStoreError::StoreNotReady { path: store_path });
        };
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::ExistingPrimary { path: &store_path },
            resource_budget,
            canonical_column_family_descriptors,
        )
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "open ready",
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
                &store_path,
                "database identity changed during admission",
            ));
        }
        let opened_control = validate_open_store_control(
            &bounded_open.db,
            &store_path,
            expected_network,
            expected_activations_fingerprint,
            expected_workload,
        )?;
        if opened_control != admitted_control {
            return Err(CanonicalStoreError::admission(
                &store_path,
                "store control changed during admission",
            ));
        }
        let CanonicalStoreBuildState::Ready(opened_ready_evidence) = opened_control.build_state
        else {
            return Err(CanonicalStoreError::StoreNotReady { path: store_path });
        };
        if opened_ready_evidence != admitted_ready_evidence {
            return Err(CanonicalStoreError::admission(
                &store_path,
                "ready evidence changed during admission",
            ));
        }
        let build_plan = opened_control.build_plan;
        Ok(Self {
            bounded_open,
            workload: expected_workload,
            build_plan,
            _cursor_auth_key: opened_control.cursor_auth_key,
            ready_evidence: opened_ready_evidence,
        })
    }

    /// Returns the immutable network persisted by the store control record.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.build_plan.network()
    }

    /// Returns the admitted activation-table identity persisted by the store.
    #[must_use]
    pub const fn network_upgrade_activations_fingerprint(
        &self,
    ) -> NetworkUpgradeActivationsFingerprint {
        self.build_plan.network_upgrade_activations_fingerprint()
    }

    /// Returns the immutable canonical workload persisted by the store.
    #[must_use]
    pub const fn workload(&self) -> CanonicalStoreWorkload {
        self.workload
    }

    /// Returns the durable boundary of intentionally retained history.
    #[must_use]
    pub const fn history_bounds(&self) -> CanonicalHistoryBounds {
        self.build_plan.history_bounds()
    }

    /// Returns the complete admitted canonical construction identity.
    #[must_use]
    pub const fn build_plan(&self) -> &super::CanonicalStoreBuildPlan {
        &self.build_plan
    }

    /// Returns the evidence that admitted this store as READY.
    #[must_use]
    pub const fn ready_evidence(&self) -> CanonicalStoreReadyEvidence {
        self.ready_evidence
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

pub(super) fn canonical_column_family_descriptors(
    block_cache: &Cache,
    resource_budget: RocksDbResourceBudget,
) -> Vec<ColumnFamilyDescriptor> {
    CANONICAL_DATA_COLUMN_FAMILIES
        .into_iter()
        .map(|name| {
            ColumnFamilyDescriptor::new(name, canonical_data_options(block_cache, resource_budget))
        })
        .collect()
}

pub(super) fn canonical_data_options(
    block_cache: &Cache,
    resource_budget: RocksDbResourceBudget,
) -> Options {
    let mut options = Options::default();
    options.set_compression_type(DBCompressionType::Snappy);
    options.set_block_based_table_factory(&build_block_based_table_factory(block_cache));
    options.set_write_buffer_size(
        usize::try_from(resource_budget.write_buffer_bytes).unwrap_or(usize::MAX),
    );
    options.set_max_write_buffer_number(resource_budget.max_write_buffer_count);
    options
}

pub(super) fn canonical_store_path(path: &Path) -> Result<std::path::PathBuf, CanonicalStoreError> {
    fs::canonicalize(path).map_err(|source| CanonicalStoreError::PathUnavailable {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn validate_resource_budget(
    resource_budget: RocksDbResourceBudget,
) -> Result<(), CanonicalStoreError> {
    resource_budget
        .validate()
        .map_err(|reason| CanonicalStoreError::InvalidResourceBudget { reason })
}

pub(super) fn create_fresh_directory(path: &Path) -> Result<(), CanonicalStoreError> {
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

pub(super) fn initialize_store_identity(
    path: &Path,
    workload: CanonicalStoreWorkload,
    build_plan: &super::CanonicalStoreBuildPlan,
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
    let encoded_control = encode_building_store_control(workload, build_plan, cursor_auth_key)
        .map_err(|source| CanonicalStoreError::admission(path, source.to_string()))?;
    batch.put(STORE_CONTROL_KEY, encoded_control);
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
    expected_activations_fingerprint: NetworkUpgradeActivationsFingerprint,
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
    let control = validate_open_store_control(
        &db,
        path,
        expected_network,
        expected_activations_fingerprint,
        expected_workload,
    )?;
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
    expected_activations_fingerprint: NetworkUpgradeActivationsFingerprint,
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
    let persisted_activations_fingerprint = persisted
        .build_plan
        .network_upgrade_activations_fingerprint();
    if persisted_activations_fingerprint != expected_activations_fingerprint {
        return Err(CanonicalStoreError::admission(
            path,
            "persisted network upgrade activations do not equal the requested activation table",
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
    use zinder_core::{BlockHash, BlockHeight, BlockId};

    use super::*;
    use crate::{
        CANONICAL_STORE_IDENTITY, CANONICAL_STORE_SCHEMA_VERSION, CanonicalStoreBuildPlan,
        RocksDbCanonicalBuilder,
    };

    #[test]
    fn building_layout_is_exact_and_not_servable() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("canonical");
        let store = RocksDbCanonicalBuilder::create_fresh(
            &path,
            CanonicalStoreWorkload::Explorer,
            complete_build_plan()?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(store.network(), Network::ZcashTestnet);
        let testnet_activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        assert_eq!(
            store.network_upgrade_activations_fingerprint(),
            testnet_activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
        );
        assert_eq!(store.workload(), CanonicalStoreWorkload::Explorer);
        assert_eq!(
            store.build_plan().history_bounds(),
            CanonicalHistoryBounds::complete()
        );
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

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &testnet_activations,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("a BUILDING store must not be servable")?;
        assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));

        let control_before = read_control(&path)?;
        let mainnet_activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashMainnet)?;
        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &mainnet_activations,
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

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &testnet_activations,
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
    fn ready_open_rejects_activation_table_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("activation-mismatch");
        let store = RocksDbCanonicalBuilder::create_fresh(
            &path,
            CanonicalStoreWorkload::Explorer,
            complete_build_plan()?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        drop(store);

        let testnet_activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        let mut shifted_activation_rows = testnet_activations.activations().to_vec();
        let latest_activation = shifted_activation_rows
            .last_mut()
            .ok_or("canonical activation fixture must not be empty")?;
        latest_activation.activation_height = BlockHeight::new(
            latest_activation
                .activation_height
                .value()
                .saturating_add(1),
        );
        let shifted_activations = zinder_core::NetworkUpgradeActivations::new(
            Network::ZcashTestnet,
            shifted_activation_rows,
        )?;

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &shifted_activations,
            CanonicalStoreWorkload::Explorer,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("activation mismatch should be rejected")?;
        assert!(error.to_string().contains("network upgrade activations"));
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

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
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
        let store = RocksDbCanonicalBuilder::create_fresh(
            &path,
            CanonicalStoreWorkload::Explorer,
            complete_build_plan()?,
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

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
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
    fn ready_open_does_not_recreate_a_missing_required_family()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("missing-family");
        let store = RocksDbCanonicalBuilder::create_fresh(
            &path,
            CanonicalStoreWorkload::Explorer,
            complete_build_plan()?,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        drop(store);

        let column_families = DB::list_cf(&Options::default(), &path)?;
        let db = DB::open_cf(&Options::default(), &path, &column_families)?;
        db.drop_cf(TRANSACTION_BLOB_COLUMN_FAMILY)?;
        drop(db);
        let families_without_transaction_blobs = DB::list_cf(&Options::default(), &path)?;

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
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

    fn complete_build_plan() -> Result<CanonicalStoreBuildPlan, Box<dyn std::error::Error>> {
        let activations =
            crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?;
        Ok(CanonicalStoreBuildPlan::complete(
            &activations,
            0,
            BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([2; 32])),
        )?)
    }
}
