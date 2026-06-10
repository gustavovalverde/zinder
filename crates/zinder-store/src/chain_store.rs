//! Chain store facade.

mod schema_migration;
mod validation;

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::Instant,
};

use parking_lot::RwLock;
use zinder_core::{
    ArtifactSchemaVersion, BlockBlobArtifact, BlockHash, BlockHeaderArtifact, BlockHeight,
    BlockHeightRange, BlockTransactionIndexArtifact, ChainEpoch, ChainEpochId,
    CompactBlockArtifact, Network, SubtreeRootArtifact, TransactionBlobArtifact,
    TransactionFactsArtifact, TransactionLocation, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentOutPoint, TransparentOutputArtifact,
    TransparentSpendFact, TransparentUnspentOutput, TreeStateArtifact, UnixTimestampMillis,
};

use crate::{
    ArtifactFamily, ChainEpochArtifacts, ChainEpochCommitOutcome, ChainEpochCommitted,
    ChainEpochReader, ChainEvent, ChainEventEnvelope, ChainRangeReverted, MempoolEvent,
    MempoolEventEnvelope, MempoolEventHistoryRequest, MempoolEventRetentionConfig,
    MempoolEventRetentionReport, ReorgWindowChange, RocksDbResourceBudget, StoreError,
    StreamCursorTokenV1,
    block_artifact::read_block_header_artifact,
    block_hash_index::block_hash_index_put,
    format::{
        ChainEventCursorAnchor, ChainEventStreamFamily, MempoolEventKind, MempoolEventStreamFamily,
        StoreKey, decode_chain_epoch, decode_chain_event_envelope, decode_mempool_event_envelope,
        decode_mempool_event_kind, decode_mempool_event_observed_at,
        decode_transparent_output_block_index, encode_address_output_index_artifact,
        encode_block_blob_artifact, encode_block_header_artifact,
        encode_block_transaction_index_artifact, encode_chain_epoch, encode_chain_event_envelope,
        encode_compact_block_artifact, encode_mempool_event_envelope, encode_subtree_root_artifact,
        encode_transaction_blob_artifact, encode_transaction_facts_artifact,
        encode_transaction_location_artifact, encode_transparent_address_tx_index_artifact,
        encode_transparent_output_artifact, encode_transparent_output_block_index,
        encode_transparent_spend_fact, encode_transparent_spend_fact_block_index,
        encode_tree_state_artifact,
    },
    kv::{
        PrefixScanControl, RocksChainStore, RocksChainStoreRead, StorageDelete, StoragePut,
        StorageTable,
    },
    transparent_output::read_current_transparent_outputs_by_outpoints,
    transparent_spend_fact::{
        read_current_transparent_spend_facts_by_outpoints,
        read_visible_transparent_spend_fact_block_outpoints,
    },
};

use schema_migration::migrate_primary_store_schema;
use validation::{
    committed_block_range, validate_chain_epoch_artifacts, validate_chain_store_options,
    validate_reorg_window_change, validate_visible_chain_commit,
};

/// Runtime options for [`PrimaryChainStore`] and [`SecondaryChainStore`].
///
/// Construct one with [`ChainStoreOptions::for_network`] for production use, or
/// [`ChainStoreOptions::for_local_tests`] for throwaway test stores. The struct
/// has no `Default` so callers must pick a posture explicitly. The
/// `rocksdb_resource_budget` carries the bounded `RocksDB` resource budget
/// described in [ADR-0020](../../../docs/adrs/0020-bounded-rocksdb-resource-budget.md).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainStoreOptions {
    /// Number of near-tip blocks that may be replaced by a reorg.
    pub reorg_window_blocks: u32,
    /// Whether each write batch asks `RocksDB` to fsync before returning.
    pub sync_writes: bool,
    /// Expected network for this store, used to persist and validate store metadata.
    pub network: Option<Network>,
    /// Bounded `RocksDB` resource budget applied at open time.
    pub rocksdb_resource_budget: RocksDbResourceBudget,
}

impl ChainStoreOptions {
    /// Returns durable production options anchored to `network` with fsync writes.
    #[must_use]
    pub const fn for_network(network: Network) -> Self {
        Self {
            reorg_window_blocks: 100,
            sync_writes: true,
            network: Some(network),
            rocksdb_resource_budget: RocksDbResourceBudget::canonical_writer_defaults(),
        }
    }

    /// Returns options suitable for throwaway local test stores.
    ///
    /// Uses unsynchronized writes, a regtest network anchor, and the
    /// tighter [`RocksDbResourceBudget::for_local_tests`] budget. Production code
    /// must use [`Self::for_network`] instead.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            reorg_window_blocks: 100,
            sync_writes: false,
            network: Some(Network::ZcashRegtest),
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
        }
    }
}

const STORE_SCHEMA_VERSION: u16 = 11;
/// Store schema version that the v11 startup rebuild accepts and migrates
/// in place. See [`schema_migration`].
const REBUILDABLE_STORE_SCHEMA_VERSION: u16 = 10;
/// Durable artifact schema version written by this binary.
///
/// Version 10 carried the fact-first layout with every hash-shaped proto
/// field in stored artifacts encoded as `string` in RPC byte order. Its
/// `address_output_index` rows were append-only history with an epoch
/// suffix on the key, filtered at read time.
/// See [ADR-0024](../../../docs/adrs/0024-wire-format-rpc-byte-order.md).
///
/// Version 11 keeps every artifact payload from version 10 and converts
/// `address_output_index` into a reorg-safe current projection: the key
/// drops the epoch suffix, rows derive from `transparent_outputs_by_outpoint`
/// at commit, and finalized-spent rows are deleted by the safe-tip
/// retention sweep. A version-10 store is migrated in place at primary
/// open by a one-shot streaming rebuild of the projection from
/// `transparent_output` and `transparent_spend_fact`; no resync is needed.
pub const CURRENT_ARTIFACT_SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(11);
/// Highest durable artifact schema version this binary can read.
pub const MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION: u16 = CURRENT_ARTIFACT_SCHEMA_VERSION.value();

/// Default maximum chain events returned by one history read.
pub const DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);

/// Chain-event retention state observed after a pruning or inspection pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainEventRetentionReport {
    /// Latest chain-event sequence written to the store.
    pub current_event_sequence: u64,
    /// Oldest retained sequence, or `None` when the store has no chain events.
    pub oldest_retained_sequence: Option<u64>,
    /// Creation time for [`Self::oldest_retained_sequence`], when retained.
    pub oldest_retained_created_at: Option<UnixTimestampMillis>,
    /// Number of event rows retained after the pass.
    pub retained_event_count: u64,
    /// Number of event rows deleted by this pass.
    pub pruned_event_count: u64,
}

/// Bounded transparent-address tx-history page request.
#[derive(Clone, Copy, Debug)]
pub struct TransparentAddressTxIndexPageRequest<'cursor> {
    /// Optional chain epoch to pin the read to. `None` reads at the
    /// currently visible epoch.
    pub at_epoch: Option<ChainEpoch>,
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Inclusive minimum block height. Ignored when `from_cursor` is
    /// `Some`.
    pub start_height: BlockHeight,
    /// Inclusive maximum block height.
    pub end_height: BlockHeight,
    /// Server-bounded maximum entries per page.
    pub max_entries: NonZeroU32,
    /// Iteration direction.
    pub descending: bool,
    /// Optional cursor to resume strictly after.
    pub from_cursor: Option<&'cursor StreamCursorTokenV1>,
}

/// Bounded transparent-address tx-history page response.
#[derive(Clone, Debug)]
pub struct TransparentAddressTxIndexPage {
    /// Chain epoch used to answer this page.
    pub chain_epoch: ChainEpoch,
    /// Tx-history artifacts in the requested order.
    pub artifacts: Vec<TransparentAddressTxIndexArtifact>,
    /// Resume cursor when the page reached `max_entries`.
    pub next_cursor: Option<StreamCursorTokenV1>,
}

/// Bounded transparent-address output read request.
///
/// Cursor handling lives entirely inside the store: the cursor token is
/// HMAC-authenticated against the store's per-instance auth key and decoded
/// into a typed position in `read_address_output_index_rows_paged`.
#[derive(Clone, Copy, Debug)]
pub struct AddressOutputIndexPageRequest<'cursor> {
    /// Optional chain epoch to pin the read to. `None` reads at the
    /// currently visible epoch.
    pub at_epoch: Option<ChainEpoch>,
    /// SHA-256 of the transparent address scriptPubKey.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Wallet-birthday optimization: skip outputs mined below this height.
    /// Ignored when `from_cursor` is `Some`.
    pub start_height: BlockHeight,
    /// Server-bounded maximum entries per page.
    pub max_entries: NonZeroU32,
    /// Optional cursor to resume strictly after. When present,
    /// `start_height` is ignored.
    pub from_cursor: Option<&'cursor StreamCursorTokenV1>,
}

/// Bounded transparent-address output page response.
#[derive(Clone, Debug)]
pub struct AddressOutputIndexPage {
    /// Chain epoch used to answer this page.
    pub chain_epoch: ChainEpoch,
    /// outputs in ascending `(block_height, outpoint)` order.
    pub outputs: Vec<TransparentUnspentOutput>,
    /// Resume cursor when the page reached `max_entries`. `None` when the
    /// scan was fully drained.
    pub next_cursor: Option<StreamCursorTokenV1>,
}

/// Bounded chain-event history read request.
#[derive(Clone, Copy, Debug)]
pub struct ChainEventHistoryRequest<'cursor> {
    /// Cursor to resume strictly after, or `None` to read from retained history start.
    pub from_cursor: Option<&'cursor StreamCursorTokenV1>,
    /// Chain-event stream family to read when `from_cursor` is absent.
    ///
    /// When `from_cursor` is present, the cursor's encoded family is
    /// authoritative so reconnecting clients do not need to remember a
    /// parallel option.
    pub family: ChainEventStreamFamily,
    /// Maximum number of events returned in this page.
    pub max_events: NonZeroU32,
}

impl<'cursor> ChainEventHistoryRequest<'cursor> {
    /// Creates a bounded chain-event history read request.
    #[must_use]
    pub const fn new(
        from_cursor: Option<&'cursor StreamCursorTokenV1>,
        max_events: NonZeroU32,
    ) -> Self {
        Self::new_for_family(from_cursor, ChainEventStreamFamily::Tip, max_events)
    }

    /// Creates a bounded chain-event history read request for `family`.
    #[must_use]
    pub const fn new_for_family(
        from_cursor: Option<&'cursor StreamCursorTokenV1>,
        family: ChainEventStreamFamily,
        max_events: NonZeroU32,
    ) -> Self {
        Self {
            from_cursor,
            family,
            max_events,
        }
    }

    /// Creates a chain-event history read request with the default page size.
    #[must_use]
    pub const fn with_default_limit(from_cursor: Option<&'cursor StreamCursorTokenV1>) -> Self {
        Self::new(from_cursor, DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS)
    }
}

/// Outcome returned after a `RocksDB` secondary catchup attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecondaryCatchupOutcome {
    /// Visible epoch before catchup.
    pub before: Option<ChainEpochId>,
    /// Visible epoch after catchup.
    pub after: Option<ChainEpochId>,
}

impl SecondaryCatchupOutcome {
    const fn new(before: Option<ChainEpochId>, after: Option<ChainEpochId>) -> Self {
        Self { before, after }
    }
}

/// Canonical chain store opened as the single primary writer.
#[derive(Clone)]
pub struct PrimaryChainStore {
    store: ChainStoreInner,
    /// Cache of the currently visible chain epoch, populated on first read
    /// and refreshed on every successful [`commit_chain_epoch`]. Lets the
    /// mempool orchestrator stamp per-event `first_seen_chain_epoch` without
    /// touching `RocksDB` on each observation.
    cached_visible_chain_epoch: Arc<RwLock<Option<ChainEpoch>>>,
}

/// Canonical chain store opened as a `RocksDB` secondary reader.
#[derive(Clone)]
pub struct SecondaryChainStore {
    store: ChainStoreInner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChainStoreReadPosture {
    Snapshot,
    Direct,
}

/// In-process facade shared by primary and secondary role handles.
#[derive(Clone)]
struct ChainStoreInner {
    inner: Arc<RocksChainStore>,
    options: ChainStoreOptions,
    cursor_auth_key: [u8; 32],
    read_posture: ChainStoreReadPosture,
}

#[derive(Clone, Copy)]
struct ChainEventHistoryBounds {
    current_event_sequence: u64,
    oldest_retained_sequence: u64,
}

/// Public Rust API used by `zinder-query` to read epoch-bound canonical data.
pub trait ChainEpochReadApi {
    /// Opens a reader pinned to the currently visible chain epoch.
    fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError>;

    /// Opens a reader pinned to a specific chain epoch.
    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError>;

    /// Reads a bounded page of retained chain events strictly after the request cursor.
    fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError>;

