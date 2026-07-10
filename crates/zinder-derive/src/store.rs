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

use rust_rocksdb::{
    Cache, ColumnFamilyDescriptor, DB, IteratorMode, Options, WriteBatch, WriteOptions,
    checkpoint::Checkpoint,
};
use zinder_core::{BlockHeight, ChainEpoch};
use zinder_store::{
    ChainEvent, RocksDbIoMode, RocksDbOpenRole, RocksDbResourceBudget,
    build_block_based_table_factory, open_bounded_rocksdb,
};

use crate::{
    consumer::block_summary::{BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA},
    consumer::ironwood_migration::{IRONWOOD_MIGRATION_CONSUMER_NAME, IRONWOOD_MIGRATION_SCHEMA},
    consumer::mempool_event_counts::MEMPOOL_EVENT_COUNTS_SCHEMA,
    consumer::recent_transactions::{
        RECENT_TRANSACTIONS_CONSUMER_NAME, RECENT_TRANSACTIONS_SCHEMA,
    },
    consumer::reorg_incidents::{REORG_INCIDENTS_CONSUMER_NAME, REORG_INCIDENTS_SCHEMA},
    consumer::transaction_fees::{TRANSACTION_FEES_CONSUMER_NAME, TRANSACTION_FEES_SCHEMA},
    consumer::transparent_address_activity::{
        TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
    },
    consumer::transparent_address_deltas::{
        TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME, TRANSPARENT_ADDRESS_DELTAS_SCHEMA,
    },
    consumer::transparent_address_transaction_history::{
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    },
    consumer::transparent_outpoint_spend::{
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
    },
    consumer::{
        BlockCommitContext, BlockKeyedConsumer, ChainCommittedEvent, ChainReorgedEvent,
        CommittedRange, DeriveConsumer, DeriveConsumerCtx, DeriveConsumerName,
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
/// layout bumps its own version and only its own data rebuilds. This
/// constant bumps only when the shared container changes, which forces a
/// whole-store wipe because no consumer's data survives a container change.
/// The version is persisted in the `consumer_metadata` column family on
/// first open and validated on subsequent opens.
pub const DERIVE_STORE_FORMAT_VERSION: u16 = 7;

const STORE_FORMAT_VERSION_KEY: &[u8] = b"\x00\x01schema_version";
const DERIVE_STATUS_KEY: &[u8] = b"\x00\x02derive_status";
const CONSUMER_SCHEMA_KEY_PREFIX: &[u8] = b"\x00\x03consumer_schema:";
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
    BLOCK_SUMMARY_SCHEMA,
    IRONWOOD_MIGRATION_SCHEMA,
    MEMPOOL_EVENT_COUNTS_SCHEMA,
    RECENT_TRANSACTIONS_SCHEMA,
    REORG_INCIDENTS_SCHEMA,
    TRANSACTION_FEES_SCHEMA,
    TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
    TRANSPARENT_ADDRESS_DELTAS_SCHEMA,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
];
const BUNDLED_CHAIN_EVENT_CONSUMER_NAMES: &[DeriveConsumerName] = &[
    BLOCK_SUMMARY_CONSUMER_NAME,
    IRONWOOD_MIGRATION_CONSUMER_NAME,
    TRANSACTION_FEES_CONSUMER_NAME,
    RECENT_TRANSACTIONS_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_DELTAS_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
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
/// column family already on disk that neither list covers. Opening those
/// on-disk orphans is what lets reconciliation drop a deleted consumer's
/// column families, since `RocksDB` refuses to open a store while leaving an
/// existing column family unlisted.
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
    block_cache: Cache,
    write_buffer_manager: rust_rocksdb::WriteBufferManager,
    io_mode: RocksDbIoMode,
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
            .finish()
    }
}

