//! In-process derive consumer dispatch driven by canonical chain events.
//!
//! `zinder-ingest` opens the derive store as a primary, tails durable
//! canonical chain events, hydrates each event's committed block contexts,
//! and hands those contexts to [`zinder_derive::DeriveStore::write_chain_event`].
//! Consumer writes and cursor advances land in one derive-store write batch
//! per chain epoch.
//!
//! Reader processes (`zinder-explorer`) open the same derive store path in
//! secondary mode (per [`zinder_derive::DeriveStore::open_secondary`]) and
//! advance their view via [`zinder_derive::DeriveStore::try_catch_up`].

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, ChainEpochId, TransactionFactsArtifact,
    TransparentOutPoint, TransparentSpendFact,
};
use zinder_derive::{
    BlockCommitContext, BlockCommitPayload, BlockSummaryConsumer, ChainEventDispatchInputs,
    DeriveStore, DeriveStoreOptions, MempoolConsumerEvent, MempoolConsumerEventVariant,
    MempoolEventCountsConsumer, RecentTransactionsConsumer, TransactionFeesConsumer,
    TransparentAddressActivityConsumer, TransparentAddressTransactionHistoryConsumer,
    TransparentSpendFacts,
};
use zinder_store::{
    ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest, MempoolEvent, MempoolEventEnvelope,
    PrimaryChainStore, StorageTuning, StreamCursorTokenV1,
};

use crate::{
    DeriveReplayPolicy, IngestDeriveConfig, IngestError,
    chain_ingest::{ingest_error_class, outcome_status},
    memory_pressure::RuntimeMemorySnapshot,
};

const DERIVE_REPLAY_STAGE_READ_EVENTS: &str = "read_events";
const DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS: &str = "hydrate_blocks";
const DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS: &str = "read_transparent_spend_facts";
const DERIVE_REPLAY_STAGE_DISPATCH_EVENT: &str = "dispatch_event";
const CANONICAL_FIRST_MEMORY_PRESSURE_HIGH_RATIO: f64 = 0.95;

/// Default poll cadence for the derive tailer when the canonical store is
/// ingesting faster than chain-event notifications arrive.
pub const DEFAULT_DERIVE_TAILER_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeriveReplayPauseReason {
    MemoryPressure,
}

impl DeriveReplayPauseReason {
    const fn as_label(self) -> &'static str {
        match self {
            Self::MemoryPressure => "memory_pressure",
        }
    }
}

/// Opens the ingest-owned derive store primary for a canonical store path.
pub fn open_primary_derive_store_for_canonical(
    canonical_path: &Path,
    tuning: StorageTuning,
) -> Result<DeriveStore, zinder_derive::DeriveStoreError> {
    DeriveStore::open(
        DeriveStore::path_for_canonical(canonical_path),
        DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: DeriveStore::bundled_consumer_column_families(),
            tuning,
        },
    )
}

/// Spawns the ingest-owned chain-event tailer for derive consumers.
///
/// The task is intentionally best-effort from the canonical ingest point of
/// view: canonical commits have already succeeded before the tailer sees an
/// event, so a derive failure is exposed through lag/error metrics and logs
/// without blocking new chain facts from being indexed.
#[must_use = "drop the handle to detach the derive tailer or await it for symmetric shutdown"]
pub fn spawn_derive_tailer_task(
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !derive_store.has_consumer_column_families() {
            tracing::info!(
                target: "zinder::ingest",
                event = "derive_tailer_disabled",
                "derive tailer disabled because the derive store has no chain-event consumers"
            );
            return;
        }

        tracing::info!(
            target: "zinder::ingest",
            event = "derive_tailer_started",
            poll_interval_ms = u64::try_from(poll_interval.as_millis()).unwrap_or(u64::MAX),
            derive_replay_concurrency = derive_config.replay_concurrency.get(),
            replay_batch_blocks = derive_config.replay_batch_blocks.get(),
            replay_policy = derive_config.replay_policy.as_kebab_case(),
            "derive chain-event tailer started"
        );

        loop {
            let (memory_snapshot, pause_reason) = current_derive_replay_pause_reason(derive_config);
            record_derive_replay_budget(derive_config.replay_policy, pause_reason, poll_interval);
            if let Some(reason) = pause_reason {
                tracing::debug!(
                    target: "zinder::ingest",
                    event = "derive_tailer_replay_paused",
                    replay_policy = derive_config.replay_policy.as_kebab_case(),
                    pause_reason = reason.as_label(),
                    memory_pressure_ratio = ?memory_snapshot.pressure_ratio(),
                    "derive replay paused so canonical ingest keeps the memory budget"
                );

                tokio::select! {
                    () = cancel.cancelled() => {
                        tracing::info!(
                            target: "zinder::ingest",
                            event = "derive_tailer_cancelled",
                            "derive chain-event tailer cancelled"
                        );
                        return;
                    }
                    () = tokio::time::sleep(poll_interval) => {}
                }
                continue;
            }

            let started_at = Instant::now();
            let outcome =
                catch_up_derive_store_to_canonical(&chain_store, &derive_store, derive_config)
                    .await;
            record_derive_tailer_tick(started_at, &outcome);
            if let Err(error) = outcome {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "derive_tailer_replay_failed",
                    error = %error,
                    "derive tailer failed to replay canonical chain events; retrying"
                );
            }

            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "derive_tailer_cancelled",
                        "derive chain-event tailer cancelled"
                    );
                    return;
                }
                () = tokio::time::sleep(poll_interval) => {}
            }
        }
    })
}

