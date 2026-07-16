//! Exact version-1 `RocksDB` wallet layout, admission, and reads.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    mem::size_of,
    num::NonZeroU16,
    path::Path,
    sync::Arc,
};
use zinder_rocksdb::{SortedVariableValues, VariableValueSortEvidence, VariableValueSorter};

use rust_rocksdb::{
    BoundColumnFamily, Cache, ColumnFamilyDescriptor, DBCompressionType,
    DEFAULT_COLUMN_FAMILY_NAME, Direction, FlushOptions, IngestExternalFileOptions, IteratorMode,
    Options, ReadOptions, WriteBatch, WriteOptions,
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

use crate::{RocksDbWalletError, projection_load::PreparedWalletProjectionLoad};

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

pub(crate) const TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY: &str = "transparent_unspent_output";
pub(crate) const TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY: &str =
    "transparent_unspent_output_by_address";
pub(crate) const TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY: &str = "transparent_spent_output";
pub(crate) const TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY: &str =
    "transparent_address_transaction";
pub(crate) const TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY: &str = "transparent_address_balance";
pub(crate) const REORG_UNDO_COLUMN_FAMILY: &str = "reorg_undo";

const WALLET_DATA_COLUMN_FAMILIES: [&str; 6] = [
    TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY,
    TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
    TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY,
    REORG_UNDO_COLUMN_FAMILY,
];
const ADDRESS_UNSPENT_KEY_BYTES: usize = 72;
const ADDRESS_TRANSACTION_KEY_BYTES: usize = 40;

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

/// Explicit bounded resources for one independent cold semantic validation.
#[derive(Clone, Copy)]
pub(crate) struct WalletColdValidationConfig<'staging> {
    pub(crate) staging_path: &'staging Path,
    pub(crate) max_sort_memory_bytes_per_sorter: u64,
    pub(crate) max_temporary_file_bytes_per_sorter: u64,
    pub(crate) max_accounted_reorg_undo_bytes: u64,
}

/// Bounded work evidence from independent cold cross-family validation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WalletColdValidationEvidence {
    pub(crate) address_index_sort: VariableValueSortEvidence,
    pub(crate) address_transaction_sort: VariableValueSortEvidence,
    pub(crate) peak_accounted_reorg_undo_bytes: u64,
    pub(crate) max_accounted_reorg_undo_bytes: u64,
    pub(crate) random_read_count: u64,
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

    /// Returns column-family options identical to those used by the BUILDING store.
    pub(crate) fn data_options(&self) -> Options {
        wallet_data_options(&self.bounded_open.block_cache, self.resource_budget)
    }

    /// Ingests six externally prepared SST families while the store is BUILDING.
    pub(crate) fn ingest_projection_ssts(
        &self,
        prepared: &mut PreparedWalletProjectionLoad,
    ) -> Result<(), RocksDbWalletError> {
        let WalletProjectionBuildState::Building(plan) = &self.control.build_state else {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "projection SST ingestion requires a BUILDING control record",
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
        for family in std::mem::take(&mut prepared.families) {
            if family.paths.is_empty() {
                continue;
            }
            let column_family = column_family(&self.bounded_open, family.name)?;
            let mut options = IngestExternalFileOptions::default();
            options.set_move_files(true);
            options.set_snapshot_consistency(true);
            options.set_allow_global_seqno(false);
            options.set_allow_blocking_flush(false);
            self.bounded_open
                .db
                .ingest_external_file_cf_opts(&column_family, &options, family.paths)
                .map_err(|source| {
                    RocksDbWalletError::rocksdb("wallet projection external SST ingestion", source)
                })?;
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
        config: WalletColdValidationConfig<'_>,
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
            config,
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
            ColumnFamilyDescriptor::new(name, wallet_data_options(block_cache, resource_budget))
        })
        .collect()
}

