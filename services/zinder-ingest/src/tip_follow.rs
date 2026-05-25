use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::stream::StreamExt;
use prost::Message;
use std::time::Instant;
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, ChainEpoch, ChainEpochId, ChainTipMetadata,
    Network, NetworkUpgradeActivations, TreeStateArtifact,
};
use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;

use zinder_runtime::{NodeUnavailableDetail, Readiness, ReadinessCause, ReadinessState};
use zinder_source::{
    ChainTipNotificationSource, NodeCapability, NodeSource, NodeTarget, SourceBlock, SourceError,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainEpochReader, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::artifact_builder::{
    CommitmentTreeSizes, DerivedBlockArtifacts, RawBlobPolicy, derive_block_with_raw_blob_policy,
    finalize_derived_block,
};
use crate::chain_ingest::{
    CanonicalBatch, IngestError, IngestRetryState, IngestSubtreeRootIndexes, commit_ingest_batch,
    current_unix_millis, fetch_block_with_retry, fetch_tree_state_for_block_with_retry,
    next_chain_epoch_id_after, next_chain_epoch_id_from, populate_subtree_root_artifacts,
    record_commit_outcome, record_ingest_fact_build_outcome, select_best_chain,
};
use crate::mempool::MempoolReadyGate;
use crate::phase::current_chain_height;
use crate::source_recovery::{
    SourceRecoveryDecision, decide_recovery, default_recovery_backoff, detail_for_new_outage,
    detail_for_ongoing_outage,
};

/// Default lag threshold (in blocks) below which tip-follow reports `Ready`.
pub const DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS: u64 = 1;
const CHAIN_TIP_WAKEUP_CHANNEL_CAPACITY: usize = 1;
#[cfg(not(test))]
const CHAIN_TIP_NOTIFICATION_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
#[cfg(test)]
const CHAIN_TIP_NOTIFICATION_RECONNECT_BACKOFF: Duration = Duration::from_millis(1);

/// Configuration for polling the upstream node tip and committing live chain changes.
///
/// `reorg_window_blocks` and `poll_interval` must be greater than zero;
/// `zinder-runtime::ConfigError::Invalid` is the canonical rejection emitted by
/// the binary's config layer before this type is constructed.
#[derive(Clone, Debug)]
pub struct TipFollowConfig {
    /// Resolved upstream node endpoint (network, JSON-RPC URL, auth, timeout,
    /// response-size cap). See [`NodeTarget`].
    pub node: NodeTarget,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// Bounded `RocksDB` resource budget applied when opening the canonical store.
    pub canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget,
    /// Number of near-tip blocks that may be replaced by a reorg. Must be greater than zero.
    pub reorg_window_blocks: u32,
    /// Delay between tip polls when no cancellation is requested. Must be greater than zero.
    pub poll_interval: Duration,
    /// Lag threshold (in blocks) below which tip-follow reports `Ready`.
    ///
    /// When `(node_tip - store_tip) <= lag_threshold_blocks` the readiness
    /// state flips from `Syncing` to `Ready`. The default is 1, meaning the
    /// service is ready as soon as the store is at most one block behind the
    /// observed node tip.
    pub lag_threshold_blocks: u64,
    /// Optional raw-byte blob write policy.
    pub raw_blob_policy: RawBlobPolicy,
    /// Node-discovered consensus upgrade activations used for transaction facts.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

/// Follows the upstream node tip until `cancel` is triggered, updating
/// `readiness` after every iteration.
pub async fn tip_follow<Source>(
    config: &TipFollowConfig,
    source: &Source,
    readiness: &Readiness,
    cancel: CancellationToken,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    let store = open_tip_follow_store(config)?;
    tip_follow_with_primary_store(config, source, store, readiness, None, None, cancel).await
}

/// Opens the primary store with the tip-follow reorg-window policy.
///
/// Binaries use this when they need to share the primary store handle with a
/// process-local adapter, such as the private ingest-control endpoint.
pub fn open_tip_follow_store(config: &TipFollowConfig) -> Result<PrimaryChainStore, IngestError> {
    let mut store_options = ChainStoreOptions::for_network(config.node.network);
    store_options.reorg_window_blocks = config.reorg_window_blocks;
    store_options.rocksdb_resource_budget = config.canonical_rocksdb_budget;
    PrimaryChainStore::open(&config.storage_path, store_options).map_err(IngestError::from)
}

/// Follows the upstream node tip with a caller-owned primary store handle.
///
/// This is the same loop as [`tip_follow`], but it avoids opening the primary
/// twice when the runtime needs the store for colocated control-plane RPCs.
///
/// `mempool_ready_gate` is consulted when the lag-based computation would
/// otherwise flip readiness to `Ready`. While the gate is unprimed, the
/// readiness state stays in `Syncing` so consumers do not observe a
/// "ready" writer that has not yet rebuilt its in-process mempool index.
/// Pass `None` for callers that do not run the mempool orchestrator
/// (tests, bulk catchup).
#[allow(
    clippy::too_many_arguments,
    reason = "tip-follow's caller-owned dependencies are deliberately exposed as positional parameters; bundling them into an orchestration struct adds one indirection without changing the binding count callers must make."
)]
pub async fn tip_follow_with_primary_store<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: PrimaryChainStore,
    readiness: &Readiness,
    mempool_ready_gate: Option<&MempoolReadyGate>,
    chain_tip_source: Option<Arc<dyn ChainTipNotificationSource>>,
    cancel: CancellationToken,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    let raw_blob_policy = config.raw_blob_policy;
    let network_upgrade_activations = Arc::clone(&config.network_upgrade_activations);
    run_tip_follow_loop(
        config,
        source,
        store,
        readiness,
        mempool_ready_gate,
        chain_tip_source,
        cancel,
        move |source_block| {
            derive_block_with_raw_blob_policy(
                source_block,
                &network_upgrade_activations,
                raw_blob_policy,
            )
        },
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "tip-follow loop tests inject the derive function while production callers keep the public dependency list explicit."
)]
#[allow(
    clippy::too_many_lines,
    reason = "the tip-follow loop is one auditable sequence of select+recover+commit+finalize+readiness; splitting it would scatter the contract across helpers without simplifying any single decision."
)]
async fn run_tip_follow_loop<Source, Derive>(
    config: &TipFollowConfig,
    source: &Source,
    store: PrimaryChainStore,
    readiness: &Readiness,
    mempool_ready_gate: Option<&MempoolReadyGate>,
    chain_tip_source: Option<Arc<dyn ChainTipNotificationSource>>,
    cancel: CancellationToken,
    derive_fn: Derive,
) -> Result<(), IngestError>
where
    Source: NodeSource,
    Derive: Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError>
        + Clone
        + Send
        + Sync,
{
    let mut retry_state = IngestRetryState::default();
    let chain_tip_task_cancel = cancel.child_token();
    let (chain_tip_wakeup_handle, mut chain_tip_wakeup_receiver) =
        start_chain_tip_notification_wakeup(chain_tip_source, chain_tip_task_cancel.clone());
    let mut outage_tracker: Option<SourceOutageTracker> = None;
    let recovery_backoff = default_recovery_backoff();

    let tip_follow_outcome = loop {
        tokio::select! {
            () = cancel.cancelled() => break Ok(()),
            () = wait_for_chain_tip_wakeup(&mut chain_tip_wakeup_receiver) => {}
            () = tokio::time::sleep(config.poll_interval) => {}
        }

        let mut iteration = match tip_follow_once(
            config,
            source,
            &store,
            &mut retry_state,
            derive_fn.clone(),
        )
        .await
        {
            Ok(iteration) => iteration,
            Err(error) => match decide_recovery(&error, recovery_backoff) {
                SourceRecoveryDecision::Recover {
                    failure_class,
                    last_reason,
                    backoff,
                } => {
                    retry_state = IngestRetryState::default();
                    let detail = update_outage_tracker(
                        &mut outage_tracker,
                        failure_class,
                        last_reason.clone(),
                    );
                    if detail.consecutive_failures == 1 {
                        tracing::warn!(
                            target: "zinder::ingest",
                            event = "tip_follow_source_unavailable",
                            failure_class = failure_class.label(),
                            error = %error,
                            "tip-follow source is unavailable; keeping the writer alive and retrying"
                        );
                    }
                    set_tip_follow_node_unavailable(readiness, &store, detail);
                    tokio::select! {
                        () = cancel.cancelled() => break Ok(()),
                        () = tokio::time::sleep(backoff) => {}
                    }
                    continue;
                }
                SourceRecoveryDecision::Exit => break Err(error),
            },
        };

        if let Some(commit_outcome) = iteration.commit_outcome.as_ref() {
            match finalize_tip_if_ready(config, &store, commit_outcome.chain_epoch) {
                Ok(Some(finalized_outcome)) => {
                    iteration.commit_outcome = Some(finalized_outcome);
                }
                Ok(None) => {}
                Err(error) => break Err(error),
            }
        }

        if outage_tracker.take().is_some() {
            tracing::info!(
                target: "zinder::ingest",
                event = "tip_follow_source_recovered",
                "tip-follow source recovered"
            );
        }

        let lag_state = match compute_tip_follow_readiness_state(
            &store,
            iteration.observed_tip_id.height,
            config,
        ) {
            Ok(lag_state) => lag_state,
            Err(error) => break Err(error),
        };
        set_tip_follow_readiness(readiness, lag_state, mempool_ready_gate);
    };

    chain_tip_task_cancel.cancel();
    if let Some(handle) = chain_tip_wakeup_handle {
        handle.abort();
    }

    tip_follow_outcome
}

