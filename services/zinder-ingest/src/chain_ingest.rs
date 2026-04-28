//! Shared per-block chain-ingest engine.
//!
//! Both backfill and tip-following ingest run through this module: it owns
//! retryable node fetches, artifact-batch state, subtree-root population,
//! and the `commit_chain_epoch` translation. Callers decide which
//! [`ReorgWindowChange`] their commit represents and construct the durable
//! [`ChainEpoch`] that the engine writes.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootIndex, TransactionArtifact, TransparentAddressUtxoArtifact,
    TransparentUtxoSpendArtifact, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_source::{NodeSource, SourceBlock, SourceError, SourceSubtreeRoots};
use zinder_store::{
    ChainEpochArtifacts, ChainEpochCommitOutcome, ChainEvent, PrimaryChainStore, ReorgWindowChange,
    StoreError,
};

use crate::{
    ArtifactDeriveError, artifact_builder::CompactBlockArtifactBuilder, derive_block_artifact,
};

const FETCH_RETRY_MAX_ATTEMPTS: u32 = 5;
#[cfg(not(test))]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(test)]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(1);
const FETCH_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
const FETCH_RETRY_FAILURE_BUDGET: u32 = 100;

/// Supported node source kinds for the current ingestion slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeSourceKind {
    /// Zebra JSON-RPC source.
    ZebraJsonRpc,
}

