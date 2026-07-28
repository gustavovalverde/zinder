//! Resumable historical backfill and startup-tail seeding for ZIP-317
//! conventional-fee distribution.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use parking_lot::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, BlockHeightRange, NetworkUpgradeActivations};
use zinder_materialized_views::{
    BlockCommitContext, ConventionalFeeDistributionBackfillCoverage,
    ConventionalFeeDistributionConsumer, ConventionalFeeDistributionTailCoverage,
    MaterializedViewStore,
};
use zinder_store::RocksDbCanonicalSecondary;

use crate::{
    IngestError,
    canonical_block_context::{
        validate_canonical_activations_identity, validate_materialized_view_canonical_identity,
    },
    materialized_view_replay::materialized_view_write_guard,
    runtime_config::{
        HistoricalWorkGate, nonzero_u32, sleep_or_cancel, wait_until_historical_work_or_cancelled,
    },
    transaction_component_backfill::read_canonical_context_batch,
};

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const BACKFILL_CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);
const BACKFILL_BATCH_BLOCKS: NonZeroU32 = nonzero_u32(256);

pub(crate) fn seed_conventional_fee_distribution_visible_tail(
    canonical: &RocksDbCanonicalSecondary,
    activations: &NetworkUpgradeActivations,
    materialized_view_store: &MaterializedViewStore,
    through_height: BlockHeight,
    batch_blocks: NonZeroU32,
) -> Result<(), IngestError> {
    loop {
        let tail = ConventionalFeeDistributionConsumer::tail_coverage(materialized_view_store)?
            .ok_or_else(|| {
                IngestError::MaterializedViewDispatch(
                    "conventional-fee distribution tail boundary is missing during startup seeding"
                        .to_owned(),
                )
            })?;
        let next_height = tail
            .complete_through_height
            .map_or(Some(tail.boundary_height), BlockHeight::next)
            .ok_or_else(|| {
                IngestError::MaterializedViewDispatch(
                    "conventional-fee distribution startup tail height overflow".to_owned(),
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
        let contexts = read_canonical_context_batch(
            canonical,
            activations,
            BlockHeightRange::inclusive(next_height, batch_end),
        )?;
        let _write_guard = materialized_view_write_guard();
        ConventionalFeeDistributionConsumer::new()
            .write_tail_seed_batch(materialized_view_store, &contexts)
            .map_err(|error| IngestError::MaterializedViewDispatch(error.to_string()))?;
    }
}

/// Bounded controls for the ingest-owned conventional-fee distribution backfill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConventionalFeeDistributionBackfillConfig {
    /// Maximum canonical blocks processed per durable coverage update.
    pub batch_blocks: NonZeroU32,
}

impl ConventionalFeeDistributionBackfillConfig {
    /// Limits the ingest runtime runs the backfill with.
    pub const DEFAULT: Self = Self {
        batch_blocks: BACKFILL_BATCH_BLOCKS,
    };
}

/// Existing storage handles used by the conventional-fee distribution backfill.
#[derive(Clone)]
pub struct ConventionalFeeDistributionBackfillContext {
    canonical: Arc<RwLock<RocksDbCanonicalSecondary>>,
    activations: Arc<NetworkUpgradeActivations>,
    materialized_view_store: MaterializedViewStore,
}

impl ConventionalFeeDistributionBackfillContext {
    /// Groups the canonical secondary and materialized-view store for task startup.
    pub fn new(
        canonical: Arc<RwLock<RocksDbCanonicalSecondary>>,
        activations: Arc<NetworkUpgradeActivations>,
        materialized_view_store: MaterializedViewStore,
    ) -> Result<Self, IngestError> {
        {
            let canonical_guard = canonical.read();
            validate_canonical_activations_identity(&canonical_guard, &activations)?;
            validate_materialized_view_canonical_identity(
                &canonical_guard,
                &materialized_view_store,
            )?;
        }
        Ok(Self {
            canonical,
            activations,
            materialized_view_store,
        })
    }
}

/// Spawns the non-readiness-blocking historical backfill task.
#[must_use = "await the handle during shutdown"]
pub fn spawn_conventional_fee_distribution_backfill_task(
    config: ConventionalFeeDistributionBackfillConfig,
    context: ConventionalFeeDistributionBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(run_conventional_fee_distribution_backfill(
        config,
        context,
        historical_work_gate,
        cancel,
    ))
}

async fn run_conventional_fee_distribution_backfill(
    config: ConventionalFeeDistributionBackfillConfig,
    context: ConventionalFeeDistributionBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) {
    tracing::info!(
        target: "zinder::ingest",
        event = "conventional_fee_distribution_backfill_started",
        batch_blocks = config.batch_blocks.get(),
        "conventional-fee distribution historical backfill started"
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
                    event = "conventional_fee_distribution_backfill_cancelled",
                    "conventional-fee distribution historical backfill cancelled"
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
                    event = "conventional_fee_distribution_backfill_progress",
                    from_height = from_height.value(),
                    through_height = through_height.value(),
                    transaction_count,
                    "conventional-fee distribution historical backfill advanced"
                );
            }
            Ok(BackfillProgress::CaughtUp { through_height }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "conventional_fee_distribution_backfill_completed",
                    through_height = through_height.map(BlockHeight::value),
                    "conventional-fee distribution historical backfill is caught up"
                );
                if sleep_or_cancel(BACKFILL_CAUGHT_UP_POLL_INTERVAL, &cancel).await {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "conventional_fee_distribution_backfill_retry",
                    error = %error,
                    retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                    "conventional-fee distribution backfill batch failed; retrying"
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
    config: ConventionalFeeDistributionBackfillConfig,
    context: ConventionalFeeDistributionBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    tokio::task::spawn_blocking(move || backfill_next_batch_blocking(config, &context))
        .await
        .map_err(|error| IngestError::BlockingTaskFailed {
            reason: error.to_string(),
        })?
}

fn backfill_next_batch_blocking(
    config: ConventionalFeeDistributionBackfillConfig,
    context: &ConventionalFeeDistributionBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    let coverage =
        ConventionalFeeDistributionConsumer::backfill_coverage(&context.materialized_view_store)?;
    let batch = match read_next_backfill_batch(config, context, coverage)? {
        BackfillBatch::CaughtUp => {
            return Ok(BackfillProgress::CaughtUp {
                through_height: coverage.map(|coverage| coverage.complete_through_height),
            });
        }
        BackfillBatch::Ready(batch) => batch,
    };
    let transaction_count = batch
        .contexts
        .iter()
        .map(|block| block.transactions.len())
        .sum();
    let next_coverage = ConventionalFeeDistributionBackfillCoverage::new(
        coverage.map_or(batch.first_available_height, |coverage| {
            coverage.complete_from_height
        }),
        batch.through_height,
        coverage.map_or(batch.first_block_time_unix_seconds, |coverage| {
            coverage.complete_from_time_unix_seconds
        }),
        batch.last_block_time_unix_seconds,
    );
    let _write_guard = materialized_view_write_guard();
    ConventionalFeeDistributionConsumer::new()
        .write_backfill_batch(
            &context.materialized_view_store,
            &batch.contexts,
            next_coverage,
        )
        .map_err(|error| IngestError::MaterializedViewDispatch(error.to_string()))?;

    Ok(BackfillProgress::Advanced {
        from_height: batch.from_height,
        through_height: batch.through_height,
        transaction_count,
    })
}

/// Outcome of resolving the next settled range to backfill.
enum BackfillBatch {
    CaughtUp,
    Ready(HydratedBackfillBatch),
}

/// One hydrated settled range plus the coverage inputs it advances.
struct HydratedBackfillBatch {
    from_height: BlockHeight,
    through_height: BlockHeight,
    first_available_height: BlockHeight,
    first_block_time_unix_seconds: i64,
    last_block_time_unix_seconds: i64,
    contexts: Vec<BlockCommitContext>,
}

/// Hydrates the next settled range while holding the canonical read lock.
///
/// The lock is released before the materialized-view write so a long batch
/// write never blocks the tailer from advancing the shared secondary.
fn read_next_backfill_batch(
    config: ConventionalFeeDistributionBackfillConfig,
    context: &ConventionalFeeDistributionBackfillContext,
    coverage: Option<ConventionalFeeDistributionBackfillCoverage>,
) -> Result<BackfillBatch, IngestError> {
    let canonical = context.canonical.read();
    let chain_epoch = canonical.chain_epoch()?;
    let first_available_height = canonical.history_bounds().first_available_height();
    let from_height = next_backfill_height(coverage, first_available_height)?;
    let Some(target_height) = historical_backfill_target(
        chain_epoch.settled_tip_height,
        ConventionalFeeDistributionConsumer::tail_coverage(&context.materialized_view_store)?,
    ) else {
        return Ok(BackfillBatch::CaughtUp);
    };
    if from_height > target_height {
        return Ok(BackfillBatch::CaughtUp);
    }
    let through_height = BlockHeight::new(
        from_height
            .value()
            .saturating_add(config.batch_blocks.get().saturating_sub(1))
            .min(target_height.value()),
    );
    let contexts = read_canonical_context_batch(
        &canonical,
        &context.activations,
        BlockHeightRange::inclusive(from_height, through_height),
    )?;
    drop(canonical);
    let empty_batch = || {
        IngestError::MaterializedViewDispatch(
            "conventional-fee distribution backfill hydrated an empty batch".to_owned(),
        )
    };
    let first_block_time_unix_seconds = contexts
        .first()
        .ok_or_else(empty_batch)?
        .block_time_unix_seconds;
    let last_block_time_unix_seconds = contexts
        .last()
        .ok_or_else(empty_batch)?
        .block_time_unix_seconds;
    Ok(BackfillBatch::Ready(HydratedBackfillBatch {
        from_height,
        through_height,
        first_available_height,
        first_block_time_unix_seconds,
        last_block_time_unix_seconds,
        contexts,
    }))
}

fn historical_backfill_target(
    settled_tip_height: BlockHeight,
    tail: Option<ConventionalFeeDistributionTailCoverage>,
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
    coverage: Option<ConventionalFeeDistributionBackfillCoverage>,
    first_available_height: BlockHeight,
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = coverage else {
        return Ok(first_available_height);
    };
    if coverage.complete_from_height != first_available_height {
        return Err(IngestError::MaterializedViewDispatch(format!(
            "conventional-fee distribution backfill coverage starts at {}, expected {}",
            coverage.complete_from_height.value(),
            first_available_height.value()
        )));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::MaterializedViewDispatch(
            "conventional-fee distribution backfill height overflow".to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
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
                Some(ConventionalFeeDistributionBackfillCoverage::new(
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
            Some(ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(100),
                BlockHeight::new(256),
                1_600_000_000,
                1_600_000_001,
            )),
            BlockHeight::new(101),
        );
        assert!(matches!(
            result,
            Err(IngestError::MaterializedViewDispatch(_))
        ));
    }

    #[test]
    fn historical_backfill_stops_before_the_live_tail() {
        let tail = ConventionalFeeDistributionTailCoverage {
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
}
