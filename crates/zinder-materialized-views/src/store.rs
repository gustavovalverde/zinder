//! `RocksDB` wrapper for the materialized-view plane.
//!
//! `MaterializedViewStore` is intentionally separate from `zinder_store::PrimaryChainStore`:
//! it lives in its own filesystem path, has its own column families, and uses
//! its own schema version. The two stores never share keys.
//!
//! Both stores share one source of truth for `RocksDB` option choices:
//! [`zinder_store::open_bounded_rocksdb`] from
//! [ADR-0020](../../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
//! That keeps the bulk-catchup-OOM trap, which is a property of unbounded
//! `RocksDB` defaults rather than the canonical store's specific layout,
//! impossible to recur in the materialized-view plane.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    hash::BuildHasher,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{RwLock, RwLockReadGuard};
use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, IteratorMode, Options, ReadOptions, Snapshot, WriteBatch,
    WriteOptions, checkpoint::Checkpoint,
};
use zinder_core::{BlockHash, BlockHeight, ChainEpoch, ChainEpochId, Network};
use zinder_store::{
    CanonicalEventCursor, CanonicalEventFence, CanonicalRetainedEvent,
    CanonicalStoreConstructionIdentity, ChainEvent, ResourceGaugeThrottle, RocksDbIoMode,
    RocksDbOpenRole, RocksDbResourceBudget, RocksDbResourceGaugeInputs, StoreRole,
    build_block_based_table_factory, open_bounded_rocksdb, record_rocksdb_resource_gauges,
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
    consumer::paid_fee_distribution::{
        PAID_FEE_DISTRIBUTION_CONSUMER_NAME, PAID_FEE_DISTRIBUTION_SCHEMA,
    },
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
        BlockCommitContext, BlockKeyedConsumer, ChainCommittedEvent, ChainReorgedEvent,
        CommittedRange, MaterializedViewBlockProjection, MaterializedViewConsumer,
        MaterializedViewConsumerCtx, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
        MaterializedViewMempoolConsumer, RevertedRange, apply_chain_committed_in_memory,
        apply_chain_reorged_in_memory,
    },
    error::{MaterializedViewError, MaterializedViewStoreColumnFamily, MaterializedViewStoreError},
};

/// Conventional subdirectory of the canonical store path where the
/// materialized-view `RocksDB` instance lives.
///
/// A separately composed replay host and any reader process opening the store
/// in secondary mode resolve the materialized-view store with
/// [`MaterializedViewStore::path_for_canonical`], so operators only configure
/// one `storage.path` per service.
pub const MATERIALIZED_VIEW_STORE_SUBDIR: &str = "materialized-views";

/// Container-format version of the materialized-view store.
///
/// Gates the parts shared by every consumer: the per-consumer schema
/// manifest layout, the cursor encoding, and the metadata column family.
/// Per-consumer column-family layouts version themselves through
/// [`MaterializedViewConsumerSchema::schema_version`]; a consumer changing its own
/// persisted row contract bumps its own version. The running binary admits
/// only an exact manifest, so any consumer schema change requires a fresh
/// materialized-view store. This constant bumps only when the shared container
/// changes. No persisted consumer data survives any schema change, so an
/// incompatible store is rejected until an operator supplies a fresh store and
/// rebuilds it from a certified recovery source. The version is persisted in
/// the `consumer_metadata` column family on first open and validated on
/// subsequent opens.
pub const MATERIALIZED_VIEW_STORE_FORMAT_VERSION: u16 = 10;

/// Total attempts used to cross a primary-compaction race while a secondary
/// catches up and validates its newly replayed manifest.
const SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS: u32 = 3;

const STORE_FORMAT_VERSION_KEY: &[u8] = b"\x00\x01schema_version";
const MATERIALIZED_VIEW_STATUS_KEY: &[u8] = b"\x00\x02materialized_view_status";
const CONSUMER_SCHEMA_KEY_PREFIX: &[u8] = b"\x00\x03consumer_schema:";
const MATERIALIZED_VIEW_STATE_KEY_PREFIX: &[u8] = b"\x00\x04consumer_state:";
const STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY: &[u8] = b"\x00\x05canonical_construction_identity";
const MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_VERSION: u8 = 1;
const MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES: usize = 1 + 9 + 95;
const MATERIALIZED_VIEW_STATE_VERSION: u8 = 1;
const MATERIALIZED_VIEW_STATE_LEN: usize = 94;
const MATERIALIZED_VIEW_ROCKSDB_PROPERTIES: [&str; 7] = [
    "rocksdb.estimate-live-data-size",
    "rocksdb.total-sst-files-size",
    "rocksdb.size-all-mem-tables",
    "rocksdb.cur-size-active-mem-table",
    "rocksdb.estimate-table-readers-mem",
    "rocksdb.estimate-pending-compaction-bytes",
    "rocksdb.num-running-compactions",
];

/// Per-consumer work staged into one successful atomic materialized-view commit.
///
/// Measurements use the stable consumer identity rather than its owned
/// column-family names. The replay scheduler turns these values into replay
/// and write metrics only after the shared batch commits successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedViewWriteMeasurement {
    /// Stable consumer identity that staged the writes.
    pub consumer: MaterializedViewConsumerName,
    /// Number of puts, deletes, and merges staged by this consumer.
    pub operations: u64,
    /// Serialized write-batch bytes attributable to this consumer.
    pub logical_bytes: u64,
    /// Time spent applying the event and staging consumer-owned rows.
    pub dispatch_duration: Duration,
}

impl MaterializedViewWriteMeasurement {
    fn from_batch_delta(
        consumer: MaterializedViewConsumerName,
        before: WriteBatchSize,
        batch: &WriteBatch,
        dispatch_duration: Duration,
    ) -> Self {
        let after = WriteBatchSize::capture(batch);
        Self {
            consumer,
            operations: usize_to_u64_saturating(after.operations.saturating_sub(before.operations)),
            logical_bytes: usize_to_u64_saturating(
                after.logical_bytes.saturating_sub(before.logical_bytes),
            ),
            dispatch_duration,
        }
    }

    fn add_batch_delta(&mut self, before: WriteBatchSize, batch: &WriteBatch) {
        let after = WriteBatchSize::capture(batch);
        self.operations = self.operations.saturating_add(usize_to_u64_saturating(
            after.operations.saturating_sub(before.operations),
        ));
        self.logical_bytes = self.logical_bytes.saturating_add(usize_to_u64_saturating(
            after.logical_bytes.saturating_sub(before.logical_bytes),
        ));
    }
}

#[derive(Clone, Copy)]
struct WriteBatchSize {
    operations: usize,
    logical_bytes: usize,
}

impl WriteBatchSize {
    fn capture(batch: &WriteBatch) -> Self {
        Self {
            operations: batch.len(),
            logical_bytes: batch.size_in_bytes(),
        }
    }
}
const ROCKSDB_DEFAULT_COLUMN_FAMILY: &str = "default";

const BUNDLED_CONSUMERS: &[MaterializedViewConsumerSchema] = &[
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
const WALLET_PROJECTION_CONSUMERS: &[MaterializedViewConsumerSchema] = &[
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
];
const BUNDLED_CHAIN_EVENT_CONSUMER_NAMES: &[MaterializedViewConsumerName] = &[
    BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
    BLOCK_SUMMARY_CONSUMER_NAME,
    IRONWOOD_MIGRATION_CONSUMER_NAME,
    COMMITMENT_ROOT_SEARCH_CONSUMER_NAME,
    CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
    PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
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
const BUNDLED_EVENT_ONLY_CHAIN_EVENT_CONSUMER_NAMES: &[MaterializedViewConsumerName] =
    &[REORG_INCIDENTS_CONSUMER_NAME];

/// Closed product workload that selects durable materialized view identities.
///
/// Presets choose which read models the materialized-view store materializes. They do not
/// change canonical facts, historical coverage, raw payload retention, or the
/// process that executes a materialized view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum MaterializedViewPreset {
    /// Wallet-serving materialized views required for transparent history and durable
    /// spender resolution.
    Wallet,
    /// Wallet-serving materialized views plus every explorer materialized view bundled with
    /// this Zinder release.
    #[default]
    Explorer,
}

impl MaterializedViewPreset {
    /// Returns the stable configuration and storage name for this preset.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::Explorer => "explorer",
        }
    }

    /// Returns the durable consumer schemas selected by this preset.
    #[must_use]
    pub const fn consumer_schemas(self) -> &'static [MaterializedViewConsumerSchema] {
        match self {
            Self::Wallet => WALLET_PROJECTION_CONSUMERS,
            Self::Explorer => BUNDLED_CONSUMERS,
        }
    }
}

/// Per-column-family options the materialized-view plane tunes at open time.
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
/// Every store family and every declared consumer family is opened. On an
/// existing store, the supplied on-disk names were already checked against
/// that exact identity before this function runs; retaining them here lets
/// `RocksDB` verify the checked set at open time.
fn column_family_descriptors(
    cache: &Cache,
    rocksdb_resource_budget: RocksDbResourceBudget,
    consumers: &[MaterializedViewConsumerSchema],
    existing_column_families: &[String],
) -> Vec<ColumnFamilyDescriptor> {
    let store_families = MaterializedViewStoreTable::all().into_iter().map(|table| {
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
    for table in MaterializedViewStoreTable::all() {
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

fn validate_column_family_identity(
    persisted: &[String],
    consumers: &[MaterializedViewConsumerSchema],
) -> Result<(), MaterializedViewStoreError> {
    let expected = std::iter::once(ROCKSDB_DEFAULT_COLUMN_FAMILY.to_owned())
        .chain(
            MaterializedViewStoreTable::all()
                .into_iter()
                .map(|table| table.column_family_name().to_owned()),
        )
        .chain(consumers.iter().flat_map(|consumer| {
            consumer
                .column_families
                .iter()
                .map(|name| (*name).to_owned())
        }))
        .collect::<BTreeSet<_>>();
    let persisted = persisted.iter().cloned().collect::<BTreeSet<_>>();
    if persisted == expected {
        return Ok(());
    }
    Err(MaterializedViewStoreError::ColumnFamilyIdentityMismatch {
        persisted: persisted.into_iter().collect(),
        expected: expected.into_iter().collect(),
    })
}

/// Rejects consumer declarations whose column families collide.
///
/// Every declared column family must be unique across consumers and must not
/// reuse a store-table name or the `RocksDB` default family. A name shared by
/// two declarations would make the persisted schema identity ambiguous, so
/// rejecting it at open time keeps the manifest unambiguous.
fn validate_consumer_declarations(
    consumers: &[MaterializedViewConsumerSchema],
) -> Result<(), MaterializedViewStoreError> {
    let mut declared = BTreeSet::<&'static str>::new();
    for consumer in consumers {
        for &name in consumer.column_families {
            let reserved = name == ROCKSDB_DEFAULT_COLUMN_FAMILY
                || MaterializedViewStoreTable::all()
                    .iter()
                    .any(|table| table.column_family_name() == name);
            if reserved || !declared.insert(name) {
                return Err(MaterializedViewStoreError::ConsumerColumnFamilyConflict { name });
            }
        }
    }
    Ok(())
}

/// Logical column-family identifier.
///
/// Mirrors `MaterializedViewStoreColumnFamily` but lives on the public store surface
/// because callers reference column families when issuing reads. Operator
/// errors carry the same enum so the two halves stay in sync.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MaterializedViewStoreTable {
    /// `chain_event_cursor` column family: per-chain-consumer cursor persistence.
    ChainEventCursor,
    /// `mempool_event_cursor` column family: per-mempool-consumer cursor persistence.
    MempoolEventCursor,
    /// `consumer_metadata` column family: schema versions and per-consumer
    /// counters.
    ConsumerMetadata,
}

impl MaterializedViewStoreTable {
    /// Returns the canonical `RocksDB` column-family name for the variant.
    #[must_use]
    pub const fn column_family_name(self) -> &'static str {
        match self {
            Self::ChainEventCursor => "chain_event_cursor",
            Self::MempoolEventCursor => "mempool_event_cursor",
            Self::ConsumerMetadata => "consumer_metadata",
        }
    }

    fn error_family(self) -> MaterializedViewStoreColumnFamily {
        match self {
            Self::ChainEventCursor => MaterializedViewStoreColumnFamily::ChainEventCursor,
            Self::MempoolEventCursor => MaterializedViewStoreColumnFamily::MempoolEventCursor,
            Self::ConsumerMetadata => MaterializedViewStoreColumnFamily::ConsumerMetadata,
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
/// [`RocksDbResourceBudget::materialized_view_writer_defaults`].
#[derive(Clone, Copy, Debug)]
pub struct MaterializedViewStoreOptions {
    /// When set, every write is flushed to the OS page cache before returning.
    /// Default `false` matches the canonical store's tunable so operators can
    /// trade durability for throughput in development environments.
    pub sync_writes: bool,
    /// Consumers to register at open time. Each declares its stable name, its
    /// schema version, and the column families it reads and writes through
    /// [`MaterializedViewStore::consumer_column_family`]. On reopen every
    /// declaration must match the persisted manifest exactly; a changed
    /// consumer requires a fresh materialized-view store.
    pub consumers: &'static [MaterializedViewConsumerSchema],
    /// Bounded `RocksDB` resource budget applied at open time.
    pub rocksdb_resource_budget: RocksDbResourceBudget,
}

impl Default for MaterializedViewStoreOptions {
    fn default() -> Self {
        Self {
            sync_writes: false,
            consumers: &[],
            rocksdb_resource_budget: RocksDbResourceBudget::materialized_view_writer_defaults(),
        }
    }
}

/// Owned `(key, payload)` pair returned by
/// [`MaterializedViewStore::range_iterate_consumer`]. Both halves are RocksDB-owned
/// bytes copied out of the iterator's borrowed buffers.
pub type ConsumerEntry = (Vec<u8>, Vec<u8>);

/// Exact canonical event applied by one materialized-view chain consumer.
///
/// The cursor and resulting fence are persisted as one versioned value in the
/// same batch as that consumer's rows. Different replay cohorts therefore
/// retain independent truthful boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedViewChainEventCheckpoint {
    cursor: CanonicalEventCursor,
    resulting_fence: CanonicalEventFence,
}

impl MaterializedViewChainEventCheckpoint {
    /// Captures the exact cursor and fence carried by one retained event.
    #[must_use]
    pub const fn from_retained_event(event: CanonicalRetainedEvent) -> Self {
        Self {
            cursor: event.cursor(),
            resulting_fence: event.resulting_fence(),
        }
    }

    /// Returns the exact canonical cursor persisted for this consumer.
    #[must_use]
    pub const fn cursor(self) -> CanonicalEventCursor {
        self.cursor
    }

    /// Returns the authenticated canonical fence produced by the event.
    #[must_use]
    pub const fn resulting_fence(self) -> CanonicalEventFence {
        self.resulting_fence
    }

    /// Verifies that this checkpoint names exactly one retained canonical event.
    ///
    /// Replay and rebuild callers must obtain `retained_event` from the
    /// admitted canonical secondary with
    /// [`zinder_store::RocksDbCanonicalSecondary::retained_event_at_cursor`]
    /// before they reuse a persisted checkpoint. A matching sequence alone is
    /// insufficient because a different canonical fence can carry that same
    /// sequence after a source mismatch or corruption.
    pub fn require_matching_retained_event(
        self,
        retained_event: CanonicalRetainedEvent,
    ) -> Result<(), MaterializedViewStoreError> {
        self.require_matching_retained_event_boundary(
            retained_event.cursor(),
            retained_event.resulting_fence(),
        )
    }

    fn require_matching_retained_event_boundary(
        self,
        cursor: CanonicalEventCursor,
        resulting_fence: CanonicalEventFence,
    ) -> Result<(), MaterializedViewStoreError> {
        if self.cursor != cursor || self.resulting_fence != resulting_fence {
            return Err(
                MaterializedViewStoreError::ChainEventCheckpointRetainedEventMismatch {
                    event_sequence: self.cursor.event_sequence(),
                },
            );
        }
        Ok(())
    }
}

/// Inputs that bind a canonical chain event to one materialized-view write.
#[derive(Clone, Copy)]
pub struct ChainEventDispatchInputs<'event> {
    /// Chain epoch the canonical commit just produced.
    pub chain_epoch: ChainEpoch,
    /// Post-commit event emitted by the canonical store.
    pub chain_event: &'event ChainEvent,
    /// Exact cursor and resulting fence emitted by the canonical store.
    pub checkpoint: MaterializedViewChainEventCheckpoint,
    /// Settled tip height observed at commit time.
    pub settled_tip_height: BlockHeight,
}

/// One verified contiguous range within a materialized-view consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedViewCoverage {
    /// First verified canonical height.
    pub complete_from_height: BlockHeight,
    /// Last verified canonical height.
    pub complete_through_height: BlockHeight,
    /// Canonical hash at [`Self::complete_through_height`].
    pub complete_through_hash: BlockHash,
}

/// Atomic read fence and optional verified coverage for one materialized-view consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedViewState {
    /// Canonical epoch whose materialized-view writes are visible.
    pub chain_epoch_id: ChainEpochId,
    /// Highest canonical height reflected by the materialized view.
    pub tip_height: BlockHeight,
    /// Canonical hash at [`Self::tip_height`].
    pub tip_hash: BlockHash,
    /// Monotonic materialized-view mutation and coverage revision.
    pub revision: u64,
    /// Verified contiguous coverage, when verification has started.
    pub coverage: Option<MaterializedViewCoverage>,
}

/// Consumers that participate in one chain-event materialized-view write.
pub struct ChainEventDispatchConsumers<
    'block_slices,
    'block_consumers,
    'event_slices,
    'event_consumers,
> {
    /// Block-keyed consumers that observe committed block contexts.
    pub block_consumers: &'block_slices mut [&'block_consumers mut dyn BlockKeyedConsumer],
    /// Direct consumers that observe the chain-event envelope.
    pub event_consumers: &'event_slices mut [&'event_consumers mut dyn MaterializedViewConsumer],
}