/// Per-outage running state for the tip-follow recovery loop.
///
/// Records the latest readiness detail and the instant the current outage
/// began so each subsequent failure can advance `consecutive_failures` and
/// `outage_seconds` without surface-level state. The tracker is reset to
/// `None` after the next successful iteration.
#[derive(Debug)]
struct SourceOutageTracker {
    detail: NodeUnavailableDetail,
    started_at: Instant,
}

fn update_outage_tracker(
    tracker: &mut Option<SourceOutageTracker>,
    failure_class: zinder_source::SourceFailureClass,
    last_reason: std::borrow::Cow<'static, str>,
) -> NodeUnavailableDetail {
    if let Some(existing) = tracker {
        let outage_seconds =
            u32::try_from(existing.started_at.elapsed().as_secs()).unwrap_or(u32::MAX);
        let detail =
            detail_for_ongoing_outage(&existing.detail, failure_class, last_reason, outage_seconds);
        existing.detail = detail.clone();
        detail
    } else {
        let detail = detail_for_new_outage(failure_class, last_reason);
        *tracker = Some(SourceOutageTracker {
            detail: detail.clone(),
            started_at: Instant::now(),
        });
        detail
    }
}

fn start_chain_tip_notification_wakeup(
    source: Option<Arc<dyn ChainTipNotificationSource>>,
    cancel: CancellationToken,
) -> (Option<JoinHandle<()>>, Option<mpsc::Receiver<()>>) {
    let (sender, receiver) = mpsc::channel(CHAIN_TIP_WAKEUP_CHANNEL_CAPACITY);
    source.map_or_else(
        || (None, None),
        |source| {
            (
                Some(spawn_chain_tip_notification_wakeup(source, sender, cancel)),
                Some(receiver),
            )
        },
    )
}

