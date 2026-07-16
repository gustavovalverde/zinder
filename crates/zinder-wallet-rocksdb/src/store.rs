//! Exact version-1 `RocksDB` wallet layout, admission, and reads.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    mem::size_of,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
};

use rust_rocksdb::{
    BoundColumnFamily, Cache, ColumnFamilyDescriptor, DBCompressionType,
    DEFAULT_COLUMN_FAMILY_NAME, Direction, FlushOptions, IteratorMode, Options, ReadOptions,
    WriteBatch, WriteOptions,
};
use zinder_core::{BlockHeight, Network, TransparentAddressScriptHash, TransparentOutPoint};
use zinder_store::{
    BoundedRocksDbOpen, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget,
    build_block_based_table_factory, open_bounded_rocksdb,
};
use zinder_wallet_projection::{
    WALLET_PROJECTION_SCHEMA_VERSION, WALLET_STORE_CONTROL_KEY, WalletAddressBalance,
    WalletAddressTransaction, WalletAddressTransactionKey, WalletAddressUnspentOutputKey,
    WalletCanonicalSourceIdentity, WalletOutpointKey, WalletProjectionBuildPlan,
    WalletProjectionBuildState, WalletProjectionDigestBuilder, WalletProjectionReadyEvidence,
    WalletProjectionRowFamily, WalletProjectionSourcePosition, WalletReorgUndo, WalletSpentOutput,
    WalletStoreControl, WalletUnspentOutput, WalletUtxoSetSummary,
};

use crate::{RocksDbWalletError, sort_merge::PreparedWalletProjection};

/// Exact clean wallet-store schema supported by this adapter.
pub const WALLET_ROCKSDB_SCHEMA_VERSION: u16 = WALLET_PROJECTION_SCHEMA_VERSION;

/// One bounded page of current outputs ordered by creation position and outpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletAddressUnspentOutputsPage {
    /// Current outputs in durable version-1 address-index order.
    pub outputs: Vec<WalletUnspentOutput>,
    /// Exclusive continuation key to pass as `after` for the next page.
    pub next_page_after: Option<WalletAddressUnspentOutputKey>,
}

/// One bounded page of address history ordered by block height and transaction index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletAddressTransactionHistoryPage {
    /// Address-touching transactions in durable version-1 history order.
    pub transactions: Vec<WalletAddressTransaction>,
    /// Exclusive continuation key to pass as `after` for the next page.
    pub next_page_after: Option<WalletAddressTransactionKey>,
}

const TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY: &str = "transparent_unspent_output";
const TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY: &str =
    "transparent_unspent_output_by_address";
const TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY: &str = "transparent_spent_output";
const TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY: &str = "transparent_address_transaction";
const TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY: &str = "transparent_address_balance";
const REORG_UNDO_COLUMN_FAMILY: &str = "reorg_undo";

const WALLET_DATA_COLUMN_FAMILIES: [&str; 6] = [
    TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY,
    TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
    TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY,
    REORG_UNDO_COLUMN_FAMILY,
];

/// A fresh BUILDING wallet store that cannot be admitted by query processes.
pub(crate) struct RocksDbWalletBuilder {
    bounded_open: BoundedRocksDbOpen,
    store_path: std::path::PathBuf,
    resource_budget: RocksDbResourceBudget,
    control: WalletStoreControl,
}

/// A cold-reopened BUILDING store whose rows have not yet been validated.
pub(crate) struct ColdRocksDbWalletBuild {
    bounded_open: BoundedRocksDbOpen,
    control: WalletStoreControl,
}

/// A cold-validated BUILDING store carrying the evidence it may publish.
pub(crate) struct ValidatedRocksDbWalletBuild {
    bounded_open: BoundedRocksDbOpen,
    control: WalletStoreControl,
    ready_evidence: WalletProjectionReadyEvidence,
    validation_evidence: WalletColdValidationEvidence,
}

/// Bounded work evidence from independent cold cross-family validation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalletColdValidationEvidence {
    pub(crate) peak_accounted_bytes: u64,
    pub(crate) random_read_count: u64,
}

/// Bounded physical-load evidence reported to the build orchestrator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalletRocksDbLoadEvidence {
    pub(crate) logical_row_bytes: u64,
    pub(crate) write_batch_count: u64,
}

impl WalletRocksDbLoadEvidence {
    fn add(&mut self, family: Self) -> Result<(), RocksDbWalletError> {
        self.logical_row_bytes = self
            .logical_row_bytes
            .checked_add(family.logical_row_bytes)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        self.write_batch_count = self
            .write_batch_count
            .checked_add(family.write_batch_count)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        Ok(())
    }
}

struct BoundedFamilyBatchWriter<'db> {
    bounded_open: &'db BoundedRocksDbOpen,
    family: Arc<BoundColumnFamily<'db>>,
    family_name: &'static str,
    max_batch_bytes: u64,
    current_batch_bytes: u64,
    batch: WriteBatch,
    evidence: WalletRocksDbLoadEvidence,
}

impl<'db> BoundedFamilyBatchWriter<'db> {
    fn new(
        bounded_open: &'db BoundedRocksDbOpen,
        family_name: &'static str,
        max_batch_bytes: u64,
    ) -> Result<Self, RocksDbWalletError> {
        Ok(Self {
            bounded_open,
            family: column_family(bounded_open, family_name)?,
            family_name,
            max_batch_bytes,
            current_batch_bytes: 0,
            batch: WriteBatch::default(),
            evidence: WalletRocksDbLoadEvidence::default(),
        })
    }

    fn put(&mut self, key: &[u8], encoded_value: &[u8]) -> Result<(), RocksDbWalletError> {
        let row_bytes = u64::try_from(key.len())
            .ok()
            .and_then(|key_bytes| {
                u64::try_from(encoded_value.len())
                    .ok()
                    .and_then(|value_bytes| key_bytes.checked_add(value_bytes))
            })
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        if row_bytes > self.max_batch_bytes {
            return Err(RocksDbWalletError::RowExceedsLoadBatchLimit {
                family: self.family_name,
                row_bytes,
                limit_bytes: self.max_batch_bytes,
            });
        }
        if self.current_batch_bytes != 0
            && self
                .current_batch_bytes
                .checked_add(row_bytes)
                .is_none_or(|next_bytes| next_bytes > self.max_batch_bytes)
        {
            self.flush()?;
        }
        self.batch.put_cf(&self.family, key, encoded_value);
        self.current_batch_bytes = self
            .current_batch_bytes
            .checked_add(row_bytes)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        self.evidence.logical_row_bytes = self
            .evidence
            .logical_row_bytes
            .checked_add(row_bytes)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        Ok(())
    }

    fn finish(mut self) -> Result<WalletRocksDbLoadEvidence, RocksDbWalletError> {
        self.flush()?;
        Ok(self.evidence)
    }

    fn flush(&mut self) -> Result<(), RocksDbWalletError> {
        if self.current_batch_bytes == 0 {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let mut write_options = WriteOptions::default();
        write_options.disable_wal(true);
        self.bounded_open
            .db
            .write_opt(&batch, &write_options)
            .map_err(|source| RocksDbWalletError::rocksdb("bounded BUILDING row load", source))?;
        self.current_batch_bytes = 0;
        self.evidence.write_batch_count = self
            .evidence
            .write_batch_count
            .checked_add(1)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        Ok(())
    }
}

/// One admitted READY version-1 wallet `RocksDB` store.
///
/// The private database handle prevents consumers from bypassing the wallet
/// row codecs or mutating a store after admission.
pub struct RocksDbWalletStore {
    bounded_open: BoundedRocksDbOpen,
    control: WalletStoreControl,
    ready_evidence: WalletProjectionReadyEvidence,
}

impl RocksDbWalletBuilder {
    /// Creates a fresh store and durably publishes its BUILDING plan.
    pub(crate) fn create_fresh(
        path: impl AsRef<Path>,
        network: Network,
        target_source_position: WalletProjectionSourcePosition,
        supported_reorg_depth: u32,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, RocksDbWalletError> {
        validate_resource_budget(resource_budget)?;
        let path = path.as_ref();
        create_fresh_directory(path)?;
        let store_path =
            fs::canonicalize(path).map_err(|source| RocksDbWalletError::PathUnavailable {
                path: path.to_path_buf(),
                source,
            })?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary { path: &store_path },
            resource_budget,
            wallet_column_family_descriptors,
        )
        .map_err(|source| RocksDbWalletError::rocksdb("fresh open", source))?;
        let control = WalletStoreControl {
            network,
            supported_reorg_depth,
            writer_generation: 1,
            build_state: WalletProjectionBuildState::Building(
                WalletProjectionBuildPlan::complete_history(target_source_position),
            ),
        };
        write_control_sync(&bounded_open, &control)?;
        Ok(Self {
            bounded_open,
            store_path,
            resource_budget,
            control,
        })
    }

    /// Loads every prepared family in exact schema order using bounded batches.
    ///
    /// Data-row WAL writes are disabled because BUILDING is never queryable and
    /// publication performs a blocking all-family flush followed by a WAL sync.
    pub(crate) fn load_prepared(
        &self,
        prepared: &PreparedWalletProjection,
        max_batch_bytes: u64,
    ) -> Result<WalletRocksDbLoadEvidence, RocksDbWalletError> {
        self.validate_prepared_identity(prepared, max_batch_bytes)?;
        let mut evidence = WalletRocksDbLoadEvidence::default();
        self.load_prepared_rows(prepared, max_batch_bytes, &mut evidence)?;
        Ok(evidence)
    }

