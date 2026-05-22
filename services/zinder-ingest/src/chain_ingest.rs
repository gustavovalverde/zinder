//! Shared per-block chain-ingest engine.
//!
//! Both backfill and tip-following ingest run through this module: it owns
//! retryable node fetches, artifact-batch state, subtree-root population,
//! and the `commit_chain_epoch` translation. Callers decide which
//! [`ReorgWindowChange`] their commit represents and construct the durable
//! [`ChainEpoch`] that the engine writes.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use zebra_chain::block::Block as ZebraBlock;
use zinder_core::wire::{encode_display_block_hash_hex, encode_zinder_native_chain_name};
use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootIndex, TransactionArtifact, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentAddressUtxoArtifact, TransparentOutPoint,
    TransparentPrevoutArtifact, TransparentUtxoSpendArtifact, TreeStateArtifact,
    UnixTimestampMillis,
};
use zinder_source::{NodeSource, SourceBlock, SourceError, SourceFailureClass, SourceSubtreeRoots};
use zinder_store::{
    ChainEpochArtifacts, ChainEpochCommitOutcome, ChainEpochReader, ChainEvent, PrimaryChainStore,
    ReorgWindowChange, StoreError,
};

use crate::{
    ArtifactDeriveError,
    artifact_builder::TransparentAddressTxIndexSpendCandidate,
    transparent_prevout_lookup::{
        TransparentPrevoutLookupMode, TransparentPrevoutLookupStage,
        read_chunked_transparent_prevouts_by_outpoints,
    },
};

const FETCH_RETRY_MAX_ATTEMPTS: u32 = 5;
#[cfg(not(test))]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(test)]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(1);
const FETCH_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
const FETCH_RETRY_FAILURE_BUDGET: u32 = 100;
const COMMIT_STAGE_RESOLVE_SPEND_ADDRESSES: &str = "resolve_spend_addresses";
const COMMIT_STAGE_BUILD_DERIVE_CONTEXTS: &str = "build_derive_contexts";
const COMMIT_STAGE_STORE_COMMIT: &str = "store_commit";
const COMMIT_STAGE_DISPATCH_DERIVE: &str = "dispatch_derive";

/// Supported node source kinds for ingestion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeSourceKind {
    /// Zebra JSON-RPC source.
    ZebraJsonRpc,
}

/// Error returned by ingestion operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IngestError {
    /// Requested node source name is not supported.
    #[error("unknown node source: {node_source}")]
    UnknownNodeSource {
        /// User-provided node source name.
        node_source: String,
    },

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

    /// A stored transparent prevout transaction did not contain the output
    /// referenced by the spending transaction.
    #[error(
        "transparent prevout {transaction_id:?}:{output_index} is missing from the resolved transaction"
    )]
    TransparentPrevoutOutputMissing {
        /// Transaction id of the resolved prevout transaction.
        transaction_id: TransactionId,
        /// Output index referenced by the spending transaction input.
        output_index: u32,
    },

    /// Shielded protocol is not supported by the current ingest subtree-root tracker.
    #[error("{protocol:?} subtree roots are not supported by this ingest tracker")]
    UnsupportedShieldedProtocol {
        /// Unsupported shielded protocol.
        protocol: ShieldedProtocol,
    },

    /// Derive-plane dispatch (consumer apply or store write) failed.
    #[error("derive dispatch failed: {0}")]
    DeriveDispatch(String),

    /// Derive-store open or operation failed.
    #[error(transparent)]
    DeriveStore(#[from] zinder_derive::DeriveStoreError),

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

    /// A `spawn_blocking` task hosting a `RocksDB` call failed to join
    /// (panic or runtime shutdown).
    #[error("blocking storage task failed to join: {reason}")]
    BlockingTaskFailed {
        /// Reason from `tokio::task::JoinError::to_string`.
        reason: String,
    },
}