fn set_tip_follow_readiness(
    readiness: &Readiness,
    lag_state: ReadinessState,
    mempool_ready_gate: Option<&MempoolReadyGate>,
) {
    let gated_state = if matches!(lag_state.cause, ReadinessCause::Ready)
        && mempool_ready_gate.is_some_and(|gate| !gate.is_primed())
    {
        ReadinessState::syncing(None, lag_state.current_height, lag_state.target_height)
    } else {
        lag_state
    };

    let current_cause = readiness.report().cause;
    // Don't override retention-driven warning states with the lag-derived
    // Ready: the retention task owns the cursor-at-risk lifecycle and the
    // orchestrator owns mempool source/hydration causes. Leaving those in
    // place keeps each subsystem the source of truth for its own causes.
    if matches!(
        current_cause,
        ReadinessCause::CursorAtRisk { .. }
            | ReadinessCause::MempoolCursorAtRisk { .. }
            | ReadinessCause::MempoolSourceUnavailable
            | ReadinessCause::MempoolHydrationLagging { .. }
    ) && matches!(gated_state.cause, ReadinessCause::Ready)
    {
        return;
    }

    readiness.set(gated_state);
}

fn set_tip_follow_node_unavailable(
    readiness: &Readiness,
    store: &PrimaryChainStore,
    detail: NodeUnavailableDetail,
) {
    readiness.set(ReadinessState::node_unavailable_with_detail(
        detail,
        current_chain_height(store),
    ));
}

fn compute_tip_follow_readiness_state(
    store: &PrimaryChainStore,
    node_tip_height: BlockHeight,
    config: &TipFollowConfig,
) -> Result<ReadinessState, IngestError> {
    let store_tip_height = store
        .current_chain_epoch()?
        .map(|chain_epoch| chain_epoch.tip_height);

    let node_tip_value = u64::from(node_tip_height.value());
    let store_tip_value = store_tip_height.map_or(0_u64, |height| u64::from(height.value()));
    let current_height = store_tip_height.map(zinder_core::BlockHeight::value);
    let target_height = Some(node_tip_height.value());

    if node_tip_value < store_tip_value {
        let rewind_depth_blocks = store_tip_value - node_tip_value;
        return Ok(ReadinessState::syncing(
            Some(rewind_depth_blocks),
            current_height,
            target_height,
        ));
    }

    let lag_blocks = node_tip_value - store_tip_value;
    if lag_blocks <= config.lag_threshold_blocks {
        Ok(ReadinessState::ready(current_height))
    } else {
        Ok(ReadinessState::syncing(
            Some(lag_blocks),
            current_height,
            target_height,
        ))
    }
}

async fn wait_for_chain_tip_wakeup(receiver: &mut Option<mpsc::Receiver<()>>) {
    let Some(active_receiver) = receiver.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if active_receiver.recv().await.is_none() {
        *receiver = None;
    }
}

fn spawn_chain_tip_notification_wakeup(
    source: Arc<dyn ChainTipNotificationSource>,
    sender: mpsc::Sender<()>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_chain_tip_notification_wakeup(source, sender, cancel).await;
    })
}

async fn run_chain_tip_notification_wakeup(
    source: Arc<dyn ChainTipNotificationSource>,
    sender: mpsc::Sender<()>,
    cancel: CancellationToken,
) {
    loop {
        let mut stream = match source.subscribe().await {
            Ok(stream) => {
                tracing::info!(
                    target: "zinder::ingest",
                    event = "chain_tip_notification_source_selected",
                    backend = "zebra-indexer-grpc",
                    "subscribed to Zebra chain_tip_change stream; tip-follow wakeups are push-based"
                );
                stream
            }
            Err(error) => {
                tracing::warn!(
                    target: "zinder::ingest",
                    event = "chain_tip_notification_source_unavailable",
                    %error,
                    "Zebra chain_tip_change subscription failed; polling tip-follow remains active"
                );
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(CHAIN_TIP_NOTIFICATION_RECONNECT_BACKOFF) => {}
                }
                continue;
            }
        };

        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                notification = stream.next() => {
                    match notification {
                        Some(Ok(notification)) => {
                            tracing::debug!(
                                target: "zinder::ingest",
                                event = "chain_tip_notification_received",
                                height = notification.tip_id.height.value(),
                                "wakeup on Zebra chain_tip_change notification"
                            );
                            if !send_chain_tip_wakeup(&sender) {
                                return;
                            }
                        }
                        Some(Err(error)) => {
                            tracing::warn!(
                                target: "zinder::ingest",
                                event = "chain_tip_notification_stream_error",
                                %error,
                                "chain_tip_change stream emitted an error; re-subscribing while polling remains active"
                            );
                            break;
                        }
                        None => {
                            tracing::warn!(
                                target: "zinder::ingest",
                                event = "chain_tip_notification_stream_ended",
                                "chain_tip_change stream ended; re-subscribing while polling remains active"
                            );
                            break;
                        }
                    }
                }
            }
        }

        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(CHAIN_TIP_NOTIFICATION_RECONNECT_BACKOFF) => {}
        }
    }
}

fn send_chain_tip_wakeup(sender: &mpsc::Sender<()>) -> bool {
    match sender.try_send(()) {
        Ok(()) | Err(TrySendError::Full(())) => true,
        Err(TrySendError::Closed(())) => false,
    }
}

/// Result of one tip-follow iteration: the node tip identity observed in
/// this iteration, plus the commit outcome (when the iteration produced one).
pub(crate) struct TipFollowIteration {
    /// Node tip identity observed at the start of the iteration.
    pub(crate) observed_tip_id: BlockId,
    /// Commit outcome when the iteration produced one, otherwise `None`.
    pub(crate) commit_outcome: Option<ChainEpochCommitOutcome>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "tip-follow iteration owns config, source, two stores, readiness, optional mempool coordination, cancellation, and an injected derive function"
)]
async fn tip_follow_once<Source, Derive>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    retry_state: &mut IngestRetryState,
    derive_fn: Derive,
) -> Result<TipFollowIteration, IngestError>
where
    Source: NodeSource,
    Derive:
        Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError> + Send + Sync,
{
    let observed_tip_id = source.tip_id().await?;
    let current_chain_epoch = store.current_chain_epoch()?;
    let Some(plan) = tip_follow_plan(
        config,
        source,
        store,
        current_chain_epoch,
        observed_tip_id,
        retry_state,
    )
    .await?
    else {
        return Ok(TipFollowIteration {
            observed_tip_id,
            commit_outcome: None,
        });
    };

    let commit_outcome = commit_tip_follow_blocks(
        config,
        source,
        store,
        &derive_fn,
        plan,
        current_chain_epoch,
        retry_state,
    )
    .await?;

    Ok(TipFollowIteration {
        observed_tip_id,
        commit_outcome: Some(commit_outcome),
    })
}

