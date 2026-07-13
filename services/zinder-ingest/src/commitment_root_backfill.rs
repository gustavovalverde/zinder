//! Resumable settled-history backfill for final note-commitment roots.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use futures_util::{StreamExt as _, stream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockFinalNoteCommitmentRoots, BlockHeight, BlockId, NetworkUpgradeActivations};
use zinder_derive::{
    BlockCommitContext, BlockCommitPayload, CommitmentRootBackfillCoverage,
    CommitmentRootSearchConsumer, DeriveStore, TransparentSpendFacts,
};
use zinder_runtime::Readiness;
use zinder_source::NodeSource;
use zinder_store::PrimaryChainStore;

use crate::{IngestError, ingest_loop::wait_until_tip_follow_or_cancelled};

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const BACKFILL_CAUGHT_UP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Bounded controls for the ingest-owned commitment-root backfill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitmentRootBackfillConfig {
    /// Whether the background task runs.
    pub enabled: bool,
    /// Maximum settled canonical blocks processed per durable coverage update.
    pub batch_blocks: NonZeroU32,
    /// Maximum concurrent historical tree-state requests.
    pub fetch_concurrency: NonZeroU32,
}

/// Existing process handles used by the commitment-root backfill.
#[derive(Clone)]
pub struct CommitmentRootBackfillContext {
    request_timeout: Duration,
    activations: Arc<NetworkUpgradeActivations>,
    source: Arc<dyn NodeSource>,
    chain_store: PrimaryChainStore,
    derive_store: DeriveStore,
}

impl CommitmentRootBackfillContext {
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

/// Spawns the non-readiness-blocking settled-history backfill task.
#[must_use = "await the handle during shutdown"]
pub fn spawn_commitment_root_backfill_task(
    config: CommitmentRootBackfillConfig,
    context: CommitmentRootBackfillContext,
    readiness: Readiness,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if !config.enabled {
        tracing::info!(
            target: "zinder::ingest",
            event = "commitment_root_backfill_disabled",
            "commitment-root historical backfill is disabled"
        );
        return None;
    }

    Some(tokio::spawn(run_commitment_root_backfill(
        config, context, readiness, cancel,
    )))
}

async fn run_commitment_root_backfill(
    config: CommitmentRootBackfillConfig,
    context: CommitmentRootBackfillContext,
    readiness: Readiness,
    cancel: CancellationToken,
) {
    let Some(sapling_activation_height) = context.activations.activation_height_by_name("Sapling")
    else {
        tracing::warn!(
            target: "zinder::ingest",
            event = "commitment_root_backfill_retry",
            error = "network upgrade activations do not include Sapling",
            "commitment-root backfill cannot determine its historical floor"
        );
        cancel.cancelled().await;
        return;
    };
    tracing::info!(
        target: "zinder::ingest",
        event = "commitment_root_backfill_started",
        from_height = sapling_activation_height.value(),
        batch_blocks = config.batch_blocks.get(),
        fetch_concurrency = config.fetch_concurrency.get(),
        "commitment-root historical backfill started"
    );

    loop {
        if wait_until_tip_follow_or_cancelled(&readiness, &cancel).await {
            return;
        }
        let backfill = backfill_next_batch(config, sapling_activation_height, &context);
        let progress = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "commitment_root_backfill_cancelled",
                    "commitment-root historical backfill cancelled"
                );
                return;
            }
            progress = backfill => progress,
        };

        match progress {
            Ok(BackfillProgress::Advanced {
                from_height,
                through_height,
                fetched_roots,
            }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "commitment_root_backfill_progress",
                    from_height = from_height.value(),
                    through_height = through_height.value(),
                    fetched_roots,
                    "commitment-root historical backfill advanced"
                );
            }
            Ok(BackfillProgress::CaughtUp { through_height }) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "commitment_root_backfill_completed",
                    through_height = through_height.map(BlockHeight::value),
                    "commitment-root historical backfill is caught up to the settled tip"
                );
                if sleep_or_cancel(BACKFILL_CAUGHT_UP_POLL_INTERVAL, &cancel).await {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "commitment_root_backfill_retry",
                    error = %error,
                    retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                    "commitment-root historical backfill batch failed; retrying"
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
        fetched_roots: usize,
    },
    CaughtUp {
        through_height: Option<BlockHeight>,
    },
}

