use std::time::Instant;

use futures_util::{Stream, StreamExt};
use zinder_core::{BlockId, ChainEpoch, ChainEpochId, Network, TreeStateArtifact};
use zinder_source::NodeSource;
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochCommitOutcome, PrimaryChainStore, ReorgWindowChange,
};

use super::block_prepare::CanonicalBlockCommitPreparation;
use super::flush::flush_pending_bulk_catchup_writes;
use super::{
    BULK_STAGE_CANONICAL_BLOCK_PREPARE, BULK_STAGE_CANONICAL_COMMIT, BULK_STAGE_CANONICAL_POSITION,
    BULK_STAGE_CHECKPOINT_TREE_STATE, BULK_STAGE_COMMIT_REASSEMBLY,
    BULK_STAGE_SUBTREE_ROOT_ATTACHMENT, BulkCatchupCompletionFlush, BulkCatchupFlushState,
    BulkCatchupRunConfig, BulkCatchupRunContext, BulkCatchupStart, nonzero_u64_to_usize,
    record_bulk_pipeline_stage_duration, usize_to_u64_saturating,
};
use crate::artifact_builder::{CommitmentTreeSizes, position_canonical_block};
use crate::chain_ingest::{
    CanonicalBatch, CanonicalBatchBudget, CanonicalBatchCloseTrigger, CanonicalBatchCost,
    IngestError, IngestRetryState, IngestSubtreeRootIndexes, commit_ingest_batch,
    current_unix_millis, next_chain_epoch_id, next_chain_epoch_id_after,
    observe_final_note_commitment_roots, populate_subtree_root_artifacts,
    record_ingest_batch_commit_trigger, record_ingest_batch_work_cost,
};
use crate::writer::construction::{
    abort_on_drop::AbortOnDropTask,
    watermark::{record_queue_depth, record_reorder_buffer},
};

