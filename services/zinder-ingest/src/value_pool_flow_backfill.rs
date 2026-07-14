//! Resumable historical and startup-tail backfill for value-pool flow history.

use std::{num::NonZeroU32, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore, VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
    ValuePoolFlowBackfillCoverage, ValuePoolFlowHistoryConsumer, ValuePoolFlowTailCoverage,
};
use zinder_store::PrimaryChainStore;

use crate::{
    IngestError,
    derive_consumers::{
        backfill_consumer_tail_boundary, derive_projection_write_guard,
        unanimous_existing_block_consumer_cursor,
    },
    ingest_loop::{HistoricalWorkGate, wait_until_historical_work_or_cancelled},
    paid_fee_distribution_backfill::{
        PaidFeeDistributionBackfillConfig, PaidFeeDistributionBackfillContext,
        hydrate_range_with_source,
    },
};

const BACKFILL_START_HEIGHT: BlockHeight = BlockHeight::new(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Bounded controls for value-pool flow history reconstruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValuePoolFlowBackfillConfig {
    /// Whether startup seeding and historical backfill run.
    pub enabled: bool,
    /// Maximum canonical blocks committed with one coverage update.
    pub batch_blocks: NonZeroU32,
    /// Maximum concurrent source requests for missing intrinsic facts.
    pub fetch_concurrency: NonZeroU32,
}

/// Existing stores used by the value-pool flow backfill.
#[derive(Clone)]
pub struct ValuePoolFlowBackfillContext {
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
    hydration_context: PaidFeeDistributionBackfillContext,
}

impl ValuePoolFlowBackfillContext {
    /// Groups canonical and derive stores for task startup.
    #[must_use]
    pub fn new(
        chain_store: PrimaryChainStore,
        derive_store: DeriveStore,
        hydration_context: PaidFeeDistributionBackfillContext,
    ) -> Self {
        Self {
            chain_store,
            derive_store,
            hydration_context,
        }
    }
}

/// Seeds source-backed visible history before persisting the inherited event cursor.
pub async fn seed_value_pool_flow_cursor_and_tail(
    config: ValuePoolFlowBackfillConfig,
    context: &ValuePoolFlowBackfillContext,
) -> Result<(), IngestError> {
    let Some(cursor) = unanimous_existing_block_consumer_cursor(&context.derive_store)? else {
        return Ok(());
    };
    let cursor_is_missing = match context
        .derive_store
        .get_chain_event_cursor(VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME)?
    {
        Some(existing) if existing != cursor => {
            return Err(IngestError::DeriveDispatch(
                "value-pool flow cursor disagrees with the existing block consumer boundary"
                    .to_owned(),
            ));
        }
        Some(_) => false,
        None => true,
    };
    let Some(authoritative_height) = context
        .derive_store
        .last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
    else {
        return Ok(());
    };
    let epoch = context.chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical chain epoch is missing while seeding value-pool flow tail".to_owned(),
        )
    })?;
    let boundary = if config.enabled {
        backfill_consumer_tail_boundary(
            epoch.settled_tip_height,
            authoritative_height,
            "value-pool flow",
        )?
    } else {
        authoritative_height.next().ok_or_else(|| {
            IngestError::DeriveDispatch("value-pool flow disabled boundary overflow".to_owned())
        })?
    };
    let boundary_changed = ValuePoolFlowHistoryConsumer::widen_tail_boundary_for_startup(
        &context.derive_store,
        boundary,
    )
    .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let tail_needs_seed = ValuePoolFlowHistoryConsumer::tail_coverage(&context.derive_store)?
        .is_some_and(|tail| {
            tail.complete_through_height
                .is_none_or(|through| through < authoritative_height)
        });
    if config.enabled && (cursor_is_missing || boundary_changed || tail_needs_seed) {
        seed_visible_tail_from_source(config, context, authoritative_height).await?;
    }
    if cursor_is_missing {
        context
            .derive_store
            .put_chain_event_cursor(VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME, &cursor)?;
    }
    Ok(())
}