fn wallet_data_options(block_cache: &Cache, resource_budget: RocksDbResourceBudget) -> Options {
    let mut options = Options::default();
    options.set_compression_type(DBCompressionType::Snappy);
    options.set_block_based_table_factory(&build_block_based_table_factory(block_cache));
    options.set_write_buffer_size(
        usize::try_from(resource_budget.write_buffer_bytes).unwrap_or(usize::MAX),
    );
    options.set_max_write_buffer_number(resource_budget.max_write_buffer_count);
    options
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
struct ExpectedReorgUndoEffects {
    block: Option<zinder_core::BlockId>,
    created_outpoints: BTreeSet<WalletOutpointKey>,
    spent_outpoints: BTreeSet<WalletOutpointKey>,
    address_transaction_keys: BTreeSet<WalletAddressTransactionKey>,
}

#[derive(Debug)]
struct AccountedValidationReorgUndoMemory {
    limit: u64,
    current: u64,
    peak: u64,
}

impl AccountedValidationReorgUndoMemory {
    const fn new(limit: u64) -> Self {
        Self {
            limit,
            current: 0,
            peak: 0,
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), RocksDbWalletError> {
        let bytes = u64::try_from(bytes)
            .map_err(|_| RocksDbWalletError::ProjectionLoadAccountingOverflow)?;
        let required_bytes = self
            .current
            .checked_add(bytes)
            .ok_or(RocksDbWalletError::ProjectionLoadAccountingOverflow)?;
        if required_bytes > self.limit {
            return Err(RocksDbWalletError::AccountedReorgUndoMemoryLimit {
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
    memory: &mut AccountedValidationReorgUndoMemory,
) -> Result<(), RocksDbWalletError> {
    if keys.contains(&key) {
        return Ok(());
    }
    memory.reserve(size_of::<Key>())?;
    if !keys.insert(key) {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "validation reorg-undo key changed during single-threaded admission",
        });
    }
    Ok(())
}

#[derive(Debug)]
struct ExpectedReorgUndoSuffix {
    undo_by_height: BTreeMap<u32, ExpectedReorgUndoEffects>,
    memory: AccountedValidationReorgUndoMemory,
}

impl ExpectedReorgUndoSuffix {
    fn new(
        ready_tip: zinder_core::BlockId,
        undo_count: u64,
        max_accounted_bytes: u64,
    ) -> Result<Self, RocksDbWalletError> {
        let mut suffix = Self {
            undo_by_height: BTreeMap::new(),
            memory: AccountedValidationReorgUndoMemory::new(max_accounted_bytes),
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
            suffix
                .memory
                .reserve(size_of::<(u32, ExpectedReorgUndoEffects)>())?;
            suffix
                .undo_by_height
                .insert(height, ExpectedReorgUndoEffects::default());
        }
        Ok(suffix)
    }

    fn observe_address_transaction(
        &mut self,
        key: WalletAddressTransactionKey,
        block: zinder_core::BlockId,
    ) -> Result<(), RocksDbWalletError> {
        if let Some(undo) = self.undo_by_height.get_mut(&block.height.value()) {
            remember_undo_block(undo, block)?;
            insert_accounted_relation_key(
                &mut undo.address_transaction_keys,
                key,
                &mut self.memory,
            )?;
        }
        Ok(())
    }

    fn observe_created(&mut self, output: &WalletUnspentOutput) -> Result<(), RocksDbWalletError> {
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

    fn observe_spent(&mut self, spent: &WalletSpentOutput) -> Result<(), RocksDbWalletError> {
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
    undo: &mut ExpectedReorgUndoEffects,
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
    config: WalletColdValidationConfig<'_>,
) -> Result<WalletColdValidationEvidence, RocksDbWalletError> {
    let counts = evidence.row_counts;
    if counts.transparent_unspent_output_count != counts.transparent_unspent_output_by_address_count
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index does not exactly cover every primary unspent output",
        });
    }
    let mut address_index_sorter = VariableValueSorter::<ADDRESS_UNSPENT_KEY_BYTES>::new(
        config.staging_path,
        "wallet-cold-validation-address-index",
        config.max_sort_memory_bytes_per_sorter,
        config.max_temporary_file_bytes_per_sorter,
    )?;
    let mut address_transaction_sorter = VariableValueSorter::<ADDRESS_TRANSACTION_KEY_BYTES>::new(
        config.staging_path,
        "wallet-cold-validation-address-transactions",
        config.max_sort_memory_bytes_per_sorter,
        config.max_temporary_file_bytes_per_sorter,
    )?;
    let mut expected_undo = ExpectedReorgUndoSuffix::new(
        evidence.source_position.tip,
        counts.reorg_undo_count,
        config.max_accounted_reorg_undo_bytes,
    )?;
    let mut digest = WalletProjectionDigestBuilder::new();
    let utxo_summary = validate_primary_output_rows(
        bounded_open,
        network,
        &mut digest,
        &mut address_index_sorter,
        &mut address_transaction_sorter,
        &mut expected_undo,
        &counts,
    )?;
    let mut sorted_address_index = address_index_sorter.finish()?;
    let address_index_sort = sorted_address_index.evidence();
    validate_address_index_and_balance_rows(
        bounded_open,
        &mut digest,
        &mut sorted_address_index,
        &counts,
    )?;
    let mut sorted_address_transactions = address_transaction_sorter.finish()?;
    let address_transaction_sort = sorted_address_transactions.evidence();
    validate_address_transaction_rows(
        bounded_open,
        &mut digest,
        counts.transparent_address_transaction_count,
        &mut sorted_address_transactions,
    )?;
    validate_reorg_undo_rows(
        bounded_open,
        &mut digest,
        counts.reorg_undo_count,
        evidence.source_position.tip,
        &expected_undo.undo_by_height,
    )?;
    let observed_row_counts = digest.row_counts();
    let observed_digest = digest.finish();
    if observed_row_counts != evidence.row_counts
        || observed_digest != evidence.projection_digest
        || utxo_summary != evidence.utxo_summary
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "READY evidence differs from cold wallet rows",
        });
    }
    Ok(WalletColdValidationEvidence {
        address_index_sort,
        address_transaction_sort,
        peak_accounted_reorg_undo_bytes: expected_undo.memory.peak,
        max_accounted_reorg_undo_bytes: config.max_accounted_reorg_undo_bytes,
        random_read_count: 0,
    })
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the ordered two-family merge keeps every derived relation tied to its decoded primary row"
)]
fn validate_primary_output_rows(
    bounded_open: &BoundedRocksDbOpen,
    network: Network,
    digest: &mut WalletProjectionDigestBuilder,
    address_index_sorter: &mut VariableValueSorter<ADDRESS_UNSPENT_KEY_BYTES>,
    address_transaction_sorter: &mut VariableValueSorter<ADDRESS_TRANSACTION_KEY_BYTES>,
    expected_undo: &mut ExpectedReorgUndoSuffix,
    counts: &zinder_wallet_projection::WalletProjectionFamilyRowCounts,
) -> Result<WalletUtxoSetSummary, RocksDbWalletError> {
    use zinder_core::wire::UtxoSetCommitmentElement;

    let unspent_family = column_family(bounded_open, TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY)?;
    let spent_family = column_family(bounded_open, TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY)?;
    let mut unspent_rows = bounded_open.db.iterator_cf_opt(
        &unspent_family,
        validation_read_options(),
        IteratorMode::Start,
    );
    let mut spent_rows = bounded_open.db.iterator_cf_opt(
        &spent_family,
        validation_read_options(),
        IteratorMode::Start,
    );
    let mut next_unspent = next_validation_row(&mut unspent_rows, "unspent validation scan")?;
    let mut next_spent = next_validation_row(&mut spent_rows, "spent validation scan")?;
    let mut unspent_count = 0u64;
    let mut spent_count = 0u64;
    let mut total_value_zat = 0u64;
    let mut commitment = zinder_core::TransparentUtxoSetCommitment::empty();
    while next_unspent.is_some() || next_spent.is_some() {
        let take_unspent = match (&next_unspent, &next_spent) {
            (Some((unspent_key, _)), Some((spent_key, _))) => {
                match unspent_key.as_ref().cmp(spent_key.as_ref()) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => {
                        return Err(RocksDbWalletError::AdmissionChanged {
                            reason: "one outpoint appears in both unspent and spent output families",
                        });
                    }
                }
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_unspent {
            let (key_bytes, value_bytes) =
                next_unspent
                    .take()
                    .ok_or(RocksDbWalletError::AdmissionChanged {
                        reason: "unspent validation merge lost its current row",
                    })?;
            let key = WalletOutpointKey::decode(&key_bytes)?;
            let output = WalletUnspentOutput::decode_value(key, &value_bytes)?;
            let address_key = WalletAddressUnspentOutputKey::new(&output);
            address_index_sorter.push(*address_key.as_bytes(), &output.value_zat.to_be_bytes())?;
            stage_expected_address_transaction(
                address_transaction_sorter,
                expected_undo,
                output.address_script_hash,
                output.created_at,
            )?;
            expected_undo.observe_created(&output)?;
            digest.append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                &key_bytes,
                &value_bytes,
            )?;
            unspent_count =
                increment_validation_count(unspent_count, "unspent row count overflow")?;
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
            next_unspent = next_validation_row(&mut unspent_rows, "unspent validation scan")?;
        } else {
            let (key_bytes, value_bytes) =
                next_spent
                    .take()
                    .ok_or(RocksDbWalletError::AdmissionChanged {
                        reason: "spent validation merge lost its current row",
                    })?;
            let key = WalletOutpointKey::decode(&key_bytes)?;
            let spent = WalletSpentOutput::decode_value(key, &value_bytes)?;
            validate_spent_position(&spent)?;
            stage_expected_address_transaction(
                address_transaction_sorter,
                expected_undo,
                spent.output.address_script_hash,
                spent.output.created_at,
            )?;
            stage_expected_address_transaction(
                address_transaction_sorter,
                expected_undo,
                spent.output.address_script_hash,
                spent.spent_at,
            )?;
            expected_undo.observe_created(&spent.output)?;
            if spent.output.created_at.block != spent.spent_at.block {
                expected_undo.observe_spent(&spent)?;
            }
            digest.append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                &key_bytes,
                &value_bytes,
            )?;
            spent_count = increment_validation_count(spent_count, "spent row count overflow")?;
            next_spent = next_validation_row(&mut spent_rows, "spent validation scan")?;
        }
    }
    if unspent_count != counts.transparent_unspent_output_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "unspent row count differs from READY evidence",
        });
    }
    if spent_count != counts.transparent_spent_output_count {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "spent row count differs from READY evidence",
        });
    }
    Ok(WalletUtxoSetSummary {
        utxo_count: unspent_count,
        total_value_zat,
        commitment,
    })
}

