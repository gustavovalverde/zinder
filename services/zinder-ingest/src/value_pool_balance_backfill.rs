//! Resumable source-backed cumulative value-pool balance history.

use std::{collections::BTreeMap, num::NonZeroU32, sync::Arc, time::Duration};

use futures_util::{StreamExt as _, stream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, BlockId, BlockValuePoolBalances};
use zinder_derive::{
    BlockCommitContext, BlockCommitPayload, BlockValuePoolBalanceFacts, DeriveStore,
    TransparentSpendFacts, ValuePoolBalanceBackfillCoverage, ValuePoolBalanceHistoryConsumer,
};
use zinder_source::{NodeCapability, NodeSource};
use zinder_store::PrimaryChainStore;

use crate::{IngestError, derive_consumers::derive_projection_write_guard};

const BACKFILL_START_HEIGHT: BlockHeight = BlockHeight::new(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SECONDS_PER_DAY: i64 = 86_400;

/// Bounded controls for cumulative value-pool history synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValuePoolBalanceBackfillConfig {
    /// Whether historical and live-tail synchronization run.
    pub enabled: bool,
    /// Maximum canonical heights scanned per durable historical update.
    pub batch_blocks: NonZeroU32,
    /// Maximum concurrent verbose historical block requests.
    pub fetch_concurrency: NonZeroU32,
}

/// Existing process handles used by cumulative pool history synchronization.
#[derive(Clone)]
pub struct ValuePoolBalanceBackfillContext {
    request_timeout: Duration,
    source: Arc<dyn NodeSource>,
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl ValuePoolBalanceBackfillContext {
    /// Groups source and storage handles for task startup.
    #[must_use]
    pub fn new(
        request_timeout: Duration,
        source: Arc<dyn NodeSource>,
        chain_store: PrimaryChainStore,
        derive_store: DeriveStore,
    ) -> Self {
        Self {
            request_timeout,
            source,
            chain_store,
            derive_store,
        }
    }
}

/// Spawns the non-readiness-blocking historical and live-tail synchronizer.
#[must_use = "await the handle during shutdown"]
pub fn spawn_value_pool_balance_backfill_task(
    config: ValuePoolBalanceBackfillConfig,
    context: ValuePoolBalanceBackfillContext,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            let outcome = synchronize_once(config, &context).await;
            let delay = match outcome {
                Ok(SyncProgress::Advanced {
                    historical_from,
                    historical_through,
                    tail_through,
                }) => {
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "value_pool_balance_backfill_progress",
                        historical_from_height = historical_from.map(BlockHeight::value),
                        historical_through_height = historical_through.map(BlockHeight::value),
                        tail_through_height = tail_through.map(BlockHeight::value),
                        "value-pool balance history advanced"
                    );
                    Duration::ZERO
                }
                Ok(SyncProgress::CaughtUp) => CAUGHT_UP_POLL_INTERVAL,
                Err(error) => {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "value_pool_balance_backfill_retry",
                        error = %error,
                        "value-pool balance history synchronization failed; retrying"
                    );
                    RETRY_INTERVAL
                }
            };
            if delay.is_zero() {
                if cancel.is_cancelled() {
                    return;
                }
                tokio::task::yield_now().await;
            } else if sleep_or_cancel(delay, &cancel).await {
                return;
            }
        }
    }))
}

enum SyncProgress {
    Advanced {
        historical_from: Option<BlockHeight>,
        historical_through: Option<BlockHeight>,
        tail_through: Option<BlockHeight>,
    },
    CaughtUp,
}

async fn synchronize_once(
    config: ValuePoolBalanceBackfillConfig,
    context: &ValuePoolBalanceBackfillContext,
) -> Result<SyncProgress, IngestError> {
    if !context
        .source
        .capabilities()
        .supports(NodeCapability::BlockValuePoolBalances)
    {
        return Err(IngestError::DeriveDispatch(
            "node source does not expose historical block value-pool balances".to_owned(),
        ));
    }

    let historical = backfill_next_historical_batch(config, context).await?;
    let tail = synchronize_live_tail(config, context).await?;
    match (historical, tail) {
        (HistoricalProgress::CaughtUp, TailProgress::CaughtUp) => Ok(SyncProgress::CaughtUp),
        (historical, tail) => Ok(SyncProgress::Advanced {
            historical_from: historical.historical_from(),
            historical_through: historical.through_height(),
            tail_through: tail.through_height(),
        }),
    }
}

