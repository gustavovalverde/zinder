//! Durable live mempool event lifecycle.
//!
//! The canonical primary retains the resumable event log but never stores the
//! mutable mempool index. The ingest owner appends an event here before it
//! publishes the matching in-memory index transition, so a writer restart can
//! always recover the event cursor history without reopening the canonical primary.

use std::collections::HashMap;

use rust_rocksdb::{DB, Direction, IteratorMode, WriteBatch, WriteOptions};
use zinder_core::{Network, TransactionId, UnixTimestampMillis};

use crate::{
    EventStreamStartPosition, MempoolEvent, MempoolEventEnvelope, MempoolEventHistoryRequest,
    MempoolEventPosition, MempoolEventRetentionConfig, MempoolEventRetentionReport,
    SnapshotPageCursorAnchor, StreamCursorTokenV1,
    format::{StoreKey, decode_mempool_event_envelope, encode_mempool_event_envelope},
};

use super::{
    CanonicalStoreError, RocksDbCanonicalStore, publication::column_family,
    rocksdb::MEMPOOL_EVENT_COLUMN_FAMILY,
};

/// Durable mempool-event sequence pointer in the canonical control family.
pub(super) const MEMPOOL_EVENT_SEQUENCE_KEY: &[u8] = b"mempool_event_sequence_v1";
/// Durable inclusive retention floor for the canonical mempool-event log.
pub(super) const MEMPOOL_EVENT_RETENTION_FLOOR_KEY: &[u8] = b"mempool_event_retention_floor_v1";

/// Validates the bounded durable mempool-log admission invariant.
///
/// The primary only needs to authenticate the floor and head rows while it
/// opens: the paged history read detects any interior gap before it could be
/// skipped. This keeps READY admission bounded even for a long retention
/// window while refusing metadata/column-family disagreement.
pub(super) fn validate_mempool_lifecycle_admission(
    db: &DB,
    network: Network,
    cursor_auth_key: [u8; 32],
) -> Result<(), CanonicalStoreError> {
    let head = read_admission_control_sequence(db, MEMPOOL_EVENT_SEQUENCE_KEY, "head")?;
    let floor = read_admission_control_sequence(db, MEMPOOL_EVENT_RETENTION_FLOOR_KEY, "floor")?;
    let event_family = column_family(db, MEMPOOL_EVENT_COLUMN_FAMILY)?;
    let first_row = db
        .iterator_cf(&event_family, IteratorMode::Start)
        .next()
        .transpose()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event admission first-row read",
            source,
        })?;
    let last_row = db
        .iterator_cf(&event_family, IteratorMode::End)
        .next()
        .transpose()
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event admission last-row read",
            source,
        })?;

    match (head, floor, first_row, last_row) {
        (None, None, None, None) => Ok(()),
        (Some(head), Some(floor), Some(first), Some(last)) => {
            if head == 0 || floor == 0 || floor > head {
                return Err(CanonicalStoreError::MempoolEventLogInvalid {
                    reason: "mempool event floor and head are outside retained history".to_owned(),
                });
            }
            validate_admission_event_row(network, cursor_auth_key, floor, &first)?;
            validate_admission_event_row(network, cursor_auth_key, head, &last)?;
            Ok(())
        }
        (None, None, Some(_), Some(_)) => Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event column family has orphan rows without lifecycle metadata"
                .to_owned(),
        }),
        _ => Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event lifecycle metadata and column family disagree".to_owned(),
        }),
    }
}

/// Decoded start of a paginated in-memory mempool snapshot.
///
/// The cursor is authenticated by the canonical primary and carries the
/// durable event position from which a consumer may subsequently resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalMempoolSnapshotStart {
    after_transaction_id: Option<TransactionId>,
    events_resume_anchor: Option<MempoolEventPosition>,
    events_resume_cursor: Option<StreamCursorTokenV1>,
}

impl CanonicalMempoolSnapshotStart {
    /// Returns the transaction-id paging position for the in-memory index.
    #[must_use]
    pub const fn after_transaction_id(&self) -> Option<TransactionId> {
        self.after_transaction_id
    }