async fn backfill_next_batch(
    config: CommitmentRootBackfillConfig,
    sapling_activation_height: BlockHeight,
    context: &CommitmentRootBackfillContext,
) -> Result<BackfillProgress, IngestError> {
    let coverage = CommitmentRootSearchConsumer::backfill_coverage(&context.derive_store)?;
    let next_height = next_backfill_height(coverage, sapling_activation_height)?;
    let Some(chain_epoch) = context.chain_store.current_chain_epoch()? else {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    };
    if next_height > chain_epoch.settled_tip_height {
        return Ok(BackfillProgress::CaughtUp {
            through_height: coverage.map(|coverage| coverage.complete_through_height),
        });
    }
    let batch_end = BlockHeight::new(
        next_height
            .value()
            .saturating_add(config.batch_blocks.get().saturating_sub(1))
            .min(chain_epoch.settled_tip_height.value()),
    );
    let canonical_blocks = read_canonical_batch(&context.chain_store, next_height, batch_end)?;
    let missing_block_ids = missing_root_block_ids(&canonical_blocks);
    let fetched_root_count = missing_block_ids.len();
    let fetched_roots = fetch_missing_roots(
        Arc::clone(&context.source),
        context.request_timeout,
        config.fetch_concurrency,
        missing_block_ids,
    )
    .await?;
    if !fetched_roots.is_empty() {
        enrich_canonical_roots(context.chain_store.clone(), fetched_roots).await?;
    }

    let contexts = read_enriched_contexts(&context.chain_store, next_height, batch_end)?;
    let next_coverage = CommitmentRootBackfillCoverage::new(
        coverage.map_or(sapling_activation_height, |coverage| {
            coverage.complete_from_height
        }),
        batch_end,
    );
    write_root_search_batch(context.derive_store.clone(), contexts, next_coverage).await?;
    Ok(BackfillProgress::Advanced {
        from_height: next_height,
        through_height: batch_end,
        fetched_roots: fetched_root_count,
    })
}

fn next_backfill_height(
    coverage: Option<CommitmentRootBackfillCoverage>,
    sapling_activation_height: BlockHeight,
) -> Result<BlockHeight, IngestError> {
    let Some(coverage) = coverage else {
        return Ok(sapling_activation_height);
    };
    if coverage.complete_from_height != sapling_activation_height {
        return Err(IngestError::DeriveDispatch(format!(
            "commitment-root backfill coverage starts at {}, expected Sapling activation {}",
            coverage.complete_from_height.value(),
            sapling_activation_height.value()
        )));
    }
    coverage.complete_through_height.next().ok_or_else(|| {
        IngestError::DeriveDispatch("commitment-root backfill height overflow".to_owned())
    })
}

#[derive(Clone, Copy)]
struct CanonicalRootBlock {
    block_id: BlockId,
    roots: Option<BlockFinalNoteCommitmentRoots>,
}

fn read_canonical_batch(
    chain_store: &PrimaryChainStore,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<Vec<CanonicalRootBlock>, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    validate_settled_boundary(reader.chain_epoch().settled_tip_height, through_height)?;
    let mut blocks = Vec::with_capacity(height_count(from_height, through_height));
    for height in inclusive_heights(from_height, through_height) {
        let header = reader.block_header_at(height)?.ok_or_else(|| {
            IngestError::DeriveDispatch(format!(
                "settled canonical block header {} is unavailable",
                height.value()
            ))
        })?;
        blocks.push(CanonicalRootBlock {
            block_id: BlockId::new(height, header.block_hash),
            roots: reader.final_note_commitment_roots_at(height)?,
        });
    }
    Ok(blocks)
}

fn missing_root_block_ids(blocks: &[CanonicalRootBlock]) -> Vec<BlockId> {
    blocks
        .iter()
        .filter_map(|block| block.roots.is_none().then_some(block.block_id))
        .collect()
}

async fn fetch_missing_roots(
    source: Arc<dyn NodeSource>,
    request_timeout: Duration,
    fetch_concurrency: NonZeroU32,
    block_ids: Vec<BlockId>,
) -> Result<Vec<BlockFinalNoteCommitmentRoots>, IngestError> {
    let mut fetches = stream::iter(block_ids.into_iter().map(|block_id| {
        let source = Arc::clone(&source);
        async move {
            tokio::time::timeout(request_timeout, source.fetch_tree_state_for_block(block_id))
                .await
                .map_err(|error| IngestError::SourceRetryDeadlineExceeded {
                    operation: format!(
                        "fetch final note-commitment roots at height {}",
                        block_id.height.value()
                    ),
                    reason: error.to_string(),
                })?
                .map(|tree_state| tree_state.final_note_commitment_roots)
                .map_err(IngestError::from)
        }
    }))
    .buffer_unordered(usize::try_from(fetch_concurrency.get()).unwrap_or(usize::MAX));
    let mut roots = Vec::new();
    while let Some(root_result) = fetches.next().await {
        roots.push(root_result?);
    }
    roots.sort_unstable_by_key(|roots| roots.height);
    Ok(roots)
}

async fn enrich_canonical_roots(
    chain_store: PrimaryChainStore,
    roots: Vec<BlockFinalNoteCommitmentRoots>,
) -> Result<(), IngestError> {
    tokio::task::spawn_blocking(move || chain_store.enrich_final_note_commitment_roots(&roots))
        .await
        .map_err(|error| IngestError::BlockingTaskFailed {
            reason: error.to_string(),
        })??;
    Ok(())
}