/// Spawns the derive replay budget metric sampler.
///
/// The derive tailer can spend multiple seconds replaying retained events. This
/// task keeps replay budget gauges tied to current memory pressure instead of
/// to the tailer's next scheduling boundary.
#[must_use = "drop the handle to detach the derive replay budget sampler or await it for symmetric shutdown"]
pub fn spawn_derive_replay_budget_metrics_task(
    derive_config: IngestDeriveConfig,
    poll_interval: Duration,
    sample_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (_memory_snapshot, pause_reason) =
                current_derive_replay_pause_reason(derive_config);
            record_derive_replay_budget(derive_config.replay_policy, pause_reason, poll_interval);

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(sample_interval) => {}
            }
        }
    })
}

fn current_derive_replay_pause_reason(
    derive_config: IngestDeriveConfig,
) -> (RuntimeMemorySnapshot, Option<DeriveReplayPauseReason>) {
    let memory_snapshot = RuntimeMemorySnapshot::sample();
    let pause_reason = derive_replay_pause_reason(derive_config.replay_policy, memory_snapshot);
    (memory_snapshot, pause_reason)
}

fn derive_replay_pause_reason(
    replay_policy: DeriveReplayPolicy,
    memory_snapshot: RuntimeMemorySnapshot,
) -> Option<DeriveReplayPauseReason> {
    if replay_policy == DeriveReplayPolicy::Continuous {
        return None;
    }
    let pressure_ratio = memory_snapshot.pressure_ratio()?;
    (pressure_ratio >= CANONICAL_FIRST_MEMORY_PRESSURE_HIGH_RATIO)
        .then_some(DeriveReplayPauseReason::MemoryPressure)
}

/// Replays retained canonical chain events that have not reached the
/// ingest-owned derive store.
///
/// The canonical store commits before the derive store because they are
/// separate `RocksDB` instances. Persisting the canonical chain-event
/// cursor in every chain consumer lets startup repair the only crash gap:
/// the canonical event is durable while the derive cursor still lags.
pub async fn catch_up_derive_store_to_canonical(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    derive_config: IngestDeriveConfig,
) -> Result<(), IngestError> {
    if !derive_store.has_consumer_column_families() {
        return Ok(());
    }

    record_current_derive_replay_tip(chain_store)?;

    let mut cursor = persisted_chain_event_cursor(derive_store)?;
    loop {
        let read_started_at = Instant::now();
        let page_outcome = chain_store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(
                cursor.as_ref(),
            ))
            .map_err(IngestError::from);
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_READ_EVENTS,
            read_started_at,
            &page_outcome,
        );
        let page = page_outcome?;
        if page.is_empty() {
            return Ok(());
        }

        for envelope in page {
            cursor = Some(
                replay_chain_event_to_derive(chain_store, derive_store, envelope, derive_config)
                    .await?,
            );
        }
    }
}