fn stage_expected_address_transaction(
    sorter: &mut VariableValueSorter<ADDRESS_TRANSACTION_KEY_BYTES>,
    expected_undo: &mut ExpectedReorgUndoSuffix,
    address_script_hash: TransparentAddressScriptHash,
    position: zinder_wallet_projection::WalletTransactionPosition,
) -> Result<(), RocksDbWalletError> {
    let key = WalletAddressTransactionKey::new(
        address_script_hash,
        position.block.height,
        position.tx_index_in_block,
    );
    let transaction =
        WalletAddressTransaction::new(key, position.transaction_id, position.block.hash);
    sorter.push(*key.as_bytes(), &transaction.encode_value())?;
    expected_undo.observe_address_transaction(key, position.block)
}

#[allow(
    clippy::too_many_lines,
    reason = "the sequential index and balance comparison must retain one visible address-group state"
)]
fn validate_address_index_and_balance_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_rows: &mut SortedVariableValues<ADDRESS_UNSPENT_KEY_BYTES>,
    counts: &zinder_wallet_projection::WalletProjectionFamilyRowCounts,
) -> Result<(), RocksDbWalletError> {
    let address_family = column_family(
        bounded_open,
        TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
    )?;
    let balance_family = column_family(bounded_open, TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY)?;
    let mut address_rows = bounded_open.db.iterator_cf_opt(
        &address_family,
        validation_read_options(),
        IteratorMode::Start,
    );
    let mut balance_rows = bounded_open.db.iterator_cf_opt(
        &balance_family,
        validation_read_options(),
        IteratorMode::Start,
    );
    let mut address_count = 0u64;
    let mut balance_count = 0u64;
    let mut current_address = None;
    let mut current_balance_zat = 0u64;
    while let Some(expected) = expected_rows.next_record()? {
        let (key_bytes, value_bytes) = next_validation_row(
            &mut address_rows,
            "address unspent validation scan",
        )?
        .ok_or(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index does not exactly cover every primary unspent output",
        })?;
        if key_bytes.as_ref() != expected.key.as_slice() || !value_bytes.is_empty() {
            return Err(RocksDbWalletError::AdmissionChanged {
                reason: "address unspent index does not exactly cover every primary unspent output",
            });
        }
        let address_key = WalletAddressUnspentOutputKey::decode(&expected.key)?;
        let value_zat =
            u64::from_be_bytes(expected.encoded_value.as_slice().try_into().map_err(|_| {
                RocksDbWalletError::AdmissionChanged {
                    reason: "cold validation address-index value is not an exact u64",
                }
            })?);
        let address = address_key.address_script_hash();
        if current_address.is_some_and(|current| current != address) {
            validate_expected_balance(
                &mut balance_rows,
                digest,
                current_address.ok_or(RocksDbWalletError::AdmissionChanged {
                    reason: "address unspent validation group disappeared",
                })?,
                current_balance_zat,
                &mut balance_count,
            )?;
            current_balance_zat = 0;
        }
        current_address = Some(address);
        current_balance_zat = current_balance_zat.checked_add(value_zat).ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "address unspent value total overflow",
            },
        )?;
        digest.append_row(
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            &key_bytes,
            &value_bytes,
        )?;
        address_count =
            increment_validation_count(address_count, "address unspent row count overflow")?;
    }
    if next_validation_row(&mut address_rows, "address unspent validation scan")?.is_some()
        || address_count != counts.transparent_unspent_output_by_address_count
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address unspent index does not exactly cover every primary unspent output",
        });
    }
    if let Some(address) = current_address {
        validate_expected_balance(
            &mut balance_rows,
            digest,
            address,
            current_balance_zat,
            &mut balance_count,
        )?;
    }
    if next_validation_row(&mut balance_rows, "address balance validation scan")?.is_some()
        || balance_count != counts.transparent_address_balance_count
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address balance rows do not exactly cover positive indexed addresses",
        });
    }
    Ok(())
}

