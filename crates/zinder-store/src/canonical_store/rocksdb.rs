use std::{fs, path::Path, sync::Arc};

use parking_lot::Mutex;
use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, DBCompressionType, DEFAULT_COLUMN_FAMILY_NAME, IteratorMode,
    Options, WriteBatch, WriteOptions, checkpoint::Checkpoint,
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
    CANONICAL_STORE_IDENTITY, CANONICAL_STORE_SCHEMA_VERSION, CanonicalStoreBuildState,
    CanonicalStoreError, CanonicalStoreReadyEvidence, CanonicalStoreWorkload,
    block_replay::{BLOCK_REPLAY_COLUMN_FAMILY, CanonicalReplayScan},
    construction_manifest::{
        copy_construction_manifest, read_construction_manifest_binding,
        validate_ready_construction_manifest,
    },
    control::{DecodedStoreControl, decode_store_control, encode_building_store_control},
    event_lifecycle::{
        PROJECTION_BUILD_LEASE_GENERATION_KEY, RETENTION_FLOOR_KEY, is_projection_build_lease_key,
    },
    mempool_lifecycle::{
        MEMPOOL_EVENT_RETENTION_FLOOR_KEY, MEMPOOL_EVENT_SEQUENCE_KEY,
        validate_mempool_lifecycle_admission,
    },
    publication::validate_ready_publication,
};

/// Cold-admitted identity and READY evidence for an owner-created canonical checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalOwnerCheckpointEvidence {
    /// Exact `RocksDB` database identity captured before physical checkpoint
    /// creation and re-read from the cold checkpoint.
    pub database_identity: Vec<u8>,
    /// Exact physical store identity admitted from the checkpoint.
    pub store_identity: &'static str,
    /// Exact physical schema admitted from the checkpoint.
    pub schema_version: u16,
    /// Immutable workload admitted from the checkpoint.
    pub workload: CanonicalStoreWorkload,
    /// Complete canonical construction identity admitted from the checkpoint.
    pub build_plan: super::CanonicalStoreBuildPlan,
    /// Persisted READY evidence read from the cold-opened checkpoint.
    pub ready_evidence: CanonicalStoreReadyEvidence,
}

/// Immutable context captured by the canonical owner when it creates a
/// physical checkpoint.
///
/// This opaque context lets a background worker cold-admit the immutable copy
/// without reopening or retaining the writer's primary handle. It deliberately
/// carries no filesystem authority and is only accepted by
/// [`RocksDbCanonicalStore::cold_admit_owner_checkpoint`].
#[derive(Clone, Debug)]
pub struct CanonicalOwnerCheckpointAdmission {
    workload: CanonicalStoreWorkload,
    build_plan: super::CanonicalStoreBuildPlan,
    database_identity: Vec<u8>,
}

pub(super) const STORE_CONTROL_KEY: &[u8] = b"store_control";
pub(super) const BLOCK_HEADER_COLUMN_FAMILY: &str = "block_header";
pub(super) const BLOCK_HASH_INDEX_COLUMN_FAMILY: &str = "block_hash_index";
pub(super) const DAILY_VALUE_POOL_BALANCE_COLUMN_FAMILY: &str = "daily_value_pool_balance";
pub(super) const TRANSACTION_LOCATION_COLUMN_FAMILY: &str = "transaction_location";
pub(super) const COMPACT_BLOCK_COLUMN_FAMILY: &str = "compact_block";
pub(super) const TREE_STATE_CHECKPOINT_COLUMN_FAMILY: &str = "tree_state_checkpoint";
pub(super) const BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_COLUMN_FAMILY: &str =
    "block_final_note_commitment_roots";
pub(super) const SUBTREE_ROOT_COLUMN_FAMILY: &str = "subtree_root";
pub(super) const CHAIN_EPOCH_COLUMN_FAMILY: &str = "chain_epoch";
pub(super) const CHAIN_EVENT_COLUMN_FAMILY: &str = "chain_event";
pub(super) const MEMPOOL_EVENT_COLUMN_FAMILY: &str = "mempool_event";
pub(super) const DISPLACED_BLOCK_FACTS_COLUMN_FAMILY: &str = "displaced_block_facts";
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

#[derive(Clone, Copy)]
pub(super) struct CanonicalStoreAdmissionExpectation {
    network: Network,
    activations_fingerprint: NetworkUpgradeActivationsFingerprint,
    workload: CanonicalStoreWorkload,
    reorg_policy: super::CanonicalReorgPolicy,
}

impl CanonicalStoreAdmissionExpectation {
    pub(super) fn from_activations(
        activations: &NetworkUpgradeActivations,
        workload: CanonicalStoreWorkload,
        reorg_policy: super::CanonicalReorgPolicy,
    ) -> Self {
        Self {
            network: activations.network(),
            activations_fingerprint: activations
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            workload,
            reorg_policy,
        }
    }

