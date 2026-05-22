//! In-process derive consumer dispatch driven by the canonical commit
//! pipeline.
//!
//! `zinder-ingest` opens the derive store as a primary, parses each block
//! committed in the current batch, and hands parsed block contexts to
//! [`zinder_derive::DeriveStore::write_chain_event`]. Consumer writes and
//! cursor advances land in one derive-store write batch per chain epoch.
//!
//! Reader processes (`zinder-explorer`) open the same derive store path in
//! secondary mode (per [`zinder_derive::DeriveStore::open_secondary`]) and
//! advance their view via [`zinder_derive::DeriveStore::try_catch_up`].

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::Instant,
};

use futures_util::stream::StreamExt as _;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto as _;
use zebra_chain::transparent;
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, ChainEpochId, TransactionId,
    TransparentOutPoint, TransparentPrevoutArtifact,
};
use zinder_derive::{
    BlockCommitContext, BlockCommitPayload, BlockSummaryConsumer, ChainEventDispatchInputs,
    DeriveStore, DeriveStoreOptions, MempoolConsumerEvent, MempoolConsumerEventVariant,
    MempoolEventCountsConsumer, PrevoutResolver, RecentTransactionsConsumer,
    TransactionFeesConsumer, TransparentAddressActivityConsumer,
};
use zinder_store::{
    ChainEpochReader, ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest, MempoolEvent,
    MempoolEventEnvelope, PrimaryChainStore, StorageTuning, StreamCursorTokenV1,
};

use crate::{
    IngestError,
    chain_ingest::{ingest_error_class, outcome_status},
    transparent_prevout_lookup::{
        TransparentPrevoutLookupMode, TransparentPrevoutLookupStage,
        read_chunked_transparent_prevouts_by_outpoints,
    },
};

const DERIVE_REPLAY_STAGE_READ_EVENTS: &str = "read_events";
const DERIVE_REPLAY_STAGE_HYDRATE_BLOCKS: &str = "hydrate_blocks";
const DERIVE_REPLAY_STAGE_RESOLVE_PREVOUTS: &str = "resolve_prevouts";
const DERIVE_REPLAY_STAGE_DISPATCH_EVENT: &str = "dispatch_event";
const DERIVE_CONTEXT_STAGE_HYDRATE_BLOCKS: &str = "hydrate_blocks";
const DERIVE_CONTEXT_STAGE_RESOLVE_PREVOUTS: &str = "resolve_prevouts";

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
    derive_concurrency: NonZeroU32,
) -> Result<(), IngestError> {
    if !derive_store.has_consumer_column_families() {
        return Ok(());
    }

    let canonical_tip_height = chain_store
        .current_chain_epoch()?
        .map(|epoch| epoch.tip_height);
    if let Some(tip_height) = canonical_tip_height {
        metrics::gauge!("zinder_ingest_derive_replay_tip_height")
            .set(f64::from(tip_height.value()));
    }

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
                replay_chain_event_to_derive(
                    chain_store,
                    derive_store,
                    envelope,
                    canonical_tip_height,
                    derive_concurrency,
                )
                .await?,
            );
        }
    }
}

