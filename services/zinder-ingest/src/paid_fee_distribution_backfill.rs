//! Startup-tail seeding and newest-first settled-history backfill for exact paid fees.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt as _, stream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHeaderArtifact, BlockHeight, NetworkUpgradeActivations, TransactionFactsArtifact,
    TransactionId, TransactionIntrinsicValueBalances, TransactionIntrinsicValueBalancesArtifact,
    TransactionLocation, TransparentOutPoint, TransparentSpendFact,
};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore, PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
    PAID_FEE_DISTRIBUTION_COVERAGE_COLUMN_FAMILY, PaidFeeDistributionBackfillCoverage,
    PaidFeeDistributionConsumer, TransactionIntrinsicValueBalanceFacts, TransparentSpendFacts,
};
use zinder_source::{NodeSource, SourceBlock};
use zinder_store::{
    ChainEpochReader, MAX_TRANSACTION_INTRINSIC_VALUE_BALANCE_ENRICHMENT_BATCH, PrimaryChainStore,
};

use crate::{
    IngestError, derive_block,
    derive_consumers::{
        backfill_consumer_tail_boundary, derive_projection_write_guard,
        unanimous_existing_block_consumer_cursor,
    },
};

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const BACKFILL_CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);
const TARGET_FLOOR_KEY: &[u8] = b"ingest_target_floor_v1";
const TARGET_FLOOR_VALUE_LEN: usize = 4;
const SETTLED_TAIL_RECONCILIATION_KEY: &[u8] = b"settled_tail_reconciliation_v1";
const SETTLED_TAIL_RECONCILIATION_VALUE_LEN: usize = 8;
const SECONDS_PER_DAY: u64 = 86_400;

/// Bounded controls for the exact paid-fee startup seed and historical backfill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaidFeeDistributionBackfillConfig {
    /// Whether startup seeding and the background task run.
    pub enabled: bool,
    /// Maximum canonical blocks processed per durable projection update.
    pub batch_blocks: NonZeroU32,
    /// Maximum concurrent historical whole-block requests.
    pub fetch_concurrency: NonZeroU32,
    /// Wall-clock history retained by the exact paid-fee projection.
    pub history_days: NonZeroU32,
    /// Extra history retained to cover consensus-permitted timestamp skew.
    pub timestamp_safety_seconds: u64,
}

/// Existing source and storage handles used by paid-fee startup and backfill.
#[derive(Clone)]
pub struct PaidFeeDistributionBackfillContext {
    request_timeout: Duration,
    activations: Arc<NetworkUpgradeActivations>,
    source: Arc<dyn NodeSource>,
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl PaidFeeDistributionBackfillContext {
    /// Groups the existing source and storage handles for task startup.
    #[must_use]
    pub fn new(
        request_timeout: Duration,
        activations: Arc<NetworkUpgradeActivations>,
        source: Arc<dyn NodeSource>,
        chain_store: PrimaryChainStore,
        derive_store: DeriveStore,
    ) -> Self {
        Self {
            request_timeout,
            activations,
            source,
            chain_store,
            derive_store,
        }
    }
}

/// Seeds the paid-fee consumer at the existing event boundary without replaying siblings.
pub async fn seed_paid_fee_distribution_cursor_and_tail(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
) -> Result<(), IngestError> {
    let Some(cursor) = unanimous_existing_block_consumer_cursor(&context.derive_store)? else {
        return Ok(());
    };
    let cursor_is_missing = match context
        .derive_store
        .get_chain_event_cursor(PAID_FEE_DISTRIBUTION_CONSUMER_NAME)?
    {
        Some(existing) if existing != cursor => {
            return Err(IngestError::DeriveDispatch(
                "paid-fee distribution cursor disagrees with the existing block consumer boundary"
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
    let chain_epoch = context.chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical chain epoch is missing while seeding paid-fee distribution tail".to_owned(),
        )
    })?;
    let boundary_height = if config.enabled {
        backfill_consumer_tail_boundary(
            chain_epoch.settled_tip_height,
            authoritative_height,
            "paid-fee distribution",
        )?
    } else {
        authoritative_height.next().ok_or_else(|| {
            IngestError::DeriveDispatch(
                "disabled paid-fee distribution tail boundary height overflow".to_owned(),
            )
        })?
    };
    let tail_boundary_changed = PaidFeeDistributionConsumer::widen_tail_boundary_for_startup(
        &context.derive_store,
        boundary_height,
    )
    .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let tail =
        PaidFeeDistributionConsumer::tail_coverage(&context.derive_store)?.ok_or_else(|| {
            IngestError::DeriveDispatch(
                "paid-fee distribution tail coverage disappeared during startup".to_owned(),
            )
        })?;
    let tail_needs_seed = tail
        .complete_through_height
        .is_none_or(|through| through < authoritative_height);
    if config.enabled && should_seed_tail(cursor_is_missing, tail_boundary_changed, tail_needs_seed)
    {
        seed_visible_tail(config, context, authoritative_height).await?;
    }
    if cursor_is_missing {
        // The cursor is deliberately last: a crash during tail seeding must
        // leave startup able to resume the cursor-neutral seed.
        context
            .derive_store
            .put_chain_event_cursor(PAID_FEE_DISTRIBUTION_CONSUMER_NAME, &cursor)?;
    }
    record_tail_initialization(
        config.enabled,
        cursor_is_missing,
        boundary_height,
        authoritative_height,
    );
    Ok(())
}

fn record_tail_initialization(
    backfill_enabled: bool,
    cursor_seeded: bool,
    boundary_height: BlockHeight,
    through_height: BlockHeight,
) {
    tracing::info!(
        target: "zinder::ingest",
        event = "paid_fee_distribution_tail_boundary_initialized",
        cursor_seeded,
        tail_boundary = boundary_height.value(),
        through_height = through_height.value(),
        "paid-fee distribution consumer joined the existing derive event boundary"
    );
    if !backfill_enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "paid_fee_distribution_startup_seed_disabled",
            "paid-fee historical startup seed is disabled; cursor isolation remains active"
        );
    }
}