    pub(super) fn from_build_plan(
        build_plan: &super::CanonicalStoreBuildPlan,
        workload: CanonicalStoreWorkload,
    ) -> Self {
        Self {
            network: build_plan.network(),
            activations_fingerprint: build_plan.network_upgrade_activations_fingerprint(),
            workload,
            reorg_policy: build_plan.reorg_policy(),
        }
    }
}

/// One admitted READY canonical version-1 `RocksDB` store.
///
/// Construction is owned exclusively by [`super::RocksDbCanonicalBuilder`].
/// This serving type cannot represent or reopen an unpublished BUILDING store.
pub struct RocksDbCanonicalStore {
    pub(super) bounded_open: BoundedRocksDbOpen,
    pub(super) workload: CanonicalStoreWorkload,
    pub(super) build_plan: super::CanonicalStoreBuildPlan,
    pub(super) cursor_auth_key: [u8; 32],
    pub(super) ready_evidence: CanonicalStoreReadyEvidence,
    /// Serializes retained-event, projection-lease, and mempool-log lifecycle changes.
    ///
    /// The primary handle owns every canonical mutation. Keeping this lock on
    /// that handle makes the pruning floor and the durable lease set one
    /// indivisible lifecycle boundary without exposing a second writer API.
    pub(super) lifecycle_lock: Arc<Mutex<()>>,
}