    /// Reads a bounded page of unspent transparent outputs.
    ///
    /// Decodes any supplied cursor against the store's per-instance auth key
    /// and resumes scanning strictly after the cursor position. Emits a
    /// `next_cursor` only when the page reached `max_entries`, signalling
    /// that more outputs may be available.
    fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError>;

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts inside an inclusive height range, in ascending or
    /// descending mined order.
    fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError>;
}

impl PrimaryChainStore {
    /// Opens or creates the canonical chain store as the primary writer.
    pub fn open(path: impl AsRef<Path>, options: ChainStoreOptions) -> Result<Self, StoreError> {
        validate_chain_store_options(options)?;
        let inner = Arc::new(RocksChainStore::open_primary(
            path,
            options.sync_writes,
            options.rocksdb_resource_budget,
        )?);
        migrate_primary_store_schema(&inner)?;
        let store =
            ChainStoreInner::from_primary_inner(inner, options, ChainStoreReadPosture::Snapshot)?;

        Ok(Self {
            store,
            cached_visible_chain_epoch: Arc::new(RwLock::new(None)),
        })
    }

    /// Flushes every column family's active memtable to `SST` and
    /// truncates the WAL.
    ///
    /// Called by `zinder-ingest` between `BulkCatchup` batches to bound
    /// the live WAL by writer cadence rather than `RocksDB`'s own
    /// WAL-size trigger. See [the OOM-recovery
    /// runbook](../../../docs/runbooks/bulk-catchup-oom-recovery.md) for
    /// the rationale.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.store.inner.flush()
    }

    /// Reads the currently visible chain epoch.
    pub fn current_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError> {
        let cached = *self.cached_visible_chain_epoch.read();
        if cached.is_some() {
            return Ok(cached);
        }
        let fetched = self.store.current_chain_epoch()?;
        if let Some(chain_epoch) = fetched {
            *self.cached_visible_chain_epoch.write() = Some(chain_epoch);
        }
        Ok(fetched)
    }

    /// Opens a reader pinned to the currently visible chain epoch.
    pub fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        self.store.current_chain_epoch_reader()
    }

    /// Opens a reader pinned to a specific chain epoch.
    pub fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        self.store.chain_epoch_reader_at(chain_epoch)
    }

    /// Resolves transparent outputs on the primary writer's direct read path.
    ///
    /// This skips snapshot pinning and visibility filtering because the writer
    /// calls it while deriving a node-validated commit against the current
    /// visible epoch. External readers must use [`ChainEpochReader`] instead.
    pub fn transparent_outputs_by_outpoints_for_writer_commit(
        &self,
        chain_epoch: ChainEpoch,
        outpoints: &[TransparentOutPoint],
    ) -> Result<HashMap<TransparentOutPoint, TransparentOutputArtifact>, StoreError> {
        read_current_transparent_outputs_by_outpoints(
            &self.store.inner.direct_read_view(),
            chain_epoch,
            outpoints,
        )
    }

    /// Atomically commits artifacts for one chain epoch and advances the visible pointer.
    pub fn commit_chain_epoch(
        &self,
        artifacts: ChainEpochArtifacts,
    ) -> Result<ChainEpochCommitOutcome, StoreError> {
        let outcome = self.store.commit_chain_epoch(artifacts)?;
        *self.cached_visible_chain_epoch.write() = Some(outcome.chain_epoch);
        Ok(outcome)
    }

    /// Reads a bounded page of retained chain events strictly after the request cursor.
    pub fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.store.chain_event_history(request)
    }

    /// Reads a bounded page of unspent transparent outputs.
    pub fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError> {
        self.store.address_output_index_page(request)
    }

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts.
    pub fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError> {
        self.store.transparent_address_tx_index_page(request)
    }

    /// Deletes retained chain-event rows older than `cutoff_created_at`.
    ///
    /// The newest event is always retained, preserving a replay anchor even
    /// when the entire event log falls outside the configured time window.
    pub fn prune_chain_events_before(
        &self,
        cutoff_created_at: UnixTimestampMillis,
    ) -> Result<ChainEventRetentionReport, StoreError> {
        self.store.prune_chain_events_before(cutoff_created_at)
    }

    /// Reads the current chain-event retention floor without pruning.
    pub fn chain_event_retention_report(&self) -> Result<ChainEventRetentionReport, StoreError> {
        self.store.chain_event_retention_report()
    }

    /// Appends a single mempool event to the persistent log.
    pub fn append_mempool_event(
        &self,
        event: MempoolEvent,
        source_observed_at: UnixTimestampMillis,
    ) -> Result<MempoolEventEnvelope, StoreError> {
        self.store.append_mempool_event(event, source_observed_at)
    }

    /// Reads a bounded page of retained mempool events strictly after the
    /// request cursor.
    pub fn mempool_event_history(
        &self,
        request: MempoolEventHistoryRequest<'_>,
    ) -> Result<Vec<MempoolEventEnvelope>, StoreError> {
        self.store.mempool_event_history(request)
    }

    /// Deletes retained mempool-event rows older than the per-variant
    /// retention windows.
    pub fn prune_mempool_events_before(
        &self,
        now: UnixTimestampMillis,
        retention: MempoolEventRetentionConfig,
    ) -> Result<MempoolEventRetentionReport, StoreError> {
        self.store.prune_mempool_events_before(now, retention)
    }

    /// Reads the current mempool-event retention floor without pruning.
    pub fn mempool_event_retention_report(
        &self,
    ) -> Result<MempoolEventRetentionReport, StoreError> {
        self.store.mempool_event_retention_report()
    }

    /// Returns the network bound to this store, used by mempool cursor
    /// encoding.
    #[must_use]
    pub fn network(&self) -> Option<Network> {
        self.store.options.network
    }

    /// Creates a `RocksDB` checkpoint for backup or fixture capture.
    pub fn create_checkpoint(&self, path: impl AsRef<Path>) -> Result<(), StoreError> {
        self.store.inner.create_checkpoint(path)
    }
}

impl SecondaryChainStore {
    /// Opens the canonical chain store as a `RocksDB` secondary reader.
    pub fn open(
        primary_path: impl AsRef<Path>,
        secondary_path: impl AsRef<Path>,
        options: ChainStoreOptions,
    ) -> Result<Self, StoreError> {
        validate_chain_store_options(options)?;
        let inner = Arc::new(RocksChainStore::open_secondary(
            primary_path,
            secondary_path,
            options.sync_writes,
            options.rocksdb_resource_budget,
        )?);
        let store =
            ChainStoreInner::from_secondary_inner(inner, options, ChainStoreReadPosture::Direct)?;

        Ok(Self { store })
    }

    /// Reads the currently visible chain epoch.
    pub fn current_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError> {
        self.store.current_chain_epoch()
    }

    /// Opens a reader pinned to the currently visible chain epoch.
    pub fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        self.store.current_chain_epoch_reader()
    }

    /// Opens a reader pinned to a specific chain epoch.
    pub fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        self.store.chain_epoch_reader_at(chain_epoch)
    }

    /// Reads a bounded page of retained chain events strictly after the request cursor.
    pub fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.store.chain_event_history(request)
    }

    /// Reads a bounded page of unspent transparent outputs.
    pub fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError> {
        self.store.address_output_index_page(request)
    }

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts.
    pub fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError> {
        self.store.transparent_address_tx_index_page(request)
    }

    /// Reads the current chain-event retention floor without pruning.
    pub fn chain_event_retention_report(&self) -> Result<ChainEventRetentionReport, StoreError> {
        self.store.chain_event_retention_report()
    }

    /// Reads a bounded page of retained mempool events strictly after the
    /// request cursor.
    pub fn mempool_event_history(
        &self,
        request: MempoolEventHistoryRequest<'_>,
    ) -> Result<Vec<MempoolEventEnvelope>, StoreError> {
        self.store.mempool_event_history(request)
    }

    /// Reads the current mempool-event retention floor without pruning.
    pub fn mempool_event_retention_report(
        &self,
    ) -> Result<MempoolEventRetentionReport, StoreError> {
        self.store.mempool_event_retention_report()
    }

    /// Replays available WAL and manifest state from the primary store.
    pub fn try_catch_up(&self) -> Result<SecondaryCatchupOutcome, StoreError> {
        let before = self.store.current_chain_epoch_id()?;
        self.store.inner.try_catch_up_with_primary()?;
        let after = self.store.current_chain_epoch_id()?;

        Ok(SecondaryCatchupOutcome::new(before, after))
    }
}

impl ChainStoreInner {
    fn from_primary_inner(
        inner: Arc<RocksChainStore>,
        options: ChainStoreOptions,
        read_posture: ChainStoreReadPosture,
    ) -> Result<Self, StoreError> {
        let cursor_auth_key = {
            let _control_guard = inner.lock_control();
            if let Some(network) = options.network {
                ensure_store_metadata(&inner, network)?;
            }
            ensure_supported_artifact_schema(inner.as_ref())?;
            ensure_cursor_auth_key(&inner)?
        };

        Ok(Self {
            inner,
            options,
            cursor_auth_key,
            read_posture,
        })
    }

    fn from_secondary_inner(
        inner: Arc<RocksChainStore>,
        options: ChainStoreOptions,
        read_posture: ChainStoreReadPosture,
    ) -> Result<Self, StoreError> {
        let cursor_auth_key = {
            if let Some(network) = options.network {
                validate_store_metadata(inner.as_ref(), network)?;
            }
            ensure_supported_artifact_schema(inner.as_ref())?;
            read_cursor_auth_key(inner.as_ref())?
        };

        Ok(Self {
            inner,
            options,
            cursor_auth_key,
            read_posture,
        })
    }

    /// Reads the currently visible chain epoch.
    fn current_chain_epoch(&self) -> Result<Option<ChainEpoch>, StoreError> {
        let read_view = self.read_view();
        read_current_chain_epoch_id(&read_view)?
            .map(|chain_epoch_id| read_chain_epoch(&read_view, chain_epoch_id))
            .transpose()
    }