impl DeriveStore {
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
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            io_mode: bounded_open.io_mode,
        };
        store.validate_or_initialize_store_format_version()?;
        store.reconcile_consumer_schemas()?;
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
            block_cache: bounded_open.block_cache,
            write_buffer_manager: bounded_open.write_buffer_manager,
            io_mode: bounded_open.io_mode,
        };
        store.require_matching_store_format_version()?;
        store.validate_secondary_consumer_schemas()?;
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
        self.db
            .try_catch_up_with_primary()
            .map_err(|source| DeriveStoreError::Operation {
                operation: "try_catch_up_with_primary",
                column_family: DeriveStoreColumnFamily::ConsumerMetadata,
                source,
            })
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

        for consumer in block_consumers.iter_mut() {
            dispatch_chain_event_to_block_consumer(&mut **consumer, inputs, &mut ctx, blocks)?;
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
        let column_family = self.column_family(DeriveStoreTable::ChainEventCursor)?;
        batch.put_cf(&column_family, consumer.as_str().as_bytes(), cursor_bytes);
        self.write(&batch)
            .map_err(|source| DeriveStoreError::Operation {
                operation: "put_chain_event_cursor",
                column_family: DeriveStoreColumnFamily::ChainEventCursor,
                source,
            })
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

    /// Rejects a secondary open whose declared consumer versions disagree with
    /// the persisted manifest.
    ///
    /// A secondary reader cannot rebuild a consumer's column families, so it
    /// refuses to read rows written under a different consumer layout and lets
    /// the primary reconcile first. Callers retry the open once the primary
    /// rewrites the manifest.
    fn validate_secondary_consumer_schemas(&self) -> Result<(), DeriveStoreError> {
        let recorded = self.read_consumer_manifest()?;
        for consumer in self.consumers {
            let persisted = recorded
                .get(consumer.name.as_str())
                .map(|entry| entry.schema_version);
            if persisted != Some(consumer.schema_version) {
                return Err(DeriveStoreError::ConsumerSchemaMismatch {
                    consumer: consumer.name.as_str(),
                    persisted,
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
    /// A consumer whose declared version matches keeps its column families
    /// and cursor. A consumer whose version moved has every row in its column
    /// families cleared and its cursor reset. A consumer recorded in the
    /// manifest but no longer declared has its rows cleared and its manifest
    /// entry removed. A newly declared consumer has its column families cleared
    /// and is then recorded at its declared version, so a family that
    /// previously belonged to another consumer starts empty rather than serving
    /// the prior owner's rows behind a fresh cursor. Reconciliation never drops
    /// a column family in place: a
    /// range-tombstone clear replays safely on an attached secondary, while a
    /// `drop_cf`/`create_cf` edit crashes a secondary mid-catchup. An emptied
    /// orphan family is reclaimed physically only when a container-format
    /// change wipes the whole derive directory.
    fn reconcile_consumer_schemas(&self) -> Result<(), DeriveStoreError> {
        let recorded = self.read_consumer_manifest()?;
        self.drop_unregistered_consumers(&recorded)?;
        for consumer in self.consumers {
            match recorded.get(consumer.name.as_str()) {
                Some(entry) if entry.schema_version == consumer.schema_version => {}
                Some(entry) => self.rebuild_consumer(consumer, entry)?,
                None => self.initialize_new_consumer(consumer)?,
            }
        }
        Ok(())
    }

    fn drop_unregistered_consumers(
        &self,
        recorded: &BTreeMap<String, ConsumerManifestEntry>,
    ) -> Result<(), DeriveStoreError> {
        for (name, entry) in recorded {
            if self
                .consumers
                .iter()
                .any(|schema| schema.name.as_str() == name.as_str())
            {
                continue;
            }
            tracing::warn!(
                target: "zinder::derive",
                event = "consumer_dropped",
                consumer = name.as_str(),
                recorded_schema_version = entry.schema_version,
                "derive consumer no longer declared; resetting its cursor and clearing its column families"
            );
            self.reset_consumer_cursors(name)?;
            for column_family in &entry.column_families {
                self.clear_consumer_column_family(column_family)?;
            }
            self.delete_consumer_manifest_entry(name)?;
        }
        Ok(())
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
            let (key, payload) = entry.map_err(|source| DeriveStoreError::SchemaReconcile {
                operation: "read_manifest",
                reason: source.to_string(),
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
        let mut batch = WriteBatch::default();
        batch.delete_cf(&chain_cf, name.as_bytes());
        batch.delete_cf(&mempool_cf, name.as_bytes());
        self.write_batch(&batch)
    }

    fn write_consumer_manifest_entry(
        &self,
        consumer: &DeriveConsumerSchema,
    ) -> Result<(), DeriveStoreError> {
        let key = consumer_schema_manifest_key(consumer.name.as_str());
        let payload = encode_manifest_entry(consumer.schema_version, consumer.column_families)
            .map_err(|reason| DeriveStoreError::SchemaReconcile {
                operation: "encode_manifest_entry",
                reason,
            })?;
        self.put(DeriveStoreTable::ConsumerMetadata, &key, &payload)
    }

    fn delete_consumer_manifest_entry(&self, name: &str) -> Result<(), DeriveStoreError> {
        let key = consumer_schema_manifest_key(name);
        let column_family = self.column_family(DeriveStoreTable::ConsumerMetadata)?;
        let mut batch = WriteBatch::default();
        batch.delete_cf(&column_family, &key);
        self.write_batch(&batch)
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
        self.db.write_opt(batch, &write_options)
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
}

fn consumer_schema_manifest_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(CONSUMER_SCHEMA_KEY_PREFIX.len() + name.len());
    key.extend_from_slice(CONSUMER_SCHEMA_KEY_PREFIX);
    key.extend_from_slice(name.as_bytes());
    key
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

#[cfg(test)]
mod tests {
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
    fn manifest_entry_round_trips_version_and_column_families() -> Result<()> {
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
}