    fn validate_prepared_identity(
        &self,
        prepared: &PreparedWalletProjection,
        max_batch_bytes: u64,
    ) -> Result<(), RocksDbWalletError> {
        if max_batch_bytes == 0 {
            return Err(RocksDbWalletError::ZeroLoadBatchLimit);
        }
        let WalletProjectionBuildState::Building(plan) = &self.control.build_state else {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "prepared rows require a BUILDING control record",
            });
        };
        if prepared.network != self.control.network
            || prepared.supported_reorg_depth != self.control.supported_reorg_depth
            || prepared.tip != plan.target_source_position.tip
        {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "prepared projection differs from the BUILDING plan",
            });
        }
        Ok(())
    }

    fn load_prepared_rows(
        &self,
        prepared: &PreparedWalletProjection,
        max_batch_bytes: u64,
        evidence: &mut WalletRocksDbLoadEvidence,
    ) -> Result<(), RocksDbWalletError> {
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for (key, output) in &prepared.unspent_outputs {
                writer.put(key.as_bytes(), &output.encode_value()?)?;
            }
            evidence.add(writer.finish()?)?;
        }
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for key in &prepared.unspent_output_by_address {
                writer.put(key.as_bytes(), &[])?;
            }
            evidence.add(writer.finish()?)?;
        }
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for (key, output) in &prepared.spent_outputs {
                writer.put(key.as_bytes(), &output.encode_value()?)?;
            }
            evidence.add(writer.finish()?)?;
        }
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for transaction in &prepared.address_transactions {
                writer.put(transaction.key.as_bytes(), &(*transaction).encode_value())?;
            }
            evidence.add(writer.finish()?)?;
        }
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for balance in &prepared.address_balances {
                if balance.balance_zat == 0 {
                    return Err(RocksDbWalletError::AdmissionChanged {
                        reason: "version 1 forbids zero-valued address balance rows",
                    });
                }
                writer.put(&balance.encode_key(), &balance.encode_value())?;
            }
            evidence.add(writer.finish()?)?;
        }
        {
            let mut writer = BoundedFamilyBatchWriter::new(
                &self.bounded_open,
                REORG_UNDO_COLUMN_FAMILY,
                max_batch_bytes,
            )?;
            for undo in &prepared.reorg_undo {
                writer.put(&undo.encode_key(), &undo.encode_value()?)?;
            }
            evidence.add(writer.finish()?)?;
        }
        Ok(())
    }

    /// Flushes, closes, and cold-reopens a complete BUILDING store.
    pub(crate) fn reopen_for_validation(
        self,
    ) -> Result<ColdRocksDbWalletBuild, RocksDbWalletError> {
        flush_complete_build(&self.bounded_open)?;
        let database_identity = self
            .bounded_open
            .db
            .get_db_identity()
            .map_err(|source| RocksDbWalletError::rocksdb("database identity read", source))?;
        let store_path = self.store_path.clone();
        let resource_budget = self.resource_budget;
        let expected_control = self.control.clone();
        drop(self);

        require_exact_column_families(&store_path)?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::ExistingPrimary { path: &store_path },
            resource_budget,
            wallet_column_family_descriptors,
        )
        .map_err(|source| RocksDbWalletError::rocksdb("cold publication reopen", source))?;
        let reopened_identity = bounded_open.db.get_db_identity().map_err(|source| {
            RocksDbWalletError::rocksdb("reopened database identity read", source)
        })?;
        if reopened_identity != database_identity {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "database identity changed before cold publication validation",
            });
        }
        let reopened_control = decode_only_control(&bounded_open)?;
        if reopened_control != expected_control {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "BUILDING control changed before cold publication validation",
            });
        }
        Ok(ColdRocksDbWalletBuild {
            bounded_open,
            control: reopened_control,
        })
    }
}

impl ColdRocksDbWalletBuild {
    /// Validates every logical row and binds the only publishable READY evidence.
    pub(crate) fn validate_rows(
        self,
        ready_evidence: WalletProjectionReadyEvidence,
        max_accounted_validation_relation_bytes: u64,
    ) -> Result<ValidatedRocksDbWalletBuild, RocksDbWalletError> {
        let WalletProjectionBuildState::Building(plan) = &self.control.build_state else {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "cold validation requires a BUILDING control record",
            });
        };
        if ready_evidence.source_position != plan.target_source_position {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "READY source position differs from the BUILDING target",
            });
        }
        let validation_evidence = validate_ready_rows(
            &self.bounded_open,
            self.control.network,
            &ready_evidence,
            max_accounted_validation_relation_bytes,
        )?;
        Ok(ValidatedRocksDbWalletBuild {
            bounded_open: self.bounded_open,
            control: self.control,
            ready_evidence,
            validation_evidence,
        })
    }
}

impl ValidatedRocksDbWalletBuild {
    /// Returns the bounded work observed before this typestate was admitted.
    pub(crate) const fn validation_evidence(&self) -> WalletColdValidationEvidence {
        self.validation_evidence
    }

    /// Atomically replaces BUILDING with the evidence established by cold validation.
    pub(crate) fn publish_ready(self) -> Result<RocksDbWalletStore, RocksDbWalletError> {
        let ready_evidence = self.ready_evidence;
        let ready_control = WalletStoreControl {
            network: self.control.network,
            supported_reorg_depth: self.control.supported_reorg_depth,
            writer_generation: self.control.writer_generation,
            build_state: WalletProjectionBuildState::Ready(ready_evidence.clone()),
        };
        write_control_sync(&self.bounded_open, &ready_control)?;
        let persisted_control = decode_only_control(&self.bounded_open)?;
        if persisted_control != ready_control {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "READY control differs after its synchronous publication",
            });
        }
        Ok(RocksDbWalletStore {
            bounded_open: self.bounded_open,
            control: ready_control,
            ready_evidence,
        })
    }
}