    /// Opens a reader pinned to the currently visible chain epoch.
    fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        let read_view = self.read_view();
        let chain_epoch = require_current_chain_epoch(&read_view)?;
        Ok(ChainEpochReader::current(chain_epoch, read_view))
    }

    /// Opens a reader pinned to a specific chain epoch.
    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        let read_view = self.read_view();
        let chain_epoch = read_chain_epoch(&read_view, chain_epoch)?;
        Ok(ChainEpochReader::at_epoch(chain_epoch, read_view))
    }

    /// Atomically commits artifacts for one chain epoch and advances the visible pointer.
    fn commit_chain_epoch(
        &self,
        artifacts: ChainEpochArtifacts,
    ) -> Result<ChainEpochCommitOutcome, StoreError> {
        let _control_guard = self.inner.lock_control();
        validate_chain_epoch_artifacts(&artifacts)?;
        let store_metadata_put =
            validate_store_metadata_for_commit(&self.inner, artifacts.chain_epoch.network)?;
        let current_chain_epoch = self.validate_commit_order(&artifacts.chain_epoch)?;
        validate_reorg_window_change(&artifacts, current_chain_epoch, self.options)?;
        validate_visible_chain_commit(&self.inner, &artifacts, current_chain_epoch)?;
        let event_sequence = self
            .current_chain_event_sequence()?
            .checked_add(1)
            .ok_or(StoreError::ChainEventSequenceOverflow)?;

        let chain_epoch = artifacts.chain_epoch;
        let block_range = committed_block_range(&artifacts, current_chain_epoch)?;
        let reorg_window_change = artifacts.reorg_window_change.clone();
        let current_projection_protected_outpoints = artifacts
            .transparent_outputs_by_outpoint
            .iter()
            .map(|prevout| (prevout.outpoint, prevout.block_height))
            .collect::<HashMap<_, _>>();
        let current_spend_projection_protected_outpoints = artifacts
            .transparent_spend_facts
            .iter()
            .map(|spend| spend.spent_outpoint)
            .collect::<HashSet<_>>();
        let retention_sweep =
            build_safe_tip_retention_sweep(self.inner.as_ref(), &artifacts, current_chain_epoch)?;
        let committed = ChainEpochCommitted::new(chain_epoch, block_range);
        let event_envelope = build_chain_event_envelope(
            event_sequence,
            committed,
            current_chain_epoch,
            &reorg_window_change,
            self.cursor_auth_key,
        )?;
        let mut puts = build_chain_epoch_puts(artifacts, &event_envelope)?;
        if let Some(store_metadata_put) = store_metadata_put {
            puts.push(store_metadata_put);
        }
        let projection_repairs = build_reorg_window_projection_repairs(
            self.inner.as_ref(),
            TransparentOutputProjectionRepairInputs {
                previous_chain_epoch: current_chain_epoch,
                chain_epoch,
                reorg_window_change: &reorg_window_change,
                protected_outpoints: &current_projection_protected_outpoints,
            },
        )?;
        let spend_projection_repairs = build_reorg_window_spend_fact_projection_repairs(
            self.inner.as_ref(),
            TransparentSpendFactProjectionRepairInputs {
                previous_chain_epoch: current_chain_epoch,
                chain_epoch,
                reorg_window_change: &reorg_window_change,
                protected_outpoints: &current_spend_projection_protected_outpoints,
            },
        )?;
        let swept_outpoints = retention_sweep.swept_outpoints;
        puts.extend(retention_sweep.puts);
        let mut deletes = projection_repairs.deletes;
        deletes.extend(spend_projection_repairs.deletes);
        deletes.extend(retention_sweep.deletes);

        self.inner.write_batch(puts, deletes)?;
        record_safe_tip_retention_sweep(swept_outpoints);

        Ok(ChainEpochCommitOutcome::new(committed, event_envelope))
    }

    /// Reads a bounded page of retained chain events strictly after the request cursor.
    fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        let read_view = self.read_view();
        let current_event_sequence = read_current_chain_event_sequence(&read_view)?;
        if current_event_sequence == 0 {
            if request.from_cursor.is_some() {
                return Err(StoreError::ChainEventCursorInvalid {
                    reason: "cursor sequence is ahead of retained history",
                });
            }

            return Ok(Vec::new());
        }

        let current_chain_epoch = require_current_chain_epoch(&read_view)?;
        let oldest_retained_sequence =
            read_oldest_retained_chain_event_sequence(&read_view, current_event_sequence)?
                .unwrap_or(1);
        let (start_sequence, family) = self.chain_event_history_start_sequence(
            &read_view,
            request,
            &current_chain_epoch,
            ChainEventHistoryBounds {
                current_event_sequence,
                oldest_retained_sequence,
            },
        )?;

        if start_sequence > current_event_sequence {
            return Ok(Vec::new());
        }

        let max_events = u64::from(request.max_events.get());
        let mut event_sequence = start_sequence;
        let mut event_envelopes = Vec::with_capacity(u32_to_usize(request.max_events.get()));
        while event_sequence <= current_event_sequence
            && u64::try_from(event_envelopes.len()).map_or(true, |count| count < max_events)
        {
            let key = StoreKey::chain_event(event_sequence);
            let Some(record_bytes) = read_view.get(StorageTable::ChainEvent, &key)? else {
                return Err(StoreError::ChainEventCursorExpired {
                    event_sequence: start_sequence.saturating_sub(1),
                    oldest_retained_sequence: event_sequence,
                });
            };
            let event_envelope =
                decode_chain_event_envelope(&key, &record_bytes, family, self.cursor_auth_key)?;
            if chain_event_matches_family(&event_envelope, family) {
                event_envelopes.push(event_envelope);
            }
            event_sequence = event_sequence
                .checked_add(1)
                .ok_or(StoreError::ChainEventSequenceOverflow)?;
        }

        Ok(event_envelopes)
    }

    /// Reads a bounded page of unspent transparent outputs.
    fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError> {
        let read_view = self.read_view();
        let chain_epoch = match request.at_epoch {
            Some(at_epoch) => {
                let stored = read_chain_epoch(&read_view, at_epoch.id)?;
                if stored != at_epoch {
                    return Err(StoreError::ChainEpochMissing {
                        chain_epoch: at_epoch.id,
                    });
                }
                stored
            }
            None => require_current_chain_epoch(&read_view)?,
        };

        let resume_after = match request.from_cursor {
            Some(cursor) => Some(
                cursor
                    .decode_address_output(chain_epoch.network, self.cursor_auth_key)
                    .map_err(|_| StoreError::AddressOutputCursorInvalid {
                        reason: "cursor token failed validation",
                    })?,
            ),
            None => None,
        };

        let outputs = crate::address_output_index::read_address_output_index_rows_paged(
            &read_view,
            crate::address_output_index::AddressOutputIndexRowsScan {
                chain_epoch,
                address_script_hash: request.address_script_hash,
                start_height: request.start_height,
                max_entries: request.max_entries,
                resume_after,
            },
        )?;

        let next_cursor = if outputs.len() >= u32_to_usize(request.max_entries.get()) {
            match outputs.last() {
                Some(output) => Some(
                    StreamCursorTokenV1::address_output(
                        chain_epoch.network,
                        crate::format::AddressOutputStreamFamily::AddressOutput,
                        output.block_height,
                        output.outpoint,
                        self.cursor_auth_key,
                    )
                    .map_err(|_| StoreError::AddressOutputCursorInvalid {
                        reason: "next-cursor encoding failed",
                    })?,
                ),
                None => None,
            }
        } else {
            None
        };

        Ok(AddressOutputIndexPage {
            chain_epoch,
            outputs,
            next_cursor,
        })
    }

    /// Reads a bounded page of transparent-address tx-history index
    /// artifacts.
    fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError> {
        let read_view = self.read_view();
        let chain_epoch = match request.at_epoch {
            Some(at_epoch) => {
                let stored = read_chain_epoch(&read_view, at_epoch.id)?;
                if stored != at_epoch {
                    return Err(StoreError::ChainEpochMissing {
                        chain_epoch: at_epoch.id,
                    });
                }
                stored
            }
            None => require_current_chain_epoch(&read_view)?,
        };

        let resume_after = match request.from_cursor {
            Some(cursor) => Some(
                cursor
                    .decode_transparent_history(chain_epoch.network, self.cursor_auth_key)
                    .map_err(|_| StoreError::TransparentHistoryCursorInvalid {
                        reason: "cursor token failed validation",
                    })?,
            ),
            None => None,
        };
        let descending = resume_after.map_or(request.descending, |payload| payload.descending);
        let resume_position = resume_after.map(|payload| {
            crate::transparent_address_tx_index::TransparentHistoryResumePosition {
                last_block_height: payload.last_block_height,
                last_tx_index_in_block: payload.last_tx_index_in_block,
            }
        });

        let artifacts =
            crate::transparent_address_tx_index::read_transparent_address_tx_index_paged(
                &read_view,
                crate::transparent_address_tx_index::TransparentAddressTxIndexScan {
                    chain_epoch,
                    address_script_hash: request.address_script_hash,
                    start_height: request.start_height,
                    end_height: request.end_height,
                    max_entries: request.max_entries,
                    descending,
                    resume_after: resume_position,
                },
            )?;

        let next_cursor =
            if artifacts.len() >= u32_to_usize(request.max_entries.get()) {
                match artifacts.last() {
                Some(artifact) => Some(
                    StreamCursorTokenV1::transparent_history(
                        crate::format::TransparentHistoryCursorAnchor {
                            network: chain_epoch.network,
                            family:
                                crate::format::TransparentHistoryStreamFamily::TransparentHistory,
                            last_block_height: artifact.block_height,
                            last_tx_index_in_block: artifact.tx_index_in_block,
                            descending,
                        },
                        self.cursor_auth_key,
                    )
                    .map_err(|_| StoreError::TransparentHistoryCursorInvalid {
                        reason: "next-cursor encoding failed",
                    })?,
                ),
                None => None,
            }
            } else {
                None
            };

        Ok(TransparentAddressTxIndexPage {
            chain_epoch,
            artifacts,
            next_cursor,
        })
    }

    fn chain_event_history_start_sequence(
        &self,
        inner: &impl RocksChainStoreRead,
        request: ChainEventHistoryRequest<'_>,
        current_chain_epoch: &ChainEpoch,
        bounds: ChainEventHistoryBounds,
    ) -> Result<(u64, ChainEventStreamFamily), StoreError> {
        let Some(cursor) = request.from_cursor else {
            return Ok((bounds.oldest_retained_sequence, request.family));
        };

        let cursor_payload = cursor
            .decode_chain_event(current_chain_epoch.network, self.cursor_auth_key)
            .map_err(|_| StoreError::ChainEventCursorInvalid {
                reason: "cursor token failed validation",
            })?;

        if cursor_payload.event_sequence > bounds.current_event_sequence {
            return Err(StoreError::ChainEventCursorInvalid {
                reason: "cursor sequence is ahead of retained history",
            });
        }

        if cursor_payload.event_sequence == 0 {
            return Err(StoreError::ChainEventCursorInvalid {
                reason: "cursor sequence is before retained history",
            });
        }

        if cursor_payload.event_sequence < bounds.oldest_retained_sequence {
            return Err(StoreError::ChainEventCursorExpired {
                event_sequence: cursor_payload.event_sequence,
                oldest_retained_sequence: bounds.oldest_retained_sequence,
            });
        }

        let cursor_event_key = StoreKey::chain_event(cursor_payload.event_sequence);
        let Some(cursor_event_bytes) = inner.get(StorageTable::ChainEvent, &cursor_event_key)?
        else {
            return Err(StoreError::ChainEventCursorExpired {
                event_sequence: cursor_payload.event_sequence,
                oldest_retained_sequence: cursor_payload.event_sequence.saturating_add(1),
            });
        };
        let cursor_event_envelope = decode_chain_event_envelope(
            &cursor_event_key,
            &cursor_event_bytes,
            cursor_payload.family,
            self.cursor_auth_key,
        )?;
        let retained_position = (
            cursor_event_envelope.chain_epoch.tip_height,
            cursor_event_envelope.chain_epoch.tip_hash,
        );
        let cursor_position = (cursor_payload.last_height, cursor_payload.last_hash);
        if retained_position != cursor_position {
            return Err(StoreError::ChainEventCursorInvalid {
                reason: "cursor position does not match retained event",
            });
        }

        cursor_payload
            .event_sequence
            .checked_add(1)
            .map(|start_sequence| (start_sequence, cursor_payload.family))
            .ok_or(StoreError::ChainEventSequenceOverflow)
    }

    fn prune_chain_events_before(
        &self,
        cutoff_created_at: UnixTimestampMillis,
    ) -> Result<ChainEventRetentionReport, StoreError> {
        let started_at = Instant::now();
        let prune_outcome = (|| {
            let _control_guard = self.inner.lock_control();
            let current_event_sequence = read_current_chain_event_sequence(self.inner.as_ref())?;
            let Some(oldest_retained_sequence) = read_oldest_retained_chain_event_sequence(
                self.inner.as_ref(),
                current_event_sequence,
            )?
            else {
                return Ok(ChainEventRetentionReport::empty());
            };

            let new_oldest_retained_sequence = self.oldest_retained_sequence_for_cutoff(
                oldest_retained_sequence,
                current_event_sequence,
                cutoff_created_at,
            )?;
            let pruned_event_count =
                new_oldest_retained_sequence.saturating_sub(oldest_retained_sequence);

            if pruned_event_count > 0 {
                let deletes = (oldest_retained_sequence..new_oldest_retained_sequence)
                    .map(|event_sequence| StorageDelete {
                        table: StorageTable::ChainEvent,
                        key: StoreKey::chain_event(event_sequence),
                    })
                    .collect();
                self.inner.write_batch(
                    vec![StoragePut {
                        table: StorageTable::StorageControl,
                        key: StoreKey::oldest_retained_chain_event_sequence(),
                        value: new_oldest_retained_sequence.to_be_bytes().to_vec(),
                    }],
                    deletes,
                )?;
            }

            self.chain_event_retention_report_locked()
                .map(|report| ChainEventRetentionReport {
                    pruned_event_count,
                    ..report
                })
        })();
        record_chain_event_prune_outcome(started_at, &prune_outcome);
        if let Ok(report) = prune_outcome {
            record_chain_event_retention_report(report);
            return Ok(report);
        }

        prune_outcome
    }

    fn oldest_retained_sequence_for_cutoff(
        &self,
        oldest_retained_sequence: u64,
        current_event_sequence: u64,
        cutoff_created_at: UnixTimestampMillis,
    ) -> Result<u64, StoreError> {
        let mut event_sequence = oldest_retained_sequence;
        while event_sequence < current_event_sequence {
            let key = StoreKey::chain_event(event_sequence);
            let Some(record_bytes) = self.inner.get(StorageTable::ChainEvent, &key)? else {
                event_sequence = event_sequence
                    .checked_add(1)
                    .ok_or(StoreError::ChainEventSequenceOverflow)?;
                continue;
            };
            let event_envelope = decode_chain_event_envelope(
                &key,
                &record_bytes,
                ChainEventStreamFamily::Tip,
                self.cursor_auth_key,
            )?;
            if event_envelope.chain_epoch.created_at >= cutoff_created_at {
                break;
            }
            event_sequence = event_sequence
                .checked_add(1)
                .ok_or(StoreError::ChainEventSequenceOverflow)?;
        }

        Ok(event_sequence)
    }

    fn chain_event_retention_report(&self) -> Result<ChainEventRetentionReport, StoreError> {
        self.chain_event_retention_report_via(&self.read_view())
    }

    fn chain_event_retention_report_locked(&self) -> Result<ChainEventRetentionReport, StoreError> {
        self.chain_event_retention_report_via(self.inner.as_ref())
    }

    fn chain_event_retention_report_via<R: RocksChainStoreRead>(
        &self,
        read_source: &R,
    ) -> Result<ChainEventRetentionReport, StoreError> {
        let current_event_sequence = read_current_chain_event_sequence(read_source)?;
        let Some(oldest_retained_sequence) =
            read_oldest_retained_chain_event_sequence(read_source, current_event_sequence)?
        else {
            return Ok(ChainEventRetentionReport::empty());
        };
        build_chain_event_retention_report(
            read_source,
            oldest_retained_sequence,
            current_event_sequence,
            self.cursor_auth_key,
            0,
        )
    }

    fn current_chain_epoch_id(&self) -> Result<Option<ChainEpochId>, StoreError> {
        let read_view = self.read_view();
        read_current_chain_epoch_id(&read_view)
    }

    fn current_chain_event_sequence(&self) -> Result<u64, StoreError> {
        read_current_chain_event_sequence(self.inner.as_ref())
    }

    fn append_mempool_event(
        &self,
        event: MempoolEvent,
        source_observed_at: UnixTimestampMillis,
    ) -> Result<MempoolEventEnvelope, StoreError> {
        let _control_guard = self.inner.lock_control();
        let network = self
            .options
            .network
            .ok_or(StoreError::InvalidChainStoreOptions {
                reason: "mempool events require a network-bound store",
            })?;
        let event_sequence = read_current_mempool_event_sequence(self.inner.as_ref())?
            .checked_add(1)
            .ok_or(StoreError::MempoolEventSequenceOverflow)?;
        let cursor = StreamCursorTokenV1::mempool_event(
            network,
            MempoolEventStreamFamily::Mempool,
            event_sequence,
            event.transaction_id(),
            self.cursor_auth_key,
        )
        .map_err(|_| StoreError::InvalidChainEpochArtifacts {
            reason: "cursor authentication key could not initialize the MAC",
        })?;
        let envelope = MempoolEventEnvelope {
            cursor,
            event_sequence,
            source_observed_unix_millis: source_observed_at.value(),
            event,
        };

        let mut puts = vec![
            StoragePut {
                table: StorageTable::MempoolEvent,
                key: StoreKey::mempool_event(event_sequence),
                value: encode_mempool_event_envelope(&envelope),
            },
            StoragePut {
                table: StorageTable::StorageControl,
                key: StoreKey::mempool_event_sequence_pointer(),
                value: event_sequence.to_be_bytes().to_vec(),
            },
        ];
        if event_sequence == 1 {
            puts.push(StoragePut {
                table: StorageTable::StorageControl,
                key: StoreKey::oldest_retained_mempool_event_sequence(),
                value: event_sequence.to_be_bytes().to_vec(),
            });
        }
        self.inner.write_batch(puts, Vec::new())?;

        Ok(envelope)
    }

    fn mempool_event_history(
        &self,
        request: MempoolEventHistoryRequest<'_>,
    ) -> Result<Vec<MempoolEventEnvelope>, StoreError> {
        let network = self
            .options
            .network
            .ok_or(StoreError::InvalidChainStoreOptions {
                reason: "mempool events require a network-bound store",
            })?;
        let read_view = self.read_view();
        let current_event_sequence = read_current_mempool_event_sequence(&read_view)?;
        if current_event_sequence == 0 {
            if request.from_cursor.is_some() {
                return Err(StoreError::MempoolEventCursorInvalid {
                    reason: "cursor sequence is ahead of retained history",
                });
            }
            return Ok(Vec::new());
        }

        let oldest_retained_sequence =
            read_oldest_retained_mempool_event_sequence(&read_view, current_event_sequence)?
                .unwrap_or(1);
        let start_sequence = self.mempool_event_history_start_sequence(
            request,
            network,
            current_event_sequence,
            oldest_retained_sequence,
        )?;

        if start_sequence > current_event_sequence {
            return Ok(Vec::new());
        }

        let max_events = u32_to_usize(request.max_events.get());
        let mut event_envelopes = Vec::with_capacity(max_events);
        let mut decode_error: Option<StoreError> = None;
        let cursor_auth_key = self.cursor_auth_key;
        read_view.scan_forward(
            StorageTable::MempoolEvent,
            &StoreKey::mempool_event(start_sequence),
            &mut |key_bytes, record_bytes| {
                if event_envelopes.len() >= max_events {
                    return Ok(PrefixScanControl::Stop);
                }
                let key = StoreKey::from_raw_bytes(key_bytes);
                match decode_mempool_event_envelope(&key, record_bytes, network, cursor_auth_key) {
                    Ok(envelope) => {
                        if envelope.event_sequence > current_event_sequence {
                            return Ok(PrefixScanControl::Stop);
                        }
                        event_envelopes.push(envelope);
                        Ok(PrefixScanControl::Continue)
                    }
                    Err(error) => {
                        decode_error = Some(error);
                        Ok(PrefixScanControl::Stop)
                    }
                }
            },
        )?;
        if let Some(error) = decode_error {
            return Err(error);
        }

        // If the first observed sequence is past `start_sequence`, the
        // requested cursor's resume point was pruned between the bounds read
        // and the iteration. Surface this as `MempoolEventCursorExpired` so
        // callers resubscribe to the new oldest-retained boundary instead
        // of silently skipping events.
        if let Some(first_envelope) = event_envelopes.first()
            && first_envelope.event_sequence > start_sequence
        {
            return Err(StoreError::MempoolEventCursorExpired {
                event_sequence: start_sequence.saturating_sub(1),
                oldest_retained_sequence: first_envelope.event_sequence,
            });
        }

        Ok(event_envelopes)
    }

    fn mempool_event_history_start_sequence(
        &self,
        request: MempoolEventHistoryRequest<'_>,
        network: Network,
        current_event_sequence: u64,
        oldest_retained_sequence: u64,
    ) -> Result<u64, StoreError> {
        let Some(cursor) = request.from_cursor else {
            return Ok(oldest_retained_sequence);
        };
        let cursor_payload = cursor
            .decode_mempool_event(network, self.cursor_auth_key)
            .map_err(|_| StoreError::MempoolEventCursorInvalid {
                reason: "cursor token failed validation",
            })?;
        if cursor_payload.event_sequence > current_event_sequence {
            return Err(StoreError::MempoolEventCursorInvalid {
                reason: "cursor sequence is ahead of retained history",
            });
        }
        if cursor_payload.event_sequence < oldest_retained_sequence {
            return Err(StoreError::MempoolEventCursorExpired {
                event_sequence: cursor_payload.event_sequence,
                oldest_retained_sequence,
            });
        }
        cursor_payload
            .event_sequence
            .checked_add(1)
            .ok_or(StoreError::MempoolEventSequenceOverflow)
    }

    fn prune_mempool_events_before(
        &self,
        now: UnixTimestampMillis,
        retention: MempoolEventRetentionConfig,
    ) -> Result<MempoolEventRetentionReport, StoreError> {
        let started_at = Instant::now();
        let prune_outcome = self.prune_mempool_events_locked(now, retention);
        record_mempool_event_prune_outcome(started_at, &prune_outcome);
        if let Ok(report) = prune_outcome {
            record_mempool_event_retention_report(report);
            return Ok(report);
        }
        prune_outcome
    }

    fn prune_mempool_events_locked(
        &self,
        now: UnixTimestampMillis,
        retention: MempoolEventRetentionConfig,
    ) -> Result<MempoolEventRetentionReport, StoreError> {
        // Phase 1: read bounds + scan against a snapshot view, lock-free, so
        // the per-event `append_mempool_event` path is not blocked during the
        // potentially-large iteration. The snapshot freezes the input the
        // delete batch is computed from; concurrent appends after the
        // snapshot only widen the retained window and remain safe to leave
        // in place.
        let read_view = self.read_view();
        let current_event_sequence = read_current_mempool_event_sequence(&read_view)?;
        let Some(oldest_retained_sequence) =
            read_oldest_retained_mempool_event_sequence(&read_view, current_event_sequence)?
        else {
            return Ok(MempoolEventRetentionReport::default());
        };
        let scan = scan_mempool_events_for_pruning(
            &read_view,
            now,
            retention,
            oldest_retained_sequence,
            current_event_sequence,
        )?;
        let new_oldest_retained = scan.new_oldest_retained.unwrap_or(current_event_sequence);
        let observed_at = read_mempool_event_observed_at(&read_view, new_oldest_retained)?;
        drop(read_view);

        // Phase 2: acquire the control lock only to apply the writes.
        //
        // The floor must advance whenever the scan computed a higher
        // `new_oldest_retained`, regardless of whether `scan.deletes` is
        // empty. A previous prune that crashed mid-batch can leave the
        // column family with gaps where the deletes physically already
        // happened but the floor never advanced; without this branch, the
        // floor would stay stuck and readers could observe a partially
        // pruned tail. Every prune call updates
        // `oldest_retained_mempool_event_sequence` atomically with the
        // column-family delete batch.
        let floor_advances = new_oldest_retained != oldest_retained_sequence;
        if floor_advances || !scan.deletes.is_empty() {
            let _control_guard = self.inner.lock_control();
            let puts = if floor_advances {
                vec![StoragePut {
                    table: StorageTable::StorageControl,
                    key: StoreKey::oldest_retained_mempool_event_sequence(),
                    value: new_oldest_retained.to_be_bytes().to_vec(),
                }]
            } else {
                Vec::new()
            };
            self.inner.write_batch(puts, scan.deletes)?;
        }

        let retained_event_count = current_event_sequence
            .saturating_sub(new_oldest_retained)
            .saturating_add(1);
        Ok(MempoolEventRetentionReport {
            current_event_sequence,
            oldest_retained_sequence: Some(new_oldest_retained),
            oldest_retained_observed_at: observed_at,
            retained_event_count,
            pruned_added_count: scan.pruned_added,
            pruned_mined_count: scan.pruned_mined,
            pruned_invalidated_count: scan.pruned_invalidated,
            pruned_suppressed_count: scan.pruned_suppressed,
        })
    }

    fn mempool_event_retention_report(&self) -> Result<MempoolEventRetentionReport, StoreError> {
        let read_view = self.read_view();
        let current_event_sequence = read_current_mempool_event_sequence(&read_view)?;
        let Some(oldest_retained_sequence) =
            read_oldest_retained_mempool_event_sequence(&read_view, current_event_sequence)?
        else {
            return Ok(MempoolEventRetentionReport::default());
        };
        let observed_at = read_mempool_event_observed_at(&read_view, oldest_retained_sequence)?;
        let retained_event_count = current_event_sequence
            .saturating_sub(oldest_retained_sequence)
            .saturating_add(1);
        Ok(MempoolEventRetentionReport {
            current_event_sequence,
            oldest_retained_sequence: Some(oldest_retained_sequence),
            oldest_retained_observed_at: observed_at,
            retained_event_count,
            pruned_added_count: 0,
            pruned_mined_count: 0,
            pruned_invalidated_count: 0,
            pruned_suppressed_count: 0,
        })
    }

    fn read_view(&self) -> crate::kv::RocksChainStoreReadView<'_> {
        match self.read_posture {
            ChainStoreReadPosture::Snapshot => self.inner.snapshot_read_view(),
            ChainStoreReadPosture::Direct => self.inner.direct_read_view(),
        }
    }

    fn validate_commit_order(
        &self,
        chain_epoch: &ChainEpoch,
    ) -> Result<Option<ChainEpoch>, StoreError> {
        let Some(current_chain_epoch) = self.current_chain_epoch_id()? else {
            return Ok(None);
        };

        if chain_epoch.id <= current_chain_epoch {
            return Err(StoreError::ChainEpochConflict {
                current: current_chain_epoch,
                attempted: chain_epoch.id,
            });
        }

        let current_epoch = read_chain_epoch(self.inner.as_ref(), current_chain_epoch)?;
        if current_epoch.network != chain_epoch.network {
            return Err(StoreError::ChainEpochNetworkMismatch {
                current: current_epoch.network,
                attempted: chain_epoch.network,
            });
        }

        Ok(Some(current_epoch))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}

