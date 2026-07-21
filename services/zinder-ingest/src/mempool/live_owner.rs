//! Live mempool ownership backed by the canonical event log.
//!
//! The mutable [`MempoolIndex`] remains process-local for low-latency wallet
//! overlays. Its resumable history, cursor head, and retention floor live in
//! the follower-owned canonical primary and are reached only through the
//! canonical control channel.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use parking_lot::{Mutex as ParkingMutex, RwLock};
use tokio::sync::Mutex;
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use zinder_core::{
    BlockId, ChainEpoch, MempoolEntry, MempoolEvictionReason, TransactionId,
    TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, TransparentOutputEntry, UnixTimestampMillis,
};
use zinder_proto::v1::wallet;
use zinder_source::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceEvent, SourceError,
};
use zinder_store::{
    EventStreamStartPosition, MempoolEvent, MempoolEventPosition, MempoolEventRetentionConfig,
    MempoolEventRetentionReport, StreamCursorTokenV1, mempool_event_envelope_message,
};

use crate::writer::control::CanonicalControlHandle;

use super::{
    MempoolApplyOutcome, MempoolEntryBuildError, MempoolIndex, MempoolIndexPreflight,
    MempoolReadySignal, build_mempool_entry,
};

const MEMPOOL_EVENT_PAGE_SIZE: u32 = 64;
const MEMPOOL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS: usize = 256;

/// One source generation buffered until its snapshot-complete marker.
///
/// Staging keeps a reconnect's partial observations out of the durable event
/// log. At completion, the owner reconciles the staged set against the last
/// durable/live set while reads remain unavailable.
#[derive(Debug)]
struct StagedMempoolGeneration {
    index: MempoolIndex,
    terminal_events: HashMap<TransactionId, (MempoolEvent, UnixTimestampMillis)>,
    next_synthetic_sequence: u64,
}

impl StagedMempoolGeneration {
    fn new() -> Self {
        Self {
            index: MempoolIndex::new(),
            terminal_events: HashMap::new(),
            next_synthetic_sequence: 0,
        }
    }

    fn next_position(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<MempoolEventPosition, Status> {
        self.next_synthetic_sequence =
            self.next_synthetic_sequence.checked_add(1).ok_or_else(|| {
                Status::resource_exhausted("staged mempool event sequence is exhausted")
            })?;
        Ok(MempoolEventPosition {
            event_sequence: self.next_synthetic_sequence,
            transaction_id,
        })
    }
}

/// Snapshot page read under the same gate as durable event append and index mutation.
#[derive(Clone, Debug)]
pub(crate) struct LiveMempoolSnapshotPage {
    pub(crate) source_tip: BlockId,
    pub(crate) entries: Vec<Arc<MempoolEntry>>,
    pub(crate) events_resume_cursor: Vec<u8>,
    pub(crate) snapshot_age_millis: u64,
    pub(crate) next_cursor: Vec<u8>,
}

/// Live mempool ownership state visible to the private control surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MempoolOwnerStatus {
    /// A source generation is building an in-memory snapshot that must not be
    /// exposed until its completion marker arrives.
    Hydrating,
    /// The durable log and process-local index have a verified transition path.
    Serving { source_tip: BlockId },
    /// A defensive post-append divergence was detected and the index must be reseeded.
    RebuildRequired,
}

/// Process-local live index with a serialized durable-log handoff.
#[derive(Clone)]
pub struct LiveMempoolOwner {
    index: MempoolIndex,
    /// Serializes preflight, durable append, index mutation, event-page reads,
    /// and snapshot anchoring. No public response can observe an event between
    /// durable append and its verified in-memory mutation.
    mutation_gate: Arc<Mutex<()>>,
    status: Arc<RwLock<MempoolOwnerStatus>>,
    staged_generation: Arc<ParkingMutex<Option<StagedMempoolGeneration>>>,
}