struct TipFollowPlan {
    source_blocks: Vec<SourceBlock>,
    reorg_window_change: ReorgWindowChange,
    parent_tip_metadata: ChainTipMetadata,
}

#[allow(
    clippy::too_many_arguments,
    reason = "private tip-follow planning keeps injected source, store, config, and retry state explicit"
)]
async fn tip_follow_plan<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    current_chain_epoch: Option<ChainEpoch>,
    observed_tip_id: BlockId,
    retry_state: &mut IngestRetryState,
) -> Result<Option<TipFollowPlan>, IngestError>
where
    Source: NodeSource,
{
    let Some(current_chain_epoch) = current_chain_epoch else {
        if observed_tip_id.height.value() == 0 {
            return Ok(None);
        }

        let first_block = fetch_block_with_retry(
            config.node.request_timeout,
            source,
            BlockHeight::new(1),
            retry_state,
        )
        .await?;
        return Ok(Some(TipFollowPlan {
            source_blocks: vec![first_block],
            reorg_window_change: ReorgWindowChange::Extend {
                block_range: BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(1)),
            },
            parent_tip_metadata: ChainTipMetadata::empty(),
        }));
    };

    if observed_tip_id.height < current_chain_epoch.tip_height {
        // `invalidateblock`-style local reorg gates expose a transient state
        // where the node has rewound before the replacement block exists.
        // Zinder's event model records replacements, not rollback-only epochs,
        // so the correct action is to wait and report not-ready via readiness.
        return Ok(None);
    }

    if observed_tip_id.height == current_chain_epoch.tip_height {
        if observed_tip_id.hash == current_chain_epoch.tip_hash {
            return Ok(None);
        }

        let observed_tip = fetch_block_with_retry(
            config.node.request_timeout,
            source,
            observed_tip_id.height,
            retry_state,
        )
        .await?;

        return replacement_tip_follow_plan(
            config,
            source,
            store,
            current_chain_epoch,
            observed_tip,
            retry_state,
        )
        .await
        .map(Some);
    }

    let next_height = current_chain_epoch
        .tip_height
        .next()
        .unwrap_or(current_chain_epoch.tip_height);
    let next_block = fetch_block_with_retry(
        config.node.request_timeout,
        source,
        next_height,
        retry_state,
    )
    .await?;
    if next_block.parent_hash == current_chain_epoch.tip_hash {
        return Ok(Some(TipFollowPlan {
            source_blocks: vec![next_block],
            reorg_window_change: ReorgWindowChange::Extend {
                block_range: BlockHeightRange::inclusive(next_height, next_height),
            },
            parent_tip_metadata: current_chain_epoch.tip_metadata,
        }));
    }

    replacement_tip_follow_plan(
        config,
        source,
        store,
        current_chain_epoch,
        next_block,
        retry_state,
    )
    .await
    .map(Some)
}

#[allow(
    clippy::too_many_arguments,
    reason = "private replacement planning keeps injected source, store, config, and retry state explicit"
)]
async fn replacement_tip_follow_plan<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    current_chain_epoch: ChainEpoch,
    replacement_tip: SourceBlock,
    retry_state: &mut IngestRetryState,
) -> Result<TipFollowPlan, IngestError>
where
    Source: NodeSource,
{
    let (source_blocks, common_ancestor_height) = replacement_blocks_to_common_ancestor(
        config,
        source,
        store,
        current_chain_epoch,
        replacement_tip,
        retry_state,
    )
    .await?;
    let reorg_window_change = select_best_chain(
        current_chain_epoch,
        &source_blocks,
        config.reorg_window_blocks,
    )?;
    let parent_tip_metadata = tip_metadata_at(store, current_chain_epoch, common_ancestor_height)?;

    Ok(TipFollowPlan {
        source_blocks,
        reorg_window_change,
        parent_tip_metadata,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "private ancestor search keeps injected source, store, config, and retry state explicit"
)]
async fn replacement_blocks_to_common_ancestor<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    current_chain_epoch: ChainEpoch,
    replacement_tip: SourceBlock,
    retry_state: &mut IngestRetryState,
) -> Result<(Vec<SourceBlock>, BlockHeight), IngestError>
where
    Source: NodeSource,
{
    let replacement_tip_height = replacement_tip.height;
    let reader = store.chain_epoch_reader_at(current_chain_epoch.id)?;
    let mut child_parent_hash = replacement_tip.parent_hash;
    let mut candidate_blocks = vec![replacement_tip];
    let mut search_height = BlockHeight::new(replacement_tip_height.value().saturating_sub(1));

    loop {
        if search_height.value() == 0 {
            return Err(IngestError::TipFollowCommonAncestorMissing {
                replacement_tip_height,
            });
        }

        let old_hash = visible_block_hash(&reader, search_height)?;
        if child_parent_hash == old_hash {
            break;
        }

        let source_block = fetch_block_with_retry(
            config.node.request_timeout,
            source,
            search_height,
            retry_state,
        )
        .await?;
        if source_block.hash == old_hash {
            break;
        }

        child_parent_hash = source_block.parent_hash;
        candidate_blocks.push(source_block);
        search_height = BlockHeight::new(search_height.value().saturating_sub(1));
    }

    candidate_blocks.sort_by_key(|block| block.height);
    Ok((candidate_blocks, search_height))
}

fn visible_block_hash(
    reader: &ChainEpochReader<'_>,
    height: BlockHeight,
) -> Result<BlockHash, IngestError> {
    reader
        .block_header_at(height)?
        .map(|block| block.block_hash)
        .ok_or(IngestError::TipFollowCommonAncestorMissing {
            replacement_tip_height: height,
        })
}