#[allow(
    clippy::too_many_lines,
    reason = "bulk catchup orchestration keeps the ordered positioning, in-flight commit, and flush-state transitions visible in one state machine"
)]
pub(super) async fn run_commit_reassembly<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    block_prepare_stream: impl Stream<Item = Result<Vec<CanonicalBlockCommitPreparation>, IngestError>>
    + Send,
    bulk_catchup_start: BulkCatchupStart,
    flush_state: &mut BulkCatchupFlushState,
    completion_flush: BulkCatchupCompletionFlush,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource + Clone,
{
    let mut chain_epoch_id = next_chain_epoch_id(run.store)?;
    let mut batch = CanonicalBatch::default();
    let mut next_subtree_root_indexes =
        IngestSubtreeRootIndexes::from_tip_metadata(bulk_catchup_start.initial_tip_metadata);
    let mut last_commit_outcome = None;
    let mut retry_state = Some(IngestRetryState::default());
    let mut loop_flush_state = Some(std::mem::take(flush_state));
    let mut in_flight_commit = None;
    let mut running_tree_sizes =
        CommitmentTreeSizes::from_tip_metadata(bulk_catchup_start.initial_tip_metadata);
    let batch_budget = CanonicalBatchBudget::new(
        run.config.canonical_batch_max_blocks,
        run.config.canonical_batch_max_artifact_bytes,
        run.config.canonical_batch_max_estimated_write_bytes,
        run.config
            .canonical_batch_min_blocks_before_estimated_write_close,
    );
    futures_util::pin_mut!(block_prepare_stream);

    loop {
        if in_flight_commit.is_some()
            && commit_reassembly_should_wait(run.config, batch.work_cost())
        {
            record_commit_reassembly_blocked();
            if let Err(error) = wait_for_in_flight_canonical_commit(
                &mut in_flight_commit,
                &mut next_subtree_root_indexes,
                &mut retry_state,
                &mut loop_flush_state,
                &mut last_commit_outcome,
            )
            .await
            {
                restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
        }

        let await_block_prepare_started_at = Instant::now();
        let Some(block_prepare_result) = block_prepare_stream.next().await else {
            break;
        };
        record_bulk_pipeline_stage_duration(
            BULK_STAGE_CANONICAL_BLOCK_PREPARE,
            await_block_prepare_started_at,
            block_prepare_result.as_ref().err(),
        );
        let position_started_at = Instant::now();
        let prepared_chunk = match block_prepare_result {
            Ok(prepared_chunk) => prepared_chunk,
            Err(error) => {
                if let Err(commit_error) = wait_for_in_flight_canonical_commit(
                    &mut in_flight_commit,
                    &mut next_subtree_root_indexes,
                    &mut retry_state,
                    &mut loop_flush_state,
                    &mut last_commit_outcome,
                )
                .await
                {
                    restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                    return Err(commit_error);
                }
                restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
        };

        for prepared in prepared_chunk {
            let CanonicalBlockCommitPreparation {
                prepared,
                prefetched_spent_transparent_outputs,
                block_prepare_reservation,
            } = prepared;
            let block = match position_canonical_block(prepared, &mut running_tree_sizes)
                .map_err(IngestError::from)
            {
                Ok(block) => block,
                Err(error) => {
                    record_bulk_pipeline_stage_duration(
                        BULK_STAGE_CANONICAL_POSITION,
                        position_started_at,
                        Some(&error),
                    );
                    if let Err(commit_error) = wait_for_in_flight_canonical_commit(
                        &mut in_flight_commit,
                        &mut next_subtree_root_indexes,
                        &mut retry_state,
                        &mut loop_flush_state,
                        &mut last_commit_outcome,
                    )
                    .await
                    {
                        restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                        return Err(commit_error);
                    }
                    restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                    return Err(error);
                }
            };
            let next_block_cost =
                CanonicalBatch::work_cost_for_block(&block, &prefetched_spent_transparent_outputs);
            if let Some(commit_trigger) =
                batch_budget.commit_trigger_before_next_block(batch.work_cost(), next_block_cost)
                && let Err(error) = close_canonical_batch_for_budget(
                    run,
                    &mut batch,
                    &mut in_flight_commit,
                    &mut next_subtree_root_indexes,
                    &mut retry_state,
                    &mut loop_flush_state,
                    &mut last_commit_outcome,
                    &mut chain_epoch_id,
                    commit_trigger,
                )
                .await
            {
                restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
            if let Err(error) = batch
                .absorb_with_prefetched_spent_outputs(block, prefetched_spent_transparent_outputs)
            {
                restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                return Err(error.into());
            }
            drop(block_prepare_reservation);

            if let Some(commit_trigger) = batch_budget.commit_trigger(batch.work_cost())
                && let Err(error) = close_canonical_batch_for_budget(
                    run,
                    &mut batch,
                    &mut in_flight_commit,
                    &mut next_subtree_root_indexes,
                    &mut retry_state,
                    &mut loop_flush_state,
                    &mut last_commit_outcome,
                    &mut chain_epoch_id,
                    commit_trigger,
                )
                .await
            {
                restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
                return Err(error);
            }
        }
        record_bulk_pipeline_stage_duration(
            BULK_STAGE_CANONICAL_POSITION,
            position_started_at,
            None,
        );
        record_ingest_batch_work_cost(batch.work_cost());
        record_commit_reassembly_state(&batch);
    }

    if let Err(error) = wait_for_in_flight_canonical_commit(
        &mut in_flight_commit,
        &mut next_subtree_root_indexes,
        &mut retry_state,
        &mut loop_flush_state,
        &mut last_commit_outcome,
    )
    .await
    {
        restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
        return Err(error);
    }

    if !batch.is_empty() {
        let commit_retry_state = retry_state
            .take()
            .ok_or(IngestError::BulkCatchupProducedNoCommit)?;
        let commit_flush_state = loop_flush_state
            .take()
            .ok_or(IngestError::BulkCatchupProducedNoCommit)?;
        in_flight_commit = Some(commit_canonical_batch_with_attachments(
            run,
            CanonicalCommitRequest {
                batch,
                next_subtree_root_indexes,
                retry_state: commit_retry_state,
                flush_state: commit_flush_state,
                chain_epoch_id,
            },
        ));
        if let Err(error) = wait_for_in_flight_canonical_commit(
            &mut in_flight_commit,
            &mut next_subtree_root_indexes,
            &mut retry_state,
            &mut loop_flush_state,
            &mut last_commit_outcome,
        )
        .await
        {
            restore_bulk_catchup_flush_state(flush_state, &mut loop_flush_state);
            return Err(error);
        }
    }

    let mut restored_flush_state = loop_flush_state
        .take()
        .ok_or(IngestError::BulkCatchupProducedNoCommit)?;
    if completion_flush.flushes_pending()
        && last_commit_outcome.is_some()
        && let Err(error) =
            flush_pending_bulk_catchup_writes(run.store, &mut restored_flush_state).await
    {
        *flush_state = restored_flush_state;
        return Err(error);
    }
    *flush_state = restored_flush_state;

    last_commit_outcome.ok_or(IngestError::BulkCatchupProducedNoCommit)
}

type InFlightCanonicalCommit =
    AbortOnDropTask<Result<CommittedCanonicalBatch, CanonicalCommitFailure>>;

/// Owned handles moved into a spawned canonical-commit task.
///
/// The commit runs on its own runtime task so the next batch keeps
/// accumulating while it attaches metadata and writes; a spawned future must
/// be `'static`, so it cannot borrow the run context.
struct CommitDependencies<Source> {
    config: BulkCatchupRunConfig,
    source: Source,
    store: PrimaryChainStore,
}

struct CommittedCanonicalBatch {
    commit_outcome: ChainEpochCommitOutcome,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: IngestRetryState,
    flush_state: BulkCatchupFlushState,
}

struct CanonicalCommitFailure {
    error: IngestError,
    retry_state: IngestRetryState,
    flush_state: BulkCatchupFlushState,
}

struct CanonicalCommitRequest {
    batch: CanonicalBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: IngestRetryState,
    flush_state: BulkCatchupFlushState,
    chain_epoch_id: ChainEpochId,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the ordered batch-close transition keeps all owned recovery state explicit"
)]
async fn close_canonical_batch_for_budget<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    batch: &mut CanonicalBatch,
    in_flight_commit: &mut Option<InFlightCanonicalCommit>,
    next_subtree_root_indexes: &mut IngestSubtreeRootIndexes,
    retry_state: &mut Option<IngestRetryState>,
    loop_flush_state: &mut Option<BulkCatchupFlushState>,
    last_commit_outcome: &mut Option<ChainEpochCommitOutcome>,
    chain_epoch_id: &mut ChainEpochId,
    commit_trigger: CanonicalBatchCloseTrigger,
) -> Result<(), IngestError>
where
    Source: NodeSource + Clone,
{
    wait_for_in_flight_canonical_commit(
        in_flight_commit,
        next_subtree_root_indexes,
        retry_state,
        loop_flush_state,
        last_commit_outcome,
    )
    .await?;
    let batch_cost = batch.work_cost();
    record_ingest_batch_work_cost(batch_cost);
    record_canonical_batch_commit_trigger(run.config, batch_cost, commit_trigger);
    let commit_batch = std::mem::take(batch);
    let commit_retry_state = retry_state
        .take()
        .ok_or(IngestError::BulkCatchupProducedNoCommit)?;
    let commit_flush_state = loop_flush_state
        .take()
        .ok_or(IngestError::BulkCatchupProducedNoCommit)?;
    *in_flight_commit = Some(commit_canonical_batch_with_attachments(
        run,
        CanonicalCommitRequest {
            batch: commit_batch,
            next_subtree_root_indexes: *next_subtree_root_indexes,
            retry_state: commit_retry_state,
            flush_state: commit_flush_state,
            chain_epoch_id: *chain_epoch_id,
        },
    ));
    record_canonical_commit_active(true);
    record_commit_reassembly_state(batch);
    *chain_epoch_id = next_chain_epoch_id_after(*chain_epoch_id)?;
    Ok(())
}

fn commit_canonical_batch_with_attachments<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    request: CanonicalCommitRequest,
) -> InFlightCanonicalCommit
where
    Source: NodeSource + Clone,
{
    let deps = CommitDependencies {
        config: run.config.clone(),
        source: run.source.clone(),
        store: run.store.clone(),
    };
    AbortOnDropTask::spawn(commit_canonical_batch(deps, request))
}

