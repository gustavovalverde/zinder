//! `RocksDB` wrapper for the derive plane.
//!
//! `DeriveStore` is intentionally separate from `zinder_store::PrimaryChainStore`:
//! it lives in its own filesystem path, has its own column families, and uses
//! its own schema version. The two stores never share keys.
//!
//! Both stores share one source of truth for `RocksDB` option choices:
//! [`zinder_store::open_bounded_rocksdb`] from
//! [ADR-0020](../../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
//! That keeps the bulk-catchup-OOM trap, which is a property of unbounded
//! `RocksDB` defaults rather than the canonical store's specific layout,
//! impossible to recur in the derive plane.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    hash::BuildHasher,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::{RwLock, RwLockReadGuard};
use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, IteratorMode, Options, ReadOptions, Snapshot, WriteBatch,
    WriteOptions, checkpoint::Checkpoint,
};
use zinder_core::{BlockHash, BlockHeight, ChainEpoch, ChainEpochId};
use zinder_store::{
    ChainEvent, ResourceGaugeThrottle, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget,
    RocksDbResourceGaugeInputs, StoreRole, build_block_based_table_factory, open_bounded_rocksdb,
    record_rocksdb_resource_gauges,
};

use crate::{
    consumer::block_production_time::{
        BLOCK_PRODUCTION_TIME_CONSUMER_NAME, BLOCK_PRODUCTION_TIME_SCHEMA,
    },
    consumer::block_summary::{BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA},
    consumer::commitment_root_search::{
        COMMITMENT_ROOT_SEARCH_CONSUMER_NAME, COMMITMENT_ROOT_SEARCH_SCHEMA,
    },
    consumer::conventional_fee_distribution::{
        CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME, CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA,
    },
    consumer::ironwood_migration::{IRONWOOD_MIGRATION_CONSUMER_NAME, IRONWOOD_MIGRATION_SCHEMA},
    consumer::mempool_event_counts::MEMPOOL_EVENT_COUNTS_SCHEMA,
    consumer::paid_fee_distribution::PAID_FEE_DISTRIBUTION_SCHEMA,
    consumer::recent_transactions::{
        RECENT_TRANSACTIONS_CONSUMER_NAME, RECENT_TRANSACTIONS_SCHEMA,
    },
    consumer::reorg_incidents::{REORG_INCIDENTS_CONSUMER_NAME, REORG_INCIDENTS_SCHEMA},
    consumer::transaction_component_summary::{
        TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME, TRANSACTION_COMPONENT_SUMMARY_SCHEMA,
    },
    consumer::transaction_fees::{TRANSACTION_FEES_CONSUMER_NAME, TRANSACTION_FEES_SCHEMA},
    consumer::transaction_history::{
        TRANSACTION_HISTORY_CONSUMER_NAME, TRANSACTION_HISTORY_SCHEMA,
    },
    consumer::transparent_address_activity::{
        TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
    },
    consumer::transparent_address_deltas::{
        TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME, TRANSPARENT_ADDRESS_DELTAS_SCHEMA,
    },
    consumer::transparent_address_ranking::{
        TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME, TRANSPARENT_ADDRESS_RANKING_SCHEMA,
    },
    consumer::transparent_address_transaction_history::{
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    },
    consumer::transparent_outpoint_spend::{
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
    },
    consumer::value_pool_balance_history::VALUE_POOL_BALANCE_HISTORY_SCHEMA,
    consumer::value_pool_flow_history::{
        VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME, VALUE_POOL_FLOW_HISTORY_SCHEMA,
    },
    consumer::{
        BlockCommitContext, BlockKeyedConsumer, BlockProjectionCheckpoint, ChainCommittedEvent,
        ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx, DeriveConsumerName,
        DeriveConsumerSchema, DeriveMempoolConsumer, RevertedRange,
        apply_chain_committed_in_memory, apply_chain_reorged_in_memory,
    },
    error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError},
};

/// Conventional subdirectory of the canonical store path where the derive
/// `RocksDB` instance lives.
///
/// Both the writer (`zinder-ingest`) and any reader process opening the
/// store in secondary mode resolve the derive store with
/// [`DeriveStore::path_for_canonical`], so operators only configure one
/// `storage.path` per service.
pub const DERIVE_STORE_SUBDIR: &str = "derive";

/// Container-format version of the derive store.
///
/// Gates the parts shared by every consumer: the per-consumer schema
/// manifest layout, the cursor encoding, and the metadata column family.
/// Per-consumer column-family layouts version themselves through
/// [`DeriveConsumerSchema::schema_version`]; a consumer changing its own
/// persisted row contract bumps its own version. Incompatible changes rebuild
/// only that consumer, while explicitly row-compatible changes preserve its
/// rows and cursor. This
/// constant bumps only when the shared container changes, which forces a
/// whole-store wipe because no consumer's data survives a container change.
/// The version is persisted in the `consumer_metadata` column family on
/// first open and validated on subsequent opens.
pub const DERIVE_STORE_FORMAT_VERSION: u16 = 7;

/// Total attempts used to cross a primary-compaction race while a secondary
/// catches up and validates its newly replayed manifest.
const SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS: u32 = 3;

const STORE_FORMAT_VERSION_KEY: &[u8] = b"\x00\x01schema_version";
const DERIVE_STATUS_KEY: &[u8] = b"\x00\x02derive_status";
const CONSUMER_SCHEMA_KEY_PREFIX: &[u8] = b"\x00\x03consumer_schema:";
const CONSUMER_PROJECTION_STATE_KEY_PREFIX: &[u8] = b"\x00\x04consumer_projection_state:";
const CONSUMER_PROJECTION_STATE_VERSION: u8 = 1;
const CONSUMER_PROJECTION_STATE_LEN: usize = 94;
const ROCKSDB_DEFAULT_COLUMN_FAMILY: &str = "default";

/// Inclusive lower bound for a full column-family clear: the empty key sorts
/// before every stored key.
const CLEAR_RANGE_LOWER_BOUND: &[u8] = &[];
/// Exclusive upper bound for a full column-family clear.
///
/// It sits above every consumer key, so one range tombstone covers the whole
/// family; any key at or above it is removed by the residue sweep in
/// [`DeriveStore::clear_consumer_column_family`].
const CLEAR_RANGE_UPPER_BOUND: &[u8] = &[0xff; 512];

const BUNDLED_CONSUMERS: &[DeriveConsumerSchema] = &[
    BLOCK_PRODUCTION_TIME_SCHEMA,
    BLOCK_SUMMARY_SCHEMA,
    IRONWOOD_MIGRATION_SCHEMA,
    COMMITMENT_ROOT_SEARCH_SCHEMA,
    CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA,
    MEMPOOL_EVENT_COUNTS_SCHEMA,
    PAID_FEE_DISTRIBUTION_SCHEMA,
    RECENT_TRANSACTIONS_SCHEMA,
    TRANSACTION_HISTORY_SCHEMA,
    REORG_INCIDENTS_SCHEMA,
    TRANSACTION_FEES_SCHEMA,
    TRANSACTION_COMPONENT_SUMMARY_SCHEMA,
    TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
    TRANSPARENT_ADDRESS_DELTAS_SCHEMA,
    TRANSPARENT_ADDRESS_RANKING_SCHEMA,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
    VALUE_POOL_BALANCE_HISTORY_SCHEMA,
    VALUE_POOL_FLOW_HISTORY_SCHEMA,
];
const BUNDLED_CHAIN_EVENT_CONSUMER_NAMES: &[DeriveConsumerName] = &[
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
    BLOCK_SUMMARY_CONSUMER_NAME,
    IRONWOOD_MIGRATION_CONSUMER_NAME,
    COMMITMENT_ROOT_SEARCH_CONSUMER_NAME,
    CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
    RECENT_TRANSACTIONS_CONSUMER_NAME,
    TRANSACTION_FEES_CONSUMER_NAME,
    TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME,
    TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
    VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
];
const BUNDLED_EVENT_ONLY_CHAIN_EVENT_CONSUMER_NAMES: &[DeriveConsumerName] =
    &[REORG_INCIDENTS_CONSUMER_NAME];

/// Per-column-family options the derive plane tunes at open time.
///
/// Adds `max_write_buffer_number = 2` on top of the canonical-store CF
/// defaults (one block-based table factory bound to the shared block
/// cache). Two write buffers let one rotate to immutable while another
/// keeps absorbing puts during compaction.
fn column_family_options(cache: &Cache, rocksdb_resource_budget: RocksDbResourceBudget) -> Options {
    let mut options = Options::default();
    options.set_block_based_table_factory(&build_block_based_table_factory(cache));
    options.set_write_buffer_size(
        usize::try_from(rocksdb_resource_budget.write_buffer_bytes).unwrap_or(usize::MAX),
    );
    options.set_max_write_buffer_number(rocksdb_resource_budget.max_write_buffer_count);
    options
}

/// Builds the open-time column-family descriptors.
///
/// Every store family and every declared consumer family is opened, plus any
/// column family already on disk that neither list covers. `RocksDB` refuses
/// to open a store while leaving an existing column family unlisted, so unknown
/// families must be opened before manifest validation can reject an undeclared
/// consumer without mutating it.
fn column_family_descriptors(
    cache: &Cache,
    rocksdb_resource_budget: RocksDbResourceBudget,
    consumers: &[DeriveConsumerSchema],
    existing_column_families: &[String],
) -> Vec<ColumnFamilyDescriptor> {
    let store_families = DeriveStoreTable::all().into_iter().map(|table| {
        ColumnFamilyDescriptor::new(
            table.column_family_name(),
            column_family_options(cache, rocksdb_resource_budget),
        )
    });
    let mut consumer_names = BTreeSet::<String>::new();
    for consumer in consumers {
        for &name in consumer.column_families {
            consumer_names.insert(name.to_owned());
        }
    }
    for name in existing_column_families {
        consumer_names.insert(name.clone());
    }
    consumer_names.remove(ROCKSDB_DEFAULT_COLUMN_FAMILY);
    for table in DeriveStoreTable::all() {
        consumer_names.remove(table.column_family_name());
    }
    let consumer_families = consumer_names.into_iter().map(|name| {
        ColumnFamilyDescriptor::new(name, column_family_options(cache, rocksdb_resource_budget))
    });
    store_families.chain(consumer_families).collect()
}

/// Lists the column families already present at `path`, or an empty list when
/// the path is not yet a `RocksDB` store.
fn existing_column_family_names(path: &Path) -> Vec<String> {
    DB::list_cf(&Options::default(), path).unwrap_or_default()
}

/// Rejects consumer declarations whose column families collide.
///
/// Every declared column family must be unique across consumers and must not
/// reuse a store-table name or the `RocksDB` default family. Reconciliation
/// drops a column family only when no declared consumer owns it, so a name
/// shared by two declarations would let one consumer's rebuild or removal wipe
/// another's rows behind a cursor that never rewinds. Rejecting at open time
/// keeps that impossible.
fn validate_consumer_declarations(
    consumers: &[DeriveConsumerSchema],
) -> Result<(), DeriveStoreError> {
    let mut declared = BTreeSet::<&'static str>::new();
    for consumer in consumers {
        for &name in consumer.column_families {
            let reserved = name == ROCKSDB_DEFAULT_COLUMN_FAMILY
                || DeriveStoreTable::all()
                    .iter()
                    .any(|table| table.column_family_name() == name);
            if reserved || !declared.insert(name) {
                return Err(DeriveStoreError::ConsumerColumnFamilyConflict { name });
            }
        }
    }
    Ok(())
}

/// Logical column-family identifier.
///
/// Mirrors `DeriveStoreColumnFamily` but lives on the public store surface
/// because callers reference column families when issuing reads. Operator
/// errors carry the same enum so the two halves stay in sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeriveStoreTable {
    /// `chain_event_cursor` column family: per-chain-consumer cursor persistence.
    ChainEventCursor,
    /// `mempool_event_cursor` column family: per-mempool-consumer cursor persistence.
    MempoolEventCursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl DeriveStoreTable {
    /// Returns the canonical `RocksDB` column-family name for the variant.
    #[must_use]
    pub const fn column_family_name(self) -> &'static str {
        match self {
            Self::ChainEventCursor => "chain_event_cursor",
            Self::MempoolEventCursor => "mempool_event_cursor",
            Self::ConsumerMetadata => "consumer_metadata",
        }
    }

    fn error_family(self) -> DeriveStoreColumnFamily {
        match self {
            Self::ChainEventCursor => DeriveStoreColumnFamily::ChainEventCursor,
            Self::MempoolEventCursor => DeriveStoreColumnFamily::MempoolEventCursor,
            Self::ConsumerMetadata => DeriveStoreColumnFamily::ConsumerMetadata,
        }
    }

    fn all() -> [Self; 3] {
        [
            Self::ChainEventCursor,
            Self::MempoolEventCursor,
            Self::ConsumerMetadata,
        ]
    }
}

/// Configurable knobs the binary applies before opening the database.
///
/// `rocksdb_resource_budget` ships with
/// [`RocksDbResourceBudget::derive_writer_defaults`].
#[derive(Clone, Copy, Debug)]
pub struct DeriveStoreOptions {
    /// When set, every write is flushed to the OS page cache before returning.
    /// Default `false` matches the canonical store's tunable so operators can
    /// trade durability for throughput in development environments.
    pub sync_writes: bool,
    /// Consumers to register at open time. Each declares its stable name, its
    /// schema version, and the column families it reads and writes through
    /// [`DeriveStore::consumer_column_family`]. On open the store reconciles
    /// each consumer's declared version against the persisted manifest,
    /// rebuilding only the consumers whose versions moved.
    pub consumers: &'static [DeriveConsumerSchema],
    /// Bounded `RocksDB` resource budget applied at open time.
    pub rocksdb_resource_budget: RocksDbResourceBudget,
}