enum HistoricalProgress {
    Advanced {
        from: BlockHeight,
        through: BlockHeight,
    },
    CaughtUp,
}

impl HistoricalProgress {
    const fn historical_from(&self) -> Option<BlockHeight> {
        match self {
            Self::Advanced { from, .. } => Some(*from),
            Self::CaughtUp => None,
        }
    }

    const fn through_height(&self) -> Option<BlockHeight> {
        match self {
            Self::Advanced { through, .. } => Some(*through),
            Self::CaughtUp => None,
        }
    }
}

async fn backfill_next_historical_batch(
    config: ValuePoolBalanceBackfillConfig,
    context: &ValuePoolBalanceBackfillContext,
) -> Result<HistoricalProgress, IngestError> {
    let coverage = ValuePoolBalanceHistoryConsumer::backfill_coverage(&context.derive_store)?;
    let next = coverage.map_or(Some(BACKFILL_START_HEIGHT), |current| {
        current.complete_through_height.next()
    });
    let Some(next) = next else {
        return Ok(HistoricalProgress::CaughtUp);
    };
    let Some(epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(HistoricalProgress::CaughtUp);
    };
    if next > epoch.settled_tip_height {
        return Ok(HistoricalProgress::CaughtUp);
    }
    let through = batch_end(next, epoch.settled_tip_height, config.batch_blocks);
    let candidates = retain_unmaterialized_candidates(
        &context.derive_store,
        read_daily_candidates(&context.chain_store, next, through)?,
    )?;
    let contexts = hydrate_candidates(config, context, candidates).await?;
    let next_coverage = ValuePoolBalanceBackfillCoverage::new(
        coverage.map_or(BACKFILL_START_HEIGHT, |current| {
            current.complete_from_height
        }),
        through,
    );
    {
        let _write_guard = derive_projection_write_guard();
        ValuePoolBalanceHistoryConsumer::new()
            .write_backfill_batch(&context.derive_store, &contexts, next_coverage)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
    Ok(HistoricalProgress::Advanced {
        from: next,
        through,
    })
}

fn retain_unmaterialized_candidates(
    derive_store: &DeriveStore,
    candidates: Vec<CanonicalBalanceBlock>,
) -> Result<Vec<CanonicalBalanceBlock>, IngestError> {
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let existing = ValuePoolBalanceHistoryConsumer::point_at_height(
                derive_store,
                candidate.block_id.height,
            )
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()));
            match existing {
                Ok(None) => Some(Ok(candidate)),
                Ok(Some(point))
                    if point.block_hash == candidate.block_id.hash
                        && point.block_time_unix_seconds
                            == candidate.block_time_unix_seconds =>
                {
                    None
                }
                Ok(Some(_)) => Some(Err(IngestError::DeriveDispatch(format!(
                    "materialized value-pool balance at height {} disagrees with the canonical block",
                    candidate.block_id.height.value()
                )))),
                Err(error) => Some(Err(error)),
            }
        })
        .collect()
}

enum TailProgress {
    Advanced { through: Option<BlockHeight> },
    CaughtUp,
}

