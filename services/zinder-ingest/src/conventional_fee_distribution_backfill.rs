//! Resumable historical backfill and startup-tail seeding for ZIP-317
//! conventional-fee distribution.

use std::{num::NonZeroU32, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_derive::{
    ConventionalFeeDistributionBackfillCoverage, ConventionalFeeDistributionConsumer,
    ConventionalFeeDistributionTailCoverage, DeriveStore,
};
use zinder_store::PrimaryChainStore;

use crate::{
    IngestError,
    derive_consumers::derive_projection_write_guard,
    ingest_loop::{HistoricalWorkGate, wait_until_historical_work_or_cancelled},
    transaction_component_backfill::read_canonical_context_batch,
};

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const BACKFILL_CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);
const BACKFILL_START_HEIGHT: BlockHeight = BlockHeight::new(1);

pub(crate) fn seed_conventional_fee_distribution_visible_tail(
    chain_store: &PrimaryChainStore,
    derive_store: &DeriveStore,
    through_height: BlockHeight,
    batch_blocks: NonZeroU32,
) -> Result<(), IngestError> {
    loop {
        let tail =
            ConventionalFeeDistributionConsumer::tail_coverage(derive_store)?.ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "conventional-fee distribution tail boundary is missing during startup seeding"
                        .to_owned(),
                )
            })?;
        let next_height = tail
            .complete_through_height
            .map_or(Some(tail.boundary_height), BlockHeight::next)
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
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
        let contexts = read_canonical_context_batch(chain_store, next_height, batch_end)?;
        let _write_guard = derive_projection_write_guard();
        ConventionalFeeDistributionConsumer::new()
            .write_tail_seed_batch(derive_store, &contexts)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
}

/// Bounded controls for the ingest-owned conventional-fee distribution backfill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConventionalFeeDistributionBackfillConfig {
    /// Whether the background task runs.
    pub enabled: bool,
    /// Maximum canonical blocks processed per durable coverage update.
    pub batch_blocks: NonZeroU32,
}

/// Existing storage handles used by the conventional-fee distribution backfill.
#[derive(Clone)]
pub struct ConventionalFeeDistributionBackfillContext {
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl ConventionalFeeDistributionBackfillContext {
    /// Groups the canonical and derive stores for task startup.
    #[must_use]
    pub fn new(chain_store: PrimaryChainStore, derive_store: DeriveStore) -> Self {
        Self {
            chain_store,
            derive_store,
        }
    }
}

/// Spawns the non-readiness-blocking historical backfill task.
#[must_use = "await the handle during shutdown"]
pub fn spawn_conventional_fee_distribution_backfill_task(
    config: ConventionalFeeDistributionBackfillConfig,
    context: ConventionalFeeDistributionBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "conventional_fee_distribution_backfill_disabled",
            "conventional-fee distribution historical backfill is disabled"
        );
        return None;
    }

    Some(tokio::spawn(run_conventional_fee_distribution_backfill(
        config,
        context,
        historical_work_gate,
        cancel,
    )))
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
        from_height = BACKFILL_START_HEIGHT.value(),
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
    let coverage = ConventionalFeeDistributionConsumer::backfill_coverage(&context.derive_store)?;
    let next_height = next_backfill_height(coverage)?;
    let Some(chain_epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    };
    let Some(target_height) = historical_backfill_target(
        chain_epoch.settled_tip_height,
        ConventionalFeeDistributionConsumer::tail_coverage(&context.derive_store)?,
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
                "conventional-fee distribution backfill hydrated an empty batch".to_owned(),
            )
        })?
        .block_time_unix_seconds;
    let last_block_time = contexts
        .last()
        .ok_or_else(|| {
            IngestError::DeriveDispatch(
                "conventional-fee distribution backfill hydrated an empty batch".to_owned(),
            )
        })?
        .block_time_unix_seconds;
    let next_coverage = ConventionalFeeDistributionBackfillCoverage::new(
        coverage.map_or(BACKFILL_START_HEIGHT, |coverage| {
            coverage.complete_from_height
        }),
        batch_end,
        coverage.map_or(first_block_time, |coverage| {
            coverage.complete_from_time_unix_seconds
        }),
        last_block_time,
    );
    let _write_guard = derive_projection_write_guard();
    ConventionalFeeDistributionConsumer::new()
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
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = coverage else {
        return Ok(BACKFILL_START_HEIGHT);
    };
    if coverage.complete_from_height != BACKFILL_START_HEIGHT {
        return Err(IngestError::DeriveDispatch(format!(
            "conventional-fee distribution backfill coverage starts at {}, expected {}",
            coverage.complete_from_height.value(),
            BACKFILL_START_HEIGHT.value()
        )));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::DeriveDispatch(
            "conventional-fee distribution backfill height overflow".to_owned(),
        )
    })
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_resumes_after_contiguous_coverage() -> Result<(), IngestError> {
        assert_eq!(next_backfill_height(None)?, BlockHeight::new(1));
        assert_eq!(
            next_backfill_height(Some(ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(256),
                1_600_000_000,
                1_600_000_001,
            )))?,
            BlockHeight::new(257)
        );
        Ok(())
    }

    #[test]
    fn backfill_rejects_wrong_coverage_floor() {
        let result = next_backfill_height(Some(ConventionalFeeDistributionBackfillCoverage::new(
            BlockHeight::new(2),
            BlockHeight::new(256),
            1_600_000_000,
            1_600_000_001,
        )));
        assert!(matches!(result, Err(IngestError::DeriveDispatch(_))));
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

    #[tokio::test]
    async fn cancellation_interrupts_backfill_wait() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(sleep_or_cancel(Duration::from_mins(1), &cancel).await);
    }
}