impl Default for DeriveStoreOptions {
    fn default() -> Self {
        Self {
            sync_writes: false,
            consumers: &[],
            rocksdb_resource_budget: RocksDbResourceBudget::derive_writer_defaults(),
        }
    }
}

/// Owned `(key, payload)` pair returned by
/// [`DeriveStore::range_iterate_consumer`]. Both halves are RocksDB-owned
/// bytes copied out of the iterator's borrowed buffers.
pub type ConsumerEntry = (Vec<u8>, Vec<u8>);

/// Cursor entry observed by derive cursor readers.
///
/// Carries the raw cursor bytes and a copy of the consumer name the caller
/// queried with so callers can match cursors to their owning consumer when
/// processing batches of reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeriveCursorEntry {
    /// Consumer the cursor was persisted for.
    pub consumer: DeriveConsumerName,
    /// Opaque cursor bytes the consumer last persisted.
    pub cursor_bytes: Vec<u8>,
}

/// Inputs that bind a canonical chain event to one derive-store write.
#[derive(Clone, Copy)]
pub struct ChainEventDispatchInputs<'event> {
    /// Chain epoch the canonical commit just produced.
    pub chain_epoch: ChainEpoch,
    /// Post-commit event emitted by the canonical store.
    pub chain_event: &'event ChainEvent,
    /// Chain-event cursor emitted by the canonical store.
    pub chain_cursor: &'event [u8],
    /// Monotonic event sequence assigned by the canonical store.
    pub event_sequence: u64,
    /// Safe tip height observed at commit time.
    pub safe_tip_height: BlockHeight,
}

/// One verified contiguous range within a consumer projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsumerProjectionCoverage {
    /// First verified canonical height.
    pub complete_from_height: BlockHeight,
    /// Last verified canonical height.
    pub complete_through_height: BlockHeight,
    /// Canonical hash at [`Self::complete_through_height`].
    pub complete_through_hash: BlockHash,
}

/// Atomic read fence and optional verified coverage for one derive consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsumerProjectionState {
    /// Canonical epoch whose projection writes are visible.
    pub projection_epoch_id: ChainEpochId,
    /// Highest canonical height reflected by the projection.
    pub projection_tip_height: BlockHeight,
    /// Canonical hash at [`Self::projection_tip_height`].
    pub projection_tip_hash: BlockHash,
    /// Monotonic projection mutation and coverage revision.
    pub revision: u64,
    /// Verified contiguous coverage, when verification has started.
    pub coverage: Option<ConsumerProjectionCoverage>,
}

/// Consumers that participate in one chain-event derive-store write.
pub struct ChainEventDispatchConsumers<
    'block_slices,
    'block_consumers,
    'event_slices,
    'event_consumers,
> {
    /// Block-keyed consumers that observe committed block contexts.
    pub block_consumers: &'block_slices mut [&'block_consumers mut dyn BlockKeyedConsumer],
    /// Direct consumers that observe the chain-event envelope.
    pub event_consumers: &'event_slices mut [&'event_consumers mut dyn DeriveConsumer],
}

/// `RocksDB`-backed durable storage for the derive plane.
///
/// Operations are atomic at the `RocksDB` `WriteBatch` granularity. Cursor
/// writes always go in a single batch with the consumer's data writes so a
/// crash mid-write never advances the cursor without persisting the
/// underlying state.
#[derive(Clone)]
pub struct DeriveStore {
    db: Arc<DB>,
    sync_writes: bool,
    storage_path: PathBuf,
    consumers: &'static [DeriveConsumerSchema],
    rocksdb_resource_budget: RocksDbResourceBudget,
    is_secondary: bool,
    catch_up_barrier: Arc<RwLock<()>>,
    block_cache: Cache,
    write_buffer_manager: rust_rocksdb::WriteBufferManager,
    statistics: Arc<Options>,
    io_mode: RocksDbIoMode,
    resource_gauge_throttle: Arc<ResourceGaugeThrottle>,
}

/// Consistent read view over one `DeriveStore` sequence.
///
/// Every method reads through the same storage snapshot, so projection
/// metadata, consumer rows, bounds, and exact counts cannot observe different
/// commits during one request. The underlying storage handles remain private.
pub struct DeriveStoreReadSnapshot<'store> {
    store: &'store DeriveStore,
    consistency: DeriveReadConsistency<'store>,
}

enum DeriveReadConsistency<'store> {
    Primary(Snapshot<'store>),
    Secondary {
        _catch_up_guard: RwLockReadGuard<'store, ()>,
    },
}

impl DeriveStoreReadSnapshot<'_> {
    fn read_options(&self) -> ReadOptions {
        let mut options = ReadOptions::default();
        if let DeriveReadConsistency::Primary(snapshot) = &self.consistency {
            options.set_snapshot(snapshot);
        }
        options
    }

    /// Reads one consumer's projection fence and verified coverage.
    pub fn consumer_projection_state(
        &self,
        consumer: DeriveConsumerName,
    ) -> Result<Option<ConsumerProjectionState>, DeriveStoreError> {
        let column_family = self
            .store
            .column_family(DeriveStoreTable::ConsumerMetadata)?;
        let key = consumer_projection_state_key(consumer.as_str());
        self.store
            .db
            .get_cf_opt(&column_family, key, &self.read_options())
            .map_err(|source| DeriveStoreError::Operation {
                operation: "get",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })?
            .map(|payload| decode_consumer_projection_state(consumer, &payload))
            .transpose()
    }

    /// Reads a single value from a consumer-owned column family.
    pub fn get_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        self.store
            .db
            .get_cf_opt(&handle, key, &self.read_options())
            .map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "get",
                name: column_family,
                source,
            })
    }

    /// Reads the ingest plane's derive-status record from this snapshot.
    pub fn get_derive_status(&self) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let column_family = self
            .store
            .column_family(DeriveStoreTable::ConsumerMetadata)?;
        self.store
            .db
            .get_cf_opt(&column_family, DERIVE_STATUS_KEY, &self.read_options())
            .map_err(|source| DeriveStoreError::Operation {
                operation: "get",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Batch-reads consumer keys in input order from this snapshot.
    pub fn multi_get_consumer<K>(
        &self,
        column_family: &'static str,
        keys: &[K],
    ) -> Result<Vec<Option<Vec<u8>>>, DeriveStoreError>
    where
        K: AsRef<[u8]>,
    {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let handle = self.store.consumer_column_family(column_family)?;
        let inputs = keys
            .iter()
            .map(|key| (&handle, key.as_ref()))
            .collect::<Vec<_>>();
        self.store
            .db
            .multi_get_cf_opt(inputs, &self.read_options())
            .into_iter()
            .map(|outcome| {
                outcome.map_err(|source| DeriveStoreError::ConsumerOperation {
                    operation: "multi_get",
                    name: column_family,
                    source,
                })
            })
            .collect()
    }

    /// Returns at most `entries_cap` rows from an inclusive consumer-key range.
    pub fn range_iterate_consumer(
        &self,
        column_family: &'static str,
        start_key: &[u8],
        end_key_inclusive: &[u8],
        entries_cap: usize,
    ) -> Result<Vec<ConsumerEntry>, DeriveStoreError> {
        if entries_cap == 0 {
            return Ok(Vec::new());
        }
        let handle = self.store.consumer_column_family(column_family)?;
        let iterator = self.store.db.iterator_cf_opt(
            &handle,
            self.read_options(),
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        let mut entries = Vec::with_capacity(entries_cap.min(64));
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "range_iterate",
                name: column_family,
                source,
            })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            entries.push((key.to_vec(), payload.to_vec()));
            if entries.len() >= entries_cap {
                break;
            }
        }
        Ok(entries)
    }

    /// Returns the lexicographically first consumer key, if one exists.
    pub fn first_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        self.consumer_edge_key(column_family, IteratorMode::Start, "first_key")
    }

    /// Returns the lexicographically last consumer key, if one exists.
    pub fn last_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        self.consumer_edge_key(column_family, IteratorMode::End, "last_key")
    }

    /// Returns the last consumer entry visible in this snapshot.
    pub fn last_consumer_entry(
        &self,
        column_family: &'static str,
    ) -> Result<Option<ConsumerEntry>, DeriveStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut iterator =
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation: "last_entry",
            name: column_family,
            source,
        })?;
        Ok(Some((key.to_vec(), payload.to_vec())))
    }

    /// Returns the lowest height in a descending-height consumer keyspace.
    pub fn first_materialized_height_descending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, DeriveStoreError> {
        let key = self.last_consumer_key(column_family)?;
        decode_descending_height_prefix(key.as_deref(), column_family, "last")
    }

    /// Returns the highest height in a descending-height consumer keyspace.
    pub fn last_materialized_height_descending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, DeriveStoreError> {
        let key = self.first_consumer_key(column_family)?;
        decode_descending_height_prefix(key.as_deref(), column_family, "first")
    }

    /// Counts every row in a consumer-owned column family exactly.
    pub fn consumer_row_count(&self, column_family: &'static str) -> Result<u64, DeriveStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut row_count = 0_u64;
        for entry in
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::Start)
        {
            entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "count_rows",
                name: column_family,
                source,
            })?;
            row_count = row_count.saturating_add(1);
        }
        Ok(row_count)
    }

    /// Counts exactly the consumer rows accepted by `predicate`.
    pub fn count_consumer_rows_matching(
        &self,
        column_family: &'static str,
        mut predicate: impl FnMut(&[u8], &[u8]) -> Result<bool, String>,
    ) -> Result<u64, DeriveStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut matching_count = 0_u64;
        for entry in
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::Start)
        {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "count_matching_rows",
                name: column_family,
                source,
            })?;
            if predicate(&key, &payload).map_err(|reason| {
                DeriveStoreError::ConsumerPayloadDecode {
                    name: column_family,
                    reason,
                }
            })? {
                matching_count = matching_count.saturating_add(1);
            }
        }
        Ok(matching_count)
    }

    fn consumer_edge_key(
        &self,
        column_family: &'static str,
        mode: IteratorMode<'_>,
        operation: &'static str,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut iterator = self
            .store
            .db
            .iterator_cf_opt(&handle, self.read_options(), mode);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation,
            name: column_family,
            source,
        })?;
        Ok(Some(key.to_vec()))
    }
}

fn decode_descending_height_prefix(
    key: Option<&[u8]>,
    column_family: &'static str,
    edge: &'static str,
) -> Result<Option<BlockHeight>, DeriveStoreError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let prefix = key.get(..zinder_core::wire::HEIGHT_KEY_LEN).ok_or_else(|| {
        DeriveStoreError::Decode {
            column_family: DeriveStoreColumnFamily::ConsumerMetadata,
            reason: format!(
                "consumer column family `{column_family}` {edge} key is shorter than the descending-height prefix"
            ),
        }
    })?;
    zinder_core::wire::decode_height_key_descending(prefix)
        .map(Some)
        .map_err(|error| DeriveStoreError::Decode {
            column_family: DeriveStoreColumnFamily::ConsumerMetadata,
            reason: format!(
                "consumer column family `{column_family}` {edge} key descending-height prefix invalid: {error}"
            ),
        })
}

impl fmt::Debug for DeriveStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeriveStore")
            .field("db", &self.db)
            .field("sync_writes", &self.sync_writes)
            .field("storage_path", &self.storage_path)
            .field("consumers", &self.consumers)
            .field("rocksdb_resource_budget", &self.rocksdb_resource_budget)
            .field("is_secondary", &self.is_secondary)
            .field("catch_up_barrier", &self.catch_up_barrier)
            .field("io_mode", &self.io_mode)
            .field("block_cache_usage_bytes", &self.block_cache.get_usage())
            .field(
                "memtable_budget_bytes",
                &self.write_buffer_manager.get_buffer_size(),
            )
            .field(
                "memtable_budget_usage_bytes",
                &self.write_buffer_manager.get_usage(),
            )
            .finish_non_exhaustive()
    }
}