impl RocksDbWalletStore {
    /// Opens an existing READY store after exact schema and control admission.
    pub fn open_ready(
        path: impl AsRef<Path>,
        expected_network: Network,
        expected_source: WalletCanonicalSourceIdentity,
        resource_budget: RocksDbResourceBudget,
    ) -> Result<Self, RocksDbWalletError> {
        validate_resource_budget(resource_budget)?;
        let path = path.as_ref();
        let store_path =
            fs::canonicalize(path).map_err(|source| RocksDbWalletError::PathUnavailable {
                path: path.to_path_buf(),
                source,
            })?;
        require_exact_column_families(&store_path)?;
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::ExistingPrimary { path: &store_path },
            resource_budget,
            wallet_column_family_descriptors,
        )
        .map_err(|source| RocksDbWalletError::rocksdb("ready open", source))?;
        require_exact_column_families(&store_path)?;
        let control = decode_only_control(&bounded_open)?;
        if control.network != expected_network {
            return Err(RocksDbWalletError::NetworkMismatch {
                expected: expected_network,
                observed: control.network,
            });
        }
        let WalletProjectionBuildState::Ready(ready_evidence) = &control.build_state else {
            return Err(RocksDbWalletError::StoreNotReady { path: store_path });
        };
        let observed_source = WalletCanonicalSourceIdentity::from_ready_evidence(ready_evidence);
        if observed_source != expected_source {
            return Err(RocksDbWalletError::CanonicalSourceMismatch {
                expected: Box::new(expected_source),
                observed: Box::new(observed_source),
            });
        }
        Ok(Self {
            bounded_open,
            ready_evidence: ready_evidence.clone(),
            control,
        })
    }

    /// Returns the decoded READY evidence that admitted this store.
    #[must_use]
    pub const fn ready_evidence(&self) -> &WalletProjectionReadyEvidence {
        &self.ready_evidence
    }

    /// Returns the store's immutable network.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.control.network
    }

    /// Returns one current output by exact outpoint.
    pub fn find_unspent_output(
        &self,
        outpoint: TransparentOutPoint,
    ) -> Result<Option<WalletUnspentOutput>, RocksDbWalletError> {
        let key = WalletOutpointKey::new(outpoint);
        self.read_optional(
            TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY,
            key.as_bytes(),
            |encoded| WalletUnspentOutput::decode_value(key, encoded),
        )
    }

    /// Returns one historical spent output by exact outpoint.
    pub fn find_spent_output(
        &self,
        outpoint: TransparentOutPoint,
    ) -> Result<Option<WalletSpentOutput>, RocksDbWalletError> {
        let key = WalletOutpointKey::new(outpoint);
        self.read_optional(
            TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY,
            key.as_bytes(),
            |encoded| WalletSpentOutput::decode_value(key, encoded),
        )
    }

    /// Resolves one exact address-ordered unspent-output index key.
    pub fn find_unspent_output_by_address_key(
        &self,
        key: WalletAddressUnspentOutputKey,
    ) -> Result<Option<WalletUnspentOutput>, RocksDbWalletError> {
        let family = column_family(
            &self.bounded_open,
            TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
        )?;
        let Some(encoded_index) = self
            .bounded_open
            .db
            .get_cf(&family, key.as_bytes())
            .map_err(|source| RocksDbWalletError::rocksdb("address unspent index read", source))?
        else {
            return Ok(None);
        };
        if !encoded_index.is_empty() {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address unspent index values must be empty",
            });
        }
        let output = self.find_unspent_output(key.outpoint())?.ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "address unspent index references a missing primary output",
            },
        )?;
        if output.address_script_hash != key.address_script_hash()
            || output.created_at.block.height != key.creation_height()
        {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address unspent index does not match its primary output",
            });
        }
        Ok(Some(output))
    }

    /// Returns one bounded page of current outputs for an address.
    ///
    /// Rows are ordered by creation height and outpoint. `after` is exclusive:
    /// pass the prior page's `next_page_after` unchanged to continue without
    /// repeating or skipping a row. The non-zero 16-bit page size bounds both
    /// work and returned memory to at most 65,535 outputs.
    pub fn address_unspent_outputs_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressUnspentOutputKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressUnspentOutputsPage, RocksDbWalletError> {
        if after.is_some_and(|key| key.address_script_hash() != address_script_hash) {
            return Err(RocksDbWalletError::ContinuationAddressMismatch {
                index: TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
            });
        }
        let family = column_family(
            &self.bounded_open,
            TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
        )?;
        let address_prefix = address_script_hash.as_bytes();
        let start = after
            .as_ref()
            .map_or(address_prefix.as_slice(), |key| key.as_bytes().as_slice());
        let mut outputs = Vec::with_capacity(usize::from(page_size.get()));
        let mut last_key = None;
        for row in self
            .bounded_open
            .db
            .iterator_cf(&family, IteratorMode::From(start, Direction::Forward))
        {
            let (key_bytes, encoded_index) = row.map_err(|source| {
                RocksDbWalletError::rocksdb("address unspent page scan", source)
            })?;
            if !key_bytes.starts_with(&address_prefix) {
                break;
            }
            if after.is_some_and(|key| key_bytes.as_ref() <= key.as_bytes().as_slice()) {
                continue;
            }
            if outputs.len() == usize::from(page_size.get()) {
                return Ok(WalletAddressUnspentOutputsPage {
                    outputs,
                    next_page_after: last_key,
                });
            }
            let key = WalletAddressUnspentOutputKey::decode(&key_bytes)?;
            if !encoded_index.is_empty() {
                return Err(RocksDbWalletError::AdmissionChanged {
                    reason: "address unspent index values must be empty",
                });
            }
            let output = self.find_unspent_output(key.outpoint())?.ok_or(
                RocksDbWalletError::AdmissionChanged {
                    reason: "address unspent index references a missing primary output",
                },
            )?;
            if output.address_script_hash != address_script_hash
                || output.created_at.block.height != key.creation_height()
            {
                return Err(RocksDbWalletError::AdmissionChanged {
                    reason: "address unspent index does not match its primary output",
                });
            }
            outputs.push(output);
            last_key = Some(key);
        }
        Ok(WalletAddressUnspentOutputsPage {
            outputs,
            next_page_after: None,
        })
    }

    /// Returns one exact address-transaction row.
    pub fn find_address_transaction(
        &self,
        key: WalletAddressTransactionKey,
    ) -> Result<Option<WalletAddressTransaction>, RocksDbWalletError> {
        self.read_optional(
            TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
            key.as_bytes(),
            |encoded| WalletAddressTransaction::decode_value(key, encoded),
        )
    }

    /// Returns one bounded page of transaction history for an address.
    ///
    /// Rows are ordered by block height and block-local transaction index.
    /// `after` is exclusive: pass the prior page's `next_page_after` unchanged
    /// to continue without repeating or skipping a row. The non-zero 16-bit
    /// page size bounds both work and returned memory to at most 65,535 rows.
    pub fn address_transaction_history_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressTransactionKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressTransactionHistoryPage, RocksDbWalletError> {
        if after.is_some_and(|key| key.address_script_hash() != address_script_hash) {
            return Err(RocksDbWalletError::ContinuationAddressMismatch {
                index: TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
            });
        }
        let family = column_family(
            &self.bounded_open,
            TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
        )?;
        let address_prefix = address_script_hash.as_bytes();
        let start = after
            .as_ref()
            .map_or(address_prefix.as_slice(), |key| key.as_bytes().as_slice());
        let mut transactions = Vec::with_capacity(usize::from(page_size.get()));
        let mut last_key = None;
        for row in self
            .bounded_open
            .db
            .iterator_cf(&family, IteratorMode::From(start, Direction::Forward))
        {
            let (key_bytes, encoded_transaction) = row.map_err(|source| {
                RocksDbWalletError::rocksdb("address transaction history page scan", source)
            })?;
            if !key_bytes.starts_with(&address_prefix) {
                break;
            }
            if after.is_some_and(|key| key_bytes.as_ref() <= key.as_bytes().as_slice()) {
                continue;
            }
            if transactions.len() == usize::from(page_size.get()) {
                return Ok(WalletAddressTransactionHistoryPage {
                    transactions,
                    next_page_after: last_key,
                });
            }
            let key = WalletAddressTransactionKey::decode(&key_bytes)?;
            transactions.push(WalletAddressTransaction::decode_value(
                key,
                &encoded_transaction,
            )?);
            last_key = Some(key);
        }
        Ok(WalletAddressTransactionHistoryPage {
            transactions,
            next_page_after: None,
        })
    }

    /// Returns one exact retained reorg-undo record by block height.
    pub fn find_reorg_undo(
        &self,
        block_height: BlockHeight,
    ) -> Result<Option<WalletReorgUndo>, RocksDbWalletError> {
        let key = block_height.value().to_be_bytes();
        self.read_optional(REORG_UNDO_COLUMN_FAMILY, &key, |encoded| {
            WalletReorgUndo::decode(&key, encoded)
        })
    }

    /// Returns one address's current balance, with an absent row represented as zero.
    pub fn address_balance(
        &self,
        address_script_hash: TransparentAddressScriptHash,
    ) -> Result<u64, RocksDbWalletError> {
        let key = address_script_hash.as_bytes();
        Ok(self
            .read_optional(TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY, &key, |encoded| {
                WalletAddressBalance::decode(&key, encoded)
            })?
            .map_or(0, |balance| balance.balance_zat))
    }

    /// Returns the complete current UTXO aggregate committed by READY.
    #[must_use]
    pub const fn utxo_summary(&self) -> &WalletUtxoSetSummary {
        &self.ready_evidence.utxo_summary
    }

    /// Returns the bounded store I/O mode selected at open.
    #[must_use]
    pub const fn io_mode(&self) -> RocksDbIoMode {
        self.bounded_open.io_mode
    }

    fn read_optional<Row>(
        &self,
        family_name: &'static str,
        key: &[u8],
        decode: impl FnOnce(
            &[u8],
        )
            -> Result<Row, zinder_wallet_projection::WalletProjectionContractError>,
    ) -> Result<Option<Row>, RocksDbWalletError> {
        let family = column_family(&self.bounded_open, family_name)?;
        self.bounded_open
            .db
            .get_cf(&family, key)
            .map_err(|source| RocksDbWalletError::rocksdb("query read", source))?
            .map(|encoded| decode(&encoded).map_err(RocksDbWalletError::from))
            .transpose()
    }
}

fn wallet_column_family_descriptors(
    block_cache: &Cache,
    resource_budget: RocksDbResourceBudget,
) -> Vec<ColumnFamilyDescriptor> {
    WALLET_DATA_COLUMN_FAMILIES
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
) -> Result<(), RocksDbWalletError> {
    resource_budget
        .validate()
        .map_err(|reason| RocksDbWalletError::InvalidResourceBudget { reason })
}

