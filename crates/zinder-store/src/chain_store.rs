//! Chain store facade.

mod validation;

use std::{num::NonZeroU32, path::Path, sync::Arc, time::Instant};
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHeightRange, ChainEpoch, ChainEpochId,
    CompactBlockArtifact, Network, SubtreeRootArtifact, TransactionArtifact,
    TransparentAddressUtxoArtifact, TransparentUtxoSpendArtifact, TreeStateArtifact,
    UnixTimestampMillis,
};

use crate::{
    ArtifactFamily, ChainEpochArtifacts, ChainEpochCommitOutcome, ChainEpochCommitted,
    ChainEpochReader, ChainEvent, ChainEventEnvelope, ChainRangeReverted, ReorgWindowChange,
    StoreError, StreamCursorTokenV1,
    format::{
        ChainEventCursorAnchor, ChainEventStreamFamily, StoreKey, decode_chain_epoch,
        decode_chain_event_envelope, encode_block_artifact, encode_chain_epoch,
        encode_chain_event_envelope, encode_compact_block_artifact, encode_subtree_root_artifact,
        encode_transaction_artifact, encode_transparent_address_utxo_artifact,
        encode_transparent_utxo_spend_artifact, encode_tree_state_artifact,
    },
    kv::{RocksChainStore, RocksChainStoreRead, StorageDelete, StoragePut, StorageTable},
};

use validation::{
    committed_block_range, validate_chain_epoch_artifacts, validate_chain_store_options,
    validate_reorg_window_change, validate_visible_chain_commit,
};

/// Runtime options for [`PrimaryChainStore`] and [`SecondaryChainStore`].
///
/// Construct one with [`ChainStoreOptions::for_network`] for production use, or
/// [`ChainStoreOptions::for_local_tests`] for throwaway test stores. The struct
/// has no `Default` so callers must pick a posture explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainStoreOptions {
    /// Number of near-tip blocks that may be replaced by a reorg.
    pub reorg_window_blocks: u32,
    /// Whether each write batch asks `RocksDB` to fsync before returning.
    pub sync_writes: bool,
    /// Expected network for this store, used to persist and validate store metadata.
    pub network: Option<Network>,
}

impl ChainStoreOptions {
    /// Returns durable production options anchored to `network` with fsync writes.
    #[must_use]
    pub const fn for_network(network: Network) -> Self {
        Self {
            reorg_window_blocks: 100,
            sync_writes: true,
            network: Some(network),
        }
    }

    /// Returns options suitable for throwaway local test stores.
    ///
    /// Uses unsynchronized writes and a regtest network anchor. Production code
    /// must use [`Self::for_network`] instead.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            reorg_window_blocks: 100,
            sync_writes: false,
            network: Some(Network::ZcashRegtest),
        }
    }
}

const STORE_SCHEMA_VERSION: u16 = 1;
/// Durable artifact schema version written by this binary.
pub const CURRENT_ARTIFACT_SCHEMA_VERSION: ArtifactSchemaVersion = ArtifactSchemaVersion::new(2);
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
}