/// Finalized canonical artifacts produced for one source block.
///
/// Output of [`finalize_derived_block`](crate::artifact_builder::finalize_derived_block).
/// Each field is final once this struct exists; the consumer absorbs it
/// into an `IngestBatch` and the batch is then committed atomically.
#[derive(Debug)]
pub struct BuiltArtifacts {
    /// Durable full-block artifact.
    pub block: BlockArtifact,
    /// Parsed block from the parallel derive phase, when available.
    pub parsed_block: Option<Arc<ZebraBlock>>,
    /// Lightwalletd compact block with final `chain_metadata` stamped.
    pub compact_block: CompactBlockArtifact,
    /// Per-transaction durable artifacts in block order.
    pub transactions: Vec<TransactionArtifact>,
    /// Transparent-output index entries.
    pub transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    /// Transparent prevout artifacts keyed by outpoint.
    pub transparent_prevouts: Vec<TransparentPrevoutArtifact>,
    /// Transparent-input spend records.
    pub transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    /// Transparent-address transaction-index entries (output side).
    pub transparent_address_tx_index: Vec<zinder_core::TransparentAddressTxIndexArtifact>,
    /// Spend-input candidates awaiting prevout resolution at commit time.
    pub transparent_address_tx_index_spend_candidates: Vec<TransparentAddressTxIndexSpendCandidate>,
    /// Running commitment-tree position after this block is folded in.
    pub tip_metadata: ChainTipMetadata,
    /// Tree-state payload archived for canonical recovery, if any.
    pub tree_state: Option<zinder_core::TreeStateArtifact>,
}

/// In-flight artifact batch accumulated between commits.
#[derive(Default)]
pub(crate) struct IngestBatch {
    pub(crate) finalized_blocks: Vec<BlockArtifact>,
    pub(crate) parsed_blocks: Vec<Option<Arc<ZebraBlock>>>,
    pub(crate) compact_blocks: Vec<CompactBlockArtifact>,
    pub(crate) transactions: Vec<TransactionArtifact>,
    pub(crate) tree_states: Vec<TreeStateArtifact>,
    pub(crate) subtree_roots: Vec<SubtreeRootArtifact>,
    pub(crate) transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    pub(crate) transparent_prevouts: Vec<TransparentPrevoutArtifact>,
    pub(crate) transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
    transparent_prevout_output_outpoints: HashSet<TransparentOutPoint>,
    transparent_prevout_spend_outpoints: HashSet<TransparentOutPoint>,
    pub(crate) transparent_address_tx_index: Vec<zinder_core::TransparentAddressTxIndexArtifact>,
    pub(crate) transparent_address_tx_index_spend_candidates:
        Vec<TransparentAddressTxIndexSpendCandidate>,
    pub(crate) tip_metadata: Option<ChainTipMetadata>,
}

impl IngestBatch {
    /// Appends one block's finalized artifacts into the in-flight batch.
    ///
    /// Called once per `finalize_derived_block` result. Each field is
    /// moved into its matching `IngestBatch` vector; the running tip
    /// metadata is overwritten with the latest finalized value.
    pub(crate) fn absorb(&mut self, built: BuiltArtifacts) {
        if let Some(tree_state) = built.tree_state {
            self.tree_states.push(tree_state);
        }
        for prevout in &built.transparent_prevouts {
            self.transparent_prevout_output_outpoints
                .insert(prevout.outpoint);
        }
        for spend in &built.transparent_utxo_spends {
            self.transparent_prevout_spend_outpoints
                .insert(spend.spent_outpoint);
        }
        self.finalized_blocks.push(built.block);
        self.parsed_blocks.push(built.parsed_block);
        self.compact_blocks.push(built.compact_block);
        self.transactions.extend(built.transactions);
        self.transparent_address_utxos
            .extend(built.transparent_address_utxos);
        self.transparent_prevouts.extend(built.transparent_prevouts);
        self.transparent_utxo_spends
            .extend(built.transparent_utxo_spends);
        self.transparent_address_tx_index
            .extend(built.transparent_address_tx_index);
        self.transparent_address_tx_index_spend_candidates
            .extend(built.transparent_address_tx_index_spend_candidates);
        self.tip_metadata = Some(built.tip_metadata);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.finalized_blocks.is_empty()
    }

    pub(crate) fn work_cost(&self) -> IngestBatchWorkCost {
        let transparent_prevout_store_lookup_count = self
            .transparent_prevout_spend_outpoints
            .difference(&self.transparent_prevout_output_outpoints)
            .count();
        IngestBatchWorkCost {
            block_count: self.finalized_blocks.len(),
            transparent_prevout_store_lookup_count,
        }
    }