impl LiveMempoolOwner {
    /// Creates an empty live index backed by the configured canonical writer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: MempoolIndex::new(),
            mutation_gate: Arc::new(Mutex::new(())),
            status: Arc::new(RwLock::new(MempoolOwnerStatus::Hydrating)),
            staged_generation: Arc::new(ParkingMutex::new(Some(StagedMempoolGeneration::new()))),
        }
    }

    /// Returns a durable/index consistency error while a source reseed is required.
    #[cfg(test)]
    pub(crate) fn require_serving(&self) -> Result<(), Status> {
        let status = *self.status.read();
        match status {
            MempoolOwnerStatus::Serving { .. } => Ok(()),
            MempoolOwnerStatus::Hydrating => Err(Status::unavailable(
                "live mempool index is hydrating from an upstream snapshot",
            )),
            MempoolOwnerStatus::RebuildRequired => Err(Status::unavailable(
                "live mempool index is rebuilding after a durable transition mismatch",
            )),
        }
    }

    fn coherent_source_tip_for(&self, chain_epoch: ChainEpoch) -> Result<BlockId, Status> {
        let status = *self.status.read();
        match status {
            MempoolOwnerStatus::Serving { source_tip } => {
                let visible_tip =
                    BlockId::new(chain_epoch.visible_tip_height, chain_epoch.visible_tip_hash);
                if source_tip != visible_tip {
                    return Err(Status::unavailable(
                        "live mempool index is not coherent with the requested chain epoch",
                    ));
                }
                Ok(source_tip)
            }
            MempoolOwnerStatus::Hydrating => Err(Status::unavailable(
                "live mempool index is hydrating from an upstream snapshot",
            )),
            MempoolOwnerStatus::RebuildRequired => Err(Status::unavailable(
                "live mempool index is rebuilding after a durable transition mismatch",
            )),
        }
    }

    /// Returns the live entry for a transaction when it is currently visible.
    pub(crate) async fn entry_for(
        &self,
        chain_epoch: ChainEpoch,
        transaction_id: TransactionId,
    ) -> Result<Option<Arc<MempoolEntry>>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.coherent_source_tip_for(chain_epoch)?;
        Ok(self.index.entry_for(transaction_id))
    }

    /// Returns live transparent outputs for one script-hash selector.
    pub(crate) async fn transparent_outputs_by_address(
        &self,
        chain_epoch: ChainEpoch,
        address_script_hash: TransparentAddressScriptHash,
        max_entries: u32,
    ) -> Result<Vec<TransparentMempoolOutput>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.coherent_source_tip_for(chain_epoch)?;
        Ok(self
            .index
            .transparent_outputs_by_address(address_script_hash, max_entries))
    }

    /// Returns live mempool spends for requested transparent outpoints.
    pub(crate) async fn transparent_spends_by_outpoint(
        &self,
        chain_epoch: ChainEpoch,
        outpoints: impl IntoIterator<Item = TransparentOutPoint>,
    ) -> Result<Vec<TransparentMempoolSpend>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.coherent_source_tip_for(chain_epoch)?;
        Ok(outpoints
            .into_iter()
            .filter_map(|outpoint| self.index.transparent_spend_by_outpoint(outpoint))
            .collect())
    }

    /// Resolves live mempool-created outputs for requested outpoints.
    pub(crate) async fn transparent_outputs_by_outpoints(
        &self,
        chain_epoch: ChainEpoch,
        outpoints: &[TransparentOutPoint],
    ) -> Result<Vec<TransparentOutputEntry>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.coherent_source_tip_for(chain_epoch)?;
        Ok(self.index.transparent_outputs_by_outpoints(outpoints))
    }

    /// Reads one in-memory snapshot page with a durable events-resume anchor.
    pub(crate) async fn snapshot_page(
        &self,
        canonical: &CanonicalControlHandle,
        chain_epoch: ChainEpoch,
        max_entries: u32,
        from_cursor: Vec<u8>,
    ) -> Result<LiveMempoolSnapshotPage, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        let source_tip = self.coherent_source_tip_for(chain_epoch)?;
        let start = canonical.begin_mempool_snapshot(from_cursor).await?;
        let snapshot = self
            .index
            .snapshot_page(max_entries, start.after_transaction_id());
        let next_cursor = match snapshot.next_after_transaction_id {
            Some(after_transaction_id) => canonical
                .encode_mempool_snapshot_next_cursor(
                    start.events_resume_anchor(),
                    after_transaction_id,
                )
                .await?
                .as_bytes()
                .to_vec(),
            None => Vec::new(),
        };
        let snapshot_age_millis = UnixTimestampMillis::now()
            .value()
            .saturating_sub(snapshot.last_updated_at.value());
        Ok(LiveMempoolSnapshotPage {
            source_tip,
            entries: snapshot.entries,
            events_resume_cursor: start
                .events_resume_cursor()
                .map_or_else(Vec::new, |cursor| cursor.as_bytes().to_vec()),
            snapshot_age_millis,
            next_cursor,
        })
    }

    /// Resolves a durable mempool stream start while excluding a concurrent index transition.
    pub(crate) async fn resolve_event_start(
        &self,
        canonical: &CanonicalControlHandle,
        start: EventStreamStartPosition,
    ) -> Result<Option<StreamCursorTokenV1>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        canonical.resolve_mempool_event_start(start).await
    }

    /// Reads one durable mempool event page and encodes it for the public wire.
    pub(crate) async fn event_page(
        &self,
        canonical: &CanonicalControlHandle,
        after_cursor: Option<StreamCursorTokenV1>,
    ) -> Result<Vec<wallet::MempoolEventEnvelope>, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        canonical
            .mempool_event_page(
                after_cursor.map(|cursor| cursor.as_bytes().to_vec()),
                std::num::NonZeroU32::new(MEMPOOL_EVENT_PAGE_SIZE)
                    .ok_or_else(|| Status::internal("mempool event page size must be nonzero"))?,
            )
            .await?
            .iter()
            .map(|envelope| {
                mempool_event_envelope_message(envelope).map_err(|_| {
                    Status::failed_precondition("durable mempool event cannot be encoded")
                })
            })
            .collect()
    }

    /// Stages one source transition during hydration or applies it durably
    /// while the reconciled generation is serving.
    ///
    /// A generation never publishes partial snapshot observations to the
    /// durable log. Its marker first reconciles the staged state with the
    /// last durable/live state while reads remain unavailable.
    pub(crate) async fn apply_event(
        &self,
        canonical: &CanonicalControlHandle,
        event: MempoolEvent,
        observed_at: UnixTimestampMillis,
    ) -> Result<MempoolApplyOutcome, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        let state = *self.status.read();
        match state {
            MempoolOwnerStatus::Hydrating => self.stage_event_locked(event, observed_at),
            MempoolOwnerStatus::Serving { .. } => {
                self.append_and_apply_locked(canonical, event, observed_at)
                    .await
            }
            MempoolOwnerStatus::RebuildRequired => Err(Status::unavailable(
                "live mempool index is rebuilding after a durable transition mismatch",
            )),
        }
    }

    async fn append_and_apply_locked(
        &self,
        canonical: &CanonicalControlHandle,
        event: MempoolEvent,
        observed_at: UnixTimestampMillis,
    ) -> Result<MempoolApplyOutcome, Status> {
        let preflight = self.index.preflight_event(&event).map_err(|error| {
            self.mark_rebuild_required();
            Status::failed_precondition(format!(
                "mempool index preflight rejected a source transition: {error}"
            ))
        })?;
        if preflight == MempoolIndexPreflight::NoChange {
            return Ok(MempoolApplyOutcome::NoChange);
        }

        let envelope = canonical
            .append_mempool_event(event.clone(), observed_at)
            .await?;
        let outcome = apply_to_index(&self.index, event, envelope.position())?;
        if outcome != MempoolApplyOutcome::Applied {
            self.mark_rebuild_required();
            return Err(Status::unavailable(
                "durable mempool event could not be applied to the live index",
            ));
        }
        record_mempool_size_gauge(&self.index);
        Ok(outcome)
    }

    async fn append_and_apply_reconciliation_batch_locked(
        &self,
        canonical: &CanonicalControlHandle,
        transitions: Vec<(MempoolEvent, UnixTimestampMillis)>,
    ) -> Result<(), Status> {
        let mut applicable_transitions = Vec::with_capacity(transitions.len());
        for (event, observed_at) in transitions {
            let preflight = self.index.preflight_event(&event).map_err(|error| {
                self.mark_rebuild_required();
                Status::failed_precondition(format!(
                    "mempool index preflight rejected a reconciliation transition: {error}"
                ))
            })?;
            if preflight == MempoolIndexPreflight::Apply {
                applicable_transitions.push((event, observed_at));
            }
        }
        if applicable_transitions.is_empty() {
            return Ok(());
        }

        let expected_envelope_count = applicable_transitions.len();
        let envelopes = canonical
            .append_mempool_events(applicable_transitions)
            .await?;
        if envelopes.len() != expected_envelope_count {
            self.mark_rebuild_required();
            return Err(Status::unavailable(
                "durable mempool reconciliation returned an incomplete transition batch",
            ));
        }
        for envelope in envelopes {
            let position = envelope.position();
            let outcome =
                apply_to_index(&self.index, envelope.event, position).inspect_err(|_status| {
                    self.mark_rebuild_required();
                })?;
            if outcome != MempoolApplyOutcome::Applied {
                self.mark_rebuild_required();
                return Err(Status::unavailable(
                    "durable mempool reconciliation could not be applied to the live index",
                ));
            }
        }
        record_mempool_size_gauge(&self.index);
        Ok(())
    }

    fn stage_event_locked(
        &self,
        event: MempoolEvent,
        observed_at: UnixTimestampMillis,
    ) -> Result<MempoolApplyOutcome, Status> {
        let transaction_id = event.transaction_id();
        let mut staged_generation_guard = self.staged_generation.lock();
        let staged_generation = staged_generation_guard.as_mut().ok_or_else(|| {
            Status::failed_precondition("mempool source emitted a transition outside hydration")
        })?;

        match &event {
            MempoolEvent::Added { .. } => {
                staged_generation.terminal_events.remove(&transaction_id);
            }
            MempoolEvent::Invalidated { .. } | MempoolEvent::Mined { .. } => {
                staged_generation
                    .terminal_events
                    .insert(transaction_id, (event.clone(), observed_at));
            }
            _ => {
                self.mark_rebuild_required();
                return Err(Status::failed_precondition(
                    "mempool source event variant is unsupported during hydration",
                ));
            }
        }

        let preflight = staged_generation
            .index
            .preflight_event(&event)
            .map_err(|error| {
                self.mark_rebuild_required();
                Status::failed_precondition(format!(
                    "staged mempool index preflight rejected a source transition: {error}"
                ))
            })?;
        if preflight == MempoolIndexPreflight::NoChange {
            return Ok(MempoolApplyOutcome::NoChange);
        }
        let position = staged_generation.next_position(transaction_id)?;
        let outcome = apply_to_index(&staged_generation.index, event, position)?;
        if outcome != MempoolApplyOutcome::Applied {
            self.mark_rebuild_required();
            return Err(Status::unavailable(
                "staged mempool transition could not be applied to the live-index model",
            ));
        }
        drop(staged_generation_guard);
        Ok(outcome)
    }

    /// Prunes durable mempool history without opening a second primary.
    pub(crate) async fn prune_events(
        &self,
        canonical: &CanonicalControlHandle,
        now: UnixTimestampMillis,
        retention: MempoolEventRetentionConfig,
    ) -> Result<MempoolEventRetentionReport, Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        canonical.prune_mempool_events(now, retention).await
    }

    fn mark_rebuild_required(&self) {
        *self.status.write() = MempoolOwnerStatus::RebuildRequired;
    }

    async fn begin_hydration(&self) {
        let _mutation_guard = self.mutation_gate.lock().await;
        *self.staged_generation.lock() = Some(StagedMempoolGeneration::new());
        *self.status.write() = MempoolOwnerStatus::Hydrating;
    }

    pub(crate) async fn complete_hydration(
        &self,
        canonical: &CanonicalControlHandle,
        source_tip: BlockId,
    ) -> Result<(), Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        if !matches!(*self.status.read(), MempoolOwnerStatus::Hydrating) {
            return Err(Status::failed_precondition(
                "mempool source emitted an unexpected snapshot-complete marker",
            ));
        }
        let chain_epoch = canonical.chain_epoch().await?.chain_epoch;
        let visible_tip =
            BlockId::new(chain_epoch.visible_tip_height, chain_epoch.visible_tip_hash);
        if source_tip != visible_tip {
            self.mark_rebuild_required();
            return Err(Status::unavailable(
                "mempool snapshot source tip does not match the canonical visible tip",
            ));
        }
        let staged_generation = self.staged_generation.lock().take().ok_or_else(|| {
            Status::failed_precondition("mempool source completion marker has no staged snapshot")
        })?;
        self.reconcile_staged_generation_locked(canonical, staged_generation)
            .await?;
        *self.status.write() = MempoolOwnerStatus::Serving { source_tip };
        record_mempool_size_gauge(&self.index);
        Ok(())
    }

    async fn reconcile_staged_generation_locked(
        &self,
        canonical: &CanonicalControlHandle,
        mut staged_generation: StagedMempoolGeneration,
    ) -> Result<(), Status> {
        let current_entries = self.index.snapshot(u32::MAX);
        let staged_entries = staged_generation.index.snapshot(u32::MAX);
        let current_transaction_ids = current_entries
            .iter()
            .map(|entry| entry.transaction_id())
            .collect::<HashSet<_>>();
        let staged_transaction_ids = staged_entries
            .iter()
            .map(|entry| entry.transaction_id())
            .collect::<HashSet<_>>();

        let mut removals = Vec::with_capacity(MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS);
        for entry in current_entries {
            let transaction_id = entry.transaction_id();
            if staged_transaction_ids.contains(&transaction_id) {
                continue;
            }
            let (terminal_event, observed_at) = staged_generation
                .terminal_events
                .remove(&transaction_id)
                .unwrap_or_else(|| {
                    (
                        MempoolEvent::Invalidated {
                            transaction_id,
                            reason: MempoolEvictionReason::Unknown,
                        },
                        UnixTimestampMillis::now(),
                    )
                });
            removals.push((terminal_event, observed_at));
            if removals.len() == MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS {
                self.append_and_apply_reconciliation_batch_locked(
                    canonical,
                    std::mem::take(&mut removals),
                )
                .await?;
            }
        }
        self.append_and_apply_reconciliation_batch_locked(canonical, removals)
            .await?;

        let mut additions = Vec::with_capacity(MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS);
        for entry in staged_entries {
            if current_transaction_ids.contains(&entry.transaction_id()) {
                continue;
            }
            additions.push((
                MempoolEvent::Added {
                    entry: entry.as_ref().clone(),
                },
                entry.first_seen_unix_millis(),
            ));
            if additions.len() == MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS {
                self.append_and_apply_reconciliation_batch_locked(
                    canonical,
                    std::mem::take(&mut additions),
                )
                .await?;
            }
        }
        self.append_and_apply_reconciliation_batch_locked(canonical, additions)
            .await?;

        Ok(())
    }

    async fn restore_durable_index(
        &self,
        canonical: &CanonicalControlHandle,
    ) -> Result<(), Status> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.index.reset();
        *self.staged_generation.lock() = Some(StagedMempoolGeneration::new());
        *self.status.write() = MempoolOwnerStatus::Hydrating;
        let mut after_cursor = None;
        loop {
            let page = canonical
                .mempool_event_page(
                    after_cursor.clone(),
                    std::num::NonZeroU32::new(MEMPOOL_EVENT_PAGE_SIZE).ok_or_else(|| {
                        Status::internal("mempool event page size must be nonzero")
                    })?,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            for envelope in page {
                after_cursor = Some(envelope.cursor.as_bytes().to_vec());
                let event = envelope.event.clone();
                let preflight = self.index.preflight_event(&event).map_err(|error| {
                    self.mark_rebuild_required();
                    Status::failed_precondition(format!(
                        "durable mempool history cannot rebuild the live index: {error}"
                    ))
                })?;
                if preflight == MempoolIndexPreflight::NoChange {
                    continue;
                }
                let outcome = apply_to_index(&self.index, event, envelope.position())?;
                if outcome != MempoolApplyOutcome::Applied {
                    self.mark_rebuild_required();
                    return Err(Status::failed_precondition(
                        "durable mempool history could not rebuild the live index",
                    ));
                }
            }
        }
        record_mempool_size_gauge(&self.index);
        Ok(())
    }

    async fn withdraw_for_rebuild(&self) {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.staged_generation.lock().take();
        self.mark_rebuild_required();
    }

    #[cfg(test)]
    fn is_serving(&self) -> bool {
        matches!(*self.status.read(), MempoolOwnerStatus::Serving { .. })
    }

    #[cfg(test)]
    fn staged_entry_count(&self) -> usize {
        self.staged_generation
            .lock()
            .as_ref()
            .map_or(0, |generation| generation.index.entry_count())
    }
}