async fn replay_chain_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    derive_config: IngestDeriveConfig,
) -> Result<StreamCursorTokenV1, IngestError> {
    if matches!(envelope.event, ChainEvent::ChainReorged { .. }) {
        return replay_reorg_event_to_derive(chain_store, derive_store, envelope, derive_config)
            .await;
    }

    let committed_range = committed_block_range_for_chain_event(&envelope)?;
    let block_count = block_height_range_len(committed_range);
    if block_count == 0 {
        return replay_empty_committed_event(chain_store, derive_store, envelope, committed_range);
    }

    replay_committed_event_to_derive_in_batches(
        chain_store,
        derive_store,
        envelope,
        committed_range,
        derive_config,
    )
    .await
}

fn replay_empty_committed_event(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    committed_range: BlockHeightRange,
) -> Result<StreamCursorTokenV1, IngestError> {
    let contexts = HashMap::new();
    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: &envelope.event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        finalized_height: envelope.finalized_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, true);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    if let Err(error) = dispatch_outcome {
        record_derive_replay_event(0, Some(&error));
        return Err(error);
    }

    record_derive_replay_event(0, None);
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(committed_range.end, tip_height);
    }
    Ok(envelope.cursor)
}

async fn replay_committed_event_to_derive_in_batches(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    committed_range: BlockHeightRange,
    derive_config: IngestDeriveConfig,
) -> Result<StreamCursorTokenV1, IngestError> {
    let block_count = block_height_range_len(committed_range);
    let mut next_height = committed_range.start;
    while next_height <= committed_range.end {
        let hydrate_started_at = Instant::now();
        let replay_blocks_outcome = hydrate_committed_block_replay_batch(
            chain_store,
            &envelope,
            next_height,
            committed_range.end,
            derive_config,
        );
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS,
            hydrate_started_at,
            &replay_blocks_outcome,
        );
        let replay_batch = match replay_blocks_outcome {
            Ok(replay_batch) => replay_batch,
            Err(error) => {
                record_derive_replay_event(block_count, Some(&error));
                return Err(error);
            }
        };

        let replay_range = replay_batch.block_range;
        let final_chunk = replay_range.end >= committed_range.end;
        let chunk_event = committed_chain_event_chunk(&envelope.event, replay_range);
        let resolve_started_at = Instant::now();
        let contexts_outcome = build_block_contexts_from_committed_event(
            chain_store,
            envelope.chain_epoch.id,
            replay_batch.blocks,
            derive_config.replay_concurrency,
        )
        .await;
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS,
            resolve_started_at,
            &contexts_outcome,
        );
        let contexts = match contexts_outcome {
            Ok(contexts) => contexts,
            Err(error) => {
                record_derive_replay_event(block_count, Some(&error));
                return Err(error);
            }
        };

        let inputs = ChainEventDispatchInputs {
            chain_epoch: envelope.chain_epoch,
            chain_event: &chunk_event,
            chain_cursor: envelope.cursor.as_bytes(),
            event_sequence: envelope.event_sequence,
            finalized_height: envelope.finalized_height,
        };
        let dispatch_started_at = Instant::now();
        let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, final_chunk);
        record_derive_replay_stage(
            DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
            dispatch_started_at,
            &dispatch_outcome,
        );
        if let Err(error) = dispatch_outcome {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }

        next_height = replay_range.end.next().ok_or_else(|| {
            IngestError::DeriveDispatch("derive replay height overflow".to_owned())
        })?;
    }

    record_derive_replay_event(block_count, None);
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(committed_range.end, tip_height);
    }
    Ok(envelope.cursor)
}

async fn replay_reorg_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    derive_config: IngestDeriveConfig,
) -> Result<StreamCursorTokenV1, IngestError> {
    let committed_range = committed_block_range_for_chain_event(&envelope)?;
    let block_count = block_height_range_len(committed_range);

    let hydrate_started_at = Instant::now();
    let replay_blocks_outcome =
        hydrate_committed_blocks_for_reorg_event(chain_store, &envelope, committed_range);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS,
        hydrate_started_at,
        &replay_blocks_outcome,
    );
    let replay_blocks = match replay_blocks_outcome {
        Ok(replay_blocks) => replay_blocks,
        Err(error) => {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }
    };

    let resolve_started_at = Instant::now();
    let contexts_outcome = build_block_contexts_from_committed_event(
        chain_store,
        envelope.chain_epoch.id,
        replay_blocks,
        derive_config.replay_concurrency,
    )
    .await;
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS,
        resolve_started_at,
        &contexts_outcome,
    );
    let contexts = match contexts_outcome {
        Ok(contexts) => contexts,
        Err(error) => {
            record_derive_replay_event(block_count, Some(&error));
            return Err(error);
        }
    };

    let inputs = ChainEventDispatchInputs {
        chain_epoch: envelope.chain_epoch,
        chain_event: &envelope.event,
        chain_cursor: envelope.cursor.as_bytes(),
        event_sequence: envelope.event_sequence,
        finalized_height: envelope.finalized_height,
    };
    let dispatch_started_at = Instant::now();
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts, true);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_DISPATCH_EVENT,
        dispatch_started_at,
        &dispatch_outcome,
    );
    if let Err(error) = dispatch_outcome {
        record_derive_replay_event(block_count, Some(&error));
        return Err(error);
    }

    record_derive_replay_event(block_count, None);
    if let Some(tip_height) = record_current_derive_replay_tip(chain_store)? {
        record_derive_replay_progress(committed_range.end, tip_height);
    }
    Ok(envelope.cursor)
}