    /// Returns the durable event position captured by this snapshot walk.
    #[must_use]
    pub const fn events_resume_anchor(&self) -> Option<MempoolEventPosition> {
        self.events_resume_anchor
    }

    /// Returns the signed event cursor matching the snapshot anchor.
    #[must_use]
    pub fn events_resume_cursor(&self) -> Option<&StreamCursorTokenV1> {
        self.events_resume_cursor.as_ref()
    }
}

impl RocksDbCanonicalStore {
    /// Appends one durable mempool transition before the ingest owner mutates
    /// its process-local live index.
    ///
    /// The event row, monotonic head pointer, and first retention floor are
    /// written in one synced batch. Callers must preflight the matching index
    /// mutation before invoking this method, then apply it with the returned
    /// position under their mutation gate.
    pub fn append_mempool_event(
        &self,
        event: MempoolEvent,
        source_observed_at: UnixTimestampMillis,
    ) -> Result<MempoolEventEnvelope, CanonicalStoreError> {
        validate_append_event(&event)?;
        let _lifecycle_guard = self.lifecycle_lock.lock();
        let current_event_sequence = current_mempool_event_sequence(self)?;
        if current_event_sequence == 0 {
            validate_mempool_lifecycle_admission(
                &self.bounded_open.db,
                self.network(),
                self.cursor_auth_key,
            )?;
        }
        let event_sequence = current_event_sequence
            .checked_add(1)
            .ok_or(CanonicalStoreError::MempoolEventSequenceOverflow)?;
        let cursor = mempool_event_cursor(self, event_sequence, event.transaction_id())?;
        let envelope = MempoolEventEnvelope {
            cursor,
            event_sequence,
            source_observed_unix_millis: source_observed_at.value(),
            event,
        };

        let event_family = column_family(&self.bounded_open.db, MEMPOOL_EVENT_COLUMN_FAMILY)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(
            &event_family,
            event_sequence.to_be_bytes(),
            encode_mempool_event_envelope(&envelope),
        );
        batch.put(MEMPOOL_EVENT_SEQUENCE_KEY, event_sequence.to_be_bytes());
        if current_event_sequence == 0 {
            batch.put(
                MEMPOOL_EVENT_RETENTION_FLOOR_KEY,
                event_sequence.to_be_bytes(),
            );
        }
        write_mempool_lifecycle_batch(self, &batch, "mempool event append")?;
        Ok(envelope)
    }