impl ChainEpochReadApi for PrimaryChainStore {
    fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        self.current_chain_epoch_reader()
    }

    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        self.chain_epoch_reader_at(chain_epoch)
    }

    fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.chain_event_history(request)
    }

    fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError> {
        self.store.address_output_index_page(request)
    }

    fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError> {
        self.store.transparent_address_tx_index_page(request)
    }
}

impl ChainEpochReadApi for SecondaryChainStore {
    fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        self.current_chain_epoch_reader()
    }

    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        self.chain_epoch_reader_at(chain_epoch)
    }

    fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.chain_event_history(request)
    }

    fn address_output_index_page(
        &self,
        request: AddressOutputIndexPageRequest<'_>,
    ) -> Result<AddressOutputIndexPage, StoreError> {
        self.store.address_output_index_page(request)
    }

    fn transparent_address_tx_index_page(
        &self,
        request: TransparentAddressTxIndexPageRequest<'_>,
    ) -> Result<TransparentAddressTxIndexPage, StoreError> {
        self.store.transparent_address_tx_index_page(request)
    }
}

fn ensure_store_metadata(
    inner: &RocksChainStore,
    expected_network: Network,
) -> Result<(), StoreError> {
    if let Some(store_metadata_put) = validate_store_metadata_for_commit(inner, expected_network)? {
        inner.write(vec![store_metadata_put])?;
    }

    Ok(())
}

fn validate_store_metadata(
    inner: &impl RocksChainStoreRead,
    expected_network: Network,
) -> Result<(), StoreError> {
    let key = StoreKey::store_metadata();
    let Some(metadata_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Err(StoreError::ArtifactMissing {
            family: ArtifactFamily::ChainEpoch,
            key: key.into(),
        });
    };
    let store_metadata = decode_current_store_metadata(&key, &metadata_bytes)?;
    if store_metadata.network != expected_network {
        return Err(StoreError::ChainEpochNetworkMismatch {
            current: store_metadata.network,
            attempted: expected_network,
        });
    }

    Ok(())
}

fn ensure_supported_artifact_schema(inner: &impl RocksChainStoreRead) -> Result<(), StoreError> {
    let Some(current_chain_epoch_id) = read_current_chain_epoch_id(inner)? else {
        return Ok(());
    };
    let current_chain_epoch = read_chain_epoch(inner, current_chain_epoch_id)?;
    let persisted_version = current_chain_epoch.artifact_schema_version.value();
    if persisted_version > MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            persisted_version,
            supported_version: MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION,
        });
    }
    if persisted_version < CURRENT_ARTIFACT_SCHEMA_VERSION.value() {
        return Err(StoreError::SchemaTooOld {
            persisted_version,
            required_version: CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        });
    }

    Ok(())
}