impl TailProgress {
    const fn through_height(&self) -> Option<BlockHeight> {
        match self {
            Self::Advanced { through } => *through,
            Self::CaughtUp => None,
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one reconciliation transaction derives stale and replacement tail ranges together"
)]
async fn synchronize_live_tail(
    config: ValuePoolBalanceBackfillConfig,
    context: &ValuePoolBalanceBackfillContext,
) -> Result<TailProgress, IngestError> {
    let Some(epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(TailProgress::CaughtUp);
    };
    let Some(boundary) = epoch.settled_tip_height.next() else {
        return Ok(TailProgress::CaughtUp);
    };
    {
        let _write_guard = derive_projection_write_guard();
        ValuePoolBalanceHistoryConsumer::move_tail_boundary_for_sync(
            &context.derive_store,
            boundary,
        )
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
    let tail = ValuePoolBalanceHistoryConsumer::tail_coverage(&context.derive_store)?.ok_or_else(
        || {
            IngestError::DeriveDispatch(
                "value-pool balance live-tail coverage disappeared".to_owned(),
            )
        },
    )?;
    let canonical = if boundary <= epoch.visible_tip_height {
        read_canonical_blocks(&context.chain_store, boundary, epoch.visible_tip_height)?
    } else {
        Vec::new()
    };

    let mut first_mismatch = None;
    for block in &canonical {
        if tail
            .complete_through_height
            .is_some_and(|through| block.block_id.height <= through)
        {
            let stored = ValuePoolBalanceHistoryConsumer::point_at_height(
                &context.derive_store,
                block.block_id.height,
            )
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
            if stored.is_none_or(|point| point.block_hash != block.block_id.hash) {
                first_mismatch = Some(block.block_id.height);
                break;
            }
        }
    }
    let truncate_from = if tail
        .complete_through_height
        .is_some_and(|through| through > epoch.visible_tip_height)
    {
        Some(
            first_mismatch
                .map_or_else(|| epoch.visible_tip_height.next(), Some)
                .ok_or_else(|| {
                    IngestError::DeriveDispatch(
                        "value-pool balance tail truncation height overflow".to_owned(),
                    )
                })?,
        )
    } else {
        first_mismatch
    };
    let mut reverted = Vec::new();
    if let (Some(from), Some(through)) = (truncate_from, tail.complete_through_height) {
        for height in (from.value()..=through.value()).rev() {
            reverted.push(BlockHeight::new(height));
        }
    }
    let append_from = truncate_from.or_else(|| {
        tail.complete_through_height
            .map_or(Some(boundary), BlockHeight::next)
    });
    let replacement_candidates = append_from.map_or_else(Vec::new, |from| {
        canonical
            .iter()
            .filter(|block| block.block_id.height >= from)
            .copied()
            .collect()
    });
    if reverted.is_empty() && replacement_candidates.is_empty() {
        return Ok(TailProgress::CaughtUp);
    }
    let replacement_contexts = hydrate_candidates(config, context, replacement_candidates).await?;
    {
        let _write_guard = derive_projection_write_guard();
        ValuePoolBalanceHistoryConsumer::new()
            .reconcile_tail(&context.derive_store, &reverted, &replacement_contexts)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    }
    Ok(TailProgress::Advanced {
        through: ValuePoolBalanceHistoryConsumer::tail_coverage(&context.derive_store)?
            .and_then(|coverage| coverage.complete_through_height),
    })
}

#[derive(Clone, Copy)]
struct CanonicalBalanceBlock {
    block_id: BlockId,
    block_time_unix_seconds: i64,
    balances_present: bool,
}

fn read_daily_candidates(
    chain_store: &PrimaryChainStore,
    from: BlockHeight,
    through: BlockHeight,
) -> Result<Vec<CanonicalBalanceBlock>, IngestError> {
    let blocks = read_canonical_blocks(chain_store, from, through)?;
    let mut by_day = BTreeMap::new();
    for block in blocks {
        by_day.insert(
            block.block_time_unix_seconds.div_euclid(SECONDS_PER_DAY),
            block,
        );
    }
    Ok(by_day.into_values().collect())
}

fn read_canonical_blocks(
    chain_store: &PrimaryChainStore,
    from: BlockHeight,
    through: BlockHeight,
) -> Result<Vec<CanonicalBalanceBlock>, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    if through > reader.chain_epoch().visible_tip_height {
        return Err(IngestError::DeriveDispatch(
            "value-pool balance scan crossed the visible canonical tip".to_owned(),
        ));
    }
    (from.value()..=through.value())
        .map(|height| {
            let height = BlockHeight::new(height);
            let header = reader.block_header_at(height)?.ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "canonical block header {} is unavailable for value-pool history",
                    height.value()
                ))
            })?;
            Ok(CanonicalBalanceBlock {
                block_id: BlockId::new(height, header.block_hash),
                block_time_unix_seconds: header.block_time,
                balances_present: reader.block_value_pool_balances_at(height)?.is_some(),
            })
        })
        .collect()
}