impl DeriveStore {
    /// Captures a consistent read view at the store's current sequence.
    #[must_use]
    pub fn read_snapshot(&self) -> DeriveStoreReadSnapshot<'_> {
        let consistency = if self.is_secondary {
            DeriveReadConsistency::Secondary {
                _catch_up_guard: self.catch_up_barrier.read(),
            }
        } else {
            DeriveReadConsistency::Primary(Snapshot::new(self.db.as_ref()))
        };
        DeriveStoreReadSnapshot {
            store: self,
            consistency,
        }
    }

    /// Returns the conventional derive-store path for a canonical-store
    /// path.
    ///
    /// Both writer and reader processes derive the path from the canonical
    /// store path via this helper so the operator only configures one
    /// `storage.path` per service. The convention nests the derive
    /// `RocksDB` files in a [`DERIVE_STORE_SUBDIR`] subdirectory of the
    /// canonical store path.
    #[must_use]
    pub fn path_for_canonical(canonical_path: &Path) -> PathBuf {
        canonical_path.join(DERIVE_STORE_SUBDIR)
    }

    /// Returns the schema declarations for the bundled derive-plane consumers.
    ///
    /// Pass this into [`DeriveStoreOptions::consumers`] to register every
    /// bundled consumer with its name, schema version, and owned column
    /// families in one call.
    #[must_use]
    pub const fn bundled_consumers() -> &'static [DeriveConsumerSchema] {
        BUNDLED_CONSUMERS
    }

    /// Returns the bundled chain-event consumer cursor names.
    #[must_use]
    pub const fn bundled_chain_event_consumer_names() -> &'static [DeriveConsumerName] {
        BUNDLED_CHAIN_EVENT_CONSUMER_NAMES
    }

    /// Returns bundled chain-event consumer cursor names that read only the
    /// event envelope and do not need committed block contexts.
    #[must_use]
    pub const fn bundled_event_only_chain_event_consumer_names() -> &'static [DeriveConsumerName] {
        BUNDLED_EVENT_ONLY_CHAIN_EVENT_CONSUMER_NAMES
    }

    /// Opens or creates a derive store at `path`.
    ///
    /// Rejects with [`DeriveStoreError::ConsumerColumnFamilyConflict`] when the
    /// declared consumers do not own disjoint column families. On a fresh path
    /// the store-format version is written immediately and the manifest is
    /// seeded with every declared consumer's version. A persisted store-format
    /// version older than [`DERIVE_STORE_FORMAT_VERSION`] deletes the whole
    /// derive directory and reopens it fresh, so the rebuild leaves no
    /// column-family drop edits in the `RocksDB` manifest for a secondary
    /// reader to replay; a newer persisted version is rejected with
    /// [`DeriveStoreError::SchemaMismatch`] so an older binary never wipes a
    /// store it cannot read. Each consumer is then reconciled: a consumer whose
    /// declared version moved has its cursor reset and its column families
    /// rebuilt while every other consumer is left untouched.
    pub fn open(
        path: impl AsRef<Path>,
        options: DeriveStoreOptions,
    ) -> Result<Self, DeriveStoreError> {
        let path = path.as_ref();
        options
            .rocksdb_resource_budget
            .validate()
            .map_err(|reason| DeriveStoreError::InvalidOptions { reason })?;
        validate_consumer_declarations(options.consumers)?;
        Self::wipe_derive_directory_if_store_format_superseded(path, &options)?;
        let existing_column_families = existing_column_family_names(path);
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary { path },
            options.rocksdb_resource_budget,
            |cache, rocksdb_resource_budget| {
                column_family_descriptors(
                    cache,
                    rocksdb_resource_budget,
                    options.consumers,
                    &existing_column_families,
                )
            },
        )
        .map_err(|source| DeriveStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let store = Self {
            db: Arc::new(bounded_open.db),
            sync_writes: options.sync_writes,
            storage_path: path.to_path_buf(),
            consumers: options.consumers,
            rocksdb_resource_budget: options.rocksdb_resource_budget,
            is_secondary: false,
            catch_up_barrier: Arc::new(RwLock::new(())),
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
        };
        store.validate_or_initialize_store_format_version()?;
        store.reconcile_consumer_schemas()?;
        store.record_rocksdb_properties();
        Ok(store)
    }

    /// Opens the derive store in `RocksDB` secondary mode.
    ///
    /// `primary_path` is the same directory the writer process opened with
    /// [`Self::open`]; `secondary_path` is a per-reader scratch directory
    /// `RocksDB` uses for the secondary instance's bookkeeping (MANIFEST
    /// tail, current view metadata). Multiple readers may open the same
    /// primary path concurrently as long as each provides a distinct
    /// secondary path.
    ///
    /// A secondary instance only catches up with the primary when
    /// [`Self::try_catch_up`] is called; reads observe the snapshot from
    /// the last successful catchup.
    ///
    /// A secondary reader cannot reconcile schemas, so it validates instead:
    /// the persisted container version must equal
    /// [`DERIVE_STORE_FORMAT_VERSION`], and every declared consumer's version
    /// must match the persisted manifest. A divergence returns
    /// [`DeriveStoreError::SchemaMismatch`] or
    /// [`DeriveStoreError::ConsumerSchemaMismatch`]; the caller retries the
    /// open once the primary has reconciled and rewritten the manifest.
    pub fn open_secondary(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        options: DeriveStoreOptions,
    ) -> Result<Self, DeriveStoreError> {
        let primary_path = primary_path.as_ref();
        let secondary_path = secondary_path.as_ref();
        options
            .rocksdb_resource_budget
            .validate()
            .map_err(|reason| DeriveStoreError::InvalidOptions { reason })?;
        validate_consumer_declarations(options.consumers)?;
        let existing_column_families = existing_column_family_names(primary_path);
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Secondary {
                primary_path,
                secondary_path,
            },
            options.rocksdb_resource_budget,
            |cache, rocksdb_resource_budget| {
                column_family_descriptors(
                    cache,
                    rocksdb_resource_budget,
                    options.consumers,
                    &existing_column_families,
                )
            },
        )
        .map_err(|source| DeriveStoreError::Open {
            path: primary_path.to_path_buf(),
            source,
        })?;
        let store = Self {
            db: Arc::new(bounded_open.db),
            sync_writes: options.sync_writes,
            storage_path: primary_path.to_path_buf(),
            consumers: options.consumers,
            rocksdb_resource_budget: options.rocksdb_resource_budget,
            is_secondary: true,
            catch_up_barrier: Arc::new(RwLock::new(())),
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
        };
        store.require_matching_store_format_version()?;
        store.validate_secondary_consumer_schemas()?;
        store.record_rocksdb_properties();
        Ok(store)
    }

    /// Advances the secondary instance's view to the primary's latest
    /// durable state.
    ///
    /// No-op on a primary instance: `RocksDB` returns immediately because
    /// there is no upstream MANIFEST to tail.
    pub fn try_catch_up(&self) -> Result<(), DeriveStoreError> {
        if !self.is_secondary {
            return Ok(());
        }
        let _catch_up_guard = self.catch_up_barrier.write();
        let mut attempt = 1;
        loop {
            let outcome = self.try_catch_up_once();
            if outcome
                .as_ref()
                .is_err_and(is_transient_secondary_missing_sst)
                && attempt < SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS
            {
                metrics::counter!("zinder_derive_secondary_catchup_retries_total").increment(1);
                tracing::debug!(
                    target: "zinder::derive",
                    event = "secondary_catchup_missing_sst_retry",
                    attempt,
                    max_attempts = SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS,
                    "derive secondary crossed a primary-compaction file race; retrying catchup"
                );
                std::thread::yield_now();
                attempt += 1;
                continue;
            }
            return outcome;
        }
    }

    fn try_catch_up_once(&self) -> Result<(), DeriveStoreError> {
        self.db
            .try_catch_up_with_primary()
            .map_err(|source| DeriveStoreError::Operation {
                operation: "try_catch_up_with_primary",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })?;
        self.require_matching_store_format_version()?;
        self.validate_secondary_consumer_schemas()
    }

    /// Flushes the write-ahead log to disk so every prior write is durable.
    ///
    /// The derive store opens with `sync_writes: false`, so writes land in the
    /// unsynced WAL. Before the canonical retention release floor vouches that
    /// projection rows up to a height are durable, the writer fsyncs the WAL so
    /// a host crash cannot leave the floor (and the canonical deletes it
    /// authorizes) ahead of the projection rows they depend on. No-op on a
    /// secondary, which owns no WAL of its own.
    pub fn flush_wal_to_disk(&self) -> Result<(), DeriveStoreError> {
        if self.is_secondary {
            return Ok(());
        }
        self.db
            .flush_wal(true)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "flush_wal",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Creates a `RocksDB` checkpoint for backup or fixture capture.
    ///
    /// The checkpoint must be taken from a primary derive store. Secondary
    /// readers may intentionally lag the primary, so checkpointing one would
    /// produce a stale restore image with a cursor that does not represent the
    /// writer's durable state.
    pub fn create_checkpoint(&self, path: impl AsRef<Path>) -> Result<(), DeriveStoreError> {
        if self.is_secondary {
            return Err(DeriveStoreError::CheckpointRequiresPrimary {
                path: self.storage_path.clone(),
            });
        }
        let checkpoint =
            Checkpoint::new(self.db.as_ref()).map_err(|source| DeriveStoreError::Checkpoint {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        checkpoint
            .create_checkpoint(path.as_ref())
            .map_err(|source| DeriveStoreError::Checkpoint {
                path: path.as_ref().to_path_buf(),
                source,
            })
    }

    /// Returns the filesystem path the store opened from.
    #[must_use]
    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }

    /// Returns the filesystem I/O mode resolved when opening this store.
    #[must_use]
    pub const fn rocksdb_io_mode(&self) -> RocksDbIoMode {
        self.io_mode
    }

    /// Returns the current shared block-cache usage in bytes.
    #[must_use]
    pub fn block_cache_usage_bytes(&self) -> usize {
        self.block_cache.get_usage()
    }

    /// Returns the configured total memtable budget in bytes.
    #[must_use]
    pub fn memtable_budget_bytes(&self) -> usize {
        self.write_buffer_manager.get_buffer_size()
    }

    /// Returns the current write-buffer-manager memory usage in bytes.
    #[must_use]
    pub fn memtable_budget_usage_bytes(&self) -> usize {
        self.write_buffer_manager.get_usage()
    }

    /// Returns true when this store was opened with at least one
    /// consumer-owned column family.
    ///
    /// The ingest writer uses this to skip derive dispatch for narrowly
    /// scoped tests that exercise canonical storage with synthetic block
    /// bytes and no derive consumers. Production opens the store through
    /// the ingest/explorer column-family list, so real deployments always
    /// dispatch.
    #[must_use]
    pub fn has_consumer_column_families(&self) -> bool {
        self.consumers
            .iter()
            .any(|schema| !schema.column_families.is_empty())
    }

    /// Dispatches chain-event consumers and atomically writes their rows plus
    /// cursor advances.
    ///
    /// `blocks` contains the already-parsed committed block contexts for this
    /// event. Callers own context construction because only the ingest writer
    /// has the canonical commit batch and prevout read path in scope.
    pub fn write_chain_event<S>(
        &self,
        consumers: &mut [&mut dyn BlockKeyedConsumer],
        inputs: ChainEventDispatchInputs<'_>,
        blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
    ) -> Result<(), DeriveError>
    where
        S: BuildHasher,
    {
        let mut event_consumers: [&mut dyn DeriveConsumer; 0] = [];
        self.write_chain_event_chunk_with_event_consumers(
            ChainEventDispatchConsumers {
                block_consumers: consumers,
                event_consumers: &mut event_consumers,
            },
            inputs,
            blocks,
            true,
        )
    }

    /// Dispatches one replay chunk and optionally advances chain-event
    /// cursors.
    ///
    /// Replay callers use this when a retained canonical event is too large
    /// to hydrate as one in-memory unit. Intermediate chunks persist
    /// deterministic consumer rows without advancing the cursor; the final
    /// chunk advances all consumer cursors in the same write batch as its
    /// rows. If the process exits between chunks, replay restarts from the
    /// canonical event and overwrites the already-materialized rows.
    pub fn write_chain_event_chunk<S>(
        &self,
        consumers: &mut [&mut dyn BlockKeyedConsumer],
        inputs: ChainEventDispatchInputs<'_>,
        blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
        advance_cursor: bool,
    ) -> Result<(), DeriveError>
    where
        S: BuildHasher,
    {
        let mut event_consumers: [&mut dyn DeriveConsumer; 0] = [];
        self.write_chain_event_chunk_with_event_consumers(
            ChainEventDispatchConsumers {
                block_consumers: consumers,
                event_consumers: &mut event_consumers,
            },
            inputs,
            blocks,
            advance_cursor,
        )
    }

    /// Dispatches one replay chunk to block-keyed and direct chain-event
    /// consumers, optionally advancing chain-event cursors.
    ///
    /// Direct consumers observe the event envelope itself rather than the
    /// committed block contexts. They share the same write batch and cursor
    /// advance as block-keyed consumers, so a crash never records an incident
    /// without advancing its replay cursor or vice versa.
    pub fn write_chain_event_chunk_with_event_consumers<S>(
        &self,
        consumers: ChainEventDispatchConsumers<'_, '_, '_, '_>,
        inputs: ChainEventDispatchInputs<'_>,
        blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
        advance_cursor: bool,
    ) -> Result<(), DeriveError>
    where
        S: BuildHasher,
    {
        let ChainEventDispatchConsumers {
            block_consumers,
            event_consumers,
        } = consumers;
        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store: self,
            batch: &mut batch,
        };
        let projection_checkpoint = block_projection_checkpoint(inputs, blocks);

        for consumer in block_consumers.iter_mut() {
            consumer
                .begin_batch(&mut ctx)
                .map_err(DeriveError::Consumer)?;
            dispatch_chain_event_to_block_consumer(&mut **consumer, inputs, &mut ctx, blocks)?;
            consumer
                .finish_batch(&mut ctx)
                .map_err(DeriveError::Consumer)?;
            consumer
                .stage_chain_event_checkpoint(projection_checkpoint, &mut ctx)
                .map_err(DeriveError::Consumer)?;
        }
        for consumer in event_consumers.iter_mut() {
            dispatch_chain_event_to_consumer(&mut **consumer, inputs, &mut ctx)?;
        }

        if advance_cursor {
            self.stage_chain_event_cursor_advances(
                &mut batch,
                block_consumers,
                inputs.chain_cursor,
            )?;
            self.stage_event_chain_event_cursor_advances(
                &mut batch,
                event_consumers,
                inputs.chain_cursor,
            )?;
        }
        self.write_batch(&batch)?;
        Ok(())
    }

    /// Dispatches one mempool consumer and atomically writes its rows plus
    /// cursor advance.
    pub fn write_mempool_event(
        &self,
        consumer: &mut dyn DeriveMempoolConsumer,
        event: &crate::consumer::MempoolConsumerEvent<'_>,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveError> {
        let mut batch = WriteBatch::default();
        let mut ctx = DeriveConsumerCtx {
            store: self,
            batch: &mut batch,
        };
        consumer
            .apply_mempool_event(event, &mut ctx)
            .map_err(DeriveError::Consumer)?;
        self.stage_mempool_event_cursor_advance(&mut batch, consumer.name(), cursor_bytes)?;
        self.write_batch(&batch)?;
        Ok(())
    }

    /// Reads a chain-event consumer's persisted cursor bytes, when present.
    pub fn get_chain_event_cursor(
        &self,
        consumer: DeriveConsumerName,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        self.get(
            DeriveStoreTable::ChainEventCursor,
            consumer.as_str().as_bytes(),
        )
    }

    /// Atomically persists `cursor_bytes` for a chain-event consumer.
    ///
    /// Each call commits its own `WriteBatch`. Consumers that need to bundle
    /// cursor advances with their own data writes use [`Self::write_batch`]
    /// instead.
    pub fn put_chain_event_cursor(
        &self,
        consumer: DeriveConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let mut batch = WriteBatch::default();
        self.stage_chain_event_cursor(&mut batch, consumer, cursor_bytes)?;
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put_chain_event_cursor",
                column_family: DeriveStoreColumnFamily::ChainEventCursor,
                source,
            })
    }

    /// Stages one chain-event cursor in a caller-owned atomic write batch.
    ///
    /// Snapshot-backed consumers use this to activate materialized state and
    /// adopt the event boundary in the same commit.
    pub fn stage_chain_event_cursor(
        &self,
        batch: &mut WriteBatch,
        consumer: DeriveConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let column_family = self.column_family(DeriveStoreTable::ChainEventCursor)?;
        batch.put_cf(&column_family, consumer.as_str().as_bytes(), cursor_bytes);
        Ok(())
    }

    /// Atomically persists `cursor_bytes` for a mempool-event consumer.
    pub fn put_mempool_event_cursor(
        &self,
        consumer: DeriveConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let mut batch = WriteBatch::default();
        let column_family = self.column_family(DeriveStoreTable::MempoolEventCursor)?;
        batch.put_cf(&column_family, consumer.as_str().as_bytes(), cursor_bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put_mempool_event_cursor",
                column_family: DeriveStoreColumnFamily::MempoolEventCursor,
                source,
            })
    }

    /// Returns the persisted store-format version recorded under
    /// `consumer_metadata`.
    pub fn store_format_version(&self) -> Result<u16, DeriveStoreError> {
        let Some(bytes) = self.get(DeriveStoreTable::ConsumerMetadata, STORE_FORMAT_VERSION_KEY)?
        else {
            return Err(DeriveStoreError::SchemaMismatch {
                persisted: 0,
                running: DERIVE_STORE_FORMAT_VERSION,
            });
        };
        decode_store_format_version(&bytes).map_err(|reason| DeriveStoreError::Decode {
            column_family: DeriveStoreColumnFamily::ConsumerMetadata,
            reason,
        })
    }

    /// Commits a prepared `WriteBatch` to the database.
    ///
    /// Consumers use this to bundle a cursor write together with their own
    /// data writes so the persisted cursor never advances without the
    /// underlying state having reached durability.
    pub fn write_batch(&self, batch: &WriteBatch) -> Result<(), DeriveStoreError> {
        self.write(batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "write_batch",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Returns a column-family handle the caller can use when staging puts
    /// directly into a [`WriteBatch`].
    pub fn column_family(
        &self,
        table: DeriveStoreTable,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, DeriveStoreError> {
        self.db
            .cf_handle(table.column_family_name())
            .ok_or_else(|| DeriveStoreError::ColumnFamilyMissing {
                column_family: table.error_family(),
            })
    }

    /// Returns a handle for a consumer-owned column family registered through
    /// [`DeriveStoreOptions::consumers`]. Consumers stage puts and deletes by
    /// calling `batch.put_cf(handle, key, value)` on the returned handle and
    /// committing through [`Self::write_batch`].
    pub fn consumer_column_family(
        &self,
        name: &'static str,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, DeriveStoreError> {
        if !self.owns_consumer_column_family(name) {
            return Err(DeriveStoreError::ConsumerColumnFamilyMissing { name });
        }
        self.db
            .cf_handle(name)
            .ok_or(DeriveStoreError::ConsumerColumnFamilyMissing { name })
    }

    fn owns_consumer_column_family(&self, name: &str) -> bool {
        self.consumers
            .iter()
            .any(|schema| schema.column_families.contains(&name))
    }

    /// Reads a single value from a consumer-owned column family.
    pub fn get_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        self.db
            .get_cf(&handle, key)
            .map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "get",
                name: column_family,
                source,
            })
    }

    /// Writes a single value into a consumer-owned column family.
    pub fn put_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
        bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&handle, key, bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "put",
                name: column_family,
                source,
            })
    }

    /// Persists the ingest plane's derive-status record so the explorer plane
    /// can surface it on `ServerInfo`. Opaque bytes by design: the store stays
    /// free of the explorer wire types, matching how consumer payloads are
    /// handled. See [`Self::get_derive_status`].
    pub fn put_derive_status(&self, bytes: &[u8]) -> Result<(), DeriveStoreError> {
        self.put(DeriveStoreTable::ConsumerMetadata, DERIVE_STATUS_KEY, bytes)
    }

    /// Reads the derive-status record the ingest plane writes each replay
    /// tick, or `None` when ingest has not written one yet.
    pub fn get_derive_status(&self) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        self.get(DeriveStoreTable::ConsumerMetadata, DERIVE_STATUS_KEY)
    }

    /// Reads one consumer's atomic projection fence and verified coverage.
    pub fn consumer_projection_state(
        &self,
        consumer: DeriveConsumerName,
    ) -> Result<Option<ConsumerProjectionState>, DeriveStoreError> {
        let key = consumer_projection_state_key(consumer.as_str());
        self.get(DeriveStoreTable::ConsumerMetadata, &key)?
            .map(|payload| decode_consumer_projection_state(consumer, &payload))
            .transpose()
    }

    /// Stages one consumer's projection state in a caller-owned atomic batch.
    pub fn stage_consumer_projection_state(
        &self,
        batch: &mut WriteBatch,
        consumer: DeriveConsumerName,
        state: ConsumerProjectionState,
    ) -> Result<(), DeriveStoreError> {
        validate_projection_coverage_bounds(consumer, &state)?;
        let column_family = self.column_family(DeriveStoreTable::ConsumerMetadata)?;
        batch.put_cf(
            &column_family,
            consumer_projection_state_key(consumer.as_str()),
            encode_consumer_projection_state(state),
        );
        Ok(())
    }

    /// Atomically persists one consumer's projection state.
    pub fn put_consumer_projection_state(
        &self,
        consumer: DeriveConsumerName,
        state: ConsumerProjectionState,
    ) -> Result<(), DeriveStoreError> {
        let mut batch = WriteBatch::default();
        self.stage_consumer_projection_state(&mut batch, consumer, state)?;
        self.write_batch(&batch)
    }

    /// Batch-reads multiple keys from a consumer-owned column family.
    ///
    /// Returns one entry per input key in input order: `None` when the key
    /// has no value. Issues one `multi_get_cf` so an N-key page costs one
    /// round-trip into `RocksDB` rather than N point lookups.
    pub fn multi_get_consumer<K>(
        &self,
        column_family: &'static str,
        keys: &[K],
    ) -> Result<Vec<Option<Vec<u8>>>, DeriveStoreError>
    where
        K: AsRef<[u8]>,
    {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let handle = self.consumer_column_family(column_family)?;
        let inputs = keys
            .iter()
            .map(|key| (&handle, key.as_ref()))
            .collect::<Vec<_>>();
        let mut out = Vec::with_capacity(keys.len());
        for outcome in self.db.multi_get_cf(inputs) {
            let bytes = outcome.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "multi_get",
                name: column_family,
                source,
            })?;
            out.push(bytes);
        }
        Ok(out)
    }

    /// Iterates a consumer-owned column family, returning at most
    /// `entries_cap` entries whose keys lie in `[start_key, end_key_inclusive]`
    /// in ascending order. The helper short-circuits the scan as soon as
    /// the cap is reached so memory is bounded by the cap rather than the
    /// size of the prefix.
    pub fn range_iterate_consumer(
        &self,
        column_family: &'static str,
        start_key: &[u8],
        end_key_inclusive: &[u8],
        entries_cap: usize,
    ) -> Result<Vec<ConsumerEntry>, DeriveStoreError> {
        if entries_cap == 0 {
            return Ok(Vec::new());
        }
        let handle = self.consumer_column_family(column_family)?;
        let iterator = self.db.iterator_cf(
            &handle,
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        let mut entries = Vec::with_capacity(entries_cap.min(64));
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "range_iterate",
                name: column_family,
                source,
            })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            entries.push((key.to_vec(), payload.to_vec()));
            if entries.len() >= entries_cap {
                break;
            }
        }
        Ok(entries)
    }

    /// Reads a bounded page from an inclusive consumer-key range.
    ///
    /// Rows before `offset` are scanned but never retained, so legacy offset
    /// pagination cannot allocate in proportion to an untrusted offset.
    pub fn page_consumer_range(
        &self,
        column_family: &'static str,
        key_range: std::ops::RangeInclusive<&[u8]>,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<ConsumerEntry>, DeriveStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let handle = self.consumer_column_family(column_family)?;
        let (start_key, end_key_inclusive) = key_range.into_inner();
        let iterator = self.db.iterator_cf(
            &handle,
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        let mut skipped = 0_u64;
        let mut entries = Vec::with_capacity(limit.min(64));
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "page_range",
                name: column_family,
                source,
            })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            if skipped < offset {
                skipped = skipped.saturating_add(1);
                continue;
            }
            entries.push((key.to_vec(), payload.to_vec()));
            if entries.len() == limit {
                break;
            }
        }
        Ok(entries)
    }

    /// Counts the rows in one consumer-owned column family without copying
    /// or decoding their payloads.
    pub fn consumer_row_count(&self, column_family: &'static str) -> Result<u64, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut row_count = 0_u64;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "count_rows",
                name: column_family,
                source,
            })?;
            row_count = row_count.saturating_add(1);
        }
        Ok(row_count)
    }

    /// Visits every row in one consumer-owned column family without copying
    /// the table into an intermediate collection.
    ///
    /// The visitor borrows each key and payload directly from the `RocksDB`
    /// iterator. Returning an error fails the scan closed as an invalid
    /// consumer row.
    pub fn visit_consumer_rows(
        &self,
        column_family: &'static str,
        mut visitor: impl FnMut(&[u8], &[u8]) -> Result<(), String>,
    ) -> Result<(), DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "visit_rows",
                name: column_family,
                source,
            })?;
            visitor(&key, &payload).map_err(|reason| DeriveStoreError::ConsumerPayloadDecode {
                name: column_family,
                reason,
            })?;
        }
        Ok(())
    }

    /// Visits every row in an inclusive consumer-key range without copying
    /// the range into an intermediate collection.
    ///
    /// The visitor borrows each key and payload directly from the `RocksDB`
    /// iterator. Returning an error fails the scan closed as an invalid
    /// consumer row.
    pub fn visit_consumer_range(
        &self,
        column_family: &'static str,
        key_range: std::ops::RangeInclusive<&[u8]>,
        mut visitor: impl FnMut(&[u8], &[u8]) -> Result<(), String>,
    ) -> Result<(), DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let (start_key, end_key_inclusive) = key_range.into_inner();
        let iterator = self.db.iterator_cf(
            &handle,
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "visit_range",
                name: column_family,
                source,
            })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            visitor(&key, &payload).map_err(|reason| DeriveStoreError::ConsumerPayloadDecode {
                name: column_family,
                reason,
            })?;
        }
        Ok(())
    }

    /// Counts consumer rows accepted by `predicate` without copying payloads.
    ///
    /// The predicate receives each key and payload directly from the storage
    /// iterator. Returning an error fails the scan closed as an invalid
    /// consumer payload.
    pub fn count_consumer_rows_matching(
        &self,
        column_family: &'static str,
        mut predicate: impl FnMut(&[u8], &[u8]) -> Result<bool, String>,
    ) -> Result<u64, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut matching_count = 0_u64;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
                operation: "count_matching_rows",
                name: column_family,
                source,
            })?;
            if predicate(&key, &payload).map_err(|reason| {
                DeriveStoreError::ConsumerPayloadDecode {
                    name: column_family,
                    reason,
                }
            })? {
                matching_count = matching_count.saturating_add(1);
            }
        }
        Ok(matching_count)
    }

    /// Returns the lexicographically last key in a consumer-owned column
    /// family, or `None` when the column family is empty.
    ///
    /// Uses `RocksDB`'s reverse iterator (`IteratorMode::End`) so the lookup
    /// is bounded by one seek plus one block read regardless of how many
    /// entries the column family holds. Callers that need the "highest"
    /// height-keyed materialized record use this instead of a full-table
    /// scan to compute derive-cursor lag at request time.
    pub fn last_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation: "last_key",
            name: column_family,
            source,
        })?;
        Ok(Some(key.to_vec()))
    }

    /// Returns the lexicographically last `(key, value)` entry in a
    /// consumer-owned column family, or `None` when it is empty.
    ///
    /// Like [`Self::last_consumer_key`] but also returns the payload, so a
    /// caller that needs the newest materialized record (the indexed head)
    /// reads it in one reverse-iterator step instead of a key lookup followed
    /// by a point get.
    pub fn last_consumer_entry(
        &self,
        column_family: &'static str,
    ) -> Result<Option<ConsumerEntry>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation: "last_entry",
            name: column_family,
            source,
        })?;
        Ok(Some((key.to_vec(), payload.to_vec())))
    }

    /// Returns the highest height materialized in an ascending-height
    /// derive column family, or `None` when the column family is empty.
    ///
    /// Decodes the lexicographically last key as a four-byte big-endian
    /// height via [`zinder_core::wire::decode_height_key_ascending`]. Use
    /// this on column families whose primary key is exactly four bytes of
    /// ascending height (the `BlockSummary` projection). Returns
    /// [`DeriveStoreError::Decode`] when the last key is not four bytes,
    /// which signals a column-family schema mismatch and should fail loudly.
    pub fn last_materialized_height_ascending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, DeriveStoreError> {
        let Some(key) = self.last_consumer_key(column_family)? else {
            return Ok(None);
        };
        zinder_core::wire::decode_height_key_ascending(&key)
            .map(Some)
            .map_err(|error| DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` last key is not a 4-byte ascending height: {error}"
                ),
            })
    }

    /// Returns the highest height materialized in a descending-height
    /// derive column family, or `None` when the column family is empty.
    ///
    /// Descending-keyed column families lay the newest block at the start
    /// of the lexicographic range, so the highest materialized height is
    /// the *first* key, not the last. This helper reads the first key via
    /// the reverse iterator's complement direction so the lookup remains
    /// bounded by one seek. Inverts the encoding from
    /// [`zinder_core::wire::decode_height_key_descending`] for callers that
    /// key on `(reverse_height, ...)` composites by inspecting the leading
    /// four bytes.
    pub fn last_materialized_height_descending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, DeriveStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::Start);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) = entry.map_err(|source| DeriveStoreError::ConsumerOperation {
            operation: "first_key",
            name: column_family,
            source,
        })?;
        let prefix = key.get(..zinder_core::wire::HEIGHT_KEY_LEN).ok_or_else(|| {
            DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` first key is shorter than the descending-height prefix"
                ),
            }
        })?;
        zinder_core::wire::decode_height_key_descending(prefix)
            .map(Some)
            .map_err(|error| DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` first key descending-height prefix invalid: {error}"
                ),
            })
    }

    fn stage_chain_event_cursor_advances(
        &self,
        batch: &mut WriteBatch,
        consumers: &[&mut dyn BlockKeyedConsumer],
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let cf = self.column_family(DeriveStoreTable::ChainEventCursor)?;
        for consumer in consumers {
            batch.put_cf(&cf, consumer.name().as_str().as_bytes(), cursor_bytes);
        }
        Ok(())
    }

    fn stage_event_chain_event_cursor_advances(
        &self,
        batch: &mut WriteBatch,
        consumers: &[&mut dyn DeriveConsumer],
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let cf = self.column_family(DeriveStoreTable::ChainEventCursor)?;
        for consumer in consumers {
            batch.put_cf(&cf, consumer.name().as_str().as_bytes(), cursor_bytes);
        }
        Ok(())
    }

    fn stage_mempool_event_cursor_advance(
        &self,
        batch: &mut WriteBatch,
        consumer_name: DeriveConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let cf = self.column_family(DeriveStoreTable::MempoolEventCursor)?;
        batch.put_cf(&cf, consumer_name.as_str().as_bytes(), cursor_bytes);
        Ok(())
    }

    /// Fails unless the persisted container version equals the running one.
    ///
    /// Secondary readers cannot initialize or migrate, so they reject a
    /// divergent container version instead of writing to the store.
    fn require_matching_store_format_version(&self) -> Result<(), DeriveStoreError> {
        let persisted = self.store_format_version()?;
        if persisted == DERIVE_STORE_FORMAT_VERSION {
            Ok(())
        } else {
            Err(DeriveStoreError::SchemaMismatch {
                persisted,
                running: DERIVE_STORE_FORMAT_VERSION,
            })
        }
    }

    /// Rejects a secondary open whose consumer declaration cannot safely read
    /// the persisted manifest.
    ///
    /// A secondary reader cannot rebuild a consumer's column families. It may
    /// read an exact schema match or an explicitly row-compatible older
    /// version with the same column-family set; every other mismatch waits for
    /// the primary to reconcile first.
    fn validate_secondary_consumer_schemas(&self) -> Result<(), DeriveStoreError> {
        let recorded = self.read_consumer_manifest()?;
        self.reject_undeclared_recorded_consumers(&recorded)?;
        for consumer in self.consumers {
            let entry = recorded.get(consumer.name.as_str());
            let compatible = entry.is_some_and(|entry| {
                Self::consumer_manifest_is_exact(consumer, entry)
                    || Self::consumer_manifest_is_row_compatible(consumer, entry)
            });
            if !compatible {
                return Err(DeriveStoreError::ConsumerSchemaMismatch {
                    consumer: consumer.name.as_str(),
                    persisted: entry.map(|entry| entry.schema_version),
                    running: consumer.schema_version,
                });
            }
        }
        Ok(())
    }

    /// Deletes the derive directory when its persisted container version is
    /// older than the running binary, so the reopened store starts fresh.
    ///
    /// A container-format change invalidates every consumer's rows at once. The
    /// whole store is wiped by deleting the directory rather than dropping
    /// column families in place: an in-place drop records column-family drop
    /// edits in the `RocksDB` manifest, and a secondary reader replaying those
    /// edits during catch-up crashes. A persisted version newer than the
    /// running one is left untouched and surfaces later as
    /// [`DeriveStoreError::SchemaMismatch`], so rolling a binary back never
    /// destroys a store it cannot read.
    fn wipe_derive_directory_if_store_format_superseded(
        path: &Path,
        options: &DeriveStoreOptions,
    ) -> Result<(), DeriveStoreError> {
        let Some(persisted) = Self::peek_store_format_version(path, options)? else {
            return Ok(());
        };
        if persisted >= DERIVE_STORE_FORMAT_VERSION {
            return Ok(());
        }
        tracing::warn!(
            target: "zinder::derive",
            event = "store_format_rebuild",
            from_store_format_version = persisted,
            to_store_format_version = DERIVE_STORE_FORMAT_VERSION,
            "derive store container format changed; deleting the derive directory and rebuilding the whole store"
        );
        std::fs::remove_dir_all(path).map_err(|source| DeriveStoreError::SchemaReconcile {
            operation: "store_format_wipe",
            reason: format!("{}: {source}", path.display()),
        })
    }

    /// Reads the persisted container version without keeping the store open.
    ///
    /// Returns `None` when `path` holds no derive store yet. The store is
    /// opened as a primary, read, and closed before the caller decides whether
    /// to wipe or open it for real.
    fn peek_store_format_version(
        path: &Path,
        options: &DeriveStoreOptions,
    ) -> Result<Option<u16>, DeriveStoreError> {
        let existing_column_families = existing_column_family_names(path);
        if existing_column_families.is_empty() {
            return Ok(None);
        }
        let bounded_open = open_bounded_rocksdb(
            RocksDbOpenRole::Primary { path },
            options.rocksdb_resource_budget,
            |cache, rocksdb_resource_budget| {
                column_family_descriptors(
                    cache,
                    rocksdb_resource_budget,
                    options.consumers,
                    &existing_column_families,
                )
            },
        )
        .map_err(|source| DeriveStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let column_family = bounded_open
            .db
            .cf_handle(DeriveStoreTable::ConsumerMetadata.column_family_name())
            .ok_or(DeriveStoreError::ColumnFamilyMissing {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
            })?;
        let persisted = bounded_open
            .db
            .get_cf(&column_family, STORE_FORMAT_VERSION_KEY)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "get",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })?
            .map(|bytes| decode_store_format_version(&bytes))
            .transpose()
            .map_err(|reason| DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason,
            })?;
        drop(column_family);
        drop(bounded_open);
        Ok(persisted)
    }

    fn validate_or_initialize_store_format_version(&self) -> Result<(), DeriveStoreError> {
        let Some(bytes) = self.get(DeriveStoreTable::ConsumerMetadata, STORE_FORMAT_VERSION_KEY)?
        else {
            return self.put(
                DeriveStoreTable::ConsumerMetadata,
                STORE_FORMAT_VERSION_KEY,
                &DERIVE_STORE_FORMAT_VERSION.to_be_bytes(),
            );
        };
        let persisted =
            decode_store_format_version(&bytes).map_err(|reason| DeriveStoreError::Decode {
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                reason,
            })?;
        if persisted == DERIVE_STORE_FORMAT_VERSION {
            Ok(())
        } else {
            Err(DeriveStoreError::SchemaMismatch {
                persisted,
                running: DERIVE_STORE_FORMAT_VERSION,
            })
        }
    }

    /// Reconciles each declared consumer's schema version against the
    /// persisted manifest.
    ///
    /// An exact match keeps its column families and cursor. An explicitly
    /// row-compatible older version with the same column-family set is adopted
    /// by advancing the manifest while retaining every row version still
    /// present. Every other older version has its rows cleared and cursor
    /// reset. A newer persisted version or undeclared recorded consumer fails
    /// closed so an older binary cannot destroy rows it does not understand. A
    /// newly declared consumer has its column
    /// families cleared and is then recorded at its declared version, so a
    /// family that previously belonged to another consumer starts empty rather
    /// than serving the prior owner's rows behind a fresh cursor.
    /// Reconciliation never drops a column family in place: a
    /// range-tombstone clear replays safely on an attached secondary, while a
    /// `drop_cf`/`create_cf` edit crashes a secondary mid-catchup. An emptied
    /// orphan family is reclaimed physically only when a container-format
    /// change wipes the whole derive directory.
    fn reconcile_consumer_schemas(&self) -> Result<(), DeriveStoreError> {
        let recorded = self.read_consumer_manifest()?;
        self.reject_undeclared_recorded_consumers(&recorded)?;
        self.reject_newer_consumer_schemas(&recorded)?;
        for consumer in self.consumers {
            match recorded.get(consumer.name.as_str()) {
                Some(entry) if Self::consumer_manifest_is_exact(consumer, entry) => {}
                Some(entry) if Self::consumer_manifest_is_row_compatible(consumer, entry) => {
                    self.adopt_row_compatible_consumer(consumer, entry)?;
                }
                Some(entry)
                    if entry.schema_version < consumer.schema_version
                        && Self::consumer_column_families_match(consumer, entry)
                        && !consumer.row_compatible_versions.is_empty() =>
                {
                    return Err(DeriveStoreError::ConsumerSchemaMismatch {
                        consumer: consumer.name.as_str(),
                        persisted: Some(entry.schema_version),
                        running: consumer.schema_version,
                    });
                }
                Some(entry) => self.rebuild_consumer(consumer, entry)?,
                None => self.initialize_new_consumer(consumer)?,
            }
        }
        Ok(())
    }

    fn reject_undeclared_recorded_consumers(
        &self,
        recorded: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), DeriveStoreError> {
        for (name, entry) in recorded {
            if self
                .consumers
                .iter()
                .all(|consumer| consumer.name.as_str() != name)
            {
                return Err(DeriveStoreError::ConsumerNotDeclared {
                    consumer: name.clone(),
                    persisted_schema_version: entry.schema_version,
                });
            }
        }
        Ok(())
    }

    fn reject_newer_consumer_schemas(
        &self,
        recorded: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), DeriveStoreError> {
        for consumer in self.consumers {
            let Some(entry) = recorded.get(consumer.name.as_str()) else {
                continue;
            };
            if entry.schema_version > consumer.schema_version {
                return Err(DeriveStoreError::ConsumerSchemaMismatch {
                    consumer: consumer.name.as_str(),
                    persisted: Some(entry.schema_version),
                    running: consumer.schema_version,
                });
            }
        }
        Ok(())
    }

    fn consumer_manifest_is_exact(
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> bool {
        recorded.schema_version == consumer.schema_version
            && Self::consumer_column_families_match(consumer, recorded)
            && Self::consumer_supports_recorded_row_versions(consumer, recorded)
    }

    fn consumer_manifest_is_row_compatible(
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> bool {
        recorded.schema_version < consumer.schema_version
            && Self::consumer_column_families_match(consumer, recorded)
            && Self::consumer_supports_recorded_row_versions(consumer, recorded)
    }

    fn consumer_column_families_match(
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> bool {
        let declared: BTreeSet<&str> = consumer.column_families.iter().copied().collect();
        let persisted: BTreeSet<&str> = recorded
            .column_families
            .iter()
            .map(String::as_str)
            .collect();
        consumer.column_families.len() == recorded.column_families.len() && declared == persisted
    }

    fn consumer_supports_recorded_row_versions(
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> bool {
        recorded.row_schema_versions.iter().all(|version| {
            *version == consumer.schema_version
                || consumer.row_compatible_versions.contains(version)
        })
    }

    fn adopt_row_compatible_consumer(
        &self,
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> Result<(), DeriveStoreError> {
        let mut row_schema_versions = recorded.row_schema_versions.clone();
        row_schema_versions.insert(consumer.schema_version);
        tracing::info!(
            target: "zinder::derive",
            event = "consumer_schema_rows_preserved",
            consumer = consumer.name.as_str(),
            from_schema_version = recorded.schema_version,
            to_schema_version = consumer.schema_version,
            "derive consumer schema version moved compatibly; preserving its rows and cursor"
        );
        self.write_consumer_manifest_entry_with_row_versions(consumer, &row_schema_versions)
    }

    fn rebuild_consumer(
        &self,
        consumer: &DeriveConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> Result<(), DeriveStoreError> {
        tracing::warn!(
            target: "zinder::derive",
            event = "consumer_schema_rebuild",
            consumer = consumer.name.as_str(),
            from_schema_version = recorded.schema_version,
            to_schema_version = consumer.schema_version,
            "derive consumer schema version moved; resetting its cursor and clearing its column families"
        );
        self.reset_consumer_cursors(consumer.name.as_str())?;
        for column_family in &recorded.column_families {
            if consumer.column_families.contains(&column_family.as_str()) {
                continue;
            }
            self.clear_consumer_column_family(column_family)?;
        }
        for column_family in consumer.column_families {
            self.clear_consumer_column_family(column_family)?;
        }
        self.write_consumer_manifest_entry(consumer)
    }

    /// Clears a newly declared consumer's column families before recording it,
    /// so a family that previously belonged to another consumer starts empty
    /// and replays from the earliest retained event.
    fn initialize_new_consumer(
        &self,
        consumer: &DeriveConsumerSchema,
    ) -> Result<(), DeriveStoreError> {
        for column_family in consumer.column_families {
            self.clear_consumer_column_family(column_family)?;
        }
        self.write_consumer_manifest_entry(consumer)
    }

    fn read_consumer_manifest(
        &self,
    ) -> Result<BTreeMap<String, ConsumerManifestEntry>, DeriveStoreError> {
        let column_family = self.column_family(DeriveStoreTable::ConsumerMetadata)?;
        let iterator = self.db.iterator_cf(
            &column_family,
            IteratorMode::From(CONSUMER_SCHEMA_KEY_PREFIX, rust_rocksdb::Direction::Forward),
        );
        let mut manifest = BTreeMap::new();
        for entry in iterator {
            let (key, payload) = entry.map_err(|source| DeriveStoreError::Operation {
                operation: "read_manifest",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })?;
            let Some(name_bytes) = key.strip_prefix(CONSUMER_SCHEMA_KEY_PREFIX) else {
                break;
            };
            let name = String::from_utf8(name_bytes.to_vec()).map_err(|error| {
                DeriveStoreError::SchemaReconcile {
                    operation: "decode_manifest_name",
                    reason: error.to_string(),
                }
            })?;
            let decoded = decode_manifest_entry(&payload).map_err(|reason| {
                DeriveStoreError::SchemaReconcile {
                    operation: "decode_manifest_entry",
                    reason,
                }
            })?;
            manifest.insert(name, decoded);
        }
        Ok(manifest)
    }

    /// Clears every row in a consumer-owned column family without dropping the
    /// family itself.
    ///
    /// A range tombstone over the full key span, plus a point-delete sweep of
    /// any residue at or above the range's exclusive upper bound, leaves the
    /// family indistinguishable from a freshly created one. The family is never
    /// dropped: a `drop_cf`/`create_cf` edit records a column-family change in
    /// the `RocksDB` manifest, and a secondary reader replaying that edit during
    /// catch-up crashes; range tombstones and point deletes replay as ordinary
    /// data writes.
    fn clear_consumer_column_family(&self, name: &str) -> Result<(), DeriveStoreError> {
        let Some(handle) = self.db.cf_handle(name) else {
            return Ok(());
        };
        let mut batch = WriteBatch::default();
        batch.delete_range_cf(&handle, CLEAR_RANGE_LOWER_BOUND, CLEAR_RANGE_UPPER_BOUND);
        let residue = self.db.iterator_cf(
            &handle,
            IteratorMode::From(CLEAR_RANGE_UPPER_BOUND, rust_rocksdb::Direction::Forward),
        );
        for entry in residue {
            let (key, _payload) = entry.map_err(|source| DeriveStoreError::SchemaReconcile {
                operation: "clear_consumer_column_family",
                reason: format!("{name}: {source}"),
            })?;
            batch.delete_cf(&handle, &key);
        }
        self.write_batch(&batch)
    }

    fn reset_consumer_cursors(&self, name: &str) -> Result<(), DeriveStoreError> {
        let chain_cf = self.column_family(DeriveStoreTable::ChainEventCursor)?;
        let mempool_cf = self.column_family(DeriveStoreTable::MempoolEventCursor)?;
        let metadata_cf = self.column_family(DeriveStoreTable::ConsumerMetadata)?;
        let mut batch = WriteBatch::default();
        batch.delete_cf(&chain_cf, name.as_bytes());
        batch.delete_cf(&mempool_cf, name.as_bytes());
        batch.delete_cf(&metadata_cf, consumer_projection_state_key(name));
        self.write_batch(&batch)
    }

    fn write_consumer_manifest_entry(
        &self,
        consumer: &DeriveConsumerSchema,
    ) -> Result<(), DeriveStoreError> {
        self.write_consumer_manifest_entry_with_row_versions(
            consumer,
            &BTreeSet::from([consumer.schema_version]),
        )
    }

    fn write_consumer_manifest_entry_with_row_versions(
        &self,
        consumer: &DeriveConsumerSchema,
        row_schema_versions: &BTreeSet<u16>,
    ) -> Result<(), DeriveStoreError> {
        let key = consumer_schema_manifest_key(consumer.name.as_str());
        let payload = encode_manifest_entry(
            consumer.schema_version,
            consumer.column_families,
            row_schema_versions,
        )
        .map_err(|reason| DeriveStoreError::SchemaReconcile {
            operation: "encode_manifest_entry",
            reason,
        })?;
        self.put(DeriveStoreTable::ConsumerMetadata, &key, &payload)
    }

    fn get(
        &self,
        table: DeriveStoreTable,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, DeriveStoreError> {
        let column_family = self.column_family(table)?;
        self.db
            .get_cf(&column_family, key)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "get",
                column_family: table.error_family(),
                source,
            })
    }

    fn put(
        &self,
        table: DeriveStoreTable,
        key: &[u8],
        bytes: &[u8],
    ) -> Result<(), DeriveStoreError> {
        let column_family = self.column_family(table)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&column_family, key, bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put",
                column_family: table.error_family(),
                source,
            })
    }

    fn write(&self, batch: &WriteBatch) -> Result<(), rust_rocksdb::Error> {
        let mut write_options = WriteOptions::default();
        write_options.set_sync(self.sync_writes);
        self.db.write_opt(batch, &write_options)?;
        if self.resource_gauge_throttle.should_sample() {
            self.record_rocksdb_properties();
        }
        Ok(())
    }

    fn record_rocksdb_properties(&self) {
        let mut column_family_names = DeriveStoreTable::all()
            .into_iter()
            .map(DeriveStoreTable::column_family_name)
            .collect::<Vec<_>>();
        for consumer in self.consumers {
            column_family_names.extend_from_slice(consumer.column_families);
        }
        let store_role = if self.is_secondary {
            StoreRole::DeriveSecondary
        } else {
            StoreRole::DerivePrimary
        };
        record_rocksdb_resource_gauges(&RocksDbResourceGaugeInputs {
            db: &self.db,
            store_role,
            column_family_names: &column_family_names,
            block_cache: &self.block_cache,
            write_buffer_manager: &self.write_buffer_manager,
            statistics: &self.statistics,
            io_mode: self.io_mode,
            resource_budget: self.rocksdb_resource_budget,
        });
    }
}

fn dispatch_chain_event_to_block_consumer<C, S>(
    consumer: &mut C,
    inputs: ChainEventDispatchInputs<'_>,
    ctx: &mut DeriveConsumerCtx<'_>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
) -> Result<(), DeriveError>
where
    C: BlockKeyedConsumer + ?Sized,
    S: BuildHasher,
{
    match inputs.chain_event {
        ChainEvent::ChainCommitted { committed } => {
            let event = ChainCommittedEvent::new(
                inputs.event_sequence,
                inputs.chain_epoch,
                inputs.safe_tip_height,
                committed.block_range.start,
                committed.block_range.end,
            );
            apply_chain_committed_in_memory(consumer, &event, ctx, blocks)
                .map_err(DeriveError::Consumer)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            let event = ChainReorgedEvent::new(
                inputs.event_sequence,
                inputs.chain_epoch,
                inputs.safe_tip_height,
                RevertedRange::new(
                    reverted.chain_epoch,
                    reverted.block_range.start,
                    reverted.block_range.end,
                ),
                CommittedRange::new(
                    committed.chain_epoch,
                    committed.block_range.start,
                    committed.block_range.end,
                ),
            );
            apply_chain_reorged_in_memory(consumer, &event, ctx, blocks)
                .map_err(DeriveError::Consumer)
        }
        _ => Err(DeriveError::UnsupportedChainEvent),
    }
}

fn block_projection_checkpoint<'event, S>(
    inputs: ChainEventDispatchInputs<'event>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
) -> BlockProjectionCheckpoint<'event>
where
    S: BuildHasher,
{
    let projected_range = match inputs.chain_event {
        ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. } => {
            Some(committed.block_range)
        }
        _ => None,
    };
    let projected_tip = projected_range.and_then(|range| {
        if range.start > range.end {
            return Some((
                inputs.chain_epoch.visible_tip_height,
                inputs.chain_epoch.visible_tip_hash,
            ));
        }
        if !range.into_iter().all(|height| blocks.contains_key(&height)) {
            return None;
        }
        blocks
            .get(&range.end)
            .map(|block| (block.height, block.block_hash))
    });
    BlockProjectionCheckpoint {
        chain_epoch: inputs.chain_epoch,
        chain_event: inputs.chain_event,
        projection_tip_height: projected_tip.map(|(height, _hash)| height),
        projection_tip_hash: projected_tip.map(|(_height, hash)| hash),
    }
}

fn dispatch_chain_event_to_consumer<C>(
    consumer: &mut C,
    inputs: ChainEventDispatchInputs<'_>,
    ctx: &mut DeriveConsumerCtx<'_>,
) -> Result<(), DeriveError>
where
    C: DeriveConsumer + ?Sized,
{
    match inputs.chain_event {
        ChainEvent::ChainCommitted { committed } => {
            let event = ChainCommittedEvent::new(
                inputs.event_sequence,
                inputs.chain_epoch,
                inputs.safe_tip_height,
                committed.block_range.start,
                committed.block_range.end,
            );
            consumer
                .apply_chain_committed(&event, ctx)
                .map_err(DeriveError::Consumer)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            let event = ChainReorgedEvent::new(
                inputs.event_sequence,
                inputs.chain_epoch,
                inputs.safe_tip_height,
                RevertedRange::new(
                    reverted.chain_epoch,
                    reverted.block_range.start,
                    reverted.block_range.end,
                ),
                CommittedRange::new(
                    committed.chain_epoch,
                    committed.block_range.start,
                    committed.block_range.end,
                ),
            );
            consumer
                .apply_chain_reorged(&event, ctx)
                .map_err(DeriveError::Consumer)
        }
        _ => Err(DeriveError::UnsupportedChainEvent),
    }
}

fn decode_store_format_version(bytes: &[u8]) -> Result<u16, String> {
    let array: [u8; 2] = bytes
        .try_into()
        .map_err(|_| format!("store format version requires 2 bytes; got {}", bytes.len()))?;
    Ok(u16::from_be_bytes(array))
}

/// One consumer's persisted schema manifest row: its declared version and the
/// column families it owned when the row was written.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsumerManifestEntry {
    schema_version: u16,
    column_families: Vec<String>,
    row_schema_versions: BTreeSet<u16>,
}

fn consumer_schema_manifest_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(CONSUMER_SCHEMA_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(CONSUMER_SCHEMA_KEY_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

fn consumer_projection_state_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(CONSUMER_PROJECTION_STATE_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(CONSUMER_PROJECTION_STATE_KEY_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

fn projection_coverage_bounds_valid(
    coverage: ConsumerProjectionCoverage,
    projection_tip_height: BlockHeight,
) -> bool {
    let ordered = coverage.complete_from_height <= coverage.complete_through_height;
    let within_tip = coverage.complete_through_height <= projection_tip_height;
    ordered && within_tip
}

fn validate_projection_coverage_bounds(
    consumer: DeriveConsumerName,
    state: &ConsumerProjectionState,
) -> Result<(), DeriveStoreError> {
    let Some(coverage) = state.coverage else {
        return Ok(());
    };
    if projection_coverage_bounds_valid(coverage, state.projection_tip_height) {
        return Ok(());
    }
    Err(DeriveStoreError::InvalidProjectionCoverage {
        consumer: consumer.as_str(),
        complete_from_height: coverage.complete_from_height.value(),
        complete_through_height: coverage.complete_through_height.value(),
        projection_tip_height: state.projection_tip_height.value(),
    })
}

fn encode_consumer_projection_state(
    state: ConsumerProjectionState,
) -> [u8; CONSUMER_PROJECTION_STATE_LEN] {
    let mut payload = [0_u8; CONSUMER_PROJECTION_STATE_LEN];
    let mut offset = 0;
    payload[offset] = CONSUMER_PROJECTION_STATE_VERSION;
    offset += 1;
    payload[offset..offset + 8].copy_from_slice(&state.projection_epoch_id.value().to_be_bytes());
    offset += 8;
    payload[offset..offset + 4].copy_from_slice(&state.projection_tip_height.value().to_be_bytes());
    offset += 4;
    payload[offset..offset + 32].copy_from_slice(&state.projection_tip_hash.as_bytes());
    offset += 32;
    payload[offset..offset + 8].copy_from_slice(&state.revision.to_be_bytes());
    offset += 8;
    if let Some(coverage) = state.coverage {
        payload[offset] = 1;
        offset += 1;
        payload[offset..offset + 4]
            .copy_from_slice(&coverage.complete_from_height.value().to_be_bytes());
        offset += 4;
        payload[offset..offset + 4]
            .copy_from_slice(&coverage.complete_through_height.value().to_be_bytes());
        offset += 4;
        payload[offset..offset + 32].copy_from_slice(&coverage.complete_through_hash.as_bytes());
    }
    payload
}

fn decode_consumer_projection_state(
    consumer: DeriveConsumerName,
    payload: &[u8],
) -> Result<ConsumerProjectionState, DeriveStoreError> {
    let bytes: [u8; CONSUMER_PROJECTION_STATE_LEN] = payload.try_into().map_err(|_| {
        projection_state_decode_error("consumer projection state length is invalid")
    })?;
    if bytes[0] != CONSUMER_PROJECTION_STATE_VERSION {
        return Err(projection_state_decode_error(
            "consumer projection state version is unsupported",
        ));
    }
    let projection_epoch_id =
        ChainEpochId::new(u64::from_be_bytes(bytes[1..9].try_into().map_err(
            |_| projection_state_decode_error("projection epoch is malformed"),
        )?));
    let projection_tip_height =
        BlockHeight::new(u32::from_be_bytes(bytes[9..13].try_into().map_err(
            |_| projection_state_decode_error("projection tip height is malformed"),
        )?));
    let projection_tip_hash = BlockHash::from_bytes(
        bytes[13..45]
            .try_into()
            .map_err(|_| projection_state_decode_error("projection tip hash is malformed"))?,
    );
    let revision = u64::from_be_bytes(
        bytes[45..53]
            .try_into()
            .map_err(|_| projection_state_decode_error("projection revision is malformed"))?,
    );
    let coverage =
        match bytes[53] {
            0 => None,
            1 => Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(u32::from_be_bytes(
                    bytes[54..58].try_into().map_err(|_| {
                        projection_state_decode_error("coverage start height is malformed")
                    })?,
                )),
                complete_through_height: BlockHeight::new(u32::from_be_bytes(
                    bytes[58..62].try_into().map_err(|_| {
                        projection_state_decode_error("coverage end height is malformed")
                    })?,
                )),
                complete_through_hash: BlockHash::from_bytes(bytes[62..94].try_into().map_err(
                    |_| projection_state_decode_error("coverage end hash is malformed"),
                )?),
            }),
            _ => {
                return Err(projection_state_decode_error(
                    "consumer projection coverage presence is invalid",
                ));
            }
        };
    let coverage = match coverage {
        Some(coverage) if !projection_coverage_bounds_valid(coverage, projection_tip_height) => {
            tracing::warn!(
                consumer = consumer.as_str(),
                complete_from_height = coverage.complete_from_height.value(),
                complete_through_height = coverage.complete_through_height.value(),
                projection_tip_height = projection_tip_height.value(),
                "dropping consumer projection coverage with invalid bounds; the consumer re-derives its coverage"
            );
            None
        }
        other => other,
    };
    Ok(ConsumerProjectionState {
        projection_epoch_id,
        projection_tip_height,
        projection_tip_hash,
        revision,
        coverage,
    })
}

fn projection_state_decode_error(reason: &'static str) -> DeriveStoreError {
    DeriveStoreError::Decode {
        column_family: DeriveStoreColumnFamily::ConsumerMetadata,
        reason: reason.to_owned(),
    }
}

fn encode_manifest_entry(
    schema_version: u16,
    column_families: &[&str],
    row_schema_versions: &BTreeSet<u16>,
) -> Result<Vec<u8>, String> {
    let count = u16::try_from(column_families.len()).map_err(|_| {
        format!(
            "consumer declares {} column families; the manifest holds at most {}",
            column_families.len(),
            u16::MAX
        )
    })?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&schema_version.to_be_bytes());
    bytes.extend_from_slice(&count.to_be_bytes());
    for name in column_families {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len()).map_err(|_| {
            format!(
                "consumer column family name is {} bytes; the manifest holds at most {}",
                name_bytes.len(),
                u16::MAX
            )
        })?;
        bytes.extend_from_slice(&name_len.to_be_bytes());
        bytes.extend_from_slice(name_bytes);
    }
    let row_version_count = u16::try_from(row_schema_versions.len()).map_err(|_| {
        format!(
            "consumer has {} row schema versions; the manifest holds at most {}",
            row_schema_versions.len(),
            u16::MAX
        )
    })?;
    bytes.extend_from_slice(&row_version_count.to_be_bytes());
    for version in row_schema_versions {
        bytes.extend_from_slice(&version.to_be_bytes());
    }
    Ok(bytes)
}

fn decode_manifest_entry(bytes: &[u8]) -> Result<ConsumerManifestEntry, String> {
    let schema_version = read_manifest_u16(bytes, 0)?;
    let count = read_manifest_u16(bytes, 2)?;
    let mut offset = 4usize;
    let mut column_families = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let name_len = usize::from(read_manifest_u16(bytes, offset)?);
        offset += 2;
        let end = offset
            .checked_add(name_len)
            .ok_or_else(|| "consumer manifest entry length overflow".to_owned())?;
        let name_bytes = bytes
            .get(offset..end)
            .ok_or_else(|| "consumer manifest entry truncated".to_owned())?;
        column_families
            .push(String::from_utf8(name_bytes.to_vec()).map_err(|error| error.to_string())?);
        offset = end;
    }
    let row_schema_versions = if offset == bytes.len() {
        BTreeSet::from([schema_version])
    } else {
        let row_version_count = read_manifest_u16(bytes, offset)?;
        offset += 2;
        let mut versions = BTreeSet::new();
        for _ in 0..row_version_count {
            let version = read_manifest_u16(bytes, offset)?;
            if !versions.insert(version) {
                return Err("consumer manifest has duplicate row schema versions".to_owned());
            }
            offset += 2;
        }
        if offset != bytes.len() {
            return Err("consumer manifest entry has trailing bytes".to_owned());
        }
        if versions.is_empty() || versions.iter().any(|version| *version > schema_version) {
            return Err("consumer manifest row schema versions are invalid".to_owned());
        }
        versions
    };
    Ok(ConsumerManifestEntry {
        schema_version,
        column_families,
        row_schema_versions,
    })
}

fn read_manifest_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| "consumer manifest offset overflow".to_owned())?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "consumer manifest entry truncated".to_owned())?;
    let array: [u8; 2] = slice
        .try_into()
        .map_err(|_| "consumer manifest u16 slice".to_owned())?;
    Ok(u16::from_be_bytes(array))
}

fn is_transient_secondary_missing_sst(error: &DeriveStoreError) -> bool {
    let Some(source) = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<rust_rocksdb::Error>())
    else {
        return false;
    };
    is_missing_sst_error(&source.kind(), source.as_ref())
}

fn is_missing_sst_error(kind: &rust_rocksdb::ErrorKind, message: &str) -> bool {
    matches!(
        kind,
        rust_rocksdb::ErrorKind::IOError | rust_rocksdb::ErrorKind::NotFound
    ) && message.contains("No such file or directory")
        && message.contains(".sst")
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use eyre::Result;
    use tempfile::tempdir;

    use super::*;

    const TEST_CONSUMER: DeriveConsumerName = DeriveConsumerName::from_static("test_consumer");
    const TEST_CONSUMER_CF: &str = "test_cf";
    const TEST_CONSUMER_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
        DeriveConsumerName::from_static("test_cf_consumer"),
        1,
        &[TEST_CONSUMER_CF],
    );

    #[test]
    fn secondary_catchup_retries_only_missing_sst_file_races() {
        assert!(is_missing_sst_error(
            &rust_rocksdb::ErrorKind::IOError,
            "IO error: No such file or directory: /derive/199308.sst"
        ));
        assert!(!is_missing_sst_error(
            &rust_rocksdb::ErrorKind::IOError,
            "IO error: No such file or directory: /derive/MANIFEST-000123"
        ));
        assert!(!is_missing_sst_error(
            &rust_rocksdb::ErrorKind::Corruption,
            "Corruption: No such file or directory: /derive/199308.sst"
        ));
    }

    #[test]
    fn reorg_incidents_cursor_is_event_only() {
        assert!(
            !DeriveStore::bundled_chain_event_consumer_names()
                .contains(&REORG_INCIDENTS_CONSUMER_NAME)
        );
        assert!(
            DeriveStore::bundled_event_only_chain_event_consumer_names()
                .contains(&REORG_INCIDENTS_CONSUMER_NAME)
        );
    }

    #[test]
    fn paid_fee_schema_is_bundled_before_ingest_dispatch_is_wired() {
        assert!(DeriveStore::bundled_consumers().contains(&PAID_FEE_DISTRIBUTION_SCHEMA));
        assert!(
            !DeriveStore::bundled_chain_event_consumer_names()
                .contains(&PAID_FEE_DISTRIBUTION_SCHEMA.name)
        );
    }

    #[test]
    fn opening_a_fresh_store_writes_the_store_format_version() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        assert_eq!(store.store_format_version()?, DERIVE_STORE_FORMAT_VERSION);
        Ok(())
    }

    #[test]
    fn cursor_round_trip_persists_and_retrieves_bytes() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
        assert!(store.get_chain_event_cursor(TEST_CONSUMER)?.is_none());
        store.put_chain_event_cursor(TEST_CONSUMER, &[1, 2, 3])?;
        assert_eq!(
            store.get_chain_event_cursor(TEST_CONSUMER)?,
            Some(vec![1, 2, 3])
        );
        store.put_chain_event_cursor(TEST_CONSUMER, &[4, 5])?;
        assert_eq!(
            store.get_chain_event_cursor(TEST_CONSUMER)?,
            Some(vec![4, 5])
        );
        Ok(())
    }

    #[test]
    fn checkpoint_preserves_cursor_rows() -> Result<()> {
        let tempdir = tempdir()?;
        let source_path = tempdir.path().join("derive-source");
        let checkpoint_path = tempdir.path().join("derive-checkpoint");
        {
            let store = DeriveStore::open(&source_path, DeriveStoreOptions::default())?;
            store.put_chain_event_cursor(TEST_CONSUMER, &[4, 5, 6])?;
            store.create_checkpoint(&checkpoint_path)?;
        }

        let checkpoint = DeriveStore::open(&checkpoint_path, DeriveStoreOptions::default())?;
        assert_eq!(
            checkpoint.get_chain_event_cursor(TEST_CONSUMER)?,
            Some(vec![4, 5, 6])
        );
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_none_for_empty_column_family() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        assert_eq!(store.last_consumer_key("test_cf")?, None);
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_lexicographically_last_key() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        let handle = store.consumer_column_family("test_cf")?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&handle, 1_u32.to_be_bytes(), b"a");
        batch.put_cf(&handle, 42_u32.to_be_bytes(), b"b");
        batch.put_cf(&handle, 7_u32.to_be_bytes(), b"c");
        drop(handle);
        store.write_batch(&batch)?;
        assert_eq!(
            store.last_consumer_key("test_cf")?,
            Some(42_u32.to_be_bytes().to_vec())
        );
        Ok(())
    }

    fn assert_snapshot_point_reads(
        snapshot: &DeriveStoreReadSnapshot<'_>,
        initial_state: ConsumerProjectionState,
        height_10_key: [u8; 4],
        height_20_key: [u8; 4],
        height_30_key: [u8; 4],
    ) -> Result<()> {
        assert_eq!(
            snapshot.consumer_projection_state(TEST_CONSUMER)?,
            Some(initial_state)
        );
        assert_eq!(
            snapshot.get_consumer(TEST_CONSUMER_CF, &height_20_key)?,
            Some(b"match-before".to_vec())
        );
        assert_eq!(
            snapshot.multi_get_consumer(
                TEST_CONSUMER_CF,
                &[height_10_key, height_20_key, height_30_key],
            )?,
            vec![Some(b"skip".to_vec()), Some(b"match-before".to_vec()), None,]
        );
        Ok(())
    }

    fn assert_snapshot_scan_reads(
        snapshot: &DeriveStoreReadSnapshot<'_>,
        height_10_key: [u8; 4],
        height_20_key: [u8; 4],
    ) -> Result<()> {
        assert_eq!(
            snapshot.range_iterate_consumer(
                TEST_CONSUMER_CF,
                &height_20_key,
                &height_10_key,
                usize::MAX,
            )?,
            vec![
                (height_20_key.to_vec(), b"match-before".to_vec()),
                (height_10_key.to_vec(), b"skip".to_vec()),
            ]
        );
        assert_eq!(
            snapshot.first_consumer_key(TEST_CONSUMER_CF)?,
            Some(height_20_key.to_vec())
        );
        assert_eq!(
            snapshot.last_consumer_key(TEST_CONSUMER_CF)?,
            Some(height_10_key.to_vec())
        );
        assert_eq!(
            snapshot.last_consumer_entry(TEST_CONSUMER_CF)?,
            Some((height_10_key.to_vec(), b"skip".to_vec()))
        );
        assert_eq!(
            snapshot.first_materialized_height_descending(TEST_CONSUMER_CF)?,
            Some(BlockHeight::new(10))
        );
        assert_eq!(
            snapshot.last_materialized_height_descending(TEST_CONSUMER_CF)?,
            Some(BlockHeight::new(20))
        );
        assert_eq!(snapshot.consumer_row_count(TEST_CONSUMER_CF)?, 2);
        assert_eq!(
            snapshot.count_consumer_rows_matching(TEST_CONSUMER_CF, |_key, payload| {
                Ok(payload.starts_with(b"match"))
            })?,
            1
        );
        Ok(())
    }

    #[test]
    fn read_snapshot_keeps_all_consumer_reads_on_one_sequence() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[TEST_CONSUMER_SCHEMA],
                ..DeriveStoreOptions::default()
            },
        )?;
        let height_5_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(5));
        let height_10_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(10));
        let height_20_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(20));
        let height_30_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(30));
        store.put_consumer(TEST_CONSUMER_CF, &height_10_key, b"skip")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_20_key, b"match-before")?;
        store.put_derive_status(b"status-before")?;
        let initial_state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(1),
            projection_tip_height: BlockHeight::new(20),
            projection_tip_hash: BlockHash::from_bytes([0x20; 32]),
            revision: 1,
            coverage: None,
        };
        store.put_consumer_projection_state(TEST_CONSUMER, initial_state)?;

        let snapshot = store.read_snapshot();

        let advanced_state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(1),
            projection_tip_height: BlockHeight::new(30),
            projection_tip_hash: BlockHash::from_bytes([0x30; 32]),
            revision: 2,
            coverage: None,
        };
        store.put_consumer(TEST_CONSUMER_CF, &height_5_key, b"match-after")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_20_key, b"match-after")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_30_key, b"match-after")?;
        store.put_consumer_projection_state(TEST_CONSUMER, advanced_state)?;
        store.put_derive_status(b"status-after")?;

        assert_eq!(
            store.get_consumer(TEST_CONSUMER_CF, &height_20_key)?,
            Some(b"match-after".to_vec())
        );
        assert_eq!(store.consumer_row_count(TEST_CONSUMER_CF)?, 4);
        assert_eq!(
            store.consumer_projection_state(TEST_CONSUMER)?,
            Some(advanced_state)
        );
        assert_eq!(store.get_derive_status()?, Some(b"status-after".to_vec()));
        assert_eq!(
            snapshot.get_derive_status()?,
            Some(b"status-before".to_vec())
        );

        assert_snapshot_point_reads(
            &snapshot,
            initial_state,
            height_10_key,
            height_20_key,
            height_30_key,
        )?;
        assert_snapshot_scan_reads(&snapshot, height_10_key, height_20_key)?;
        drop(snapshot);
        Ok(())
    }

    #[test]
    fn secondary_read_snapshot_blocks_catch_up_until_reads_finish() -> Result<()> {
        let primary_directory = tempdir()?;
        let secondary_directory = tempdir()?;
        let options = DeriveStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..DeriveStoreOptions::default()
        };
        let primary = DeriveStore::open(primary_directory.path(), options)?;
        primary.put_consumer(TEST_CONSUMER_CF, b"before", b"visible")?;
        let secondary = DeriveStore::open_secondary(
            primary_directory.path(),
            secondary_directory.path(),
            options,
        )?;
        let snapshot = secondary.read_snapshot();
        primary.put_consumer(TEST_CONSUMER_CF, b"after", b"new")?;

        let (started_sender, started_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let catch_up_store = secondary.clone();
        let catch_up = thread::spawn(move || {
            let _ = started_sender.send(());
            let outcome = catch_up_store.try_catch_up().is_ok();
            let _ = finished_sender.send(outcome);
        });
        started_receiver.recv_timeout(Duration::from_secs(1))?;
        assert!(finished_receiver.try_recv().is_err());
        assert_eq!(snapshot.get_consumer(TEST_CONSUMER_CF, b"after")?, None);

        drop(snapshot);
        assert!(finished_receiver.recv_timeout(Duration::from_secs(2))?);
        assert!(catch_up.join().is_ok());
        assert_eq!(
            secondary.get_consumer(TEST_CONSUMER_CF, b"after")?,
            Some(b"new".to_vec())
        );
        Ok(())
    }

    #[test]
    fn visit_consumer_rows_streams_rows_and_fails_closed() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        store.put_consumer(TEST_CONSUMER_CF, b"a", b"one")?;
        store.put_consumer(TEST_CONSUMER_CF, b"b", b"two")?;

        let mut visited = Vec::new();
        store.visit_consumer_rows(TEST_CONSUMER_CF, |key, payload| {
            visited.push((key.to_vec(), payload.to_vec()));
            Ok(())
        })?;
        assert_eq!(
            visited,
            vec![
                (b"a".to_vec(), b"one".to_vec()),
                (b"b".to_vec(), b"two".to_vec())
            ]
        );

        let invalid = store.visit_consumer_rows(TEST_CONSUMER_CF, |_key, _payload| {
            Err("invalid fixture row".to_owned())
        });
        assert!(matches!(
            invalid,
            Err(DeriveStoreError::ConsumerPayloadDecode { name, reason })
                if name == TEST_CONSUMER_CF && reason == "invalid fixture row"
        ));
        Ok(())
    }

    #[test]
    fn visit_consumer_range_streams_only_inclusive_bounds_and_fails_closed() -> Result<()> {
        let tempdir = tempdir()?;
        let options = DeriveStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..DeriveStoreOptions::default()
        };
        let store = DeriveStore::open(tempdir.path(), options)?;
        for key in [b"a", b"b", b"c", b"d"] {
            store.put_consumer(TEST_CONSUMER_CF, key, key)?;
        }

        let mut visited = Vec::new();
        store.visit_consumer_range(
            TEST_CONSUMER_CF,
            b"b".as_slice()..=b"c".as_slice(),
            |key, _| {
                visited.push(key.to_vec());
                Ok(())
            },
        )?;
        assert_eq!(visited, vec![b"b".to_vec(), b"c".to_vec()]);

        let invalid = store.visit_consumer_range(
            TEST_CONSUMER_CF,
            b"b".as_slice()..=b"c".as_slice(),
            |_key, _payload| Err("invalid bounded fixture row".to_owned()),
        );
        assert!(matches!(
            invalid,
            Err(DeriveStoreError::ConsumerPayloadDecode { name, reason })
                if name == TEST_CONSUMER_CF && reason == "invalid bounded fixture row"
        ));
        Ok(())
    }

    #[test]
    fn reopening_a_store_with_an_advanced_store_format_version_returns_mismatch() -> Result<()> {
        let tempdir = tempdir()?;
        {
            let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;
            store.put(
                DeriveStoreTable::ConsumerMetadata,
                STORE_FORMAT_VERSION_KEY,
                &(DERIVE_STORE_FORMAT_VERSION + 1).to_be_bytes(),
            )?;
        }
        let outcome = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default());
        assert!(matches!(
            outcome,
            Err(DeriveStoreError::SchemaMismatch {
                persisted,
                running,
            }) if persisted == DERIVE_STORE_FORMAT_VERSION + 1 && running == DERIVE_STORE_FORMAT_VERSION
        ));
        Ok(())
    }

    #[test]
    fn reopening_a_store_with_an_older_store_format_version_rebuilds() -> Result<()> {
        let tempdir = tempdir()?;
        {
            let store = DeriveStore::open(
                tempdir.path(),
                DeriveStoreOptions {
                    consumers: &[TEST_CONSUMER_SCHEMA],
                    ..DeriveStoreOptions::default()
                },
            )?;
            let handle = store.consumer_column_family("test_cf")?;
            let mut batch = WriteBatch::default();
            batch.put_cf(&handle, 1_u32.to_be_bytes(), b"row");
            drop(handle);
            store.write_batch(&batch)?;
            store.put_chain_event_cursor(TEST_CONSUMER, &[9])?;
            store.put(
                DeriveStoreTable::ConsumerMetadata,
                STORE_FORMAT_VERSION_KEY,
                &(DERIVE_STORE_FORMAT_VERSION - 1).to_be_bytes(),
            )?;
        }
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[TEST_CONSUMER_SCHEMA],
                ..DeriveStoreOptions::default()
            },
        )?;
        assert_eq!(store.store_format_version()?, DERIVE_STORE_FORMAT_VERSION);
        assert_eq!(store.last_consumer_key("test_cf")?, None);
        assert!(store.get_chain_event_cursor(TEST_CONSUMER)?.is_none());
        Ok(())
    }

    #[test]
    fn manifest_entry_round_trips_writer_row_versions_and_column_families() -> Result<()> {
        let row_schema_versions = BTreeSet::from([1, 2, 3]);
        let encoded = encode_manifest_entry(3, &["alpha", "beta_index"], &row_schema_versions)
            .map_err(|reason| eyre::eyre!(reason))?;
        let decoded = decode_manifest_entry(&encoded).map_err(|reason| eyre::eyre!(reason))?;
        assert_eq!(decoded.schema_version, 3);
        assert_eq!(decoded.row_schema_versions, row_schema_versions);
        assert_eq!(
            decoded.column_families,
            vec!["alpha".to_owned(), "beta_index".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn consumer_projection_state_round_trips_verified_coverage() -> Result<()> {
        let state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(42),
            projection_tip_height: BlockHeight::new(100),
            projection_tip_hash: BlockHash::from_bytes([0xA1; 32]),
            revision: 7,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(90),
                complete_through_hash: BlockHash::from_bytes([0xB2; 32]),
            }),
        };

        let decoded = decode_consumer_projection_state(
            TEST_CONSUMER,
            &encode_consumer_projection_state(state),
        )?;

        assert_eq!(decoded, state);
        Ok(())
    }

    #[test]
    fn decoding_coverage_past_projection_tip_drops_coverage() -> Result<()> {
        let state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(42),
            projection_tip_height: BlockHeight::new(10),
            projection_tip_hash: BlockHash::from_bytes([0xA1; 32]),
            revision: 7,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(11),
                complete_through_hash: BlockHash::from_bytes([0xB2; 32]),
            }),
        };

        let decoded = decode_consumer_projection_state(
            TEST_CONSUMER,
            &encode_consumer_projection_state(state),
        )?;

        assert_eq!(decoded.coverage, None);
        assert_eq!(decoded.projection_tip_height, BlockHeight::new(10));
        assert_eq!(decoded.revision, 7);
        Ok(())
    }

    #[test]
    fn encoding_a_manifest_entry_rejects_more_column_families_than_the_count_field_holds() {
        let column_families = vec!["x"; usize::from(u16::MAX) + 1];
        let outcome = encode_manifest_entry(1, &column_families, &BTreeSet::from([1]));
        assert!(matches!(outcome, Err(reason) if reason.contains("column families")));
    }

    #[test]
    fn encoding_a_manifest_entry_rejects_a_column_family_name_longer_than_the_length_field_holds() {
        let overlong = "a".repeat(usize::from(u16::MAX) + 1);
        let outcome = encode_manifest_entry(1, &[overlong.as_str()], &BTreeSet::from([1]));
        assert!(matches!(outcome, Err(reason) if reason.contains("column family name")));
    }

    #[test]
    fn legacy_manifest_without_row_versions_uses_writer_version_as_provenance() -> Result<()> {
        let mut encoded = encode_manifest_entry(3, &["alpha"], &BTreeSet::from([3]))
            .map_err(|reason| eyre::eyre!(reason))?;
        encoded.truncate(encoded.len().saturating_sub(4));

        let decoded = decode_manifest_entry(&encoded).map_err(|reason| eyre::eyre!(reason))?;

        assert_eq!(decoded.schema_version, 3);
        assert_eq!(decoded.row_schema_versions, BTreeSet::from([3]));
        Ok(())
    }

    #[test]
    fn decoding_a_manifest_entry_rejects_duplicate_row_versions() -> Result<()> {
        let mut encoded = encode_manifest_entry(2, &["alpha"], &BTreeSet::from([1, 2]))
            .map_err(|reason| eyre::eyre!(reason))?;
        let duplicate_version_offset = encoded.len().saturating_sub(2);
        encoded.extend_from_within(duplicate_version_offset..);
        let row_version_count_offset = 2 + 2 + 2 + "alpha".len();
        encoded[row_version_count_offset..row_version_count_offset + 2]
            .copy_from_slice(&3_u16.to_be_bytes());

        let outcome = decode_manifest_entry(&encoded);

        assert!(matches!(outcome, Err(reason) if reason.contains("duplicate row schema")));
        Ok(())
    }

    #[test]
    fn opening_store_rejects_zero_wal_budget() -> Result<()> {
        let tempdir = tempdir()?;
        let mut options = DeriveStoreOptions::default();
        options.rocksdb_resource_budget.max_wal_bytes = 0;

        let outcome = DeriveStore::open(tempdir.path(), options);

        assert!(matches!(
            outcome,
            Err(DeriveStoreError::InvalidOptions { reason })
                if reason.contains("max_wal_bytes")
        ));
        Ok(())
    }

    #[test]
    fn opening_store_rejects_negative_open_file_budget() -> Result<()> {
        let tempdir = tempdir()?;
        let mut options = DeriveStoreOptions::default();
        options.rocksdb_resource_budget.max_open_files = -1;

        let outcome = DeriveStore::open(tempdir.path(), options);

        assert!(matches!(
            outcome,
            Err(DeriveStoreError::InvalidOptions { reason })
                if reason.contains("max_open_files")
        ));
        Ok(())
    }

    fn projection_state_with_coverage(
        projection_tip_height: u32,
        complete_from_height: u32,
        complete_through_height: u32,
    ) -> ConsumerProjectionState {
        ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(1),
            projection_tip_height: BlockHeight::new(projection_tip_height),
            projection_tip_hash: BlockHash::from_bytes([0x11; 32]),
            revision: 7,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(complete_from_height),
                complete_through_height: BlockHeight::new(complete_through_height),
                complete_through_hash: BlockHash::from_bytes([0x22; 32]),
            }),
        }
    }

    #[test]
    fn staging_projection_coverage_with_inverted_bounds_is_rejected() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;

        let inverted = projection_state_with_coverage(200, 150, 100);
        match store.put_consumer_projection_state(TEST_CONSUMER, inverted) {
            Err(DeriveStoreError::InvalidProjectionCoverage {
                consumer,
                complete_from_height,
                complete_through_height,
                projection_tip_height,
            }) => {
                assert_eq!(consumer, TEST_CONSUMER.as_str());
                assert_eq!(complete_from_height, 150);
                assert_eq!(complete_through_height, 100);
                assert_eq!(projection_tip_height, 200);
            }
            other => {
                return Err(eyre::eyre!(
                    "expected InvalidProjectionCoverage, got {other:?}"
                ));
            }
        }

        assert!(store.consumer_projection_state(TEST_CONSUMER)?.is_none());
        Ok(())
    }

    #[test]
    fn staging_projection_coverage_beyond_tip_is_rejected() -> Result<()> {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(tempdir.path(), DeriveStoreOptions::default())?;

        let beyond_tip = projection_state_with_coverage(180_256, 1, 180_512);
        match store.put_consumer_projection_state(TEST_CONSUMER, beyond_tip) {
            Err(DeriveStoreError::InvalidProjectionCoverage {
                complete_through_height,
                projection_tip_height,
                ..
            }) => {
                assert_eq!(complete_through_height, 180_512);
                assert_eq!(projection_tip_height, 180_256);
            }
            other => {
                return Err(eyre::eyre!(
                    "expected InvalidProjectionCoverage, got {other:?}"
                ));
            }
        }

        assert!(store.consumer_projection_state(TEST_CONSUMER)?.is_none());
        Ok(())
    }
}