    /// Reads a bounded page of durable mempool events strictly after its
    /// authenticated cursor, or from the retained floor when absent.
    pub fn mempool_event_history(
        &self,
        request: MempoolEventHistoryRequest<'_>,
    ) -> Result<Vec<MempoolEventEnvelope>, CanonicalStoreError> {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        let current_event_sequence = current_mempool_event_sequence(self)?;
        if current_event_sequence == 0 {
            if request.from_cursor.is_some() {
                return Err(CanonicalStoreError::MempoolEventCursorInvalid {
                    reason: "cursor sequence is ahead of retained history",
                });
            }
            return Ok(Vec::new());
        }
        let oldest_retained_sequence = mempool_event_retention_floor(self, current_event_sequence)?;
        let start_sequence = match request.from_cursor {
            None => oldest_retained_sequence,
            Some(cursor) => validate_mempool_resume_cursor(
                self,
                cursor,
                current_event_sequence,
                oldest_retained_sequence,
            )?,
        };
        if start_sequence > current_event_sequence {
            return Ok(Vec::new());
        }

        let max_events = usize::try_from(request.max_events.get()).map_err(|_| {
            CanonicalStoreError::MempoolEventLogInvalid {
                reason: "mempool event page length cannot fit this platform".to_owned(),
            }
        })?;
        let event_family = column_family(&self.bounded_open.db, MEMPOOL_EVENT_COLUMN_FAMILY)?;
        let mut events = Vec::with_capacity(max_events);
        let iterator = self.bounded_open.db.iterator_cf(
            &event_family,
            IteratorMode::From(&start_sequence.to_be_bytes(), Direction::Forward),
        );
        for row in iterator {
            let (key, encoded_event) =
                row.map_err(|source| CanonicalStoreError::RocksDbOperation {
                    operation: "mempool event history scan",
                    source,
                })?;
            let event_sequence = event_sequence_from_key(&key)?;
            if event_sequence > current_event_sequence || events.len() >= max_events {
                break;
            }
            if event_sequence < start_sequence {
                continue;
            }
            let emitted_event_count = u64::try_from(events.len()).map_err(|_| {
                CanonicalStoreError::MempoolEventLogInvalid {
                    reason: "mempool event page length cannot fit u64".to_owned(),
                }
            })?;
            let expected_sequence = start_sequence
                .checked_add(emitted_event_count)
                .ok_or(CanonicalStoreError::MempoolEventSequenceOverflow)?;
            if event_sequence != expected_sequence {
                return Err(CanonicalStoreError::MempoolEventCursorExpired {
                    event_sequence: event_sequence.saturating_sub(1),
                    oldest_retained_sequence: event_sequence,
                });
            }
            events.push(decode_mempool_event(self, event_sequence, &encoded_event)?);
        }
        if events.is_empty() && start_sequence <= current_event_sequence {
            return Err(CanonicalStoreError::MempoolEventLogInvalid {
                reason: "mempool event head points to an absent retained row".to_owned(),
            });
        }
        Ok(events)
    }

    /// Resolves a public mempool-event subscription start to the cursor that
    /// page reads must resume strictly after.
    pub fn resolve_mempool_event_stream_start(
        &self,
        start: &EventStreamStartPosition,
    ) -> Result<Option<StreamCursorTokenV1>, CanonicalStoreError> {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        let current_event_sequence = current_mempool_event_sequence(self)?;
        match start {
            EventStreamStartPosition::EarliestRetained => Ok(None),
            EventStreamStartPosition::AfterCursor(cursor) => {
                if current_event_sequence == 0 {
                    return Err(CanonicalStoreError::MempoolEventCursorInvalid {
                        reason: "cursor sequence is ahead of retained history",
                    });
                }
                let oldest_retained_sequence =
                    mempool_event_retention_floor(self, current_event_sequence)?;
                let _next = validate_mempool_resume_cursor(
                    self,
                    cursor,
                    current_event_sequence,
                    oldest_retained_sequence,
                )?;
                Ok(Some(cursor.clone()))
            }
            EventStreamStartPosition::LiveTail => {
                if current_event_sequence == 0 {
                    return Ok(None);
                }
                let envelope = read_mempool_event(self, current_event_sequence)?;
                Ok(Some(envelope.cursor))
            }
        }
    }

