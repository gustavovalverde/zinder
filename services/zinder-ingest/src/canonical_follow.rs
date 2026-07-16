//! Continuous append-only following for the version-1 canonical store.

use std::{future::Future, num::NonZeroU32, sync::Arc, time::Duration};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHeight, BlockId, ChainTipMetadata, CommitmentTreeAccumulator,
    CommitmentTreeAccumulatorError, CommitmentTreeCheckpoint, NetworkUpgradeActivations,
    ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange, UnixTimestampMillis,
};
use zinder_runtime::{IngestPhase, Readiness, ReadinessState};
use zinder_source::{NodeSource, SourceBlock, SourceError};
use zinder_store::{
    CanonicalBuildBlock, CanonicalBuildSubtreeRoot, CanonicalLiveAppend, CanonicalStoreError,
    RocksDbCanonicalStore,
};

use crate::{
    CanonicalBlockConstructionError, CanonicalConstructionError, CommitmentTreeSizes, IngestError,
    RawBlobPolicy,
    canonical_construction::{canonical_build_block, compact_block_commitments},
    position_canonical_block, prepare_canonical_block,
    source_recovery::{
        SourceRecoveryDecision, decide_recovery, default_recovery_backoff, detail_for_new_outage,
        detail_for_ongoing_outage,
    },
};

/// Polling and bounded-source settings for version-1 canonical following.
#[derive(Clone, Copy, Debug)]
pub struct CanonicalFollowConfig {
    /// Maximum wall time for one source request.
    pub request_timeout: Duration,
    /// Delay before re-observing an unchanged tip.
    pub poll_interval: Duration,
    /// Maximum ready lag from the latest atomic source observation.
    pub lag_threshold_blocks: u64,
    /// Optional deterministic stop height for certification runs.
    pub target_height: Option<BlockHeight>,
}

/// Source and operational state bound to one canonical follower lane.
pub struct CanonicalFollower<'a, Source> {
    source: &'a Source,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    config: CanonicalFollowConfig,
    readiness: &'a Readiness,
    cancel: &'a CancellationToken,
}

impl<'a, Source> CanonicalFollower<'a, Source>
where
    Source: NodeSource,
{
    /// Binds one source and admitted activation table to a follower lane.
    #[must_use]
    pub fn new(
        source: &'a Source,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
        config: CanonicalFollowConfig,
        readiness: &'a Readiness,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            source,
            network_upgrade_activations,
            config,
            readiness,
            cancel,
        }
    }

    async fn recover_source_failure(
        &self,
        source_error: SourceError,
        source_outage: &mut SourceOutage,
        visible_height: BlockHeight,
    ) -> Result<bool, CanonicalFollowError> {
        let ingest_error = IngestError::from(source_error);
        let SourceRecoveryDecision::Recover {
            failure_class,
            last_reason,
            backoff,
        } = decide_recovery(&ingest_error, default_recovery_backoff())
        else {
            return Err(CanonicalFollowError::SourceRecoveryRejected);
        };
        let detail = source_outage.as_ref().map_or_else(
            || detail_for_new_outage(failure_class, last_reason.clone()),
            |(started_at, previous)| {
                detail_for_ongoing_outage(
                    previous,
                    failure_class,
                    last_reason.clone(),
                    u32::try_from(started_at.elapsed().as_secs()).unwrap_or(u32::MAX),
                )
            },
        );
        source_outage.get_or_insert_with(|| (std::time::Instant::now(), detail.clone()));
        if let Some((_, current_detail)) = source_outage.as_mut() {
            *current_detail = detail.clone();
        }
        self.readiness
            .set(ReadinessState::node_unavailable_with_detail(
                detail,
                Some(visible_height.value()),
            ));
        metrics::counter!("zinder_ingest_canonical_source_recoveries_total").increment(1);
        Ok(tokio::select! {
            () = self.cancel.cancelled() => true,
            () = tokio::time::sleep(backoff) => false,
        })
    }

    fn commit_append(
        &self,
        store: RocksDbCanonicalStore,
        append: CanonicalLiveAppend,
        observed_tip: BlockId,
    ) -> Result<RocksDbCanonicalStore, CanonicalFollowError> {
        let commit_started = std::time::Instant::now();
        let (store, fence) =
            store.commit_live_append(append, self.network_upgrade_activations.as_ref())?;
        metrics::counter!("zinder_ingest_canonical_live_appends_total").increment(1);
        metrics::histogram!("zinder_ingest_canonical_live_commit_seconds")
            .record(commit_started.elapsed().as_secs_f64());
        record_fence_metrics(&store, observed_tip);
        set_follow_readiness(
            self.readiness,
            &store,
            observed_tip,
            self.config.lag_threshold_blocks,
        );
        tracing::info!(
            target: "zinder::ingest",
            event = "canonical_live_append_committed",
            chain_epoch = fence.chain_epoch_id().value(),
            chain_event_sequence = fence.chain_event_sequence(),
            visible_tip_height = fence.visible_tip().height.value(),
            visible_tip_hash = ?fence.visible_tip().hash,
            sequence_digest = ?fence.sequence_digest().as_bytes(),
            source_tip_height = observed_tip.height.value(),
            historical_prevout_reads = 0_u64,
            cross_block_wallet_reads = 0_u64,
            "committed one authenticated version-1 canonical append"
        );
        Ok(store)
    }
}