fn create_fresh_directory(path: &Path) -> Result<(), RocksDbWalletError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| RocksDbWalletError::PathUnavailable {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(RocksDbWalletError::PathNotFresh {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(RocksDbWalletError::PathUnavailable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn required_column_family_names() -> Vec<String> {
    std::iter::once(DEFAULT_COLUMN_FAMILY_NAME)
        .chain(WALLET_DATA_COLUMN_FAMILIES)
        .map(str::to_owned)
        .collect()
}

fn require_exact_column_families(path: &Path) -> Result<(), RocksDbWalletError> {
    let mut expected = required_column_family_names();
    let mut observed = rust_rocksdb::DB::list_cf(&Options::default(), path)
        .map_err(|source| RocksDbWalletError::rocksdb("column-family admission", source))?;
    expected.sort_unstable();
    observed.sort_unstable();
    if expected != observed {
        return Err(RocksDbWalletError::ColumnFamilyContractMismatch { expected, observed });
    }
    Ok(())
}

fn decode_only_control(
    bounded_open: &BoundedRocksDbOpen,
) -> Result<WalletStoreControl, RocksDbWalletError> {
    let mut iterator = bounded_open.db.iterator(IteratorMode::Start);
    let Some(first) = iterator.next() else {
        return Err(RocksDbWalletError::StoreControlMissing);
    };
    let (key, encoded) =
        first.map_err(|source| RocksDbWalletError::rocksdb("control scan", source))?;
    if key.as_ref() != WALLET_STORE_CONTROL_KEY || iterator.next().is_some() {
        return Err(RocksDbWalletError::StoreControlCardinalityMismatch);
    }
    WalletStoreControl::decode(&encoded).map_err(RocksDbWalletError::from)
}

fn write_control_sync(
    bounded_open: &BoundedRocksDbOpen,
    control: &WalletStoreControl,
) -> Result<(), RocksDbWalletError> {
    let encoded = control.encode()?;
    let mut batch = WriteBatch::default();
    batch.put(WALLET_STORE_CONTROL_KEY, encoded);
    let mut write_options = WriteOptions::default();
    write_options.set_sync(true);
    bounded_open
        .db
        .write_opt(&batch, &write_options)
        .map_err(|source| RocksDbWalletError::rocksdb("control publication", source))
}

fn flush_complete_build(bounded_open: &BoundedRocksDbOpen) -> Result<(), RocksDbWalletError> {
    let mut families = Vec::with_capacity(WALLET_DATA_COLUMN_FAMILIES.len() + 1);
    for family_name in
        std::iter::once(DEFAULT_COLUMN_FAMILY_NAME).chain(WALLET_DATA_COLUMN_FAMILIES)
    {
        families.push(column_family(bounded_open, family_name)?);
    }
    let family_refs = families.iter().collect::<Vec<_>>();
    let mut options = FlushOptions::default();
    options.set_wait(true);
    bounded_open
        .db
        .flush_cfs_opt(&family_refs, &options)
        .map_err(|source| RocksDbWalletError::rocksdb("publication column-family flush", source))?;
    bounded_open
        .db
        .flush_wal(true)
        .map_err(|source| RocksDbWalletError::rocksdb("publication WAL sync", source))
}

#[derive(Debug, Default)]
struct ExpectedUndoEffects {
    block: Option<zinder_core::BlockId>,
    created_outpoints: BTreeSet<WalletOutpointKey>,
    spent_outpoints: BTreeSet<WalletOutpointKey>,
    address_transaction_keys: BTreeSet<WalletAddressTransactionKey>,
}

#[derive(Debug)]
struct AccountedValidationRelationMemory {
    limit: u64,
    current: u64,
    peak: u64,
}

impl AccountedValidationRelationMemory {
    const fn new(limit: u64) -> Self {
        Self {
            limit,
            current: 0,
            peak: 0,
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), RocksDbWalletError> {
        let bytes = u64::try_from(bytes).map_err(|_| RocksDbWalletError::LoadAccountingOverflow)?;
        let required_bytes = self
            .current
            .checked_add(bytes)
            .ok_or(RocksDbWalletError::LoadAccountingOverflow)?;
        if required_bytes > self.limit {
            return Err(RocksDbWalletError::AccountedValidationRelationMemoryLimit {
                limit_bytes: self.limit,
                required_bytes,
            });
        }
        self.current = required_bytes;
        self.peak = self.peak.max(required_bytes);
        Ok(())
    }
}

#[allow(
    clippy::set_contains_or_insert,
    reason = "the retained-key budget must be admitted only for absent keys and before insertion"
)]
fn insert_accounted_relation_key<Key: Ord>(
    keys: &mut BTreeSet<Key>,
    key: Key,
    memory: &mut AccountedValidationRelationMemory,
) -> Result<(), RocksDbWalletError> {
    if keys.contains(&key) {
        return Ok(());
    }
    memory.reserve(size_of::<Key>())?;
    if !keys.insert(key) {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "validation relationship key changed during single-threaded admission",
        });
    }
    Ok(())
}

#[derive(Debug)]
struct ExpectedWalletRelations {
    address_transactions: BTreeMap<WalletAddressTransactionKey, WalletAddressTransaction>,
    undo_by_height: BTreeMap<u32, ExpectedUndoEffects>,
    memory: AccountedValidationRelationMemory,
    random_read_count: u64,
}

impl ExpectedWalletRelations {
    fn new(
        ready_tip: zinder_core::BlockId,
        undo_count: u64,
        max_accounted_bytes: u64,
    ) -> Result<Self, RocksDbWalletError> {
        let mut relations = Self {
            address_transactions: BTreeMap::new(),
            undo_by_height: BTreeMap::new(),
            memory: AccountedValidationRelationMemory::new(max_accounted_bytes),
            random_read_count: 0,
        };
        let first_height = u64::from(ready_tip.height.value())
            .checked_sub(undo_count)
            .and_then(|height| height.checked_add(1))
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "reorg undo suffix falls outside the READY source range",
            })?;
        for offset in 0..undo_count {
            let height =
                first_height
                    .checked_add(offset)
                    .ok_or(RocksDbWalletError::AdmissionChanged {
                        reason: "reorg undo suffix height overflow",
                    })?;
            let height =
                u32::try_from(height).map_err(|_| RocksDbWalletError::AdmissionChanged {
                    reason: "reorg undo suffix height exceeds u32::MAX",
                })?;
            relations
                .memory
                .reserve(size_of::<(u32, ExpectedUndoEffects)>())?;
            relations
                .undo_by_height
                .insert(height, ExpectedUndoEffects::default());
        }
        Ok(relations)
    }

    fn observe_output(&mut self, output: &WalletUnspentOutput) -> Result<(), RocksDbWalletError> {
        self.observe_address_transaction(output.address_script_hash, output.created_at)?;
        self.observe_undo_created(output)
    }

    fn observe_spent_output(
        &mut self,
        spent: &WalletSpentOutput,
    ) -> Result<(), RocksDbWalletError> {
        validate_spent_position(spent)?;
        self.observe_output(&spent.output)?;
        self.observe_address_transaction(spent.output.address_script_hash, spent.spent_at)?;
        if spent.output.created_at.block != spent.spent_at.block {
            self.observe_undo_spent(spent)?;
        }
        Ok(())
    }

    fn observe_address_transaction(
        &mut self,
        address_script_hash: TransparentAddressScriptHash,
        position: zinder_wallet_projection::WalletTransactionPosition,
    ) -> Result<(), RocksDbWalletError> {
        let key = WalletAddressTransactionKey::new(
            address_script_hash,
            position.block.height,
            position.tx_index_in_block,
        );
        let expected =
            WalletAddressTransaction::new(key, position.transaction_id, position.block.hash);
        match self.address_transactions.entry(key) {
            Entry::Vacant(entry) => {
                self.memory.reserve(size_of::<(
                    WalletAddressTransactionKey,
                    WalletAddressTransaction,
                )>())?;
                entry.insert(expected);
            }
            Entry::Occupied(entry) if entry.get() != &expected => {
                return Err(RocksDbWalletError::AdmissionChanged {
                    reason: "one address transaction key resolves to different transactions",
                });
            }
            Entry::Occupied(_) => {}
        }
        if let Some(undo) = self.undo_by_height.get_mut(&position.block.height.value()) {
            remember_undo_block(undo, position.block)?;
            insert_accounted_relation_key(
                &mut undo.address_transaction_keys,
                key,
                &mut self.memory,
            )?;
        }
        Ok(())
    }

    fn observe_undo_created(
        &mut self,
        output: &WalletUnspentOutput,
    ) -> Result<(), RocksDbWalletError> {
        let Some(undo) = self
            .undo_by_height
            .get_mut(&output.created_at.block.height.value())
        else {
            return Ok(());
        };
        remember_undo_block(undo, output.created_at.block)?;
        let key = WalletOutpointKey::new(output.outpoint);
        insert_accounted_relation_key(&mut undo.created_outpoints, key, &mut self.memory)?;
        Ok(())
    }

    fn observe_undo_spent(&mut self, spent: &WalletSpentOutput) -> Result<(), RocksDbWalletError> {
        let Some(undo) = self
            .undo_by_height
            .get_mut(&spent.spent_at.block.height.value())
        else {
            return Ok(());
        };
        remember_undo_block(undo, spent.spent_at.block)?;
        let key = WalletOutpointKey::new(spent.output.outpoint);
        insert_accounted_relation_key(&mut undo.spent_outpoints, key, &mut self.memory)?;
        Ok(())
    }

    fn record_random_read(&mut self) -> Result<(), RocksDbWalletError> {
        self.random_read_count =
            self.random_read_count
                .checked_add(1)
                .ok_or(RocksDbWalletError::AdmissionChanged {
                    reason: "cold validation random-read count overflow",
                })?;
        Ok(())
    }

    const fn evidence(&self) -> WalletColdValidationEvidence {
        WalletColdValidationEvidence {
            peak_accounted_bytes: self.memory.peak,
            random_read_count: self.random_read_count,
        }
    }
}

fn validate_spent_position(spent: &WalletSpentOutput) -> Result<(), RocksDbWalletError> {
    let created = spent.output.created_at;
    let consumed = spent.spent_at;
    if created.block.height.value() > consumed.block.height.value()
        || (created.block.height == consumed.block.height
            && (created.block != consumed.block
                || created.tx_index_in_block >= consumed.tx_index_in_block))
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "spent output does not follow its exact canonical creation position",
        });
    }
    Ok(())
}

fn remember_undo_block(
    undo: &mut ExpectedUndoEffects,
    block: zinder_core::BlockId,
) -> Result<(), RocksDbWalletError> {
    if undo.block.is_some_and(|observed| observed != block) {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "wallet effects disagree on their canonical block identity",
        });
    }
    undo.block = Some(block);
    Ok(())
}

fn validate_ready_rows(
    bounded_open: &BoundedRocksDbOpen,
    network: Network,
    evidence: &WalletProjectionReadyEvidence,
    max_accounted_validation_relation_bytes: u64,
) -> Result<WalletColdValidationEvidence, RocksDbWalletError> {
    let counts = evidence.row_counts;
    if counts.transparent_unspent_output_count != counts.transparent_unspent_output_by_address_count
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index does not exactly cover every primary unspent output",
        });
    }
    let mut relations = ExpectedWalletRelations::new(
        evidence.source_position.tip,
        counts.reorg_undo_count,
        max_accounted_validation_relation_bytes,
    )?;
    let mut digest = WalletProjectionDigestBuilder::new();
    let (utxo_count, total_value_zat, commitment) = validate_unspent_rows(
        bounded_open,
        network,
        &mut digest,
        counts.transparent_unspent_output_count,
        &mut relations,
    )?;
    let expected_balances = validate_address_unspent_rows(
        bounded_open,
        &mut digest,
        counts.transparent_unspent_output_by_address_count,
        &mut relations,
    )?;
    validate_spent_rows(
        bounded_open,
        &mut digest,
        counts.transparent_spent_output_count,
        &mut relations,
    )?;
    validate_address_transaction_rows(
        bounded_open,
        &mut digest,
        counts.transparent_address_transaction_count,
        &relations.address_transactions,
    )?;
    validate_address_balance_rows(
        bounded_open,
        &mut digest,
        counts.transparent_address_balance_count,
        &expected_balances,
    )?;
    validate_reorg_undo_rows(
        bounded_open,
        &mut digest,
        counts.reorg_undo_count,
        evidence.source_position.tip,
        &relations.undo_by_height,
    )?;
    let observed_row_counts = digest.row_counts();
    let observed_digest = digest.finish();
    if observed_row_counts != evidence.row_counts
        || observed_digest != evidence.projection_digest
        || (WalletUtxoSetSummary {
            utxo_count,
            total_value_zat,
            commitment,
        }) != evidence.utxo_summary
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "READY evidence differs from cold wallet rows",
        });
    }
    Ok(relations.evidence())
}