fn tip_metadata_at(
    store: &PrimaryChainStore,
    current_chain_epoch: ChainEpoch,
    height: BlockHeight,
) -> Result<ChainTipMetadata, IngestError> {
    if height.value() == 0 {
        return Ok(ChainTipMetadata::empty());
    }
    if height == current_chain_epoch.tip_height {
        return Ok(current_chain_epoch.tip_metadata);
    }

    let reader = store.chain_epoch_reader_at(current_chain_epoch.id)?;
    let compact_block = reader
        .compact_block_at(height)?
        .ok_or(IngestError::TipFollowParentMetadataUnavailable { height })?;
    let compact_block = LightwalletdCompactBlock::decode(compact_block.payload_bytes.as_slice())
        .map_err(|_| IngestError::TipFollowParentMetadataUnavailable { height })?;
    let chain_metadata = compact_block
        .chain_metadata
        .ok_or(IngestError::TipFollowParentMetadataUnavailable { height })?;

    Ok(ChainTipMetadata::new(
        chain_metadata.sapling_commitment_tree_size,
        chain_metadata.orchard_commitment_tree_size,
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "private commit helper keeps the derive-function test injection visible"
)]
async fn commit_tip_follow_blocks<Source, Derive>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    derive_fn: &Derive,
    plan: TipFollowPlan,
    current_chain_epoch: Option<ChainEpoch>,
    retry_state: &mut IngestRetryState,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    Derive:
        Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError> + Send + Sync,
{
    let mut batch = CanonicalBatch::default();
    let mut running_tree_sizes = CommitmentTreeSizes::from_tip_metadata(plan.parent_tip_metadata);
    for source_block in plan.source_blocks {
        if source_block.network != config.node.network {
            return Err(IngestError::Source(
                zinder_source::SourceError::SourceProtocolMismatch {
                    reason: "source block network does not match tip-follow configuration",
                },
            ));
        }

        let fact_build_started_at = Instant::now();
        let built_outcome = derive_fn(&source_block)
            .map_err(IngestError::from)
            .and_then(|derived| {
                finalize_derived_block(derived, &mut running_tree_sizes).map_err(IngestError::from)
            });
        record_ingest_fact_build_outcome(fact_build_started_at, &built_outcome);
        let built = built_outcome?;
        batch.absorb(built);
    }

    let next_subtree_root_indexes =
        IngestSubtreeRootIndexes::from_tip_metadata(plan.parent_tip_metadata);
    let _updated_subtree_root_indexes = populate_subtree_root_artifacts(
        config.node.request_timeout,
        source,
        &mut batch,
        next_subtree_root_indexes,
        retry_state,
    )
    .await?;
    populate_tip_follow_tree_state_checkpoint(config, source, &mut batch, retry_state).await?;
    let chain_epoch_id = next_chain_epoch_id_from(current_chain_epoch.as_ref())?;
    let chain_epoch = chain_epoch_for_tip_commit(
        config.node.network,
        chain_epoch_id,
        current_chain_epoch,
        &batch,
    )?;

    commit_ingest_batch(store, chain_epoch, &mut batch, plan.reorg_window_change).await
}

async fn populate_tip_follow_tree_state_checkpoint<Source>(
    config: &TipFollowConfig,
    source: &Source,
    batch: &mut CanonicalBatch,
    retry_state: &mut IngestRetryState,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    if !batch.tree_states.is_empty() {
        return Ok(());
    }
    let Some(tip_block) = batch.block_headers.last() else {
        return Ok(());
    };
    if !source.capabilities().supports(NodeCapability::TreeState) {
        return Ok(());
    }

    let block_id = BlockId::new(tip_block.height, tip_block.block_hash);
    let source_tree_state = match fetch_tree_state_for_block_with_retry(
        config.node.request_timeout,
        source,
        block_id,
        retry_state,
    )
    .await
    {
        Ok(source_tree_state) => source_tree_state,
        Err(IngestError::Source(SourceError::NodeCapabilityMissing { .. })) => return Ok(()),
        Err(error) => return Err(error),
    };
    batch.push_tree_state_checkpoint(TreeStateArtifact::new(
        source_tree_state.block_id.height,
        source_tree_state.block_id.hash,
        source_tree_state.payload_bytes,
    ));
    Ok(())
}

fn chain_epoch_for_tip_commit(
    network: Network,
    chain_epoch_id: ChainEpochId,
    current_chain_epoch: Option<ChainEpoch>,
    batch: &CanonicalBatch,
) -> Result<ChainEpoch, IngestError> {
    let tip_block = batch
        .block_headers
        .last()
        .ok_or(IngestError::EmptyCanonicalBatch)?;
    let parent_finalized = current_chain_epoch.map_or(
        (BlockHeight::new(0), BlockHash::from_bytes([0; 32])),
        |chain_epoch| (chain_epoch.finalized_height, chain_epoch.finalized_hash),
    );

    Ok(ChainEpoch {
        id: chain_epoch_id,
        network,
        tip_height: tip_block.height,
        tip_hash: tip_block.block_hash,
        finalized_height: parent_finalized.0,
        finalized_hash: parent_finalized.1,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: batch.tip_metadata.ok_or(IngestError::EmptyCanonicalBatch)?,
        created_at: current_unix_millis()?,
    })
}

fn finalize_tip_if_ready(
    config: &TipFollowConfig,
    store: &PrimaryChainStore,
    chain_epoch: ChainEpoch,
) -> Result<Option<ChainEpochCommitOutcome>, IngestError> {
    let Some(finalized_height) =
        finalized_height_for_tip(chain_epoch.tip_height, config.reorg_window_blocks)
    else {
        return Ok(None);
    };
    if finalized_height <= chain_epoch.finalized_height {
        return Ok(None);
    }

    let reader = store.chain_epoch_reader_at(chain_epoch.id)?;
    let finalized_block = reader.block_header_at(finalized_height)?.ok_or(
        IngestError::TipFollowParentMetadataUnavailable {
            height: finalized_height,
        },
    )?;
    let finalized_chain_epoch = ChainEpoch {
        id: next_chain_epoch_id_after(chain_epoch.id)?,
        finalized_height,
        finalized_hash: finalized_block.block_hash,
        created_at: current_unix_millis()?,
        ..chain_epoch
    };
    let commit_outcome = store
        .commit_chain_epoch(
            ChainEpochArtifacts::new(
                finalized_chain_epoch,
                Vec::<zinder_core::BlockHeaderArtifact>::new(),
                Vec::new(),
            )
            .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
                height: finalized_height,
            }),
        )
        .map_err(IngestError::from)?;
    record_commit_outcome(&commit_outcome);

    Ok(Some(commit_outcome))
}