async fn commit_canonical_batch<Source>(
    deps: CommitDependencies<Source>,
    request: CanonicalCommitRequest,
) -> Result<CommittedCanonicalBatch, CanonicalCommitFailure>
where
    Source: NodeSource,
{
    let run = BulkCatchupRunContext::new(&deps.config, &deps.source, &deps.store);
    let mut batch = request.batch;
    let mut retry_state = request.retry_state;
    let mut flush_state = request.flush_state;
    let updated_subtree_root_indexes = match populate_bulk_catchup_subtree_roots(
        &run,
        &mut batch,
        request.next_subtree_root_indexes,
        &mut retry_state,
    )
    .await
    {
        Ok(updated_subtree_root_indexes) => updated_subtree_root_indexes,
        Err(error) => {
            return Err(CanonicalCommitFailure {
                error,
                retry_state,
                flush_state,
            });
        }
    };

    populate_bulk_catchup_tree_state_checkpoint(&run, &mut batch).await;

    let commit_outcome = match commit_built_bulk_catchup_batch(
        run.store,
        run.config.node.network,
        request.chain_epoch_id,
        batch,
    )
    .await
    {
        Ok((commit_outcome, _drained_batch)) => commit_outcome,
        Err(error) => {
            return Err(CanonicalCommitFailure {
                error,
                retry_state,
                flush_state,
            });
        }
    };

    flush_state.record_committed_epoch();
    if let Err(error) = flush_canonical_writes_if_due(&run, &mut flush_state).await {
        return Err(CanonicalCommitFailure {
            error,
            retry_state,
            flush_state,
        });
    }

    Ok(CommittedCanonicalBatch {
        commit_outcome,
        next_subtree_root_indexes: updated_subtree_root_indexes,
        retry_state,
        flush_state,
    })
}