/// Error returned by ingestion operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IngestError {
    /// Requested network name is not supported.
    #[error("unknown network: {network_name}")]
    UnknownNetwork {
        /// User-provided network name.
        network_name: String,
    },

    /// Requested node source name is not supported.
    #[error("unknown node source: {node_source}")]
    UnknownNodeSource {
        /// User-provided node source name.
        node_source: String,
    },

    /// Requested backfill range is invalid.
    #[error("invalid backfill range: from height {from_height:?} exceeds to height {to_height:?}")]
    InvalidBackfillRange {
        /// First requested height.
        from_height: BlockHeight,
        /// Last requested height.
        to_height: BlockHeight,
    },

    /// Commit batch size is invalid.
    #[error("invalid commit batch size: commit batch blocks must be greater than zero")]
    InvalidCommitBatchBlocks,

    /// Reorg window size is invalid.
    #[error("invalid reorg window: reorg window blocks must be greater than zero")]
    InvalidReorgWindowBlocks,

    /// Tip-follow poll interval is invalid.
    #[error("invalid tip-follow poll interval: poll interval must be greater than zero")]
    InvalidTipFollowPollInterval,

    /// JSON-RPC response byte limit is invalid.
    #[error("invalid JSON-RPC response byte limit: max response bytes must be greater than zero")]
    InvalidMaxResponseBytes,

    /// Node returned fewer subtree roots than committed tree sizes require.
    #[error(
        "{protocol:?} subtree roots are unavailable from {start_index:?}: expected {expected_count}, got {actual_count}"
    )]
    SubtreeRootsUnavailable {
        /// Shielded protocol requested.
        protocol: ShieldedProtocol,
        /// First requested subtree-root index.
        start_index: SubtreeRootIndex,
        /// Number of subtree roots required by chain metadata.
        expected_count: u32,
        /// Number of subtree roots returned by the node source.
        actual_count: usize,
    },

    /// Source returned a subtree root that cannot be bound to a committed block.
    #[error(
        "{protocol:?} subtree root {subtree_index:?} completes at {completing_block_height:?}, outside the committed batch"
    )]
    SubtreeRootCompletingBlockMissing {
        /// Shielded protocol requested.
        protocol: ShieldedProtocol,
        /// Subtree-root index returned by the node source.
        subtree_index: SubtreeRootIndex,
        /// Height of the block that completed this subtree.
        completing_block_height: BlockHeight,
    },

    /// Shielded protocol is not supported by the current ingest subtree-root tracker.
    #[error("{protocol:?} subtree roots are not supported by this ingest tracker")]
    UnsupportedShieldedProtocol {
        /// Unsupported shielded protocol.
        protocol: ShieldedProtocol,
    },

    /// Internal batching produced an empty commit.
    #[error("internal error: attempted to commit an empty ingest batch")]
    EmptyIngestBatch,

    /// Backfill loop ended without committing any batch.
    #[error("internal error: backfill loop produced no commit")]
    BackfillProducedNoCommit,

    /// Historical backfill was asked to finalize blocks inside the live reorg window.
    #[error(
        "backfill to height {to_height:?} is inside the node-reported reorg window: tip {tip_height:?}, reorg window {reorg_window_blocks} blocks, maximum historical height {maximum_historical_height:?}; pass --allow-near-tip-finalize only for local or explicitly disposable stores"
    )]
    NearTipBackfillRequiresExplicitFinalize {
        /// Last requested backfill height.
        to_height: BlockHeight,
        /// Current node tip height.
        tip_height: BlockHeight,
        /// Configured store reorg window in blocks.
        reorg_window_blocks: u32,
        /// Highest height that can be finalized without explicit override.
        maximum_historical_height: BlockHeight,
    },

    /// Backfill cannot derive a chain-global commitment-tree size base.
    #[error(
        "backfill from height {from_height:?} requires contiguous commitment-tree metadata; start a fresh store at height 1 or append immediately after current tip {current_tip_height:?}"
    )]
    BackfillRequiresContiguousTipMetadata {
        /// First requested backfill height.
        from_height: BlockHeight,
        /// Current store tip height, when the store is not empty.
        current_tip_height: Option<BlockHeight>,
    },

    /// Backfill checkpoint height does not match the requested `from_height`.
    #[error(
        "backfill checkpoint height {checkpoint_height:?} does not align with from_height {from_height:?}; from_height must equal checkpoint_height + 1"
    )]
    BackfillCheckpointMisaligned {
        /// Operator-supplied checkpoint height.
        checkpoint_height: BlockHeight,
        /// Requested first backfill height.
        from_height: BlockHeight,
    },

    /// Tip-follow observed a node tip below the current store tip.
    #[error("node tip {observed_tip_height:?} is behind current store tip {current_tip_height:?}")]
    TipFollowObservedTipBehindStore {
        /// Node tip height.
        observed_tip_height: BlockHeight,
        /// Current store tip height.
        current_tip_height: BlockHeight,
    },

    /// Tip-follow could not find a common ancestor inside the visible chain.
    #[error("could not find a common ancestor for replacement block {replacement_tip_height:?}")]
    TipFollowCommonAncestorMissing {
        /// Replacement tip that started ancestor search.
        replacement_tip_height: BlockHeight,
    },

    /// Tip-follow could not recover chain metadata for a replacement parent.
    #[error("chain-tip metadata is unavailable at replacement parent height {height:?}")]
    TipFollowParentMetadataUnavailable {
        /// Parent height whose metadata was required.
        height: BlockHeight,
    },

    /// Reorg replacement exceeded the configured non-finalized window.
    #[error(
        "reorg from {from_height:?} exceeds the configured window: replacement depth {replacement_depth}, window {configured_window_blocks} blocks"
    )]
    ReorgWindowExceeded {
        /// First replacement height.
        from_height: BlockHeight,
        /// Number of replaced visible heights.
        replacement_depth: u32,
        /// Configured reorg window.
        configured_window_blocks: u32,
    },

    /// Retryable node failures exceeded the per-run ingest budget.
    #[error(
        "ingest source retry budget exceeded during {operation}: {retryable_failures} failures, budget {failure_budget}"
    )]
    SourceRetryBudgetExceeded {
        /// Source operation being retried.
        operation: String,
        /// Retryable failures observed in this run.
        retryable_failures: u32,
        /// Configured retryable failure budget.
        failure_budget: u32,
    },

    /// Retryable node failures outlasted the per-block fetch deadline.
    #[error("ingest source retry deadline exceeded during {operation}: {reason}")]
    SourceRetryDeadlineExceeded {
        /// Source operation being retried.
        operation: String,
        /// Last retryable failure reason.
        reason: String,
    },

    /// System time is before Unix epoch.
    #[error("system time is before Unix epoch")]
    SystemTimeBeforeUnixEpoch {
        /// System time error.
        #[source]
        source: std::time::SystemTimeError,
    },

    /// Current timestamp does not fit Zinder's timestamp value.
    #[error("current Unix timestamp does not fit u64 milliseconds")]
    TimestampTooLarge,

    /// Node source failed.
    #[error(transparent)]
    Source(#[from] SourceError),

    /// Artifact derivation failed.
    #[error(transparent)]
    ArtifactDerive(#[from] ArtifactDeriveError),

    /// Canonical store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Builds canonical artifacts from one node source block.
pub(crate) trait ArtifactBuilder {
    /// Builds canonical artifacts from `source_block`, advancing internal
    /// commitment-tree state as the chain extends.
    fn build(&mut self, source_block: &SourceBlock) -> Result<BuiltArtifacts, ArtifactDeriveError>;
}

/// Canonical artifacts produced by one [`ArtifactBuilder::build`] call.
#[derive(Debug)]
pub(crate) struct BuiltArtifacts {
    pub(crate) block: BlockArtifact,
    pub(crate) compact_block: CompactBlockArtifact,
    pub(crate) transactions: Vec<TransactionArtifact>,
    pub(crate) transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    pub(crate) transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    pub(crate) tip_metadata: ChainTipMetadata,
}

/// Live-source [`ArtifactBuilder`] that tracks commitment-tree size across
/// contiguous block builds.
#[derive(Debug)]
pub(crate) struct IngestArtifactBuilder {
    compact_block_artifact_builder: CompactBlockArtifactBuilder,
}

impl IngestArtifactBuilder {
    /// Creates a builder seeded with `initial_tip_metadata` so the first build
    /// produces tree-size deltas relative to the parent chain state.
    pub(crate) fn with_initial_tip_metadata(initial_tip_metadata: ChainTipMetadata) -> Self {
        Self {
            compact_block_artifact_builder: CompactBlockArtifactBuilder::with_initial_tip_metadata(
                initial_tip_metadata,
            ),
        }
    }
}

impl ArtifactBuilder for IngestArtifactBuilder {
    fn build(&mut self, source_block: &SourceBlock) -> Result<BuiltArtifacts, ArtifactDeriveError> {
        let block = derive_block_artifact(source_block)?;
        let compact_block_build = self.compact_block_artifact_builder.build(source_block)?;

        Ok(BuiltArtifacts {
            block,
            compact_block: compact_block_build.compact_block,
            transactions: compact_block_build.transactions,
            transparent_address_utxos: compact_block_build.transparent_address_utxos,
            transparent_utxo_spends: compact_block_build.transparent_utxo_spends,
            tip_metadata: compact_block_build.tip_metadata,
        })
    }
}

/// In-flight artifact batch accumulated between commits.
#[derive(Default)]
pub(crate) struct IngestBatch {
    pub(crate) finalized_blocks: Vec<BlockArtifact>,
    pub(crate) compact_blocks: Vec<CompactBlockArtifact>,
    pub(crate) transactions: Vec<TransactionArtifact>,
    pub(crate) tree_states: Vec<TreeStateArtifact>,
    pub(crate) subtree_roots: Vec<SubtreeRootArtifact>,
    pub(crate) transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    pub(crate) transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    pub(crate) tip_metadata: Option<ChainTipMetadata>,
}

impl IngestBatch {
    pub(crate) fn len(&self) -> usize {
        self.finalized_blocks.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.finalized_blocks.is_empty()
    }
}

/// Tracks the next subtree-root index per shielded protocol so a follow-up
/// batch knows which roots the source has already provided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IngestSubtreeRootIndexes {
    sapling: SubtreeRootIndex,
    orchard: SubtreeRootIndex,
}

impl Default for IngestSubtreeRootIndexes {
    fn default() -> Self {
        Self {
            sapling: SubtreeRootIndex::new(0),
            orchard: SubtreeRootIndex::new(0),
        }
    }
}

impl IngestSubtreeRootIndexes {
    pub(crate) fn from_tip_metadata(tip_metadata: ChainTipMetadata) -> Self {
        Self {
            sapling: SubtreeRootIndex::new(
                tip_metadata.completed_subtree_count(ShieldedProtocol::Sapling),
            ),
            orchard: SubtreeRootIndex::new(
                tip_metadata.completed_subtree_count(ShieldedProtocol::Orchard),
            ),
        }
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "non-exhaustive core protocols must fail closed until ingest tracks them"
    )]
    pub(crate) fn index_for(
        self,
        protocol: ShieldedProtocol,
    ) -> Result<SubtreeRootIndex, IngestError> {
        match protocol {
            ShieldedProtocol::Sapling => Ok(self.sapling),
            ShieldedProtocol::Orchard => Ok(self.orchard),
            _ => Err(IngestError::UnsupportedShieldedProtocol { protocol }),
        }
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "non-exhaustive core protocols must fail closed until ingest tracks them"
    )]
    pub(crate) fn set_index(
        &mut self,
        protocol: ShieldedProtocol,
        subtree_index: SubtreeRootIndex,
    ) -> Result<(), IngestError> {
        match protocol {
            ShieldedProtocol::Sapling => self.sapling = subtree_index,
            ShieldedProtocol::Orchard => self.orchard = subtree_index,
            _ => return Err(IngestError::UnsupportedShieldedProtocol { protocol }),
        }

        Ok(())
    }
}