fn validate_expected_balance<I, Key, Value>(
    balance_rows: &mut I,
    digest: &mut WalletProjectionDigestBuilder,
    address_script_hash: TransparentAddressScriptHash,
    balance_zat: u64,
    balance_count: &mut u64,
) -> Result<(), RocksDbWalletError>
where
    I: Iterator<Item = Result<(Key, Value), rust_rocksdb::Error>>,
    Key: AsRef<[u8]>,
    Value: AsRef<[u8]>,
{
    if balance_zat == 0 {
        return Ok(());
    }
    let expected = WalletAddressBalance {
        address_script_hash,
        balance_zat,
    };
    let (key_bytes, value_bytes) =
        next_validation_row(balance_rows, "address balance validation scan")?.ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "address balance rows differ from indexed unspent-output sums",
            },
        )?;
    let observed = WalletAddressBalance::decode(key_bytes.as_ref(), value_bytes.as_ref())?;
    if observed != expected {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address balance rows differ from indexed unspent-output sums",
        });
    }
    digest.append_row(
        WalletProjectionRowFamily::TransparentAddressBalance,
        key_bytes.as_ref(),
        value_bytes.as_ref(),
    )?;
    *balance_count =
        increment_validation_count(*balance_count, "address balance row count overflow")?;
    Ok(())
}