    /// Decodes a snapshot paging cursor or captures the current durable
    /// mempool-event head for a new snapshot walk.
    pub fn begin_mempool_snapshot(
        &self,
        cursor_bytes: &[u8],
    ) -> Result<CanonicalMempoolSnapshotStart, CanonicalStoreError> {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        let current_event_sequence = current_mempool_event_sequence(self)?;
        let current_anchor = if current_event_sequence == 0 {
            None
        } else {
            Some(read_mempool_event(self, current_event_sequence)?.position())
        };

        let (after_transaction_id, events_resume_anchor) = if cursor_bytes.is_empty() {
            (None, current_anchor)
        } else {
            let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes.to_vec());
            let payload = cursor
                .decode_snapshot_page(self.network(), self.cursor_auth_key)
                .map_err(|_| CanonicalStoreError::MempoolSnapshotCursorInvalid {
                    reason: "cursor token failed validation",
                })?;
            if payload
                .events_resume_anchor
                .is_some_and(|anchor| anchor.event_sequence > current_event_sequence)
            {
                return Err(CanonicalStoreError::MempoolSnapshotCursorExpired {
                    anchor_event_sequence: payload
                        .events_resume_anchor
                        .map_or(0, |anchor| anchor.event_sequence),
                    current_event_sequence,
                });
            }
            (
                Some(payload.after_transaction_id),
                payload.events_resume_anchor,
            )
        };
        let events_resume_cursor = events_resume_anchor
            .map(|anchor| mempool_event_cursor(self, anchor.event_sequence, anchor.transaction_id))
            .transpose()?;
        Ok(CanonicalMempoolSnapshotStart {
            after_transaction_id,
            events_resume_anchor,
            events_resume_cursor,
        })
    }

    /// Mints the authenticated next-page cursor for one existing snapshot
    /// walk. The caller supplies the same durable anchor returned by
    /// [`Self::begin_mempool_snapshot`].
    pub fn encode_mempool_snapshot_next_cursor(
        &self,
        events_resume_anchor: Option<MempoolEventPosition>,
        after_transaction_id: TransactionId,
    ) -> Result<StreamCursorTokenV1, CanonicalStoreError> {
        StreamCursorTokenV1::snapshot_page(
            SnapshotPageCursorAnchor {
                network: self.network(),
                events_resume_anchor,
                after_transaction_id,
            },
            self.cursor_auth_key,
        )
        .map_err(|_| CanonicalStoreError::MempoolSnapshotCursorInvalid {
            reason: "cursor authentication key could not initialize the MAC",
        })
    }

    /// Reads the durable mempool-event retention floor and per-variant counts.
    pub fn mempool_event_retention_report(
        &self,
    ) -> Result<MempoolEventRetentionReport, CanonicalStoreError> {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        mempool_event_retention_report_locked(self)
    }

    /// Prunes one contiguous expired prefix from the durable mempool-event
    /// log while always retaining the current head and every active entry's
    /// last `Added` event as replay anchors.
    pub fn prune_mempool_events_before(
        &self,
        now: UnixTimestampMillis,
        retention: MempoolEventRetentionConfig,
    ) -> Result<MempoolEventRetentionReport, CanonicalStoreError> {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        let current_event_sequence = current_mempool_event_sequence(self)?;
        if current_event_sequence == 0 || retention.is_unbounded() {
            return mempool_event_retention_report_locked(self);
        }
        let oldest_retained_sequence = mempool_event_retention_floor(self, current_event_sequence)?;
        let earliest_active_add_sequence = earliest_active_mempool_add_sequence(
            self,
            oldest_retained_sequence,
            current_event_sequence,
        )?;
        let mut new_floor = oldest_retained_sequence;
        let mut pruned_added_count = 0_u64;
        let mut pruned_mined_count = 0_u64;
        let mut pruned_invalidated_count = 0_u64;
        let mut batch = WriteBatch::default();
        let event_family = column_family(&self.bounded_open.db, MEMPOOL_EVENT_COLUMN_FAMILY)?;
        for event_sequence in oldest_retained_sequence..current_event_sequence {
            if earliest_active_add_sequence.is_some_and(|active| event_sequence >= active) {
                break;
            }
            let envelope = read_mempool_event(self, event_sequence)?;
            let Some(window) = retention_window_for(&envelope.event, retention)? else {
                break;
            };
            if !age_exceeds_window(now, envelope.source_observed_unix_millis, window) {
                break;
            }
            batch.delete_cf(&event_family, event_sequence.to_be_bytes());
            new_floor = event_sequence.saturating_add(1);
            increment_pruned_count(
                &envelope.event,
                &mut pruned_added_count,
                &mut pruned_mined_count,
                &mut pruned_invalidated_count,
            )?;
        }
        if new_floor != oldest_retained_sequence {
            batch.put(MEMPOOL_EVENT_RETENTION_FLOOR_KEY, new_floor.to_be_bytes());
            write_mempool_lifecycle_batch(self, &batch, "mempool event prune")?;
        }
        let mut report = mempool_event_retention_report_locked(self)?;
        report.pruned_added_count = pruned_added_count;
        report.pruned_mined_count = pruned_mined_count;
        report.pruned_invalidated_count = pruned_invalidated_count;
        Ok(report)
    }
}