/// Counter of retryable source failures observed across a single ingest run.
#[derive(Default)]
pub(crate) struct IngestRetryState {
    retryable_failures: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct SubtreeRootFetchParams {
    pub(crate) protocol: ShieldedProtocol,
    pub(crate) start_index: SubtreeRootIndex,
    pub(crate) max_entries: NonZeroU32,
}

/// Fetches one block from the node source, retrying transient failures
/// until the per-call deadline or the per-run failure budget is exhausted.
pub(crate) async fn fetch_block_with_retry<Source>(
    request_timeout: Duration,
    source: &Source,
    height: BlockHeight,
    retry_state: &mut IngestRetryState,
) -> Result<SourceBlock, IngestError>
where
    Source: NodeSource,
{
    let started_at = Instant::now();
    let source_outcome = retry_source_request(
        "fetch_block",
        format!("fetch block at height {}", height.value()),
        request_timeout,
        retry_state,
        || async { source.fetch_block_by_height(height).await },
    )
    .await;
    record_ingest_source_outcome("fetch_block", started_at, &source_outcome);
    source_outcome
}

/// Fetches subtree roots from the node source with the same retry policy
/// as block fetches.
pub(crate) async fn fetch_subtree_roots_with_retry<Source>(
    request_timeout: Duration,
    source: &Source,
    request: SubtreeRootFetchParams,
    retry_state: &mut IngestRetryState,
) -> Result<SourceSubtreeRoots, IngestError>
where
    Source: NodeSource,
{
    let started_at = Instant::now();
    let source_outcome = retry_source_request(
        "fetch_subtree_roots",
        format!(
            "fetch {:?} subtree roots from index {}",
            request.protocol,
            request.start_index.value()
        ),
        request_timeout,
        retry_state,
        || async {
            source
                .fetch_subtree_roots(request.protocol, request.start_index, request.max_entries)
                .await
        },
    )
    .await;
    record_ingest_source_outcome("fetch_subtree_roots", started_at, &source_outcome);
    source_outcome
}

async fn retry_source_request<RequestResult, Fut, MakeRequest>(
    operation_label: &'static str,
    operation: String,
    request_timeout: Duration,
    retry_state: &mut IngestRetryState,
    mut request: MakeRequest,
) -> Result<RequestResult, IngestError>
where
    MakeRequest: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<RequestResult, SourceError>>,
{
    let started_at = Instant::now();
    let deadline = fetch_retry_deadline(request_timeout);
    let mut attempt = 1;
    let mut next_backoff = FETCH_RETRY_INITIAL_BACKOFF;

    loop {
        match request().await {
            Ok(response) => return Ok(response),
            Err(error) if source_error_is_retryable(&error) => {
                retry_state.retryable_failures = retry_state.retryable_failures.saturating_add(1);
                metrics::counter!(
                    "zinder_ingest_source_retry_total",
                    "operation" => operation_label
                )
                .increment(1);
                if retry_state.retryable_failures > FETCH_RETRY_FAILURE_BUDGET {
                    return Err(IngestError::SourceRetryBudgetExceeded {
                        operation,
                        retryable_failures: retry_state.retryable_failures,
                        failure_budget: FETCH_RETRY_FAILURE_BUDGET,
                    });
                }

                if attempt >= FETCH_RETRY_MAX_ATTEMPTS
                    || started_at.elapsed().saturating_add(next_backoff) > deadline
                {
                    return Err(IngestError::SourceRetryDeadlineExceeded {
                        operation,
                        reason: error.to_string(),
                    });
                }

                tokio::time::sleep(next_backoff).await;
                next_backoff = next_fetch_retry_backoff(next_backoff);
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(IngestError::Source(error)),
        }
    }
}

fn fetch_retry_deadline(request_timeout: Duration) -> Duration {
    request_timeout
        .saturating_mul(FETCH_RETRY_MAX_ATTEMPTS)
        .max(Duration::from_secs(5))
}

fn next_fetch_retry_backoff(current_backoff: Duration) -> Duration {
    current_backoff
        .saturating_mul(2)
        .min(FETCH_RETRY_MAX_BACKOFF)
}

pub(crate) fn source_error_is_retryable(error: &SourceError) -> bool {
    error.is_retryable()
}

/// Classifies an already-built candidate chain segment.
///
/// Parent-hash continuity is the reachable rule for the current polling source:
/// Zebra JSON-RPC exposes one upstream-node-selected best chain at a time. If a
/// future `non_finalized_blocks` capability exposes competing branches, this
/// function is the place to add cumulative-chainwork tie-breaking before it
/// returns a replacement transition.
pub(crate) fn select_best_chain(
    current_chain_epoch: ChainEpoch,
    candidate_blocks: &[SourceBlock],
    reorg_window_blocks: u32,
) -> Result<ReorgWindowChange, IngestError> {
    let Some(first_candidate) = candidate_blocks.first() else {
        return Ok(ReorgWindowChange::Unchanged);
    };
    let last_candidate = candidate_blocks
        .last()
        .ok_or(IngestError::EmptyIngestBatch)?;

    if first_candidate.height
        == BlockHeight::new(current_chain_epoch.tip_height.value().saturating_add(1))
        && first_candidate.parent_hash == current_chain_epoch.tip_hash
    {
        return Ok(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(first_candidate.height, last_candidate.height),
        });
    }

    let replacement_depth = current_chain_epoch
        .tip_height
        .value()
        .saturating_sub(first_candidate.height.value())
        .saturating_add(1);
    if replacement_depth > reorg_window_blocks {
        return Err(IngestError::ReorgWindowExceeded {
            from_height: first_candidate.height,
            replacement_depth,
            configured_window_blocks: reorg_window_blocks,
        });
    }

    Ok(ReorgWindowChange::Replace {
        from_height: first_candidate.height,
    })
}

/// Fetches and appends subtree-root artifacts for any subtrees completed by
/// the blocks already accumulated in `batch`.
pub(crate) async fn populate_subtree_root_artifacts<Source>(
    request_timeout: Duration,
    source: &Source,
    batch: &mut IngestBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: &mut IngestRetryState,
) -> Result<IngestSubtreeRootIndexes, IngestError>
where
    Source: NodeSource,
{
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyIngestBatch)?;
    let block_hash_by_height = batch
        .finalized_blocks
        .iter()
        .map(|block| (block.height, block.block_hash))
        .collect::<HashMap<_, _>>();
    let mut updated_subtree_root_indexes = next_subtree_root_indexes;