fn finalized_height_for_tip(
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) -> Option<BlockHeight> {
    (tip_height.value() > reorg_window_blocks)
        .then(|| BlockHeight::new(tip_height.value() - reorg_window_blocks))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error,
        num::NonZeroU32,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
    };

    use async_trait::async_trait;
    use futures_util::stream;
    use parking_lot::Mutex;
    use tempfile::tempdir;
    use zinder_core::{SubtreeRootHash, SubtreeRootIndex};
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
    use zinder_runtime::ReadinessCause;
    use zinder_source::{
        ChainTipNotification, ChainTipNotificationStream, NodeCapabilities, SourceBlockHeader,
        SourceError, SourceSubtreeRoot, SourceSubtreeRoots, ZebraJsonRpcSource,
    };

    use super::*;

    #[tokio::test]
    async fn tip_follow_commits_first_available_height() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-empty-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(3);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();

        let commit_outcome = test_tip_follow_once(&config, &source, &store, &mut retry_state)
            .await?
            .ok_or("expected a tip-follow commit")?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        assert_eq!(
            commit_outcome.chain_epoch.artifact_schema_version,
            CURRENT_ARTIFACT_SCHEMA_VERSION
        );
        assert_eq!(
            commit_outcome.chain_epoch.finalized_height,
            BlockHeight::new(0)
        );

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_skips_when_tip_hash_is_unchanged() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-unchanged-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(1);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _commit_outcome =
            test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        let skipped = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        assert!(skipped.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_extends_by_one_block() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-extend-store");
        let mut config = test_tip_follow_config(&storage_path, 10);
        config.poll_interval = Duration::from_millis(1);
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        let second = test_tip_follow_once(&config, &source, &store, &mut retry_state)
            .await?
            .ok_or("expected extension commit")?;

        assert_eq!(second.chain_epoch.tip_height, BlockHeight::new(2));
        let reader = store.current_chain_epoch_reader()?;
        assert_eq!(
            reader
                .block_header_at(BlockHeight::new(2))?
                .ok_or("missing block 2")?
                .parent_hash,
            block_hash(1)
        );

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_replaces_in_window_branch() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-reorg-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;
        let _second = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        source.replace_block(2, block_hash(20), block_hash(1));
        let reorged = test_tip_follow_once(&config, &source, &store, &mut retry_state)
            .await?
            .ok_or("expected replacement commit")?;

        assert_eq!(reorged.chain_epoch.tip_height, BlockHeight::new(2));
        assert_eq!(reorged.chain_epoch.tip_hash, block_hash(20));
        let reader = store.current_chain_epoch_reader()?;
        assert_eq!(
            reader
                .block_header_at(BlockHeight::new(2))?
                .ok_or("missing replacement block")?
                .block_hash,
            block_hash(20)
        );

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_waits_when_node_tip_rewinds_before_replacement()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-rewind-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;
        let _second = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        source.set_tip_height(1);
        let skipped = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        assert!(skipped.is_none());
        assert_eq!(
            store
                .current_chain_epoch()?
                .ok_or("missing chain epoch")?
                .tip_height,
            BlockHeight::new(2)
        );

        source.replace_block(2, block_hash(20), block_hash(1));
        source.set_tip_height(2);
        let reorged = test_tip_follow_once(&config, &source, &store, &mut retry_state)
            .await?
            .ok_or("expected replacement commit")?;

        assert_eq!(reorged.chain_epoch.tip_height, BlockHeight::new(2));
        assert_eq!(reorged.chain_epoch.tip_hash, block_hash(20));

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_exits_after_cancellation() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-cancel-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(1);
        let readiness = Readiness::default();
        let cancel = CancellationToken::new();
        cancel.cancel();

        tip_follow(&config, &source, &readiness, cancel).await?;

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_keeps_running_when_tip_id_is_temporarily_unavailable()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-tip-id-recovery-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(1).with_retryable_tip_failures(10);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let readiness = Readiness::default();
        let cancel = CancellationToken::new();
        let task = {
            let readiness = readiness.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_tip_follow_loop(
                    &config,
                    &source,
                    store,
                    &readiness,
                    None,
                    None,
                    cancel,
                    test_derive_block_fn,
                )
                .await
            })
        };

        wait_for_readiness_cause(&readiness, |cause| {
            matches!(cause, ReadinessCause::NodeUnavailable(_))
        })
        .await?;
        wait_for_readiness_cause(&readiness, |cause| matches!(cause, ReadinessCause::Ready))
            .await?;
        cancel.cancel();
        task.await??;

        Ok(())
    }

    #[tokio::test]
    async fn tip_follow_keeps_running_when_block_fetch_retry_deadline_is_exceeded()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-block-fetch-recovery-store");
        let config = test_tip_follow_config(&storage_path, 10);
        let source = TestNodeSource::linear(1).with_retryable_block_failures(10);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let readiness = Readiness::default();
        let cancel = CancellationToken::new();
        let task = {
            let readiness = readiness.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_tip_follow_loop(
                    &config,
                    &source,
                    store,
                    &readiness,
                    None,
                    None,
                    cancel,
                    test_derive_block_fn,
                )
                .await
            })
        };

        wait_for_readiness_cause(&readiness, |cause| {
            matches!(cause, ReadinessCause::NodeUnavailable(_))
        })
        .await?;
        wait_for_readiness_cause(&readiness, |cause| matches!(cause, ReadinessCause::Ready))
            .await?;
        cancel.cancel();
        task.await??;

        Ok(())
    }

    #[tokio::test]
    async fn chain_tip_wakeup_resubscribes_after_stream_error() -> Result<(), Box<dyn Error>> {
        let source = Arc::new(TestChainTipNotificationSource::new(vec![
            Box::pin(stream::iter(vec![Err(
                SourceError::ChainTipStreamUnavailable {
                    reason: "test stream failed".to_owned(),
                },
            )])),
            Box::pin(stream::iter(vec![Ok(ChainTipNotification {
                tip_id: BlockId::new(BlockHeight::new(1), block_hash(1)),
            })])),
        ]));
        let (sender, mut receiver) = mpsc::channel(CHAIN_TIP_WAKEUP_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let task = spawn_chain_tip_notification_wakeup(source.clone(), sender, cancel.clone());

        let wakeup = tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await?;

        cancel.cancel();
        task.abort();
        assert!(wakeup.is_some(), "expected a wake-up after re-subscribe");
        assert!(
            source.subscribe_count.load(Ordering::SeqCst) >= 2,
            "expected the chain-tip wake-up task to re-subscribe"
        );

        Ok(())
    }

    #[tokio::test]
    async fn readiness_state_reports_ready_when_lag_within_threshold() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-readiness-ready-store");
        let mut config = test_tip_follow_config(&storage_path, 10);
        config.lag_threshold_blocks = 1;
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;
        let _second = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        let node_tip_height = source.tip_id().await?.height;
        let readiness_state =
            super::compute_tip_follow_readiness_state(&store, node_tip_height, &config)?;

        assert!(matches!(readiness_state.cause, ReadinessCause::Ready));
        assert_eq!(readiness_state.current_height, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn readiness_state_reports_syncing_when_lag_exceeds_threshold()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-readiness-syncing-store");
        let mut config = test_tip_follow_config(&storage_path, 10);
        config.lag_threshold_blocks = 1;
        let source = TestNodeSource::linear(10);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        let node_tip_height = source.tip_id().await?.height;
        let readiness_state =
            super::compute_tip_follow_readiness_state(&store, node_tip_height, &config)?;

        assert!(matches!(
            readiness_state.cause,
            ReadinessCause::Syncing {
                lag_blocks: Some(9),
            }
        ));
        assert_eq!(readiness_state.current_height, Some(1));
        assert_eq!(readiness_state.target_height, Some(10));
        Ok(())
    }

    #[tokio::test]
    async fn readiness_state_reports_syncing_when_node_tip_rewinds() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-readiness-rewind-store");
        let mut config = test_tip_follow_config(&storage_path, 10);
        config.lag_threshold_blocks = 1;
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;
        let _second = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        let readiness_state =
            super::compute_tip_follow_readiness_state(&store, BlockHeight::new(1), &config)?;

        assert!(matches!(
            readiness_state.cause,
            ReadinessCause::Syncing {
                lag_blocks: Some(1),
            }
        ));
        assert_eq!(readiness_state.current_height, Some(2));
        assert_eq!(readiness_state.target_height, Some(1));
        Ok(())
    }

    fn test_tip_follow_config(storage_path: &Path, reorg_window_blocks: u32) -> TipFollowConfig {
        TipFollowConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:39232".to_owned(),
                zinder_source::NodeAuth::None,
                Duration::from_secs(30),
                zinder_source::DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            storage_path: storage_path.to_owned(),
            canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            raw_blob_policy: RawBlobPolicy::All,
            network_upgrade_activations: Arc::new(
                zinder_testkit::sample_regtest_upgrade_activations(),
            ),
            reorg_window_blocks,
            poll_interval: Duration::from_millis(1),
            lag_threshold_blocks: super::DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        }
    }

    async fn test_tip_follow_once(
        config: &TipFollowConfig,
        source: &TestNodeSource,
        store: &PrimaryChainStore,
        retry_state: &mut IngestRetryState,
    ) -> Result<Option<ChainEpochCommitOutcome>, IngestError> {
        let iteration =
            tip_follow_once(config, source, store, retry_state, test_derive_block_fn).await?;
        Ok(iteration.commit_outcome)
    }

    /// Test derive function for the tip-follow loop.
    ///
    /// Ignores the source block content and returns a synthetic
    /// `DerivedBlockArtifacts`, so tests can drive the loop against
    /// `TestNodeSource`'s mock blocks (which carry
    /// `format!("raw-block-{height}")` payloads rather than real Zcash
    /// bytes that would parse through `derive_block`).
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match the Fn(&SourceBlock) -> Result<DerivedBlockArtifacts, _> shape tip-follow expects"
    )]
    fn test_derive_block_fn(
        source_block: &SourceBlock,
    ) -> Result<DerivedBlockArtifacts, crate::ArtifactDeriveError> {
        Ok(DerivedBlockArtifacts {
            block_header: zinder_core::BlockHeaderArtifact::new(
                source_block.height,
                source_block.hash,
                source_block.parent_hash,
                [0; 32],
                [0; 32],
                i64::from(source_block.block_time_seconds),
                0,
                [0; 32],
                0,
                u64::try_from(source_block.raw_block_bytes.len()).unwrap_or(u64::MAX),
            ),
            block_blob: Some(zinder_core::BlockBlobArtifact::new(
                source_block.height,
                source_block.hash,
                source_block.parent_hash,
                source_block.raw_block_bytes.clone(),
            )),
            partial_compact_block: LightwalletdCompactBlock {
                proto_version: 1,
                height: u64::from(source_block.height.value()),
                hash: zinder_core::wire::encode_internal_block_hash(source_block.hash).to_vec(),
                prev_hash: zinder_core::wire::encode_internal_block_hash(source_block.parent_hash)
                    .to_vec(),
                time: source_block.block_time_seconds,
                header: Vec::new(),
                vtx: Vec::new(),
                chain_metadata: None,
            },
            tree_size_additions: CommitmentTreeSizes::default(),
            block_transaction_index: Vec::new(),
            transaction_locations: Vec::new(),
            transaction_facts: Vec::new(),
            transaction_blobs: Vec::new(),
            address_output_index: Vec::new(),
            transparent_outputs_by_outpoint: Vec::new(),
        })
    }

    struct TestNodeSource {
        tip_height: AtomicU32,
        tip_failures_remaining: AtomicU32,
        block_failures_remaining: AtomicU32,
        blocks: Mutex<Vec<TestSourceBlock>>,
    }

    #[derive(Clone, Copy)]
    struct TestSourceBlock {
        height: BlockHeight,
        hash: BlockHash,
        parent_hash: BlockHash,
    }

    impl TestNodeSource {
        fn linear(tip_height: u32) -> Self {
            let blocks = (1..=tip_height)
                .map(|height| TestSourceBlock {
                    height: BlockHeight::new(height),
                    hash: block_hash(height),
                    parent_hash: block_hash(height.saturating_sub(1)),
                })
                .collect();

            Self {
                tip_height: AtomicU32::new(tip_height),
                tip_failures_remaining: AtomicU32::new(0),
                block_failures_remaining: AtomicU32::new(0),
                blocks: Mutex::new(blocks),
            }
        }

        fn with_retryable_tip_failures(self, failure_count: u32) -> Self {
            self.tip_failures_remaining
                .store(failure_count, Ordering::SeqCst);
            self
        }

        fn with_retryable_block_failures(self, failure_count: u32) -> Self {
            self.block_failures_remaining
                .store(failure_count, Ordering::SeqCst);
            self
        }

        fn set_tip_height(&self, tip_height: u32) {
            self.tip_height.store(tip_height, Ordering::SeqCst);
        }

        fn replace_block(&self, height: u32, hash: BlockHash, parent_hash: BlockHash) {
            {
                let mut blocks = self.blocks.lock();
                if let Some(block) = blocks
                    .iter_mut()
                    .find(|block| block.height == BlockHeight::new(height))
                {
                    block.hash = hash;
                    block.parent_hash = parent_hash;
                }
            }
        }
    }

    #[async_trait]
    impl NodeSource for TestNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
            if consume_retryable_failure(&self.block_failures_remaining) {
                return Err(SourceError::BlockUnavailable {
                    height,
                    reason: "test block temporarily unavailable".to_owned(),
                });
            }

            let block = self
                .blocks
                .lock()
                .iter()
                .copied()
                .find(|block| block.height == height)
                .ok_or_else(|| SourceError::BlockUnavailable {
                    height,
                    reason: "test block unavailable".to_owned(),
                })?;
            let header = SourceBlockHeader {
                network: Network::ZcashRegtest,
                height,
                hash: block.hash,
                parent_hash: block.parent_hash,
                block_time_seconds: 1_774_668_400,
            };

            Ok(SourceBlock::new(
                header,
                format!("raw-block-{}", height.value()).into_bytes(),
            ))
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            if consume_retryable_failure(&self.tip_failures_remaining) {
                return Err(SourceError::NodeUnavailable {
                    reason: "test tip temporarily unavailable".to_owned(),
                });
            }

            let height = BlockHeight::new(self.tip_height.load(Ordering::SeqCst));
            let hash = self
                .blocks
                .lock()
                .iter()
                .copied()
                .find(|block| block.height == height)
                .map_or_else(|| BlockHash::from_bytes([0; 32]), |block| block.hash);
            Ok(BlockId::new(height, hash))
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: zinder_core::ShieldedProtocol,
            start_index: SubtreeRootIndex,
            max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            let subtree_roots: Vec<_> = (0..max_entries.get())
                .map(|offset| {
                    SourceSubtreeRoot::new(
                        SubtreeRootIndex::new(start_index.value().saturating_add(offset)),
                        SubtreeRootHash::from_bytes([0x44; 32]),
                        BlockHeight::new(1),
                    )
                })
                .collect();

            Ok(SourceSubtreeRoots::new(
                protocol,
                start_index,
                subtree_roots,
            ))
        }
    }

    struct TestChainTipNotificationSource {
        subscribe_count: AtomicU32,
        streams: Mutex<VecDeque<ChainTipNotificationStream>>,
    }

    impl TestChainTipNotificationSource {
        fn new(streams: Vec<ChainTipNotificationStream>) -> Self {
            Self {
                subscribe_count: AtomicU32::new(0),
                streams: Mutex::new(VecDeque::from(streams)),
            }
        }
    }

    #[async_trait]
    impl ChainTipNotificationSource for TestChainTipNotificationSource {
        async fn subscribe(&self) -> Result<ChainTipNotificationStream, SourceError> {
            self.subscribe_count.fetch_add(1, Ordering::SeqCst);
            self.streams
                .lock()
                .pop_front()
                .ok_or_else(|| SourceError::ChainTipStreamUnavailable {
                    reason: "test subscription unavailable".to_owned(),
                })
        }
    }

    async fn wait_for_readiness_cause(
        readiness: &Readiness,
        mut accepts: impl FnMut(ReadinessCause) -> bool,
    ) -> Result<(), Box<dyn Error>> {
        match tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if accepts(readiness.report().cause) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        {
            Ok(()) => {}
            Err(error) => {
                return Err(format!(
                    "timed out waiting for readiness cause; last report: {:?}; {error}",
                    readiness.report()
                )
                .into());
            }
        }
        Ok(())
    }

    fn consume_retryable_failure(counter: &AtomicU32) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failure_count| {
                failure_count.checked_sub(1)
            })
            .is_ok()
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }
}
