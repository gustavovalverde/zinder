//! Resumable historical backfill and startup-tail seeding for transaction
//! component summaries.

use std::{collections::HashSet, num::NonZeroU32, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeaderArtifact, BlockHeight, CanonicalHistoryBounds, TransactionId};
use zinder_derive::{
    BlockCommitContext, BlockCommitPayload, DeriveStore, TransactionComponentBackfillCoverage,
    TransactionComponentSummaryConsumer, TransactionIntrinsicValueBalanceFacts,
    TransparentSpendFacts,
};
use zinder_store::PrimaryChainStore;

use crate::{
    IngestError,
    derive_consumers::derive_projection_write_guard,
    loop_config::{HistoricalWorkGate, wait_until_historical_work_or_cancelled},
};

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const BACKFILL_CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Seeds the canonical visible range already covered by the event cursor that
/// a newly added transaction-component consumer inherits at startup.
///
/// The tail boundary is initialized separately before this call. Batches are
/// cursor-neutral, resumable after a crash, and remain owned by normal reorg
/// events once ingest starts.
pub(crate) fn seed_transaction_component_visible_tail(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    through_height: BlockHeight,
    batch_blocks: NonZeroU32,
) -> Result<(), IngestError> {
    loop {
        let tail =
            TransactionComponentSummaryConsumer::tail_coverage(derive_store)?.ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "transaction-component tail boundary is missing during startup seeding"
                        .to_owned(),
                )
            })?;
        let next_height = tail
            .complete_through_height
            .map_or(Some(tail.boundary_height), BlockHeight::next)
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "transaction-component startup tail height overflow".to_owned(),
                )
            })?;
        if next_height > through_height {
            return Ok(());
        }
        let batch_end = BlockHeight::new(
            next_height
                .value()
                .saturating_add(batch_blocks.get().saturating_sub(1))
                .min(through_height.value()),
        );
        let contexts = read_canonical_context_batch(chain_store, next_height, batch_end)?;
        let _write_guard = derive_projection_write_guard();
        TransactionComponentSummaryConsumer::new()
            .write_tail_seed_batch(derive_store, &contexts)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
}

/// Bounded controls for the ingest-owned transaction-component backfill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionComponentBackfillConfig {
    /// Whether the background task runs.
    pub enabled: bool,
    /// Maximum settled canonical blocks processed per durable coverage update.
    pub batch_blocks: NonZeroU32,
}

/// Existing storage handles used by the transaction-component backfill.
#[derive(Clone)]
pub struct TransactionComponentBackfillContext {
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl TransactionComponentBackfillContext {
    /// Groups the canonical and derive stores for task startup.
    #[must_use]
    pub fn new(chain_store: PrimaryChainStore, derive_store: DeriveStore) -> Self {
        Self {
            chain_store,
            derive_store,
        }
    }
}

/// Spawns the non-readiness-blocking settled-history backfill task.
#[must_use = "await the handle during shutdown"]
pub fn spawn_transaction_component_backfill_task(
    config: TransactionComponentBackfillConfig,
    context: TransactionComponentBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "transaction_component_backfill_disabled",
            "transaction-component historical backfill is disabled"
        );
        return None;
    }

    Some(tokio::spawn(run_transaction_component_backfill(
        config,
        context,
        historical_work_gate,
        cancel,
    )))
}