fn should_seed_tail(
    cursor_is_missing: bool,
    tail_boundary_changed: bool,
    tail_needs_seed: bool,
) -> bool {
    cursor_is_missing || tail_boundary_changed || tail_needs_seed
}

async fn seed_visible_tail(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    authoritative_height: BlockHeight,
) -> Result<(), IngestError> {
    loop {
        let tail = PaidFeeDistributionConsumer::tail_coverage(&context.derive_store)?.ok_or_else(
            || {
                IngestError::DeriveDispatch(
                    "paid-fee distribution tail boundary disappeared during startup".to_owned(),
                )
            },
        )?;
        let next_height = tail
            .complete_through_height
            .map_or(Some(tail.boundary_height), BlockHeight::next)
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "paid-fee distribution startup tail height overflow".to_owned(),
                )
            })?;
        if next_height > authoritative_height {
            return Ok(());
        }
        let batch_end = forward_batch_end(next_height, authoritative_height, config.batch_blocks);
        let contexts =
            hydrate_range_with_source(config, context, next_height, batch_end, false).await?;
        let _write_guard = derive_projection_write_guard();
        PaidFeeDistributionConsumer::new()
            .write_tail_seed_batch(&context.derive_store, &contexts)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
        tracing::info!(
            target: "zinder::ingest",
            event = "paid_fee_distribution_tail_seed_progress",
            from_height = next_height.value(),
            through_height = batch_end.value(),
            "paid-fee distribution startup tail seed advanced"
        );
    }
}

/// Spawns the non-readiness-blocking newest-first historical backfill task.
#[must_use = "await the handle during shutdown"]
pub fn spawn_paid_fee_distribution_backfill_task(
    config: PaidFeeDistributionBackfillConfig,
    context: PaidFeeDistributionBackfillContext,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "paid_fee_distribution_backfill_disabled",
            "paid-fee distribution historical backfill is disabled"
        );
        return None;
    }
    Some(tokio::spawn(run_paid_fee_distribution_backfill(
        config, context, cancel,
    )))
}

async fn run_paid_fee_distribution_backfill(
    config: PaidFeeDistributionBackfillConfig,
    context: PaidFeeDistributionBackfillContext,
    cancel: CancellationToken,
) {
    let Some(target_floor) = resolve_target_floor_until_cancelled(config, &context, &cancel).await
    else {
        return;
    };
    record_backfill_started(config, target_floor);

    loop {
        if reconcile_settled_tail_or_retry(config, &context, &cancel).await {
            continue;
        }
        if advance_backfill_once(config, target_floor, &context, &cancel).await {
            return;
        }
    }
}

async fn resolve_target_floor_until_cancelled(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    cancel: &CancellationToken,
) -> Option<BlockHeight> {
    loop {
        let resolution = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "paid_fee_distribution_backfill_cancelled",
                    stage = "resolve_target_floor",
                    "paid-fee distribution historical backfill cancelled"
                );
                return None;
            }
            resolution = resolve_target_floor(config, context) => resolution,
        };
        match resolution {
            Ok(target_floor) => return Some(target_floor),
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "paid_fee_distribution_backfill_retry",
                    stage = "resolve_target_floor",
                    error = %error,
                    retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                    "paid-fee distribution backfill could not resolve its durable target floor; retrying"
                );
                if sleep_or_cancel(BACKFILL_RETRY_INTERVAL, cancel).await {
                    return None;
                }
            }
        }
    }
}

fn record_backfill_started(config: PaidFeeDistributionBackfillConfig, target_floor: BlockHeight) {
    tracing::info!(
        target: "zinder::ingest",
        event = "paid_fee_distribution_backfill_started",
        target_floor_height = target_floor.value(),
        batch_blocks = config.batch_blocks.get(),
        fetch_concurrency = config.fetch_concurrency.get(),
        history_days = config.history_days.get(),
        timestamp_safety_seconds = config.timestamp_safety_seconds,
        direction = "newest_first",
        "paid-fee distribution historical backfill started"
    );
}

async fn reconcile_settled_tail_or_retry(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    cancel: &CancellationToken,
) -> bool {
    match reconcile_settled_tail(config, context).await {
        Ok(SettledTailReconciliation::Reconciled(range)) => {
            tracing::info!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_settled_tail_reconciled",
                from_height = range.from_height.value(),
                through_height = range.through_height.value(),
                fetched_blocks = range.fetched_blocks,
                "paid-fee seeded-tail intrinsic artifacts reconciled canonically"
            );
            false
        }
        Ok(SettledTailReconciliation::UpToDate) => false,
        Ok(SettledTailReconciliation::AwaitingSeededTail) => {
            tracing::info!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_awaiting_seeded_tail",
                retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                "paid-fee settled-tail reconciliation is waiting for derive replay to seed the live-tail boundary"
            );
            if !sleep_or_cancel(BACKFILL_RETRY_INTERVAL, cancel).await {
                return true;
            }
            false
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_settled_tail_reconciliation_retry",
                error = %error,
                retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                "paid-fee settled-tail reconciliation failed; durable progress was not advanced"
            );
            if !sleep_or_cancel(BACKFILL_RETRY_INTERVAL, cancel).await {
                return true;
            }
            false
        }
    }
}