fn persisted_chain_event_cursor(
    derive_store: &DeriveStore,
) -> Result<Option<StreamCursorTokenV1>, IngestError> {
    let mut cursor: Option<Vec<u8>> = None;
    for consumer_name in DeriveStore::bundled_chain_event_consumer_names() {
        let Some(candidate) = derive_store.get_chain_event_cursor(*consumer_name)? else {
            continue;
        };
        if let Some(existing) = cursor.as_ref() {
            if existing != &candidate {
                return Err(IngestError::DeriveDispatch(
                    "chain derive consumer cursors disagree".to_owned(),
                ));
            }
        } else {
            cursor = Some(candidate);
        }
    }
    Ok(cursor.map(StreamCursorTokenV1::from_bytes))
}

fn committed_block_range_for_chain_event(
    envelope: &ChainEventEnvelope,
) -> Result<BlockHeightRange, IngestError> {
    let (ChainEvent::ChainCommitted { committed } | ChainEvent::ChainReorged { committed, .. }) =
        &envelope.event
    else {
        return Err(IngestError::DeriveDispatch(
            "unsupported chain event variant".to_owned(),
        ));
    };
    Ok(committed.block_range)
}

fn hydrate_committed_block_replay_batch(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    start_height: BlockHeight,
    end_height: BlockHeight,
    derive_config: IngestDeriveConfig,
) -> Result<CanonicalReplayBatch, IngestError> {
    let reader = chain_store.chain_epoch_reader_at(envelope.chain_epoch.id)?;
    let max_blocks = nonzero_u32_to_usize(derive_config.replay_batch_blocks);
    let remaining_blocks = usize::try_from(
        end_height
            .value()
            .saturating_sub(start_height.value())
            .saturating_add(1),
    )
    .unwrap_or(usize::MAX);
    let mut replay_blocks = Vec::with_capacity(max_blocks.min(remaining_blocks));
    let mut next_height = start_height;

    while next_height <= end_height && replay_blocks.len() < max_blocks {
        let height = next_height;
        let Some(header) = reader.block_header_at(height)? else {
            return Err(IngestError::DeriveDispatch(format!(
                "committed chain event {} references unavailable block-header facts {}",
                envelope.event_sequence,
                height.value()
            )));
        };
        let transaction_ids = reader.transaction_ids_at_height(height)?;
        let mut facts_by_id = reader.transaction_facts_by_ids(&transaction_ids)?;
        let mut transactions = Vec::with_capacity(transaction_ids.len());
        for transaction_id in transaction_ids {
            let Some(transaction) = facts_by_id.remove(&transaction_id).flatten() else {
                return Err(IngestError::DeriveDispatch(format!(
                    "committed chain event {} references unavailable transaction facts {}",
                    envelope.event_sequence,
                    hex::encode(transaction_id.as_bytes())
                )));
            };
            transactions.push(transaction);
        }
        let transparent_spends = transparent_spent_outpoints_for_transactions(&transactions);
        replay_blocks.push(CanonicalReplayBlock {
            height,
            block_hash: header.block_hash,
            previous_block_hash: header.parent_hash,
            block_time_unix_seconds: header.block_time,
            block_size_bytes: header.block_size_bytes,
            transactions,
            transparent_spends,
        });
        next_height = height.next().ok_or_else(|| {
            IngestError::DeriveDispatch("derive replay height overflow".to_owned())
        })?;
    }

    let Some(first) = replay_blocks.first() else {
        return Err(IngestError::DeriveDispatch(
            "derive replay batch did not hydrate any blocks".to_owned(),
        ));
    };
    let last = replay_blocks
        .last()
        .ok_or_else(|| IngestError::DeriveDispatch("derive replay batch empty".to_owned()))?;
    Ok(CanonicalReplayBatch {
        block_range: BlockHeightRange::inclusive(first.height, last.height),
        blocks: replay_blocks,
    })
}