async fn seed_visible_tail_from_source(
    config: ValuePoolFlowBackfillConfig,
    context: &ValuePoolFlowBackfillContext,
    authoritative_height: BlockHeight,
) -> Result<(), IngestError> {
    loop {
        let tail = ValuePoolFlowHistoryConsumer::tail_coverage(&context.derive_store)?.ok_or_else(
            || {
                IngestError::DeriveDispatch(
                    "value-pool flow tail disappeared during startup".to_owned(),
                )
            },
        )?;
        let next = tail
            .complete_through_height
            .map_or(Some(tail.boundary_height), BlockHeight::next)
            .ok_or_else(|| {
                IngestError::DeriveDispatch("value-pool flow startup tail overflow".to_owned())
            })?;
        if next > authoritative_height {
            return Ok(());
        }
        let through = batch_end(next, authoritative_height, config.batch_blocks);
        let (contexts, _) = hydrate_range_with_source(
            paid_hydration_config(config),
            &context.hydration_context,
            next,
            through,
            false,
        )
        .await?;
        let _write_guard = derive_projection_write_guard();
        ValuePoolFlowHistoryConsumer::new()
            .write_tail_seed_batch(&context.derive_store, &contexts)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
}

/// Spawns the non-readiness-blocking historical worker.
#[must_use = "await the handle during shutdown"]
pub fn spawn_value_pool_flow_backfill_task(
    config: ValuePoolFlowBackfillConfig,
    context: ValuePoolFlowBackfillContext,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            if wait_until_historical_work_or_cancelled(&historical_work_gate, &cancel).await {
                return;
            }
            let progress = backfill_next_batch(config, &context).await;
            match progress {
                Ok(BackfillProgress::Advanced { from, through }) => tracing::info!(
                    target: "zinder::ingest",
                    event = "value_pool_flow_backfill_progress",
                    from_height = from.value(),
                    through_height = through.value(),
                    "value-pool flow historical backfill advanced"
                ),
                Ok(BackfillProgress::CaughtUp) => {
                    if sleep_or_cancel(CAUGHT_UP_POLL_INTERVAL, &cancel).await {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "value_pool_flow_backfill_retry",
                        error = %error,
                        "value-pool flow historical backfill failed; retrying"
                    );
                    if sleep_or_cancel(RETRY_INTERVAL, &cancel).await {
                        return;
                    }
                }
            }
        }
    }))
}

enum BackfillProgress {
    Advanced {
        from: BlockHeight,
        through: BlockHeight,
    },
    CaughtUp,
}

async fn backfill_next_batch(
    config: ValuePoolFlowBackfillConfig,
    context: &ValuePoolFlowBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    let coverage = ValuePoolFlowHistoryConsumer::backfill_coverage(&context.derive_store)?;
    let next = next_backfill_height(coverage)?;
    let Some(epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(BackfillProgress::CaughtUp);
    };
    let Some(target) = historical_target(
        epoch.settled_tip_height,
        ValuePoolFlowHistoryConsumer::tail_coverage(&context.derive_store)?,
    ) else {
        return Ok(BackfillProgress::CaughtUp);
    };
    if next > target {
        return Ok(BackfillProgress::CaughtUp);
    }
    let through = batch_end(next, target, config.batch_blocks);
    let (contexts, _) = hydrate_range_with_source(
        paid_hydration_config(config),
        &context.hydration_context,
        next,
        through,
        true,
    )
    .await?;
    let first_time = contexts
        .first()
        .ok_or_else(empty_batch_error)?
        .block_time_unix_seconds;
    let last_time = contexts
        .last()
        .ok_or_else(empty_batch_error)?
        .block_time_unix_seconds;
    let next_coverage = ValuePoolFlowBackfillCoverage::new(
        coverage.map_or(BACKFILL_START_HEIGHT, |current| {
            current.complete_from_height
        }),
        through,
        coverage.map_or(first_time, |current| {
            current.complete_from_time_unix_seconds
        }),
        last_time,
    );
    let _write_guard = derive_projection_write_guard();
    ValuePoolFlowHistoryConsumer::new()
        .write_backfill_batch(&context.derive_store, &contexts, next_coverage)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(BackfillProgress::Advanced {
        from: next,
        through,
    })
}