    fn clear_work_cost(&mut self) {
        self.transparent_prevout_output_outpoints.clear();
        self.transparent_prevout_spend_outpoints.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IngestBatchWorkCost {
    pub(crate) block_count: usize,
    pub(crate) transparent_prevout_store_lookup_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IngestBatchBudget {
    max_blocks: NonZeroU32,
    max_transparent_prevout_store_lookups: NonZeroU32,
}

impl IngestBatchBudget {
    pub(crate) const fn new(
        max_blocks: NonZeroU32,
        max_transparent_prevout_store_lookups: NonZeroU32,
    ) -> Self {
        Self {
            max_blocks,
            max_transparent_prevout_store_lookups,
        }
    }

    pub(crate) fn commit_trigger(
        self,
        cost: IngestBatchWorkCost,
    ) -> Option<IngestBatchCommitTrigger> {
        if cost.block_count >= nonzero_u32_to_usize(self.max_blocks) {
            return Some(IngestBatchCommitTrigger::BlockCount);
        }
        if cost.transparent_prevout_store_lookup_count
            >= nonzero_u32_to_usize(self.max_transparent_prevout_store_lookups)
        {
            return Some(IngestBatchCommitTrigger::TransparentPrevoutStoreLookupCount);
        }
        None
    }
}

fn nonzero_u32_to_usize(amount: NonZeroU32) -> usize {
    match usize::try_from(amount.get()) {
        Ok(converted) => converted,
        Err(_error) => usize::MAX,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IngestBatchCommitTrigger {
    BlockCount,
    TransparentPrevoutStoreLookupCount,
}

impl IngestBatchCommitTrigger {
    pub(crate) const fn metric_label(self) -> &'static str {
        match self {
            Self::BlockCount => "block_count",
            Self::TransparentPrevoutStoreLookupCount => "transparent_prevout_store_lookup_count",
        }
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
        || async { source.fetch_block_at(height).await },
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
            Err(error) if per_call_retry_permitted(&error) => {
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

/// Returns whether `retry_source_request` should sleep and re-issue the
/// same call.
///
/// Per-call retry handles the failure classes where retrying the *same
/// request* can plausibly succeed: transient transport
/// ([`SourceFailureClass::NodeUnreachable`]) and short-lived subscription
/// failures ([`SourceFailureClass::StreamDisconnected`]). View-stale
/// failures ([`SourceFailureClass::UpstreamViewChanged`]) and structural
/// failures are bubbled up to the loop, which re-observes the upstream
/// before issuing dependent requests.
fn per_call_retry_permitted(error: &SourceError) -> bool {
    matches!(
        error.upstream_classification(),
        SourceFailureClass::NodeUnreachable | SourceFailureClass::StreamDisconnected,
    )
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

    if current_chain_epoch
        .tip_height
        .next()
        .is_some_and(|next_tip| first_candidate.height == next_tip)
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
pub(crate) async fn commit_ingest_batch(
    store: &PrimaryChainStore,
    derive_store: &zinder_derive::DeriveStore,
    chain_epoch: ChainEpoch,
    batch: &mut IngestBatch,
    reorg_window_change: ReorgWindowChange,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let started_at = Instant::now();
    let batch_cost = batch.work_cost();
    if batch.is_empty() {
        let commit_outcome = Err(IngestError::EmptyIngestBatch);
        record_ingest_commit_outcome(started_at, batch_cost, &commit_outcome);
        return commit_outcome;
    }

    let derive_contexts = build_derive_contexts_for_commit(store, derive_store, batch)?;
    let resolved_prevouts = derive_contexts
        .as_ref()
        .map(|contexts| contexts.prevouts_by_outpoint.as_ref());

    let resolve_spend_addresses_started_at = Instant::now();
    let resolve_spend_addresses_outcome =
        append_transparent_spend_tx_index_artifacts(store, batch, resolved_prevouts);
    record_ingest_commit_stage_outcome(
        COMMIT_STAGE_RESOLVE_SPEND_ADDRESSES,
        resolve_spend_addresses_started_at,
        &resolve_spend_addresses_outcome,
    );
    resolve_spend_addresses_outcome?;

    batch.tip_metadata = None;

    let artifacts = drain_batch_into_chain_epoch_artifacts(chain_epoch, batch, reorg_window_change);
    record_ingest_batch_work_cost(batch.work_cost());

    let store_commit_started_at = Instant::now();
    let commit_result = commit_chain_epoch_blocking(store.clone(), artifacts).await;
    record_ingest_commit_stage_outcome(
        COMMIT_STAGE_STORE_COMMIT,
        store_commit_started_at,
        &commit_result,
    );

    match commit_result {
        Ok(commit_summary) => {
            record_commit_outcome(&commit_summary);
            if let Some(contexts) = derive_contexts.as_ref() {
                let dispatch_derive_started_at = Instant::now();
                let dispatch_derive_outcome =
                    dispatch_derive_for_committed(derive_store, &commit_summary, &contexts.blocks);
                record_ingest_commit_stage_outcome(
                    COMMIT_STAGE_DISPATCH_DERIVE,
                    dispatch_derive_started_at,
                    &dispatch_derive_outcome,
                );
                dispatch_derive_outcome?;
            }
            let commit_outcome = Ok(commit_summary);
            record_ingest_commit_outcome(started_at, batch_cost, &commit_outcome);
            commit_outcome
        }
        Err(error) => {
            let commit_outcome = Err(error);
            record_ingest_commit_outcome(started_at, batch_cost, &commit_outcome);
            commit_outcome
        }
    }
}

fn build_derive_contexts_for_commit(
    store: &PrimaryChainStore,
    derive_store: &zinder_derive::DeriveStore,
    batch: &IngestBatch,
) -> Result<Option<crate::derive_consumers::BatchBlockContexts>, IngestError> {
    let build_derive_contexts_started_at = Instant::now();
    let derive_contexts_outcome = if derive_store.has_consumer_column_families() {
        let build_contexts = || {
            crate::derive_consumers::build_block_contexts_from_batch(
                store,
                &batch.finalized_blocks,
                &batch.parsed_blocks,
                &batch.transparent_prevouts,
            )
        };
        if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(build_contexts).map(Some)
        } else {
            build_contexts().map(Some)
        }
    } else {
        Ok(None)
    };
    record_ingest_commit_stage_outcome(
        COMMIT_STAGE_BUILD_DERIVE_CONTEXTS,
        build_derive_contexts_started_at,
        &derive_contexts_outcome,
    );
    derive_contexts_outcome
}

fn drain_batch_into_chain_epoch_artifacts(
    chain_epoch: ChainEpoch,
    batch: &mut IngestBatch,
    reorg_window_change: ReorgWindowChange,
) -> ChainEpochArtifacts {
    let mut artifacts = ChainEpochArtifacts::new(
        chain_epoch,
        std::mem::take(&mut batch.finalized_blocks),
        std::mem::take(&mut batch.compact_blocks),
    );
    batch.parsed_blocks.clear();

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
    if !batch.transparent_prevouts.is_empty() {
        artifacts =
            artifacts.with_transparent_prevouts(std::mem::take(&mut batch.transparent_prevouts));
    }
    if !batch.transparent_utxo_spends.is_empty() {
        artifacts = artifacts
            .with_transparent_utxo_spends(std::mem::take(&mut batch.transparent_utxo_spends));
    }
    if !batch.transparent_address_tx_index.is_empty() {
        artifacts = artifacts.with_transparent_address_tx_index(std::mem::take(
            &mut batch.transparent_address_tx_index,
        ));
    }
    batch.clear_work_cost();

    artifacts.with_reorg_window_change(reorg_window_change)
}

async fn commit_chain_epoch_blocking(
    store: PrimaryChainStore,
    artifacts: ChainEpochArtifacts,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    tokio::task::spawn_blocking(move || {
        store
            .commit_chain_epoch(artifacts)
            .map_err(IngestError::from)
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
}

fn dispatch_derive_for_committed(
    derive_store: &zinder_derive::DeriveStore,
    commit_summary: &zinder_store::ChainEpochCommitOutcome,
    contexts: &HashMap<BlockHeight, std::sync::Arc<zinder_derive::BlockCommitContext>>,
) -> Result<(), IngestError> {
    let inputs = zinder_derive::ChainEventDispatchInputs {
        chain_epoch: commit_summary.chain_epoch,
        chain_event: &commit_summary.event,
        chain_cursor: commit_summary.event_envelope.cursor.as_bytes(),
        event_sequence: commit_summary.event_envelope.event_sequence,
        finalized_height: commit_summary.event_envelope.finalized_height,
    };
    crate::derive_consumers::dispatch_chain_event(derive_store, inputs, contexts)
}

fn append_transparent_spend_tx_index_artifacts(
    store: &PrimaryChainStore,
    batch: &mut IngestBatch,
    resolved_prevouts: Option<&HashMap<TransparentOutPoint, TransparentPrevoutArtifact>>,
) -> Result<(), IngestError> {
    if batch
        .transparent_address_tx_index_spend_candidates
        .is_empty()
    {
        return Ok(());
    }

    // Build an in-batch outpoint -> address map so spends within the batch
    // resolve without touching the store when derive consumers are disabled.
    let in_batch_prevout_addresses: HashMap<TransparentOutPoint, TransparentAddressScriptHash> =
        batch
            .transparent_prevouts
            .iter()
            .map(|artifact| (artifact.outpoint, artifact.address_script_hash))
            .collect();

    let current_chain_reader = if store.current_chain_epoch()?.is_some() {
        Some(store.current_chain_epoch_reader()?)
    } else {
        None
    };
    let spend_candidates = std::mem::take(&mut batch.transparent_address_tx_index_spend_candidates);
    let store_prevout_addresses = if let Some(reader) = current_chain_reader.as_ref() {
        resolve_spend_candidate_address_hashes_from_store(
            reader,
            &spend_candidates,
            &in_batch_prevout_addresses,
            resolved_prevouts,
        )?
    } else {
        HashMap::new()
    };
    let mut emitted = batch
        .transparent_address_tx_index
        .iter()
        .map(|artifact| {
            (
                artifact.address_script_hash,
                artifact.block_height,
                artifact.tx_index_in_block,
            )
        })
        .collect::<HashSet<_>>();

    for candidate in spend_candidates {
        let address_script_hash = if let Some(address_script_hash) = resolved_prevouts
            .and_then(|prevouts| prevouts.get(&candidate.spent_outpoint))
            .map(|prevout| prevout.address_script_hash)
        {
            address_script_hash
        } else if let Some(address_script_hash) = in_batch_prevout_addresses
            .get(&candidate.spent_outpoint)
            .copied()
        {
            address_script_hash
        } else {
            match store_prevout_addresses
                .get(&candidate.spent_outpoint)
                .copied()
            {
                Some(address_script_hash) => address_script_hash,
                None => continue,
            }
        };
        if emitted.insert((
            address_script_hash,
            candidate.block_height,
            candidate.tx_index_in_block,
        )) {
            batch
                .transparent_address_tx_index
                .push(TransparentAddressTxIndexArtifact::new(
                    address_script_hash,
                    candidate.block_height,
                    candidate.tx_index_in_block,
                    candidate.transaction_id,
                    candidate.block_hash,
                ));
        }
    }

    Ok(())
}

fn resolve_spend_candidate_address_hashes_from_store(
    reader: &ChainEpochReader<'_>,
    spend_candidates: &[TransparentAddressTxIndexSpendCandidate],
    in_batch_prevout_addresses: &HashMap<TransparentOutPoint, TransparentAddressScriptHash>,
    resolved_prevouts: Option<&HashMap<TransparentOutPoint, TransparentPrevoutArtifact>>,
) -> Result<HashMap<TransparentOutPoint, TransparentAddressScriptHash>, IngestError> {
    let mut unresolved_outpoints = Vec::new();
    let mut seen = HashSet::new();
    for candidate in spend_candidates {
        let outpoint = candidate.spent_outpoint;
        if in_batch_prevout_addresses.contains_key(&outpoint)
            || resolved_prevouts.is_some_and(|prevouts| prevouts.contains_key(&outpoint))
            || !seen.insert(outpoint)
        {
            continue;
        }
        unresolved_outpoints.push(outpoint);
    }

    let prevouts = read_chunked_transparent_prevouts_by_outpoints(
        reader,
        TransparentPrevoutLookupMode::WriterCommit,
        TransparentPrevoutLookupStage::SpendAddressIndex,
        &unresolved_outpoints,
    )?;
    Ok(prevouts
        .into_iter()
        .map(|(outpoint, prevout)| (outpoint, prevout.address_script_hash))
        .collect())
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

/// Records derive wall-clock and outcome for one source block.
///
/// In bulk catchup this wraps the parallel-safe `derive_block` call inside
/// the buffered stream; in tip-follow this wraps the one-block artifact
/// build. The histogram is the per-block CPU contribution to ingest
/// throughput before serial finalization and commit work.
pub(crate) fn record_ingest_derive_outcome<T>(
    started_at: Instant,
    derive_outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_derive_duration_seconds",
        "status" => outcome_status(derive_outcome),
        "error_class" => ingest_error_class(derive_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_derive_total",
        "status" => outcome_status(derive_outcome),
        "error_class" => ingest_error_class(derive_outcome.as_ref().err())
    )
    .increment(1);
}

fn record_ingest_commit_stage_outcome<T>(
    stage: &'static str,
    started_at: Instant,
    stage_outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_commit_stage_duration_seconds",
        "stage" => stage,
        "status" => outcome_status(stage_outcome),
        "error_class" => ingest_error_class(stage_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
}

/// Publishes the current work cost of an in-flight ingest batch.
///
/// Called after every successful absorb and after every commit so the
/// gauges track the writer-visible queue. The block-count gauge climbs
/// from `0` to `commit_batch_blocks`; the transparent-prevout gauge
/// surfaces the store-lookup budget that can end a batch earlier.
pub(crate) fn record_ingest_batch_work_cost(cost: IngestBatchWorkCost) {
    metrics::gauge!("zinder_ingest_batch_accumulator_blocks")
        .set(f64::from(usize_to_u32_saturating(cost.block_count)));
    metrics::gauge!("zinder_ingest_batch_transparent_prevout_store_lookup_outpoints").set(
        f64::from(usize_to_u32_saturating(
            cost.transparent_prevout_store_lookup_count,
        )),
    );
}

pub(crate) fn record_ingest_batch_commit_trigger(trigger: IngestBatchCommitTrigger) {
    metrics::counter!(
        "zinder_ingest_batch_commit_trigger_total",
        "trigger" => trigger.metric_label()
    )
    .increment(1);
}

fn record_ingest_commit_outcome(
    started_at: Instant,
    batch_cost: IngestBatchWorkCost,
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
    .record(usize_to_u32_saturating(batch_cost.block_count));
    metrics::histogram!(
        "zinder_ingest_commit_batch_transparent_prevout_store_lookup_count",
        "status" => outcome_status(commit_outcome)
    )
    .record(usize_to_u32_saturating(
        batch_cost.transparent_prevout_store_lookup_count,
    ));
}

pub(crate) const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

pub(crate) fn ingest_error_class(error: Option<&IngestError>) -> &'static str {
    match error {
        None => "none",
        Some(IngestError::UnknownNodeSource { .. }) => "unknown_node_source",
        Some(IngestError::SubtreeRootsUnavailable { .. }) => "subtree_roots_unavailable",
        Some(IngestError::SubtreeRootCompletingBlockMissing { .. }) => {
            "subtree_root_completing_block_missing"
        }
        Some(IngestError::TransparentPrevoutOutputMissing { .. }) => {
            "transparent_prevout_output_missing"
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
        Some(IngestError::BlockingTaskFailed { .. }) => "blocking_task_failed",
        Some(IngestError::DeriveDispatch(_)) => "derive_dispatch",
        Some(IngestError::DeriveStore(_)) => "derive_store",
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
                network = encode_zinder_native_chain_name(chain_epoch.network),
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
                network = encode_zinder_native_chain_name(chain_epoch.network),
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
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u64_to_f64(chain_epoch.id.value()));
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u32_to_f64(chain_epoch.tip_height.value()));
    metrics::gauge!(
        "zinder_ingest_writer_finalized_height",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u32_to_f64(chain_epoch.finalized_height.value()));
}

fn display_block_hash(block_hash: BlockHash) -> String {
    encode_display_block_hash_hex(block_hash)
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

#[cfg(test)]
mod tests {
    use std::{error::Error, num::NonZeroU32};

    use prost::Message;
    use serde_json::Value;
    use tempfile::tempdir;
    use zinder_core::{Network, TransactionId, TransparentAddressScriptHash};
    use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
    use zinder_source::{SourceBlock, decode_display_block_hash};
    use zinder_store::{CURRENT_ARTIFACT_SCHEMA_VERSION, TransparentAddressTxIndexPageRequest};
    use zinder_testkit::StoreFixture;

    use super::*;
    use crate::artifact_builder::{
        CommitmentTreeSizes, derive_block as derive_block_for_test, finalize_derived_block,
    };

    /// Test convenience: run the production two-stage pipeline on
    /// `source_block` against a zeroed running tree-size offset.
    fn derive_for_test(
        source_block: &zinder_source::SourceBlock,
    ) -> Result<BuiltArtifacts, ArtifactDeriveError> {
        let derived = derive_block_for_test(source_block)?;
        let mut counters = CommitmentTreeSizes::default();
        finalize_derived_block(derived, &mut counters)
    }

    #[test]
    fn ingest_batch_work_cost_counts_deduped_store_prevout_lookups() {
        let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x44; 32]), 0);
        let mut batch = IngestBatch::default();

        batch.absorb(test_built_artifacts(
            1,
            &[],
            &[spent_outpoint, spent_outpoint],
        ));

        assert_eq!(
            batch.work_cost(),
            IngestBatchWorkCost {
                block_count: 1,
                transparent_prevout_store_lookup_count: 1
            }
        );
    }

    #[test]
    fn ingest_batch_work_cost_excludes_in_batch_prevouts() {
        let in_batch_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x55; 32]), 0);
        let store_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x66; 32]), 0);
        let mut batch = IngestBatch::default();

        batch.absorb(test_built_artifacts(
            1,
            &[in_batch_outpoint],
            &[in_batch_outpoint, store_outpoint],
        ));

        assert_eq!(
            batch.work_cost(),
            IngestBatchWorkCost {
                block_count: 1,
                transparent_prevout_store_lookup_count: 1
            }
        );
    }

    #[test]
    fn ingest_batch_budget_triggers_on_prevout_store_lookup_limit() {
        let budget = IngestBatchBudget::new(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU32::MIN.saturating_add(1),
        );

        assert_eq!(
            budget.commit_trigger(IngestBatchWorkCost {
                block_count: 3,
                transparent_prevout_store_lookup_count: 2,
            }),
            Some(IngestBatchCommitTrigger::TransparentPrevoutStoreLookupCount)
        );
    }

    #[tokio::test]
    async fn commit_ingest_batch_indexes_transparent_spend_only_history()
    -> Result<(), Box<dyn Error>> {
        let (store_fixture, prevout_block, prevout_transaction_id, address_script_hash) =
            committed_prevout_fixture()?;
        let store = store_fixture.chain_store().clone();
        let spending_transaction_id = TransactionId::from_bytes([0x77; 32]);
        let spending_block_height = BlockHeight::new(2);
        let spending_block_hash = test_block_hash(2);
        let mut batch = transparent_spend_batch(
            prevout_block.hash,
            prevout_transaction_id,
            spending_transaction_id,
            spending_block_height,
            spending_block_hash,
        );
        let spending_chain_epoch = test_chain_epoch(
            2,
            prevout_block.network,
            spending_block_height,
            spending_block_hash,
        );
        let derive_dir = tempdir()?;
        let derive_store = zinder_derive::DeriveStore::open(
            derive_dir.path(),
            zinder_derive::DeriveStoreOptions {
                sync_writes: false,
                consumer_column_families: &[],
                tuning: zinder_store::StorageTuning::for_local_tests(),
            },
        )?;

        commit_ingest_batch(
            &store,
            &derive_store,
            spending_chain_epoch,
            &mut batch,
            ReorgWindowChange::FinalizeThrough {
                height: spending_block_height,
            },
        )
        .await?;

        let page =
            store.transparent_address_tx_index_page(TransparentAddressTxIndexPageRequest {
                at_epoch: None,
                address_script_hash,
                start_height: BlockHeight::new(0),
                end_height: BlockHeight::new(10),
                max_entries: NonZeroU32::new(10).ok_or("page size must be nonzero")?,
                descending: false,
                from_cursor: None,
            })?;

        let artifact = page
            .artifacts
            .first()
            .ok_or("transparent spend history artifact should be indexed")?;
        assert_eq!(page.artifacts.len(), 1);
        assert_eq!(artifact.block_height, spending_block_height);
        assert_eq!(artifact.block_hash, spending_block_hash);
        assert_eq!(artifact.tx_index_in_block, 7);
        assert_eq!(artifact.transaction_id, spending_transaction_id);

        Ok(())
    }

    fn test_built_artifacts(
        height: u32,
        in_batch_prevouts: &[TransparentOutPoint],
        spent_outpoints: &[TransparentOutPoint],
    ) -> BuiltArtifacts {
        let block_height = BlockHeight::new(height);
        let block_hash = test_block_hash(u8::try_from(height).unwrap_or(u8::MAX));
        let transparent_prevouts = in_batch_prevouts
            .iter()
            .copied()
            .map(|outpoint| {
                TransparentPrevoutArtifact::new(
                    outpoint,
                    1,
                    vec![0x51],
                    TransparentAddressScriptHash::of_script_pub_key(&[0x51]),
                    block_height,
                    block_hash,
                )
            })
            .collect();
        let transparent_utxo_spends = spent_outpoints
            .iter()
            .copied()
            .map(|outpoint| TransparentUtxoSpendArtifact::new(outpoint, block_height, block_hash))
            .collect();
        BuiltArtifacts {
            block: BlockArtifact::new(block_height, block_hash, test_block_hash(0), Vec::new()),
            parsed_block: None,
            compact_block: CompactBlockArtifact::new(block_height, block_hash, Vec::new()),
            transactions: Vec::new(),
            transparent_address_utxos: Vec::new(),
            transparent_prevouts,
            transparent_utxo_spends,
            transparent_address_tx_index: Vec::new(),
            transparent_address_tx_index_spend_candidates: Vec::new(),
            tip_metadata: ChainTipMetadata::empty(),
            tree_state: None,
        }
    }

    fn committed_prevout_fixture() -> Result<
        (
            StoreFixture,
            SourceBlock,
            TransactionId,
            TransparentAddressScriptHash,
        ),
        Box<dyn Error>,
    > {
        let prevout_block = fixture_source_block()?;
        let prevout_built = derive_for_test(&prevout_block)?;
        let prevout_transactions = prevout_built.transactions.clone();
        let prevout_transaction_id = prevout_transactions
            .first()
            .ok_or("fixture block must contain a transaction")?
            .transaction_id;
        let script_pub_key = first_transparent_output_script_pub_key(&prevout_block)?;
        let address_script_hash =
            TransparentAddressScriptHash::of_script_pub_key(script_pub_key.as_slice());

        let store_fixture = StoreFixture::open()?;
        let store = store_fixture.chain_store().clone();
        let prevout_chain_epoch = test_chain_epoch(
            1,
            prevout_block.network,
            prevout_block.height,
            prevout_block.hash,
        );
        store.commit_chain_epoch(
            ChainEpochArtifacts::new(
                prevout_chain_epoch,
                vec![prevout_built.block.clone()],
                vec![prevout_built.compact_block],
            )
            .with_transactions(prevout_transactions)
            .with_transparent_address_utxos(prevout_built.transparent_address_utxos)
            .with_transparent_prevouts(prevout_built.transparent_prevouts),
        )?;

        Ok((
            store_fixture,
            prevout_block,
            prevout_transaction_id,
            address_script_hash,
        ))
    }

    fn transparent_spend_batch(
        parent_hash: BlockHash,
        prevout_transaction_id: TransactionId,
        spending_transaction_id: TransactionId,
        spending_block_height: BlockHeight,
        spending_block_hash: BlockHash,
    ) -> IngestBatch {
        IngestBatch {
            finalized_blocks: vec![BlockArtifact::new(
                spending_block_height,
                spending_block_hash,
                parent_hash,
                b"spending-block".to_vec(),
            )],
            compact_blocks: vec![CompactBlockArtifact::new(
                spending_block_height,
                spending_block_hash,
                b"spending-compact-block".to_vec(),
            )],
            transparent_address_tx_index_spend_candidates: vec![
                TransparentAddressTxIndexSpendCandidate {
                    spent_outpoint: TransparentOutPoint::new(prevout_transaction_id, 0),
                    block_height: spending_block_height,
                    block_hash: spending_block_hash,
                    tx_index_in_block: 7,
                    transaction_id: spending_transaction_id,
                },
            ],
            tip_metadata: Some(ChainTipMetadata::empty()),
            ..IngestBatch::default()
        }
    }

    fn fixture_source_block() -> Result<SourceBlock, Box<dyn Error>> {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/z3-regtest-block-1.json"))?;
        let raw_block_hex = string_field(&fixture, "raw_block_hex")?;
        let raw_block_bytes = hex::decode(raw_block_hex)?;
        let height = u32_field(&fixture, "height")?;
        let source_block = SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(height),
            raw_block_bytes,
        )?;

        assert_eq!(
            source_block.hash,
            decode_display_block_hash(string_field(&fixture, "hash")?)?
        );
        assert_eq!(
            source_block.parent_hash,
            decode_display_block_hash(string_field(&fixture, "previousblockhash")?)?
        );
        assert_eq!(
            source_block.block_time_seconds,
            u32_field(&fixture, "time")?
        );

        Ok(source_block)
    }

    fn first_transparent_output_script_pub_key(
        source_block: &SourceBlock,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let compact_block_artifact = derive_for_test(source_block)?.compact_block;
        let compact_block =
            LightwalletdCompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
        let transaction = compact_block
            .vtx
            .first()
            .ok_or("fixture compact block must contain a transaction")?;
        let output = transaction
            .vout
            .first()
            .ok_or("fixture compact transaction must contain a transparent output")?;

        Ok(output.script_pub_key.clone())
    }

    fn test_chain_epoch(
        id: u64,
        network: Network,
        tip_height: BlockHeight,
        tip_hash: BlockHash,
    ) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network,
            tip_height,
            tip_hash,
            finalized_height: tip_height,
            finalized_hash: tip_hash,
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_669_000_000 + id),
        }
    }

    fn test_block_hash(seed: u8) -> BlockHash {
        BlockHash::from_bytes([seed; 32])
    }

    fn string_field<'value>(value: &'value Value, field: &str) -> Result<&'value str, String> {
        value
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{field} must be a string"))
    }

    fn u32_field(value: &Value, field: &str) -> Result<u32, String> {
        let raw = value
            .get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{field} must be an unsigned integer"))?;
        u32::try_from(raw).map_err(|error| format!("{field} exceeds u32: {error}"))
    }
}