fn validate_address_transaction_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    expected_rows: &mut SortedVariableValues<ADDRESS_TRANSACTION_KEY_BYTES>,
) -> Result<(), RocksDbWalletError> {
    let family = column_family(bounded_open, TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY)?;
    let mut rows =
        bounded_open
            .db
            .iterator_cf_opt(&family, validation_read_options(), IteratorMode::Start);
    let mut count = 0u64;
    let mut pending = expected_rows.next_record()?;
    while let Some(expected) = pending.take() {
        WalletAddressTransaction::decode_value(
            WalletAddressTransactionKey::decode(&expected.key)?,
            &expected.encoded_value,
        )?;
        loop {
            let Some(next) = expected_rows.next_record()? else {
                validate_expected_address_transaction(&mut rows, digest, &expected, &mut count)?;
                break;
            };
            if next.key != expected.key {
                validate_expected_address_transaction(&mut rows, digest, &expected, &mut count)?;
                pending = Some(next);
                break;
            }
            if next.encoded_value != expected.encoded_value {
                return Err(RocksDbWalletError::AdmissionChanged {
                    reason: "one address transaction key resolves to different transactions",
                });
            }
        }
    }
    if next_validation_row(&mut rows, "address transaction validation scan")?.is_some()
        || count != expected_count
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address transaction rows do not exactly cover output create/spend effects",
        });
    }
    Ok(())
}