    for (protocol, completed_subtree_count) in [
        (
            ShieldedProtocol::Sapling,
            tip_metadata.completed_subtree_count(ShieldedProtocol::Sapling),
        ),
        (
            ShieldedProtocol::Orchard,
            tip_metadata.completed_subtree_count(ShieldedProtocol::Orchard),
        ),
    ] {
        let next_subtree_root_index = next_subtree_root_indexes.index_for(protocol)?;
        if completed_subtree_count <= next_subtree_root_index.value() {
            continue;
        }

        let required_count = completed_subtree_count - next_subtree_root_index.value();
        let max_entries =
            NonZeroU32::new(required_count).ok_or(IngestError::SubtreeRootsUnavailable {
                protocol,
                start_index: next_subtree_root_index,
                expected_count: required_count,
                actual_count: 0,
            })?;
        let source_subtree_roots = fetch_subtree_roots_with_retry(
            request_timeout,
            source,
            SubtreeRootFetchParams {
                protocol,
                start_index: next_subtree_root_index,
                max_entries,
            },
            retry_state,
        )
        .await?;
        append_subtree_root_artifacts(
            batch,
            &block_hash_by_height,
            source_subtree_roots,
            required_count,
        )?;
        updated_subtree_root_indexes
            .set_index(protocol, SubtreeRootIndex::new(completed_subtree_count))?;
    }