fn validate_unspent_rows(
    bounded_open: &BoundedRocksDbOpen,
    network: Network,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    relations: &mut ExpectedWalletRelations,
) -> Result<(u64, u64, zinder_core::TransparentUtxoSetCommitment), RocksDbWalletError> {
    use zinder_core::wire::UtxoSetCommitmentElement;

    let family = column_family(bounded_open, TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY)?;
    let mut count = 0u64;
    let mut total_value_zat = 0u64;
    let mut commitment = zinder_core::TransparentUtxoSetCommitment::empty();
    for row in
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start)
    {
        let (key_bytes, value_bytes) =
            row.map_err(|source| RocksDbWalletError::rocksdb("unspent validation scan", source))?;
        let key = WalletOutpointKey::decode(&key_bytes)?;
        let output = WalletUnspentOutput::decode_value(key, &value_bytes)?;
        relations.observe_output(&output)?;
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutput,
            &key_bytes,
            &value_bytes,
        )?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "unspent row count overflow",
            })?;
        total_value_zat = total_value_zat.checked_add(output.value_zat).ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "unspent value total overflow",
            },
        )?;
        commitment.insert(&UtxoSetCommitmentElement {
            network_id: network.id(),
            outpoint: output.outpoint,
            value_zat: output.value_zat,
            script_pub_key: &output.script_pub_key,
            block_height: output.created_at.block.height,
        });
    }
    if count != expected_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "unspent row count differs from READY evidence",
        });
    }
    Ok((count, total_value_zat, commitment))
}

fn validate_address_unspent_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    relations: &mut ExpectedWalletRelations,
) -> Result<Vec<WalletAddressBalance>, RocksDbWalletError> {
    let address_family = column_family(
        bounded_open,
        TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
    )?;
    let unspent_family = column_family(bounded_open, TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY)?;
    let mut count = 0u64;
    let mut current_address = None;
    let mut current_balance_zat = 0u64;
    let mut expected_balances = Vec::new();
    for row in bounded_open.db.iterator_cf_opt(
        &address_family,
        validation_read_options(),
        IteratorMode::Start,
    ) {
        let (key_bytes, value_bytes) = row.map_err(|source| {
            RocksDbWalletError::rocksdb("address unspent validation scan", source)
        })?;
        let output = resolve_indexed_unspent_output(
            bounded_open,
            &unspent_family,
            &key_bytes,
            &value_bytes,
            relations,
        )?;
        let address = output.address_script_hash;
        if current_address.is_some_and(|current| current != address) {
            append_expected_balance(
                &mut expected_balances,
                current_address.ok_or(RocksDbWalletError::AdmissionChanged {
                    reason: "address unspent validation group disappeared",
                })?,
                current_balance_zat,
                &mut relations.memory,
            )?;
            current_balance_zat = 0;
        }
        current_address = Some(address);
        current_balance_zat = current_balance_zat.checked_add(output.value_zat).ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "address unspent value total overflow",
            },
        )?;
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            &key_bytes,
            &value_bytes,
        )?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "address unspent row count overflow",
            })?;
    }
    if count != expected_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent row count differs from READY evidence",
        });
    }
    if let Some(address) = current_address {
        append_expected_balance(
            &mut expected_balances,
            address,
            current_balance_zat,
            &mut relations.memory,
        )?;
    }
    Ok(expected_balances)
}

fn resolve_indexed_unspent_output(
    bounded_open: &BoundedRocksDbOpen,
    unspent_family: &Arc<BoundColumnFamily<'_>>,
    key_bytes: &[u8],
    value_bytes: &[u8],
    relations: &mut ExpectedWalletRelations,
) -> Result<WalletUnspentOutput, RocksDbWalletError> {
    let address_key = WalletAddressUnspentOutputKey::decode(key_bytes)?;
    if !value_bytes.is_empty() {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index values must be empty",
        });
    }
    relations.record_random_read()?;
    let outpoint_key = WalletOutpointKey::new(address_key.outpoint());
    let encoded_output = bounded_open
        .db
        .get_cf_opt(
            unspent_family,
            outpoint_key.as_bytes(),
            &validation_read_options(),
        )
        .map_err(|source| {
            RocksDbWalletError::rocksdb("address unspent primary validation lookup", source)
        })?
        .ok_or(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index references a missing primary output",
        })?;
    let output = WalletUnspentOutput::decode_value(outpoint_key, &encoded_output)?;
    if WalletAddressUnspentOutputKey::new(&output) != address_key {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index does not match its primary output",
        });
    }
    Ok(output)
}

fn append_expected_balance(
    balances: &mut Vec<WalletAddressBalance>,
    address_script_hash: TransparentAddressScriptHash,
    balance_zat: u64,
    memory: &mut AccountedValidationRelationMemory,
) -> Result<(), RocksDbWalletError> {
    if balance_zat == 0 {
        return Ok(());
    }
    memory.reserve(size_of::<WalletAddressBalance>())?;
    balances.push(WalletAddressBalance {
        address_script_hash,
        balance_zat,
    });
    Ok(())
}

fn validate_spent_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    relations: &mut ExpectedWalletRelations,
) -> Result<(), RocksDbWalletError> {
    let family = column_family(bounded_open, TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY)?;
    let unspent_family = column_family(bounded_open, TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY)?;
    let mut count = 0u64;
    for row in
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start)
    {
        let (key_bytes, encoded_value) =
            row.map_err(|source| RocksDbWalletError::rocksdb("spent validation scan", source))?;
        let spent = WalletSpentOutput::decode_value(
            WalletOutpointKey::decode(&key_bytes)?,
            &encoded_value,
        )?;
        relations.record_random_read()?;
        if bounded_open
            .db
            .get_cf_opt(&unspent_family, &key_bytes, &validation_read_options())
            .map_err(|source| {
                RocksDbWalletError::rocksdb("spent/unspent disjointness validation lookup", source)
            })?
            .is_some()
        {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "one outpoint appears in both unspent and spent output families",
            });
        }
        relations.observe_spent_output(&spent)?;
        digest.append_row(
            WalletProjectionRowFamily::TransparentSpentOutput,
            &key_bytes,
            &encoded_value,
        )?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "spent row count overflow",
            })?;
    }
    if count != expected_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "spent row count differs from READY evidence",
        });
    }
    Ok(())
}

fn validate_address_transaction_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    expected_transactions: &BTreeMap<WalletAddressTransactionKey, WalletAddressTransaction>,
) -> Result<(), RocksDbWalletError> {
    let family = column_family(bounded_open, TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY)?;
    let mut expected = expected_transactions.iter();
    let mut count = 0u64;
    for row in
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start)
    {
        let (key_bytes, encoded_value) = row.map_err(|source| {
            RocksDbWalletError::rocksdb("address transaction validation scan", source)
        })?;
        let key = WalletAddressTransactionKey::decode(&key_bytes)?;
        let transaction = WalletAddressTransaction::decode_value(key, &encoded_value)?;
        let Some((expected_key, expected_transaction)) = expected.next() else {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address transaction family contains an unexpected row",
            });
        };
        if expected_key != &key || expected_transaction != &transaction {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address transaction rows differ from output create/spend effects",
            });
        }
        digest.append_row(
            WalletProjectionRowFamily::TransparentAddressTransaction,
            &key_bytes,
            &encoded_value,
        )?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "address transaction row count overflow",
            })?;
    }
    if expected.next().is_some() || count != expected_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address transaction rows do not exactly cover output create/spend effects",
        });
    }
    Ok(())
}

fn validate_address_balance_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    expected_balances: &[WalletAddressBalance],
) -> Result<(), RocksDbWalletError> {
    let family = column_family(bounded_open, TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY)?;
    let mut expected = expected_balances.iter();
    let mut count = 0u64;
    for row in
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start)
    {
        let (key, encoded_value) = row.map_err(|source| {
            RocksDbWalletError::rocksdb("address balance validation scan", source)
        })?;
        let balance = WalletAddressBalance::decode(&key, &encoded_value)?;
        let Some(expected_balance) = expected.next() else {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address balance family contains an unexpected row",
            });
        };
        if expected_balance != &balance {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address balance rows differ from indexed unspent-output sums",
            });
        }
        digest.append_row(
            WalletProjectionRowFamily::TransparentAddressBalance,
            &key,
            &encoded_value,
        )?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "address balance row count overflow",
            })?;
    }
    if expected.next().is_some() || count != expected_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address balance rows do not exactly cover positive indexed addresses",
        });
    }
    Ok(())
}