impl Default for LiveMempoolOwner {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs the one source-to-index owner until controlled shutdown.
///
/// A completed source generation is reconciled with the durable/live state
/// under the owner gate. Serving transitions append to the follower-owned
/// primary before applying the matching in-memory overlay; hydrating
/// transitions remain staged until their snapshot-complete marker. If the
/// impossible post-append mismatch is detected, the public index is withdrawn
/// and the source stream is reopened from a fresh snapshot.
pub async fn run_live_mempool_owner(
    source: Arc<dyn MempoolSource>,
    canonical: CanonicalControlHandle,
    owner: LiveMempoolOwner,
    mempool_ready_signal: MempoolReadySignal,
    cancel: CancellationToken,
) {
    if !restore_durable_mempool_index(&owner, &canonical, &mempool_ready_signal, &cancel).await {
        return;
    }
    loop {
        match run_source_generation(&source, &canonical, &owner, &mempool_ready_signal, &cancel)
            .await
        {
            MempoolOwnerLoopOutcome::Shutdown => return,
            MempoolOwnerLoopOutcome::Rehydrate => continue,
            MempoolOwnerLoopOutcome::Rebuild => {}
        }
        if !withdraw_and_wait_for_mempool_rebuild(&owner, &mempool_ready_signal, &cancel).await {
            return;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MempoolOwnerLoopOutcome {
    Shutdown,
    Rehydrate,
    Rebuild,
}

async fn restore_durable_mempool_index(
    owner: &LiveMempoolOwner,
    canonical: &CanonicalControlHandle,
    mempool_ready_signal: &MempoolReadySignal,
    cancel: &CancellationToken,
) -> bool {
    loop {
        match owner.restore_durable_index(canonical).await {
            Ok(()) => return true,
            Err(status) => {
                mempool_ready_signal.set_hydrating();
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "mempool_durable_replay_failed",
                    code = ?status.code(),
                    "live mempool owner could not reconstruct its durable live state"
                );
                if wait_or_cancel(cancel, MEMPOOL_RECONNECT_BACKOFF).await {
                    return false;
                }
            }
        }
    }
}

async fn run_source_generation(
    source: &Arc<dyn MempoolSource>,
    canonical: &CanonicalControlHandle,
    owner: &LiveMempoolOwner,
    mempool_ready_signal: &MempoolReadySignal,
    cancel: &CancellationToken,
) -> MempoolOwnerLoopOutcome {
    owner.begin_hydration().await;
    mempool_ready_signal.set_hydrating();
    if wait_for_canonical_epoch(canonical, cancel).await.is_none() {
        return MempoolOwnerLoopOutcome::Shutdown;
    }
    let event_stream = match source.events().await {
        Ok(event_stream) => {
            tracing::info!(
                target: "zinder::ingest",
                event = "mempool_source_opened",
                "live mempool source stream opened"
            );
            event_stream
        }
        Err(error) => {
            record_source_open_failure(&error);
            return if wait_or_cancel(cancel, MEMPOOL_RECONNECT_BACKOFF).await {
                MempoolOwnerLoopOutcome::Shutdown
            } else {
                MempoolOwnerLoopOutcome::Rehydrate
            };
        }
    };
    consume_source_events(event_stream, canonical, owner, mempool_ready_signal, cancel).await
}

async fn consume_source_events(
    mut event_stream: zinder_source::MempoolSourceEventStream,
    canonical: &CanonicalControlHandle,
    owner: &LiveMempoolOwner,
    mempool_ready_signal: &MempoolReadySignal,
    cancel: &CancellationToken,
) -> MempoolOwnerLoopOutcome {
    loop {
        let source_event = tokio::select! {
            () = cancel.cancelled() => return MempoolOwnerLoopOutcome::Shutdown,
            source_event = event_stream.next() => source_event,
        };
        let Some(source_event) = source_event else {
            tracing::warn!(
                target: "zinder::ingest",
                event = "mempool_source_closed",
                "live mempool source stream closed; reconnecting"
            );
            return MempoolOwnerLoopOutcome::Rebuild;
        };
        match source_event {
            Ok(MempoolSourceEvent::InitialSnapshotComplete { source_tip }) => {
                if let Err(status) = owner.complete_hydration(canonical, source_tip).await {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "mempool_snapshot_marker_rejected",
                        code = ?status.code(),
                        "live mempool source completion marker was rejected"
                    );
                    return MempoolOwnerLoopOutcome::Rebuild;
                }
                mempool_ready_signal.set_ready();
                tracing::info!(
                    target: "zinder::ingest",
                    event = "mempool_snapshot_complete",
                    "live mempool snapshot completed; live reads are available"
                );
            }
            Ok(source_event) => {
                if let Err(status) =
                    apply_source_event(owner, canonical, cancel, source_event).await
                {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "mempool_event_rejected",
                        code = ?status.code(),
                        "live mempool source event was not published"
                    );
                    return MempoolOwnerLoopOutcome::Rebuild;
                }
            }
            Err(error) => {
                metrics::counter!(
                    "zinder_mempool_source_errors_total",
                    "kind" => "stream_item"
                )
                .increment(1);
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "mempool_source_item_error",
                    error = %error,
                    "live mempool source emitted an error item"
                );
                return MempoolOwnerLoopOutcome::Rebuild;
            }
        }
    }
}