/// Finds the oldest retained `Added` event required to reconstruct the
/// current process-local index after a restart.
///
/// The canonical store deliberately persists an event history rather than a
/// second durable mempool index. Prefix pruning therefore cannot cross an
/// active transaction's last add: doing so would make a later empty source
/// snapshot unable to publish the terminal transition that removes it.
#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is non-exhaustive; unrecognized variants must not silently change replay retention."
)]
fn earliest_active_mempool_add_sequence(
    store: &RocksDbCanonicalStore,
    oldest_retained_sequence: u64,
    current_event_sequence: u64,
) -> Result<Option<u64>, CanonicalStoreError> {
    let mut active_add_sequences = HashMap::<TransactionId, u64>::new();
    for event_sequence in oldest_retained_sequence..=current_event_sequence {
        let envelope = read_mempool_event(store, event_sequence)?;
        match envelope.event {
            MempoolEvent::Added { entry } => {
                active_add_sequences.insert(entry.transaction_id, event_sequence);
            }
            MempoolEvent::Invalidated { transaction_id, .. }
            | MempoolEvent::Mined { transaction_id, .. } => {
                active_add_sequences.remove(&transaction_id);
            }
            _ => {
                return Err(CanonicalStoreError::MempoolEventLogInvalid {
                    reason: "mempool event variant is unsupported for replay retention".to_owned(),
                });
            }
        }
    }
    Ok(active_add_sequences.into_values().min())
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is non-exhaustive outside zinder-store; unrecognized variants fail closed before durable append."
)]
fn validate_append_event(event: &MempoolEvent) -> Result<(), CanonicalStoreError> {
    match event {
        MempoolEvent::Added { .. }
        | MempoolEvent::Invalidated { .. }
        | MempoolEvent::Mined { .. } => Ok(()),
        _ => Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event variant is unsupported".to_owned(),
        }),
    }
}

fn current_mempool_event_sequence(
    store: &RocksDbCanonicalStore,
) -> Result<u64, CanonicalStoreError> {
    let sequence_pointer_bytes = store
        .bounded_open
        .db
        .get(MEMPOOL_EVENT_SEQUENCE_KEY)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event sequence read",
            source,
        })?;
    match sequence_pointer_bytes {
        None => Ok(0),
        Some(bytes) => {
            let sequence = decode_u64(&bytes, "mempool event sequence pointer")?;
            if sequence == 0 {
                return Err(CanonicalStoreError::MempoolEventLogInvalid {
                    reason: "mempool event sequence pointer must be absent or nonzero".to_owned(),
                });
            }
            Ok(sequence)
        }
    }
}

fn mempool_event_retention_floor(
    store: &RocksDbCanonicalStore,
    current_event_sequence: u64,
) -> Result<u64, CanonicalStoreError> {
    let retention_floor_bytes = store
        .bounded_open
        .db
        .get(MEMPOOL_EVENT_RETENTION_FLOOR_KEY)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event retention floor read",
            source,
        })?;
    let floor = retention_floor_bytes.map_or(Ok(1), |bytes| {
        decode_u64(&bytes, "mempool event retention floor")
    })?;
    if floor == 0 || floor > current_event_sequence {
        return Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event retention floor is outside retained history".to_owned(),
        });
    }
    Ok(floor)
}