fn validate_expected_address_transaction<I, Key, Value>(
    rows: &mut I,
    digest: &mut WalletProjectionDigestBuilder,
    expected: &zinder_rocksdb::VariableValueRecord<ADDRESS_TRANSACTION_KEY_BYTES>,
    count: &mut u64,
) -> Result<(), RocksDbWalletError>
where
    I: Iterator<Item = Result<(Key, Value), rust_rocksdb::Error>>,
    Key: AsRef<[u8]>,
    Value: AsRef<[u8]>,
{
    let (key_bytes, value_bytes) =
        next_validation_row(rows, "address transaction validation scan")?.ok_or(
            RocksDbWalletError::AdmissionChanged {
                reason: "address transaction rows do not exactly cover output create/spend effects",
            },
        )?;
    if key_bytes.as_ref() != expected.key.as_slice()
        || value_bytes.as_ref() != expected.encoded_value
    {
        return Err(RocksDbWalletError::AdmissionChanged {
            reason: "address transaction rows differ from output create/spend effects",
        });
    }
    let key = WalletAddressTransactionKey::decode(key_bytes.as_ref())?;
    WalletAddressTransaction::decode_value(key, value_bytes.as_ref())?;
    digest.append_row(
        WalletProjectionRowFamily::TransparentAddressTransaction,
        key_bytes.as_ref(),
        value_bytes.as_ref(),
    )?;
    *count = increment_validation_count(*count, "address transaction row count overflow")?;
    Ok(())
}

fn next_validation_row<I, Key, Value>(
    rows: &mut I,
    operation: &'static str,
) -> Result<Option<(Key, Value)>, RocksDbWalletError>
where
    I: Iterator<Item = Result<(Key, Value), rust_rocksdb::Error>>,
{
    rows.next()
        .transpose()
        .map_err(|source| RocksDbWalletError::rocksdb(operation, source))
}

fn increment_validation_count(
    count: u64,
    overflow_reason: &'static str,
) -> Result<u64, RocksDbWalletError> {
    count
        .checked_add(1)
        .ok_or(RocksDbWalletError::AdmissionChanged {
            reason: overflow_reason,
        })
}