fn validate_store_metadata_for_commit(
    inner: &RocksChainStore,
    expected_network: Network,
) -> Result<Option<StoragePut>, StoreError> {
    let key = StoreKey::store_metadata();
    if let Some(metadata_bytes) = inner.get(StorageTable::StorageControl, &key)? {
        let store_metadata = decode_current_store_metadata(&key, &metadata_bytes)?;
        if store_metadata.network != expected_network {
            return Err(StoreError::ChainEpochNetworkMismatch {
                current: store_metadata.network,
                attempted: expected_network,
            });
        }

        return Ok(None);
    }

    if let Some(current_chain_epoch_id) = read_current_chain_epoch_id(inner)? {
        let current_chain_epoch = read_chain_epoch(inner, current_chain_epoch_id)?;
        if current_chain_epoch.network != expected_network {
            return Err(StoreError::ChainEpochNetworkMismatch {
                current: current_chain_epoch.network,
                attempted: expected_network,
            });
        }
    }

    Ok(Some(StoragePut {
        table: StorageTable::StorageControl,
        key,
        value: encode_store_metadata(expected_network),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoreMetadata {
    schema_version: u16,
    network: Network,
}

fn encode_store_metadata(network: Network) -> Vec<u8> {
    let mut metadata = Vec::with_capacity(6);
    metadata.extend_from_slice(&STORE_SCHEMA_VERSION.to_be_bytes());
    metadata.extend_from_slice(&network.id().to_be_bytes());
    metadata
}

fn decode_current_store_metadata(
    key: &StoreKey,
    metadata_bytes: &[u8],
) -> Result<StoreMetadata, StoreError> {
    let store_metadata = decode_store_metadata(key, metadata_bytes)?;
    if store_metadata.schema_version != STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch {
            persisted_version: store_metadata.schema_version,
            expected_version: STORE_SCHEMA_VERSION,
        });
    }

    Ok(store_metadata)
}

fn decode_store_metadata(
    key: &StoreKey,
    metadata_bytes: &[u8],
) -> Result<StoreMetadata, StoreError> {
    if metadata_bytes.len() != 6 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.clone().into(),
            reason: "store metadata must be 6 bytes",
        });
    }

    let schema_version_bytes =
        <[u8; 2]>::try_from(&metadata_bytes[0..2]).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.clone().into(),
            reason: "store metadata schema version must be 2 bytes",
        })?;
    let schema_version = u16::from_be_bytes(schema_version_bytes);

    let network_id_bytes =
        <[u8; 4]>::try_from(&metadata_bytes[2..6]).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.clone().into(),
            reason: "store metadata network id must be 4 bytes",
        })?;
    let network_id = u32::from_be_bytes(network_id_bytes);
    let network = Network::from_id(network_id).ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEpoch,
        key: key.clone().into(),
        reason: "store metadata has an unknown network id",
    })?;

    Ok(StoreMetadata {
        schema_version,
        network,
    })
}

fn build_chain_event(
    committed: ChainEpochCommitted,
    previous_chain_epoch: Option<ChainEpoch>,
    reorg_window_change: &ReorgWindowChange,
) -> Result<ChainEvent, StoreError> {
    let event = match *reorg_window_change {
        ReorgWindowChange::Replace { from_height } => {
            let previous_chain_epoch =
                previous_chain_epoch.ok_or(StoreError::InvalidChainEpochArtifacts {
                    reason: "replacement requires an existing chain epoch",
                })?;
            let reverted = ChainRangeReverted::new(
                previous_chain_epoch,
                BlockHeightRange::inclusive(from_height, previous_chain_epoch.tip_height),
            );

            ChainEvent::ChainReorged {
                reverted,
                committed,
            }
        }
        ReorgWindowChange::Unchanged
        | ReorgWindowChange::Extend { .. }
        | ReorgWindowChange::AdvanceSafeTipTo { .. } => ChainEvent::ChainCommitted { committed },
    };

    Ok(event)
}

fn build_chain_event_envelope(
    event_sequence: u64,
    committed: ChainEpochCommitted,
    previous_chain_epoch: Option<ChainEpoch>,
    reorg_window_change: &ReorgWindowChange,
    cursor_auth_key: [u8; 32],
) -> Result<ChainEventEnvelope, StoreError> {
    let event = build_chain_event(committed, previous_chain_epoch, reorg_window_change)?;
    let chain_epoch = committed.chain_epoch;
    let cursor = StreamCursorTokenV1::chain_event(
        chain_epoch.network,
        ChainEventStreamFamily::Tip,
        event_sequence,
        ChainEventCursorAnchor {
            height: chain_epoch.tip_height,
            hash: chain_epoch.tip_hash,
        },
        cursor_auth_key,
    )
    .map_err(|_| StoreError::InvalidChainEpochArtifacts {
        reason: "cursor authentication key could not initialize the MAC",
    })?;

    Ok(ChainEventEnvelope::new(
        cursor,
        event_sequence,
        chain_epoch,
        chain_epoch.safe_tip_height,
        event,
    ))
}

fn chain_event_matches_family(
    event_envelope: &ChainEventEnvelope,
    family: ChainEventStreamFamily,
) -> bool {
    match family {
        ChainEventStreamFamily::Tip => true,
        ChainEventStreamFamily::Safe => {
            event_envelope.chain_epoch.tip_height <= event_envelope.safe_tip_height
                && matches!(&event_envelope.event, ChainEvent::ChainCommitted { .. })
        }
    }
}

fn build_chain_epoch_puts(
    artifacts: ChainEpochArtifacts,
    event_envelope: &ChainEventEnvelope,
) -> Result<Vec<StoragePut>, StoreError> {
    let ChainEpochArtifacts {
        chain_epoch,
        block_headers,
        block_blobs,
        compact_blocks,
        block_transaction_index,
        transaction_locations,
        transaction_facts,
        transaction_blobs,
        tree_states,
        subtree_roots,
        transparent_outputs_by_outpoint,
        transparent_spend_facts,
        transparent_address_tx_index,
        reorg_window_change: _,
    } = artifacts;

    let mut puts = Vec::new();
    puts.push(StoragePut {
        table: StorageTable::ChainEpoch,
        key: StoreKey::chain_epoch(chain_epoch.id),
        value: encode_chain_epoch(&chain_epoch),
    });
    push_block_header_artifact_puts(&mut puts, chain_epoch, block_headers)?;
    push_block_blob_artifact_puts(&mut puts, chain_epoch, block_blobs)?;
    push_compact_block_artifact_puts(&mut puts, chain_epoch, compact_blocks)?;
    push_block_transaction_index_artifact_puts(&mut puts, chain_epoch, block_transaction_index)?;
    push_transaction_location_puts(&mut puts, chain_epoch, transaction_locations)?;
    push_transaction_facts_artifact_puts(&mut puts, chain_epoch, transaction_facts)?;
    push_transaction_blob_artifact_puts(&mut puts, chain_epoch, transaction_blobs)?;
    push_tree_state_artifact_puts(&mut puts, chain_epoch, tree_states)?;
    push_subtree_root_artifact_puts(&mut puts, chain_epoch, subtree_roots)?;
    push_transparent_output_artifact_puts(&mut puts, chain_epoch, transparent_outputs_by_outpoint)?;
    push_transparent_spend_fact_puts(&mut puts, chain_epoch, transparent_spend_facts)?;
    push_transparent_address_tx_index_artifact_puts(
        &mut puts,
        chain_epoch,
        transparent_address_tx_index,
    )?;
    push_commit_control_puts(&mut puts, chain_epoch, event_envelope);

    Ok(puts)
}

struct TransparentOutputProjectionRepairs {
    deletes: Vec<StorageDelete>,
}

#[derive(Clone, Copy)]
struct TransparentOutputProjectionRepairInputs<'input> {
    previous_chain_epoch: Option<ChainEpoch>,
    chain_epoch: ChainEpoch,
    reorg_window_change: &'input ReorgWindowChange,
    /// Outpoints re-created by this commit, with their new creation height.
    protected_outpoints: &'input HashMap<TransparentOutPoint, BlockHeight>,
}

/// Builds the `Replace` repair for the transparent-output projection and the
/// address-output projection it derives.
///
/// Both projections key creation facts, so both repair from the same set of
/// reverted creation outpoints. The address row carries the creation height
/// in its key: a protected outpoint re-created at a different height still
/// needs its old address row deleted, while the outpoint-keyed
/// `transparent_output` row is simply overwritten by this commit's put.
fn build_reorg_window_projection_repairs(
    inner: &impl RocksChainStoreRead,
    inputs: TransparentOutputProjectionRepairInputs<'_>,
) -> Result<TransparentOutputProjectionRepairs, StoreError> {
    let ReorgWindowChange::Replace { from_height } = inputs.reorg_window_change else {
        return Ok(TransparentOutputProjectionRepairs {
            deletes: Vec::new(),
        });
    };

    let mut deletes = Vec::new();
    let previous_chain_epoch =
        inputs
            .previous_chain_epoch
            .ok_or(StoreError::InvalidChainEpochArtifacts {
                reason: "reorg replacement requires an existing previous chain epoch",
            })?;
    let reverted_outpoints =
        read_reverted_transparent_output_outpoints(inner, previous_chain_epoch, *from_height)?;
    let reverted_outputs = read_current_transparent_outputs_by_outpoints(
        inner,
        previous_chain_epoch,
        &reverted_outpoints,
    )?;
    for outpoint in reverted_outpoints {
        if let Some(reverted_output) = reverted_outputs.get(&outpoint)
            && inputs.protected_outpoints.get(&outpoint) != Some(&reverted_output.block_height)
        {
            deletes.push(StorageDelete {
                table: StorageTable::AddressOutputIndex,
                key: StoreKey::address_output_index(
                    inputs.chain_epoch.network,
                    reverted_output.address_script_hash,
                    reverted_output.block_height,
                    outpoint,
                ),
            });
        }
        if inputs.protected_outpoints.contains_key(&outpoint) {
            continue;
        }

        deletes.push(StorageDelete {
            table: StorageTable::TransparentOutput,
            key: StoreKey::transparent_output(inputs.chain_epoch.network, outpoint),
        });
    }

    Ok(TransparentOutputProjectionRepairs { deletes })
}

struct TransparentSpendFactProjectionRepairs {
    deletes: Vec<StorageDelete>,
}

#[derive(Clone, Copy)]
struct TransparentSpendFactProjectionRepairInputs<'input> {
    previous_chain_epoch: Option<ChainEpoch>,
    chain_epoch: ChainEpoch,
    reorg_window_change: &'input ReorgWindowChange,
    protected_outpoints: &'input HashSet<TransparentOutPoint>,
}

fn build_reorg_window_spend_fact_projection_repairs(
    inner: &impl RocksChainStoreRead,
    inputs: TransparentSpendFactProjectionRepairInputs<'_>,
) -> Result<TransparentSpendFactProjectionRepairs, StoreError> {
    let ReorgWindowChange::Replace { from_height } = inputs.reorg_window_change else {
        return Ok(TransparentSpendFactProjectionRepairs {
            deletes: Vec::new(),
        });
    };

    let mut deletes = Vec::new();
    let previous_chain_epoch =
        inputs
            .previous_chain_epoch
            .ok_or(StoreError::InvalidChainEpochArtifacts {
                reason: "reorg replacement requires an existing previous chain epoch",
            })?;
    let reverted_outpoints =
        read_reverted_transparent_spend_fact_outpoints(inner, previous_chain_epoch, *from_height)?;
    for outpoint in reverted_outpoints {
        if inputs.protected_outpoints.contains(&outpoint) {
            continue;
        }

        let current_key = StoreKey::transparent_spend_fact(inputs.chain_epoch.network, outpoint);
        deletes.push(StorageDelete {
            table: StorageTable::TransparentSpendFact,
            key: current_key,
        });
    }

    Ok(TransparentSpendFactProjectionRepairs { deletes })
}

struct SafeTipRetentionSweep {
    puts: Vec<StoragePut>,
    deletes: Vec<StorageDelete>,
    swept_outpoints: u64,
}

impl SafeTipRetentionSweep {
    const fn empty() -> Self {
        Self {
            puts: Vec::new(),
            deletes: Vec::new(),
            swept_outpoints: 0,
        }
    }
}