fn paid_hydration_config(config: ValuePoolFlowBackfillConfig) -> PaidFeeDistributionBackfillConfig {
    PaidFeeDistributionBackfillConfig {
        enabled: true,
        batch_blocks: config.batch_blocks,
        fetch_concurrency: config.fetch_concurrency,
        history_days: NonZeroU32::MIN,
        timestamp_safety_seconds: 0,
    }
}

fn empty_batch_error() -> IngestError {
    IngestError::DeriveDispatch("value-pool flow backfill hydrated an empty batch".to_owned())
}

fn next_backfill_height(
    coverage: Option<ValuePoolFlowBackfillCoverage>,
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = coverage else {
        return Ok(BACKFILL_START_HEIGHT);
    };
    if coverage.complete_from_height != BACKFILL_START_HEIGHT {
        return Err(IngestError::DeriveDispatch(
            "value-pool flow historical coverage does not start at height 1".to_owned(),
        ));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::DeriveDispatch("value-pool flow backfill height overflow".to_owned())
    })
}

fn historical_target(
    settled_tip: BlockHeight,
    tail: Option<ValuePoolFlowTailCoverage>,
) -> Option<BlockHeight> {
    let before_tail = tail?.boundary_height.value().checked_sub(1)?;
    Some(BlockHeight::new(settled_tip.value().min(before_tail)))
}