impl PrimaryChainStore {
    /// Opens or creates the canonical chain store as the primary writer.
    pub fn open(path: impl AsRef<Path>, options: ChainStoreOptions) -> Result<Self, StoreError> {
        validate_chain_store_options(options)?;
        let inner = Arc::new(RocksChainStore::open_primary(path, options.sync_writes)?);
        let store =
            ChainStoreInner::from_primary_inner(inner, options, ChainStoreReadPosture::Snapshot)?;

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

    /// Atomically commits artifacts for one chain epoch and advances the visible pointer.
    pub fn commit_chain_epoch(
        &self,
        artifacts: ChainEpochArtifacts,
    ) -> Result<ChainEpochCommitOutcome, StoreError> {
        self.store.commit_chain_epoch(artifacts)
    }

    /// Reads a bounded page of retained chain events strictly after the request cursor.
    pub fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.store.chain_event_history(request)
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

    /// Reads the current chain-event retention floor without pruning.
    pub fn chain_event_retention_report(&self) -> Result<ChainEventRetentionReport, StoreError> {
        self.store.chain_event_retention_report()
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
        Ok(ChainEpochReader::new(chain_epoch, read_view))
    }

    /// Opens a reader pinned to a specific chain epoch.
    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        let read_view = self.read_view();
        let chain_epoch = read_chain_epoch(&read_view, chain_epoch)?;
        Ok(ChainEpochReader::new(chain_epoch, read_view))
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
        let deletes = build_reorg_window_deletes(
            self.inner.as_ref(),
            current_chain_epoch,
            &reorg_window_change,
        );

        self.inner.write_batch(puts, deletes)?;

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
                return Err(StoreError::EventCursorInvalid {
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
                return Err(StoreError::EventCursorExpired {
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
            .map_err(|_| StoreError::EventCursorInvalid {
                reason: "cursor token failed validation",
            })?;

        if cursor_payload.event_sequence > bounds.current_event_sequence {
            return Err(StoreError::EventCursorInvalid {
                reason: "cursor sequence is ahead of retained history",
            });
        }

        if cursor_payload.event_sequence == 0 {
            return Err(StoreError::EventCursorInvalid {
                reason: "cursor sequence is before retained history",
            });
        }

        if cursor_payload.event_sequence < bounds.oldest_retained_sequence {
            return Err(StoreError::EventCursorExpired {
                event_sequence: cursor_payload.event_sequence,
                oldest_retained_sequence: bounds.oldest_retained_sequence,
            });
        }

        let cursor_event_key = StoreKey::chain_event(cursor_payload.event_sequence);
        let Some(cursor_event_bytes) = inner.get(StorageTable::ChainEvent, &cursor_event_key)?
        else {
            return Err(StoreError::EventCursorExpired {
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
            return Err(StoreError::EventCursorInvalid {
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
        let read_view = self.read_view();
        let current_event_sequence = read_current_chain_event_sequence(&read_view)?;
        let Some(oldest_retained_sequence) =
            read_oldest_retained_chain_event_sequence(&read_view, current_event_sequence)?
        else {
            return Ok(ChainEventRetentionReport::empty());
        };
        build_chain_event_retention_report(
            &read_view,
            oldest_retained_sequence,
            current_event_sequence,
            self.cursor_auth_key,
            0,
        )
    }

    fn chain_event_retention_report_locked(&self) -> Result<ChainEventRetentionReport, StoreError> {
        let current_event_sequence = read_current_chain_event_sequence(self.inner.as_ref())?;
        let Some(oldest_retained_sequence) =
            read_oldest_retained_chain_event_sequence(self.inner.as_ref(), current_event_sequence)?
        else {
            return Ok(ChainEventRetentionReport::empty());
        };
        build_chain_event_retention_report(
            self.inner.as_ref(),
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
    let store_metadata = decode_store_metadata(&key, &metadata_bytes)?;
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

    Ok(())
}

fn validate_store_metadata_for_commit(
    inner: &RocksChainStore,
    expected_network: Network,
) -> Result<Option<StoragePut>, StoreError> {
    let key = StoreKey::store_metadata();
    if let Some(metadata_bytes) = inner.get(StorageTable::StorageControl, &key)? {
        let store_metadata = decode_store_metadata(&key, &metadata_bytes)?;
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
    network: Network,
}

fn encode_store_metadata(network: Network) -> Vec<u8> {
    let mut metadata = Vec::with_capacity(6);
    metadata.extend_from_slice(&STORE_SCHEMA_VERSION.to_be_bytes());
    metadata.extend_from_slice(&network.id().to_be_bytes());
    metadata
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
    if schema_version != STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch {
            persisted_version: schema_version,
            expected_version: STORE_SCHEMA_VERSION,
        });
    }

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

    Ok(StoreMetadata { network })
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
        | ReorgWindowChange::FinalizeThrough { .. } => ChainEvent::ChainCommitted { committed },
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
        chain_epoch.finalized_height,
        event,
    ))
}

fn chain_event_matches_family(
    event_envelope: &ChainEventEnvelope,
    family: ChainEventStreamFamily,
) -> bool {
    match family {
        ChainEventStreamFamily::Tip => true,
        ChainEventStreamFamily::Finalized => {
            event_envelope.chain_epoch.tip_height <= event_envelope.finalized_height
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
        finalized_blocks,
        compact_blocks,
        transactions,
        tree_states,
        subtree_roots,
        transparent_address_utxos,
        transparent_utxo_spends,
        reorg_window_change: _,
    } = artifacts;

    let mut puts = Vec::new();
    puts.push(StoragePut {
        table: StorageTable::ChainEpoch,
        key: StoreKey::chain_epoch(chain_epoch.id),
        value: encode_chain_epoch(&chain_epoch),
    });
    push_block_artifact_puts(&mut puts, chain_epoch, finalized_blocks)?;
    push_compact_block_artifact_puts(&mut puts, chain_epoch, compact_blocks)?;
    push_transaction_artifact_puts(&mut puts, chain_epoch, transactions)?;
    push_tree_state_artifact_puts(&mut puts, chain_epoch, tree_states)?;
    push_subtree_root_artifact_puts(&mut puts, chain_epoch, subtree_roots)?;
    push_transparent_address_utxo_artifact_puts(&mut puts, chain_epoch, transparent_address_utxos)?;
    push_transparent_utxo_spend_artifact_puts(&mut puts, chain_epoch, transparent_utxo_spends)?;
    push_commit_control_puts(&mut puts, chain_epoch, event_envelope);

    Ok(puts)
}

fn build_reorg_window_deletes(
    _inner: &impl RocksChainStoreRead,
    _previous_chain_epoch: Option<ChainEpoch>,
    _reorg_window_change: &ReorgWindowChange,
) -> Vec<StorageDelete> {
    Vec::new()
}

fn push_block_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    finalized_blocks: Vec<BlockArtifact>,
) -> Result<(), StoreError> {
    for block in finalized_blocks {
        let height = block.height;
        let encoded_block = encode_block_artifact(block)?;
        puts.push(StoragePut {
            table: StorageTable::FinalizedBlock,
            key: StoreKey::finalized_block(chain_epoch.network, chain_epoch.id, height),
            value: encoded_block,
        });
        puts.push(StoragePut {
            table: StorageTable::ReorgWindow,
            key: StoreKey::visible_block_epoch(chain_epoch.network, height, chain_epoch.id),
            value: visibility_value(chain_epoch),
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

fn push_transaction_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transactions: Vec<TransactionArtifact>,
) -> Result<(), StoreError> {
    for transaction in transactions {
        let transaction_id = transaction.transaction_id;
        let encoded_transaction = encode_transaction_artifact(transaction)?;
        puts.push(StoragePut {
            table: StorageTable::Transaction,
            key: StoreKey::transaction(chain_epoch.network, chain_epoch.id, transaction_id),
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

fn push_transparent_address_utxo_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
) -> Result<(), StoreError> {
    for utxo in transparent_address_utxos {
        let key = StoreKey::transparent_address_utxo(
            chain_epoch.network,
            utxo.address_script_hash,
            utxo.block_height,
            utxo.outpoint,
            chain_epoch.id,
        );
        let encoded_utxo = encode_transparent_address_utxo_artifact(utxo)?;
        puts.push(StoragePut {
            table: StorageTable::TransparentAddressUtxo,
            key,
            value: encoded_utxo,
        });
    }

    Ok(())
}

fn push_transparent_utxo_spend_artifact_puts(
    puts: &mut Vec<StoragePut>,
    chain_epoch: ChainEpoch,
    transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
) -> Result<(), StoreError> {
    for spend in transparent_utxo_spends {
        let key = StoreKey::transparent_utxo_spend(
            chain_epoch.network,
            spend.spent_outpoint,
            chain_epoch.id,
        );
        let encoded_spend = encode_transparent_utxo_spend_artifact(spend)?;
        puts.push(StoragePut {
            table: StorageTable::TransparentUtxoSpend,
            key,
            value: encoded_spend,
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
        return Err(StoreError::EventCursorExpired {
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

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tempfile::tempdir;
    use zinder_core::{
        ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainTipMetadata,
        CompactBlockArtifact, TreeStateArtifact, UnixTimestampMillis,
    };

    use super::*;

    #[test]
    fn current_artifact_schema_version_matches_supported_guard() {
        assert_eq!(CURRENT_ARTIFACT_SCHEMA_VERSION.value(), 2);
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
            StoreError::EventCursorExpired {
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
        second_epoch.finalized_height = first_epoch.tip_height;
        second_epoch.finalized_hash = first_epoch.tip_hash;
        let second_tree_state = TreeStateArtifact::new(
            second_block.height,
            second_block.block_hash,
            b"tree-state-2".to_vec(),
        );
        let (mut replacement_epoch, replacement_block, replacement_compact_block) =
            synthetic_epoch_with_hash_seed(3, 2, 200, 1);
        replacement_epoch.finalized_height = first_epoch.tip_height;
        replacement_epoch.finalized_hash = first_epoch.tip_hash;
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
                    expected_version: 1,
                } if persisted_version == u16::MAX
            ),
            "unexpected error: {error:?}"
        );

        Ok(())
    }

    fn synthetic_epoch(
        chain_epoch_id: u64,
        height: u32,
    ) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
        synthetic_epoch_with_hash_seed(chain_epoch_id, height, height, height.saturating_sub(1))
    }

    fn synthetic_epoch_with_hash_seed(
        chain_epoch_id: u64,
        height: u32,
        hash_seed: u32,
        parent_hash_seed: u32,
    ) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
        let source_hash = block_hash(hash_seed);
        let parent_hash = block_hash(parent_hash_seed);
        let block_height = BlockHeight::new(height);

        (
            ChainEpoch {
                id: ChainEpochId::new(chain_epoch_id),
                network: Network::ZcashRegtest,
                tip_height: block_height,
                tip_hash: source_hash,
                finalized_height: block_height,
                finalized_hash: source_hash,
                artifact_schema_version: ArtifactSchemaVersion::new(1),
                tip_metadata: ChainTipMetadata::empty(),
                created_at: UnixTimestampMillis::new(1_774_668_500_000 + u64::from(height)),
            },
            BlockArtifact::new(
                block_height,
                source_hash,
                parent_hash,
                format!("raw-block-{chain_epoch_id}-{height}").into_bytes(),
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
}