type SourceOutage = Option<(std::time::Instant, zinder_runtime::NodeUnavailableDetail)>;

/// Failure while following the clean version-1 canonical store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalFollowError {
    /// The upstream source request failed before a canonical write began.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The shared recovery classifier rejected a source-shaped failure.
    #[error("source-shaped canonical follower failure was classified as fatal")]
    SourceRecoveryRejected,
    /// Block-local canonical preparation rejected the source payload.
    #[error(transparent)]
    BlockConstruction(#[from] CanonicalBlockConstructionError),
    /// Shared canonical preparation rejected the compact block artifacts.
    #[error(transparent)]
    CanonicalPreparation(#[from] CanonicalConstructionError),
    /// Commitment-tree preparation rejected the next source block.
    #[error("canonical live commitment-tree state failed at height {height:?}")]
    CommitmentTreeState {
        /// Height being applied.
        height: BlockHeight,
        /// Exact accumulator failure.
        #[source]
        source: CommitmentTreeAccumulatorError,
    },
    /// The source checkpoint did not authenticate the locally derived next frontier.
    #[error("source checkpoint differs from the locally derived live checkpoint at {height:?}")]
    SourceCheckpointMismatch {
        /// Height whose tree state diverged.
        height: BlockHeight,
    },
    /// Size-only compact metadata diverged from the typed frontier accumulator.
    #[error(
        "live block {height:?} commitment-tree positions diverged: positioned={positioned:?}, accumulated={accumulated:?}"
    )]
    CommitmentTreePositionMismatch {
        /// Block whose independent position calculations diverged.
        height: BlockHeight,
        /// Position stamped into the compact block.
        positioned: ChainTipMetadata,
        /// Position derived from typed frontiers.
        accumulated: ChainTipMetadata,
    },
    /// A completed-subtree position regressed between adjacent blocks.
    #[error("{protocol:?} completed-subtree count regressed from {previous_count} to {next_count}")]
    SubtreePositionRegression {
        /// Shielded pool whose completed-subtree count regressed.
        protocol: ShieldedProtocol,
        /// Completed subtrees at the authenticated predecessor.
        previous_count: u32,
        /// Completed subtrees after the next block.
        next_count: u32,
    },
    /// The source no longer extends the authenticated local fence.
    #[error(
        "append-only following requires reorg handling: local tip {local_tip:?}, source tip {source_tip:?}"
    )]
    ReorgRequired {
        /// Authenticated local visible tip.
        local_tip: BlockId,
        /// Latest atomic source observation.
        source_tip: BlockId,
    },
    /// A blocking preparation task stopped without returning its result.
    #[error("canonical live block preparation task stopped: {reason}")]
    PreparationTaskStopped {
        /// Tokio task failure.
        reason: String,
    },
    /// The consuming atomic store transition failed or could not be verified.
    #[error(transparent)]
    Store(#[from] CanonicalStoreError),
}