async fn run_transaction_component_backfill(
    config: TransactionComponentBackfillConfig,
    context: TransactionComponentBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) {
    tracing::info!(
        target: "zinder::ingest",
        event = "transaction_component_backfill_started",
        batch_blocks = config.batch_blocks.get(),
        "transaction-component historical backfill started"
    );

    loop {
        if wait_until_historical_work_or_cancelled(&historical_work_gate, &cancel).await {
            return;
        }
        let backfill = backfill_next_batch(config, context.clone());
        let progress = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_component_backfill_cancelled",
                    "transaction-component historical backfill cancelled"
                );
                return;
            }
            progress = backfill => progress,
        };

        match progress {
            Ok(BackfillProgress::Advanced {
                from_height,
                through_height,
                transaction_count,
            }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_component_backfill_progress",
                    from_height = from_height.value(),
                    through_height = through_height.value(),
                    transaction_count,
                    "transaction-component historical backfill advanced"
                );
            }
            Ok(BackfillProgress::CaughtUp { through_height }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "transaction_component_backfill_completed",
                    through_height = through_height.map(BlockHeight::value),
                    "transaction-component historical backfill is caught up to the settled tip"
                );
                if sleep_or_cancel(BACKFILL_CAUGHT_UP_POLL_INTERVAL, &cancel).await {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "transaction_component_backfill_retry",
                    error = %error,
                    retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                    "transaction-component historical backfill batch failed; retrying"
                );
                if sleep_or_cancel(BACKFILL_RETRY_INTERVAL, &cancel).await {
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackfillProgress {
    Advanced {
        from_height: BlockHeight,
        through_height: BlockHeight,
        transaction_count: usize,
    },
    CaughtUp {
        through_height: Option<BlockHeight>,
    },
}

async fn backfill_next_batch(
    config: TransactionComponentBackfillConfig,
    context: TransactionComponentBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    tokio::task::spawn_blocking(move || backfill_next_batch_blocking(config, &context))
        .await
        .map_err(|error| IngestError::BlockingTaskFailed {
            reason: error.to_string(),
        })?
}

fn backfill_next_batch_blocking(
    config: TransactionComponentBackfillConfig,
    context: &TransactionComponentBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    let coverage = TransactionComponentSummaryConsumer::backfill_coverage(&context.derive_store)?;
    let Some(chain_epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    };
    let history_bounds = canonical_history_bounds(&context.chain_store)?;
    let first_available_height = history_bounds.first_available_height();
    let next_height = next_backfill_height(coverage, first_available_height)?;
    let Some(target_height) = historical_backfill_target(
        chain_epoch.settled_tip_height,
        TransactionComponentSummaryConsumer::tail_coverage(&context.derive_store)?,
    ) else {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    };
    if next_height > target_height {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    }

    let batch_end = BlockHeight::new(
        next_height
            .value()
            .saturating_add(config.batch_blocks.get().saturating_sub(1))
            .min(target_height.value()),
    );
    let contexts = read_canonical_context_batch(&context.chain_store, next_height, batch_end)?;
    let transaction_count = contexts.iter().map(|block| block.transactions.len()).sum();
    let first_block_time = contexts
        .first()
        .ok_or_else(|| {
            IngestError::DeriveDispatch(
                "transaction-component backfill hydrated an empty batch".to_owned(),
            )
        })?
        .block_time_unix_seconds;
    let last_block_time = contexts
        .last()
        .ok_or_else(|| {
            IngestError::DeriveDispatch(
                "transaction-component backfill hydrated an empty batch".to_owned(),
            )
        })?
        .block_time_unix_seconds;
    let next_coverage = TransactionComponentBackfillCoverage::new(
        coverage.map_or(first_available_height, |coverage| {
            coverage.complete_from_height
        }),
        batch_end,
        coverage.map_or(first_block_time, |coverage| {
            coverage.complete_from_time_unix_seconds
        }),
        last_block_time,
    );
    let _write_guard = derive_projection_write_guard();
    TransactionComponentSummaryConsumer::new()
        .write_backfill_batch(&context.derive_store, &contexts, next_coverage)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;

    Ok(BackfillProgress::Advanced {
        from_height: next_height,
        through_height: batch_end,
        transaction_count,
    })
}

fn historical_backfill_target(
    settled_tip_height: BlockHeight,
    tail: Option<zinder_derive::TransactionComponentTailCoverage>,
) -> Option<BlockHeight> {
    let Some(tail) = tail else {
        return Some(settled_tip_height);
    };
    let height_before_tail = tail.boundary_height.value().checked_sub(1)?;
    Some(BlockHeight::new(
        settled_tip_height.value().min(height_before_tail),
    ))
}

fn next_backfill_height(
    coverage: Option<TransactionComponentBackfillCoverage>,
    first_available_height: BlockHeight,
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = coverage else {
        return Ok(first_available_height);
    };
    if coverage.complete_from_height != first_available_height {
        return Err(IngestError::DeriveDispatch(format!(
            "transaction-component backfill coverage starts at {}, expected {}",
            coverage.complete_from_height.value(),
            first_available_height.value()
        )));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::DeriveDispatch("transaction-component backfill height overflow".to_owned())
    })
}

struct StagedBlock {
    header: BlockHeaderArtifact,
    transaction_ids: Vec<TransactionId>,
}

pub(crate) fn read_canonical_context_batch(
    chain_store: &PrimaryChainStore,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<Vec<BlockCommitContext>, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    validate_visible_boundary(reader.chain_epoch().visible_tip_height, through_height)?;
    let history_bounds = reader.canonical_history_bounds();
    let (staged_blocks, batch_transaction_ids) =
        stage_canonical_blocks(&reader, &history_bounds, from_height, through_height)?;
    hydrate_staged_blocks(&reader, staged_blocks, &batch_transaction_ids)
}