async fn populate_bulk_catchup_tree_state_checkpoint<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    batch: &mut CanonicalBatch,
) where
    Source: NodeSource,
{
    let started_at = Instant::now();
    if !run
        .source
        .capabilities()
        .supports(zinder_source::NodeCapability::TreeState)
    {
        return;
    }
    let existing_heights = batch
        .tree_states
        .iter()
        .map(|tree_state| tree_state.height)
        .collect::<std::collections::HashSet<_>>();
    let mut checkpoint_targets = batch
        .block_headers
        .iter()
        .filter(|header| {
            header.height.value() % zinder_store::TREE_STATE_CHECKPOINT_STRIDE == 0
                && !existing_heights.contains(&header.height)
        })
        .map(|header| header.height)
        .collect::<std::collections::HashSet<_>>();
    if let Some(tip) = batch.block_headers.last()
        && !existing_heights.contains(&tip.height)
    {
        checkpoint_targets.insert(tip.height);
    }
    let mut targets = batch
        .block_headers
        .iter()
        .filter(|header| checkpoint_targets.contains(&header.height))
        .map(|header| BlockId::new(header.height, header.block_hash))
        .collect::<Vec<_>>();
    targets.sort_unstable_by_key(|block_id| block_id.height);
    for block_id in targets {
        let Some(tree_state) = observe_final_note_commitment_roots(
            run.config.node.request_timeout,
            run.source,
            block_id,
        )
        .await
        else {
            continue;
        };
        batch.push_final_note_commitment_roots(tree_state.final_note_commitment_roots);
        batch.push_tree_state_checkpoint(TreeStateArtifact::new(
            tree_state.block_id.height,
            tree_state.block_id.hash,
            tree_state.payload_bytes,
        ));
    }
    record_bulk_pipeline_stage_duration(BULK_STAGE_CHECKPOINT_TREE_STATE, started_at, None);
}

async fn wait_for_in_flight_canonical_commit(
    in_flight_commit: &mut Option<InFlightCanonicalCommit>,
    next_subtree_root_indexes: &mut IngestSubtreeRootIndexes,
    retry_state: &mut Option<IngestRetryState>,
    flush_state: &mut Option<BulkCatchupFlushState>,
    last_commit_outcome: &mut Option<ChainEpochCommitOutcome>,
) -> Result<(), IngestError> {
    let Some(commit) = in_flight_commit.take() else {
        return Ok(());
    };
    match commit.join().await {
        Ok(Ok(committed_batch)) => {
            apply_committed_canonical_batch(
                committed_batch,
                next_subtree_root_indexes,
                retry_state,
                flush_state,
                last_commit_outcome,
            );
            record_canonical_commit_active(false);
            Ok(())
        }
        Ok(Err(failure)) => {
            *retry_state = Some(failure.retry_state);
            *flush_state = Some(failure.flush_state);
            record_canonical_commit_active(false);
            Err(failure.error)
        }
        Err(join_error) => {
            record_canonical_commit_active(false);
            Err(IngestError::BlockingTaskFailed {
                reason: join_error.to_string(),
            })
        }
    }
}

fn apply_committed_canonical_batch(
    committed_batch: CommittedCanonicalBatch,
    next_subtree_root_indexes: &mut IngestSubtreeRootIndexes,
    retry_state: &mut Option<IngestRetryState>,
    flush_state: &mut Option<BulkCatchupFlushState>,
    last_commit_outcome: &mut Option<ChainEpochCommitOutcome>,
) {
    *next_subtree_root_indexes = committed_batch.next_subtree_root_indexes;
    *retry_state = Some(committed_batch.retry_state);
    *flush_state = Some(committed_batch.flush_state);
    *last_commit_outcome = Some(committed_batch.commit_outcome);
}

fn restore_bulk_catchup_flush_state(
    target: &mut BulkCatchupFlushState,
    source: &mut Option<BulkCatchupFlushState>,
) {
    if let Some(flush_state) = source.take() {
        *target = flush_state;
    }
}

fn commit_reassembly_should_wait(
    config: &BulkCatchupRunConfig,
    batch_cost: CanonicalBatchCost,
) -> bool {
    batch_cost.blocks > 0
        && batch_cost.artifact_bytes
            >= nonzero_u64_to_usize(config.commit_reassembly_max_queued_artifact_bytes)
}