fn batch_end(start: BlockHeight, target: BlockHeight, blocks: NonZeroU32) -> BlockHeight {
    BlockHeight::new(
        start
            .value()
            .saturating_add(blocks.get().saturating_sub(1))
            .min(target.value()),
    )
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use serde_json::Value;
    use zinder_core::{
        ArtifactSchemaVersion, BlockId, ChainEpoch, ChainEpochId, Network, UnixTimestampMillis,
        wire::encode_height_key_ascending,
    };
    use zinder_source::{
        NodeCapabilities, NodeSource, SourceBlock, SourceError, ZebraJsonRpcSource,
    };
    use zinder_store::{ChainEpochArtifacts, RocksDbResourceBudget};
    use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

    use super::*;
    use crate::{CommitmentTreeSizes, derive_block, finalize_derived_block};

    struct FixtureSource {
        block: SourceBlock,
        fetches: AtomicUsize,
    }

    #[async_trait]
    impl NodeSource for FixtureSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if height == self.block.height {
                Ok(self.block.clone())
            } else {
                Err(SourceError::BlockUnavailable {
                    height,
                    reason: "fixture source only serves one block".to_owned(),
                })
            }
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(self.block.height, self.block.hash))
        }
    }

    #[test]
    fn restart_resumes_after_durable_coverage() -> Result<(), IngestError> {
        assert_eq!(next_backfill_height(None)?, BlockHeight::new(1));
        assert_eq!(
            next_backfill_height(Some(ValuePoolFlowBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(256),
                10,
                20,
            )))?,
            BlockHeight::new(257)
        );
        Ok(())
    }

    #[test]
    fn historical_range_stops_before_live_tail() {
        assert_eq!(
            historical_target(
                BlockHeight::new(105),
                Some(ValuePoolFlowTailCoverage::from_boundary(BlockHeight::new(
                    101
                ))),
            ),
            Some(BlockHeight::new(100))
        );
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression keeps preserved canonical state, source hydration, and cursor assertions together"
    )]
    async fn preserved_store_startup_hydrates_missing_intrinsic_facts_from_source()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let source_block = regtest_fixture_block()?;
        let derived = derive_block(&source_block, &sample_regtest_upgrade_activations())?;
        let mut tree_sizes = CommitmentTreeSizes::default();
        let built = finalize_derived_block(derived, &mut tree_sizes)?;
        assert!(!built.transaction_intrinsic_value_balances.is_empty());

        let fixture = StoreFixture::open()?;
        let chain_store = fixture.chain_store().clone();
        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            visible_tip_height: source_block.height,
            visible_tip_hash: source_block.hash,
            settled_tip_height: BlockHeight::new(0),
            settled_tip_hash: source_block.parent_hash,
            artifact_schema_version: ArtifactSchemaVersion::new(12),
            tip_metadata: built.tip_metadata,
            created_at: UnixTimestampMillis::new(1_774_669_000_000),
        };
        chain_store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                chain_epoch,
                vec![built.block_header],
                vec![built.compact_block],
            )
            .with_block_transaction_index(built.block_transaction_index)
            .with_transaction_locations(built.transaction_locations)
            .with_transaction_facts(built.transaction_facts)
            .with_transaction_blobs(built.transaction_blobs),
        )?;
        let transaction_id = chain_store
            .current_chain_epoch_reader()?
            .transaction_ids_at_height(source_block.height)?
            .into_iter()
            .next()
            .ok_or("fixture block must contain a transaction")?;
        assert!(
            chain_store
                .current_chain_epoch_reader()?
                .transaction_intrinsic_value_balances_by_id(transaction_id)?
                .is_none()
        );

        let derive_store = crate::open_primary_derive_store_for_canonical(
            fixture.tempdir_path(),
            RocksDbResourceBudget::for_local_tests(),
        )?;
        derive_store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &encode_height_key_ascending(source_block.height),
            b"preserved",
        )?;
        for consumer_name in DeriveStore::bundled_chain_event_consumer_names() {
            if *consumer_name != VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME {
                derive_store.put_chain_event_cursor(*consumer_name, b"existing-cursor")?;
            }
        }
        let source = Arc::new(FixtureSource {
            block: source_block.clone(),
            fetches: AtomicUsize::new(0),
        });
        let hydration_context = PaidFeeDistributionBackfillContext::new(
            Duration::from_secs(1),
            Arc::new(sample_regtest_upgrade_activations()),
            source.clone(),
            chain_store,
            derive_store.clone(),
        );
        let context = ValuePoolFlowBackfillContext::new(
            fixture.chain_store().clone(),
            derive_store.clone(),
            hydration_context,
        );
        seed_value_pool_flow_cursor_and_tail(
            ValuePoolFlowBackfillConfig {
                enabled: true,
                batch_blocks: NonZeroU32::MIN,
                fetch_concurrency: NonZeroU32::MIN,
            },
            &context,
        )
        .await?;

        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            derive_store.get_chain_event_cursor(VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME)?,
            Some(b"existing-cursor".to_vec())
        );
        assert_eq!(
            ValuePoolFlowHistoryConsumer::tail_coverage(&derive_store)?,
            Some(ValuePoolFlowTailCoverage {
                boundary_height: source_block.height,
                complete_through_height: Some(source_block.height),
                complete_through_time_unix_seconds: Some(i64::from(
                    source_block.block_time_seconds,
                )),
            })
        );
        Ok(())
    }

    fn regtest_fixture_block() -> Result<SourceBlock, Box<dyn Error + Send + Sync>> {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/z3-regtest-block-1.json"))?;
        let raw_block_hex = fixture
            .get("raw_block_hex")
            .and_then(Value::as_str)
            .ok_or("fixture raw_block_hex is missing")?;
        let raw_block_bytes = hex::decode(raw_block_hex)?;
        let height = fixture
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|height| u32::try_from(height).ok())
            .ok_or("fixture height is missing")?;
        Ok(SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(height),
            raw_block_bytes,
        )?)
    }
}