fn hydrate_committed_blocks_for_reorg_event(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    committed_range: BlockHeightRange,
) -> Result<Vec<CanonicalReplayBlock>, IngestError> {
    let reader = chain_store.chain_epoch_reader_at(envelope.chain_epoch.id)?;
    let mut replay_blocks = Vec::with_capacity(committed_range.into_iter().len());
    for height in committed_range {
        replay_blocks.push(hydrate_committed_block(&reader, envelope, height)?);
    }
    Ok(replay_blocks)
}

fn hydrate_committed_block(
    reader: &zinder_store::ChainEpochReader<'_>,
    envelope: &ChainEventEnvelope,
    height: BlockHeight,
) -> Result<CanonicalReplayBlock, IngestError> {
    let Some(header) = reader.block_header_at(height)? else {
        return Err(IngestError::DeriveDispatch(format!(
            "committed chain event {} references unavailable block-header facts {}",
            envelope.event_sequence,
            height.value()
        )));
    };
    let transaction_ids = reader.transaction_ids_at_height(height)?;
    let mut facts_by_id = reader.transaction_facts_by_ids(&transaction_ids)?;
    let mut transactions = Vec::with_capacity(transaction_ids.len());
    for transaction_id in transaction_ids {
        let Some(transaction) = facts_by_id.remove(&transaction_id).flatten() else {
            return Err(IngestError::DeriveDispatch(format!(
                "committed chain event {} references unavailable transaction facts {}",
                envelope.event_sequence,
                hex::encode(transaction_id.as_bytes())
            )));
        };
        transactions.push(transaction);
    }
    let transparent_spends = transparent_spent_outpoints_for_transactions(&transactions);
    Ok(CanonicalReplayBlock {
        height,
        block_hash: header.block_hash,
        previous_block_hash: header.parent_hash,
        block_time_unix_seconds: header.block_time,
        block_size_bytes: header.block_size_bytes,
        transactions,
        transparent_spends,
    })
}

fn committed_chain_event_chunk(event: &ChainEvent, replay_range: BlockHeightRange) -> ChainEvent {
    match event {
        ChainEvent::ChainCommitted { committed } => ChainEvent::ChainCommitted {
            committed: zinder_store::ChainEpochCommitted {
                chain_epoch: committed.chain_epoch,
                block_range: replay_range,
            },
        },
        ChainEvent::ChainReorged { .. } | _ => event.clone(),
    }
}

/// Dispatches the configured chain-event consumers against parsed block
/// contexts and lets `DeriveStore` own the write-batch boundary.
pub(crate) fn dispatch_chain_event(
    derive_store: &DeriveStore,
    inputs: ChainEventDispatchInputs<'_>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>>,
    advance_cursor: bool,
) -> Result<(), IngestError> {
    let mut block_summary = BlockSummaryConsumer::new();
    let mut transaction_fees = TransactionFeesConsumer::new();
    let mut recent_transactions = RecentTransactionsConsumer::new();
    let mut transparent_activity = TransparentAddressActivityConsumer::new();
    let mut transparent_transaction_history = TransparentAddressTransactionHistoryConsumer::new();
    let mut consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 5] = [
        &mut block_summary,
        &mut transaction_fees,
        &mut recent_transactions,
        &mut transparent_activity,
        &mut transparent_transaction_history,
    ];
    derive_store
        .write_chain_event_chunk(&mut consumers, inputs, blocks, advance_cursor)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(())
}