impl RocksDbCanonicalStore {
    pub(super) fn from_published(
        bounded_open: BoundedRocksDbOpen,
        workload: CanonicalStoreWorkload,
        build_plan: super::CanonicalStoreBuildPlan,
        cursor_auth_key: [u8; 32],
        ready_evidence: &CanonicalStoreReadyEvidence,
    ) -> Self {
        Self {
            bounded_open,
            workload,
            build_plan,
            cursor_auth_key,
            ready_evidence: *ready_evidence,
            lifecycle_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Opens an existing READY version-1 canonical store after exact admission.
    ///
    /// Admission validates the complete column-family set, singleton control
    /// key, identity, schema, exact network-upgrade activation table, workload,
    /// canonical reorg policy, source range, and readiness evidence before
    /// opening a writer that cannot create data families.
    pub fn open_ready(
        path: impl AsRef<Path>,
        expected_network_upgrade_activations: &NetworkUpgradeActivations,
        expected_workload: CanonicalStoreWorkload,
        expected_reorg_policy: super::CanonicalReorgPolicy,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        let path = path.as_ref();
        let expectation = CanonicalStoreAdmissionExpectation::from_activations(
            expected_network_upgrade_activations,
            expected_workload,
            expected_reorg_policy,
        );
        Self::open_ready_with_expectation(path, expectation, resource_budget)
    }

    fn open_ready_with_expectation(
        path: &Path,
        expectation: CanonicalStoreAdmissionExpectation,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, CanonicalStoreError> {
        validate_resource_budget(resource_budget)?;
        let store_path = canonical_store_path(path)?;
        let (admitted_database_identity, admitted_control) =
            admit_existing_store(&store_path, expectation)?;
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
        let opened_control =
            validate_open_store_control(&bounded_open.db, &store_path, expectation)?;
        validate_mempool_lifecycle_admission(
            &bounded_open.db,
            opened_control.network,
            opened_control.cursor_auth_key,
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
        validate_ready_publication(&bounded_open.db, &build_plan, &opened_ready_evidence)?;
        Ok(Self {
            bounded_open,
            workload: expectation.workload,
            build_plan,
            cursor_auth_key: opened_control.cursor_auth_key,
            ready_evidence: opened_ready_evidence,
            lifecycle_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Creates and cold-admits one physical checkpoint from this canonical owner.
    ///
    /// `target` must not exist. The returned identity and READY evidence are
    /// read through exact cold admission of the completed checkpoint, never
    /// copied from this live handle. Requiring mutable access keeps this API on
    /// the canonical owner's mutation surface; serving secondaries expose no
    /// checkpoint operation.
    pub fn create_owner_checkpoint(
        &mut self,
        target: impl AsRef<Path>,
        admission_resource_budget: RocksDbResourceBudget,
    ) -> Result<CanonicalOwnerCheckpointEvidence, CanonicalStoreError> {
        let target = target.as_ref();
        let admission = self.create_owner_checkpoint_physical(target)?;
        Self::cold_admit_owner_checkpoint(target, &admission, admission_resource_budget)
    }

    /// Creates the physical checkpoint while the canonical owner holds the
    /// primary handle, then returns immutable admission context.
    ///
    /// Callers must run [`Self::cold_admit_owner_checkpoint`] outside the
    /// writer's serialization queue. A full canonical cold admission may scan
    /// a mainnet-sized immutable copy for a long time; holding primary
    /// ownership through that scan would stall live following.
    pub fn create_owner_checkpoint_physical(
        &mut self,
        target: impl AsRef<Path>,
    ) -> Result<CanonicalOwnerCheckpointAdmission, CanonicalStoreError> {
        let target = target.as_ref();
        require_absent_checkpoint_target(target)?;
        let admission = self.owner_checkpoint_readmission(target)?;
        let checkpoint = Checkpoint::new(&self.bounded_open.db).map_err(|source| {
            CanonicalStoreError::CheckpointFailed {
                path: target.to_path_buf(),
                source,
            }
        })?;
        checkpoint.create_checkpoint(target).map_err(|source| {
            CanonicalStoreError::CheckpointFailed {
                path: target.to_path_buf(),
                source,
            }
        })?;
        copy_construction_manifest(self.bounded_open.db.path(), target)?;
        Ok(admission)
    }

    /// Captures an opaque primary-owner admission context for re-admitting an
    /// existing physical checkpoint.
    ///
    /// The context contains no filesystem authority and can only be consumed
    /// by [`Self::cold_admit_owner_checkpoint`], which still performs complete
    /// cold admission. The canonical control owner obtains this context on its
    /// serialized primary queue immediately before it re-admits a checkpoint;
    /// secondaries never expose this operation.
    pub fn owner_checkpoint_readmission(
        &self,
        target: impl AsRef<Path>,
    ) -> Result<CanonicalOwnerCheckpointAdmission, CanonicalStoreError> {
        let target = target.as_ref();
        let database_identity = self.bounded_open.db.get_db_identity().map_err(|source| {
            CanonicalStoreError::CheckpointFailed {
                path: target.to_path_buf(),
                source,
            }
        })?;
        Ok(CanonicalOwnerCheckpointAdmission {
            workload: self.workload,
            build_plan: self.build_plan.clone(),
            database_identity,
        })
    }

    /// Cold-admits an immutable owner-created checkpoint without opening or
    /// retaining its source primary.
    ///
    /// This is intentionally a static operation so the expensive complete
    /// readback runs independently of the canonical writer queue.
    pub fn cold_admit_owner_checkpoint(
        target: impl AsRef<Path>,
        admission: &CanonicalOwnerCheckpointAdmission,
        admission_resource_budget: RocksDbResourceBudget,
    ) -> Result<CanonicalOwnerCheckpointEvidence, CanonicalStoreError> {
        validate_resource_budget(admission_resource_budget)?;
        let target = target.as_ref();
        let expectation = CanonicalStoreAdmissionExpectation::from_build_plan(
            &admission.build_plan,
            admission.workload,
        );
        let cold_checkpoint =
            Self::open_ready_with_expectation(target, expectation, admission_resource_budget)?;
        let cold_database_identity =
            cold_checkpoint
                .bounded_open
                .db
                .get_db_identity()
                .map_err(|source| CanonicalStoreError::CheckpointFailed {
                    path: target.to_path_buf(),
                    source,
                })?;
        if cold_database_identity != admission.database_identity {
            return Err(CanonicalStoreError::admission(
                target,
                "checkpoint database identity differs from the physical owner checkpoint",
            ));
        }
        Ok(CanonicalOwnerCheckpointEvidence {
            database_identity: cold_database_identity,
            store_identity: CANONICAL_STORE_IDENTITY,
            schema_version: CANONICAL_STORE_SCHEMA_VERSION,
            workload: cold_checkpoint.workload,
            build_plan: cold_checkpoint.build_plan.clone(),
            ready_evidence: cold_checkpoint.ready_evidence,
        })
    }

    /// Reads the immutable construction-manifest identity without opening a
    /// `RocksDB` primary. Archive packagers use this narrow descriptor to bind
    /// a physical checkpoint to the first READY construction proof.
    pub fn read_construction_manifest_binding(
        path: impl AsRef<Path>,
    ) -> Result<super::CanonicalConstructionManifestBinding, CanonicalStoreError> {
        read_construction_manifest_binding(path.as_ref())
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

    /// Returns the immutable maximum supported canonical replacement depth.
    #[must_use]
    pub const fn reorg_policy(&self) -> super::CanonicalReorgPolicy {
        self.build_plan.reorg_policy()
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

    /// Returns the authenticated replay prefix through the settled tip.
    #[must_use]
    pub const fn sequence_checkpoint(&self) -> super::CanonicalSequenceCheckpoint {
        self.ready_evidence.sequence_checkpoint
    }

    /// Scans the complete published canonical replay exactly once in height order.
    ///
    /// The scan bypasses the `RocksDB` block cache, decodes and authenticates every
    /// replay row, and verifies the final count, tip, and ordered sequence digest
    /// against the READY record before it terminates successfully.
    pub fn scan_canonical_replay(&self) -> Result<CanonicalReplayScan<'_>, CanonicalStoreError> {
        CanonicalReplayScan::new(&self.bounded_open.db, &self.ready_evidence)
    }

    /// Returns the filesystem I/O mode selected by the bounded `RocksDB` open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }
}

fn require_absent_checkpoint_target(path: &Path) -> Result<(), CanonicalStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(CanonicalStoreError::CheckpointTargetExists {
            path: path.to_path_buf(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CanonicalStoreError::PathUnavailable {
            path: path.to_path_buf(),
            source,
        }),
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

pub(super) fn admit_existing_store(
    path: &Path,
    expectation: CanonicalStoreAdmissionExpectation,
) -> Result<(Vec<u8>, DecodedStoreControl), CanonicalStoreError> {
    let column_families = DB::list_cf(&Options::default(), path).map_err(|source| {
        CanonicalStoreError::admission(path, format!("column-family discovery failed: {source}"))
    })?;
    validate_exact_column_families(path, &column_families)?;
    let db = DB::open_cf_for_read_only(&Options::default(), path, &column_families, false)
        .map_err(|source| {
            CanonicalStoreError::admission(path, format!("read-only open failed: {source}"))
        })?;
    let control = validate_open_store_control(&db, path, expectation)?;
    if let CanonicalStoreBuildState::Ready(ready) = control.build_state {
        validate_ready_construction_manifest(path, &ready)?;
    }
    validate_mempool_lifecycle_admission(&db, control.network, control.cursor_auth_key)?;
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

pub(super) fn validate_open_store_control(
    db: &DB,
    path: &Path,
    expectation: CanonicalStoreAdmissionExpectation,
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
            RETENTION_FLOOR_KEY
            | PROJECTION_BUILD_LEASE_GENERATION_KEY
            | MEMPOOL_EVENT_SEQUENCE_KEY
            | MEMPOOL_EVENT_RETENTION_FLOOR_KEY => {}
            lease_key if is_projection_build_lease_key(lease_key) => {}
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
    if persisted.network != expectation.network {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "persisted network {:?} does not equal requested network {:?}",
                persisted.network, expectation.network
            ),
        ));
    }
    let persisted_activations_fingerprint = persisted
        .build_plan
        .network_upgrade_activations_fingerprint();
    if persisted_activations_fingerprint != expectation.activations_fingerprint {
        return Err(CanonicalStoreError::admission(
            path,
            "persisted network upgrade activations do not equal the requested activation table",
        ));
    }
    if persisted.workload != expectation.workload {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "persisted workload {} does not equal requested workload {}",
                persisted.workload.as_str(),
                expectation.workload.as_str()
            ),
        ));
    }
    if persisted.build_plan.reorg_policy() != expectation.reorg_policy {
        return Err(CanonicalStoreError::admission(
            path,
            format!(
                "persisted reorg window {} does not equal requested reorg window {}",
                persisted.build_plan.reorg_policy().reorg_window_blocks(),
                expectation.reorg_policy.reorg_window_blocks()
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
        CANONICAL_STORE_IDENTITY, CANONICAL_STORE_SCHEMA_VERSION, CanonicalReorgPolicy,
        CanonicalStoreBuildPlan, RocksDbCanonicalBuilder,
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
            CanonicalReorgPolicy::new(100)?,
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
            CanonicalReorgPolicy::new(100)?,
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
            CanonicalReorgPolicy::new(100)?,
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
            CanonicalReorgPolicy::new(100)?,
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
            CanonicalReorgPolicy::new(100)?,
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
            .copy_from_slice(&1_u16.to_le_bytes());
        db.put(STORE_CONTROL_KEY, &control)?;
        drop(db);

        let error = RocksDbCanonicalStore::open_ready(
            &path,
            &crate::canonical_store::test_network_upgrade_activations(Network::ZcashTestnet)?,
            CanonicalStoreWorkload::Explorer,
            CanonicalReorgPolicy::new(100)?,
            RocksDbResourceBudget::for_local_tests(),
        )
        .err()
        .ok_or("another schema version should be rejected")?;
        assert!(
            error
                .to_string()
                .contains("schema version 1 does not equal required version 5"),
            "{error}"
        );
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
            CanonicalReorgPolicy::new(100)?,
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
    fn contract_identity_and_schema_are_exact() {
        assert_eq!(CANONICAL_STORE_IDENTITY, "canonical");
        assert_eq!(CANONICAL_STORE_SCHEMA_VERSION, 5);
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
            CanonicalReorgPolicy::new(100)?,
        )?)
    }
}