fn validate_reorg_undo_rows(
    bounded_open: &BoundedRocksDbOpen,
    digest: &mut WalletProjectionDigestBuilder,
    expected_count: u64,
    ready_tip: zinder_core::BlockId,
    expected_by_height: &BTreeMap<u32, ExpectedReorgUndoEffects>,
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

    const TEST_VALIDATION_SORT_MEMORY_BYTES: u64 = 16 * 1024 * 1024;
    const TEST_VALIDATION_TEMPORARY_FILE_BYTES: u64 = 256 * 1024 * 1024;
    const TEST_VALIDATION_REORG_UNDO_BYTES: u64 = 16 * 1024 * 1024;

    fn validation_config(staging_path: &Path) -> WalletColdValidationConfig<'_> {
        WalletColdValidationConfig {
            staging_path,
            max_sort_memory_bytes_per_sorter: TEST_VALIDATION_SORT_MEMORY_BYTES,
            max_temporary_file_bytes_per_sorter: TEST_VALIDATION_TEMPORARY_FILE_BYTES,
            max_accounted_reorg_undo_bytes: TEST_VALIDATION_REORG_UNDO_BYTES,
        }
    }

    struct SemanticValidationFixture {
        supported_reorg_depth: u32,
        unspent_outputs: Vec<(WalletOutpointKey, WalletUnspentOutput)>,
        unspent_output_by_address: Vec<WalletAddressUnspentOutputKey>,
        spent_outputs: Vec<(WalletOutpointKey, WalletSpentOutput)>,
        address_transactions: Vec<WalletAddressTransaction>,
        address_balances: Vec<WalletAddressBalance>,
        reorg_undo: Vec<WalletReorgUndo>,
        row_counts: WalletProjectionFamilyRowCounts,
        projection_digest: WalletProjectionDigest,
    }

    fn load_semantic_validation_fixture(
        builder: &RocksDbWalletBuilder,
        fixture: &SemanticValidationFixture,
    ) -> Result<(), RocksDbWalletError> {
        let mut batch = WriteBatch::default();
        let unspent_family = column_family(
            &builder.bounded_open,
            TRANSPARENT_UNSPENT_OUTPUT_COLUMN_FAMILY,
        )?;
        for (key, output) in &fixture.unspent_outputs {
            batch.put_cf(&unspent_family, key.as_bytes(), output.encode_value()?);
        }
        let address_index_family = column_family(
            &builder.bounded_open,
            TRANSPARENT_UNSPENT_OUTPUT_BY_ADDRESS_COLUMN_FAMILY,
        )?;
        for key in &fixture.unspent_output_by_address {
            batch.put_cf(&address_index_family, key.as_bytes(), []);
        }
        let spent_family = column_family(
            &builder.bounded_open,
            TRANSPARENT_SPENT_OUTPUT_COLUMN_FAMILY,
        )?;
        for (key, output) in &fixture.spent_outputs {
            batch.put_cf(&spent_family, key.as_bytes(), output.encode_value()?);
        }
        let address_transaction_family = column_family(
            &builder.bounded_open,
            TRANSPARENT_ADDRESS_TRANSACTION_COLUMN_FAMILY,
        )?;
        for transaction in &fixture.address_transactions {
            batch.put_cf(
                &address_transaction_family,
                transaction.key.as_bytes(),
                transaction.encode_value(),
            );
        }
        let balance_family = column_family(
            &builder.bounded_open,
            TRANSPARENT_ADDRESS_BALANCE_COLUMN_FAMILY,
        )?;
        for balance in &fixture.address_balances {
            batch.put_cf(
                &balance_family,
                balance.encode_key(),
                balance.encode_value(),
            );
        }
        let undo_family = column_family(&builder.bounded_open, REORG_UNDO_COLUMN_FAMILY)?;
        for undo in &fixture.reorg_undo {
            batch.put_cf(&undo_family, undo.encode_key(), undo.encode_value()?);
        }
        let mut write_options = WriteOptions::default();
        write_options.disable_wal(true);
        builder
            .bounded_open
            .db
            .write_opt(&batch, &write_options)
            .map_err(|source| RocksDbWalletError::rocksdb("semantic validation fixture", source))
    }

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

    fn semantic_validation_fixture(
        network: Network,
        unspent: &WalletUnspentOutput,
        spent: &WalletSpentOutput,
        balance: WalletAddressBalance,
    ) -> Result<SemanticValidationFixture, zinder_wallet_projection::WalletProjectionContractError>
    {
        let evidence = ready_evidence(network, unspent, spent, balance)?;
        Ok(SemanticValidationFixture {
            supported_reorg_depth: 0,
            unspent_outputs: vec![(WalletOutpointKey::new(unspent.outpoint), unspent.clone())],
            unspent_output_by_address: vec![WalletAddressUnspentOutputKey::new(unspent)],
            spent_outputs: vec![(WalletOutpointKey::new(spent.output.outpoint), spent.clone())],
            address_transactions: expected_address_transactions(unspent, Some(spent)),
            address_balances: vec![balance],
            reorg_undo: Vec::new(),
            row_counts: evidence.row_counts,
            projection_digest: evidence.projection_digest,
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

    fn zero_value_semantic_validation_fixture(
        network: Network,
        unspent: &WalletUnspentOutput,
    ) -> Result<SemanticValidationFixture, zinder_wallet_projection::WalletProjectionContractError>
    {
        let evidence = zero_value_ready_evidence(network, unspent)?;
        Ok(SemanticValidationFixture {
            supported_reorg_depth: 0,
            unspent_outputs: vec![(WalletOutpointKey::new(unspent.outpoint), unspent.clone())],
            unspent_output_by_address: vec![WalletAddressUnspentOutputKey::new(unspent)],
            spent_outputs: Vec::new(),
            address_transactions: expected_address_transactions(unspent, None),
            address_balances: Vec::new(),
            reorg_undo: Vec::new(),
            row_counts: evidence.row_counts,
            projection_digest: evidence.projection_digest,
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
                Self::OrphanAddressIndex | Self::MissingAddressIndex => {
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
        prepared: &mut SemanticValidationFixture,
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
        prepared: &mut SemanticValidationFixture,
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
            let mut prepared = semantic_validation_fixture(network, &unspent, &spent, balance)?;
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
            load_semantic_validation_fixture(&builder, &prepared)?;
            let result = builder
                .reopen_for_validation()?
                .validate_rows(evidence, validation_config(temporary.path()));
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
    fn reorg_undo_memory_admission_precedes_set_insertion() -> Result<(), Box<dyn std::error::Error>>
    {
        let admitted_bytes = size_of::<(u32, ExpectedReorgUndoEffects)>();
        let memory_limit = u64::try_from(admitted_bytes)?;
        let mut suffix = ExpectedReorgUndoSuffix::new(source_position().tip, 1, memory_limit)?;
        let output = sample_output(0x22, 12_345)?;

        assert!(matches!(
            suffix.observe_created(&output),
            Err(RocksDbWalletError::AccountedReorgUndoMemoryLimit { .. })
        ));
        let undo = suffix
            .undo_by_height
            .get(&source_position().tip.height.value())
            .ok_or("test undo suffix disappeared")?;
        assert!(undo.created_outpoints.is_empty());
        Ok(())
    }

    #[test]
    fn duplicate_reorg_undo_keys_need_no_additional_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let admitted_bytes = size_of::<(u32, ExpectedReorgUndoEffects)>()
            .checked_add(size_of::<WalletOutpointKey>())
            .ok_or("test reorg undo memory overflow")?;
        let memory_limit = u64::try_from(admitted_bytes)?;
        let mut suffix = ExpectedReorgUndoSuffix::new(source_position().tip, 1, memory_limit)?;
        let output = sample_output(0x22, 12_345)?;

        suffix.observe_created(&output)?;
        suffix.observe_created(&output)?;

        let undo = suffix
            .undo_by_height
            .get(&source_position().tip.height.value())
            .ok_or("test undo suffix disappeared")?;
        assert_eq!(undo.created_outpoints.len(), 1);
        assert_eq!(suffix.memory.current, memory_limit);
        assert_eq!(suffix.memory.peak, memory_limit);
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
        let mut prepared = semantic_validation_fixture(network, &unspent, &spent, balance)?;
        let mut evidence = ready_evidence(network, &unspent, &spent, balance)?;
        prepared.supported_reorg_depth = 1;
        prepared.reorg_undo = vec![WalletReorgUndo {
            block: source_position().tip,
            created_outpoints: Vec::new(),
            spent_outpoints: Vec::new(),
            address_transaction_keys: Vec::new(),
        }];
        refresh_prepared_evidence(&mut prepared, &mut evidence)?;
        let builder = RocksDbWalletBuilder::create_fresh(
            temporary.path().join("wallet"),
            network,
            source_position(),
            1,
            RocksDbResourceBudget::for_local_tests(),
        )?;
        load_semantic_validation_fixture(&builder, &prepared)?;

        let mut config = validation_config(temporary.path());
        config.max_accounted_reorg_undo_bytes = 0;
        assert!(matches!(
            builder
                .reopen_for_validation()?
                .validate_rows(evidence, config),
            Err(RocksDbWalletError::AccountedReorgUndoMemoryLimit { .. })
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
        let prepared = semantic_validation_fixture(network, &unspent, &spent, balance)?;
        load_semantic_validation_fixture(&builder, &prepared)?;
        let evidence = ready_evidence(network, &unspent, &spent, balance)?;
        let store = builder
            .reopen_for_validation()?
            .validate_rows(evidence.clone(), validation_config(temporary.path()))?
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
        let prepared = semantic_validation_fixture(network, &unspent, &spent, balance)?;
        load_semantic_validation_fixture(&builder, &prepared)?;
        let evidence = ready_evidence(network, &unspent, &spent, balance)?;
        drop(
            builder
                .reopen_for_validation()?
                .validate_rows(evidence.clone(), validation_config(temporary.path()))?
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
        let prepared = zero_value_semantic_validation_fixture(network, &unspent)?;
        load_semantic_validation_fixture(&builder, &prepared)?;
        let evidence = zero_value_ready_evidence(network, &unspent)?;
        let store = builder
            .reopen_for_validation()?
            .validate_rows(evidence.clone(), validation_config(temporary.path()))?
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