/// Dispatches one committed mempool event into ingest-owned derive
/// consumers.
pub(crate) fn dispatch_mempool_event(
    derive_store: &DeriveStore,
    envelope: &MempoolEventEnvelope,
) -> Result<(), IngestError> {
    match &envelope.event {
        MempoolEvent::Added { entry } => {
            let transaction_id = entry.transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Added {
                    transaction_id: &transaction_id,
                    raw_transaction_bytes: entry.raw_transaction_bytes.as_slice(),
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Invalidated { transaction_id, .. } => {
            let transaction_id = transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Invalidated {
                    transaction_id: &transaction_id,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => {
            let transaction_id = transaction_id.as_bytes();
            let block_hash = block_hash.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Mined {
                    transaction_id: &transaction_id,
                    mined_height: *mined_height,
                    block_hash: &block_hash,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        MempoolEvent::Suppressed { transaction_id } => {
            let transaction_id = transaction_id.as_bytes();
            let event = MempoolConsumerEvent::new(
                envelope.event_sequence,
                envelope.source_observed_unix_millis,
                MempoolConsumerEventVariant::Suppressed {
                    transaction_id: &transaction_id,
                },
            );
            apply_mempool_event(derive_store, &event, envelope.cursor.as_bytes())
        }
        _ => Err(IngestError::DeriveDispatch(
            "unsupported mempool event variant".to_owned(),
        )),
    }
}

fn apply_mempool_event(
    derive_store: &DeriveStore,
    event: &MempoolConsumerEvent<'_>,
    cursor_bytes: &[u8],
) -> Result<(), IngestError> {
    let mut event_counts = MempoolEventCountsConsumer::new();
    derive_store
        .write_mempool_event(&mut event_counts, event, cursor_bytes)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(())
}

async fn build_block_contexts_from_committed_event(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: Vec<CanonicalReplayBlock>,
    _derive_replay_concurrency: NonZeroU32,
) -> Result<HashMap<BlockHeight, Arc<BlockCommitContext>>, IngestError> {
    let transparent_spends = read_transparent_spend_facts_for_committed_blocks(
        chain_store,
        chain_epoch_id,
        &replay_blocks,
    )
    .await?;
    let mut out = HashMap::with_capacity(replay_blocks.len());
    for block in replay_blocks {
        let context = BlockCommitContext::new(
            BlockCommitPayload {
                height: block.height,
                block_hash: encode_block_hash(block.block_hash),
                previous_block_hash: encode_block_hash(block.previous_block_hash),
                block_time_unix_seconds: block.block_time_unix_seconds,
                block_size_bytes: block.block_size_bytes,
                transactions: block.transactions,
            },
            TransparentSpendFacts::from_map(Arc::clone(&transparent_spends)),
        );
        out.insert(block.height, Arc::new(context));
    }
    Ok(out)
}

struct CanonicalReplayBlock {
    height: BlockHeight,
    block_hash: BlockHash,
    previous_block_hash: BlockHash,
    block_time_unix_seconds: i64,
    block_size_bytes: u64,
    transactions: Vec<TransactionFactsArtifact>,
    transparent_spends: Vec<TransparentOutPoint>,
}

struct CanonicalReplayBatch {
    block_range: BlockHeightRange,
    blocks: Vec<CanonicalReplayBlock>,
}

fn transparent_spent_outpoints_for_transactions(
    transactions: &[TransactionFactsArtifact],
) -> Vec<TransparentOutPoint> {
    let mut spends = Vec::new();
    for transaction in transactions {
        for input in &transaction.transparent_inputs {
            if !input.spent_outpoint.is_coinbase_sentinel() {
                spends.push(input.spent_outpoint);
            }
        }
    }
    spends
}

async fn read_transparent_spend_facts_for_committed_blocks(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: &[CanonicalReplayBlock],
) -> Result<Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>, IngestError> {
    let mut requested_outpoints = HashSet::<TransparentOutPoint>::new();
    for block in replay_blocks {
        requested_outpoints.extend(block.transparent_spends.iter().copied());
    }

    let unique_spent_outpoint_count = requested_outpoints.len();
    record_transparent_spend_fact_requested_outpoints(unique_spent_outpoint_count);
    let outpoints = requested_outpoints.into_iter().collect::<Vec<_>>();
    let store = chain_store.clone();
    let read_started_at = Instant::now();
    let read_outcome = tokio::task::spawn_blocking(move || {
        let reader = store.chain_epoch_reader_at(chain_epoch_id)?;
        reader.transparent_spend_facts_by_outpoints(&outpoints)
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
    .map_err(IngestError::from);
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_READ_TRANSPARENT_SPEND_FACTS,
        read_started_at,
        &read_outcome,
    );
    let resolved = read_outcome?;
    record_transparent_spend_fact_count("resolved", resolved.len());
    record_transparent_spend_fact_count(
        "unresolved",
        unique_spent_outpoint_count.saturating_sub(resolved.len()),
    );
    Ok(Arc::new(resolved))
}

fn record_derive_replay_stage<T>(
    stage: &'static str,
    started_at: Instant,
    outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_derive_replay_stage_duration_seconds",
        "stage" => stage,
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
}

fn record_transparent_spend_fact_count(status: &'static str, count: usize) {
    if count == 0 {
        return;
    }
    metrics::counter!(
        "zinder_ingest_transparent_spend_fact_read_total",
        "status" => status
    )
    .increment(usize_to_u64_saturating(count));
}

fn record_transparent_spend_fact_requested_outpoints(count: usize) {
    metrics::histogram!("zinder_ingest_transparent_spend_fact_requested_outpoint_count")
        .record(usize_to_u32_saturating(count));
}

fn record_derive_replay_event(block_count: usize, error: Option<&IngestError>) {
    let status = if error.is_some() { "error" } else { "ok" };
    let error_class = ingest_error_class(error);
    metrics::counter!(
        "zinder_ingest_derive_replay_events_total",
        "status" => status,
        "error_class" => error_class
    )
    .increment(1);
    metrics::counter!(
        "zinder_ingest_derive_replay_blocks_total",
        "status" => status,
        "error_class" => error_class
    )
    .increment(usize_to_u64_saturating(block_count));
}

fn record_derive_replay_progress(progress_height: BlockHeight, canonical_tip_height: BlockHeight) {
    metrics::gauge!("zinder_ingest_derive_replay_height").set(f64::from(progress_height.value()));
    metrics::gauge!("zinder_ingest_derive_replay_lag_blocks").set(f64::from(
        canonical_tip_height
            .value()
            .saturating_sub(progress_height.value()),
    ));
}

fn record_current_derive_replay_tip(
    chain_store: &PrimaryChainStore,
) -> Result<Option<BlockHeight>, IngestError> {
    let canonical_tip_height = chain_store
        .current_chain_epoch()?
        .map(|epoch| epoch.tip_height);
    if let Some(tip_height) = canonical_tip_height {
        metrics::gauge!("zinder_ingest_derive_replay_tip_height")
            .set(f64::from(tip_height.value()));
    }
    Ok(canonical_tip_height)
}

fn record_derive_replay_budget(
    replay_policy: DeriveReplayPolicy,
    pause_reason: Option<DeriveReplayPauseReason>,
    poll_interval: Duration,
) {
    metrics::gauge!(
        "zinder_ingest_derive_replay_policy",
        "policy" => replay_policy.as_kebab_case()
    )
    .set(1.0);
    metrics::gauge!("zinder_ingest_derive_replay_paused").set(if pause_reason.is_some() {
        1.0
    } else {
        0.0
    });
    metrics::gauge!(
        "zinder_ingest_derive_replay_pause_reason",
        "reason" => DeriveReplayPauseReason::MemoryPressure.as_label()
    )
    .set(
        if pause_reason == Some(DeriveReplayPauseReason::MemoryPressure) {
            1.0
        } else {
            0.0
        },
    );
    metrics::gauge!("zinder_ingest_derive_replay_budget_seconds").set(if pause_reason.is_some() {
        0.0
    } else {
        poll_interval.as_secs_f64()
    });
}

fn record_derive_tailer_tick(started_at: Instant, outcome: &Result<(), IngestError>) {
    metrics::histogram!(
        "zinder_ingest_derive_tailer_tick_duration_seconds",
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_derive_tailer_ticks_total",
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .increment(1);
}

fn block_height_range_len(block_range: BlockHeightRange) -> usize {
    if block_range.start > block_range.end {
        return 0;
    }
    let length = block_range
        .end
        .value()
        .saturating_sub(block_range.start.value())
        .saturating_add(1);
    usize::try_from(length).map_or(usize::MAX, |converted| converted)
}

fn nonzero_u32_to_usize(amount: NonZeroU32) -> usize {
    usize::try_from(amount.get()).unwrap_or(usize::MAX)
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}

fn encode_block_hash(hash: BlockHash) -> Vec<u8> {
    hash.as_bytes().to_vec()
}