async fn advance_backfill_once(
    config: PaidFeeDistributionBackfillConfig,
    target_floor: BlockHeight,
    context: &PaidFeeDistributionBackfillContext,
    cancel: &CancellationToken,
) -> bool {
    let progress = tokio::select! {
        () = cancel.cancelled() => {
            tracing::info!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_backfill_cancelled",
                "paid-fee distribution historical backfill cancelled"
            );
            return true;
        }
        progress = backfill_next_batch(config, target_floor, context) => progress,
    };
    let delay = match progress {
        Ok(BackfillProgress::Prepended {
            from_height,
            through_height,
            transaction_count,
            fetched_blocks,
        }) => {
            tracing::info!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_backfill_progress",
                from_height = from_height.value(),
                through_height = through_height.value(),
                transaction_count,
                fetched_blocks,
                direction = "newest_first",
                "paid-fee distribution historical coverage prepended"
            );
            return false;
        }
        Ok(BackfillProgress::Complete {
            complete_from_height,
        }) => {
            tracing::info!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_backfill_completed",
                target_floor_height = target_floor.value(),
                complete_from_height = complete_from_height.map(BlockHeight::value),
                "paid-fee distribution historical backfill reached its durable target floor"
            );
            BACKFILL_CAUGHT_UP_POLL_INTERVAL
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "paid_fee_distribution_backfill_retry",
                error = %error,
                retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                "paid-fee distribution backfill batch failed; coverage was not advanced"
            );
            BACKFILL_RETRY_INTERVAL
        }
    };
    sleep_or_cancel(delay, cancel).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReconciledTailRange {
    from_height: BlockHeight,
    through_height: BlockHeight,
    fetched_blocks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettledTailReconciliation {
    Reconciled(ReconciledTailRange),
    AwaitingSeededTail,
    UpToDate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SettledTailReconciliationCoverage {
    complete_from_height: BlockHeight,
    complete_through_height: BlockHeight,
}

async fn reconcile_settled_tail(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
) -> Result<SettledTailReconciliation, IngestError> {
    let Some(tail) = PaidFeeDistributionConsumer::tail_coverage(&context.derive_store)? else {
        return Ok(SettledTailReconciliation::AwaitingSeededTail);
    };
    let Some(tail_complete) = tail.complete_through_height else {
        return Ok(SettledTailReconciliation::UpToDate);
    };
    let Some(chain_epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(SettledTailReconciliation::UpToDate);
    };
    let target = tail_complete.min(chain_epoch.settled_tip_height);
    let coverage = read_settled_tail_reconciliation_coverage(&context.derive_store)?;
    let Some((from_height, through_height)) = next_settled_tail_reconciliation_range(
        tail.boundary_height,
        target,
        coverage,
        config.batch_blocks,
    ) else {
        return Ok(SettledTailReconciliation::UpToDate);
    };

    let (_expectations, missing) =
        read_canonical_expectations(&context.chain_store, from_height, through_height, true)
            .await?;
    let fetched_blocks = missing.len();
    let artifacts = fetch_missing_intrinsic_artifacts(config, context, missing).await?;
    enrich_settled_intrinsic_artifacts(context.chain_store.clone(), &artifacts).await?;

    let complete_from_height = coverage.map_or(from_height, |coverage| {
        coverage.complete_from_height.min(from_height)
    });
    persist_settled_tail_reconciliation_coverage(
        &context.derive_store,
        SettledTailReconciliationCoverage {
            complete_from_height,
            complete_through_height: through_height,
        },
    )?;
    Ok(SettledTailReconciliation::Reconciled(ReconciledTailRange {
        from_height,
        through_height,
        fetched_blocks,
    }))
}

fn next_settled_tail_reconciliation_range(
    tail_boundary: BlockHeight,
    settled_tail_tip: BlockHeight,
    coverage: Option<SettledTailReconciliationCoverage>,
    batch_blocks: NonZeroU32,
) -> Option<(BlockHeight, BlockHeight)> {
    if settled_tail_tip < tail_boundary {
        return None;
    }
    let next_height = match coverage {
        Some(coverage) if coverage.complete_from_height <= tail_boundary => {
            coverage.complete_through_height.next()?
        }
        Some(_) | None => tail_boundary,
    };
    if next_height > settled_tail_tip {
        return None;
    }
    Some((
        next_height,
        forward_batch_end(next_height, settled_tail_tip, batch_blocks),
    ))
}

fn read_settled_tail_reconciliation_coverage(
    store: &DeriveStore,
) -> Result<Option<SettledTailReconciliationCoverage>, IngestError> {
    let Some(bytes) = store.get_consumer(
        PAID_FEE_DISTRIBUTION_COVERAGE_COLUMN_FAMILY,
        SETTLED_TAIL_RECONCILIATION_KEY,
    )?
    else {
        return Ok(None);
    };
    let bytes: [u8; SETTLED_TAIL_RECONCILIATION_VALUE_LEN] = bytes.try_into().map_err(|_| {
        IngestError::DeriveDispatch(
            "paid-fee settled-tail reconciliation coverage is malformed".to_owned(),
        )
    })?;
    Ok(Some(SettledTailReconciliationCoverage {
        complete_from_height: BlockHeight::new(u32::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ])),
        complete_through_height: BlockHeight::new(u32::from_be_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
    }))
}

fn persist_settled_tail_reconciliation_coverage(
    store: &DeriveStore,
    coverage: SettledTailReconciliationCoverage,
) -> Result<(), IngestError> {
    let mut bytes = [0u8; SETTLED_TAIL_RECONCILIATION_VALUE_LEN];
    bytes[..4].copy_from_slice(&coverage.complete_from_height.value().to_be_bytes());
    bytes[4..].copy_from_slice(&coverage.complete_through_height.value().to_be_bytes());
    store.put_consumer(
        PAID_FEE_DISTRIBUTION_COVERAGE_COLUMN_FAMILY,
        SETTLED_TAIL_RECONCILIATION_KEY,
        &bytes,
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackfillProgress {
    Prepended {
        from_height: BlockHeight,
        through_height: BlockHeight,
        transaction_count: usize,
        fetched_blocks: usize,
    },
    Complete {
        complete_from_height: Option<BlockHeight>,
    },
}

async fn backfill_next_batch(
    config: PaidFeeDistributionBackfillConfig,
    target_floor: BlockHeight,
    context: &PaidFeeDistributionBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    let coverage = PaidFeeDistributionConsumer::backfill_coverage(&context.derive_store)?;
    let tail =
        PaidFeeDistributionConsumer::tail_coverage(&context.derive_store)?.ok_or_else(|| {
            IngestError::DeriveDispatch(
                "paid-fee distribution backfill requires a seeded live-tail boundary".to_owned(),
            )
        })?;
    let Some(batch_end) = next_prepend_end(coverage, tail.boundary_height, target_floor) else {
        return Ok(BackfillProgress::Complete {
            complete_from_height: coverage.map(|coverage| coverage.complete_from_height),
        });
    };
    let chain_epoch = context.chain_store.current_chain_epoch()?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "canonical chain epoch is missing during paid-fee backfill".to_owned(),
        )
    })?;
    if batch_end > chain_epoch.settled_tip_height {
        return Err(IngestError::DeriveDispatch(format!(
            "paid-fee distribution next prepend height {} is not settled through {}",
            batch_end.value(),
            chain_epoch.settled_tip_height.value()
        )));
    }
    let batch_start = backward_batch_start(target_floor, batch_end, config.batch_blocks);
    let (contexts, fetched_blocks) =
        hydrate_settled_range(config, context, batch_start, batch_end).await?;
    let transaction_count = contexts.iter().map(|block| block.transactions.len()).sum();
    let first = contexts.first().ok_or_else(|| {
        IngestError::DeriveDispatch("paid-fee backfill hydrated an empty batch".to_owned())
    })?;
    let last = contexts.last().ok_or_else(|| {
        IngestError::DeriveDispatch("paid-fee backfill hydrated an empty batch".to_owned())
    })?;
    let next_coverage = PaidFeeDistributionBackfillCoverage::new(
        first.height,
        coverage.map_or(last.height, |coverage| coverage.complete_through_height),
        first.block_time_unix_seconds,
        coverage.map_or(last.block_time_unix_seconds, |coverage| {
            coverage.complete_through_time_unix_seconds
        }),
    );
    let _write_guard = derive_projection_write_guard();
    PaidFeeDistributionConsumer::new()
        .write_backfill_batch(&context.derive_store, &contexts, next_coverage)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(BackfillProgress::Prepended {
        from_height: batch_start,
        through_height: batch_end,
        transaction_count,
        fetched_blocks,
    })
}