/// Builds the safe-tip retention sweep for an `AdvanceSafeTipTo` commit.
///
/// A projection row may be physically deleted only when no commit the store
/// will ever accept can make it live again. `validate_reorg_window_change`
/// floors every `Replace` at `safe_tip + 1`, so a spend at or below
/// `safe_tip_height` is irreversible: the spent output's rows are deleted
/// from `address_output_index`, `transparent_output`, and
/// `transparent_spend_fact` in the same commit batch.
///
/// The sweep covers heights from the persisted swept-through marker up to
/// `min(new safe tip, previous tip)`. Spend facts committed by this same
/// batch are not durable yet when the sweep reads, so bulk catchup (which
/// advances the safe tip to the batch tip) sweeps each batch one commit
/// later. A non-monotonic `AdvanceSafeTipTo` target leaves the marker and
/// the projections untouched.
fn build_safe_tip_retention_sweep(
    inner: &impl RocksChainStoreRead,
    artifacts: &ChainEpochArtifacts,
    previous_chain_epoch: Option<ChainEpoch>,
) -> Result<SafeTipRetentionSweep, StoreError> {
    if !matches!(
        artifacts.reorg_window_change,
        ReorgWindowChange::AdvanceSafeTipTo { .. }
    ) {
        return Ok(SafeTipRetentionSweep::empty());
    }
    let chain_epoch = artifacts.chain_epoch;
    let Some(previous_chain_epoch) = previous_chain_epoch else {
        // Checkpoint bootstrap: nothing below the operator-supplied safe tip
        // is stored, so the marker starts at that boundary instead of
        // walking millions of empty heights on the first follow-up sweep.
        if artifacts.block_headers.is_empty() {
            return Ok(SafeTipRetentionSweep {
                puts: vec![transparent_retention_swept_height_put(
                    chain_epoch.safe_tip_height,
                )],
                deletes: Vec::new(),
                swept_outpoints: 0,
            });
        }
        return Ok(SafeTipRetentionSweep::empty());
    };

    let swept_through =
        read_transparent_retention_swept_height(inner)?.unwrap_or(BlockHeight::new(0));
    let sweep_ceiling = chain_epoch
        .safe_tip_height
        .min(previous_chain_epoch.tip_height);
    if sweep_ceiling <= swept_through {
        return Ok(SafeTipRetentionSweep::empty());
    }

    let mut deletes = Vec::new();
    let mut swept_outpoints = 0_u64;
    let sweep_range = BlockHeightRange::inclusive(
        BlockHeight::new(swept_through.value().saturating_add(1)),
        sweep_ceiling,
    );
    for height in sweep_range {
        let outpoints = read_visible_transparent_spend_fact_block_outpoints(
            inner,
            previous_chain_epoch,
            height,
        )?;
        if outpoints.is_empty() {
            continue;
        }
        let spends = read_current_transparent_spend_facts_by_outpoints(
            inner,
            previous_chain_epoch,
            &outpoints,
        )?;
        for outpoint in outpoints {
            let Some(spend) = spends.get(&outpoint) else {
                continue;
            };
            if spend.block_height != height {
                continue;
            }
            deletes.push(StorageDelete {
                table: StorageTable::AddressOutputIndex,
                key: StoreKey::address_output_index(
                    chain_epoch.network,
                    spend.spent_address_script_hash,
                    spend.spent_block_height,
                    outpoint,
                ),
            });
            deletes.push(StorageDelete {
                table: StorageTable::TransparentOutput,
                key: StoreKey::transparent_output(chain_epoch.network, outpoint),
            });
            deletes.push(StorageDelete {
                table: StorageTable::TransparentSpendFact,
                key: StoreKey::transparent_spend_fact(chain_epoch.network, outpoint),
            });
            swept_outpoints = swept_outpoints.saturating_add(1);
        }
    }

    Ok(SafeTipRetentionSweep {
        puts: vec![transparent_retention_swept_height_put(sweep_ceiling)],
        deletes,
        swept_outpoints,
    })
}

fn transparent_retention_swept_height_put(height: BlockHeight) -> StoragePut {
    StoragePut {
        table: StorageTable::StorageControl,
        key: StoreKey::transparent_retention_swept_height(),
        value: height.value().to_be_bytes().to_vec(),
    }
}

fn read_transparent_retention_swept_height(
    inner: &impl RocksChainStoreRead,
) -> Result<Option<BlockHeight>, StoreError> {
    let key = StoreKey::transparent_retention_swept_height();
    let Some(height_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(None);
    };

    let height_bytes =
        <[u8; 4]>::try_from(height_bytes.as_slice()).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentSpendFact,
            key: key.clone().into(),
            reason: "transparent retention swept height must be 4 bytes",
        })?;
    Ok(Some(BlockHeight::new(u32::from_be_bytes(height_bytes))))
}

fn record_safe_tip_retention_sweep(swept_outpoints: u64) {
    if swept_outpoints == 0 {
        return;
    }
    metrics::counter!("zinder_store_retention_swept_outpoints_total").increment(swept_outpoints);
}

fn address_output_row(artifact: &TransparentOutputArtifact) -> TransparentUnspentOutput {
    TransparentUnspentOutput::new(
        artifact.address_script_hash,
        artifact.script_pub_key.clone(),
        artifact.outpoint,
        artifact.value_zat,
        artifact.block_height,
        artifact.block_hash,
    )
}

fn read_reverted_transparent_spend_fact_outpoints(
    inner: &impl RocksChainStoreRead,
    previous_chain_epoch: ChainEpoch,
    from_height: BlockHeight,
) -> Result<Vec<TransparentOutPoint>, StoreError> {
    let mut outpoints = HashSet::new();
    for height in BlockHeightRange::inclusive(from_height, previous_chain_epoch.tip_height) {
        outpoints.extend(read_visible_transparent_spend_fact_block_outpoints(
            inner,
            previous_chain_epoch,
            height,
        )?);
    }

    let mut outpoints = outpoints.into_iter().collect::<Vec<_>>();
    sort_transparent_outpoints(&mut outpoints);
    Ok(outpoints)
}

fn read_reverted_transparent_output_outpoints(
    inner: &impl RocksChainStoreRead,
    previous_chain_epoch: ChainEpoch,
    from_height: BlockHeight,
) -> Result<Vec<TransparentOutPoint>, StoreError> {
    let mut outpoints = HashSet::new();
    for height in BlockHeightRange::inclusive(from_height, previous_chain_epoch.tip_height) {
        outpoints.extend(read_visible_transparent_output_block_outpoints(
            inner,
            previous_chain_epoch,
            height,
        )?);
    }

    let mut outpoints = outpoints.into_iter().collect::<Vec<_>>();
    sort_transparent_outpoints(&mut outpoints);
    Ok(outpoints)
}

fn read_visible_transparent_output_block_outpoints(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<Vec<TransparentOutPoint>, StoreError> {
    let Some(block) = read_block_header_artifact(inner, chain_epoch, height)? else {
        return Ok(Vec::new());
    };

    let prefix = StoreKey::transparent_output_block_index_prefix(chain_epoch.network, height);
    let mut outpoints = None;
    let mut scan_error = None;
    inner.scan_prefix_reverse(
        StorageTable::TransparentOutputBlockIndex,
        &prefix,
        &mut |key_bytes, envelope_bytes| {
            let key = StoreKey::from_raw_bytes(key_bytes);
            let Some(source_epoch) = StoreKey::transparent_artifact_chain_epoch_id(key_bytes)
            else {
                scan_error = Some(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::TransparentOutput,
                    key: key.into(),
                    reason: "transparent spend index key is malformed",
                });
                return Ok(PrefixScanControl::Stop);
            };
            if source_epoch > chain_epoch.id {
                return Ok(PrefixScanControl::Continue);
            }

            match decode_transparent_output_block_index(&key, envelope_bytes) {
                Ok((block_hash, block_outpoints)) if block_hash == block.block_hash => {
                    outpoints = Some(block_outpoints);
                    Ok(PrefixScanControl::Stop)
                }
                Ok(_) => Ok(PrefixScanControl::Continue),
                Err(error) => {
                    scan_error = Some(error);
                    Ok(PrefixScanControl::Stop)
                }
            }
        },
    )?;

    if let Some(error) = scan_error {
        return Err(error);
    }

    Ok(outpoints.unwrap_or_default())
}

fn sort_transparent_outpoints(outpoints: &mut [TransparentOutPoint]) {
    outpoints.sort_by(|left, right| {
        left.transaction_id
            .as_bytes()
            .cmp(&right.transaction_id.as_bytes())
            .then(left.output_index.cmp(&right.output_index))
    });
}

fn push_block_header_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    block_headers: Vec<BlockHeaderArtifact>,
) -> Result<(), StoreError> {
    for block in block_headers {
        let height = block.height;
        let block_hash = block.block_hash;
        let encoded_block = encode_block_header_artifact(&block)?;
        puts.push(StoragePut {
            table: StorageTable::BlockHeader,
            key: StoreKey::block_header(chain_epoch.network, chain_epoch.id, height),
            value: encoded_block,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_block_epoch(chain_epoch.network, height, chain_epoch.id),
            value: visibility_value(chain_epoch),
        });
        puts.push(block_hash_index_put(
            chain_epoch.network,
            chain_epoch.id,
            height,
            block_hash,
        ));
    }

    Ok(())
}

fn push_block_blob_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    block_blobs: Vec<BlockBlobArtifact>,
) -> Result<(), StoreError> {
    for block_blob in block_blobs {
        let height = block_blob.height;
        let encoded_blob = encode_block_blob_artifact(block_blob)?;
        puts.push(StoragePut {
            table: StorageTable::BlockBlob,
            key: StoreKey::block_blob(chain_epoch.network, chain_epoch.id, height),
            value: encoded_blob,
        });
    }

    Ok(())
}

fn push_compact_block_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    compact_blocks: Vec<CompactBlockArtifact>,
) -> Result<(), StoreError> {
    for compact_block in compact_blocks {
        let height = compact_block.height;
        let encoded_compact_block = encode_compact_block_artifact(compact_block)?;
        puts.push(StoragePut {
            table: StorageTable::CompactBlock,
            key: StoreKey::compact_block(chain_epoch.network, chain_epoch.id, height),
            value: encoded_compact_block,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_compact_block_epoch(chain_epoch.network, height, chain_epoch.id),
            value: visibility_value(chain_epoch),
        });
    }

    Ok(())
}

fn push_block_transaction_index_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    artifacts: Vec<BlockTransactionIndexArtifact>,
) -> Result<(), StoreError> {
    for artifact in artifacts {
        let block_height = artifact.block_height;
        let tx_index_in_block = artifact.tx_index_in_block;
        let encoded_artifact = encode_block_transaction_index_artifact(artifact)?;
        puts.push(StoragePut {
            table: StorageTable::BlockTransactionIndex,
            key: StoreKey::block_transaction_index(
                chain_epoch.network,
                chain_epoch.id,
                block_height,
                tx_index_in_block,
            ),
            value: encoded_artifact,
        });
    }

    Ok(())
}

fn push_transaction_location_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    locations: Vec<TransactionLocation>,
) -> Result<(), StoreError> {
    for location in locations {
        let transaction_id = location.transaction_id;
        let encoded_location = encode_transaction_location_artifact(location)?;
        puts.push(StoragePut {
            table: StorageTable::TransactionLocation,
            key: StoreKey::transaction_location(
                chain_epoch.network,
                chain_epoch.id,
                transaction_id,
            ),
            value: encoded_location,
        });
    }

    Ok(())
}

fn push_transaction_facts_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transaction_facts: Vec<TransactionFactsArtifact>,
) -> Result<(), StoreError> {
    for transaction in transaction_facts {
        let transaction_id = transaction.location.transaction_id;
        let encoded_transaction = encode_transaction_facts_artifact(transaction)?;
        puts.push(StoragePut {
            table: StorageTable::TransactionFacts,
            key: StoreKey::transaction_facts(chain_epoch.network, chain_epoch.id, transaction_id),
            value: encoded_transaction,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_transaction_epoch(
                chain_epoch.network,
                transaction_id,
                chain_epoch.id,
            ),
            value: visibility_value(chain_epoch),
        });
    }

    Ok(())
}

fn push_transaction_blob_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transaction_blobs: Vec<TransactionBlobArtifact>,
) -> Result<(), StoreError> {
    for transaction_blob in transaction_blobs {
        let transaction_id = transaction_blob.location.transaction_id;
        let encoded_blob = encode_transaction_blob_artifact(transaction_blob)?;
        puts.push(StoragePut {
            table: StorageTable::TransactionBlob,
            key: StoreKey::transaction_blob(chain_epoch.network, chain_epoch.id, transaction_id),
            value: encoded_blob,
        });
    }

    Ok(())
}

fn push_tree_state_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    tree_states: Vec<TreeStateArtifact>,
) -> Result<(), StoreError> {
    for tree_state in tree_states {
        let height = tree_state.height;
        let encoded_tree_state = encode_tree_state_artifact(tree_state)?;
        puts.push(StoragePut {
            table: StorageTable::TreeState,
            key: StoreKey::tree_state(chain_epoch.network, chain_epoch.id, height),
            value: encoded_tree_state,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_tree_state_epoch(chain_epoch.network, height, chain_epoch.id),
            value: visibility_value(chain_epoch),
        });
    }

    Ok(())
}

fn push_subtree_root_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    subtree_roots: Vec<SubtreeRootArtifact>,
) -> Result<(), StoreError> {
    for subtree_root in subtree_roots {
        let protocol = subtree_root.protocol;
        let subtree_index = subtree_root.subtree_index;
        let encoded_subtree_root = encode_subtree_root_artifact(&subtree_root)?;
        puts.push(StoragePut {
            table: StorageTable::SubtreeRoot,
            key: StoreKey::subtree_root(
                chain_epoch.network,
                chain_epoch.id,
                protocol,
                subtree_index,
            ),
            value: encoded_subtree_root,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_subtree_root_epoch(
                chain_epoch.network,
                protocol,
                subtree_index,
                chain_epoch.id,
            ),
            value: visibility_value(chain_epoch),
        });
    }

    Ok(())
}