fn validate_mempool_resume_cursor(
    store: &RocksDbCanonicalStore,
    cursor: &StreamCursorTokenV1,
    current_event_sequence: u64,
    oldest_retained_sequence: u64,
) -> Result<u64, CanonicalStoreError> {
    let payload = cursor
        .decode_mempool_event(store.network(), store.cursor_auth_key)
        .map_err(|_| CanonicalStoreError::MempoolEventCursorInvalid {
            reason: "cursor token failed validation",
        })?;
    if payload.event_sequence > current_event_sequence {
        return Err(CanonicalStoreError::MempoolEventCursorInvalid {
            reason: "cursor sequence is ahead of retained history",
        });
    }
    if payload.event_sequence < oldest_retained_sequence {
        return Err(CanonicalStoreError::MempoolEventCursorExpired {
            event_sequence: payload.event_sequence,
            oldest_retained_sequence,
        });
    }
    let envelope = read_mempool_event(store, payload.event_sequence)?;
    if envelope.transaction_id() != payload.last_transaction_id {
        return Err(CanonicalStoreError::MempoolEventCursorInvalid {
            reason: "cursor transaction id does not match the retained event",
        });
    }
    payload
        .event_sequence
        .checked_add(1)
        .ok_or(CanonicalStoreError::MempoolEventSequenceOverflow)
}

fn mempool_event_cursor(
    store: &RocksDbCanonicalStore,
    event_sequence: u64,
    transaction_id: TransactionId,
) -> Result<StreamCursorTokenV1, CanonicalStoreError> {
    StreamCursorTokenV1::mempool_event(
        store.network(),
        event_sequence,
        transaction_id,
        store.cursor_auth_key,
    )
    .map_err(|_| CanonicalStoreError::MempoolEventLogInvalid {
        reason: "cursor authentication key could not initialize the MAC".to_owned(),
    })
}

fn read_mempool_event(
    store: &RocksDbCanonicalStore,
    event_sequence: u64,
) -> Result<MempoolEventEnvelope, CanonicalStoreError> {
    let event_family = column_family(&store.bounded_open.db, MEMPOOL_EVENT_COLUMN_FAMILY)?;
    let encoded_event = store
        .bounded_open
        .db
        .get_cf(&event_family, event_sequence.to_be_bytes())
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event read",
            source,
        })?
        .ok_or_else(|| CanonicalStoreError::MempoolEventLogInvalid {
            reason: format!("mempool event {event_sequence} is absent"),
        })?;
    decode_mempool_event(store, event_sequence, &encoded_event)
}

fn decode_mempool_event(
    store: &RocksDbCanonicalStore,
    event_sequence: u64,
    encoded_event: &[u8],
) -> Result<MempoolEventEnvelope, CanonicalStoreError> {
    let envelope = decode_mempool_event_envelope(
        &StoreKey::mempool_event(event_sequence),
        encoded_event,
        store.network(),
        store.cursor_auth_key,
    )
    .map_err(|error| CanonicalStoreError::MempoolEventLogInvalid {
        reason: error.to_string(),
    })?;
    if envelope.event_sequence != event_sequence {
        return Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: format!(
                "mempool event key {event_sequence} does not match envelope sequence {}",
                envelope.event_sequence
            ),
        });
    }
    Ok(envelope)
}

fn event_sequence_from_key(key: &[u8]) -> Result<u64, CanonicalStoreError> {
    let key =
        <[u8; 8]>::try_from(key).map_err(|_| CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event key must be exactly 8 bytes".to_owned(),
        })?;
    Ok(u64::from_be_bytes(key))
}

fn decode_u64(bytes: &[u8], field: &'static str) -> Result<u64, CanonicalStoreError> {
    let bytes =
        <[u8; 8]>::try_from(bytes).map_err(|_| CanonicalStoreError::MempoolEventLogInvalid {
            reason: format!("{field} must be exactly 8 bytes"),
        })?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_admission_control_sequence(
    db: &DB,
    key: &[u8],
    field: &'static str,
) -> Result<Option<u64>, CanonicalStoreError> {
    db.get(key)
        .map_err(|source| CanonicalStoreError::RocksDbOperation {
            operation: "mempool event admission metadata read",
            source,
        })?
        .map(|bytes| decode_u64(&bytes, field))
        .transpose()
}

fn validate_admission_event_row(
    network: Network,
    cursor_auth_key: [u8; 32],
    expected_sequence: u64,
    row: &(Box<[u8]>, Box<[u8]>),
) -> Result<(), CanonicalStoreError> {
    let observed_sequence = event_sequence_from_key(&row.0)?;
    if observed_sequence != expected_sequence {
        return Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: format!(
                "mempool event physical key {observed_sequence} does not match admitted sequence {expected_sequence}"
            ),
        });
    }
    let envelope = decode_mempool_event_envelope(
        &StoreKey::mempool_event(observed_sequence),
        &row.1,
        network,
        cursor_auth_key,
    )
    .map_err(|error| CanonicalStoreError::MempoolEventLogInvalid {
        reason: error.to_string(),
    })?;
    if envelope.event_sequence != observed_sequence {
        return Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: format!(
                "mempool event physical key {observed_sequence} does not match envelope sequence {}",
                envelope.event_sequence
            ),
        });
    }
    Ok(())
}