async fn hydrate_candidates(
    config: ValuePoolBalanceBackfillConfig,
    context: &ValuePoolBalanceBackfillContext,
    candidates: Vec<CanonicalBalanceBlock>,
) -> Result<Vec<BlockCommitContext>, IngestError> {
    let missing = candidates
        .iter()
        .filter_map(|candidate| (!candidate.balances_present).then_some(candidate.block_id))
        .collect();
    let fetched = fetch_balances(
        Arc::clone(&context.source),
        context.request_timeout,
        config.fetch_concurrency,
        missing,
    )
    .await?;
    if !fetched.is_empty() {
        let chain_store = context.chain_store.clone();
        tokio::task::spawn_blocking(move || chain_store.enrich_block_value_pool_balances(&fetched))
            .await
            .map_err(|error| IngestError::BlockingTaskFailed {
                reason: error.to_string(),
            })??;
    }

    let reader = context.chain_store.current_chain_epoch_reader()?;
    candidates
        .into_iter()
        .map(|candidate| {
            let header = reader
                .block_header_at(candidate.block_id.height)?
                .ok_or_else(|| {
                    IngestError::DeriveDispatch(format!(
                        "canonical block {} disappeared after value-pool enrichment",
                        candidate.block_id.height.value()
                    ))
                })?;
            if header.block_hash != candidate.block_id.hash
                || header.block_time != candidate.block_time_unix_seconds
            {
                return Err(IngestError::DeriveDispatch(
                    "canonical block changed during value-pool enrichment".to_owned(),
                ));
            }
            let balances = reader
                .block_value_pool_balances_at(candidate.block_id.height)?
                .ok_or_else(|| {
                    IngestError::DeriveDispatch(format!(
                        "value-pool balances {} remain unavailable after enrichment",
                        candidate.block_id.height.value()
                    ))
                })?;
            Ok(BlockCommitContext::new(
                BlockCommitPayload {
                    height: candidate.block_id.height,
                    block_hash: candidate.block_id.hash,
                    previous_block_hash: header.parent_hash,
                    block_time_unix_seconds: header.block_time,
                    block_size_bytes: header.block_size_bytes,
                    transactions: Vec::new(),
                    final_note_commitment_roots: None,
                },
                TransparentSpendFacts::Offline,
            )
            .with_block_value_pool_balances(BlockValuePoolBalanceFacts::from_pools(balances.pools)))
        })
        .collect()
}

async fn fetch_balances(
    source: Arc<dyn NodeSource>,
    request_timeout: Duration,
    fetch_concurrency: NonZeroU32,
    block_ids: Vec<BlockId>,
) -> Result<Vec<BlockValuePoolBalances>, IngestError> {
    let mut fetches = stream::iter(block_ids.into_iter().map(|block_id| {
        let source = Arc::clone(&source);
        async move {
            tokio::time::timeout(
                request_timeout,
                source.fetch_block_value_pool_balances(block_id),
            )
            .await
            .map_err(|error| IngestError::SourceRetryDeadlineExceeded {
                operation: format!(
                    "fetch value-pool balances at height {}",
                    block_id.height.value()
                ),
                reason: error.to_string(),
            })?
            .map_err(IngestError::from)
        }
    }))
    .buffer_unordered(usize::try_from(fetch_concurrency.get()).unwrap_or(usize::MAX));
    let mut balances = Vec::new();
    while let Some(fetch_outcome) = fetches.next().await {
        balances.push(fetch_outcome?);
    }
    balances.sort_unstable_by_key(|snapshot| snapshot.block_id.height);
    Ok(balances)
}