fn record_commit_reassembly_state(batch: &CanonicalBatch) {
    let batch_cost = batch.work_cost();
    record_queue_depth(BULK_STAGE_COMMIT_REASSEMBLY, batch_cost.blocks);
    record_reorder_buffer(
        BULK_STAGE_COMMIT_REASSEMBLY,
        batch_cost.blocks,
        usize_to_u64_saturating(batch_cost.artifact_bytes),
    );
}

fn record_commit_reassembly_blocked() {
    metrics::counter!(
        "zinder_ingest_bulk_pipeline_watermark_blocked_total",
        "stage" => BULK_STAGE_COMMIT_REASSEMBLY
    )
    .increment(1);
}

fn record_canonical_commit_active(is_active: bool) {
    let active = if is_active { 1.0 } else { 0.0 };
    metrics::gauge!(
        "zinder_ingest_bulk_pipeline_active",
        "stage" => BULK_STAGE_CANONICAL_COMMIT
    )
    .set(active);
}

async fn flush_canonical_writes_if_due<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    flush_state: &mut BulkCatchupFlushState,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    if flush_state.should_flush(run.config.flush_interval_epochs) {
        flush_pending_bulk_catchup_writes(run.store, flush_state).await?;
    }
    Ok(())
}

async fn populate_bulk_catchup_subtree_roots<Source>(
    run: &BulkCatchupRunContext<'_, Source>,
    batch: &mut CanonicalBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: &mut IngestRetryState,
) -> Result<IngestSubtreeRootIndexes, IngestError>
where
    Source: NodeSource,
{
    let started_at = Instant::now();
    let outcome = populate_subtree_root_artifacts(
        run.config.node.request_timeout,
        run.source,
        batch,
        next_subtree_root_indexes,
        retry_state,
    )
    .await;
    record_bulk_pipeline_stage_duration(
        BULK_STAGE_SUBTREE_ROOT_ATTACHMENT,
        started_at,
        outcome.as_ref().err(),
    );
    outcome
}

fn record_canonical_batch_commit_trigger(
    config: &BulkCatchupRunConfig,
    batch_cost: CanonicalBatchCost,
    commit_trigger: CanonicalBatchCloseTrigger,
) {
    record_ingest_batch_commit_trigger(commit_trigger);
    tracing::info!(
        target: "zinder::ingest",
        event = "bulk_catchup_batch_budget_reached",
        trigger = commit_trigger.metric_label(),
        block_count = batch_cost.blocks,
        transaction_count = batch_cost.transactions,
        transparent_output_count = batch_cost.transparent_outputs,
        transparent_spend_reference_count = batch_cost.transparent_spend_references,
        estimated_write_bytes = batch_cost.estimated_write_bytes,
        max_blocks = config.canonical_batch_max_blocks.get(),
        "bulk-catchup batch budget reached; committing accumulated artifacts"
    );
}

/// Commits a built bulk catchup batch and returns the drained batch buffer.
async fn commit_built_bulk_catchup_batch(
    store: &PrimaryChainStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: CanonicalBatch,
) -> Result<(ChainEpochCommitOutcome, CanonicalBatch), IngestError> {
    let mut batch = batch;
    let outcome =
        commit_built_bulk_catchup_batch_inner(store, network, chain_epoch_id, &mut batch).await?;
    Ok((outcome, batch))
}

async fn commit_built_bulk_catchup_batch_inner(
    store: &PrimaryChainStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: &mut CanonicalBatch,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let tip_block = batch
        .block_headers
        .last()
        .ok_or(IngestError::EmptyCanonicalBatch)?;
    let tip_height = tip_block.height;
    let tip_hash = tip_block.block_hash;
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyCanonicalBatch)?;
    let chain_epoch = ChainEpoch {
        id: chain_epoch_id,
        network,
        visible_tip_height: tip_height,
        visible_tip_hash: tip_hash,
        settled_tip_height: tip_height,
        settled_tip_hash: tip_hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata,
        created_at: current_unix_millis()?,
    };
    let commit_started_at = Instant::now();
    let commit_outcome = commit_ingest_batch(
        store,
        chain_epoch,
        batch,
        ReorgWindowChange::AdvanceSafeTipTo { height: tip_height },
    )
    .await;
    record_bulk_pipeline_stage_duration(
        BULK_STAGE_CANONICAL_COMMIT,
        commit_started_at,
        commit_outcome.as_ref().err(),
    );
    commit_outcome
}