fn push_transparent_output_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
) -> Result<(), StoreError> {
    let mut block_outpoints = HashMap::<(BlockHeight, BlockHash), Vec<TransparentOutPoint>>::new();
    for artifact in transparent_outputs_by_outpoint {
        let current_key = StoreKey::transparent_output(chain_epoch.network, artifact.outpoint);
        block_outpoints
            .entry((artifact.block_height, artifact.block_hash))
            .or_default()
            .push(artifact.outpoint);
        puts.push(StoragePut {
            table: StorageTable::AddressOutputIndex,
            key: StoreKey::address_output_index(
                chain_epoch.network,
                artifact.address_script_hash,
                artifact.block_height,
                artifact.outpoint,
            ),
            value: encode_address_output_index_artifact(address_output_row(&artifact))?,
        });
        let encoded = encode_transparent_output_artifact(artifact)?;
        puts.push(StoragePut {
            table: StorageTable::TransparentOutput,
            key: current_key,
            value: encoded,
        });
    }

    let mut block_outpoints = block_outpoints.into_iter().collect::<Vec<_>>();
    block_outpoints.sort_by(
        |((left_height, left_hash), _), ((right_height, right_hash), _)| {
            left_height
                .cmp(right_height)
                .then(left_hash.as_bytes().cmp(&right_hash.as_bytes()))
        },
    );
    for ((block_height, block_hash), mut outpoints) in block_outpoints {
        sort_transparent_outpoints(&mut outpoints);
        outpoints.dedup();
        puts.push(StoragePut {
            table: StorageTable::TransparentOutputBlockIndex,
            key: StoreKey::transparent_output_block_index(
                chain_epoch.network,
                block_height,
                chain_epoch.id,
            ),
            value: encode_transparent_output_block_index(block_hash, &outpoints)?,
        });
    }

    Ok(())
}

fn push_transparent_spend_fact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transparent_spend_facts: Vec<TransparentSpendFact>,
) -> Result<(), StoreError> {
    let mut block_spent_outpoints =
        HashMap::<(BlockHeight, BlockHash), Vec<TransparentOutPoint>>::new();
    for spend in transparent_spend_facts {
        let current_key =
            StoreKey::transparent_spend_fact(chain_epoch.network, spend.spent_outpoint);
        block_spent_outpoints
            .entry((spend.block_height, spend.block_hash))
            .or_default()
            .push(spend.spent_outpoint);
        let encoded_spend = encode_transparent_spend_fact(&spend)?;
        puts.push(StoragePut {
            table: StorageTable::TransparentSpendFact,
            key: current_key,
            value: encoded_spend,
        });
    }

    let mut block_spent_outpoints = block_spent_outpoints.into_iter().collect::<Vec<_>>();
    block_spent_outpoints.sort_by(
        |((left_height, left_hash), _), ((right_height, right_hash), _)| {
            left_height
                .cmp(right_height)
                .then(left_hash.as_bytes().cmp(&right_hash.as_bytes()))
        },
    );
    for ((block_height, block_hash), mut spent_outpoints) in block_spent_outpoints {
        sort_transparent_outpoints(&mut spent_outpoints);
        spent_outpoints.dedup();
        puts.push(StoragePut {
            table: StorageTable::TransparentSpendFactBlockIndex,
            key: StoreKey::transparent_spend_fact_block_index(
                chain_epoch.network,
                block_height,
                chain_epoch.id,
            ),
            value: encode_transparent_spend_fact_block_index(block_hash, &spent_outpoints)?,
        });
    }

    Ok(())
}

fn push_transparent_address_tx_index_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transparent_address_tx_index: Vec<TransparentAddressTxIndexArtifact>,
) -> Result<(), StoreError> {
    for artifact in transparent_address_tx_index {
        let key = StoreKey::transparent_address_tx_index(
            chain_epoch.network,
            artifact.address_script_hash,
            artifact.block_height,
            artifact.tx_index_in_block,
            chain_epoch.id,
        );
        let encoded = encode_transparent_address_tx_index_artifact(artifact)?;
        puts.push(StoragePut {
            table: StorageTable::TransparentAddressTxIndex,
            key,
            value: encoded,
        });
    }

    Ok(())
}

fn visibility_value(chain_epoch: ChainEpoch) -> Vec<u8> {
    chain_epoch.id.value().to_be_bytes().to_vec()
}

fn push_commit_control_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    event_envelope: &ChainEventEnvelope,
) {
    puts.push(StoragePut {
        table: StorageTable::StorageControl,
        key: StoreKey::visible_chain_epoch_pointer(),
        value: visibility_value(chain_epoch),
    });
    puts.push(StoragePut {
        table: StorageTable::ChainEvent,
        key: StoreKey::chain_event(event_envelope.event_sequence),
        value: encode_chain_event_envelope(event_envelope),
    });
    puts.push(StoragePut {
        table: StorageTable::StorageControl,
        key: StoreKey::chain_event_sequence_pointer(),
        value: event_envelope.event_sequence.to_be_bytes().to_vec(),
    });
    if event_envelope.event_sequence == 1 {
        puts.push(StoragePut {
            table: StorageTable::StorageControl,
            key: StoreKey::oldest_retained_chain_event_sequence(),
            value: event_envelope.event_sequence.to_be_bytes().to_vec(),
        });
    }
}

fn require_current_chain_epoch(inner: &impl RocksChainStoreRead) -> Result<ChainEpoch, StoreError> {
    read_current_chain_epoch_id(inner)?
        .map(|chain_epoch_id| read_chain_epoch(inner, chain_epoch_id))
        .transpose()?
        .ok_or(StoreError::NoVisibleChainEpoch)
}

fn read_current_chain_epoch_id(
    inner: &impl RocksChainStoreRead,
) -> Result<Option<ChainEpochId>, StoreError> {
    let key = StoreKey::visible_chain_epoch_pointer();
    let Some(chain_epoch_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(None);
    };

    if chain_epoch_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.into(),
            reason: "visible chain epoch pointer must be 8 bytes",
        });
    }

    let chain_epoch_bytes = <[u8; 8]>::try_from(chain_epoch_bytes.as_slice()).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.clone().into(),
            reason: "visible chain epoch pointer must be 8 bytes",
        }
    })?;
    Ok(Some(ChainEpochId::new(u64::from_be_bytes(
        chain_epoch_bytes,
    ))))
}

fn read_current_chain_event_sequence(inner: &impl RocksChainStoreRead) -> Result<u64, StoreError> {
    let key = StoreKey::chain_event_sequence_pointer();
    let Some(event_sequence_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(0);
    };

    if event_sequence_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEvent,
            key: key.into(),
            reason: "chain event sequence pointer must be 8 bytes",
        });
    }

    let event_sequence_bytes =
        <[u8; 8]>::try_from(event_sequence_bytes.as_slice()).map_err(|_| {
            StoreError::ArtifactCorrupt {
                family: ArtifactFamily::ChainEvent,
                key: key.clone().into(),
                reason: "chain event sequence pointer must be 8 bytes",
            }
        })?;
    Ok(u64::from_be_bytes(event_sequence_bytes))
}

fn read_oldest_retained_chain_event_sequence(
    inner: &impl RocksChainStoreRead,
    current_event_sequence: u64,
) -> Result<Option<u64>, StoreError> {
    if current_event_sequence == 0 {
        return Ok(None);
    }

    let key = StoreKey::oldest_retained_chain_event_sequence();
    let Some(event_sequence_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(Some(1));
    };

    if event_sequence_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEvent,
            key: key.into(),
            reason: "oldest retained chain event sequence must be 8 bytes",
        });
    }

    let event_sequence_bytes =
        <[u8; 8]>::try_from(event_sequence_bytes.as_slice()).map_err(|_| {
            StoreError::ArtifactCorrupt {
                family: ArtifactFamily::ChainEvent,
                key: key.clone().into(),
                reason: "oldest retained chain event sequence must be 8 bytes",
            }
        })?;
    let oldest_retained_sequence = u64::from_be_bytes(event_sequence_bytes);
    if oldest_retained_sequence == 0 || oldest_retained_sequence > current_event_sequence {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEvent,
            key: key.into(),
            reason: "oldest retained chain event sequence is outside retained history",
        });
    }

    Ok(Some(oldest_retained_sequence))
}

impl ChainEventRetentionReport {
    const fn empty() -> Self {
        Self {
            current_event_sequence: 0,
            oldest_retained_sequence: None,
            oldest_retained_created_at: None,
            retained_event_count: 0,
            pruned_event_count: 0,
        }
    }
}

fn build_chain_event_retention_report(
    inner: &impl RocksChainStoreRead,
    oldest_retained_sequence: u64,
    current_event_sequence: u64,
    cursor_auth_key: [u8; 32],
    pruned_event_count: u64,
) -> Result<ChainEventRetentionReport, StoreError> {
    let oldest_retained_created_at =
        read_chain_event_created_at(inner, oldest_retained_sequence, cursor_auth_key)?;
    let retained_event_count = current_event_sequence
        .saturating_sub(oldest_retained_sequence)
        .saturating_add(1);

    Ok(ChainEventRetentionReport {
        current_event_sequence,
        oldest_retained_sequence: Some(oldest_retained_sequence),
        oldest_retained_created_at: Some(oldest_retained_created_at),
        retained_event_count,
        pruned_event_count,
    })
}

fn read_chain_event_created_at(
    inner: &impl RocksChainStoreRead,
    event_sequence: u64,
    cursor_auth_key: [u8; 32],
) -> Result<UnixTimestampMillis, StoreError> {
    let key = StoreKey::chain_event(event_sequence);
    let Some(record_bytes) = inner.get(StorageTable::ChainEvent, &key)? else {
        return Err(StoreError::ChainEventCursorExpired {
            event_sequence,
            oldest_retained_sequence: event_sequence.saturating_add(1),
        });
    };
    let event_envelope = decode_chain_event_envelope(
        &key,
        &record_bytes,
        ChainEventStreamFamily::Tip,
        cursor_auth_key,
    )?;

    Ok(event_envelope.chain_epoch.created_at)
}

fn record_chain_event_prune_outcome(
    started_at: Instant,
    prune_outcome: &Result<ChainEventRetentionReport, StoreError>,
) {
    metrics::histogram!(
        "zinder_chain_event_pruning_duration_seconds",
        "status" => outcome_status(prune_outcome)
    )
    .record(started_at.elapsed());
    if let Ok(report) = prune_outcome {
        metrics::counter!("zinder_chain_event_pruned_total").increment(report.pruned_event_count);
    }
}

fn record_chain_event_retention_report(report: ChainEventRetentionReport) {
    metrics::gauge!("zinder_chain_event_retained").set(u64_to_f64(report.retained_event_count));
    metrics::gauge!("zinder_chain_event_retention_oldest_sequence")
        .set(u64_to_f64(report.oldest_retained_sequence.unwrap_or(0)));
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain-event retention values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

fn read_cursor_auth_key(inner: &impl RocksChainStoreRead) -> Result<[u8; 32], StoreError> {
    let key = StoreKey::cursor_auth_key();
    if let Some(cursor_auth_key_bytes) = inner.get(StorageTable::StorageControl, &key)? {
        let cursor_auth_key =
            <[u8; 32]>::try_from(cursor_auth_key_bytes.as_slice()).map_err(|_| {
                StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::ChainEvent,
                    key: key.clone().into(),
                    reason: "cursor authentication key must be 32 bytes",
                }
            })?;

        return Ok(cursor_auth_key);
    }

    Err(StoreError::ArtifactMissing {
        family: ArtifactFamily::ChainEvent,
        key: key.into(),
    })
}

fn ensure_cursor_auth_key(inner: &RocksChainStore) -> Result<[u8; 32], StoreError> {
    let key = StoreKey::cursor_auth_key();
    match read_cursor_auth_key(inner) {
        Ok(cursor_auth_key) => return Ok(cursor_auth_key),
        Err(StoreError::ArtifactMissing { .. }) => {}
        Err(error) => return Err(error),
    }

    let mut cursor_auth_key = [0; 32];
    getrandom::fill(&mut cursor_auth_key)
        .map_err(|source| StoreError::EntropyUnavailable { source })?;
    inner.write(vec![StoragePut {
        table: StorageTable::StorageControl,
        key,
        value: cursor_auth_key.to_vec(),
    }])?;

    Ok(cursor_auth_key)
}

fn read_chain_epoch(
    inner: &impl RocksChainStoreRead,
    chain_epoch: ChainEpochId,
) -> Result<ChainEpoch, StoreError> {
    let key = StoreKey::chain_epoch(chain_epoch);
    let Some(record_bytes) = inner.get(StorageTable::ChainEpoch, &key)? else {
        return Err(StoreError::ChainEpochMissing { chain_epoch });
    };

    decode_chain_epoch(&key, &record_bytes)
}