    Ok(updated_subtree_root_indexes)
}

fn append_subtree_root_artifacts(
    batch: &mut IngestBatch,
    block_hash_by_height: &HashMap<BlockHeight, zinder_core::BlockHash>,
    source_subtree_roots: SourceSubtreeRoots,
    expected_count: u32,
) -> Result<(), IngestError> {
    if source_subtree_roots.subtree_roots.len() < u32_to_usize(expected_count) {
        return Err(IngestError::SubtreeRootsUnavailable {
            protocol: source_subtree_roots.protocol,
            start_index: source_subtree_roots.start_index,
            expected_count,
            actual_count: source_subtree_roots.subtree_roots.len(),
        });
    }

    for source_subtree_root in source_subtree_roots.subtree_roots {
        let Some(completing_block_hash) =
            block_hash_by_height.get(&source_subtree_root.completing_block_height)
        else {
            return Err(IngestError::SubtreeRootCompletingBlockMissing {
                protocol: source_subtree_roots.protocol,
                subtree_index: source_subtree_root.subtree_index,
                completing_block_height: source_subtree_root.completing_block_height,
            });
        };

        batch.subtree_roots.push(SubtreeRootArtifact::new(
            source_subtree_roots.protocol,
            source_subtree_root.subtree_index,
            source_subtree_root.root_hash,
            source_subtree_root.completing_block_height,
            *completing_block_hash,
        ));
    }

    Ok(())
}