async fn replay_chain_event_to_derive(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    envelope: ChainEventEnvelope,
    canonical_tip_height: Option<BlockHeight>,
    derive_concurrency: NonZeroU32,
) -> Result<StreamCursorTokenV1, IngestError> {
    let committed_range = committed_block_range_for_chain_event(&envelope)?;
    let block_count = block_height_range_len(committed_range);

    let hydrate_started_at = Instant::now();
    let replay_blocks_outcome = hydrate_committed_blocks_for_chain_event(
        chain_store,
        &envelope,
        committed_range,
        derive_concurrency,
    )
    .await;
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
    );
    record_derive_replay_stage(
        DERIVE_REPLAY_STAGE_RESOLVE_PREVOUTS,
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
    let dispatch_outcome = dispatch_chain_event(derive_store, inputs, &contexts);
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
    if let Some(tip_height) = canonical_tip_height {
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

async fn hydrate_committed_blocks_for_chain_event(
    chain_store: &PrimaryChainStore,
    envelope: &ChainEventEnvelope,
    committed_range: BlockHeightRange,
    derive_concurrency: NonZeroU32,
) -> Result<Vec<ParsedReplayBlock>, IngestError> {
    let reader = chain_store.chain_epoch_reader_at(envelope.chain_epoch.id)?;
    let block_artifacts = reader.blocks_in_range(committed_range)?;
    let mut finalized_blocks = Vec::with_capacity(block_artifacts.len());
    for (height, block) in committed_range.into_iter().zip(block_artifacts) {
        let Some(block) = block else {
            return Err(IngestError::DeriveDispatch(format!(
                "committed chain event {} references unavailable block {}",
                envelope.event_sequence,
                height.value()
            )));
        };
        finalized_blocks.push(block);
    }
    parse_replay_blocks(finalized_blocks, derive_concurrency).await
}

/// Builds per-block derive contexts from the batch before the canonical
/// artifacts are moved into the store commit.
pub(crate) fn build_block_contexts_from_batch(
    chain_store: &PrimaryChainStore,
    finalized_blocks: &[BlockArtifact],
    parsed_blocks: &[Option<Arc<ZebraBlock>>],
    transparent_prevouts: &[TransparentPrevoutArtifact],
) -> Result<BatchBlockContexts, IngestError> {
    let current_chain_reader = if chain_store.current_chain_epoch()?.is_some() {
        Some(chain_store.current_chain_epoch_reader()?)
    } else {
        None
    };
    build_block_contexts(
        finalized_blocks,
        parsed_blocks,
        current_chain_reader.as_ref(),
        transparent_prevouts,
        TransparentPrevoutLookupMode::WriterCommit,
    )
}

pub(crate) struct BatchBlockContexts {
    pub(crate) blocks: HashMap<BlockHeight, Arc<BlockCommitContext>>,
    pub(crate) prevouts_by_outpoint: Arc<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>>,
}

/// Dispatches the configured chain-event consumers against parsed block
/// contexts and lets `DeriveStore` own the write-batch boundary.
pub(crate) fn dispatch_chain_event(
    derive_store: &DeriveStore,
    inputs: ChainEventDispatchInputs<'_>,
    blocks: &HashMap<BlockHeight, Arc<BlockCommitContext>>,
) -> Result<(), IngestError> {
    let mut block_summary = BlockSummaryConsumer::new();
    let mut transaction_fees = TransactionFeesConsumer::new();
    let mut recent_transactions = RecentTransactionsConsumer::new();
    let mut transparent_activity = TransparentAddressActivityConsumer::new();
    let mut consumers: [&mut dyn zinder_derive::BlockKeyedConsumer; 4] = [
        &mut block_summary,
        &mut transaction_fees,
        &mut recent_transactions,
        &mut transparent_activity,
    ];
    derive_store
        .write_chain_event(&mut consumers, inputs, blocks)
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

fn build_block_contexts_from_committed_event(
    chain_store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    replay_blocks: Vec<ParsedReplayBlock>,
) -> Result<HashMap<BlockHeight, Arc<BlockCommitContext>>, IngestError> {
    let reader = chain_store.chain_epoch_reader_at(chain_epoch_id)?;
    build_block_contexts_from_parsed_blocks(
        replay_blocks,
        Some(&reader),
        &[],
        TransparentPrevoutLookupMode::ReaderEpoch,
    )
    .map(|contexts| contexts.blocks)
}

fn build_block_contexts(
    finalized_blocks: &[BlockArtifact],
    cached_blocks: &[Option<Arc<ZebraBlock>>],
    chain_reader: Option<&ChainEpochReader<'_>>,
    transparent_prevouts: &[TransparentPrevoutArtifact],
    stored_prevout_lookup: TransparentPrevoutLookupMode,
) -> Result<BatchBlockContexts, IngestError> {
    let hydrate_started_at = Instant::now();
    let parsed_blocks_outcome = parsed_blocks_for_commit(finalized_blocks, cached_blocks);
    record_derive_context_stage(
        DERIVE_CONTEXT_STAGE_HYDRATE_BLOCKS,
        hydrate_started_at,
        &parsed_blocks_outcome,
    );
    let parsed_blocks = parsed_blocks_outcome?;
    build_block_contexts_from_parsed_blocks(
        parsed_blocks,
        chain_reader,
        transparent_prevouts,
        stored_prevout_lookup,
    )
}

fn build_block_contexts_from_parsed_blocks(
    parsed_blocks: Vec<ParsedReplayBlock>,
    chain_reader: Option<&ChainEpochReader<'_>>,
    transparent_prevouts: &[TransparentPrevoutArtifact],
    stored_prevout_lookup: TransparentPrevoutLookupMode,
) -> Result<BatchBlockContexts, IngestError> {
    let resolve_started_at = Instant::now();
    let prevouts_outcome = resolve_prevouts_for_blocks(
        chain_reader,
        transparent_prevouts,
        &parsed_blocks,
        stored_prevout_lookup,
    );
    record_derive_context_stage(
        DERIVE_CONTEXT_STAGE_RESOLVE_PREVOUTS,
        resolve_started_at,
        &prevouts_outcome,
    );
    let prevout_artifacts = prevouts_outcome?;
    let prevouts = Arc::new(
        prevout_artifacts
            .iter()
            .map(|(outpoint, artifact)| (*outpoint, artifact.clone().into_prevout()))
            .collect::<HashMap<_, _>>(),
    );
    let mut out = HashMap::with_capacity(parsed_blocks.len());
    for parsed in parsed_blocks {
        let context = BlockCommitContext::new(
            BlockCommitPayload {
                height: parsed.height,
                block_hash: encode_block_hash(parsed.block_hash),
                previous_block_hash: encode_block_hash(parsed.previous_block_hash),
                raw_block_size_bytes: parsed.raw_block_size_bytes,
                block: parsed.block,
            },
            PrevoutResolver::from_map(Arc::clone(&prevouts)),
        );
        out.insert(parsed.height, Arc::new(context));
    }
    Ok(BatchBlockContexts {
        blocks: out,
        prevouts_by_outpoint: prevout_artifacts,
    })
}

async fn parse_replay_blocks(
    finalized_blocks: Vec<BlockArtifact>,
    derive_concurrency: NonZeroU32,
) -> Result<Vec<ParsedReplayBlock>, IngestError> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "zinder-core rejects targets with pointer widths below 32 bits, so u32 fits in usize"
    )]
    let parse_concurrency = derive_concurrency.get() as usize;
    let block_count = finalized_blocks.len();
    let mut parse_stream = futures_util::stream::iter(finalized_blocks)
        .map(|finalized_block| {
            tokio::task::spawn_blocking(move || parse_block_artifact(&finalized_block))
        })
        .buffered(parse_concurrency);
    let mut out = Vec::with_capacity(block_count);

    while let Some(parse_outcome) = parse_stream.next().await {
        let parsed_block =
            parse_outcome.map_err(|join_error| IngestError::BlockingTaskFailed {
                reason: join_error.to_string(),
            })??;
        out.push(parsed_block);
    }
    Ok(out)
}