pub(crate) fn canonical_history_bounds(
    chain_store: &PrimaryChainStore,
) -> Result<CanonicalHistoryBounds, IngestError> {
    chain_store.canonical_history_bounds()?.ok_or_else(|| {
        IngestError::DeriveDispatch("canonical history bounds are unavailable".to_owned())
    })
}

fn stage_canonical_blocks(
    reader: &zinder_store::ChainEpochReader<'_>,
    history_bounds: &CanonicalHistoryBounds,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<(Vec<StagedBlock>, Vec<TransactionId>), IngestError> {
    let chain_epoch = reader.chain_epoch();
    let mut staged_blocks = Vec::with_capacity(height_count(from_height, through_height));
    let mut batch_transaction_ids = Vec::new();
    let mut unique_transaction_ids = HashSet::new();
    let mut expected_parent_hash = read_predecessor_hash(reader, history_bounds, from_height)?;
    for height in inclusive_heights(from_height, through_height) {
        let header = reader.block_header_at(height)?.ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "canonical block header {} is unavailable",
                height.value()
            ))
        })?;
        if header.height != height {
            return Err(IngestError::DeriveDispatch(format!(
                "canonical block header {} reports height {}",
                height.value(),
                header.height.value()
            )));
        }
        if expected_parent_hash.is_some_and(|expected| header.parent_hash != expected) {
            return Err(IngestError::DeriveDispatch(format!(
                "settled canonical block header {} does not connect to its predecessor",
                height.value()
            )));
        }
        if height == chain_epoch.settled_tip_height
            && header.block_hash != chain_epoch.settled_tip_hash
        {
            return Err(IngestError::DeriveDispatch(format!(
                "settled canonical block header {} does not match the settled-tip hash",
                height.value()
            )));
        }
        if height == chain_epoch.visible_tip_height
            && header.block_hash != chain_epoch.visible_tip_hash
        {
            return Err(IngestError::DeriveDispatch(format!(
                "canonical block header {} does not match the visible-tip hash",
                height.value()
            )));
        }

        let transaction_ids = reader.transaction_ids_at_height(height)?;
        if transaction_ids.is_empty() {
            return Err(IngestError::DeriveDispatch(format!(
                "canonical transaction index {} is empty",
                height.value()
            )));
        }
        for transaction_id in &transaction_ids {
            if !unique_transaction_ids.insert(*transaction_id) {
                return Err(IngestError::DeriveDispatch(format!(
                    "canonical transaction index repeats transaction {}",
                    hex::encode(transaction_id.as_bytes())
                )));
            }
        }
        batch_transaction_ids.extend_from_slice(&transaction_ids);
        expected_parent_hash = Some(header.block_hash);
        staged_blocks.push(StagedBlock {
            header,
            transaction_ids,
        });
    }
    Ok((staged_blocks, batch_transaction_ids))
}

fn read_predecessor_hash(
    reader: &zinder_store::ChainEpochReader<'_>,
    history_bounds: &CanonicalHistoryBounds,
    from_height: BlockHeight,
) -> Result<Option<zinder_core::BlockHash>, IngestError> {
    let first_available_height = history_bounds.first_available_height();
    if from_height == first_available_height {
        return Ok(history_bounds
            .preceding_checkpoint()
            .map(|checkpoint| checkpoint.hash));
    }
    if from_height < first_available_height {
        return Err(IngestError::DeriveDispatch(format!(
            "canonical batch starts at {}, before the first available height {}",
            from_height.value(),
            first_available_height.value()
        )));
    }
    let predecessor_height =
        BlockHeight::new(from_height.value().checked_sub(1).ok_or_else(|| {
            IngestError::DeriveDispatch(
                "transaction-component backfill cannot validate a height-zero batch".to_owned(),
            )
        })?);
    let predecessor = reader.block_header_at(predecessor_height)?.ok_or_else(|| {
        IngestError::DeriveDispatch(format!(
            "canonical predecessor header {} is unavailable",
            predecessor_height.value()
        ))
    })?;
    if predecessor.height != predecessor_height {
        return Err(IngestError::DeriveDispatch(format!(
            "canonical predecessor header {} reports height {}",
            predecessor_height.value(),
            predecessor.height.value()
        )));
    }
    Ok(Some(predecessor.block_hash))
}