/// Drains `batch` into a [`ChainEpochArtifacts`] commit, atomically applies
/// `chain_epoch` and `reorg_window_change` to the store, and returns the
/// commit outcome.
///
/// Each caller decides what `chain_epoch` and `reorg_window_change` mean for
/// its mode: backfill always advances finalization to the new tip; the
/// tip-follower issues `Extend` for tip advancement and `Replace` for
/// reorgs, then advances finalization separately once the new tip is at
/// least `reorg_window_blocks` deep.
pub(crate) fn commit_ingest_batch(
    store: &PrimaryChainStore,
    chain_epoch: ChainEpoch,
    batch: &mut IngestBatch,
    reorg_window_change: ReorgWindowChange,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let started_at = Instant::now();
    let block_count = batch.len();
    if batch.is_empty() {
        let commit_outcome = Err(IngestError::EmptyIngestBatch);
        record_ingest_commit_outcome(started_at, block_count, &commit_outcome);
        return commit_outcome;
    }
    batch.tip_metadata = None;
    let mut artifacts = ChainEpochArtifacts::new(
        chain_epoch,
        std::mem::take(&mut batch.finalized_blocks),
        std::mem::take(&mut batch.compact_blocks),
    );

    if !batch.transactions.is_empty() {
        artifacts = artifacts.with_transactions(std::mem::take(&mut batch.transactions));
    }

    if !batch.tree_states.is_empty() {
        artifacts = artifacts.with_tree_states(std::mem::take(&mut batch.tree_states));
    }

    if !batch.subtree_roots.is_empty() {
        artifacts = artifacts.with_subtree_roots(std::mem::take(&mut batch.subtree_roots));
    }

    if !batch.transparent_address_utxos.is_empty() {
        artifacts = artifacts
            .with_transparent_address_utxos(std::mem::take(&mut batch.transparent_address_utxos));
    }

    if !batch.transparent_utxo_spends.is_empty() {
        artifacts = artifacts
            .with_transparent_utxo_spends(std::mem::take(&mut batch.transparent_utxo_spends));
    }

    let commit_result = store
        .commit_chain_epoch(artifacts.with_reorg_window_change(reorg_window_change))
        .map_err(IngestError::from);

    match commit_result {
        Ok(commit_summary) => {
            record_commit_outcome(&commit_summary);
            let commit_outcome = Ok(commit_summary);
            record_ingest_commit_outcome(started_at, block_count, &commit_outcome);
            commit_outcome
        }
        Err(error) => {
            let commit_outcome = Err(error);
            record_ingest_commit_outcome(started_at, block_count, &commit_outcome);
            commit_outcome
        }
    }
}