struct ParsedReplayBlock {
    height: BlockHeight,
    block_hash: BlockHash,
    previous_block_hash: BlockHash,
    raw_block_size_bytes: usize,
    block: Arc<ZebraBlock>,
    transparent_outpoints: HashSet<TransparentOutPoint>,
}

fn parsed_blocks_for_commit(
    finalized_blocks: &[BlockArtifact],
    cached_blocks: &[Option<Arc<ZebraBlock>>],
) -> Result<Vec<ParsedReplayBlock>, IngestError> {
    if finalized_blocks.len() == cached_blocks.len() && cached_blocks.iter().all(Option::is_some) {
        return finalized_blocks
            .iter()
            .zip(cached_blocks)
            .map(|(finalized, cached)| {
                let block = Arc::clone(cached.as_ref().ok_or_else(|| {
                    IngestError::DeriveDispatch("cached parsed block is unavailable".to_owned())
                })?);
                Ok(parsed_block_from_cached(finalized, block))
            })
            .collect();
    }

    finalized_blocks
        .iter()
        .map(parse_block_artifact)
        .collect::<Result<Vec<_>, _>>()
}

fn parsed_block_from_cached(
    finalized: &BlockArtifact,
    block: Arc<ZebraBlock>,
) -> ParsedReplayBlock {
    let transparent_outpoints = transparent_outpoints_for_block(&block);
    ParsedReplayBlock {
        height: finalized.height,
        block_hash: finalized.block_hash,
        previous_block_hash: finalized.parent_hash,
        raw_block_size_bytes: finalized.payload_bytes.len(),
        block,
        transparent_outpoints,
    }
}

fn parse_block_artifact(finalized: &BlockArtifact) -> Result<ParsedReplayBlock, IngestError> {
    let block: ZebraBlock = finalized
        .payload_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| IngestError::DeriveDispatch(format!("block parse: {error}")))?;
    Ok(parsed_block_from_cached(finalized, Arc::new(block)))
}