async fn resolve_target_floor(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
) -> Result<BlockHeight, IngestError> {
    let chain_store = context.chain_store.clone();
    let requested_floor = tokio::task::spawn_blocking(move || {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?
            .as_secs();
        let history_seconds = u64::from(config.history_days.get())
            .checked_mul(SECONDS_PER_DAY)
            .and_then(|seconds| seconds.checked_add(config.timestamp_safety_seconds))
            .ok_or_else(|| {
                IngestError::DeriveDispatch(
                    "paid-fee distribution history duration overflow".to_owned(),
                )
            })?;
        let cutoff = now.saturating_sub(history_seconds);
        select_history_floor(&chain_store, cutoff)
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })??;

    let persisted = read_persisted_target_floor(&context.derive_store)?;
    let target_floor = persisted.map_or(requested_floor, |existing| existing.min(requested_floor));
    if persisted != Some(target_floor) {
        context.derive_store.put_consumer(
            PAID_FEE_DISTRIBUTION_COVERAGE_COLUMN_FAMILY,
            TARGET_FLOOR_KEY,
            &target_floor.value().to_be_bytes(),
        )?;
        tracing::info!(
            target: "zinder::ingest",
            event = "paid_fee_distribution_target_floor_persisted",
            target_floor_height = target_floor.value(),
            previous_target_floor_height = persisted.map(BlockHeight::value),
            "paid-fee distribution durable target floor initialized or expanded"
        );
    }
    Ok(target_floor)
}

fn read_persisted_target_floor(store: &DeriveStore) -> Result<Option<BlockHeight>, IngestError> {
    let Some(bytes) = store.get_consumer(
        PAID_FEE_DISTRIBUTION_COVERAGE_COLUMN_FAMILY,
        TARGET_FLOOR_KEY,
    )?
    else {
        return Ok(None);
    };
    let bytes: [u8; TARGET_FLOOR_VALUE_LEN] = bytes.try_into().map_err(|_| {
        IngestError::DeriveDispatch(
            "paid-fee distribution durable target floor is malformed".to_owned(),
        )
    })?;
    Ok(Some(BlockHeight::new(u32::from_be_bytes(bytes))))
}

fn select_history_floor(
    chain_store: &PrimaryChainStore,
    cutoff_unix_seconds: u64,
) -> Result<BlockHeight, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    let mut height = reader.chain_epoch().settled_tip_height;
    loop {
        let header = reader.block_header_at(height)?.ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "canonical header {} is unavailable while selecting paid-fee history floor",
                height.value()
            ))
        })?;
        let block_time = u64::try_from(header.block_time).unwrap_or(0);
        if block_time < cutoff_unix_seconds || height.value() <= 1 {
            return Ok(height);
        }
        height = BlockHeight::new(height.value() - 1);
    }
}