fn record_ingest_source_outcome<Response>(
    operation: &'static str,
    started_at: Instant,
    source_outcome: &Result<Response, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_source_request_duration_seconds",
        "operation" => operation,
        "status" => outcome_status(source_outcome),
        "error_class" => ingest_error_class(source_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_source_request_total",
        "operation" => operation,
        "status" => outcome_status(source_outcome),
        "error_class" => ingest_error_class(source_outcome.as_ref().err())
    )
    .increment(1);
}

fn record_ingest_commit_outcome(
    started_at: Instant,
    block_count: usize,
    commit_outcome: &Result<ChainEpochCommitOutcome, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_commit_duration_seconds",
        "status" => outcome_status(commit_outcome),
        "error_class" => ingest_error_class(commit_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::histogram!(
        "zinder_ingest_commit_batch_block_count",
        "status" => outcome_status(commit_outcome)
    )
    .record(usize_to_u32_saturating(block_count));
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn ingest_error_class(error: Option<&IngestError>) -> &'static str {
    match error {
        None => "none",
        Some(IngestError::UnknownNetwork { .. }) => "unknown_network",
        Some(IngestError::UnknownNodeSource { .. }) => "unknown_node_source",
        Some(IngestError::InvalidBackfillRange { .. }) => "invalid_backfill_range",
        Some(IngestError::InvalidCommitBatchBlocks) => "invalid_commit_batch_blocks",
        Some(IngestError::InvalidReorgWindowBlocks) => "invalid_reorg_window_blocks",
        Some(IngestError::InvalidTipFollowPollInterval) => "invalid_tip_follow_poll_interval",
        Some(IngestError::InvalidMaxResponseBytes) => "invalid_max_response_bytes",
        Some(IngestError::SubtreeRootsUnavailable { .. }) => "subtree_roots_unavailable",
        Some(IngestError::SubtreeRootCompletingBlockMissing { .. }) => {
            "subtree_root_completing_block_missing"
        }
        Some(IngestError::UnsupportedShieldedProtocol { .. }) => "unsupported_shielded_protocol",
        Some(IngestError::EmptyIngestBatch) => "empty_ingest_batch",
        Some(IngestError::BackfillProducedNoCommit) => "backfill_produced_no_commit",
        Some(IngestError::NearTipBackfillRequiresExplicitFinalize { .. }) => {
            "near_tip_backfill_requires_explicit_finalize"
        }
        Some(IngestError::BackfillRequiresContiguousTipMetadata { .. }) => {
            "backfill_requires_contiguous_tip_metadata"
        }
        Some(IngestError::BackfillCheckpointMisaligned { .. }) => "backfill_checkpoint_misaligned",
        Some(IngestError::TipFollowObservedTipBehindStore { .. }) => {
            "tip_follow_observed_tip_behind_store"
        }
        Some(IngestError::TipFollowCommonAncestorMissing { .. }) => {
            "tip_follow_common_ancestor_missing"
        }
        Some(IngestError::TipFollowParentMetadataUnavailable { .. }) => {
            "tip_follow_parent_metadata_unavailable"
        }
        Some(IngestError::ReorgWindowExceeded { .. }) => "reorg_window_exceeded",
        Some(IngestError::SourceRetryBudgetExceeded { .. }) => "source_retry_budget_exceeded",
        Some(IngestError::SourceRetryDeadlineExceeded { .. }) => "source_retry_deadline_exceeded",
        Some(IngestError::SystemTimeBeforeUnixEpoch { .. }) => "system_time_before_unix_epoch",
        Some(IngestError::TimestampTooLarge) => "timestamp_too_large",
        Some(IngestError::Source(_)) => "source",
        Some(IngestError::ArtifactDerive(_)) => "artifact_derive",
        Some(IngestError::Store(_)) => "store",
    }
}

fn usize_to_u32_saturating(amount: usize) -> u32 {
    u32::try_from(amount).map_or(u32::MAX, |converted| converted)
}

/// Emits a structured tracing event for a successful chain-epoch commit.
///
/// Operators consume two event names from this surface, matching the
/// `ChainEvent` vocabulary used in `docs/architecture/chain-events.md`:
///
/// * `chain_committed` for pure appends, finalization advances, and any other
///   transition that does not invalidate previously visible blocks.
/// * `chain_reorged` for transitions that replace a previously visible
///   non-finalized range. Emitted at `WARN` because reorgs warrant operator
///   attention even when the configured window absorbs them.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive ChainEvent must degrade to a typed warning if a new variant ships before this code is updated"
)]
pub(crate) fn record_commit_outcome(commit_outcome: &ChainEpochCommitOutcome) {
    let chain_epoch = commit_outcome.chain_epoch;
    let event_sequence = commit_outcome.event_envelope.event_sequence;
    record_writer_progress(chain_epoch);

    match &commit_outcome.event {
        ChainEvent::ChainCommitted { committed } => {
            tracing::info!(
                target: "zinder::ingest",
                event = "chain_committed",
                chain_epoch_id = chain_epoch.id.value(),
                network = chain_epoch.network.name(),
                tip_height = chain_epoch.tip_height.value(),
                tip_hash = %display_block_hash(chain_epoch.tip_hash),
                finalized_height = chain_epoch.finalized_height.value(),
                block_range_start = committed.block_range.start.value(),
                block_range_end = committed.block_range.end.value(),
                event_sequence,
                "chain committed"
            );
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "chain_reorged",
                chain_epoch_id = chain_epoch.id.value(),
                network = chain_epoch.network.name(),
                tip_height = chain_epoch.tip_height.value(),
                tip_hash = %display_block_hash(chain_epoch.tip_hash),
                finalized_height = chain_epoch.finalized_height.value(),
                committed_block_range_start = committed.block_range.start.value(),
                committed_block_range_end = committed.block_range.end.value(),
                reverted_block_range_start = reverted.block_range.start.value(),
                reverted_block_range_end = reverted.block_range.end.value(),
                event_sequence,
                "chain reorged"
            );
        }
        _ => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "chain_event_unrecognized",
                chain_epoch_id = chain_epoch.id.value(),
                event_sequence,
                "unrecognized chain event variant"
            );
        }
    }
}