/// `RocksDB`-backed durable storage for the materialized-view plane.
///
/// Operations are atomic at the `RocksDB` `WriteBatch` granularity. Cursor
/// writes always go in a single batch with the consumer's data writes so a
/// crash mid-write never advances the cursor without persisting the
/// underlying state.
#[derive(Clone)]
pub struct MaterializedViewStore {
    db: Arc<DB>,
    construction_identity: CanonicalStoreConstructionIdentity,
    sync_writes: bool,
    storage_path: PathBuf,
    consumers: &'static [MaterializedViewConsumerSchema],
    rocksdb_resource_budget: RocksDbResourceBudget,
    is_secondary: bool,
    catch_up_barrier: Arc<RwLock<()>>,
    terminal_catch_up_failure: Arc<AtomicBool>,
    block_cache: Cache,
    write_buffer_manager: rust_rocksdb::WriteBufferManager,
    statistics: Arc<Options>,
    io_mode: RocksDbIoMode,
    resource_gauge_throttle: Arc<ResourceGaugeThrottle>,
    logical_write_bytes: Arc<AtomicU64>,
}

/// Consistent read view over one `MaterializedViewStore` sequence.
///
/// Every method reads through the same storage snapshot, so materialized-view
/// metadata, consumer rows, bounds, and exact counts cannot observe different
/// commits during one request. The underlying storage handles remain private.
pub struct MaterializedViewStoreReadSnapshot<'store> {
    store: &'store MaterializedViewStore,
    consistency: MaterializedViewReadConsistency<'store>,
}

enum MaterializedViewReadConsistency<'store> {
    Primary(Snapshot<'store>),
    Secondary {
        _catch_up_guard: RwLockReadGuard<'store, ()>,
    },
}