async fn withdraw_and_wait_for_mempool_rebuild(
    owner: &LiveMempoolOwner,
    mempool_ready_signal: &MempoolReadySignal,
    cancel: &CancellationToken,
) -> bool {
    owner.withdraw_for_rebuild().await;
    mempool_ready_signal.set_hydrating();
    tracing::error!(
        target: "zinder::ingest",
        event = "mempool_index_rebuild_required",
        "withdrew live mempool reads until the source reseeds the index"
    );
    !wait_or_cancel(cancel, MEMPOOL_RECONNECT_BACKOFF).await
}

/// Runs the durable mempool-event retention loop through the canonical writer.
pub async fn run_mempool_retention(
    canonical: CanonicalControlHandle,
    owner: LiveMempoolOwner,
    retention: MempoolEventRetentionConfig,
    check_interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(check_interval) => {
                if let Err(status) = owner.prune_events(&canonical, UnixTimestampMillis::now(), retention).await {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "mempool_event_retention_failed",
                        code = ?status.code(),
                        "durable mempool-event retention pass failed"
                    );
                }
            }
        }
    }
}

async fn apply_source_event(
    owner: &LiveMempoolOwner,
    canonical: &CanonicalControlHandle,
    cancel: &CancellationToken,
    source_event: MempoolSourceEvent,
) -> Result<(), Status> {
    let observed_at = match &source_event {
        MempoolSourceEvent::Added(entry) => entry.observed_at_unix_millis,
        MempoolSourceEvent::Invalidated { .. } | MempoolSourceEvent::Mined { .. } => {
            UnixTimestampMillis::now()
        }
        MempoolSourceEvent::InitialSnapshotComplete { .. } | _ => UnixTimestampMillis::now(),
    };
    let event = match source_event {
        MempoolSourceEvent::Added(source_entry) => {
            let chain_epoch = wait_for_canonical_epoch(canonical, cancel)
                .await
                .ok_or_else(|| Status::cancelled("mempool owner cancelled"))?;
            build_mempool_entry(source_entry, chain_epoch)
                .map(|entry| MempoolEvent::Added { entry })
                .map_err(|error| {
                    record_hydration_failure(hydration_failure_reason(&error));
                    Status::failed_precondition("mempool transaction hydration failed")
                })?
        }
        MempoolSourceEvent::Invalidated {
            transaction_id,
            reason,
        } => MempoolEvent::Invalidated {
            transaction_id,
            reason,
        },
        MempoolSourceEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        },
        MempoolSourceEvent::InitialSnapshotComplete { .. } | _ => {
            record_hydration_failure(MempoolHydrationFailureReason::UnknownSourceEventVariant);
            return Err(Status::failed_precondition(
                "mempool source event variant is unsupported",
            ));
        }
    };
    let _outcome = owner.apply_event(canonical, event, observed_at).await?;
    Ok(())
}