fn validate_reorg_undo_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    ready_tip: zinder_core::BlockId,
    expected_by_height: &BTreeMap<u32, ExpectedUndoEffects>,
) -> Result<(), RocksDbWalletError> {
    let family = column_family(bounded_open, REORG_UNDO_COLUMN_FAMILY)?;
    let first_height = u64::from(ready_tip.height.value())
        .checked_sub(expected_count)
        .and_then(|height| height.checked_add(1))
        .ok_or(RocksDbWalletError::AdmissionChanged {
            reason: "reorg undo suffix falls outside the READY source range",
        })?;
    let mut count = 0u64;
    let mut last_undo = None;
    for row in
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start)
    {
        let (key, encoded_value) = row
            .map_err(|source| RocksDbWalletError::rocksdb("reorg undo validation scan", source))?;
        let undo = WalletReorgUndo::decode(&key, &encoded_value)?;
        let expected_height =
            first_height
                .checked_add(count)
                .ok_or(RocksDbWalletError::AdmissionChanged {
                    reason: "reorg undo suffix height overflow",
                })?;
        if u64::from(undo.block.height.value()) != expected_height {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "reorg undo rows are not the exact contiguous READY suffix",
            });
        }
        let expected = expected_by_height.get(&undo.block.height.value()).ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "reorg undo row falls outside the reconstructed suffix",
            },
        )?;
        if expected.block.is_some_and(|block| block != undo.block)
            || !undo
                .created_outpoints
                .iter()
                .copied()
                .eq(expected.created_outpoints.iter().copied())
            || !undo
                .spent_outpoints
                .iter()
                .copied()
                .eq(expected.spent_outpoints.iter().copied())
            || !undo
                .address_transaction_keys
                .iter()
                .copied()
                .eq(expected.address_transaction_keys.iter().copied())
        {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "reorg undo row differs from reconstructed wallet block effects",
            });
        }
        digest.append_row(WalletProjectionRowFamily::ReorgUndo, &key, &encoded_value)?;
        count = count
            .checked_add(1)
            .ok_or(RocksDbWalletError::AdmissionChanged {
                reason: "reorg undo row count overflow",
            })?;
        last_undo = Some(undo);
    }
    if count != expected_count
        || u64::try_from(expected_by_height.len()).ok() != Some(expected_count)
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "reorg undo row count differs from READY evidence",
        });
    }
    if let Some(last_undo) = last_undo
        && last_undo.block != ready_tip
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "tip reorg undo does not match the READY source tip",
        });
    }
    Ok(())
}

fn column_family<'db>(
    bounded_open: &'db BoundedRocksDbOpen,
    name: &'static str,
) -> Result<Arc<BoundColumnFamily<'db>>, RocksDbWalletError> {
    bounded_open
        .db
        .cf_handle(name)
        .ok_or(RocksDbWalletError::ColumnFamilyUnavailable { name })
}