impl MaterializedViewStoreReadSnapshot<'_> {
    fn read_options(&self) -> ReadOptions {
        let mut options = ReadOptions::default();
        if let MaterializedViewReadConsistency::Primary(snapshot) = &self.consistency {
            options.set_snapshot(snapshot);
        }
        options
    }

    /// Reads one consumer's atomic state and verified coverage.
    pub fn consumer_state(
        &self,
        consumer: MaterializedViewConsumerName,
    ) -> Result<Option<MaterializedViewState>, MaterializedViewStoreError> {
        let column_family = self
            .store
            .column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        let key = consumer_state_key(consumer.as_str());
        self.store
            .db
            .get_cf_opt(&column_family, key, &self.read_options())
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })?
            .map(|payload| decode_materialized_view_state(consumer, &payload))
            .transpose()
    }

    /// Reads a chain-event consumer's exact applied checkpoint from this snapshot.
    pub fn chain_event_checkpoint(
        &self,
        consumer: MaterializedViewConsumerName,
    ) -> Result<Option<MaterializedViewChainEventCheckpoint>, MaterializedViewStoreError> {
        let column_family = self
            .store
            .column_family(MaterializedViewStoreTable::ChainEventCursor)?;
        self.store
            .db
            .get_cf_opt(
                &column_family,
                consumer.as_str().as_bytes(),
                &self.read_options(),
            )
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: MaterializedViewStoreColumnFamily::ChainEventCursor,
                source,
            })
            .and_then(|checkpoint| {
                checkpoint
                    .map(|encoded| decode_chain_event_checkpoint(consumer, &encoded))
                    .transpose()
            })
    }

    /// Reads a single value from a consumer-owned column family.
    pub fn get_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        self.store
            .db
            .get_cf_opt(&handle, key, &self.read_options())
            .map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                operation: "get",
                name: column_family,
                source,
            })
    }

    /// Reads the ingest plane's materialized-view status record from this snapshot.
    pub fn get_materialized_view_status(
        &self,
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let column_family = self
            .store
            .column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        self.store
            .db
            .get_cf_opt(
                &column_family,
                MATERIALIZED_VIEW_STATUS_KEY,
                &self.read_options(),
            )
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Batch-reads consumer keys in input order from this snapshot.
    pub fn multi_get_consumer<K>(
        &self,
        column_family: &'static str,
        keys: &[K],
    ) -> Result<Vec<Option<Vec<u8>>>, MaterializedViewStoreError>
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
                outcome.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<Vec<ConsumerEntry>, MaterializedViewStoreError> {
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
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        self.consumer_edge_key(column_family, IteratorMode::Start, "first_key")
    }

    /// Returns the lexicographically last consumer key, if one exists.
    pub fn last_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        self.consumer_edge_key(column_family, IteratorMode::End, "last_key")
    }

    /// Returns the last consumer entry visible in this snapshot.
    pub fn last_consumer_entry(
        &self,
        column_family: &'static str,
    ) -> Result<Option<ConsumerEntry>, MaterializedViewStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut iterator =
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                operation: "last_entry",
                name: column_family,
                source,
            })?;
        Ok(Some((key.to_vec(), payload.to_vec())))
    }

    /// Returns the highest height materialized in an ascending-height
    /// consumer column family from this snapshot.
    pub fn last_materialized_height_ascending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
        let Some(key) = self.last_consumer_key(column_family)? else {
            return Ok(None);
        };
        zinder_core::wire::decode_height_key_ascending(&key)
            .map(Some)
            .map_err(|error| MaterializedViewStoreError::Decode {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` last key is not a 4-byte ascending height: {error}"
                ),
            })
    }

    /// Returns the lowest height in a descending-height consumer keyspace.
    pub fn first_materialized_height_descending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
        let key = self.last_consumer_key(column_family)?;
        decode_descending_height_prefix(key.as_deref(), column_family, "last")
    }

    /// Returns the highest height in a descending-height consumer keyspace.
    pub fn last_materialized_height_descending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
        let key = self.first_consumer_key(column_family)?;
        decode_descending_height_prefix(key.as_deref(), column_family, "first")
    }

    /// Counts every row in a consumer-owned column family exactly.
    pub fn consumer_row_count(
        &self,
        column_family: &'static str,
    ) -> Result<u64, MaterializedViewStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut row_count = 0_u64;
        for entry in
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::Start)
        {
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<u64, MaterializedViewStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut matching_count = 0_u64;
        for entry in
            self.store
                .db
                .iterator_cf_opt(&handle, self.read_options(), IteratorMode::Start)
        {
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                    operation: "count_matching_rows",
                    name: column_family,
                    source,
                })?;
            if predicate(&key, &payload).map_err(|reason| {
                MaterializedViewStoreError::ConsumerPayloadDecode {
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
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let handle = self.store.consumer_column_family(column_family)?;
        let mut iterator = self
            .store
            .db
            .iterator_cf_opt(&handle, self.read_options(), mode);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let prefix = key.get(..zinder_core::wire::HEIGHT_KEY_LEN).ok_or_else(|| {
        MaterializedViewStoreError::Decode {
            column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            reason: format!(
                "consumer column family `{column_family}` {edge} key is shorter than the descending-height prefix"
            ),
        }
    })?;
    zinder_core::wire::decode_height_key_descending(prefix)
        .map(Some)
        .map_err(|error| MaterializedViewStoreError::Decode {
            column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            reason: format!(
                "consumer column family `{column_family}` {edge} key descending-height prefix invalid: {error}"
            ),
        })
}

impl fmt::Debug for MaterializedViewStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedViewStore")
            .field("db", &self.db)
            .field("construction_identity", &self.construction_identity)
            .field("sync_writes", &self.sync_writes)
            .field("storage_path", &self.storage_path)
            .field("consumers", &self.consumers)
            .field("rocksdb_resource_budget", &self.rocksdb_resource_budget)
            .field("is_secondary", &self.is_secondary)
            .field(
                "logical_write_bytes",
                &self.logical_write_bytes.load(Ordering::Relaxed),
            )
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

impl MaterializedViewStore {
    /// Returns the immutable canonical construction claim persisted by the store.
    #[must_use]
    pub const fn construction_identity(&self) -> CanonicalStoreConstructionIdentity {
        self.construction_identity
    }

    /// Returns the network carried by the canonical construction identity.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.construction_identity.network()
    }

    /// Captures a consistent read view at the store's current sequence.
    pub fn read_snapshot(
        &self,
    ) -> Result<MaterializedViewStoreReadSnapshot<'_>, MaterializedViewStoreError> {
        let consistency = if self.is_secondary {
            let catch_up_guard = self.admit_secondary_read()?;
            MaterializedViewReadConsistency::Secondary {
                _catch_up_guard: catch_up_guard,
            }
        } else {
            MaterializedViewReadConsistency::Primary(Snapshot::new(self.db.as_ref()))
        };
        Ok(MaterializedViewStoreReadSnapshot {
            store: self,
            consistency,
        })
    }

    /// Returns the conventional materialized-view path for a canonical-store
    /// path.
    ///
    /// Both writer and reader processes derive the path from the canonical
    /// store path via this helper so the operator only configures one
    /// `storage.path` per service. The convention nests the materialized-view
    /// `RocksDB` files in a [`MATERIALIZED_VIEW_STORE_SUBDIR`] subdirectory of the
    /// canonical store path.
    #[must_use]
    pub fn path_for_canonical(canonical_path: &Path) -> PathBuf {
        canonical_path.join(MATERIALIZED_VIEW_STORE_SUBDIR)
    }

    /// Returns the schema declarations for the bundled materialized-view consumers.
    ///
    /// Pass this into [`MaterializedViewStoreOptions::consumers`] to register every
    /// bundled consumer with its name, schema version, and owned column
    /// families in one call.
    #[must_use]
    pub const fn bundled_consumers() -> &'static [MaterializedViewConsumerSchema] {
        BUNDLED_CONSUMERS
    }

    /// Returns the bundled chain-event consumer cursor names.
    #[must_use]
    pub const fn bundled_chain_event_consumer_names() -> &'static [MaterializedViewConsumerName] {
        BUNDLED_CHAIN_EVENT_CONSUMER_NAMES
    }

    /// Returns bundled chain-event consumer cursor names that read only the
    /// event envelope and do not need committed block contexts.
    #[must_use]
    pub const fn bundled_event_only_chain_event_consumer_names()
    -> &'static [MaterializedViewConsumerName] {
        BUNDLED_EVENT_ONLY_CHAIN_EVENT_CONSUMER_NAMES
    }

    /// Returns whether this store declared the durable consumer identity at
    /// open time.
    #[must_use]
    pub fn has_consumer(&self, consumer_name: MaterializedViewConsumerName) -> bool {
        self.consumers
            .iter()
            .any(|schema| schema.name == consumer_name)
    }

    /// Returns the closed product workload represented by this store's
    /// selected consumer identities.
    ///
    /// Generic test stores and stores with explorer materialized views classify as
    /// explorer. Only the exact wallet identity set classifies as wallet.
    #[must_use]
    pub fn effective_materialized_view_preset(&self) -> MaterializedViewPreset {
        let is_wallet = self.consumers.len() == WALLET_PROJECTION_CONSUMERS.len()
            && WALLET_PROJECTION_CONSUMERS
                .iter()
                .all(|schema| self.has_consumer(schema.name));
        if is_wallet {
            MaterializedViewPreset::Wallet
        } else {
            MaterializedViewPreset::Explorer
        }
    }

    /// Iterates the selected block-keyed chain-event consumer identities.
    pub fn chain_event_consumer_names(
        &self,
    ) -> impl Iterator<Item = MaterializedViewConsumerName> + '_ {
        BUNDLED_CHAIN_EVENT_CONSUMER_NAMES
            .iter()
            .copied()
            .filter(|consumer_name| self.has_consumer(*consumer_name))
    }

    /// Iterates the selected event-only chain-event consumer identities.
    pub fn event_only_chain_event_consumer_names(
        &self,
    ) -> impl Iterator<Item = MaterializedViewConsumerName> + '_ {
        BUNDLED_EVENT_ONLY_CHAIN_EVENT_CONSUMER_NAMES
            .iter()
            .copied()
            .filter(|consumer_name| self.has_consumer(*consumer_name))
    }

    /// Opens or creates a materialized-view store at `path`.
    ///
    /// Rejects with [`MaterializedViewStoreError::ConsumerColumnFamilyConflict`] when the
    /// declared consumers do not own disjoint column families. On a fresh path
    /// the store-format version and complete manifest are written atomically.
    /// On every subsequent open, the persisted container version and every
    /// declared consumer identity must match exactly. Any divergence is
    /// rejected without mutation; the operator must choose a fresh path and
    /// rebuild from a certified recovery source.
    pub fn open(
        path: impl AsRef<Path>,
        construction_identity: CanonicalStoreConstructionIdentity,
        options: MaterializedViewStoreOptions,
    ) -> Result<Self, MaterializedViewStoreError> {
        Self::open_primary(path.as_ref(), construction_identity, options, None)
    }

    /// Opens or creates a materialized-view store for one closed materialized-view preset.
    ///
    /// The durable per-consumer manifest is preflighted before the primary
    /// opens the database. Reopening a wallet store as explorer, or an explorer
    /// store as wallet, fails before that manifest can be expanded or reduced.
    pub fn open_with_materialized_view_preset(
        path: impl AsRef<Path>,
        construction_identity: CanonicalStoreConstructionIdentity,
        materialized_view_preset: MaterializedViewPreset,
        mut options: MaterializedViewStoreOptions,
    ) -> Result<Self, MaterializedViewStoreError> {
        options.consumers = materialized_view_preset.consumer_schemas();
        Self::open_primary(
            path.as_ref(),
            construction_identity,
            options,
            Some(materialized_view_preset),
        )
    }

    /// Detects the closed materialized-view preset recorded by an existing materialized-view
    /// store without opening it for writes.
    ///
    /// Returns `None` when the path is not a materialized-view `RocksDB` store. Manifests
    /// containing explorer materialized views are classified as
    /// [`MaterializedViewPreset::Explorer`]; only the exact two-identity wallet
    /// manifest is classified as wallet.
    pub fn detect_materialized_view_preset_at_path(
        path: impl AsRef<Path>,
    ) -> Result<Option<MaterializedViewPreset>, MaterializedViewStoreError> {
        Self::require_matching_store_format_at_path(path.as_ref())?;
        let existing_column_families = existing_column_family_names(path.as_ref());
        if !existing_column_families.is_empty() {
            Self::peek_canonical_construction_identity(path.as_ref(), &existing_column_families)?;
        }
        let Some(recorded_consumers) = Self::read_consumer_manifest_at_path(path.as_ref())? else {
            return Ok(None);
        };
        Self::reject_unknown_consumer_identities(&recorded_consumers)?;
        Ok(Some(Self::preset_for_recorded_consumers(
            &recorded_consumers,
        )))
    }

    fn open_primary(
        path: &Path,
        construction_identity: CanonicalStoreConstructionIdentity,
        options: MaterializedViewStoreOptions,
        materialized_view_preset: Option<MaterializedViewPreset>,
    ) -> Result<Self, MaterializedViewStoreError> {
        options
            .rocksdb_resource_budget
            .validate()
            .map_err(|reason| MaterializedViewStoreError::InvalidOptions { reason })?;
        validate_consumer_declarations(options.consumers)?;
        Self::require_matching_store_format_at_path(path)?;
        Self::require_matching_construction_identity_at_path(path, construction_identity)?;
        let existing_column_families = existing_column_family_names(path);
        let is_fresh_store = existing_column_families.is_empty();
        if let Some(materialized_view_preset) = materialized_view_preset {
            Self::preflight_materialized_view_preset_at_path(path, materialized_view_preset)?;
        }
        if !is_fresh_store {
            Self::preflight_consumer_schemas_at_path(path, options.consumers)?;
            validate_column_family_identity(&existing_column_families, options.consumers)?;
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
        .map_err(|source| MaterializedViewStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let store = Self {
            db: Arc::new(bounded_open.db),
            construction_identity,
            sync_writes: options.sync_writes,
            storage_path: path.to_path_buf(),
            consumers: options.consumers,
            rocksdb_resource_budget: options.rocksdb_resource_budget,
            is_secondary: false,
            catch_up_barrier: Arc::new(RwLock::new(())),
            terminal_catch_up_failure: Arc::new(AtomicBool::new(false)),
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
            logical_write_bytes: Arc::new(AtomicU64::new(0)),
        };
        if is_fresh_store {
            store.initialize_store_metadata()?;
        }
        store.require_matching_store_format_version()?;
        store.require_matching_construction_identity()?;
        store.validate_consumer_schemas()?;
        store.validate_chain_event_checkpoints()?;
        store.record_rocksdb_properties();
        Ok(store)
    }

    /// Opens the materialized-view store in `RocksDB` secondary mode.
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
    /// A secondary reader cannot initialize schemas, so it validates instead:
    /// the persisted container version must equal
    /// [`MATERIALIZED_VIEW_STORE_FORMAT_VERSION`], and every declared consumer's version
    /// must match the persisted manifest. A divergence returns
    /// [`MaterializedViewStoreError::SchemaMismatch`] or
    /// [`MaterializedViewStoreError::ConsumerSchemaMismatch`]; the caller
    /// retries against a fresh store after rebuilding from a certified recovery
    /// source.
    pub fn open_secondary(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        options: MaterializedViewStoreOptions,
    ) -> Result<Self, MaterializedViewStoreError> {
        Self::open_secondary_store(
            primary_path.as_ref(),
            secondary_path.as_ref(),
            options,
            None,
        )
    }

    /// Opens a secondary reader for one closed materialized-view preset.
    ///
    /// The primary must already have initialized the same durable preset.
    pub fn open_secondary_with_materialized_view_preset(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        materialized_view_preset: MaterializedViewPreset,
        mut options: MaterializedViewStoreOptions,
    ) -> Result<Self, MaterializedViewStoreError> {
        options.consumers = materialized_view_preset.consumer_schemas();
        Self::open_secondary_store(
            primary_path.as_ref(),
            secondary_path.as_ref(),
            options,
            Some(materialized_view_preset),
        )
    }

    fn open_secondary_store(
        primary_path: &Path,
        secondary_path: &Path,
        options: MaterializedViewStoreOptions,
        materialized_view_preset: Option<MaterializedViewPreset>,
    ) -> Result<Self, MaterializedViewStoreError> {
        options
            .rocksdb_resource_budget
            .validate()
            .map_err(|reason| MaterializedViewStoreError::InvalidOptions { reason })?;
        validate_consumer_declarations(options.consumers)?;
        Self::require_matching_store_format_at_path(primary_path)?;
        let existing_column_families = existing_column_family_names(primary_path);
        let construction_identity =
            Self::peek_canonical_construction_identity(primary_path, &existing_column_families)?;
        if let Some(materialized_view_preset) = materialized_view_preset {
            Self::preflight_materialized_view_preset_at_path(
                primary_path,
                materialized_view_preset,
            )?;
        }
        if !existing_column_families.is_empty() {
            Self::preflight_consumer_schemas_at_path(primary_path, options.consumers)?;
            validate_column_family_identity(&existing_column_families, options.consumers)?;
        }
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
        .map_err(|source| MaterializedViewStoreError::Open {
            path: primary_path.to_path_buf(),
            source,
        })?;
        let store = Self {
            db: Arc::new(bounded_open.db),
            construction_identity,
            sync_writes: options.sync_writes,
            storage_path: primary_path.to_path_buf(),
            consumers: options.consumers,
            rocksdb_resource_budget: options.rocksdb_resource_budget,
            is_secondary: true,
            catch_up_barrier: Arc::new(RwLock::new(())),
            terminal_catch_up_failure: Arc::new(AtomicBool::new(false)),
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            statistics: bounded_open.statistics,
            io_mode: bounded_open.io_mode,
            resource_gauge_throttle: Arc::new(ResourceGaugeThrottle::default()),
            logical_write_bytes: Arc::new(AtomicU64::new(0)),
        };
        store.require_matching_store_format_version()?;
        store.require_matching_construction_identity()?;
        store.validate_secondary_consumer_schemas()?;
        store.validate_chain_event_checkpoints()?;
        store.record_rocksdb_properties();
        Ok(store)
    }

    /// Advances the secondary instance's view to the primary's latest
    /// durable state.
    ///
    /// No-op on a primary instance: `RocksDB` returns immediately because
    /// there is no upstream MANIFEST to tail.
    pub fn try_catch_up(&self) -> Result<(), MaterializedViewStoreError> {
        if !self.is_secondary {
            return Ok(());
        }
        if self.terminal_catch_up_failure.load(Ordering::Acquire) {
            return Err(MaterializedViewStoreError::SecondaryReadsFenced);
        }
        let _catch_up_guard = self.catch_up_barrier.write();
        if self.terminal_catch_up_failure.load(Ordering::Acquire) {
            return Err(MaterializedViewStoreError::SecondaryReadsFenced);
        }
        self.try_catch_up_while_reads_blocked()
    }

    fn try_catch_up_while_reads_blocked(&self) -> Result<(), MaterializedViewStoreError> {
        let mut attempt = 1;
        loop {
            let outcome = self.try_catch_up_once();
            if outcome
                .as_ref()
                .is_err_and(is_transient_secondary_missing_sst)
                && attempt < SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS
            {
                metrics::counter!("zinder_materialized_views_secondary_catchup_retries_total")
                    .increment(1);
                tracing::debug!(
                    target: "zinder::materialized_views",
                    event = "secondary_catchup_missing_sst_retry",
                    attempt,
                    max_attempts = SECONDARY_CATCHUP_MISSING_SST_ATTEMPTS,
                    "materialized-view secondary crossed a primary-compaction file race; retrying catchup"
                );
                std::thread::yield_now();
                attempt += 1;
                continue;
            }
            if outcome.is_err() {
                self.terminal_catch_up_failure
                    .store(true, Ordering::Release);
            }
            return outcome;
        }
    }

    fn admit_secondary_read(&self) -> Result<RwLockReadGuard<'_, ()>, MaterializedViewStoreError> {
        let catch_up_guard = self.catch_up_barrier.read();
        if self.terminal_catch_up_failure.load(Ordering::Acquire) {
            return Err(MaterializedViewStoreError::SecondaryReadsFenced);
        }
        Ok(catch_up_guard)
    }

    fn try_catch_up_once(&self) -> Result<(), MaterializedViewStoreError> {
        self.db.try_catch_up_with_primary().map_err(|source| {
            MaterializedViewStoreError::Operation {
                operation: "try_catch_up_with_primary",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            }
        })?;
        self.require_matching_store_format_version()?;
        self.require_matching_construction_identity()?;
        self.validate_secondary_consumer_schemas()?;
        self.validate_chain_event_checkpoints()
    }

    /// Flushes the write-ahead log to disk so every prior write is durable.
    ///
    /// The materialized-view store opens with `sync_writes: false`, so writes land in the
    /// unsynced WAL. Before the canonical retention release floor vouches that
    /// materialized-view rows up to a height are durable, the writer fsyncs the WAL so
    /// a host crash cannot leave the floor (and the canonical deletes it
    /// authorizes) ahead of the materialized-view rows they depend on. No-op on a
    /// secondary, which owns no WAL of its own.
    pub fn flush_wal_to_disk(&self) -> Result<(), MaterializedViewStoreError> {
        if self.is_secondary {
            return Ok(());
        }
        self.db
            .flush_wal(true)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "flush_wal",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Creates a `RocksDB` checkpoint for backup or fixture capture.
    ///
    /// The checkpoint must be taken from a primary materialized-view store. Secondary
    /// readers may intentionally lag the primary, so checkpointing one would
    /// produce a stale restore image with a cursor that does not represent the
    /// writer's durable state.
    pub fn create_checkpoint(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(), MaterializedViewStoreError> {
        if self.is_secondary {
            return Err(MaterializedViewStoreError::CheckpointRequiresPrimary {
                path: self.storage_path.clone(),
            });
        }
        let checkpoint = Checkpoint::new(self.db.as_ref()).map_err(|source| {
            MaterializedViewStoreError::Checkpoint {
                path: path.as_ref().to_path_buf(),
                source,
            }
        })?;
        checkpoint
            .create_checkpoint(path.as_ref())
            .map_err(|source| MaterializedViewStoreError::Checkpoint {
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
    /// The ingest writer uses this to skip materialized-view dispatch for narrowly
    /// scoped tests that exercise canonical storage with synthetic block
    /// bytes and no materialized-view consumers. Production opens the store through
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
    ) -> Result<Vec<MaterializedViewWriteMeasurement>, MaterializedViewError>
    where
        S: BuildHasher,
    {
        let mut event_consumers: [&mut dyn MaterializedViewConsumer; 0] = [];
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
    ) -> Result<Vec<MaterializedViewWriteMeasurement>, MaterializedViewError>
    where
        S: BuildHasher,
    {
        let mut event_consumers: [&mut dyn MaterializedViewConsumer; 0] = [];
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
    ) -> Result<Vec<MaterializedViewWriteMeasurement>, MaterializedViewError>
    where
        S: BuildHasher,
    {
        self.validate_chain_event_dispatch_inputs(inputs)?;
        let ChainEventDispatchConsumers {
            block_consumers,
            event_consumers,
        } = consumers;
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store: self,
            batch: &mut batch,
        };
        let block_projection = block_projection(inputs, blocks);
        let mut measurements =
            Vec::with_capacity(block_consumers.len().saturating_add(event_consumers.len()));

        for consumer in block_consumers.iter_mut() {
            let consumer_name = consumer.name();
            let before = WriteBatchSize::capture(ctx.batch);
            let started_at = Instant::now();
            consumer
                .begin_batch(&mut ctx)
                .map_err(MaterializedViewError::Consumer)?;
            dispatch_chain_event_to_block_consumer(&mut **consumer, inputs, &mut ctx, blocks)?;
            consumer
                .finish_batch(&mut ctx)
                .map_err(MaterializedViewError::Consumer)?;
            consumer
                .stage_block_projection_state(block_projection, &mut ctx)
                .map_err(MaterializedViewError::Consumer)?;
            measurements.push(MaterializedViewWriteMeasurement::from_batch_delta(
                consumer_name,
                before,
                ctx.batch,
                started_at.elapsed(),
            ));
        }
        for consumer in event_consumers.iter_mut() {
            let consumer_name = consumer.name();
            let before = WriteBatchSize::capture(ctx.batch);
            let started_at = Instant::now();
            dispatch_chain_event_to_consumer(&mut **consumer, inputs, &mut ctx)?;
            measurements.push(MaterializedViewWriteMeasurement::from_batch_delta(
                consumer_name,
                before,
                ctx.batch,
                started_at.elapsed(),
            ));
        }

        if advance_cursor {
            let cursor_column_family =
                self.column_family(MaterializedViewStoreTable::ChainEventCursor)?;
            for consumer in block_consumers.iter() {
                stage_consumer_checkpoint_and_measure(
                    &mut batch,
                    &cursor_column_family,
                    consumer.name(),
                    inputs.checkpoint,
                    &mut measurements,
                );
            }
            for consumer in event_consumers.iter() {
                stage_consumer_checkpoint_and_measure(
                    &mut batch,
                    &cursor_column_family,
                    consumer.name(),
                    inputs.checkpoint,
                    &mut measurements,
                );
            }
        }
        self.write_batch(&batch)?;
        record_materialized_view_write_measurements(&measurements);
        Ok(measurements)
    }

    fn validate_chain_event_dispatch_inputs(
        &self,
        inputs: ChainEventDispatchInputs<'_>,
    ) -> Result<(), MaterializedViewError> {
        if inputs.chain_epoch.network != self.network() {
            return Err(MaterializedViewStoreError::ChainEventNetworkMismatch {
                expected: self.network(),
                observed: inputs.chain_epoch.network,
            }
            .into());
        }
        let resulting_fence = inputs.checkpoint.resulting_fence();
        if resulting_fence.chain_epoch_id() != inputs.chain_epoch.id
            || resulting_fence.visible_tip().height != inputs.chain_epoch.visible_tip_height
            || resulting_fence.visible_tip().hash != inputs.chain_epoch.visible_tip_hash
        {
            return Err(
                MaterializedViewStoreError::ChainEventCheckpointEpochMismatch {
                    event_sequence: inputs.checkpoint.cursor().event_sequence(),
                }
                .into(),
            );
        }
        Ok(())
    }

    /// Dispatches one mempool consumer and atomically writes its rows plus
    /// cursor advance.
    pub fn write_mempool_event(
        &self,
        consumer: &mut dyn MaterializedViewMempoolConsumer,
        event: &crate::consumer::MempoolConsumerEvent<'_>,
        cursor_bytes: &[u8],
    ) -> Result<MaterializedViewWriteMeasurement, MaterializedViewError> {
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store: self,
            batch: &mut batch,
        };
        let consumer_name = consumer.name();
        let before = WriteBatchSize::capture(ctx.batch);
        let started_at = Instant::now();
        consumer
            .apply_mempool_event(event, &mut ctx)
            .map_err(MaterializedViewError::Consumer)?;
        let mut measurement = MaterializedViewWriteMeasurement::from_batch_delta(
            consumer_name,
            before,
            ctx.batch,
            started_at.elapsed(),
        );
        let cursor_before = WriteBatchSize::capture(&batch);
        self.stage_mempool_event_cursor_advance(&mut batch, consumer_name, cursor_bytes)?;
        measurement.add_batch_delta(cursor_before, &batch);
        self.write_batch(&batch)?;
        record_materialized_view_write_measurements(std::slice::from_ref(&measurement));
        Ok(measurement)
    }

    /// Reads a chain-event consumer's exact persisted checkpoint, when present.
    pub fn chain_event_checkpoint(
        &self,
        consumer: MaterializedViewConsumerName,
    ) -> Result<Option<MaterializedViewChainEventCheckpoint>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        self.get(
            MaterializedViewStoreTable::ChainEventCursor,
            consumer.as_str().as_bytes(),
        )?
        .map(|encoded| decode_chain_event_checkpoint(consumer, &encoded))
        .transpose()
    }

    /// Atomically persists one authenticated checkpoint for a chain consumer.
    ///
    /// Each call commits its own `WriteBatch`. Consumers that need to bundle
    /// checkpoint advances with their own data writes use [`Self::write_batch`]
    /// instead.
    pub fn put_chain_event_checkpoint(
        &self,
        consumer: MaterializedViewConsumerName,
        checkpoint: MaterializedViewChainEventCheckpoint,
    ) -> Result<(), MaterializedViewStoreError> {
        let mut batch = WriteBatch::default();
        self.stage_chain_event_checkpoint(&mut batch, consumer, checkpoint)?;
        self.write(&batch)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "put_chain_event_checkpoint",
                column_family: MaterializedViewStoreColumnFamily::ChainEventCursor,
                source,
            })?;
        self.record_consumer_batch_if_selected(consumer, &batch);
        Ok(())
    }

    /// Refreshes exported `RocksDB` resource and I/O metrics immediately.
    ///
    /// Normal writes sample these metrics through a one-second throttle. A
    /// bounded benchmark or an operator snapshot can call this after a short
    /// write burst so the exported ticker gauges include the completed work.
    pub fn refresh_rocksdb_resource_metrics(&self) {
        self.record_rocksdb_properties();
    }

    /// Returns serialized bytes submitted in successful write batches since
    /// this process opened the store.
    #[must_use]
    pub fn logical_write_bytes(&self) -> u64 {
        self.logical_write_bytes.load(Ordering::Relaxed)
    }

    /// Stages one chain-event checkpoint in a caller-owned atomic write batch.
    ///
    /// Snapshot-backed consumers use this to activate materialized state and
    /// adopt the event boundary in the same commit.
    pub fn stage_chain_event_checkpoint(
        &self,
        batch: &mut WriteBatch,
        consumer: MaterializedViewConsumerName,
        checkpoint: MaterializedViewChainEventCheckpoint,
    ) -> Result<(), MaterializedViewStoreError> {
        if !self
            .chain_event_consumer_names()
            .chain(self.event_only_chain_event_consumer_names())
            .any(|selected| selected == consumer)
        {
            return Err(MaterializedViewStoreError::ConsumerNotSelected {
                consumer: consumer.as_str(),
            });
        }
        let column_family = self.column_family(MaterializedViewStoreTable::ChainEventCursor)?;
        batch.put_cf(
            &column_family,
            consumer.as_str().as_bytes(),
            encode_chain_event_checkpoint(checkpoint),
        );
        Ok(())
    }

    /// Atomically persists `cursor_bytes` for a mempool-event consumer.
    pub fn put_mempool_event_cursor(
        &self,
        consumer: MaterializedViewConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), MaterializedViewStoreError> {
        let mut batch = WriteBatch::default();
        let column_family = self.column_family(MaterializedViewStoreTable::MempoolEventCursor)?;
        batch.put_cf(&column_family, consumer.as_str().as_bytes(), cursor_bytes);
        self.write(&batch)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "put_mempool_event_cursor",
                column_family: MaterializedViewStoreColumnFamily::MempoolEventCursor,
                source,
            })
    }

    /// Returns the persisted store-format version recorded under
    /// `consumer_metadata`.
    pub fn store_format_version(&self) -> Result<u16, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        self.store_format_version_unfenced()
    }

    fn store_format_version_unfenced(&self) -> Result<u16, MaterializedViewStoreError> {
        let Some(bytes) = self.get(
            MaterializedViewStoreTable::ConsumerMetadata,
            STORE_FORMAT_VERSION_KEY,
        )?
        else {
            return Err(MaterializedViewStoreError::SchemaMismatch {
                persisted: 0,
                running: MATERIALIZED_VIEW_STORE_FORMAT_VERSION,
            });
        };
        decode_store_format_version(&bytes).map_err(|reason| MaterializedViewStoreError::Decode {
            column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            reason,
        })
    }

    /// Commits a prepared `WriteBatch` to the database.
    ///
    /// Consumers use this to bundle a cursor write together with their own
    /// data writes so the persisted cursor never advances without the
    /// underlying state having reached durability.
    pub fn write_batch(&self, batch: &WriteBatch) -> Result<(), MaterializedViewStoreError> {
        self.write(batch)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "write_batch",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })
    }

    /// Commits a batch owned by one stable consumer identity and attributes
    /// its successful writes to that consumer's operational counters.
    ///
    /// Callers use this for backfill, seed, and repair batches that do not pass
    /// through chain-event dispatch. The batch must contain only rows and
    /// metadata owned by `consumer`.
    pub fn write_consumer_batch(
        &self,
        consumer: MaterializedViewConsumerName,
        batch: &WriteBatch,
    ) -> Result<(), MaterializedViewStoreError> {
        if !self.has_consumer(consumer) {
            return Err(MaterializedViewStoreError::ConsumerNotSelected {
                consumer: consumer.as_str(),
            });
        }
        self.write_batch(batch)?;
        self.record_consumer_batch_if_selected(consumer, batch);
        Ok(())
    }

    fn record_consumer_batch_if_selected(
        &self,
        consumer: MaterializedViewConsumerName,
        batch: &WriteBatch,
    ) {
        if !self.has_consumer(consumer) {
            return;
        }
        record_materialized_view_write_measurements(std::slice::from_ref(
            &MaterializedViewWriteMeasurement {
                consumer,
                operations: usize_to_u64_saturating(batch.len()),
                logical_bytes: usize_to_u64_saturating(batch.size_in_bytes()),
                dispatch_duration: Duration::ZERO,
            },
        ));
    }

    /// Returns a column-family handle the caller can use when staging puts
    /// directly into a [`WriteBatch`].
    pub fn column_family(
        &self,
        table: MaterializedViewStoreTable,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, MaterializedViewStoreError> {
        self.db
            .cf_handle(table.column_family_name())
            .ok_or_else(|| MaterializedViewStoreError::ColumnFamilyMissing {
                column_family: table.error_family(),
            })
    }

    /// Returns a handle for a consumer-owned column family registered through
    /// [`MaterializedViewStoreOptions::consumers`]. Consumers stage puts and deletes by
    /// calling `batch.put_cf(handle, key, value)` on the returned handle and
    /// committing through [`Self::write_batch`].
    pub fn consumer_column_family(
        &self,
        name: &'static str,
    ) -> Result<Arc<rust_rocksdb::BoundColumnFamily<'_>>, MaterializedViewStoreError> {
        if !self.owns_consumer_column_family(name) {
            return Err(MaterializedViewStoreError::ConsumerColumnFamilyMissing { name });
        }
        self.db
            .cf_handle(name)
            .ok_or(MaterializedViewStoreError::ConsumerColumnFamilyMissing { name })
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
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        self.db.get_cf(&handle, key).map_err(|source| {
            MaterializedViewStoreError::ConsumerOperation {
                operation: "get",
                name: column_family,
                source,
            }
        })
    }

    /// Writes a single value into a consumer-owned column family.
    pub fn put_consumer(
        &self,
        column_family: &'static str,
        key: &[u8],
        bytes: &[u8],
    ) -> Result<(), MaterializedViewStoreError> {
        let handle = self.consumer_column_family(column_family)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&handle, key, bytes);
        self.write(&batch)
            .map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                operation: "put",
                name: column_family,
                source,
            })?;
        if let Some(consumer_name) = self
            .consumers
            .iter()
            .find(|schema| schema.column_families.contains(&column_family))
            .map(|schema| schema.name)
        {
            self.record_consumer_batch_if_selected(consumer_name, &batch);
        }
        Ok(())
    }

    /// Persists the ingest plane's materialized-view status record so the explorer plane
    /// can surface it on `ServerInfo`. Opaque bytes by design: the store stays
    /// free of the explorer wire types, matching how consumer payloads are
    /// handled. See [`Self::get_materialized_view_status`].
    pub fn put_materialized_view_status(
        &self,
        bytes: &[u8],
    ) -> Result<(), MaterializedViewStoreError> {
        self.put(
            MaterializedViewStoreTable::ConsumerMetadata,
            MATERIALIZED_VIEW_STATUS_KEY,
            bytes,
        )
    }

    /// Reads the materialized-view status record the ingest plane writes each replay
    /// tick, or `None` when ingest has not written one yet.
    pub fn get_materialized_view_status(
        &self,
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        self.get(
            MaterializedViewStoreTable::ConsumerMetadata,
            MATERIALIZED_VIEW_STATUS_KEY,
        )
    }

    /// Reads one consumer's atomic state and verified coverage.
    pub fn consumer_state(
        &self,
        consumer: MaterializedViewConsumerName,
    ) -> Result<Option<MaterializedViewState>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let key = consumer_state_key(consumer.as_str());
        self.get(MaterializedViewStoreTable::ConsumerMetadata, &key)?
            .map(|payload| decode_materialized_view_state(consumer, &payload))
            .transpose()
    }

    /// Stages one consumer's state in a caller-owned atomic batch.
    pub fn stage_consumer_state(
        &self,
        batch: &mut WriteBatch,
        consumer: MaterializedViewConsumerName,
        state: MaterializedViewState,
    ) -> Result<(), MaterializedViewStoreError> {
        validate_materialized_view_coverage_bounds(consumer, &state)?;
        let column_family = self.column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        batch.put_cf(
            &column_family,
            consumer_state_key(consumer.as_str()),
            encode_materialized_view_state(state),
        );
        Ok(())
    }

    /// Atomically persists one consumer's state.
    pub fn put_consumer_state(
        &self,
        consumer: MaterializedViewConsumerName,
        state: MaterializedViewState,
    ) -> Result<(), MaterializedViewStoreError> {
        let mut batch = WriteBatch::default();
        self.stage_consumer_state(&mut batch, consumer, state)?;
        self.write_batch(&batch)?;
        self.record_consumer_batch_if_selected(consumer, &batch);
        Ok(())
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
    ) -> Result<Vec<Option<Vec<u8>>>, MaterializedViewStoreError>
    where
        K: AsRef<[u8]>,
    {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
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
            let bytes =
                outcome.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<Vec<ConsumerEntry>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
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
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    /// Rows before `offset` are scanned but never retained, so offset
    /// pagination cannot allocate in proportion to an untrusted offset.
    pub fn page_consumer_range(
        &self,
        column_family: &'static str,
        key_range: std::ops::RangeInclusive<&[u8]>,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<ConsumerEntry>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
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
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    pub fn consumer_row_count(
        &self,
        column_family: &'static str,
    ) -> Result<u64, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let mut row_count = 0_u64;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<(), MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                    operation: "visit_rows",
                    name: column_family,
                    source,
                })?;
            visitor(&key, &payload).map_err(|reason| {
                MaterializedViewStoreError::ConsumerPayloadDecode {
                    name: column_family,
                    reason,
                }
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
    ) -> Result<(), MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let (start_key, end_key_inclusive) = key_range.into_inner();
        let iterator = self.db.iterator_cf(
            &handle,
            IteratorMode::From(start_key, rust_rocksdb::Direction::Forward),
        );
        for entry in iterator {
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                    operation: "visit_range",
                    name: column_family,
                    source,
                })?;
            if key.as_ref() > end_key_inclusive {
                break;
            }
            visitor(&key, &payload).map_err(|reason| {
                MaterializedViewStoreError::ConsumerPayloadDecode {
                    name: column_family,
                    reason,
                }
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
    ) -> Result<u64, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let mut matching_count = 0_u64;
        for entry in self.db.iterator_cf(&handle, IteratorMode::Start) {
            let (key, payload) =
                entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                    operation: "count_matching_rows",
                    name: column_family,
                    source,
                })?;
            if predicate(&key, &payload).map_err(|reason| {
                MaterializedViewStoreError::ConsumerPayloadDecode {
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
    /// scan to compute materialized-view cursor lag at request time.
    pub fn last_consumer_key(
        &self,
        column_family: &'static str,
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
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
    ) -> Result<Option<ConsumerEntry>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::End);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                operation: "last_entry",
                name: column_family,
                source,
            })?;
        Ok(Some((key.to_vec(), payload.to_vec())))
    }

    /// Returns the highest height materialized in an ascending-height
    /// materialized-view column family, or `None` when the column family is empty.
    ///
    /// Decodes the lexicographically last key as a four-byte big-endian
    /// height via [`zinder_core::wire::decode_height_key_ascending`]. Use
    /// this on column families whose primary key is exactly four bytes of
    /// ascending height (the `BlockSummary` materialized view). Returns
    /// [`MaterializedViewStoreError::Decode`] when the last key is not four bytes,
    /// which signals a column-family schema mismatch and should fail loudly.
    pub fn last_materialized_height_ascending(
        &self,
        column_family: &'static str,
    ) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
        let Some(key) = self.last_consumer_key(column_family)? else {
            return Ok(None);
        };
        zinder_core::wire::decode_height_key_ascending(&key)
            .map(Some)
            .map_err(|error| MaterializedViewStoreError::Decode {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` last key is not a 4-byte ascending height: {error}"
                ),
            })
    }

    /// Returns the highest height materialized in a descending-height
    /// materialized-view column family, or `None` when the column family is empty.
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
    ) -> Result<Option<BlockHeight>, MaterializedViewStoreError> {
        let _secondary_read_guard = self
            .is_secondary
            .then(|| self.admit_secondary_read())
            .transpose()?;
        let handle = self.consumer_column_family(column_family)?;
        let mut iterator = self.db.iterator_cf(&handle, IteratorMode::Start);
        let Some(entry) = iterator.next() else {
            return Ok(None);
        };
        let (key, _payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerOperation {
                operation: "first_key",
                name: column_family,
                source,
            })?;
        let prefix = key.get(..zinder_core::wire::HEIGHT_KEY_LEN).ok_or_else(|| {
            MaterializedViewStoreError::Decode {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` first key is shorter than the descending-height prefix"
                ),
            }
        })?;
        zinder_core::wire::decode_height_key_descending(prefix)
            .map(Some)
            .map_err(|error| MaterializedViewStoreError::Decode {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                reason: format!(
                    "consumer column family `{column_family}` first key descending-height prefix invalid: {error}"
                ),
            })
    }

    fn stage_mempool_event_cursor_advance(
        &self,
        batch: &mut WriteBatch,
        consumer_name: MaterializedViewConsumerName,
        cursor_bytes: &[u8],
    ) -> Result<(), MaterializedViewStoreError> {
        let cf = self.column_family(MaterializedViewStoreTable::MempoolEventCursor)?;
        batch.put_cf(&cf, consumer_name.as_str().as_bytes(), cursor_bytes);
        Ok(())
    }

    /// Fails unless the persisted container version equals the running one.
    ///
    /// Secondary readers cannot initialize or migrate, so they reject a
    /// divergent container version instead of writing to the store.
    fn require_matching_store_format_version(&self) -> Result<(), MaterializedViewStoreError> {
        let persisted = self.store_format_version_unfenced()?;
        if persisted == MATERIALIZED_VIEW_STORE_FORMAT_VERSION {
            Ok(())
        } else {
            Err(MaterializedViewStoreError::SchemaMismatch {
                persisted,
                running: MATERIALIZED_VIEW_STORE_FORMAT_VERSION,
            })
        }
    }

    /// Fails unless the opened store carries the admitted construction identity.
    fn require_matching_construction_identity(&self) -> Result<(), MaterializedViewStoreError> {
        let Some(bytes) = self.get(
            MaterializedViewStoreTable::ConsumerMetadata,
            STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY,
        )?
        else {
            return Err(MaterializedViewStoreError::CanonicalConstructionIdentityMissing);
        };
        let observed = decode_canonical_construction_identity(&bytes)?;
        require_matching_construction_identity(self.construction_identity, observed)
    }

    /// Rejects a secondary open whose consumer declaration cannot safely read
    /// the persisted manifest.
    fn validate_secondary_consumer_schemas(&self) -> Result<(), MaterializedViewStoreError> {
        self.validate_consumer_schemas()
    }

    /// Validates every persisted chain-event checkpoint before rows are read.
    fn validate_chain_event_checkpoints(&self) -> Result<(), MaterializedViewStoreError> {
        let column_family = self.column_family(MaterializedViewStoreTable::ChainEventCursor)?;
        for entry in self.db.iterator_cf(&column_family, IteratorMode::Start) {
            let (consumer_bytes, encoded) =
                entry.map_err(|source| MaterializedViewStoreError::Operation {
                    operation: "validate_chain_event_checkpoints",
                    column_family: MaterializedViewStoreColumnFamily::ChainEventCursor,
                    source,
                })?;
            let Some(consumer) = self
                .chain_event_consumer_names()
                .chain(self.event_only_chain_event_consumer_names())
                .find(|name| name.as_str().as_bytes() == consumer_bytes.as_ref())
            else {
                return Err(MaterializedViewStoreError::ConsumerManifest {
                    operation: "validate_chain_event_checkpoints",
                    reason: format!(
                        "checkpoint belongs to non-chain consumer `{}`",
                        String::from_utf8_lossy(&consumer_bytes)
                    ),
                });
            };
            decode_chain_event_checkpoint(consumer, &encoded)?;
        }
        Ok(())
    }

    /// Rejects a persisted container version different from the running
    /// format without opening the store for mutation.
    fn require_matching_store_format_at_path(
        path: &Path,
    ) -> Result<(), MaterializedViewStoreError> {
        let Some(persisted) = Self::peek_store_format_version(path)? else {
            return Ok(());
        };
        if persisted == MATERIALIZED_VIEW_STORE_FORMAT_VERSION {
            return Ok(());
        }
        Err(MaterializedViewStoreError::SchemaMismatch {
            persisted,
            running: MATERIALIZED_VIEW_STORE_FORMAT_VERSION,
        })
    }

    /// Rejects an existing store whose construction identity differs from the opener.
    fn require_matching_construction_identity_at_path(
        path: &Path,
        expected: CanonicalStoreConstructionIdentity,
    ) -> Result<(), MaterializedViewStoreError> {
        let existing_column_families = existing_column_family_names(path);
        if existing_column_families.is_empty() {
            return Ok(());
        }
        let observed = Self::peek_canonical_construction_identity(path, &existing_column_families)?;
        require_matching_construction_identity(expected, observed)
    }

    fn peek_canonical_construction_identity(
        path: &Path,
        existing_column_families: &[String],
    ) -> Result<CanonicalStoreConstructionIdentity, MaterializedViewStoreError> {
        let db =
            DB::open_cf_for_read_only(&Options::default(), path, existing_column_families, false)
                .map_err(|source| MaterializedViewStoreError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        let column_family = db
            .cf_handle(MaterializedViewStoreTable::ConsumerMetadata.column_family_name())
            .ok_or(MaterializedViewStoreError::ColumnFamilyMissing {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            })?;
        let Some(bytes) = db
            .get_cf(&column_family, STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })?
        else {
            return Err(MaterializedViewStoreError::CanonicalConstructionIdentityMissing);
        };
        decode_canonical_construction_identity(&bytes)
    }

    /// Reads the persisted container version without keeping the store open.
    ///
    /// Returns `None` when `path` holds no materialized-view store yet. The store is
    /// opened read-only and closed before the caller decides whether it may open
    /// the current format.
    fn peek_store_format_version(path: &Path) -> Result<Option<u16>, MaterializedViewStoreError> {
        let existing_column_families = existing_column_family_names(path);
        if existing_column_families.is_empty() {
            return Ok(None);
        }
        let db =
            DB::open_cf_for_read_only(&Options::default(), path, &existing_column_families, false)
                .map_err(|source| MaterializedViewStoreError::Open {
                    path: path.to_path_buf(),
                    source,
                })?;
        let column_family = db
            .cf_handle(MaterializedViewStoreTable::ConsumerMetadata.column_family_name())
            .ok_or(MaterializedViewStoreError::ColumnFamilyMissing {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            })?;
        let persisted = db
            .get_cf(&column_family, STORE_FORMAT_VERSION_KEY)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                source,
            })?
            .map(|bytes| decode_store_format_version(&bytes))
            .transpose()
            .map_err(|reason| MaterializedViewStoreError::Decode {
                column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
                reason,
            })?;
        Ok(persisted)
    }

    /// Reads the existing per-consumer manifest without opening the store for
    /// writes or creating column families.
    fn preflight_materialized_view_preset_at_path(
        path: &Path,
        requested: MaterializedViewPreset,
    ) -> Result<(), MaterializedViewStoreError> {
        Self::require_matching_store_format_at_path(path)?;
        let Some(recorded_consumers) = Self::read_consumer_manifest_at_path(path)? else {
            return Ok(());
        };
        Self::validate_preset_consumer_identities(requested, &recorded_consumers)
    }

    fn read_consumer_manifest_at_path(
        path: &Path,
    ) -> Result<Option<BTreeMap<String, ConsumerManifestEntry>>, MaterializedViewStoreError> {
        let existing_column_families = existing_column_family_names(path);
        if existing_column_families.is_empty() {
            return Ok(None);
        }
        let db =
            DB::open_cf_for_read_only(&Options::default(), path, &existing_column_families, false)
                .map_err(|source| MaterializedViewStoreError::Open {
                    path: path.to_path_buf(),
                    source,
                })?;
        let Some(column_family) =
            db.cf_handle(MaterializedViewStoreTable::ConsumerMetadata.column_family_name())
        else {
            return Ok(Some(BTreeMap::new()));
        };
        let iterator = db.iterator_cf(
            &column_family,
            IteratorMode::From(CONSUMER_SCHEMA_KEY_PREFIX, rust_rocksdb::Direction::Forward),
        );
        let recorded_consumers = decode_consumer_manifest_entries(iterator)?;
        Ok(Some(recorded_consumers))
    }

    /// Writes the format, canonical construction, and full manifest atomically.
    ///
    /// A crash before this batch commits leaves no initialized store metadata
    /// to reopen; a crash after it commits leaves a complete identity.
    fn initialize_store_metadata(&self) -> Result<(), MaterializedViewStoreError> {
        let metadata = self.column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(
            &metadata,
            STORE_FORMAT_VERSION_KEY,
            MATERIALIZED_VIEW_STORE_FORMAT_VERSION.to_be_bytes(),
        );
        batch.put_cf(
            &metadata,
            STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY,
            self.construction_identity.encode_persisted(),
        );
        for consumer in self.consumers {
            let payload = encode_manifest_entry(consumer.schema_version, consumer.column_families)
                .map_err(|reason| MaterializedViewStoreError::ConsumerManifest {
                    operation: "encode_manifest_entry",
                    reason,
                })?;
            batch.put_cf(
                &metadata,
                consumer_schema_manifest_key(consumer.name.as_str()),
                payload,
            );
        }
        self.write_batch(&batch)
    }

    fn preflight_consumer_schemas_at_path(
        path: &Path,
        consumers: &[MaterializedViewConsumerSchema],
    ) -> Result<(), MaterializedViewStoreError> {
        let Some(recorded) = Self::read_consumer_manifest_at_path(path)? else {
            return Ok(());
        };
        Self::validate_declared_consumer_schemas(consumers, &recorded)
    }

    fn validate_consumer_schemas(&self) -> Result<(), MaterializedViewStoreError> {
        let recorded = self.read_consumer_manifest()?;
        Self::validate_declared_consumer_schemas(self.consumers, &recorded)
    }

    fn validate_preset_consumer_identities(
        requested: MaterializedViewPreset,
        recorded_consumers: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), MaterializedViewStoreError> {
        Self::reject_unknown_consumer_identities(recorded_consumers)?;
        let requested_consumers = requested.consumer_schemas();
        let exact_identity_set = recorded_consumers.len() == requested_consumers.len()
            && requested_consumers
                .iter()
                .all(|consumer| recorded_consumers.contains_key(consumer.name.as_str()));
        if exact_identity_set {
            Ok(())
        } else {
            Err(
                MaterializedViewStoreError::MaterializedViewPresetRequiresFreshStore {
                    requested: requested.as_str(),
                },
            )
        }
    }

    fn reject_unknown_consumer_identities(
        recorded_consumers: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), MaterializedViewStoreError> {
        for (name, entry) in recorded_consumers {
            if BUNDLED_CONSUMERS
                .iter()
                .all(|consumer| consumer.name.as_str() != name)
            {
                return Err(MaterializedViewStoreError::ConsumerNotDeclared {
                    consumer: name.clone(),
                    persisted_schema_version: entry.schema_version,
                });
            }
        }
        Ok(())
    }

    fn preset_for_recorded_consumers(
        recorded_consumers: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> MaterializedViewPreset {
        let recorded_is_wallet = recorded_consumers.len() == WALLET_PROJECTION_CONSUMERS.len()
            && WALLET_PROJECTION_CONSUMERS
                .iter()
                .all(|schema| recorded_consumers.contains_key(schema.name.as_str()));
        if recorded_is_wallet {
            MaterializedViewPreset::Wallet
        } else {
            MaterializedViewPreset::Explorer
        }
    }

    fn validate_declared_consumer_schemas(
        declared: &[MaterializedViewConsumerSchema],
        recorded: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), MaterializedViewStoreError> {
        for (name, entry) in recorded {
            if declared
                .iter()
                .all(|consumer| consumer.name.as_str() != name)
            {
                return Err(MaterializedViewStoreError::ConsumerNotDeclared {
                    consumer: name.clone(),
                    persisted_schema_version: entry.schema_version,
                });
            }
        }
        for consumer in declared {
            let entry = recorded.get(consumer.name.as_str());
            if !entry.is_some_and(|entry| Self::consumer_manifest_is_exact(consumer, entry)) {
                return Err(MaterializedViewStoreError::ConsumerSchemaMismatch {
                    consumer: consumer.name.as_str(),
                    persisted: entry.map(|entry| entry.schema_version),
                    running: consumer.schema_version,
                });
            }
        }
        Ok(())
    }

    fn consumer_manifest_is_exact(
        consumer: &MaterializedViewConsumerSchema,
        recorded: &ConsumerManifestEntry,
    ) -> bool {
        recorded.schema_version == consumer.schema_version
            && Self::consumer_column_families_match(consumer, recorded)
    }

    fn consumer_column_families_match(
        consumer: &MaterializedViewConsumerSchema,
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

    fn read_consumer_manifest(
        &self,
    ) -> Result<BTreeMap<String, ConsumerManifestEntry>, MaterializedViewStoreError> {
        let column_family = self.column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        let iterator = self.db.iterator_cf(
            &column_family,
            IteratorMode::From(CONSUMER_SCHEMA_KEY_PREFIX, rust_rocksdb::Direction::Forward),
        );
        decode_consumer_manifest_entries(iterator)
    }

    fn get(
        &self,
        table: MaterializedViewStoreTable,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, MaterializedViewStoreError> {
        let column_family = self.column_family(table)?;
        self.db.get_cf(&column_family, key).map_err(|source| {
            MaterializedViewStoreError::Operation {
                operation: "get",
                column_family: table.error_family(),
                source,
            }
        })
    }

    fn put(
        &self,
        table: MaterializedViewStoreTable,
        key: &[u8],
        bytes: &[u8],
    ) -> Result<(), MaterializedViewStoreError> {
        let column_family = self.column_family(table)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&column_family, key, bytes);
        self.write(&batch)
            .map_err(|source| MaterializedViewStoreError::Operation {
                operation: "put",
                column_family: table.error_family(),
                source,
            })
    }

    fn write(&self, batch: &WriteBatch) -> Result<(), rust_rocksdb::Error> {
        let batch_bytes = u64::try_from(batch.size_in_bytes()).unwrap_or(u64::MAX);
        let mut write_options = WriteOptions::default();
        write_options.set_sync(self.sync_writes);
        self.db.write_opt(batch, &write_options)?;
        self.logical_write_bytes
            .fetch_add(batch_bytes, Ordering::Relaxed);
        if self.resource_gauge_throttle.should_sample() {
            self.record_rocksdb_properties();
        }
        Ok(())
    }

    fn record_rocksdb_properties(&self) {
        let mut column_family_names = MaterializedViewStoreTable::all()
            .into_iter()
            .map(MaterializedViewStoreTable::column_family_name)
            .collect::<Vec<_>>();
        for consumer in self.consumers {
            column_family_names.extend_from_slice(consumer.column_families);
        }
        let store_role = if self.is_secondary {
            StoreRole::MaterializedViewSecondary
        } else {
            StoreRole::MaterializedViewPrimary
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
        self.record_materialized_view_rocksdb_properties(store_role);
    }

    fn record_materialized_view_rocksdb_properties(&self, store_role: StoreRole) {
        for consumer in self.consumers {
            for property in MATERIALIZED_VIEW_ROCKSDB_PROPERTIES {
                let mut aggregate = 0_u64;
                let mut sampled = false;
                for column_family_name in consumer.column_families {
                    let Some(column_family) = self.db.cf_handle(column_family_name) else {
                        continue;
                    };
                    let Ok(Some(property_sample)) =
                        self.db.property_int_value_cf(&column_family, property)
                    else {
                        continue;
                    };
                    aggregate = aggregate.saturating_add(property_sample);
                    sampled = true;
                }
                if sampled {
                    metrics::gauge!(
                        "zinder_materialized_view_rocksdb_property",
                        "consumer" => consumer.name.as_str(),
                        "property" => property,
                        "store_role" => store_role.as_str()
                    )
                    .set(u64_to_f64(aggregate));
                }
            }
        }
    }
}

fn stage_consumer_checkpoint_and_measure(
    batch: &mut WriteBatch,
    cursor_column_family: &Arc<rust_rocksdb::BoundColumnFamily<'_>>,
    consumer: MaterializedViewConsumerName,
    checkpoint: MaterializedViewChainEventCheckpoint,
    measurements: &mut [MaterializedViewWriteMeasurement],
) {
    let before = WriteBatchSize::capture(batch);
    batch.put_cf(
        cursor_column_family,
        consumer.as_str().as_bytes(),
        encode_chain_event_checkpoint(checkpoint),
    );
    if let Some(measurement) = measurements
        .iter_mut()
        .find(|measurement| measurement.consumer == consumer)
    {
        measurement.add_batch_delta(before, batch);
    }
}

fn record_materialized_view_write_measurements(measurements: &[MaterializedViewWriteMeasurement]) {
    for measurement in measurements {
        let consumer = measurement.consumer.as_str();
        metrics::counter!(
            "zinder_materialized_view_write_operations_total",
            "consumer" => consumer
        )
        .increment(measurement.operations);
        metrics::counter!(
            "zinder_materialized_view_write_bytes_total",
            "consumer" => consumer
        )
        .increment(measurement.logical_bytes);
    }
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges are f64 and approximate very large byte counters"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

fn dispatch_chain_event_to_block_consumer<C, S>(
    consumer: &mut C,
    inputs: ChainEventDispatchInputs<'_>,
    ctx: &mut MaterializedViewConsumerCtx<'_>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
) -> Result<(), MaterializedViewError>
where
    C: BlockKeyedConsumer + ?Sized,
    S: BuildHasher,
{
    match inputs.chain_event {
        ChainEvent::ChainCommitted { committed } => {
            let event = ChainCommittedEvent::new(
                inputs.checkpoint.cursor().event_sequence(),
                inputs.chain_epoch,
                inputs.settled_tip_height,
                committed.block_range.start,
                committed.block_range.end,
            );
            apply_chain_committed_in_memory(consumer, &event, ctx, blocks)
                .map_err(MaterializedViewError::Consumer)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            let event = ChainReorgedEvent::new(
                inputs.checkpoint.cursor().event_sequence(),
                inputs.chain_epoch,
                inputs.settled_tip_height,
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
                .map_err(MaterializedViewError::Consumer)
        }
        _ => Err(MaterializedViewError::UnsupportedChainEvent),
    }
}

fn block_projection<'event, S>(
    inputs: ChainEventDispatchInputs<'event>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>, S>,
) -> MaterializedViewBlockProjection<'event>
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
    MaterializedViewBlockProjection {
        chain_epoch: inputs.chain_epoch,
        chain_event: inputs.chain_event,
        tip_height: projected_tip.map(|(height, _hash)| height),
        tip_hash: projected_tip.map(|(_height, hash)| hash),
    }
}

fn dispatch_chain_event_to_consumer<C>(
    consumer: &mut C,
    inputs: ChainEventDispatchInputs<'_>,
    ctx: &mut MaterializedViewConsumerCtx<'_>,
) -> Result<(), MaterializedViewError>
where
    C: MaterializedViewConsumer + ?Sized,
{
    match inputs.chain_event {
        ChainEvent::ChainCommitted { committed } => {
            let event = ChainCommittedEvent::new(
                inputs.checkpoint.cursor().event_sequence(),
                inputs.chain_epoch,
                inputs.settled_tip_height,
                committed.block_range.start,
                committed.block_range.end,
            );
            consumer
                .apply_chain_committed(&event, ctx)
                .map_err(MaterializedViewError::Consumer)
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            let event = ChainReorgedEvent::new(
                inputs.checkpoint.cursor().event_sequence(),
                inputs.chain_epoch,
                inputs.settled_tip_height,
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
                .map_err(MaterializedViewError::Consumer)
        }
        _ => Err(MaterializedViewError::UnsupportedChainEvent),
    }
}

fn decode_store_format_version(bytes: &[u8]) -> Result<u16, String> {
    let array: [u8; 2] = bytes
        .try_into()
        .map_err(|_| format!("store format version requires 2 bytes; got {}", bytes.len()))?;
    Ok(u16::from_be_bytes(array))
}

fn decode_canonical_construction_identity(
    bytes: &[u8],
) -> Result<CanonicalStoreConstructionIdentity, MaterializedViewStoreError> {
    CanonicalStoreConstructionIdentity::decode_persisted(bytes).map_err(|source| {
        MaterializedViewStoreError::CanonicalConstructionIdentityMalformed { source }
    })
}

fn encode_chain_event_checkpoint(
    checkpoint: MaterializedViewChainEventCheckpoint,
) -> [u8; MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES] {
    let mut encoded = [0; MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES];
    encoded[0] = MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_VERSION;
    encoded[1..10].copy_from_slice(&checkpoint.cursor().as_bytes());
    encoded[10..105].copy_from_slice(&checkpoint.resulting_fence().encode_persisted());
    encoded
}

fn decode_chain_event_checkpoint(
    consumer: MaterializedViewConsumerName,
    encoded: &[u8],
) -> Result<MaterializedViewChainEventCheckpoint, MaterializedViewStoreError> {
    if encoded.len() != MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES {
        return Err(MaterializedViewStoreError::ChainEventCheckpointMalformed {
            consumer: consumer.as_str(),
            reason: format!(
                "checkpoint requires {MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES} bytes; observed {}",
                encoded.len()
            ),
        });
    }
    if encoded[0] != MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_VERSION {
        return Err(MaterializedViewStoreError::ChainEventCheckpointMalformed {
            consumer: consumer.as_str(),
            reason: format!("unsupported checkpoint version {}", encoded[0]),
        });
    }
    let cursor = CanonicalEventCursor::from_persisted(&encoded[1..10]).map_err(|source| {
        MaterializedViewStoreError::ChainEventCheckpointMalformed {
            consumer: consumer.as_str(),
            reason: source.to_string(),
        }
    })?;
    let resulting_fence =
        CanonicalEventFence::decode_persisted(&encoded[10..105]).map_err(|source| {
            MaterializedViewStoreError::ChainEventCheckpointMalformed {
                consumer: consumer.as_str(),
                reason: source.to_string(),
            }
        })?;
    if cursor.event_sequence() != resulting_fence.chain_event_sequence() {
        return Err(
            MaterializedViewStoreError::ChainEventCheckpointFenceMismatch {
                consumer: consumer.as_str(),
                event_sequence: cursor.event_sequence(),
            },
        );
    }
    Ok(MaterializedViewChainEventCheckpoint {
        cursor,
        resulting_fence,
    })
}

fn require_matching_construction_identity(
    expected: CanonicalStoreConstructionIdentity,
    observed: CanonicalStoreConstructionIdentity,
) -> Result<(), MaterializedViewStoreError> {
    if observed.network() != expected.network() {
        return Err(
            MaterializedViewStoreError::CanonicalConstructionNetworkMismatch {
                expected: expected.network(),
                observed: observed.network(),
            },
        );
    }
    if observed.network_upgrade_activations_fingerprint()
        != expected.network_upgrade_activations_fingerprint()
    {
        return Err(
            MaterializedViewStoreError::CanonicalConstructionActivationsFingerprintMismatch {
                expected: expected.network_upgrade_activations_fingerprint(),
                observed: observed.network_upgrade_activations_fingerprint(),
            },
        );
    }
    if observed.construction_manifest_binding() != expected.construction_manifest_binding() {
        return Err(
            MaterializedViewStoreError::CanonicalConstructionManifestBindingMismatch {
                expected: expected.construction_manifest_binding(),
                observed: observed.construction_manifest_binding(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
/// Creates one structurally valid, non-admitted construction claim for isolated store tests.
///
/// Production callers must obtain the claim from an admitted canonical store;
/// this helper exists only to exercise the materialized-view persistence boundary
/// without constructing a full canonical fixture in every unit test.
pub(crate) fn test_construction_identity(
    network: Network,
) -> Result<
    CanonicalStoreConstructionIdentity,
    zinder_store::CanonicalStoreConstructionIdentityDecodeError,
> {
    let mut encoded = [0_u8; 73];
    encoded[0] = 1;
    encoded[1..5].copy_from_slice(&network.id().to_be_bytes());
    encoded[5..7].copy_from_slice(
        &zinder_core::NetworkUpgradeActivationsFingerprintVersion::CURRENT
            .value()
            .to_be_bytes(),
    );
    encoded[39..41].copy_from_slice(
        &zinder_store::CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION.to_be_bytes(),
    );
    CanonicalStoreConstructionIdentity::decode_persisted(&encoded)
}

#[cfg(test)]
/// Creates one valid checkpoint for tests that exercise only local persistence.
pub(crate) fn test_chain_event_checkpoint() -> eyre::Result<MaterializedViewChainEventCheckpoint> {
    let mut encoded = [0_u8; MATERIALIZED_VIEW_CHAIN_EVENT_CHECKPOINT_BYTES - 10];
    encoded[0] = 1;
    encoded[1..9].copy_from_slice(&1_u64.to_be_bytes());
    encoded[9..17].copy_from_slice(&1_u64.to_be_bytes());
    encoded[17..21].copy_from_slice(&1_u32.to_be_bytes());
    encoded[21..53].copy_from_slice(&[0x11; 32]);
    encoded[53..55].copy_from_slice(&1_u16.to_be_bytes());
    encoded[55..63].copy_from_slice(&1_u64.to_be_bytes());
    let fence = CanonicalEventFence::decode_persisted(&encoded)?;
    Ok(MaterializedViewChainEventCheckpoint {
        cursor: CanonicalEventCursor::at(fence.chain_event_sequence())?,
        resulting_fence: fence,
    })
}

fn decode_consumer_manifest_entries(
    iterator: impl Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rust_rocksdb::Error>>,
) -> Result<BTreeMap<String, ConsumerManifestEntry>, MaterializedViewStoreError> {
    let mut manifest = BTreeMap::new();
    for entry in iterator {
        let (key, payload) =
            entry.map_err(|source| MaterializedViewStoreError::ConsumerManifest {
                operation: "read_manifest",
                reason: source.to_string(),
            })?;
        let Some(name_bytes) = key.strip_prefix(CONSUMER_SCHEMA_KEY_PREFIX) else {
            break;
        };
        let name = String::from_utf8(name_bytes.to_vec()).map_err(|error| {
            MaterializedViewStoreError::ConsumerManifest {
                operation: "decode_manifest_name",
                reason: error.to_string(),
            }
        })?;
        let decoded = decode_manifest_entry(&payload).map_err(|reason| {
            MaterializedViewStoreError::ConsumerManifest {
                operation: "decode_manifest_entry",
                reason,
            }
        })?;
        manifest.insert(name, decoded);
    }
    Ok(manifest)
}

/// One consumer's persisted schema manifest row: its declared version and the
/// column families it owned when the row was written.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsumerManifestEntry {
    schema_version: u16,
    column_families: Vec<String>,
}

fn consumer_schema_manifest_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(CONSUMER_SCHEMA_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(CONSUMER_SCHEMA_KEY_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

fn consumer_state_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(MATERIALIZED_VIEW_STATE_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(MATERIALIZED_VIEW_STATE_KEY_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
}

fn materialized_view_coverage_bounds_valid(
    coverage: MaterializedViewCoverage,
    tip_height: BlockHeight,
) -> bool {
    let ordered = coverage.complete_from_height <= coverage.complete_through_height;
    let within_tip = coverage.complete_through_height <= tip_height;
    ordered && within_tip
}

fn validate_materialized_view_coverage_bounds(
    consumer: MaterializedViewConsumerName,
    state: &MaterializedViewState,
) -> Result<(), MaterializedViewStoreError> {
    let Some(coverage) = state.coverage else {
        return Ok(());
    };
    if materialized_view_coverage_bounds_valid(coverage, state.tip_height) {
        return Ok(());
    }
    Err(
        MaterializedViewStoreError::InvalidMaterializedViewCoverage {
            consumer: consumer.as_str(),
            complete_from_height: coverage.complete_from_height.value(),
            complete_through_height: coverage.complete_through_height.value(),
            tip_height: state.tip_height.value(),
        },
    )
}

fn encode_materialized_view_state(
    state: MaterializedViewState,
) -> [u8; MATERIALIZED_VIEW_STATE_LEN] {
    let mut payload = [0_u8; MATERIALIZED_VIEW_STATE_LEN];
    let mut offset = 0;
    payload[offset] = MATERIALIZED_VIEW_STATE_VERSION;
    offset += 1;
    payload[offset..offset + 8].copy_from_slice(&state.chain_epoch_id.value().to_be_bytes());
    offset += 8;
    payload[offset..offset + 4].copy_from_slice(&state.tip_height.value().to_be_bytes());
    offset += 4;
    payload[offset..offset + 32].copy_from_slice(&state.tip_hash.as_bytes());
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

fn decode_materialized_view_state(
    consumer: MaterializedViewConsumerName,
    payload: &[u8],
) -> Result<MaterializedViewState, MaterializedViewStoreError> {
    let bytes: [u8; MATERIALIZED_VIEW_STATE_LEN] = payload.try_into().map_err(|_| {
        materialized_view_state_decode_error("materialized-view state length is invalid")
    })?;
    if bytes[0] != MATERIALIZED_VIEW_STATE_VERSION {
        return Err(materialized_view_state_decode_error(
            "materialized-view state version is unsupported",
        ));
    }
    let chain_epoch_id =
        ChainEpochId::new(u64::from_be_bytes(bytes[1..9].try_into().map_err(
            |_| materialized_view_state_decode_error("chain epoch is malformed"),
        )?));
    let tip_height =
        BlockHeight::new(u32::from_be_bytes(bytes[9..13].try_into().map_err(
            |_| materialized_view_state_decode_error("tip height is malformed"),
        )?));
    let tip_hash = BlockHash::from_bytes(
        bytes[13..45]
            .try_into()
            .map_err(|_| materialized_view_state_decode_error("tip hash is malformed"))?,
    );
    let revision = u64::from_be_bytes(bytes[45..53].try_into().map_err(|_| {
        materialized_view_state_decode_error("materialized-view revision is malformed")
    })?);
    let coverage = match bytes[53] {
        0 => None,
        1 => Some(MaterializedViewCoverage {
            complete_from_height: BlockHeight::new(u32::from_be_bytes(
                bytes[54..58].try_into().map_err(|_| {
                    materialized_view_state_decode_error("coverage start height is malformed")
                })?,
            )),
            complete_through_height: BlockHeight::new(u32::from_be_bytes(
                bytes[58..62].try_into().map_err(|_| {
                    materialized_view_state_decode_error("coverage end height is malformed")
                })?,
            )),
            complete_through_hash: BlockHash::from_bytes(bytes[62..94].try_into().map_err(
                |_| materialized_view_state_decode_error("coverage end hash is malformed"),
            )?),
        }),
        _ => {
            return Err(materialized_view_state_decode_error(
                "materialized-view coverage presence is invalid",
            ));
        }
    };
    let coverage = match coverage {
        Some(coverage) if !materialized_view_coverage_bounds_valid(coverage, tip_height) => {
            tracing::warn!(
                consumer = consumer.as_str(),
                complete_from_height = coverage.complete_from_height.value(),
                complete_through_height = coverage.complete_through_height.value(),
                tip_height = tip_height.value(),
                "dropping materialized-view coverage with invalid bounds; the consumer re-derives its coverage"
            );
            None
        }
        other => other,
    };
    Ok(MaterializedViewState {
        chain_epoch_id,
        tip_height,
        tip_hash,
        revision,
        coverage,
    })
}

fn materialized_view_state_decode_error(reason: &'static str) -> MaterializedViewStoreError {
    MaterializedViewStoreError::Decode {
        column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
        reason: reason.to_owned(),
    }
}

fn encode_manifest_entry(schema_version: u16, column_families: &[&str]) -> Result<Vec<u8>, String> {
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
    if offset != bytes.len() {
        return Err("consumer manifest entry has trailing bytes".to_owned());
    }
    Ok(ConsumerManifestEntry {
        schema_version,
        column_families,
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

fn is_transient_secondary_missing_sst(error: &MaterializedViewStoreError) -> bool {
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
    use zinder_core::{
        ArtifactSchemaVersion, BlockHeightRange, ChainTipMetadata, UnixTimestampMillis,
    };
    use zinder_store::{ChainEpochCommitted, ChainRangeReverted};

    use super::*;

    const TEST_CONSUMER: MaterializedViewConsumerName =
        MaterializedViewConsumerName::from_static("test_consumer");
    const TEST_CONSUMER_CF: &str = "test_cf";
    const TEST_CONSUMER_SCHEMA: MaterializedViewConsumerSchema =
        MaterializedViewConsumerSchema::new(
            MaterializedViewConsumerName::from_static("test_cf_consumer"),
            1,
            &[TEST_CONSUMER_CF],
        );

    fn block_keyed_test_epoch(id: u64, tip_height: u32, tip_hash: BlockHash) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(tip_height),
            visible_tip_hash: tip_hash,
            settled_tip_height: BlockHeight::new(0),
            settled_tip_hash: Network::ZcashRegtest.genesis_hash(),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(id),
        }
    }

    fn block_keyed_test_checkpoint(
        event_sequence: u64,
        tip_height: u32,
        tip_hash: BlockHash,
    ) -> Result<MaterializedViewChainEventCheckpoint> {
        let mut encoded = [0_u8; 95];
        encoded[0] = 1;
        encoded[1..9].copy_from_slice(&event_sequence.to_be_bytes());
        encoded[9..17].copy_from_slice(&event_sequence.to_be_bytes());
        encoded[17..21].copy_from_slice(&tip_height.to_be_bytes());
        encoded[21..53].copy_from_slice(&tip_hash.as_bytes());
        encoded[53..55].copy_from_slice(&1_u16.to_be_bytes());
        encoded[55..63].copy_from_slice(&u64::from(tip_height).to_be_bytes());
        encoded[63..95].fill(u8::try_from(event_sequence).unwrap_or(u8::MAX));
        let resulting_fence = CanonicalEventFence::decode_persisted(&encoded)?;
        Ok(MaterializedViewChainEventCheckpoint {
            cursor: CanonicalEventCursor::at(event_sequence)?,
            resulting_fence,
        })
    }

    struct BlockKeyedTestState<'a> {
        chain_epoch: ChainEpoch,
        chain_event: &'a ChainEvent,
        tip_height: BlockHeight,
        tip_hash: BlockHash,
        checkpoint: Option<MaterializedViewChainEventCheckpoint>,
    }

    fn stage_block_keyed_test_state(
        store: &MaterializedViewStore,
        state: &BlockKeyedTestState<'_>,
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        crate::consumer::stage_block_keyed_consumer_state(
            BLOCK_SUMMARY_CONSUMER_NAME,
            MaterializedViewBlockProjection {
                chain_epoch: state.chain_epoch,
                chain_event: state.chain_event,
                tip_height: Some(state.tip_height),
                tip_hash: Some(state.tip_hash),
            },
            &mut ctx,
        )
        .map_err(|error| eyre::eyre!(error.to_string()))?;
        if let Some(checkpoint) = state.checkpoint {
            store.stage_chain_event_checkpoint(
                ctx.batch,
                BLOCK_SUMMARY_CONSUMER_NAME,
                checkpoint,
            )?;
        }
        store.write_consumer_batch(BLOCK_SUMMARY_CONSUMER_NAME, &batch)?;
        Ok(())
    }

    fn assert_incomplete_block_keyed_replay_page(
        store: &MaterializedViewStore,
    ) -> Result<ChainEpoch> {
        let first_hash = BlockHash::from_bytes([0x10; 32]);
        let first_epoch = block_keyed_test_epoch(1, 11, BlockHash::from_bytes([0x11; 32]));
        let first_page = ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch: first_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(10),
                    BlockHeight::new(10),
                ),
            },
        };
        stage_block_keyed_test_state(
            store,
            &BlockKeyedTestState {
                chain_epoch: first_epoch,
                chain_event: &first_page,
                tip_height: BlockHeight::new(10),
                tip_hash: first_hash,
                checkpoint: None,
            },
        )?;
        let intermediate = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("first replay page must persist state"))?;
        assert_eq!(intermediate.revision, 1);
        assert_eq!(intermediate.tip_height, BlockHeight::new(10));
        assert_eq!(
            intermediate.coverage,
            Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(10),
                complete_through_height: BlockHeight::new(10),
                complete_through_hash: first_hash,
            })
        );
        assert!(
            store
                .chain_event_checkpoint(BLOCK_SUMMARY_CONSUMER_NAME)?
                .is_none(),
            "an intermediate replay page must not advance admission"
        );
        Ok(first_epoch)
    }

    fn assert_converged_block_keyed_replay(
        store: &MaterializedViewStore,
        first_epoch: ChainEpoch,
    ) -> Result<()> {
        let final_hash = BlockHash::from_bytes([0x11; 32]);
        let final_page = ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch: first_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(11),
                    BlockHeight::new(11),
                ),
            },
        };
        let first_checkpoint = block_keyed_test_checkpoint(1, 11, final_hash)?;
        stage_block_keyed_test_state(
            store,
            &BlockKeyedTestState {
                chain_epoch: first_epoch,
                chain_event: &final_page,
                tip_height: BlockHeight::new(11),
                tip_hash: final_hash,
                checkpoint: Some(first_checkpoint),
            },
        )?;
        let converged = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("final replay page must persist state"))?;
        assert_eq!(converged.revision, 2);
        assert_eq!(converged.tip_height, BlockHeight::new(11));
        assert_eq!(
            converged.coverage,
            Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(10),
                complete_through_height: BlockHeight::new(11),
                complete_through_hash: final_hash,
            })
        );
        assert_eq!(
            store.chain_event_checkpoint(BLOCK_SUMMARY_CONSUMER_NAME)?,
            Some(first_checkpoint)
        );
        Ok(())
    }

    fn assert_block_keyed_append_and_reorg(store: &MaterializedViewStore) -> Result<()> {
        let committed_hash = BlockHash::from_bytes([0x12; 32]);
        let committed_epoch = block_keyed_test_epoch(2, 12, committed_hash);
        let committed_event = ChainEvent::ChainCommitted {
            committed: ChainEpochCommitted {
                chain_epoch: committed_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(12),
                    BlockHeight::new(12),
                ),
            },
        };
        stage_block_keyed_test_state(
            store,
            &BlockKeyedTestState {
                chain_epoch: committed_epoch,
                chain_event: &committed_event,
                tip_height: BlockHeight::new(12),
                tip_hash: committed_hash,
                checkpoint: Some(block_keyed_test_checkpoint(2, 12, committed_hash)?),
            },
        )?;
        let committed = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("contiguous commit must persist state"))?;
        assert_eq!(committed.revision, 3);

        let replacement_hash = BlockHash::from_bytes([0xf2; 32]);
        let replacement_epoch = block_keyed_test_epoch(3, 12, replacement_hash);
        let reorg_event = ChainEvent::ChainReorged {
            reverted: ChainRangeReverted {
                chain_epoch: committed_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(12),
                    BlockHeight::new(12),
                ),
            },
            committed: ChainEpochCommitted {
                chain_epoch: replacement_epoch,
                block_range: BlockHeightRange::inclusive(
                    BlockHeight::new(12),
                    BlockHeight::new(12),
                ),
            },
        };
        stage_block_keyed_test_state(
            store,
            &BlockKeyedTestState {
                chain_epoch: replacement_epoch,
                chain_event: &reorg_event,
                tip_height: BlockHeight::new(12),
                tip_hash: replacement_hash,
                checkpoint: Some(block_keyed_test_checkpoint(3, 12, replacement_hash)?),
            },
        )?;
        let replaced = store
            .consumer_state(BLOCK_SUMMARY_CONSUMER_NAME)?
            .ok_or_else(|| eyre::eyre!("reorg replacement must persist state"))?;
        assert_eq!(replaced.revision, 4);
        assert_eq!(replaced.chain_epoch_id, ChainEpochId::new(3));
        assert_eq!(replaced.tip_hash, replacement_hash);
        assert_eq!(
            replaced.coverage,
            Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(10),
                complete_through_height: BlockHeight::new(12),
                complete_through_hash: replacement_hash,
            })
        );
        Ok(())
    }

    #[test]
    fn secondary_catchup_retries_only_missing_sst_file_races() {
        assert!(is_missing_sst_error(
            &rust_rocksdb::ErrorKind::IOError,
            "IO error: No such file or directory: /materialized-views/199308.sst"
        ));
        assert!(!is_missing_sst_error(
            &rust_rocksdb::ErrorKind::IOError,
            "IO error: No such file or directory: /materialized-views/MANIFEST-000123"
        ));
        assert!(!is_missing_sst_error(
            &rust_rocksdb::ErrorKind::Corruption,
            "Corruption: No such file or directory: /materialized-views/199308.sst"
        ));
    }

    #[test]
    fn reorg_incidents_cursor_is_event_only() {
        assert!(
            !MaterializedViewStore::bundled_chain_event_consumer_names()
                .contains(&REORG_INCIDENTS_CONSUMER_NAME)
        );
        assert!(
            MaterializedViewStore::bundled_event_only_chain_event_consumer_names()
                .contains(&REORG_INCIDENTS_CONSUMER_NAME)
        );
    }

    #[test]
    fn paid_fee_schema_and_cursor_participate_in_bundled_dispatch() {
        assert!(MaterializedViewStore::bundled_consumers().contains(&PAID_FEE_DISTRIBUTION_SCHEMA));
        assert!(
            MaterializedViewStore::bundled_chain_event_consumer_names()
                .contains(&PAID_FEE_DISTRIBUTION_SCHEMA.name)
        );
    }

    #[test]
    fn opening_a_fresh_store_atomically_writes_its_format_and_construction_identity() -> Result<()>
    {
        let tempdir = tempdir()?;
        let construction_identity = test_construction_identity(Network::ZcashRegtest)?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            construction_identity,
            MaterializedViewStoreOptions::default(),
        )?;
        assert_eq!(
            store.store_format_version()?,
            MATERIALIZED_VIEW_STORE_FORMAT_VERSION
        );
        assert_eq!(store.construction_identity(), construction_identity);
        assert_eq!(
            store.get(
                MaterializedViewStoreTable::ConsumerMetadata,
                STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY,
            )?,
            Some(construction_identity.encode_persisted().to_vec())
        );
        Ok(())
    }

    #[test]
    fn primary_rejects_a_different_canonical_construction_identity_before_opening() -> Result<()> {
        let tempdir = tempdir()?;
        MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions::default(),
        )?;

        let outcome = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashTestnet)?,
            MaterializedViewStoreOptions::default(),
        );

        assert!(matches!(
            outcome,
            Err(
                MaterializedViewStoreError::CanonicalConstructionNetworkMismatch {
                    expected: Network::ZcashTestnet,
                    observed: Network::ZcashRegtest,
                }
            )
        ));
        Ok(())
    }

    #[test]
    fn chain_event_checkpoint_round_trip_persists_and_retrieves_identity() -> Result<()> {
        let tempdir = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        assert!(
            store
                .chain_event_checkpoint(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?
                .is_none()
        );
        let checkpoint = test_chain_event_checkpoint()?;
        store.put_chain_event_checkpoint(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, checkpoint)?;
        assert_eq!(
            store.chain_event_checkpoint(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?,
            Some(checkpoint)
        );
        Ok(())
    }

    #[test]
    fn block_keyed_state_stays_unadmitted_between_pages_then_converges_and_tracks_reorgs()
    -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions {
                consumers: &[BLOCK_SUMMARY_SCHEMA],
                ..MaterializedViewStoreOptions::default()
            },
        )?;

        // A newly added consumer starts at its first authenticated replay
        // page, converges atomically, then advances through append and reorg.
        let first_epoch = assert_incomplete_block_keyed_replay_page(&store)?;
        assert_converged_block_keyed_replay(&store, first_epoch)?;
        assert_block_keyed_append_and_reorg(&store)?;
        Ok(())
    }

    #[test]
    fn chain_event_checkpoint_rejects_an_unselected_consumer_before_write() -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions::default(),
        )?;

        let outcome =
            store.put_chain_event_checkpoint(TEST_CONSUMER, test_chain_event_checkpoint()?);

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::ConsumerNotSelected { consumer })
                if consumer == TEST_CONSUMER.as_str()
        ));
        assert!(store.chain_event_checkpoint(TEST_CONSUMER)?.is_none());
        Ok(())
    }

    #[test]
    fn chain_event_checkpoint_rejects_a_cursor_sequence_different_from_its_resulting_fence()
    -> Result<()> {
        let mut encoded = encode_chain_event_checkpoint(test_chain_event_checkpoint()?);
        encoded[2..10].copy_from_slice(&2_u64.to_be_bytes());

        let outcome = decode_chain_event_checkpoint(TEST_CONSUMER, &encoded);

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::ChainEventCheckpointFenceMismatch {
                consumer,
                event_sequence: 2,
            }) if consumer == TEST_CONSUMER.as_str()
        ));
        Ok(())
    }

    #[test]
    fn checkpoint_reuse_rejects_a_same_sequence_different_retained_fence() -> Result<()> {
        let checkpoint = test_chain_event_checkpoint()?;
        let mut other_fence_record = checkpoint.resulting_fence().encode_persisted();
        other_fence_record[21] ^= 0xff;
        let other_fence = CanonicalEventFence::decode_persisted(&other_fence_record)?;

        assert_eq!(
            checkpoint.cursor().event_sequence(),
            other_fence.chain_event_sequence()
        );
        assert!(matches!(
            checkpoint.require_matching_retained_event_boundary(checkpoint.cursor(), other_fence),
            Err(
                MaterializedViewStoreError::ChainEventCheckpointRetainedEventMismatch {
                    event_sequence: 1,
                }
            )
        ));
        Ok(())
    }

    #[test]
    fn secondary_open_rejects_a_malformed_persisted_chain_event_checkpoint() -> Result<()> {
        let primary_directory = tempdir()?;
        let secondary_directory = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let primary = MaterializedViewStore::open(
            primary_directory.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        primary.put(
            MaterializedViewStoreTable::ChainEventCursor,
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str().as_bytes(),
            &[0],
        )?;
        drop(primary);

        let outcome = MaterializedViewStore::open_secondary(
            primary_directory.path(),
            secondary_directory.path(),
            options,
        );

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::ChainEventCheckpointMalformed { consumer, .. })
                if consumer == TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str()
        ));
        Ok(())
    }

    #[test]
    fn materialized_view_write_measurement_attributes_rows_and_cursor_to_stable_identity()
    -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions {
                consumers: &[TEST_CONSUMER_SCHEMA],
                ..MaterializedViewStoreOptions::default()
            },
        )?;
        let consumer_name = TEST_CONSUMER_SCHEMA.name;
        let consumer_column_family = store.consumer_column_family(TEST_CONSUMER_CF)?;
        let mut batch = WriteBatch::default();
        let before = WriteBatchSize::capture(&batch);
        batch.put_cf(&consumer_column_family, b"row-key", b"row-payload");
        let mut measurements = vec![MaterializedViewWriteMeasurement::from_batch_delta(
            consumer_name,
            before,
            &batch,
            Duration::ZERO,
        )];
        let cursor_column_family =
            store.column_family(MaterializedViewStoreTable::ChainEventCursor)?;
        stage_consumer_checkpoint_and_measure(
            &mut batch,
            &cursor_column_family,
            consumer_name,
            test_chain_event_checkpoint()?,
            &mut measurements,
        );

        assert_eq!(measurements[0].consumer, consumer_name);
        assert_eq!(measurements[0].operations, 2);
        assert!(measurements[0].logical_bytes > 0);
        Ok(())
    }

    #[test]
    fn materialized_view_owned_batch_rejects_an_unselected_identity_before_write() -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions::default(),
        )?;
        let metadata = store.column_family(MaterializedViewStoreTable::ConsumerMetadata)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&metadata, b"must-not-commit", b"row");

        let outcome = store.write_consumer_batch(TEST_CONSUMER, &batch);

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::ConsumerNotSelected { consumer: consumer_name })
                if consumer_name == TEST_CONSUMER.as_str()
        ));
        assert_eq!(
            store.get(
                MaterializedViewStoreTable::ConsumerMetadata,
                b"must-not-commit"
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn checkpoint_preserves_cursor_rows() -> Result<()> {
        let tempdir = tempdir()?;
        let source_path = tempdir.path().join("materialized-view-source");
        let checkpoint_path = tempdir.path().join("materialized-view-checkpoint");
        let options = MaterializedViewStoreOptions {
            consumers: &[TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        {
            let store = MaterializedViewStore::open(
                &source_path,
                test_construction_identity(Network::ZcashRegtest)?,
                options,
            )?;
            let checkpoint = test_chain_event_checkpoint()?;
            store
                .put_chain_event_checkpoint(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, checkpoint)?;
            store.create_checkpoint(&checkpoint_path)?;
        }

        let checkpoint = MaterializedViewStore::open(
            &checkpoint_path,
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        assert_eq!(
            checkpoint.chain_event_checkpoint(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?,
            Some(test_chain_event_checkpoint()?)
        );
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_none_for_empty_column_family() -> Result<()> {
        let tempdir = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        assert_eq!(store.last_consumer_key("test_cf")?, None);
        Ok(())
    }

    #[test]
    fn last_consumer_key_returns_lexicographically_last_key() -> Result<()> {
        let tempdir = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
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
        snapshot: &MaterializedViewStoreReadSnapshot<'_>,
        initial_state: MaterializedViewState,
        height_10_key: [u8; 4],
        height_20_key: [u8; 4],
        height_30_key: [u8; 4],
    ) -> Result<()> {
        assert_eq!(snapshot.consumer_state(TEST_CONSUMER)?, Some(initial_state));
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
        snapshot: &MaterializedViewStoreReadSnapshot<'_>,
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
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions {
                consumers: &[TEST_CONSUMER_SCHEMA],
                ..MaterializedViewStoreOptions::default()
            },
        )?;
        let height_5_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(5));
        let height_10_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(10));
        let height_20_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(20));
        let height_30_key = zinder_core::wire::encode_height_key_descending(BlockHeight::new(30));
        store.put_consumer(TEST_CONSUMER_CF, &height_10_key, b"skip")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_20_key, b"match-before")?;
        store.put_materialized_view_status(b"status-before")?;
        let initial_state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(1),
            tip_height: BlockHeight::new(20),
            tip_hash: BlockHash::from_bytes([0x20; 32]),
            revision: 1,
            coverage: None,
        };
        store.put_consumer_state(TEST_CONSUMER, initial_state)?;

        let snapshot = store.read_snapshot()?;

        let advanced_state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(1),
            tip_height: BlockHeight::new(30),
            tip_hash: BlockHash::from_bytes([0x30; 32]),
            revision: 2,
            coverage: None,
        };
        store.put_consumer(TEST_CONSUMER_CF, &height_5_key, b"match-after")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_20_key, b"match-after")?;
        store.put_consumer(TEST_CONSUMER_CF, &height_30_key, b"match-after")?;
        store.put_consumer_state(TEST_CONSUMER, advanced_state)?;
        store.put_materialized_view_status(b"status-after")?;

        assert_eq!(
            store.get_consumer(TEST_CONSUMER_CF, &height_20_key)?,
            Some(b"match-after".to_vec())
        );
        assert_eq!(store.consumer_row_count(TEST_CONSUMER_CF)?, 4);
        assert_eq!(store.consumer_state(TEST_CONSUMER)?, Some(advanced_state));
        assert_eq!(
            store.get_materialized_view_status()?,
            Some(b"status-after".to_vec())
        );
        assert_eq!(
            snapshot.get_materialized_view_status()?,
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
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let primary = MaterializedViewStore::open(
            primary_directory.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        primary.put_consumer(TEST_CONSUMER_CF, b"before", b"visible")?;
        let secondary = MaterializedViewStore::open_secondary(
            primary_directory.path(),
            secondary_directory.path(),
            options,
        )?;
        let snapshot = secondary.read_snapshot()?;
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
    fn terminal_secondary_catchup_failure_fences_a_concurrent_read_before_rows_serve() -> Result<()>
    {
        let primary_directory = tempdir()?;
        let secondary_directory = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let primary = MaterializedViewStore::open(
            primary_directory.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
        let secondary = MaterializedViewStore::open_secondary(
            primary_directory.path(),
            secondary_directory.path(),
            options,
        )?;
        primary.put_consumer(TEST_CONSUMER_CF, b"advanced", b"must-not-serve")?;
        primary.put(
            MaterializedViewStoreTable::ConsumerMetadata,
            STORE_CANONICAL_CONSTRUCTION_IDENTITY_KEY,
            &test_construction_identity(Network::ZcashTestnet)?.encode_persisted(),
        )?;

        let (catch_up_locked_sender, catch_up_locked_receiver) = mpsc::channel();
        let (continue_catch_up_sender, continue_catch_up_receiver) = mpsc::channel();
        let catch_up_store = secondary.clone();
        let catch_up = thread::spawn(move || {
            let _catch_up_guard = catch_up_store.catch_up_barrier.write();
            let _ = catch_up_locked_sender.send(());
            let _ = continue_catch_up_receiver.recv();
            catch_up_store.try_catch_up_while_reads_blocked()
        });
        catch_up_locked_receiver.recv_timeout(Duration::from_secs(1))?;

        let (read_started_sender, read_started_receiver) = mpsc::channel();
        let concurrent_store = secondary.clone();
        let concurrent_read = thread::spawn(move || {
            let _ = read_started_sender.send(());
            concurrent_store
                .read_snapshot()
                .and_then(|snapshot| snapshot.get_consumer(TEST_CONSUMER_CF, b"advanced"))
        });
        read_started_receiver.recv_timeout(Duration::from_secs(1))?;
        continue_catch_up_sender.send(())?;

        assert!(matches!(
            catch_up.join(),
            Ok(Err(
                MaterializedViewStoreError::CanonicalConstructionNetworkMismatch { .. }
            ))
        ));
        assert!(matches!(
            concurrent_read.join(),
            Ok(Err(MaterializedViewStoreError::SecondaryReadsFenced))
        ));
        assert!(matches!(
            secondary.get_consumer(TEST_CONSUMER_CF, b"advanced"),
            Err(MaterializedViewStoreError::SecondaryReadsFenced)
        ));
        Ok(())
    }

    #[test]
    fn visit_consumer_rows_streams_rows_and_fails_closed() -> Result<()> {
        let tempdir = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
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
            Err(MaterializedViewStoreError::ConsumerPayloadDecode { name, reason })
                if name == TEST_CONSUMER_CF && reason == "invalid fixture row"
        ));
        Ok(())
    }

    #[test]
    fn visit_consumer_range_streams_only_inclusive_bounds_and_fails_closed() -> Result<()> {
        let tempdir = tempdir()?;
        let options = MaterializedViewStoreOptions {
            consumers: &[TEST_CONSUMER_SCHEMA],
            ..MaterializedViewStoreOptions::default()
        };
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        )?;
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
            Err(MaterializedViewStoreError::ConsumerPayloadDecode { name, reason })
                if name == TEST_CONSUMER_CF && reason == "invalid bounded fixture row"
        ));
        Ok(())
    }

    fn incompatible_store_format_rejection_preserves_rows(persisted_version: u16) -> Result<()> {
        let tempdir = tempdir()?;
        {
            let store = MaterializedViewStore::open(
                tempdir.path(),
                test_construction_identity(Network::ZcashRegtest)?,
                MaterializedViewStoreOptions {
                    consumers: &[TEST_CONSUMER_SCHEMA, TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
                    ..MaterializedViewStoreOptions::default()
                },
            )?;
            store.put_consumer(TEST_CONSUMER_CF, b"row", b"value")?;
            store.put_chain_event_checkpoint(
                TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
                test_chain_event_checkpoint()?,
            )?;
            store.put(
                MaterializedViewStoreTable::ConsumerMetadata,
                STORE_FORMAT_VERSION_KEY,
                &persisted_version.to_be_bytes(),
            )?;
        }
        let column_families_before = existing_column_family_names(tempdir.path());
        let outcome = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions {
                consumers: &[TEST_CONSUMER_SCHEMA, TRANSPARENT_OUTPOINT_SPEND_SCHEMA],
                ..MaterializedViewStoreOptions::default()
            },
        );
        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::SchemaMismatch {
                persisted,
                running,
            }) if persisted == persisted_version && running == MATERIALIZED_VIEW_STORE_FORMAT_VERSION
        ));
        assert_eq!(
            existing_column_family_names(tempdir.path()),
            column_families_before
        );

        let db = DB::open_cf_for_read_only(
            &Options::default(),
            tempdir.path(),
            existing_column_family_names(tempdir.path()),
            false,
        )?;
        let consumer = db
            .cf_handle(TEST_CONSUMER_CF)
            .ok_or_else(|| eyre::eyre!("test consumer column family is missing"))?;
        assert_eq!(
            db.get_cf(&consumer, b"row")?.as_deref(),
            Some(b"value".as_slice())
        );
        let cursor = db
            .cf_handle(MaterializedViewStoreTable::ChainEventCursor.column_family_name())
            .ok_or_else(|| eyre::eyre!("chain-event cursor column family is missing"))?;
        assert_eq!(
            db.get_cf(
                &cursor,
                TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str().as_bytes()
            )?
            .as_deref(),
            Some(encode_chain_event_checkpoint(test_chain_event_checkpoint()?).as_slice())
        );
        let metadata = db
            .cf_handle(MaterializedViewStoreTable::ConsumerMetadata.column_family_name())
            .ok_or_else(|| eyre::eyre!("consumer metadata column family is missing"))?;
        assert_eq!(
            db.get_cf(&metadata, STORE_FORMAT_VERSION_KEY)?.as_deref(),
            Some(persisted_version.to_be_bytes().as_slice())
        );
        Ok(())
    }

    #[test]
    fn lower_store_format_version_returns_mismatch_without_mutation() -> Result<()> {
        incompatible_store_format_rejection_preserves_rows(
            MATERIALIZED_VIEW_STORE_FORMAT_VERSION - 1,
        )
    }

    #[test]
    fn reopening_a_store_with_an_advanced_store_format_version_returns_mismatch_without_mutation()
    -> Result<()> {
        incompatible_store_format_rejection_preserves_rows(
            MATERIALIZED_VIEW_STORE_FORMAT_VERSION + 1,
        )
    }

    #[test]
    fn incompatible_store_format_is_rejected_before_manifest_decoding() -> Result<()> {
        let tempdir = tempdir()?;
        {
            let store = MaterializedViewStore::open(
                tempdir.path(),
                test_construction_identity(Network::ZcashRegtest)?,
                MaterializedViewStoreOptions {
                    consumers: &[TEST_CONSUMER_SCHEMA],
                    ..MaterializedViewStoreOptions::default()
                },
            )?;
            store.put(
                MaterializedViewStoreTable::ConsumerMetadata,
                &consumer_schema_manifest_key(TEST_CONSUMER.as_str()),
                &[0, 1, 0, 0, 0, 1],
            )?;
            store.put(
                MaterializedViewStoreTable::ConsumerMetadata,
                STORE_FORMAT_VERSION_KEY,
                &(MATERIALIZED_VIEW_STORE_FORMAT_VERSION - 1).to_be_bytes(),
            )?;
        }

        for outcome in [
            MaterializedViewStore::detect_materialized_view_preset_at_path(tempdir.path())
                .map(|_| ()),
            MaterializedViewStore::open_with_materialized_view_preset(
                tempdir.path(),
                test_construction_identity(Network::ZcashRegtest)?,
                MaterializedViewPreset::Explorer,
                MaterializedViewStoreOptions::default(),
            )
            .map(|_| ()),
        ] {
            assert!(matches!(
                outcome,
                Err(MaterializedViewStoreError::SchemaMismatch {
                    persisted,
                    running,
                }) if persisted == MATERIALIZED_VIEW_STORE_FORMAT_VERSION - 1 && running == MATERIALIZED_VIEW_STORE_FORMAT_VERSION
            ));
        }
        Ok(())
    }

    #[test]
    fn manifest_entry_round_trips_schema_version_and_column_families() -> Result<()> {
        let encoded = encode_manifest_entry(3, &["alpha", "beta_index"])
            .map_err(|reason| eyre::eyre!(reason))?;
        let decoded = decode_manifest_entry(&encoded).map_err(|reason| eyre::eyre!(reason))?;
        assert_eq!(decoded.schema_version, 3);
        assert_eq!(
            decoded.column_families,
            vec!["alpha".to_owned(), "beta_index".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn consumer_state_round_trips_verified_coverage() -> Result<()> {
        let state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(42),
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([0xA1; 32]),
            revision: 7,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(90),
                complete_through_hash: BlockHash::from_bytes([0xB2; 32]),
            }),
        };

        let decoded =
            decode_materialized_view_state(TEST_CONSUMER, &encode_materialized_view_state(state))?;

        assert_eq!(decoded, state);
        Ok(())
    }

    #[test]
    fn decoding_coverage_past_materialized_view_tip_drops_coverage() -> Result<()> {
        let state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(42),
            tip_height: BlockHeight::new(10),
            tip_hash: BlockHash::from_bytes([0xA1; 32]),
            revision: 7,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(11),
                complete_through_hash: BlockHash::from_bytes([0xB2; 32]),
            }),
        };

        let decoded =
            decode_materialized_view_state(TEST_CONSUMER, &encode_materialized_view_state(state))?;

        assert_eq!(decoded.coverage, None);
        assert_eq!(decoded.tip_height, BlockHeight::new(10));
        assert_eq!(decoded.revision, 7);
        Ok(())
    }

    #[test]
    fn encoding_a_manifest_entry_rejects_more_column_families_than_the_count_field_holds() {
        let column_families = vec!["x"; usize::from(u16::MAX) + 1];
        let outcome = encode_manifest_entry(1, &column_families);
        assert!(matches!(outcome, Err(reason) if reason.contains("column families")));
    }

    #[test]
    fn encoding_a_manifest_entry_rejects_a_column_family_name_longer_than_the_length_field_holds() {
        let overlong = "a".repeat(usize::from(u16::MAX) + 1);
        let outcome = encode_manifest_entry(1, &[overlong.as_str()]);
        assert!(matches!(outcome, Err(reason) if reason.contains("column family name")));
    }

    #[test]
    fn decoding_a_manifest_entry_rejects_trailing_bytes() -> Result<()> {
        let mut encoded =
            encode_manifest_entry(2, &["alpha"]).map_err(|reason| eyre::eyre!(reason))?;
        encoded.extend_from_slice(&[0, 1]);

        let outcome = decode_manifest_entry(&encoded);

        assert!(matches!(outcome, Err(reason) if reason.contains("trailing bytes")));
        Ok(())
    }

    #[test]
    fn opening_store_rejects_zero_wal_budget() -> Result<()> {
        let tempdir = tempdir()?;
        let mut options = MaterializedViewStoreOptions::default();
        options.rocksdb_resource_budget.max_wal_bytes = 0;

        let outcome = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        );

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::InvalidOptions { reason })
                if reason.contains("max_wal_bytes")
        ));
        Ok(())
    }

    #[test]
    fn opening_store_rejects_negative_open_file_budget() -> Result<()> {
        let tempdir = tempdir()?;
        let mut options = MaterializedViewStoreOptions::default();
        options.rocksdb_resource_budget.max_open_files = -1;

        let outcome = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            options,
        );

        assert!(matches!(
            outcome,
            Err(MaterializedViewStoreError::InvalidOptions { reason })
                if reason.contains("max_open_files")
        ));
        Ok(())
    }

    fn materialized_view_state_with_coverage(
        tip_height: u32,
        complete_from_height: u32,
        complete_through_height: u32,
    ) -> MaterializedViewState {
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(1),
            tip_height: BlockHeight::new(tip_height),
            tip_hash: BlockHash::from_bytes([0x11; 32]),
            revision: 7,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(complete_from_height),
                complete_through_height: BlockHeight::new(complete_through_height),
                complete_through_hash: BlockHash::from_bytes([0x22; 32]),
            }),
        }
    }

    #[test]
    fn staging_materialized_view_coverage_with_inverted_bounds_is_rejected() -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions::default(),
        )?;

        let inverted = materialized_view_state_with_coverage(200, 150, 100);
        match store.put_consumer_state(TEST_CONSUMER, inverted) {
            Err(MaterializedViewStoreError::InvalidMaterializedViewCoverage {
                consumer,
                complete_from_height,
                complete_through_height,
                tip_height,
            }) => {
                assert_eq!(consumer, TEST_CONSUMER.as_str());
                assert_eq!(complete_from_height, 150);
                assert_eq!(complete_through_height, 100);
                assert_eq!(tip_height, 200);
            }
            other => {
                return Err(eyre::eyre!(
                    "expected InvalidMaterializedViewCoverage, got {other:?}"
                ));
            }
        }

        assert!(store.consumer_state(TEST_CONSUMER)?.is_none());
        Ok(())
    }

    #[test]
    fn staging_materialized_view_coverage_beyond_tip_is_rejected() -> Result<()> {
        let tempdir = tempdir()?;
        let store = MaterializedViewStore::open(
            tempdir.path(),
            test_construction_identity(Network::ZcashRegtest)?,
            MaterializedViewStoreOptions::default(),
        )?;

        let beyond_tip = materialized_view_state_with_coverage(180_256, 1, 180_512);
        match store.put_consumer_state(TEST_CONSUMER, beyond_tip) {
            Err(MaterializedViewStoreError::InvalidMaterializedViewCoverage {
                complete_through_height,
                tip_height,
                ..
            }) => {
                assert_eq!(complete_through_height, 180_512);
                assert_eq!(tip_height, 180_256);
            }
            other => {
                return Err(eyre::eyre!(
                    "expected InvalidMaterializedViewCoverage, got {other:?}"
                ));
            }
        }

        assert!(store.consumer_state(TEST_CONSUMER)?.is_none());
        Ok(())
    }
}