async fn wait_for_canonical_epoch(
    canonical: &CanonicalControlHandle,
    cancel: &CancellationToken,
) -> Option<ChainEpoch> {
    loop {
        let snapshot = tokio::select! {
            () = cancel.cancelled() => return None,
            snapshot = canonical.chain_epoch() => snapshot,
        };
        match snapshot {
            Ok(snapshot) => return Some(snapshot.chain_epoch),
            Err(status) => {
                tracing::debug!(
                    target: "zinder::ingest",
                    event = "mempool_waiting_for_canonical_writer",
                    code = ?status.code(),
                    "mempool owner is waiting for the canonical writer command channel"
                );
                if wait_or_cancel(cancel, MEMPOOL_RECONNECT_BACKOFF).await {
                    return None;
                }
            }
        }
    }
}

async fn wait_or_cancel(cancel: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is non-exhaustive; the index must fail closed rather than inventing a live transition."
)]
fn apply_to_index(
    index: &MempoolIndex,
    event: MempoolEvent,
    position: zinder_store::MempoolEventPosition,
) -> Result<MempoolApplyOutcome, Status> {
    match event {
        MempoolEvent::Added { entry } => Ok(index.apply_added(entry, position)),
        MempoolEvent::Invalidated { transaction_id, .. } => {
            Ok(index.apply_invalidated(transaction_id, position))
        }
        MempoolEvent::Mined { transaction_id, .. } => {
            Ok(index.apply_mined(transaction_id, position))
        }
        _ => Err(Status::failed_precondition(
            "mempool event variant is unsupported",
        )),
    }
}