fn hydrate_staged_blocks(
    reader: &zinder_store::ChainEpochReader<'_>,
    staged_blocks: Vec<StagedBlock>,
    batch_transaction_ids: &[TransactionId],
) -> Result<Vec<BlockCommitContext>, IngestError> {
    let mut facts_by_id = reader.transaction_facts_by_ids(batch_transaction_ids)?;
    let intrinsic_by_id = reader
        .transaction_intrinsic_value_balances_by_ids(batch_transaction_ids)?
        .into_iter()
        .filter_map(|(transaction_id, artifact)| {
            artifact.map(|artifact| (transaction_id, artifact.value_balances))
        })
        .collect();
    let intrinsic_by_id = std::sync::Arc::new(intrinsic_by_id);
    let mut contexts = Vec::with_capacity(staged_blocks.len());
    for staged_block in staged_blocks {
        let mut transactions = Vec::with_capacity(staged_block.transaction_ids.len());
        for (transaction_index, transaction_id) in
            staged_block.transaction_ids.into_iter().enumerate()
        {
            let transaction = facts_by_id
                .remove(&transaction_id)
                .flatten()
                .ok_or_else(|| {
                    IngestError::DeriveDispatch(format!(
                        "canonical transaction facts {} are unavailable",
                        hex::encode(transaction_id.as_bytes())
                    ))
                })?;
            validate_transaction_fact(
                &staged_block.header,
                transaction_index,
                transaction_id,
                &transaction,
            )?;
            transactions.push(transaction);
        }
        contexts.push(
            BlockCommitContext::new(
                BlockCommitPayload {
                    height: staged_block.header.height,
                    block_hash: staged_block.header.block_hash,
                    previous_block_hash: staged_block.header.parent_hash,
                    block_time_unix_seconds: staged_block.header.block_time,
                    block_size_bytes: staged_block.header.block_size_bytes,
                    transactions,
                    final_note_commitment_roots: None,
                },
                TransparentSpendFacts::Offline,
            )
            .with_transaction_intrinsic_value_balances(
                TransactionIntrinsicValueBalanceFacts::from_map(std::sync::Arc::clone(
                    &intrinsic_by_id,
                )),
            ),
        );
    }
    Ok(contexts)
}

fn validate_transaction_fact(
    header: &BlockHeaderArtifact,
    transaction_index: usize,
    expected_transaction_id: TransactionId,
    transaction: &zinder_core::TransactionFactsArtifact,
) -> Result<(), IngestError> {
    let expected_index = u32::try_from(transaction_index).map_err(|_| {
        IngestError::DeriveDispatch(format!(
            "transaction index overflows u32 at canonical height {}",
            header.height.value()
        ))
    })?;
    if transaction.public_facts.transaction_id != expected_transaction_id
        || transaction.location.transaction_id != expected_transaction_id
        || transaction.location.block_height != header.height
        || transaction.location.block_hash != header.block_hash
        || transaction.location.tx_index_in_block != expected_index
    {
        return Err(IngestError::DeriveDispatch(format!(
            "canonical transaction facts {} do not match block {} index {}",
            hex::encode(expected_transaction_id.as_bytes()),
            header.height.value(),
            transaction_index
        )));
    }
    Ok(())
}