/// Follows atomic Zebra tip observations through version-1 canonical appends.
///
/// Source failures happen before the consuming store commit and are retried
/// with the admitted writer handle. Any store error terminates the writer lane;
/// the caller must reopen through normal READY admission to determine the
/// durable outcome.
pub async fn follow_canonical_tip<Source>(
    mut store: RocksDbCanonicalStore,
    follower: CanonicalFollower<'_, Source>,
) -> Result<RocksDbCanonicalStore, CanonicalFollowError>
where
    Source: NodeSource,
{
    metrics::counter!("zinder_ingest_canonical_historical_prevout_reads_total").absolute(0);
    metrics::counter!("zinder_ingest_canonical_cross_block_wallet_reads_total").absolute(0);
    follower.readiness.set_phase(IngestPhase::FollowingTip);
    let mut source_outage = None;

    loop {
        if follower.cancel.is_cancelled() {
            return Ok(store);
        }
        if follower
            .config
            .target_height
            .is_some_and(|target| store.event_fence().visible_tip().height >= target)
        {
            let visible_height = store.event_fence().visible_tip().height.value();
            follower.readiness.set(
                ReadinessState::ready_with_target(Some(visible_height), Some(visible_height))
                    .with_phase(IngestPhase::FollowingTip),
            );
            record_fence_metrics(&store, store.event_fence().visible_tip());
            return Ok(store);
        }

        let prepared = match prepare_follow_iteration(
            &store,
            follower.source,
            Arc::clone(&follower.network_upgrade_activations),
            follower.config,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(CanonicalFollowError::Source(source_error)) => {
                if follower
                    .recover_source_failure(
                        source_error,
                        &mut source_outage,
                        store.event_fence().visible_tip().height,
                    )
                    .await?
                {
                    return Ok(store);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        source_outage = None;

        match prepared {
            FollowIteration::AtTip { observed_tip } => {
                record_fence_metrics(&store, observed_tip);
                set_follow_readiness(
                    follower.readiness,
                    &store,
                    observed_tip,
                    follower.config.lag_threshold_blocks,
                );
                tokio::select! {
                    () = follower.cancel.cancelled() => return Ok(store),
                    () = tokio::time::sleep(follower.config.poll_interval) => {}
                }
            }
            FollowIteration::Append {
                append,
                observed_tip,
            } => {
                store = follower.commit_append(store, *append, observed_tip)?;
            }
        }
    }
}

enum FollowIteration {
    AtTip {
        observed_tip: BlockId,
    },
    Append {
        append: Box<CanonicalLiveAppend>,
        observed_tip: BlockId,
    },
}

async fn prepare_follow_iteration<Source>(
    store: &RocksDbCanonicalStore,
    source: &Source,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    config: CanonicalFollowConfig,
) -> Result<FollowIteration, CanonicalFollowError>
where
    Source: NodeSource,
{
    let anchor = store.append_anchor()?;
    let observed_tip = source_request(config.request_timeout, source.tip_id()).await?;
    let local_tip = anchor.event_fence().visible_tip();
    if observed_tip.height < local_tip.height
        || (observed_tip.height == local_tip.height && observed_tip.hash != local_tip.hash)
    {
        return Err(CanonicalFollowError::ReorgRequired {
            local_tip,
            source_tip: observed_tip,
        });
    }
    let effective_target = config.target_height.map_or(observed_tip.height, |target| {
        target.min(observed_tip.height)
    });
    if effective_target <= local_tip.height {
        return Ok(FollowIteration::AtTip { observed_tip });
    }
    let next_height = local_tip
        .height
        .next()
        .ok_or(CanonicalFollowError::ReorgRequired {
            local_tip,
            source_tip: observed_tip,
        })?;
    let source_block =
        source_request(config.request_timeout, source.fetch_block_at(next_height)).await?;
    if source_block.parent_hash != local_tip.hash {
        return Err(CanonicalFollowError::ReorgRequired {
            local_tip,
            source_tip: observed_tip,
        });
    }
    let source_checkpoint = source_request(
        config.request_timeout,
        source.fetch_chain_checkpoint(next_height, network_upgrade_activations.as_ref()),
    )
    .await?;
    let predecessor_checkpoint = anchor.tip_checkpoint().clone();
    let preparation_started = std::time::Instant::now();
    let activations_for_prepare = Arc::clone(&network_upgrade_activations);
    let (block, next_tip_metadata) = tokio::task::spawn_blocking(move || {
        prepare_live_block(
            &source_block,
            &predecessor_checkpoint,
            &source_checkpoint,
            activations_for_prepare.as_ref(),
        )
    })
    .await
    .map_err(|source| CanonicalFollowError::PreparationTaskStopped {
        reason: source.to_string(),
    })??;
    metrics::histogram!("zinder_ingest_canonical_live_prepare_seconds")
        .record(preparation_started.elapsed().as_secs_f64());
    let subtree_roots = fetch_live_subtree_roots(
        source,
        config.request_timeout,
        anchor.tip_checkpoint().tip_metadata(),
        next_tip_metadata,
    )
    .await?;
    Ok(FollowIteration::Append {
        append: Box::new(CanonicalLiveAppend::new(
            anchor.event_fence(),
            block,
            subtree_roots,
            anchor.settled_tip(),
            UnixTimestampMillis::now(),
        )),
        observed_tip,
    })
}

fn prepare_live_block(
    source_block: &SourceBlock,
    predecessor_checkpoint: &CommitmentTreeCheckpoint,
    source_checkpoint: &CommitmentTreeCheckpoint,
    network_upgrade_activations: &NetworkUpgradeActivations,
) -> Result<(CanonicalBuildBlock, ChainTipMetadata), CanonicalFollowError> {
    let height = source_block.height;
    let prepared = prepare_canonical_block(
        source_block,
        network_upgrade_activations,
        RawBlobPolicy::Transactions,
    )?;
    let commitments = compact_block_commitments(&prepared)?;
    let mut accumulator = CommitmentTreeAccumulator::from_validated_frontiers(
        predecessor_checkpoint.block_id.height,
        &predecessor_checkpoint.frontiers,
        network_upgrade_activations,
    )
    .map_err(|source| CanonicalFollowError::CommitmentTreeState { height, source })?;
    accumulator
        .append_block_commitments(
            height,
            &commitments.sapling,
            &commitments.orchard,
            &commitments.ironwood,
        )
        .map_err(|source| CanonicalFollowError::CommitmentTreeState { height, source })?;
    let mut running_tree_sizes =
        CommitmentTreeSizes::from_tip_metadata(predecessor_checkpoint.tip_metadata());
    let positioned = position_canonical_block(prepared, &mut running_tree_sizes)?;
    let accumulated_tip_metadata = accumulator.tip_metadata();
    if positioned.tip_metadata != accumulated_tip_metadata {
        return Err(CanonicalFollowError::CommitmentTreePositionMismatch {
            height,
            positioned: positioned.tip_metadata,
            accumulated: accumulated_tip_metadata,
        });
    }
    let derived_checkpoint = CommitmentTreeCheckpoint::new(
        BlockId::new(height, source_block.hash),
        source_block.block_time_seconds,
        accumulator
            .validated_frontiers()
            .map_err(|source| CanonicalFollowError::CommitmentTreeState { height, source })?,
    );
    if &derived_checkpoint != source_checkpoint {
        return Err(CanonicalFollowError::SourceCheckpointMismatch { height });
    }
    let tip_metadata = positioned.tip_metadata;
    Ok((
        canonical_build_block(positioned, Some(derived_checkpoint), None),
        tip_metadata,
    ))
}

async fn fetch_live_subtree_roots<Source>(
    source: &Source,
    request_timeout: Duration,
    previous_tip_metadata: ChainTipMetadata,
    next_tip_metadata: ChainTipMetadata,
) -> Result<Vec<CanonicalBuildSubtreeRoot>, CanonicalFollowError>
where
    Source: NodeSource,
{
    let mut roots = Vec::new();
    for range in live_subtree_root_ranges(previous_tip_metadata, next_tip_metadata)? {
        let response =
            source_request(request_timeout, source.fetch_subtree_root_range(range)).await?;
        roots.extend(
            response
                .subtree_roots
                .into_iter()
                .map(|root| CanonicalBuildSubtreeRoot {
                    protocol: response.protocol,
                    subtree_index: root.subtree_index,
                    root_hash: root.root_hash,
                    completing_block_height: root.completing_block_height,
                }),
        );
    }
    Ok(roots)
}

fn live_subtree_root_ranges(
    previous: ChainTipMetadata,
    next: ChainTipMetadata,
) -> Result<Vec<SubtreeRootRange>, CanonicalFollowError> {
    let mut ranges = Vec::with_capacity(3);
    for protocol in [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ] {
        let start_index = previous.completed_subtree_count(protocol);
        let completed_at_next = next.completed_subtree_count(protocol);
        let root_count = completed_at_next.checked_sub(start_index).ok_or(
            CanonicalFollowError::SubtreePositionRegression {
                protocol,
                previous_count: start_index,
                next_count: completed_at_next,
            },
        )?;
        if let Some(max_entries) = NonZeroU32::new(root_count) {
            ranges.push(SubtreeRootRange::new(
                protocol,
                SubtreeRootIndex::new(start_index),
                max_entries,
            ));
        }
    }
    Ok(ranges)
}

async fn source_request<T>(
    timeout: Duration,
    request: impl Future<Output = Result<T, SourceError>>,
) -> Result<T, CanonicalFollowError> {
    tokio::time::timeout(timeout, request)
        .await
        .map_err(|_| SourceError::NodeUnavailable {
            reason: format!("canonical follow source request exceeded {timeout:?}"),
        })?
        .map_err(CanonicalFollowError::from)
}

fn set_follow_readiness(
    readiness: &Readiness,
    store: &RocksDbCanonicalStore,
    observed_tip: BlockId,
    lag_threshold_blocks: u64,
) {
    let current_height = store.event_fence().visible_tip().height;
    let lag = u64::from(
        observed_tip
            .height
            .value()
            .saturating_sub(current_height.value()),
    );
    let state = if lag <= lag_threshold_blocks {
        ReadinessState::ready_with_target(
            Some(current_height.value()),
            Some(observed_tip.height.value()),
        )
    } else {
        ReadinessState::syncing(
            Some(lag),
            Some(current_height.value()),
            Some(observed_tip.height.value()),
        )
    };
    metrics::gauge!("zinder_ingest_canonical_lag_blocks").set(f64::from(
        observed_tip
            .height
            .value()
            .saturating_sub(store.event_fence().visible_tip().height.value()),
    ));
    readiness.set(state.with_phase(IngestPhase::FollowingTip));
}

fn record_fence_metrics(store: &RocksDbCanonicalStore, observed_tip: BlockId) {
    let fence = store.event_fence();
    metrics::gauge!("zinder_ingest_canonical_chain_epoch").set(f64::from(
        u32::try_from(fence.chain_epoch_id().value()).unwrap_or(u32::MAX),
    ));
    metrics::gauge!("zinder_ingest_canonical_chain_event_sequence").set(f64::from(
        u32::try_from(fence.chain_event_sequence()).unwrap_or(u32::MAX),
    ));
    metrics::gauge!("zinder_ingest_canonical_tip_height")
        .set(f64::from(fence.visible_tip().height.value()));
    metrics::gauge!("zinder_ingest_canonical_lag_blocks").set(f64::from(
        observed_tip
            .height
            .value()
            .saturating_sub(fence.visible_tip().height.value()),
    ));
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use zinder_core::{
        ChainTipMetadata, SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange,
    };

    use super::live_subtree_root_ranges;

    #[test]
    fn live_ranges_fetch_only_newly_completed_subtrees() -> Result<(), Box<dyn std::error::Error>> {
        let previous = ChainTipMetadata::new(SUBTREE_LEAF_COUNT * 2 + 9, SUBTREE_LEAF_COUNT, 0);
        let next = ChainTipMetadata::new(SUBTREE_LEAF_COUNT * 4, SUBTREE_LEAF_COUNT * 2 + 1, 12);

        assert_eq!(
            live_subtree_root_ranges(previous, next)?,
            vec![
                SubtreeRootRange::new(
                    ShieldedProtocol::Sapling,
                    SubtreeRootIndex::new(2),
                    NonZeroU32::new(2).ok_or("Sapling range must be nonzero")?,
                ),
                SubtreeRootRange::new(
                    ShieldedProtocol::Orchard,
                    SubtreeRootIndex::new(1),
                    NonZeroU32::new(1).ok_or("Orchard range must be nonzero")?,
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn live_ranges_reject_completed_subtree_regression() {
        let outcome = live_subtree_root_ranges(
            ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0, 0),
            ChainTipMetadata::empty(),
        );

        assert!(outcome.is_err());
    }
}