fn read_enriched_contexts(
    chain_store: &PrimaryChainStore,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<Vec<BlockCommitContext>, IngestError> {
    let reader = chain_store.current_chain_epoch_reader()?;
    validate_settled_boundary(reader.chain_epoch().settled_tip_height, through_height)?;
    inclusive_heights(from_height, through_height)
        .map(|height| {
            let header = reader.block_header_at(height)?.ok_or_else(|| {
                IngestError::DeriveDispatch(format!(
                    "settled canonical block header {} is unavailable after enrichment",
                    height.value()
                ))
            })?;
            let roots = reader
                .final_note_commitment_roots_at(height)?
                .ok_or_else(|| {
                    IngestError::DeriveDispatch(format!(
                        "final note-commitment roots {} are unavailable after enrichment",
                        height.value()
                    ))
                })?;
            Ok(BlockCommitContext::new(
                BlockCommitPayload {
                    height,
                    block_hash: header.block_hash,
                    previous_block_hash: header.parent_hash,
                    block_time_unix_seconds: header.block_time,
                    block_size_bytes: header.block_size_bytes,
                    transactions: Vec::new(),
                    final_note_commitment_roots: Some(roots),
                },
                TransparentSpendFacts::Offline,
            ))
        })
        .collect()
}

fn validate_settled_boundary(
    settled_tip_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<(), IngestError> {
    if through_height <= settled_tip_height {
        return Ok(());
    }
    Err(IngestError::DeriveDispatch(
        "commitment-root backfill batch crossed the settled canonical boundary".to_owned(),
    ))
}

async fn write_root_search_batch(
    derive_store: DeriveStore,
    contexts: Vec<BlockCommitContext>,
    coverage: CommitmentRootBackfillCoverage,
) -> Result<(), IngestError> {
    tokio::task::spawn_blocking(move || {
        CommitmentRootSearchConsumer::new()
            .write_backfill_batch(&derive_store, &contexts, coverage)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })??;
    Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use zinder_core::{BlockHash, FinalNoteCommitmentRoot};
    use zinder_source::{
        NodeCapabilities, SourceBlock, SourceError, SourceTreeState, ZebraJsonRpcSource,
    };

    use super::*;

    #[derive(Default)]
    struct ConcurrencyTrackingSource {
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    #[async_trait]
    impl NodeSource for ConcurrencyTrackingSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            Err(SourceError::BlockUnavailable {
                height,
                reason: "unused by commitment-root backfill test".to_owned(),
            })
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Err(SourceError::NodeUnavailable {
                reason: "unused by commitment-root backfill test".to_owned(),
            })
        }

        async fn fetch_tree_state_for_block(
            &self,
            block_id: BlockId,
        ) -> Result<SourceTreeState, SourceError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            self.maximum_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(SourceTreeState::with_final_note_commitment_roots(
                roots(block_id),
                Vec::new(),
            ))
        }
    }

    fn block_id(height: u32) -> BlockId {
        BlockId::new(
            BlockHeight::new(height),
            BlockHash::from_bytes([height.to_le_bytes()[0]; 32]),
        )
    }

    fn roots(block_id: BlockId) -> BlockFinalNoteCommitmentRoots {
        BlockFinalNoteCommitmentRoots::new(
            block_id.height,
            block_id.hash,
            Some(FinalNoteCommitmentRoot::from_bytes(
                [block_id.height.value().to_le_bytes()[0]; 32],
            )),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn missing_root_fetches_respect_concurrency_bound() -> Result<(), IngestError> {
        let source = Arc::new(ConcurrencyTrackingSource::default());
        let block_ids = (1..=12).map(block_id).collect();
        let fetched = fetch_missing_roots(
            source.clone(),
            Duration::from_secs(1),
            NonZeroU32::new(3).ok_or_else(|| {
                IngestError::DeriveDispatch("test concurrency must be nonzero".to_owned())
            })?,
            block_ids,
        )
        .await?;

        assert_eq!(fetched.len(), 12);
        assert!(source.maximum_active.load(Ordering::SeqCst) <= 3);
        Ok(())
    }

    #[test]
    fn backfill_resumes_after_contiguous_coverage() -> Result<(), IngestError> {
        let activation = BlockHeight::new(100);
        assert_eq!(next_backfill_height(None, activation)?, activation);
        assert_eq!(
            next_backfill_height(
                Some(CommitmentRootBackfillCoverage::new(
                    activation,
                    BlockHeight::new(149),
                )),
                activation,
            )?,
            BlockHeight::new(150)
        );
        Ok(())
    }

    #[test]
    fn canonical_root_rows_are_reused_before_fetch() {
        let present = block_id(100);
        let missing = block_id(101);
        let blocks = [
            CanonicalRootBlock {
                block_id: present,
                roots: Some(roots(present)),
            },
            CanonicalRootBlock {
                block_id: missing,
                roots: None,
            },
        ];

        assert_eq!(missing_root_block_ids(&blocks), vec![missing]);
    }

    #[test]
    fn backfill_rejects_heights_above_settled_boundary() {
        assert!(validate_settled_boundary(BlockHeight::new(199), BlockHeight::new(200)).is_err());
        assert!(validate_settled_boundary(BlockHeight::new(200), BlockHeight::new(200)).is_ok());
    }

    #[tokio::test]
    async fn cancellation_interrupts_backfill_wait() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(sleep_or_cancel(Duration::from_mins(1), &cancel).await);
    }
}