fn validate_visible_boundary(
    visible_tip_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<(), IngestError> {
    if through_height <= visible_tip_height {
        return Ok(());
    }
    Err(IngestError::DeriveDispatch(
        "transaction-component batch crossed the visible canonical boundary".to_owned(),
    ))
}

fn inclusive_heights(
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> impl Iterator<Item = BlockHeight> {
    (from_height.value()..=through_height.value()).map(BlockHeight::new)
}

fn height_count(from_height: BlockHeight, through_height: BlockHeight) -> usize {
    usize::try_from(
        through_height
            .value()
            .saturating_sub(from_height.value())
            .saturating_add(1),
    )
    .unwrap_or(usize::MAX)
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHeightRange, ChainEpochId, Network, TransactionId};
    use zinder_store::{ChainEpochArtifacts, ChainStoreOptions, ReorgWindowChange};
    use zinder_testkit::{ChainFixture, FixtureTransactionRows, encode_fixture_block_replay};

    use super::*;

    #[test]
    fn backfill_resumes_after_contiguous_coverage() -> Result<(), IngestError> {
        let first_available_height = BlockHeight::new(101);
        assert_eq!(
            next_backfill_height(None, first_available_height)?,
            first_available_height
        );
        assert_eq!(
            next_backfill_height(
                Some(TransactionComponentBackfillCoverage::new(
                    first_available_height,
                    BlockHeight::new(256),
                    1_600_000_000,
                    1_600_000_001,
                )),
                first_available_height
            )?,
            BlockHeight::new(257)
        );
        Ok(())
    }

    #[test]
    fn backfill_rejects_wrong_coverage_floor() {
        let result = next_backfill_height(
            Some(TransactionComponentBackfillCoverage::new(
                BlockHeight::new(100),
                BlockHeight::new(256),
                1_600_000_000,
                1_600_000_001,
            )),
            BlockHeight::new(101),
        );
        assert!(matches!(result, Err(IngestError::DeriveDispatch(_))));
    }

    #[test]
    fn canonical_batch_starts_after_an_artifactless_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(21);
        let checkpoint_block = chain
            .block_at(BlockHeight::new(20))
            .ok_or("checkpoint fixture block missing")?;
        let first_available_block = chain
            .block_at(BlockHeight::new(21))
            .ok_or("first available fixture block missing")?;
        let directory = tempfile::tempdir()?;
        let store =
            PrimaryChainStore::open(directory.path(), ChainStoreOptions::for_local_tests())?;
        let mut checkpoint_epoch = chain
            .chain_epoch(ChainEpochId::new(1))
            .ok_or("checkpoint fixture epoch missing")?;
        checkpoint_epoch.visible_tip_height = checkpoint_block.height;
        checkpoint_epoch.visible_tip_hash = checkpoint_block.hash;
        checkpoint_epoch.settled_tip_height = checkpoint_block.height;
        checkpoint_epoch.settled_tip_hash = checkpoint_block.hash;
        store.commit_artifactless_checkpoint(checkpoint_epoch)?;

        let transaction_rows = FixtureTransactionRows::from_raw_transaction(
            TransactionId::from_bytes([0x42; 32]),
            first_available_block.height,
            first_available_block.hash,
            0,
            [0x01],
        );
        let first_available_epoch = chain
            .chain_epoch(ChainEpochId::new(2))
            .ok_or("first available fixture epoch missing")?;
        let block_header = first_available_block.block_header_artifact();
        let replay_envelope =
            encode_fixture_block_replay(&block_header, std::slice::from_ref(&transaction_rows));
        let transaction_intrinsic_value_balances = transaction_rows
            .intrinsic_value_balances_artifact()
            .ok_or("fixture transaction intrinsic balances missing")?;
        store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                first_available_epoch,
                vec![block_header],
                vec![replay_envelope],
                vec![first_available_block.compact_block_artifact()],
            )
            .with_block_transaction_index(vec![transaction_rows.block_transaction_index])
            .with_transaction_locations(vec![transaction_rows.location])
            .with_transaction_facts(vec![transaction_rows.facts])
            .with_transaction_intrinsic_value_balances(vec![transaction_intrinsic_value_balances])
            .with_reorg_window_change(ReorgWindowChange::Extend {
                block_range: BlockHeightRange::inclusive(
                    first_available_block.height,
                    first_available_block.height,
                ),
            }),
        )?;

        let contexts = read_canonical_context_batch(
            &store,
            first_available_block.height,
            first_available_block.height,
        )?;

        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].height, first_available_block.height);
        assert_eq!(contexts[0].block_hash, first_available_block.hash);
        Ok(())
    }

    #[test]
    fn batch_rejects_heights_above_visible_boundary() {
        assert!(validate_visible_boundary(BlockHeight::new(199), BlockHeight::new(200)).is_err());
        assert!(validate_visible_boundary(BlockHeight::new(200), BlockHeight::new(200)).is_ok());
    }

    #[test]
    fn historical_backfill_stops_before_the_live_tail() {
        let tail = zinder_derive::TransactionComponentTailCoverage {
            boundary_height: BlockHeight::new(101),
            complete_through_height: Some(BlockHeight::new(110)),
            complete_through_time_unix_seconds: Some(1_700_000_000),
        };
        assert_eq!(
            historical_backfill_target(BlockHeight::new(105), Some(tail)),
            Some(BlockHeight::new(100))
        );
        assert_eq!(
            historical_backfill_target(BlockHeight::new(99), Some(tail)),
            Some(BlockHeight::new(99))
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_backfill_wait() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(sleep_or_cancel(Duration::from_mins(1), &cancel).await);
    }
}