fn record_writer_progress(chain_epoch: ChainEpoch) {
    metrics::gauge!(
        "zinder_ingest_writer_chain_epoch_id",
        "network" => chain_epoch.network.name()
    )
    .set(u64_to_f64(chain_epoch.id.value()));
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => chain_epoch.network.name()
    )
    .set(u32_to_f64(chain_epoch.tip_height.value()));
    metrics::gauge!(
        "zinder_ingest_writer_finalized_height",
        "network" => chain_epoch.network.name()
    )
    .set(u32_to_f64(chain_epoch.finalized_height.value()));
}

fn display_block_hash(block_hash: BlockHash) -> String {
    let mut bytes = block_hash.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain progress values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; block heights are diagnostic"
)]
fn u32_to_f64(sample: u32) -> f64 {
    f64::from(sample)
}

/// Returns the next [`ChainEpochId`] for a fresh commit on `store`.
pub(crate) fn next_chain_epoch_id(store: &PrimaryChainStore) -> Result<ChainEpochId, IngestError> {
    next_chain_epoch_id_from(store.current_chain_epoch()?.as_ref())
}

/// Returns the next [`ChainEpochId`] given an already-resolved current chain
/// epoch, avoiding a second `current_chain_epoch` read when the caller already
/// holds it.
pub(crate) fn next_chain_epoch_id_from(
    current_chain_epoch: Option<&ChainEpoch>,
) -> Result<ChainEpochId, IngestError> {
    current_chain_epoch.map_or(Ok(ChainEpochId::new(1)), |chain_epoch| {
        next_chain_epoch_id_after(chain_epoch.id)
    })
}

/// Returns the [`ChainEpochId`] that follows `chain_epoch_id`.
pub(crate) fn next_chain_epoch_id_after(
    chain_epoch_id: ChainEpochId,
) -> Result<ChainEpochId, IngestError> {
    chain_epoch_id
        .value()
        .checked_add(1)
        .map(ChainEpochId::new)
        .ok_or(StoreError::ChainEpochSequenceOverflow)
        .map_err(IngestError::from)
}

/// Returns the current Unix wall-clock time in milliseconds for stamping
/// `chain_epoch.created_at`.
pub(crate) fn current_unix_millis() -> Result<UnixTimestampMillis, IngestError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| IngestError::SystemTimeBeforeUnixEpoch { source })?;
    let elapsed_millis =
        u64::try_from(elapsed.as_millis()).map_err(|_| IngestError::TimestampTooLarge)?;

    Ok(UnixTimestampMillis::new(elapsed_millis))
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}