#[derive(Clone)]
struct CanonicalBlockExpectation {
    header: BlockHeaderArtifact,
    transaction_locations: Vec<TransactionLocation>,
}

struct HydratedPaidFeeTransactions {
    transactions_by_block: Vec<Vec<TransactionFactsArtifact>>,
    intrinsic_by_id: HashMap<TransactionId, TransactionIntrinsicValueBalances>,
    transparent_parent_ids: HashSet<TransactionId>,
}

async fn hydrate_settled_range(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<(Vec<zinder_derive::BlockCommitContext>, usize), IngestError> {
    let (expectations, missing) =
        read_canonical_expectations(&context.chain_store, from_height, through_height, true)
            .await?;
    let fetched_blocks = missing.len();
    let fetched = fetch_missing_intrinsic_artifacts(config, context, missing).await?;
    enrich_settled_intrinsic_artifacts(context.chain_store.clone(), &fetched).await?;
    let contexts = hydrate_canonical_contexts(
        context.chain_store.clone(),
        expectations,
        HashMap::new(),
        true,
    )
    .await?;
    Ok((contexts, fetched_blocks))
}

pub(crate) async fn hydrate_range_with_source(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    from_height: BlockHeight,
    through_height: BlockHeight,
    require_settled: bool,
) -> Result<Vec<zinder_derive::BlockCommitContext>, IngestError> {
    let (expectations, missing) = read_canonical_expectations(
        &context.chain_store,
        from_height,
        through_height,
        require_settled,
    )
    .await?;
    let fetched = fetch_missing_intrinsic_artifacts(config, context, missing).await?;
    let overlay = fetched
        .iter()
        .map(|artifact| (artifact.location.transaction_id, *artifact))
        .collect();
    hydrate_canonical_contexts(
        context.chain_store.clone(),
        expectations,
        overlay,
        require_settled,
    )
    .await
}

async fn read_canonical_expectations(
    chain_store: &PrimaryChainStore,
    from_height: BlockHeight,
    through_height: BlockHeight,
    require_settled: bool,
) -> Result<
    (
        Vec<CanonicalBlockExpectation>,
        Vec<CanonicalBlockExpectation>,
    ),
    IngestError,
> {
    let store = chain_store.clone();
    tokio::task::spawn_blocking(move || {
        let reader = store.current_chain_epoch_reader()?;
        let boundary = if require_settled {
            reader.chain_epoch().settled_tip_height
        } else {
            reader.chain_epoch().visible_tip_height
        };
        if through_height > boundary {
            return Err(IngestError::DeriveDispatch(format!(
                "paid-fee hydration range through {} crosses canonical boundary {}",
                through_height.value(),
                boundary.value()
            )));
        }
        let mut expectations = Vec::new();
        let mut missing = Vec::new();
        for height in inclusive_heights(from_height, through_height) {
            let header = reader.block_header_at(height)?.ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "canonical block header {} is unavailable for paid-fee hydration",
                    height.value()
                ))
            })?;
            if header.height != height {
                return Err(IngestError::DeriveDispatch(format!(
                    "canonical paid-fee header {} reports height {}",
                    height.value(),
                    header.height.value()
                )));
            }
            let transaction_ids = reader.transaction_ids_at_height(height)?;
            if transaction_ids.is_empty() {
                return Err(IngestError::DeriveDispatch(format!(
                    "canonical transaction index {} is empty during paid-fee hydration",
                    height.value()
                )));
            }
            let mut locations = Vec::with_capacity(transaction_ids.len());
            let mut block_missing = false;
            for transaction_id in transaction_ids {
                let location = reader
                    .transaction_location_by_id(transaction_id)?
                    .ok_or_else(|| {
                        IngestError::DeriveDispatch(format!(
                            "canonical transaction location {} is unavailable",
                            hex::encode(transaction_id.as_bytes())
                        ))
                    })?;
                if location.block_height != height || location.block_hash != header.block_hash {
                    return Err(IngestError::DeriveDispatch(format!(
                        "canonical transaction location {} is stale for height {}",
                        hex::encode(transaction_id.as_bytes()),
                        height.value()
                    )));
                }
                block_missing |= reader
                    .transaction_intrinsic_value_balances_by_id(transaction_id)?
                    .is_none();
                locations.push(location);
            }
            let expectation = CanonicalBlockExpectation {
                header,
                transaction_locations: locations,
            };
            if block_missing {
                missing.push(expectation.clone());
            }
            expectations.push(expectation);
        }
        Ok((expectations, missing))
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })?
}

async fn fetch_missing_intrinsic_artifacts(
    config: PaidFeeDistributionBackfillConfig,
    context: &PaidFeeDistributionBackfillContext,
    missing: Vec<CanonicalBlockExpectation>,
) -> Result<Vec<TransactionIntrinsicValueBalancesArtifact>, IngestError> {
    let mut fetches = stream::iter(missing.into_iter().map(|expected| {
        let source = Arc::clone(&context.source);
        let activations = Arc::clone(&context.activations);
        async move {
            let height = expected.header.height;
            let source_block =
                tokio::time::timeout(context.request_timeout, source.fetch_block_at(height))
                    .await
                    .map_err(|error| IngestError::SourceRetryDeadlineExceeded {
                        operation: format!(
                            "fetch paid-fee source block at height {}",
                            height.value()
                        ),
                        reason: error.to_string(),
                    })??;
            validate_source_block(&source_block, &expected)?;
            let derived = derive_block(&source_block, &activations)?;
            validate_derived_locations(
                &derived.transaction_intrinsic_value_balances,
                &expected.transaction_locations,
            )?;
            Ok::<_, IngestError>(derived.transaction_intrinsic_value_balances)
        }
    }))
    .buffer_unordered(usize::try_from(config.fetch_concurrency.get()).unwrap_or(usize::MAX));
    let mut artifacts = Vec::new();
    while let Some(fetch_result) = fetches.next().await {
        artifacts.extend(fetch_result?);
    }
    artifacts.sort_unstable_by_key(|artifact| {
        (
            artifact.location.block_height,
            artifact.location.tx_index_in_block,
        )
    });
    Ok(artifacts)
}