fn hydration_failure_reason(error: &MempoolEntryBuildError) -> MempoolHydrationFailureReason {
    match error {
        MempoolEntryBuildError::TransactionParseFailed { .. } => {
            MempoolHydrationFailureReason::TransactionParseFailed
        }
        MempoolEntryBuildError::TransactionIdMismatch { .. } => {
            MempoolHydrationFailureReason::TransactionIdMismatch
        }
        MempoolEntryBuildError::AuthDigestMismatch { .. } => {
            MempoolHydrationFailureReason::AuthDigestMismatch
        }
        MempoolEntryBuildError::CompactTransactionBuildFailed { .. } => {
            MempoolHydrationFailureReason::CompactTransactionBuildFailed
        }
        MempoolEntryBuildError::TransparentOutputIndexOverflow => {
            MempoolHydrationFailureReason::TransparentOutputIndexOverflow
        }
    }
}

fn record_hydration_failure(reason: MempoolHydrationFailureReason) {
    metrics::counter!(
        "zinder_mempool_hydration_failures_total",
        "reason" => reason.as_label()
    )
    .increment(1);
}

fn record_source_open_failure(error: &SourceError) {
    metrics::counter!(
        "zinder_mempool_source_errors_total",
        "kind" => "stream_open"
    )
    .increment(1);
    tracing::warn!(
        target: "zinder::ingest",
        event = "mempool_source_open_failed",
        error = %error,
        "live mempool source could not open; reconnecting"
    );
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges accept f64 samples; live mempool size is diagnostic."
)]
fn record_mempool_size_gauge(index: &MempoolIndex) {
    metrics::gauge!("zinder_mempool_entries").set(index.entry_count() as f64);
}

#[cfg(test)]
mod tests {
    use std::{error::Error, num::NonZeroU32, sync::Arc, time::Duration};

    use tokio_util::sync::CancellationToken;
    use tonic::Code;
    use zebra_chain::{
        serialization::ZcashDeserializeInto as _, transaction::Transaction as ZebraTransaction,
    };
    use zinder_core::{AuthDigest, RawTransactionBytes, TransactionId, UnixTimestampMillis};
    use zinder_source::MempoolSourceEntry;
    use zinder_store::MempoolEvent;
    use zinder_testkit::{MockMempoolSource, MockMempoolSourceControl};

    use crate::{
        MempoolReadyGate, mempool_ready_channel,
        writer::control::{
            CanonicalControlCommand, CanonicalControlHandle, apply_canonical_control_command,
            canonical_control_channel, test_support::published_fixture_store,
        },
    };

    use super::{
        LiveMempoolOwner, MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS, run_live_mempool_owner,
    };