fn read_current_mempool_event_sequence(
    inner: &impl RocksChainStoreRead,
) -> Result<u64, StoreError> {
    let key = StoreKey::mempool_event_sequence_pointer();
    let Some(event_sequence_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(0);
    };
    if event_sequence_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.into(),
            reason: "mempool event sequence pointer must be 8 bytes",
        });
    }
    let event_sequence_bytes =
        <[u8; 8]>::try_from(event_sequence_bytes.as_slice()).map_err(|_| {
            StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "mempool event sequence pointer must be 8 bytes",
            }
        })?;
    Ok(u64::from_be_bytes(event_sequence_bytes))
}

fn read_oldest_retained_mempool_event_sequence(
    inner: &impl RocksChainStoreRead,
    current_event_sequence: u64,
) -> Result<Option<u64>, StoreError> {
    if current_event_sequence == 0 {
        return Ok(None);
    }
    let key = StoreKey::oldest_retained_mempool_event_sequence();
    let Some(event_sequence_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(Some(1));
    };
    if event_sequence_bytes.len() != 8 {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.into(),
            reason: "oldest retained mempool event sequence must be 8 bytes",
        });
    }
    let event_sequence_bytes =
        <[u8; 8]>::try_from(event_sequence_bytes.as_slice()).map_err(|_| {
            StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "oldest retained mempool event sequence must be 8 bytes",
            }
        })?;
    let oldest_retained_sequence = u64::from_be_bytes(event_sequence_bytes);
    if oldest_retained_sequence == 0 || oldest_retained_sequence > current_event_sequence {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.into(),
            reason: "oldest retained mempool event sequence is outside retained history",
        });
    }
    Ok(Some(oldest_retained_sequence))
}

fn read_mempool_event_observed_at(
    inner: &impl RocksChainStoreRead,
    event_sequence: u64,
) -> Result<Option<UnixTimestampMillis>, StoreError> {
    let key = StoreKey::mempool_event(event_sequence);
    let Some(record_bytes) = inner.get(StorageTable::MempoolEvent, &key)? else {
        return Ok(None);
    };
    Ok(Some(decode_mempool_event_observed_at(&key, &record_bytes)?))
}

struct MempoolPruneScan {
    deletes: Vec<StorageDelete>,
    pruned_added: u64,
    pruned_mined: u64,
    pruned_invalidated: u64,
    pruned_suppressed: u64,
    new_oldest_retained: Option<u64>,
}

fn scan_mempool_events_for_pruning(
    inner: &impl RocksChainStoreRead,
    now: UnixTimestampMillis,
    retention: MempoolEventRetentionConfig,
    oldest_retained_sequence: u64,
    current_event_sequence: u64,
) -> Result<MempoolPruneScan, StoreError> {
    let mut deletes: Vec<StorageDelete> = Vec::new();
    let mut pruned_added = 0_u64;
    let mut pruned_mined = 0_u64;
    let mut pruned_invalidated = 0_u64;
    let mut pruned_suppressed = 0_u64;
    let mut new_oldest_retained: Option<u64> = None;
    let mut iter_error: Option<StoreError> = None;

    inner.scan_forward(
        StorageTable::MempoolEvent,
        &StoreKey::mempool_event(oldest_retained_sequence),
        &mut |key_bytes, record_bytes| {
            let key = StoreKey::from_raw_bytes(key_bytes);
            let Some(event_sequence) = StoreKey::mempool_event_sequence_from_key(key_bytes) else {
                iter_error = Some(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::MempoolEvent,
                    key: key.into(),
                    reason: "mempool event key has malformed length",
                });
                return Ok(PrefixScanControl::Stop);
            };
            if event_sequence >= current_event_sequence {
                return Ok(PrefixScanControl::Stop);
            }

            let observed_at = match decode_mempool_event_observed_at(&key, record_bytes) {
                Ok(observed_at) => observed_at,
                Err(error) => {
                    iter_error = Some(error);
                    return Ok(PrefixScanControl::Stop);
                }
            };
            let kind = match decode_mempool_event_kind(&key, record_bytes) {
                Ok(kind) => kind,
                Err(error) => {
                    iter_error = Some(error);
                    return Ok(PrefixScanControl::Stop);
                }
            };
            let retention_window = match kind {
                MempoolEventKind::Added => retention.added_retention,
                MempoolEventKind::Mined => retention.mined_retention,
                MempoolEventKind::Invalidated | MempoolEventKind::Suppressed => {
                    retention.invalidated_retention
                }
            };
            let should_prune =
                retention_window.is_some_and(|window| age_exceeds_window(now, observed_at, window));
            if should_prune {
                deletes.push(StorageDelete {
                    table: StorageTable::MempoolEvent,
                    key: StoreKey::mempool_event(event_sequence),
                });
                match kind {
                    MempoolEventKind::Added => pruned_added = pruned_added.saturating_add(1),
                    MempoolEventKind::Mined => pruned_mined = pruned_mined.saturating_add(1),
                    MempoolEventKind::Invalidated => {
                        pruned_invalidated = pruned_invalidated.saturating_add(1);
                    }
                    MempoolEventKind::Suppressed => {
                        pruned_suppressed = pruned_suppressed.saturating_add(1);
                    }
                }
            } else if new_oldest_retained.is_none() {
                new_oldest_retained = Some(event_sequence);
            }
            Ok(PrefixScanControl::Continue)
        },
    )?;
    if let Some(error) = iter_error {
        return Err(error);
    }

    Ok(MempoolPruneScan {
        deletes,
        pruned_added,
        pruned_mined,
        pruned_invalidated,
        pruned_suppressed,
        new_oldest_retained,
    })
}

fn age_exceeds_window(
    now: UnixTimestampMillis,
    observed_at: UnixTimestampMillis,
    window: std::time::Duration,
) -> bool {
    let age_millis = now.value().saturating_sub(observed_at.value());
    let window_millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    age_millis > window_millis
}

fn record_mempool_event_prune_outcome(
    started_at: Instant,
    prune_outcome: &Result<MempoolEventRetentionReport, StoreError>,
) {
    metrics::histogram!(
        "zinder_mempool_event_pruning_duration_seconds",
        "status" => outcome_status(prune_outcome)
    )
    .record(started_at.elapsed());
    if let Ok(report) = prune_outcome {
        metrics::counter!(
            "zinder_mempool_events_pruned_total",
            "kind" => "added"
        )
        .increment(report.pruned_added_count);
        metrics::counter!(
            "zinder_mempool_events_pruned_total",
            "kind" => "mined"
        )
        .increment(report.pruned_mined_count);
        metrics::counter!(
            "zinder_mempool_events_pruned_total",
            "kind" => "invalidated"
        )
        .increment(report.pruned_invalidated_count);
    }
}

fn record_mempool_event_retention_report(report: MempoolEventRetentionReport) {
    metrics::gauge!("zinder_mempool_events_retained").set(u64_to_f64(report.retained_event_count));
    metrics::gauge!("zinder_mempool_event_retention_oldest_sequence")
        .set(u64_to_f64(report.oldest_retained_sequence.unwrap_or(0)));
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, ChainTipMetadata, CompactBlockArtifact,
        TreeStateArtifact, UnixTimestampMillis,
    };

    use super::*;

    #[test]
    fn current_artifact_schema_version_matches_supported_guard() {
        assert_eq!(CURRENT_ARTIFACT_SCHEMA_VERSION.value(), 11);
        assert_eq!(
            MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION,
            CURRENT_ARTIFACT_SCHEMA_VERSION.value()
        );
    }

    #[test]
    fn chain_event_history_reports_expired_cursor_when_retained_row_is_missing()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
        let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);

        let first_commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
            first_epoch,
            vec![first_block],
            vec![first_compact_block],
        ))?;
        store.commit_chain_epoch(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        ))?;

        store
            .store
            .inner
            .delete(StorageTable::ChainEvent, &StoreKey::chain_event(2))?;

        let error = match store.chain_event_history(ChainEventHistoryRequest::with_default_limit(
            Some(&first_commit.event_envelope.cursor),
        )) {
            Ok(event_history) => {
                return Err(format!("expected expired cursor, got {event_history:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StoreError::ChainEventCursorExpired {
                event_sequence: 1,
                oldest_retained_sequence: 2,
            }
        ));

        Ok(())
    }

    #[test]
    fn replacement_commit_retains_superseded_height_visibility_rows() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
        let (mut second_epoch, second_block, second_compact_block) =
            synthetic_epoch_with_hash_seed(2, 2, 2, 1);
        second_epoch.safe_tip_height = first_epoch.tip_height;
        second_epoch.safe_tip_hash = first_epoch.tip_hash;
        let second_tree_state = TreeStateArtifact::new(
            second_block.height,
            second_block.block_hash,
            b"tree-state-2".to_vec(),
        );
        let (mut replacement_epoch, replacement_block, replacement_compact_block) =
            synthetic_epoch_with_hash_seed(3, 2, 200, 1);
        replacement_epoch.safe_tip_height = first_epoch.tip_height;
        replacement_epoch.safe_tip_hash = first_epoch.tip_hash;
        let replacement_tree_state = TreeStateArtifact::new(
            replacement_block.height,
            replacement_block.block_hash,
            b"replacement-tree-state-2".to_vec(),
        );

        store.commit_chain_epoch(ChainEpochArtifacts::new(
            first_epoch,
            vec![first_block],
            vec![first_compact_block],
        ))?;
        store.commit_chain_epoch(
            ChainEpochArtifacts::new(second_epoch, vec![second_block], vec![second_compact_block])
                .with_tree_states(vec![second_tree_state]),
        )?;

        let stale_visibility_keys = height_visibility_keys(second_epoch, BlockHeight::new(2));
        assert_reorg_window_visibility(&store, &stale_visibility_keys, true)?;

        store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                replacement_epoch,
                vec![replacement_block],
                vec![replacement_compact_block],
            )
            .with_tree_states(vec![replacement_tree_state])
            .with_reorg_window_change(ReorgWindowChange::Replace {
                from_height: BlockHeight::new(2),
            }),
        )?;

        assert_reorg_window_visibility(&store, &stale_visibility_keys, true)?;

        Ok(())
    }

    fn height_visibility_keys(chain_epoch: ChainEpoch, height: BlockHeight) -> [StoreKey; 3] {
        [
            StoreKey::visible_block_epoch(chain_epoch.network, height, chain_epoch.id),
            StoreKey::visible_compact_block_epoch(chain_epoch.network, height, chain_epoch.id),
            StoreKey::visible_tree_state_epoch(chain_epoch.network, height, chain_epoch.id),
        ]
    }

    fn assert_reorg_window_visibility(
        store: &PrimaryChainStore,
        keys: &[StoreKey],
        expected_present: bool,
    ) -> Result<(), StoreError> {
        for key in keys {
            let present = store
                .store
                .inner
                .get(StorageTable::ReorgWindow, key)?
                .is_some();
            assert_eq!(present, expected_present);
        }

        Ok(())
    }

    #[test]
    fn open_refuses_persisted_store_with_unexpected_schema_version() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("schema-mismatch-store");
        {
            let store =
                PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_local_tests())?;
            let mut metadata_bytes = Vec::with_capacity(6);
            metadata_bytes.extend_from_slice(&u16::MAX.to_be_bytes());
            metadata_bytes.extend_from_slice(&Network::ZcashRegtest.id().to_be_bytes());
            store.store.inner.write(vec![StoragePut {
                table: StorageTable::StorageControl,
                key: StoreKey::store_metadata(),
                value: metadata_bytes,
            }])?;
        }

        let Err(error) =
            PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_local_tests())
        else {
            return Err("expected schema-mismatch rejection on reopen".into());
        };

        assert!(
            matches!(
                error,
                StoreError::SchemaMismatch {
                    persisted_version,
                    expected_version: STORE_SCHEMA_VERSION,
                } if persisted_version == u16::MAX
            ),
            "unexpected error: {error:?}"
        );

        Ok(())
    }

    fn synthetic_epoch(
        chain_epoch_id: u64,
        height: u32,
    ) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
        synthetic_epoch_with_hash_seed(chain_epoch_id, height, height, height.saturating_sub(1))
    }

    fn synthetic_epoch_with_hash_seed(
        chain_epoch_id: u64,
        height: u32,
        hash_seed: u32,
        parent_hash_seed: u32,
    ) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
        let source_hash = block_hash(hash_seed);
        let parent_hash = block_hash(parent_hash_seed);
        let block_height = BlockHeight::new(height);

        (
            ChainEpoch {
                id: ChainEpochId::new(chain_epoch_id),
                network: Network::ZcashRegtest,
                tip_height: block_height,
                tip_hash: source_hash,
                safe_tip_height: block_height,
                safe_tip_hash: source_hash,
                artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
                tip_metadata: ChainTipMetadata::empty(),
                created_at: UnixTimestampMillis::new(1_774_668_500_000 + u64::from(height)),
            },
            synthetic_block_header(
                block_height,
                source_hash,
                parent_hash,
                format!("raw-block-{chain_epoch_id}-{height}").as_bytes(),
            ),
            CompactBlockArtifact::new(
                block_height,
                source_hash,
                format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
            ),
        )
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }

    fn synthetic_block_header(
        height: BlockHeight,
        block_hash: BlockHash,
        parent_hash: BlockHash,
        raw_block_bytes: &[u8],
    ) -> BlockHeaderArtifact {
        BlockHeaderArtifact::new(
            height,
            block_hash,
            parent_hash,
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            u64::try_from(raw_block_bytes.len()).unwrap_or(u64::MAX),
        )
    }
}