fn validate_source_block(
    source_block: &SourceBlock,
    expected: &CanonicalBlockExpectation,
) -> Result<(), IngestError> {
    let header = &expected.header;
    let source_identity = (
        source_block.height,
        source_block.hash,
        source_block.parent_hash,
    );
    let canonical_identity = (header.height, header.block_hash, header.parent_hash);
    if source_identity == canonical_identity {
        return Ok(());
    }
    Err(IngestError::DeriveDispatch(format!(
        "source block at height {} does not match the canonical paid-fee block identity",
        header.height.value()
    )))
}

fn validate_derived_locations(
    artifacts: &[TransactionIntrinsicValueBalancesArtifact],
    expected: &[TransactionLocation],
) -> Result<(), IngestError> {
    if artifacts.len() == expected.len()
        && artifacts
            .iter()
            .zip(expected)
            .all(|(artifact, location)| artifact.location == *location)
    {
        return Ok(());
    }
    Err(IngestError::DeriveDispatch(
        "source-derived paid-fee transaction locations do not match the canonical block index"
            .to_owned(),
    ))
}

async fn enrich_settled_intrinsic_artifacts(
    chain_store: PrimaryChainStore,
    artifacts: &[TransactionIntrinsicValueBalancesArtifact],
) -> Result<(), IngestError> {
    let artifacts = artifacts.to_vec();
    tokio::task::spawn_blocking(move || {
        for chunk in artifacts.chunks(MAX_TRANSACTION_INTRINSIC_VALUE_BALANCE_ENRICHMENT_BATCH) {
            chain_store.enrich_transaction_intrinsic_value_balances(chunk)?;
        }
        Ok::<_, IngestError>(())
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })?
}

async fn hydrate_canonical_contexts(
    chain_store: PrimaryChainStore,
    expectations: Vec<CanonicalBlockExpectation>,
    intrinsic_overlay: HashMap<TransactionId, TransactionIntrinsicValueBalancesArtifact>,
    require_settled: bool,
) -> Result<Vec<zinder_derive::BlockCommitContext>, IngestError> {
    tokio::task::spawn_blocking(move || {
        hydrate_canonical_contexts_blocking(
            &chain_store,
            expectations,
            &intrinsic_overlay,
            require_settled,
        )
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })?
}

fn hydrate_canonical_contexts_blocking(
    chain_store: &PrimaryChainStore,
    expectations: Vec<CanonicalBlockExpectation>,
    intrinsic_overlay: &HashMap<TransactionId, TransactionIntrinsicValueBalancesArtifact>,
    require_settled: bool,
) -> Result<Vec<zinder_derive::BlockCommitContext>, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    validate_hydration_boundary(&reader, &expectations, require_settled)?;
    let hydrated = load_paid_fee_transactions(&reader, &expectations, intrinsic_overlay)?;
    let transparent_spends = resolve_transparent_spends(&reader, &hydrated)?;
    Ok(build_paid_fee_contexts(
        expectations,
        hydrated,
        transparent_spends,
    ))
}

fn validate_hydration_boundary(
    reader: &ChainEpochReader<'_>,
    expectations: &[CanonicalBlockExpectation],
    require_settled: bool,
) -> Result<(), IngestError> {
    let boundary = if require_settled {
        reader.chain_epoch().settled_tip_height
    } else {
        reader.chain_epoch().visible_tip_height
    };
    let through_height = expectations
        .last()
        .map(|block| block.header.height)
        .ok_or_else(|| {
            IngestError::DeriveDispatch("paid-fee context hydration received no blocks".to_owned())
        })?;
    if through_height > boundary {
        return Err(IngestError::DeriveDispatch(
            "paid-fee context hydration crossed its canonical boundary".to_owned(),
        ));
    }
    Ok(())
}

fn load_paid_fee_transactions(
    reader: &ChainEpochReader<'_>,
    expectations: &[CanonicalBlockExpectation],
    intrinsic_overlay: &HashMap<TransactionId, TransactionIntrinsicValueBalancesArtifact>,
) -> Result<HydratedPaidFeeTransactions, IngestError> {
    let transaction_ids: Vec<_> = expectations
        .iter()
        .flat_map(|block| {
            block
                .transaction_locations
                .iter()
                .map(|location| location.transaction_id)
        })
        .collect();
    let mut facts_by_id = reader.transaction_facts_by_ids(&transaction_ids)?;
    let mut hydrated = HydratedPaidFeeTransactions {
        transactions_by_block: Vec::with_capacity(expectations.len()),
        intrinsic_by_id: HashMap::new(),
        transparent_parent_ids: HashSet::new(),
    };
    for expected in expectations {
        validate_expected_header(reader, expected)?;
        let mut transactions = Vec::with_capacity(expected.transaction_locations.len());
        for location in &expected.transaction_locations {
            let transaction = facts_by_id
                .remove(&location.transaction_id)
                .flatten()
                .ok_or_else(|| missing_transaction_facts_error(location.transaction_id))?;
            if transaction.location != *location {
                return Err(IngestError::DeriveDispatch(format!(
                    "canonical transaction facts {} changed location during paid-fee hydration",
                    hex::encode(location.transaction_id.as_bytes())
                )));
            }
            let intrinsic = load_intrinsic_value_balances(reader, intrinsic_overlay, location)?;
            hydrated
                .intrinsic_by_id
                .insert(location.transaction_id, intrinsic.value_balances);
            hydrated.transparent_parent_ids.extend(
                transaction
                    .transparent_inputs
                    .iter()
                    .map(|input| input.spent_outpoint.transaction_id),
            );
            transactions.push(transaction);
        }
        hydrated.transactions_by_block.push(transactions);
    }
    Ok(hydrated)
}