fn batch_end(from: BlockHeight, target: BlockHeight, batch_blocks: NonZeroU32) -> BlockHeight {
    BlockHeight::new(
        from.value()
            .saturating_add(batch_blocks.get().saturating_sub(1))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, ChainEpoch, ChainEpochId, ChainTipMetadata,
        CompactBlockArtifact, Network, UnixTimestampMillis, ValuePoolBalance,
    };
    use zinder_derive::{DeriveStoreOptions, VALUE_POOL_BALANCE_HISTORY_SCHEMA};
    use zinder_source::{NodeCapabilities, SourceBlock, SourceError};
    use zinder_store::{
        CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainStoreOptions, ReorgWindowChange,
        RocksDbResourceBudget,
    };

    use super::*;

    struct BalanceSource {
        capabilities: NodeCapabilities,
        request_count: AtomicUsize,
    }

    impl BalanceSource {
        fn new() -> Result<Self, zinder_source::NodeCapabilitiesError> {
            Ok(Self {
                capabilities: NodeCapabilities::new([NodeCapability::BlockValuePoolBalances])?,
                request_count: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl NodeSource for BalanceSource {
        fn capabilities(&self) -> NodeCapabilities {
            self.capabilities
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            Err(SourceError::BlockUnavailable {
                height,
                reason: "unused by value-pool balance backfill test".to_owned(),
            })
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Err(SourceError::NodeUnavailable {
                reason: "unused by value-pool balance backfill test".to_owned(),
            })
        }

        async fn fetch_block_value_pool_balances(
            &self,
            block_id: BlockId,
        ) -> Result<BlockValuePoolBalances, SourceError> {
            self.request_count.fetch_add(1, Ordering::SeqCst);
            Ok(BlockValuePoolBalances::new(
                block_id,
                block_time(block_id.height),
                vec![
                    ValuePoolBalance::new(
                        "transparent",
                        true,
                        Some(u64::from(block_id.height.value())),
                    ),
                    ValuePoolBalance::new("future-pool", false, None),
                ],
            ))
        }
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario proves initial backfill, ownership transfer, restart, and reorg replacement"
    )]
    async fn daily_backfill_and_live_tail_resume_and_replace_reorged_hash() -> eyre::Result<()> {
        let tempdir = tempdir()?;
        let chain_store = PrimaryChainStore::open(
            tempdir.path().join("canonical"),
            ChainStoreOptions::for_local_tests(),
        )?;
        let derive_store = DeriveStore::open(
            tempdir.path().join("derive"),
            DeriveStoreOptions {
                sync_writes: false,
                consumers: &[VALUE_POOL_BALANCE_HISTORY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        commit_initial_chain(&chain_store, 1_002, 1_000)?;
        let source = Arc::new(BalanceSource::new()?);
        let context = ValuePoolBalanceBackfillContext::new(
            Duration::from_secs(1),
            source.clone(),
            chain_store.clone(),
            derive_store.clone(),
        );
        let config = ValuePoolBalanceBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::new(1_000)
                .ok_or_else(|| eyre::eyre!("batch must be nonzero"))?,
            fetch_concurrency: NonZeroU32::new(4)
                .ok_or_else(|| eyre::eyre!("concurrency must be nonzero"))?,
        };

        assert!(matches!(
            synchronize_once(config, &context).await?,
            SyncProgress::Advanced { .. }
        ));
        assert_eq!(
            ValuePoolBalanceHistoryConsumer::backfill_coverage(&derive_store)?,
            Some(ValuePoolBalanceBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(1_000),
            ))
        );
        assert_eq!(
            ValuePoolBalanceHistoryConsumer::tail_coverage(&derive_store)?,
            Some(zinder_derive::ValuePoolBalanceTailCoverage {
                boundary_height: BlockHeight::new(1_001),
                complete_through_height: Some(BlockHeight::new(1_002)),
            })
        );
        let historical_days = ValuePoolBalanceHistoryConsumer::read_newest_days(&derive_store, 10)?;
        assert_eq!(historical_days.len(), 3);
        assert_eq!(source.request_count.load(Ordering::SeqCst), 5);

        advance_settled_tip(&chain_store, 2, 1_001)?;
        assert!(matches!(
            synchronize_once(config, &context).await?,
            SyncProgress::Advanced { .. }
        ));
        assert_eq!(
            ValuePoolBalanceHistoryConsumer::backfill_coverage(&derive_store)?,
            Some(ValuePoolBalanceBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(1_001),
            ))
        );
        assert_eq!(source.request_count.load(Ordering::SeqCst), 5);

        assert!(matches!(
            synchronize_once(config, &context).await?,
            SyncProgress::CaughtUp
        ));
        assert_eq!(source.request_count.load(Ordering::SeqCst), 5);

        let replacement_hash = block_hash(1_002, 0x80);
        commit_replacement(&chain_store, replacement_hash)?;
        assert!(matches!(
            synchronize_once(config, &context).await?,
            SyncProgress::Advanced { .. }
        ));
        assert_eq!(source.request_count.load(Ordering::SeqCst), 6);
        assert_eq!(
            ValuePoolBalanceHistoryConsumer::point_at_height(
                &derive_store,
                BlockHeight::new(1_002),
            )?
            .map(|point| point.block_hash),
            Some(replacement_hash)
        );
        Ok(())
    }

    fn advance_settled_tip(
        store: &PrimaryChainStore,
        epoch_id: u64,
        settled_tip: u32,
    ) -> eyre::Result<()> {
        let tip_hash = block_hash(1_002, 0);
        store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                epoch(
                    epoch_id,
                    1_002,
                    tip_hash,
                    settled_tip,
                    block_hash(settled_tip, 0),
                ),
                Vec::new(),
                Vec::new(),
            )
            .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo {
                height: BlockHeight::new(settled_tip),
            }),
        )?;
        Ok(())
    }

    fn commit_initial_chain(
        store: &PrimaryChainStore,
        visible_tip: u32,
        settled_tip: u32,
    ) -> eyre::Result<()> {
        let mut headers = Vec::new();
        let mut compact = Vec::new();
        let mut parent = BlockHash::from_bytes([0; 32]);
        for height in 1..=visible_tip {
            let hash = block_hash(height, 0);
            headers.push(header(height, hash, parent));
            compact.push(CompactBlockArtifact::new(
                BlockHeight::new(height),
                hash,
                vec![height.to_le_bytes()[0]],
            ));
            parent = hash;
        }
        let epoch = epoch(
            1,
            visible_tip,
            parent,
            settled_tip,
            block_hash(settled_tip, 0),
        );
        store.commit_chain_epoch(ChainEpochArtifacts::new(epoch, headers, compact))?;
        Ok(())
    }

    fn commit_replacement(
        store: &PrimaryChainStore,
        replacement_hash: BlockHash,
    ) -> eyre::Result<()> {
        let parent = block_hash(1_001, 0);
        let replacement = header(1_002, replacement_hash, parent);
        let epoch = epoch(3, 1_002, replacement_hash, 1_001, block_hash(1_001, 0));
        store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                epoch,
                vec![replacement],
                vec![CompactBlockArtifact::new(
                    BlockHeight::new(1_002),
                    replacement_hash,
                    vec![0x80],
                )],
            )
            .with_reorg_window_change(ReorgWindowChange::Replace {
                from_height: BlockHeight::new(1_002),
            }),
        )?;
        Ok(())
    }

    fn epoch(
        id: u64,
        visible_tip: u32,
        visible_hash: BlockHash,
        settled_tip: u32,
        settled_hash: BlockHash,
    ) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(visible_tip),
            visible_tip_hash: visible_hash,
            settled_tip_height: BlockHeight::new(settled_tip),
            settled_tip_hash: settled_hash,
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(id),
        }
    }

    fn header(height: u32, hash: BlockHash, parent: BlockHash) -> BlockHeaderArtifact {
        BlockHeaderArtifact::new(
            BlockHeight::new(height),
            hash,
            parent,
            [0; 32],
            [0; 32],
            block_time(BlockHeight::new(height)),
            0,
            [0; 32],
            0,
            1,
        )
    }

    fn block_time(height: BlockHeight) -> i64 {
        i64::from(height.value()) * 225
    }

    fn block_hash(height: u32, salt: u8) -> BlockHash {
        let mut bytes = [salt; 32];
        bytes[..4].copy_from_slice(&height.to_be_bytes());
        BlockHash::from_bytes(bytes)
    }
}