fn transparent_outpoints_for_block(block: &ZebraBlock) -> HashSet<TransparentOutPoint> {
    let mut outpoints = HashSet::new();
    for (position, transaction) in block.transactions.iter().enumerate() {
        if position == 0 {
            continue;
        }
        for input in transaction.inputs() {
            if let transparent::Input::PrevOut { outpoint, .. } = input {
                let outpoint = TransparentOutPoint::new(
                    TransactionId::from_bytes(outpoint.hash.0),
                    outpoint.index,
                );
                if !outpoint.is_coinbase_sentinel() {
                    outpoints.insert(outpoint);
                }
            }
        }
    }
    outpoints
}

fn resolve_prevouts_for_blocks(
    chain_reader: Option<&ChainEpochReader<'_>>,
    transparent_prevouts: &[TransparentPrevoutArtifact],
    parsed_blocks: &[ParsedReplayBlock],
    stored_prevout_lookup: TransparentPrevoutLookupMode,
) -> Result<Arc<HashMap<TransparentOutPoint, TransparentPrevoutArtifact>>, IngestError> {
    let mut requested_outpoints = HashSet::<TransparentOutPoint>::new();
    for block in parsed_blocks {
        for outpoint in &block.transparent_outpoints {
            requested_outpoints.insert(*outpoint);
        }
    }

    let unique_prevout_count = requested_outpoints.len();
    record_prevout_resolution_requested_outpoints(unique_prevout_count);
    let in_batch_prevouts = transparent_prevouts
        .iter()
        .map(|prevout| (prevout.outpoint, prevout))
        .collect::<HashMap<_, _>>();
    let mut resolved = HashMap::with_capacity(unique_prevout_count);
    let mut unresolved_store_outpoints = Vec::new();
    for outpoint in requested_outpoints {
        if let Some(prevout) = in_batch_prevouts.get(&outpoint) {
            resolved.insert(outpoint, (*prevout).clone());
        } else {
            unresolved_store_outpoints.push(outpoint);
        }
    }
    record_prevout_resolution_count("in_batch", resolved.len());

    let Some(reader) = chain_reader else {
        record_prevout_resolution_count(
            "unresolved",
            unique_prevout_count.saturating_sub(resolved.len()),
        );
        return Ok(Arc::new(resolved));
    };

    let indexed_count = resolve_indexed_prevouts(
        reader,
        stored_prevout_lookup,
        &unresolved_store_outpoints,
        &mut resolved,
    )?;
    record_prevout_resolution_count("indexed_prevout", indexed_count);

    record_prevout_resolution_count(
        "unresolved",
        unique_prevout_count.saturating_sub(resolved.len()),
    );

    Ok(Arc::new(resolved))
}

fn resolve_indexed_prevouts(
    reader: &ChainEpochReader<'_>,
    stored_prevout_lookup: TransparentPrevoutLookupMode,
    outpoints: &[TransparentOutPoint],
    resolved: &mut HashMap<TransparentOutPoint, TransparentPrevoutArtifact>,
) -> Result<usize, IngestError> {
    let resolved_before_index = resolved.len();
    let prevouts_by_outpoint = read_chunked_transparent_prevouts_by_outpoints(
        reader,
        stored_prevout_lookup,
        TransparentPrevoutLookupStage::DeriveContext,
        outpoints,
    )?;
    resolved.extend(prevouts_by_outpoint);
    Ok(resolved.len().saturating_sub(resolved_before_index))
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

fn record_derive_context_stage<T>(
    stage: &'static str,
    started_at: Instant,
    outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_derive_context_stage_duration_seconds",
        "stage" => stage,
        "status" => outcome_status(outcome),
        "error_class" => ingest_error_class(outcome.as_ref().err())
    )
    .record(started_at.elapsed());
}

fn record_prevout_resolution_count(source: &'static str, count: usize) {
    if count == 0 {
        return;
    }
    metrics::counter!(
        "zinder_ingest_prevout_resolution_total",
        "source" => source
    )
    .increment(usize_to_u64_saturating(count));
}

fn record_prevout_resolution_requested_outpoints(count: usize) {
    metrics::histogram!("zinder_ingest_prevout_resolution_requested_outpoint_count")
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

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).unwrap_or(u32::MAX)
}

fn encode_block_hash(hash: BlockHash) -> Vec<u8> {
    hash.as_bytes().to_vec()
}