fn validate_expected_header(
    reader: &ChainEpochReader<'_>,
    expected: &CanonicalBlockExpectation,
) -> Result<(), IngestError> {
    let current_header = reader
        .block_header_at(expected.header.height)?
        .ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "canonical paid-fee header {} disappeared during hydration",
                expected.header.height.value()
            ))
        })?;
    if current_header != expected.header {
        return Err(IngestError::DeriveDispatch(format!(
            "canonical paid-fee header {} changed during hydration",
            expected.header.height.value()
        )));
    }
    Ok(())
}

fn missing_transaction_facts_error(transaction_id: TransactionId) -> IngestError {
    IngestError::DeriveDispatch(format!(
        "canonical transaction facts {} are unavailable for paid-fee hydration",
        hex::encode(transaction_id.as_bytes())
    ))
}

fn load_intrinsic_value_balances(
    reader: &ChainEpochReader<'_>,
    intrinsic_overlay: &HashMap<TransactionId, TransactionIntrinsicValueBalancesArtifact>,
    location: &TransactionLocation,
) -> Result<TransactionIntrinsicValueBalancesArtifact, IngestError> {
    match intrinsic_overlay.get(&location.transaction_id).copied() {
        Some(artifact) if artifact.location == *location => Ok(artifact),
        Some(_) => Err(IngestError::DeriveDispatch(
            "source-derived intrinsic balance location became stale".to_owned(),
        )),
        None => reader
            .transaction_intrinsic_value_balances_by_id(location.transaction_id)?
            .ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "intrinsic value balances {} are unavailable after enrichment",
                    hex::encode(location.transaction_id.as_bytes())
                ))
            }),
    }
}

fn resolve_transparent_spends(
    reader: &ChainEpochReader<'_>,
    hydrated: &HydratedPaidFeeTransactions,
) -> Result<HashMap<TransparentOutPoint, TransparentSpendFact>, IngestError> {
    let parent_ids: Vec<_> = hydrated.transparent_parent_ids.iter().copied().collect();
    let parent_facts = reader.transaction_facts_by_ids(&parent_ids)?;
    let mut transparent_spends = HashMap::new();
    for transaction in hydrated.transactions_by_block.iter().flatten() {
        for input in &transaction.transparent_inputs {
            let parent = parent_facts
                .get(&input.spent_outpoint.transaction_id)
                .and_then(|facts| facts.as_ref())
                .ok_or_else(|| missing_parent_transaction_error(input.spent_outpoint))?;
            let output = parent
                .transparent_outputs
                .iter()
                .find(|output| output.output_index == input.spent_outpoint.output_index)
                .ok_or(IngestError::TransparentOutputOutputMissing {
                    transaction_id: input.spent_outpoint.transaction_id,
                    output_index: input.spent_outpoint.output_index,
                })?;
            let spend = TransparentSpendFact::new(
                input.spent_outpoint,
                input.input_index,
                transaction.location.transaction_id,
                transaction.location.tx_index_in_block,
                transaction.location.block_height,
                transaction.location.block_hash,
                output.value_zat,
                output.address_script_hash,
                parent.location.block_height,
                parent.location.block_hash,
            );
            if transparent_spends
                .insert(input.spent_outpoint, spend)
                .is_some()
            {
                return Err(IngestError::DeriveDispatch(
                    "paid-fee hydration encountered a repeated transparent outpoint".to_owned(),
                ));
            }
        }
    }
    Ok(transparent_spends)
}

fn missing_parent_transaction_error(outpoint: TransparentOutPoint) -> IngestError {
    IngestError::DeriveDispatch(format!(
        "paid-fee transparent parent transaction {} is unavailable",
        hex::encode(outpoint.transaction_id.as_bytes())
    ))
}

fn build_paid_fee_contexts(
    expectations: Vec<CanonicalBlockExpectation>,
    hydrated: HydratedPaidFeeTransactions,
    transparent_spends: HashMap<TransparentOutPoint, TransparentSpendFact>,
) -> Vec<zinder_derive::BlockCommitContext> {
    let transparent_spends = Arc::new(transparent_spends);
    let intrinsic_by_id = Arc::new(hydrated.intrinsic_by_id);
    expectations
        .into_iter()
        .zip(hydrated.transactions_by_block)
        .map(|(expected, transactions)| {
            zinder_derive::BlockCommitContext::new(
                zinder_derive::BlockCommitPayload {
                    height: expected.header.height,
                    block_hash: expected.header.block_hash,
                    previous_block_hash: expected.header.parent_hash,
                    block_time_unix_seconds: expected.header.block_time,
                    block_size_bytes: expected.header.block_size_bytes,
                    transactions,
                    final_note_commitment_roots: None,
                },
                TransparentSpendFacts::from_map(Arc::clone(&transparent_spends)),
            )
            .with_transaction_intrinsic_value_balances(
                TransactionIntrinsicValueBalanceFacts::from_map(Arc::clone(&intrinsic_by_id)),
            )
        })
        .collect()
}