fn mempool_event_retention_report_locked(
    store: &RocksDbCanonicalStore,
) -> Result<MempoolEventRetentionReport, CanonicalStoreError> {
    let current_event_sequence = current_mempool_event_sequence(store)?;
    if current_event_sequence == 0 {
        return Ok(MempoolEventRetentionReport::default());
    }
    let oldest_retained_sequence = mempool_event_retention_floor(store, current_event_sequence)?;
    let oldest = read_mempool_event(store, oldest_retained_sequence)?;
    Ok(MempoolEventRetentionReport {
        current_event_sequence,
        oldest_retained_sequence: Some(oldest_retained_sequence),
        oldest_retained_observed_at: Some(UnixTimestampMillis::new(
            oldest.source_observed_unix_millis,
        )),
        retained_event_count: current_event_sequence
            .saturating_sub(oldest_retained_sequence)
            .saturating_add(1),
        pruned_added_count: 0,
        pruned_mined_count: 0,
        pruned_invalidated_count: 0,
    })
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is non-exhaustive outside zinder-store; unrecognized variants do not receive an invented retention policy."
)]
fn retention_window_for(
    event: &MempoolEvent,
    retention: MempoolEventRetentionConfig,
) -> Result<Option<std::time::Duration>, CanonicalStoreError> {
    match event {
        MempoolEvent::Added { .. } => Ok(retention.added_retention),
        MempoolEvent::Mined { .. } => Ok(retention.mined_retention),
        MempoolEvent::Invalidated { .. } => Ok(retention.invalidated_retention),
        _ => Err(CanonicalStoreError::MempoolEventLogInvalid {
            reason: "mempool event variant is unsupported".to_owned(),
        }),
    }
}

fn age_exceeds_window(
    now: UnixTimestampMillis,
    observed_at_millis: u64,
    window: std::time::Duration,
) -> bool {
    let age_millis = now.value().saturating_sub(observed_at_millis);
    let window_millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    age_millis > window_millis
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is non-exhaustive outside zinder-store; unrecognized variants fail closed during retention."
)]
fn increment_pruned_count(
    event: &MempoolEvent,
    added: &mut u64,
    mined: &mut u64,
    invalidated: &mut u64,
) -> Result<(), CanonicalStoreError> {
    match event {
        MempoolEvent::Added { .. } => *added = added.saturating_add(1),
        MempoolEvent::Mined { .. } => *mined = mined.saturating_add(1),
        MempoolEvent::Invalidated { .. } => *invalidated = invalidated.saturating_add(1),
        _ => {
            return Err(CanonicalStoreError::MempoolEventLogInvalid {
                reason: "mempool event variant is unsupported".to_owned(),
            });
        }
    }
    Ok(())
}

fn write_mempool_lifecycle_batch(
    store: &RocksDbCanonicalStore,
    batch: &WriteBatch,
    _operation: &'static str,
) -> Result<(), CanonicalStoreError> {
    let mut options = WriteOptions::default();
    options.disable_wal(false);
    options.set_sync(true);
    store
        .bounded_open
        .db
        .write_opt(batch, &options)
        .map_err(
            |source| CanonicalStoreError::LiveCommitWriteOutcomeUnknown {
                path: store.bounded_open.db.path().to_path_buf(),
                source,
            },
        )
}