    #[tokio::test]
    async fn snapshot_marker_with_a_different_source_tip_remains_private()
    -> Result<(), Box<dyn Error>> {
        let temporary = tempfile::TempDir::new()?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                apply_canonical_control_command(&mut store, command);
            }
        });
        let owner = LiveMempoolOwner::default();
        let mismatched_source_tip = zinder_core::BlockId::new(
            zinder_core::BlockHeight::new(1),
            zinder_core::BlockHash::from_bytes([2; 32]),
        );

        let outcome = owner
            .complete_hydration(&canonical, mismatched_source_tip)
            .await;

        assert_eq!(
            outcome.err().map(|status| status.code()),
            Some(Code::Unavailable)
        );
        assert!(!owner.is_serving());
        assert!(
            canonical
                .mempool_event_page(
                    None,
                    NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
                )
                .await?
                .is_empty()
        );
        command_task.abort();
        let _ = command_task.await;
        Ok(())
    }

    #[tokio::test]
    async fn point_read_rejects_a_chain_epoch_newer_than_its_certified_source_tip()
    -> Result<(), Box<dyn Error>> {
        let temporary = tempfile::TempDir::new()?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                apply_canonical_control_command(&mut store, command);
            }
        });
        let owner = LiveMempoolOwner::default();
        owner
            .complete_hydration(&canonical, fixture_source_tip())
            .await?;
        let mut newer_chain_epoch = canonical.chain_epoch().await?.chain_epoch;
        newer_chain_epoch.visible_tip_hash = zinder_core::BlockHash::from_bytes([2; 32]);

        let outcome = owner
            .entry_for(newer_chain_epoch, TransactionId::from_bytes([0xA5; 32]))
            .await;

        assert_eq!(
            outcome.err().map(|status| status.code()),
            Some(Code::Unavailable)
        );
        command_task.abort();
        let _ = command_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_snapshot_marker_hides_partial_hydration_and_reconnect_gap()
    -> Result<(), Box<dyn Error>> {
        let temporary = tempfile::TempDir::new()?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                apply_canonical_control_command(&mut store, command);
            }
        });
        let (source, source_control) = MockMempoolSource::streaming();
        let owner = LiveMempoolOwner::default();
        let (ready_signal, ready_gate) = mempool_ready_channel();
        let cancel = CancellationToken::new();
        let owner_task = tokio::spawn(run_live_mempool_owner(
            Arc::new(source),
            canonical.clone(),
            owner.clone(),
            ready_signal,
            cancel.clone(),
        ));

        assert_partial_snapshot_is_private(&source_control, &owner, &ready_gate, &canonical)
            .await?;
        let first_generation_cursor =
            complete_initial_generation(&source_control, &owner, &ready_gate, &canonical).await?;
        reconcile_empty_snapshot(
            &source_control,
            &owner,
            &ready_gate,
            &canonical,
            first_generation_cursor.as_bytes(),
        )
        .await?;
        assert_abandoned_generation_is_not_durable(
            &source_control,
            &owner,
            &ready_gate,
            &canonical,
        )
        .await?;

        cancel.cancel();
        owner_task.await?;
        command_task.abort();
        let _ = command_task.await;
        Ok(())
    }

    async fn assert_partial_snapshot_is_private(
        source_control: &MockMempoolSourceControl,
        owner: &LiveMempoolOwner,
        ready_gate: &MempoolReadyGate,
        canonical: &CanonicalControlHandle,
    ) -> Result<(), Box<dyn Error>> {
        wait_for_source_open(source_control, 1).await?;
        assert_eq!(
            owner.require_serving().err().map(|status| status.code()),
            Some(Code::Unavailable)
        );
        assert!(!ready_gate.is_hydrated());

        source_control.push_added(source_entry(0xA1)?)?;
        wait_for_staged_entries(owner, 1).await?;
        assert_eq!(owner.index.entry_count(), 0);
        assert_eq!(
            owner.require_serving().err().map(|status| status.code()),
            Some(Code::Unavailable)
        );
        assert!(!ready_gate.is_hydrated());
        assert!(
            canonical
                .mempool_event_page(
                    None,
                    NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
                )
                .await?
                .is_empty(),
            "partial source snapshots must not reach durable history"
        );

        source_control.push_added(source_entry(0xA2)?)?;
        wait_for_staged_entries(owner, 2).await?;
        assert_eq!(
            owner.require_serving().err().map(|status| status.code()),
            Some(Code::Unavailable)
        );
        Ok(())
    }

    async fn complete_initial_generation(
        source_control: &MockMempoolSourceControl,
        owner: &LiveMempoolOwner,
        ready_gate: &MempoolReadyGate,
        canonical: &CanonicalControlHandle,
    ) -> Result<zinder_store::StreamCursorTokenV1, Box<dyn Error>> {
        source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_serving(owner, true).await?;
        wait_for_hydration(ready_gate, true).await?;
        assert_eq!(owner.index.entry_count(), 2);
        let history = canonical
            .mempool_event_page(
                None,
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(
            history.len(),
            2,
            "the source completion marker is not durable history"
        );
        assert!(
            history
                .iter()
                .all(|envelope| matches!(envelope.event, MempoolEvent::Added { .. }))
        );
        Ok(history
            .last()
            .ok_or("first completed source generation omitted its durable cursor")?
            .cursor
            .clone())
    }

    async fn reconcile_empty_snapshot(
        source_control: &MockMempoolSourceControl,
        owner: &LiveMempoolOwner,
        ready_gate: &MempoolReadyGate,
        canonical: &CanonicalControlHandle,
        first_generation_cursor: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        source_control.close_stream();
        wait_for_serving(owner, false).await?;
        wait_for_hydration(ready_gate, false).await?;
        wait_for_source_open(source_control, 2).await?;

        // The old index stays private until an empty replacement snapshot's
        // marker makes its terminal events durable.
        assert_eq!(owner.index.entry_count(), 2);
        source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_serving(owner, true).await?;
        wait_for_hydration(ready_gate, true).await?;
        assert_eq!(owner.index.entry_count(), 0);
        let terminal_history = canonical
            .mempool_event_page(
                Some(first_generation_cursor.to_vec()),
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(terminal_history.len(), 2);
        assert!(
            terminal_history
                .iter()
                .all(|envelope| matches!(envelope.event, MempoolEvent::Invalidated { .. }))
        );
        Ok(())
    }

    async fn assert_abandoned_generation_is_not_durable(
        source_control: &MockMempoolSourceControl,
        owner: &LiveMempoolOwner,
        ready_gate: &MempoolReadyGate,
        canonical: &CanonicalControlHandle,
    ) -> Result<(), Box<dyn Error>> {
        source_control.close_stream();
        wait_for_serving(owner, false).await?;
        wait_for_hydration(ready_gate, false).await?;
        wait_for_source_open(source_control, 3).await?;
        let abandoned_entry = source_entry(0xB1)?;
        let abandoned_transaction_id = abandoned_entry.transaction_id;
        source_control.push_added(abandoned_entry)?;
        wait_for_staged_entries(owner, 1).await?;
        let before_abandoned_generation = canonical
            .mempool_event_page(
                None,
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(before_abandoned_generation.len(), 4);

        source_control.close_stream();
        wait_for_source_open(source_control, 4).await?;
        source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_serving(owner, true).await?;
        wait_for_hydration(ready_gate, true).await?;
        assert_eq!(owner.index.entry_count(), 0);
        let after_abandoned_generation = canonical
            .mempool_event_page(
                None,
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(after_abandoned_generation.len(), 4);
        assert!(
            after_abandoned_generation
                .iter()
                .all(|envelope| envelope.transaction_id() != abandoned_transaction_id)
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_restart_reconciles_retained_live_history_against_empty_snapshot()
    -> Result<(), Box<dyn Error>> {
        let temporary = tempfile::TempDir::new()?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                apply_canonical_control_command(&mut store, command);
            }
        });

        let (first_source, first_source_control) = MockMempoolSource::streaming();
        let first_owner = LiveMempoolOwner::default();
        let (first_ready_signal, first_ready_gate) = mempool_ready_channel();
        let first_cancel = CancellationToken::new();
        let first_owner_task = tokio::spawn(run_live_mempool_owner(
            Arc::new(first_source),
            canonical.clone(),
            first_owner.clone(),
            first_ready_signal,
            first_cancel.clone(),
        ));
        wait_for_source_open(&first_source_control, 1).await?;
        first_source_control.push_added(source_entry(0xC1)?)?;
        wait_for_staged_entries(&first_owner, 1).await?;
        first_source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_serving(&first_owner, true).await?;
        wait_for_hydration(&first_ready_gate, true).await?;
        let first_history = canonical
            .mempool_event_page(
                None,
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(first_history.len(), 1);
        let restart_cursor = first_history[0].cursor.clone();

        first_cancel.cancel();
        first_owner_task.await?;
        drop(first_owner);

        let (second_source, second_source_control) = MockMempoolSource::streaming();
        let second_owner = LiveMempoolOwner::default();
        let (second_ready_signal, second_ready_gate) = mempool_ready_channel();
        let second_cancel = CancellationToken::new();
        let second_owner_task = tokio::spawn(run_live_mempool_owner(
            Arc::new(second_source),
            canonical.clone(),
            second_owner.clone(),
            second_ready_signal,
            second_cancel.clone(),
        ));
        wait_for_source_open(&second_source_control, 1).await?;
        assert_eq!(second_owner.index.entry_count(), 1);
        assert_eq!(
            second_owner
                .require_serving()
                .err()
                .map(|status| status.code()),
            Some(Code::Unavailable),
            "durable replay must stay private until the replacement snapshot completes"
        );
        second_source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_serving(&second_owner, true).await?;
        wait_for_hydration(&second_ready_gate, true).await?;
        assert_eq!(second_owner.index.entry_count(), 0);
        let terminal_history = canonical
            .mempool_event_page(
                Some(restart_cursor.as_bytes().to_vec()),
                NonZeroU32::new(8).ok_or("mempool page size must be nonzero")?,
            )
            .await?;
        assert_eq!(terminal_history.len(), 1);
        assert!(matches!(
            terminal_history[0].event,
            MempoolEvent::Invalidated { .. }
        ));

        second_cancel.cancel();
        second_owner_task.await?;
        command_task.abort();
        let _ = command_task.await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activation_scale_empty_replacement_reconciles_in_bounded_batches()
    -> Result<(), Box<dyn Error>> {
        let temporary = tempfile::TempDir::new()?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let (batch_size_sender, mut batch_size_receiver) = tokio::sync::mpsc::unbounded_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                if let CanonicalControlCommand::AppendMempoolEvents { events, .. } = &command {
                    let _send_outcome = batch_size_sender.send(events.len());
                }
                apply_canonical_control_command(&mut store, command);
            }
        });
        let (source, source_control) = MockMempoolSource::streaming();
        let owner = LiveMempoolOwner::default();
        let (ready_signal, ready_gate) = mempool_ready_channel();
        let cancel = CancellationToken::new();
        let owner_task = tokio::spawn(run_live_mempool_owner(
            Arc::new(source),
            canonical.clone(),
            owner.clone(),
            ready_signal,
            cancel.clone(),
        ));
        let transaction_count = MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS * 4;
        wait_for_source_open(&source_control, 1).await?;
        for transaction_nonce in 0..transaction_count {
            source_control
                .push_added(source_entry_with_nonce(u32::try_from(transaction_nonce)?)?)?;
        }
        wait_for_staged_entries(&owner, transaction_count).await?;
        source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_hydration(&ready_gate, true).await?;
        assert_eq!(owner.index.entry_count(), transaction_count);

        source_control.close_stream();
        wait_for_source_open(&source_control, 2).await?;
        source_control.complete_initial_snapshot(fixture_source_tip())?;
        wait_for_hydration(&ready_gate, true).await?;

        assert_eq!(owner.index.entry_count(), 0);
        let expected_event_count = transaction_count.saturating_mul(2);
        assert_eq!(durable_event_count(&canonical).await?, expected_event_count);
        let reconciliation_batch_sizes =
            std::iter::from_fn(|| batch_size_receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(reconciliation_batch_sizes.len() > 2);
        assert!(
            reconciliation_batch_sizes
                .iter()
                .all(|batch_size| *batch_size <= MAX_MEMPOOL_RECONCILIATION_BATCH_EVENTS)
        );
        assert_eq!(
            reconciliation_batch_sizes.iter().sum::<usize>(),
            expected_event_count
        );
        cancel.cancel();
        owner_task.await?;
        command_task.abort();
        let _ = command_task.await;
        Ok(())
    }

    fn source_entry(transaction_tag: u8) -> Result<MempoolSourceEntry, Box<dyn Error>> {
        source_entry_with_nonce(u32::from(transaction_tag))
    }

    fn source_entry_with_nonce(
        transaction_nonce: u32,
    ) -> Result<MempoolSourceEntry, Box<dyn Error>> {
        let raw_transaction_bytes = synthetic_v4_tx_bytes(transaction_nonce);
        let transaction: ZebraTransaction =
            raw_transaction_bytes.as_slice().zcash_deserialize_into()?;
        Ok(MempoolSourceEntry {
            transaction_id: TransactionId::from_bytes(transaction.hash().0),
            auth_digest: transaction
                .auth_digest()
                .map(|digest| AuthDigest::from_bytes(digest.0)),
            raw_transaction_bytes: RawTransactionBytes::new(raw_transaction_bytes),
            observed_at_unix_millis: UnixTimestampMillis::new(1_750_000_000_000),
        })
    }

    fn fixture_source_tip() -> zinder_core::BlockId {
        zinder_core::BlockId::new(
            zinder_core::BlockHeight::new(1),
            zinder_core::BlockHash::from_bytes([1; 32]),
        )
    }

    fn synthetic_v4_tx_bytes(transaction_nonce: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x8000_0004_u32.to_le_bytes());
        bytes.extend_from_slice(&0x892F_2085_u32.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&transaction_nonce.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.push(0);
        bytes
    }

    async fn durable_event_count(
        canonical: &CanonicalControlHandle,
    ) -> Result<usize, Box<dyn Error>> {
        let page_size = NonZeroU32::new(64).ok_or("mempool page size must be nonzero")?;
        let mut after_cursor = None;
        let mut event_count = 0_usize;
        loop {
            let page = canonical
                .mempool_event_page(after_cursor, page_size)
                .await?;
            let Some(last) = page.last() else {
                return Ok(event_count);
            };
            event_count = event_count.saturating_add(page.len());
            after_cursor = Some(last.cursor.as_bytes().to_vec());
        }
    }

    async fn wait_for_source_open(
        control: &MockMempoolSourceControl,
        expected_open_count: u32,
    ) -> Result<(), Box<dyn Error>> {
        wait_until(|| control.open_count() >= expected_open_count).await
    }

    async fn wait_for_staged_entries(
        owner: &LiveMempoolOwner,
        expected_entry_count: usize,
    ) -> Result<(), Box<dyn Error>> {
        wait_until(|| owner.staged_entry_count() == expected_entry_count).await
    }

    async fn wait_for_serving(
        owner: &LiveMempoolOwner,
        expected: bool,
    ) -> Result<(), Box<dyn Error>> {
        wait_until(|| owner.is_serving() == expected).await
    }

    async fn wait_for_hydration(
        ready_gate: &super::super::MempoolReadyGate,
        expected: bool,
    ) -> Result<(), Box<dyn Error>> {
        wait_until(|| ready_gate.is_hydrated() == expected).await
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) -> Result<(), Box<dyn Error>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !condition() {
            if std::time::Instant::now() > deadline {
                return Err("timed out waiting for live mempool owner state".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }
}