fn next_prepend_end(
    coverage: Option<PaidFeeDistributionBackfillCoverage>,
    tail_boundary: BlockHeight,
    target_floor: BlockHeight,
) -> Option<BlockHeight> {
    let next = match coverage {
        Some(coverage) if coverage.complete_from_height <= target_floor => return None,
        Some(coverage) => coverage.complete_from_height.value().checked_sub(1)?,
        None => {
            let next = tail_boundary.value().checked_sub(1)?;
            if next < target_floor.value() {
                return None;
            }
            next
        }
    };
    Some(BlockHeight::new(next))
}

fn backward_batch_start(
    target_floor: BlockHeight,
    batch_end: BlockHeight,
    batch_blocks: NonZeroU32,
) -> BlockHeight {
    BlockHeight::new(
        batch_end
            .value()
            .saturating_sub(batch_blocks.get().saturating_sub(1))
            .max(target_floor.value()),
    )
}

fn forward_batch_end(
    batch_start: BlockHeight,
    target_end: BlockHeight,
    batch_blocks: NonZeroU32,
) -> BlockHeight {
    BlockHeight::new(
        batch_start
            .value()
            .saturating_add(batch_blocks.get().saturating_sub(1))
            .min(target_end.value()),
    )
}

fn inclusive_heights(
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> impl Iterator<Item = BlockHeight> {
    (from_height.value()..=through_height.value()).map(BlockHeight::new)
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

    fn nonzero(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap_or(NonZeroU32::MIN)
    }

    #[test]
    fn first_batch_ends_immediately_before_live_tail() {
        assert_eq!(
            next_prepend_end(None, BlockHeight::new(1_001), BlockHeight::new(100)),
            Some(BlockHeight::new(1_000))
        );
    }

    #[test]
    fn subsequent_batch_prepends_existing_coverage() {
        let coverage = PaidFeeDistributionBackfillCoverage::new(
            BlockHeight::new(900),
            BlockHeight::new(1_000),
            1_700_000_000,
            1_700_001_000,
        );
        assert_eq!(
            next_prepend_end(
                Some(coverage),
                BlockHeight::new(1_001),
                BlockHeight::new(100)
            ),
            Some(BlockHeight::new(899))
        );
    }

    #[test]
    fn backfill_stops_after_reaching_durable_floor() {
        let coverage = PaidFeeDistributionBackfillCoverage::new(
            BlockHeight::new(100),
            BlockHeight::new(1_000),
            1_600_000_000,
            1_700_001_000,
        );
        assert_eq!(
            next_prepend_end(
                Some(coverage),
                BlockHeight::new(1_001),
                BlockHeight::new(100)
            ),
            None
        );
    }

    #[test]
    fn genesis_tail_boundary_completes_instead_of_underflowing() {
        assert_eq!(
            next_prepend_end(None, BlockHeight::new(0), BlockHeight::new(0)),
            None
        );
    }

    #[test]
    fn tail_boundary_at_or_below_floor_completes_without_inverting_the_batch() {
        assert_eq!(
            next_prepend_end(None, BlockHeight::new(1), BlockHeight::new(1)),
            None
        );
        assert_eq!(
            next_prepend_end(None, BlockHeight::new(1), BlockHeight::new(5)),
            None
        );
        assert_eq!(
            next_prepend_end(None, BlockHeight::new(10), BlockHeight::new(5)),
            Some(BlockHeight::new(9))
        );
    }

    #[test]
    fn backward_batch_is_clipped_at_target_floor() {
        assert_eq!(
            backward_batch_start(BlockHeight::new(850), BlockHeight::new(900), nonzero(100)),
            BlockHeight::new(850)
        );
        assert_eq!(
            backward_batch_start(BlockHeight::new(1), BlockHeight::new(900), nonzero(100)),
            BlockHeight::new(801)
        );
    }

    #[test]
    fn target_floor_expansion_is_monotonic() {
        let persisted = BlockHeight::new(500);
        let requested_shorter = BlockHeight::new(600);
        let requested_longer = BlockHeight::new(400);
        assert_eq!(persisted.min(requested_shorter), persisted);
        assert_eq!(persisted.min(requested_longer), requested_longer);
    }

    #[test]
    fn matching_cursor_does_not_suppress_incomplete_tail_repair() {
        assert!(should_seed_tail(false, false, true));
        assert!(!should_seed_tail(false, false, false));
    }

    #[test]
    fn settled_tail_reconciliation_is_bounded_and_resumable() {
        let first = next_settled_tail_reconciliation_range(
            BlockHeight::new(1_001),
            BlockHeight::new(1_250),
            None,
            nonzero(100),
        );
        assert_eq!(
            first,
            Some((BlockHeight::new(1_001), BlockHeight::new(1_100)))
        );
        let resumed = next_settled_tail_reconciliation_range(
            BlockHeight::new(1_001),
            BlockHeight::new(1_250),
            Some(SettledTailReconciliationCoverage {
                complete_from_height: BlockHeight::new(1_001),
                complete_through_height: BlockHeight::new(1_100),
            }),
            nonzero(100),
        );
        assert_eq!(
            resumed,
            Some((BlockHeight::new(1_101), BlockHeight::new(1_200)))
        );
    }

    #[test]
    fn widened_tail_restarts_reconciliation_at_earlier_boundary() {
        let range = next_settled_tail_reconciliation_range(
            BlockHeight::new(900),
            BlockHeight::new(1_250),
            Some(SettledTailReconciliationCoverage {
                complete_from_height: BlockHeight::new(1_001),
                complete_through_height: BlockHeight::new(1_200),
            }),
            nonzero(100),
        );
        assert_eq!(range, Some((BlockHeight::new(900), BlockHeight::new(999))));
    }
}