fn validation_read_options() -> ReadOptions {
    let mut options = ReadOptions::default();
    options.fill_cache(false);
    options.set_readahead_size(2 * 1024 * 1024);
    options
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zinder_core::wire::UtxoSetCommitmentElement;
    use zinder_core::{
        BlockHash, BlockHeight, BlockId, CanonicalBlockFactsSequenceDigest,
        CanonicalBlockFactsSequenceDigestVersion, ChainEpochId, TransactionId,
        TransparentUtxoSetCommitment,
    };
    use zinder_wallet_projection::{
        WalletProjectionDigest, WalletProjectionFamilyRowCounts, WalletTransactionPosition,
    };

    use super::*;

    const TEST_VALIDATION_MEMORY_LIMIT: u64 = 16 * 1024 * 1024;

    fn source_position() -> WalletProjectionSourcePosition {
        WalletProjectionSourcePosition::new(
            ChainEpochId::new(1),
            BlockId::new(BlockHeight::new(1), BlockHash::from_bytes([0x11; 32])),
            1,
        )
    }

    fn source_identity() -> WalletCanonicalSourceIdentity {
        WalletCanonicalSourceIdentity::new(
            source_position(),
            CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                CanonicalBlockFactsSequenceDigestVersion::V1,
                1,
                [0x77; 32],
            ),
        )
    }

    fn sample_output(
        transaction_byte: u8,
        value_zat: u64,
    ) -> Result<WalletUnspentOutput, zinder_wallet_projection::WalletProjectionContractError> {
        let transaction_id = TransactionId::from_bytes([transaction_byte; 32]);
        WalletUnspentOutput::new(
            TransparentOutPoint::new(transaction_id, 0),
            TransparentAddressScriptHash::from_bytes([transaction_byte; 32]),
            value_zat,
            [0x51],
            WalletTransactionPosition::new(transaction_id, 0, source_position().tip),
        )
    }

    fn expected_address_transactions(
        unspent: &WalletUnspentOutput,
        spent: Option<&WalletSpentOutput>,
    ) -> Vec<WalletAddressTransaction> {
        let mut positions = vec![(unspent.address_script_hash, unspent.created_at)];
        if let Some(spent) = spent {
            positions.push((spent.output.address_script_hash, spent.output.created_at));
            positions.push((spent.output.address_script_hash, spent.spent_at));
        }
        let mut transactions = positions
            .into_iter()
            .map(|(address_script_hash, position)| {
                let key = WalletAddressTransactionKey::new(
                    address_script_hash,
                    position.block.height,
                    position.tx_index_in_block,
                );
                WalletAddressTransaction::new(key, position.transaction_id, position.block.hash)
            })
            .collect::<Vec<_>>();
        transactions.sort_unstable_by_key(|transaction| transaction.key);
        transactions.dedup_by_key(|transaction| transaction.key);
        transactions
    }

    fn projection_digest(
        unspent: &WalletUnspentOutput,
        spent: &WalletSpentOutput,
        balance: WalletAddressBalance,
    ) -> Result<WalletProjectionDigest, zinder_wallet_projection::WalletProjectionContractError>
    {
        let mut digest = WalletProjectionDigestBuilder::new();
        let unspent_key = WalletOutpointKey::new(unspent.outpoint);
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutput,
            unspent_key.as_bytes(),
            &unspent.encode_value()?,
        )?;
        let address_key = WalletAddressUnspentOutputKey::new(unspent);
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            address_key.as_bytes(),
            &[],
        )?;
        let spent_key = WalletOutpointKey::new(spent.output.outpoint);
        digest.append_row(
            WalletProjectionRowFamily::TransparentSpentOutput,
            spent_key.as_bytes(),
            &spent.encode_value()?,
        )?;
        let address_transactions = expected_address_transactions(unspent, Some(spent));
        for transaction in address_transactions {
            digest.append_row(
                WalletProjectionRowFamily::TransparentAddressTransaction,
                transaction.key.as_bytes(),
                &transaction.encode_value(),
            )?;
        }
        digest.append_row(
            WalletProjectionRowFamily::TransparentAddressBalance,
            &balance.encode_key(),
            &balance.encode_value(),
        )?;
        Ok(digest.finish())
    }

    fn ready_evidence(
        network: Network,
        unspent: &WalletUnspentOutput,
        spent: &WalletSpentOutput,
        balance: WalletAddressBalance,
    ) -> Result<
        WalletProjectionReadyEvidence,
        zinder_wallet_projection::WalletProjectionContractError,
    > {
        let mut commitment = TransparentUtxoSetCommitment::empty();
        commitment.insert(&UtxoSetCommitmentElement {
            network_id: network.id(),
            outpoint: unspent.outpoint,
            value_zat: unspent.value_zat,
            script_pub_key: &unspent.script_pub_key,
            block_height: unspent.created_at.block.height,
        });
        Ok(WalletProjectionReadyEvidence {
            source_position: source_position(),
            source_sequence_digest:
                CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                    CanonicalBlockFactsSequenceDigestVersion::V1,
                    1,
                    [0x77; 32],
                ),
            projection_digest: projection_digest(unspent, spent, balance)?,
            row_counts: WalletProjectionFamilyRowCounts {
                transparent_unspent_output_count: 1,
                transparent_unspent_output_by_address_count: 1,
                transparent_spent_output_count: 1,
                transparent_address_transaction_count: 3,
                transparent_address_balance_count: 1,
                reorg_undo_count: 0,
            },
            utxo_summary: WalletUtxoSetSummary {
                utxo_count: 1,
                total_value_zat: unspent.value_zat,
                commitment,
            },
        })
    }

    fn prepared_projection(
        network: Network,
        unspent: &WalletUnspentOutput,
        spent: &WalletSpentOutput,
        balance: WalletAddressBalance,
    ) -> Result<PreparedWalletProjection, zinder_wallet_projection::WalletProjectionContractError>
    {
        let evidence = ready_evidence(network, unspent, spent, balance)?;
        Ok(PreparedWalletProjection {
            network,
            supported_reorg_depth: 0,
            first_block: source_position().tip,
            tip: source_position().tip,
            source_sequence_digest: evidence.source_sequence_digest,
            unspent_outputs: vec![(WalletOutpointKey::new(unspent.outpoint), unspent.clone())],
            unspent_output_by_address: vec![WalletAddressUnspentOutputKey::new(unspent)],
            spent_outputs: vec![(WalletOutpointKey::new(spent.output.outpoint), spent.clone())],
            address_transactions: expected_address_transactions(unspent, Some(spent)),
            address_balances: vec![balance],
            reorg_undo: Vec::new(),
            row_counts: evidence.row_counts,
            utxo_summary: evidence.utxo_summary,
            projection_digest: evidence.projection_digest,
            counters: crate::sort_merge::WalletSortMergeCounters {
                scanned_block_count: 1,
                scanned_transaction_count: 2,
                staged_output_count: 2,
                staged_spend_count: 1,
                historical_prevout_read_count: 0,
                peak_accounted_bytes: 0,
                max_accounted_bytes: 0,
            },
            phase_durations: crate::sort_merge::WalletSortMergePhaseDurations::default(),
        })
    }

    fn zero_value_ready_evidence(
        network: Network,
        unspent: &WalletUnspentOutput,
    ) -> Result<
        WalletProjectionReadyEvidence,
        zinder_wallet_projection::WalletProjectionContractError,
    > {
        let unspent_key = WalletOutpointKey::new(unspent.outpoint);
        let address_key = WalletAddressUnspentOutputKey::new(unspent);
        let mut digest = WalletProjectionDigestBuilder::new();
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutput,
            unspent_key.as_bytes(),
            &unspent.encode_value()?,
        )?;
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            address_key.as_bytes(),
            &[],
        )?;
        let address_transactions = expected_address_transactions(unspent, None);
        for transaction in address_transactions {
            digest.append_row(
                WalletProjectionRowFamily::TransparentAddressTransaction,
                transaction.key.as_bytes(),
                &transaction.encode_value(),
            )?;
        }

        let mut commitment = TransparentUtxoSetCommitment::empty();
        commitment.insert(&UtxoSetCommitmentElement {
            network_id: network.id(),
            outpoint: unspent.outpoint,
            value_zat: 0,
            script_pub_key: &unspent.script_pub_key,
            block_height: unspent.created_at.block.height,
        });
        Ok(WalletProjectionReadyEvidence {
            source_position: source_position(),
            source_sequence_digest: source_identity().source_sequence_digest(),
            projection_digest: digest.finish(),
            row_counts: WalletProjectionFamilyRowCounts {
                transparent_unspent_output_count: 1,
                transparent_unspent_output_by_address_count: 1,
                transparent_spent_output_count: 0,
                transparent_address_transaction_count: 1,
                transparent_address_balance_count: 0,
                reorg_undo_count: 0,
            },
            utxo_summary: WalletUtxoSetSummary {
                utxo_count: 1,
                total_value_zat: 0,
                commitment,
            },
        })
    }

    fn zero_value_prepared_projection(
        network: Network,
        unspent: &WalletUnspentOutput,
    ) -> Result<PreparedWalletProjection, zinder_wallet_projection::WalletProjectionContractError>
    {
        let evidence = zero_value_ready_evidence(network, unspent)?;
        Ok(PreparedWalletProjection {
            network,
            supported_reorg_depth: 0,
            first_block: source_position().tip,
            tip: source_position().tip,
            source_sequence_digest: evidence.source_sequence_digest,
            unspent_outputs: vec![(WalletOutpointKey::new(unspent.outpoint), unspent.clone())],
            unspent_output_by_address: vec![WalletAddressUnspentOutputKey::new(unspent)],
            spent_outputs: Vec::new(),
            address_transactions: expected_address_transactions(unspent, None),
            address_balances: Vec::new(),
            reorg_undo: Vec::new(),
            row_counts: evidence.row_counts,
            utxo_summary: evidence.utxo_summary,
            projection_digest: evidence.projection_digest,
            counters: crate::sort_merge::WalletSortMergeCounters {
                scanned_block_count: 1,
                scanned_transaction_count: 1,
                staged_output_count: 1,
                staged_spend_count: 0,
                historical_prevout_read_count: 0,
                peak_accounted_bytes: 0,
                max_accounted_bytes: 0,
            },
            phase_durations: crate::sort_merge::WalletSortMergePhaseDurations::default(),
        })
    }

    #[derive(Clone, Copy)]
    enum SemanticTamper {
        OrphanAddressIndex,
        MissingAddressIndex,
        OverlappingOutputState,
        DifferentBlockAtSameHeight,
        NonForwardSameBlockSpend,
        IncorrectBalance,
        MissingAddressTransaction,
        IncorrectUndo,
    }

    impl SemanticTamper {
        const fn expected_reason(self) -> &'static str {
            match self {
                Self::OrphanAddressIndex => {
                    "address unspent index references a missing primary output"
                }
                Self::MissingAddressIndex => {
                    "address unspent index does not exactly cover every primary unspent output"
                }
                Self::OverlappingOutputState => {
                    "one outpoint appears in both unspent and spent output families"
                }
                Self::DifferentBlockAtSameHeight | Self::NonForwardSameBlockSpend => {
                    "spent output does not follow its exact canonical creation position"
                }
                Self::IncorrectBalance => {
                    "address balance rows differ from indexed unspent-output sums"
                }
                Self::MissingAddressTransaction => {
                    "address transaction rows do not exactly cover output create/spend effects"
                }
                Self::IncorrectUndo => {
                    "reorg undo row differs from reconstructed wallet block effects"
                }
            }
        }
    }

    fn refresh_prepared_evidence(
        prepared: &mut PreparedWalletProjection,
        evidence: &mut WalletProjectionReadyEvidence,
    ) -> Result<(), zinder_wallet_projection::WalletProjectionContractError> {
        let mut digest = WalletProjectionDigestBuilder::new();
        for (key, output) in &prepared.unspent_outputs {
            digest.append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                key.as_bytes(),
                &output.encode_value()?,
            )?;
        }
        for key in &prepared.unspent_output_by_address {
            digest.append_row(
                WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
                key.as_bytes(),
                &[],
            )?;
        }
        for (key, output) in &prepared.spent_outputs {
            digest.append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                key.as_bytes(),
                &output.encode_value()?,
            )?;
        }
        for transaction in &prepared.address_transactions {
            digest.append_row(
                WalletProjectionRowFamily::TransparentAddressTransaction,
                transaction.key.as_bytes(),
                &transaction.encode_value(),
            )?;
        }
        for balance in &prepared.address_balances {
            digest.append_row(
                WalletProjectionRowFamily::TransparentAddressBalance,
                &balance.encode_key(),
                &balance.encode_value(),
            )?;
        }
        for undo in &prepared.reorg_undo {
            digest.append_row(
                WalletProjectionRowFamily::ReorgUndo,
                &undo.encode_key(),
                &undo.encode_value()?,
            )?;
        }
        prepared.row_counts = digest.row_counts();
        prepared.projection_digest = digest.finish();
        evidence.row_counts = prepared.row_counts;
        evidence.projection_digest = prepared.projection_digest;
        Ok(())
    }

    fn apply_semantic_tamper(
        tamper: SemanticTamper,
        prepared: &mut PreparedWalletProjection,
        evidence: &mut WalletProjectionReadyEvidence,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match tamper {
            SemanticTamper::OrphanAddressIndex => {
                let orphan = sample_output(0x99, 1)?;
                prepared.unspent_output_by_address =
                    vec![WalletAddressUnspentOutputKey::new(&orphan)];
            }
            SemanticTamper::MissingAddressIndex => {
                prepared.unspent_output_by_address.clear();
            }
            SemanticTamper::OverlappingOutputState => {
                let output = prepared.unspent_outputs[0].1.clone();
                let spent = WalletSpentOutput::new(
                    output.clone(),
                    WalletTransactionPosition::new(
                        TransactionId::from_bytes([0x55; 32]),
                        1,
                        output.created_at.block,
                    ),
                    0,
                );
                prepared
                    .spent_outputs
                    .push((WalletOutpointKey::new(output.outpoint), spent));
                prepared.spent_outputs.sort_unstable_by_key(|(key, _)| *key);
            }
            SemanticTamper::DifferentBlockAtSameHeight => {
                let spent = prepared.spent_outputs[0].1.clone();
                prepared.spent_outputs[0].1 = WalletSpentOutput::new(
                    spent.output,
                    WalletTransactionPosition::new(
                        spent.spent_at.transaction_id,
                        spent.spent_at.tx_index_in_block,
                        BlockId::new(
                            spent.spent_at.block.height,
                            BlockHash::from_bytes([0xaa; 32]),
                        ),
                    ),
                    spent.input_index,
                );
            }
            SemanticTamper::NonForwardSameBlockSpend => {
                let spent = prepared.spent_outputs[0].1.clone();
                prepared.spent_outputs[0].1 = WalletSpentOutput::new(
                    spent.output.clone(),
                    WalletTransactionPosition::new(
                        spent.spent_at.transaction_id,
                        spent.output.created_at.tx_index_in_block,
                        spent.output.created_at.block,
                    ),
                    spent.input_index,
                );
            }
            SemanticTamper::IncorrectBalance => {
                prepared.address_balances[0].balance_zat = prepared.address_balances[0]
                    .balance_zat
                    .checked_add(1)
                    .ok_or("test balance overflow")?;
            }
            SemanticTamper::MissingAddressTransaction => {
                prepared.address_transactions.pop();
            }
            SemanticTamper::IncorrectUndo => {
                prepared.supported_reorg_depth = 1;
                prepared.reorg_undo = vec![WalletReorgUndo {
                    block: source_position().tip,
                    created_outpoints: Vec::new(),
                    spent_outpoints: Vec::new(),
                    address_transaction_keys: Vec::new(),
                }];
            }
        }
        refresh_prepared_evidence(prepared, evidence)?;
        Ok(())
    }

    #[test]
    fn building_store_refuses_ready_admission_and_nonfresh_reuse()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("wallet");
        let builder = RocksDbWalletBuilder::create_fresh(
            &path,
            Network::ZcashRegtest,
            source_position(),
            0,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        drop(builder);

        assert!(matches!(
            RocksDbWalletBuilder::create_fresh(
                &path,
                Network::ZcashRegtest,
                source_position(),
                0,
                RocksDbResourceBudget::for_local_tests(),
            ),
            Err(RocksDbWalletError::PathNotFresh { .. })
        ));
        assert!(matches!(
            RocksDbWalletStore::open_ready(
                &path,
                Network::ZcashRegtest,
                source_identity(),
                RocksDbResourceBudget::for_local_tests(),
            ),
            Err(RocksDbWalletError::StoreNotReady { .. })
        ));
        Ok(())
    }

    #[test]
    fn cold_validation_rejects_cross_family_tampering_with_matching_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        for (index, tamper) in [
            SemanticTamper::OrphanAddressIndex,
            SemanticTamper::MissingAddressIndex,
            SemanticTamper::OverlappingOutputState,
            SemanticTamper::DifferentBlockAtSameHeight,
            SemanticTamper::NonForwardSameBlockSpend,
            SemanticTamper::IncorrectBalance,
            SemanticTamper::MissingAddressTransaction,
            SemanticTamper::IncorrectUndo,
        ]
        .into_iter()
        .enumerate()
        {
            let temporary = TempDir::new()?;
            let network = Network::ZcashRegtest;
            let unspent = sample_output(0x22, 12_345)?;
            let spent_source = sample_output(0x33, 45_678)?;
            let spent = WalletSpentOutput::new(
                spent_source,
                WalletTransactionPosition::new(
                    TransactionId::from_bytes([0x44; 32]),
                    1,
                    source_position().tip,
                ),
                0,
            );
            let balance = WalletAddressBalance {
                address_script_hash: unspent.address_script_hash,
                balance_zat: unspent.value_zat,
            };
            let mut prepared = prepared_projection(network, &unspent, &spent, balance)?;
            let mut evidence = ready_evidence(network, &unspent, &spent, balance)?;
            apply_semantic_tamper(tamper, &mut prepared, &mut evidence)?;
            let path = temporary.path().join(format!("wallet-{index}"));
            let builder = RocksDbWalletBuilder::create_fresh(
                path,
                network,
                source_position(),
                prepared.supported_reorg_depth,
                RocksDbResourceBudget::for_local_tests(),
            )?;
            builder.load_prepared(&prepared, 1024 * 1024)?;
            let result = builder
                .reopen_for_validation()?
                .validate_rows(evidence, TEST_VALIDATION_MEMORY_LIMIT);
            let Err(error) = result else {
                return Err("semantically tampered wallet store was admitted".into());
            };
            let RocksDbWalletError::AdmissionChanged { reason } = error else {
                return Err(std::io::Error::other(error.to_string()).into());
            };
            assert_eq!(reason, tamper.expected_reason());
        }
        Ok(())
    }

    #[test]
    fn relationship_memory_admission_precedes_undo_set_insertion()
    -> Result<(), Box<dyn std::error::Error>> {
        let admitted_bytes = size_of::<(u32, ExpectedUndoEffects)>()
            .checked_add(size_of::<(
                WalletAddressTransactionKey,
                WalletAddressTransaction,
            )>())
            .ok_or("test relationship memory overflow")?;
        let memory_limit = u64::try_from(admitted_bytes)?;
        let mut relations = ExpectedWalletRelations::new(source_position().tip, 1, memory_limit)?;
        let output = sample_output(0x22, 12_345)?;

        assert!(matches!(
            relations.observe_output(&output),
            Err(RocksDbWalletError::AccountedValidationRelationMemoryLimit { .. })
        ));
        let undo = relations
            .undo_by_height
            .get(&source_position().tip.height.value())
            .ok_or("test undo relationship disappeared")?;
        assert!(undo.address_transaction_keys.is_empty());
        assert!(undo.created_outpoints.is_empty());
        Ok(())
    }

    #[test]
    fn duplicate_relationship_keys_need_no_additional_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let admitted_bytes = size_of::<(u32, ExpectedUndoEffects)>()
            .checked_add(size_of::<(
                WalletAddressTransactionKey,
                WalletAddressTransaction,
            )>())
            .and_then(|bytes| bytes.checked_add(size_of::<WalletAddressTransactionKey>()))
            .and_then(|bytes| bytes.checked_add(size_of::<WalletOutpointKey>()))
            .ok_or("test relationship memory overflow")?;
        let memory_limit = u64::try_from(admitted_bytes)?;
        let mut relations = ExpectedWalletRelations::new(source_position().tip, 1, memory_limit)?;
        let output = sample_output(0x22, 12_345)?;

        relations.observe_output(&output)?;
        relations.observe_output(&output)?;

        let undo = relations
            .undo_by_height
            .get(&source_position().tip.height.value())
            .ok_or("test undo relationship disappeared")?;
        assert_eq!(undo.address_transaction_keys.len(), 1);
        assert_eq!(undo.created_outpoints.len(), 1);
        assert_eq!(relations.memory.current, memory_limit);
        assert_eq!(relations.memory.peak, memory_limit);
        Ok(())
    }

    #[test]
    fn cold_validation_refuses_before_crossing_its_accounted_memory_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let network = Network::ZcashRegtest;
        let unspent = sample_output(0x22, 12_345)?;
        let spent_source = sample_output(0x33, 45_678)?;
        let spent = WalletSpentOutput::new(
            spent_source,
            WalletTransactionPosition::new(
                TransactionId::from_bytes([0x44; 32]),
                1,
                source_position().tip,
            ),
            0,
        );
        let balance = WalletAddressBalance {
            address_script_hash: unspent.address_script_hash,
            balance_zat: unspent.value_zat,
        };
        let prepared = prepared_projection(network, &unspent, &spent, balance)?;
        let evidence = ready_evidence(network, &unspent, &spent, balance)?;
        let builder = RocksDbWalletBuilder::create_fresh(
            temporary.path().join("wallet"),
            network,
            source_position(),
            0,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        builder.load_prepared(&prepared, 1024 * 1024)?;

        assert!(matches!(
            builder.reopen_for_validation()?.validate_rows(evidence, 0),
            Err(RocksDbWalletError::AccountedValidationRelationMemoryLimit { .. })
        ));
        Ok(())
    }

    #[test]
    fn ready_store_serves_exact_version_one_rows() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("wallet");
        let network = Network::ZcashRegtest;
        let unspent = sample_output(0x22, 12_345)?;
        let spent_source = sample_output(0x33, 45_678)?;
        let spent = WalletSpentOutput::new(
            spent_source,
            WalletTransactionPosition::new(
                TransactionId::from_bytes([0x44; 32]),
                1,
                source_position().tip,
            ),
            0,
        );
        let balance = WalletAddressBalance {
            address_script_hash: unspent.address_script_hash,
            balance_zat: unspent.value_zat,
        };
        let builder = RocksDbWalletBuilder::create_fresh(
            &path,
            network,
            source_position(),
            0,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let prepared = prepared_projection(network, &unspent, &spent, balance)?;
        builder.load_prepared(&prepared, 1024 * 1024)?;
        let evidence = ready_evidence(network, &unspent, &spent, balance)?;
        let store = builder
            .reopen_for_validation()?
            .validate_rows(evidence.clone(), TEST_VALIDATION_MEMORY_LIMIT)?
            .publish_ready()?;

        assert_eq!(store.ready_evidence(), &evidence);
        assert_eq!(
            store.find_unspent_output(unspent.outpoint)?,
            Some(unspent.clone())
        );
        assert_eq!(store.find_spent_output(spent.output.outpoint)?, Some(spent));
        assert_eq!(
            store.address_balance(unspent.address_script_hash)?,
            unspent.value_zat
        );
        assert_eq!(store.utxo_summary(), &evidence.utxo_summary);
        drop(store);

        let reopened = RocksDbWalletStore::open_ready(
            &path,
            network,
            source_identity(),
            RocksDbResourceBudget::for_local_tests(),
        )?;
        assert_eq!(reopened.ready_evidence(), &evidence);
        Ok(())
    }

    #[test]
    fn ready_store_refuses_stale_source_position_or_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let path = temporary.path().join("wallet");
        let network = Network::ZcashRegtest;
        let unspent = sample_output(0x22, 12_345)?;
        let spent_source = sample_output(0x33, 45_678)?;
        let spent = WalletSpentOutput::new(
            spent_source,
            WalletTransactionPosition::new(
                TransactionId::from_bytes([0x44; 32]),
                1,
                source_position().tip,
            ),
            0,
        );
        let balance = WalletAddressBalance {
            address_script_hash: unspent.address_script_hash,
            balance_zat: unspent.value_zat,
        };
        let builder = RocksDbWalletBuilder::create_fresh(
            &path,
            network,
            source_position(),
            0,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let prepared = prepared_projection(network, &unspent, &spent, balance)?;
        builder.load_prepared(&prepared, 1024 * 1024)?;
        let evidence = ready_evidence(network, &unspent, &spent, balance)?;
        drop(
            builder
                .reopen_for_validation()?
                .validate_rows(evidence.clone(), TEST_VALIDATION_MEMORY_LIMIT)?
                .publish_ready()?,
        );
        let stale_position = WalletCanonicalSourceIdentity::new(
            WalletProjectionSourcePosition::new(
                ChainEpochId::new(2),
                BlockId::new(BlockHeight::new(2), BlockHash::from_bytes([0x55; 32])),
                2,
            ),
            evidence.source_sequence_digest,
        );
        assert!(matches!(
            RocksDbWalletStore::open_ready(
                &path,
                network,
                stale_position,
                RocksDbResourceBudget::for_local_tests(),
            ),
            Err(RocksDbWalletError::CanonicalSourceMismatch { .. })
        ));
        let stale_digest = WalletCanonicalSourceIdentity::new(
            evidence.source_position,
            CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
                CanonicalBlockFactsSequenceDigestVersion::V1,
                1,
                [0x88; 32],
            ),
        );
        assert!(matches!(
            RocksDbWalletStore::open_ready(
                &path,
                network,
                stale_digest,
                RocksDbResourceBudget::for_local_tests(),
            ),
            Err(RocksDbWalletError::CanonicalSourceMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn ready_store_retains_zero_value_utxo_without_balance_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let network = Network::ZcashRegtest;
        let unspent = sample_output(0x66, 0)?;
        let builder = RocksDbWalletBuilder::create_fresh(
            temporary.path().join("wallet"),
            network,
            source_position(),
            0,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        let prepared = zero_value_prepared_projection(network, &unspent)?;
        builder.load_prepared(&prepared, 1024 * 1024)?;
        let evidence = zero_value_ready_evidence(network, &unspent)?;
        let store = builder
            .reopen_for_validation()?
            .validate_rows(evidence.clone(), TEST_VALIDATION_MEMORY_LIMIT)?
            .publish_ready()?;

        assert_eq!(store.find_unspent_output(unspent.outpoint)?, Some(unspent));
        assert_eq!(
            store.address_balance(prepared.unspent_outputs[0].1.address_script_hash)?,
            0
        );
        assert_eq!(evidence.row_counts.transparent_address_balance_count, 0);
        assert_eq!(store.utxo_summary().utxo_count, 1);
        Ok(())
    }
}
