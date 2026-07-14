//! Shared per-block chain-ingest engine.
//!
//! Both bulk catchup and tip-following ingest run through this module: it owns
//! retryable node fetches, artifact-batch state, subtree-root population,
//! and the `commit_chain_epoch` translation. Callers decide which
//! [`ReorgWindowChange`] their commit represents and construct the durable
//! [`ChainEpoch`] that the engine writes.

use std::{
    collections::HashMap,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use zinder_core::wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name};
use zinder_core::{
    BlockBlobArtifact, BlockFinalNoteCommitmentRoots, BlockHash, BlockHeaderArtifact, BlockHeight,
    BlockHeightRange, BlockTransactionIndexArtifact, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootIndex,
    TransactionBlobArtifact, TransactionFactsArtifact, TransactionId,
    TransactionIntrinsicValueBalancesArtifact, TransactionLocation, TransparentOutPoint,
    TransparentOutputArtifact, TransparentSpendFact, TreeStateArtifact, UnixTimestampMillis,
};
use zinder_source::{
    NodeCapability, NodeSource, SourceBlock, SourceChainSegment, SourceChainSegmentLimits,
    SourceError, SourceFailureClass, SourceSubtreeRoots, SourceTreeState,
};
use zinder_store::{
    ChainEpochArtifacts, ChainEpochCommitOutcome, ChainEvent, PrimaryChainStore, ReorgWindowChange,
    StoreError, StoreReadCaller,
};

use crate::ArtifactDeriveError;

const FETCH_RETRY_MAX_ATTEMPTS: u32 = 5;
#[cfg(not(test))]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(test)]
const FETCH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(1);
const FETCH_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
const FETCH_RETRY_FAILURE_BUDGET: u32 = 100;
const COMMIT_STAGE_STORE_COMMIT: &str = "store_commit";
/// Default estimated canonical write bytes accumulated before a bulk-catchup batch closes.
pub const DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES: u64 = 536_870_912;
/// Default minimum batch size before estimated write bytes can close a bulk-catchup batch.
pub const DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE: u32 = 100;

/// Stride between consecutive tree-state checkpoints written by the ingest
/// pipeline.
///
/// Wallet implementations (notably `zcash_client_backend`) cap their rewind
/// window at roughly 100 blocks behind `fully_scanned_height`; if the
/// nearest checkpoint at-or-below that window is older than the cap, the
/// wallet cannot recover from a stall without a manual reset. An ingest
/// batch can span thousands of blocks (bulk catchup) or tens of blocks
/// (tip-follow during chain catchup), so a single end-of-batch checkpoint
/// leaves arbitrary gaps. Emitting one checkpoint every 100 heights in
/// both ingest phases guarantees the wallet can always find an anchor
/// inside its rewind window regardless of where in a batch it landed.
pub(crate) const TREE_STATE_CHECKPOINT_STRIDE: u32 = 100;
const ESTIMATED_BLOCK_WRITE_BYTES: usize = 512;
const ESTIMATED_BLOCK_TRANSACTION_INDEX_WRITE_BYTES: usize = 96;
const ESTIMATED_TRANSACTION_LOCATION_WRITE_BYTES: usize = 96;
const ESTIMATED_TRANSACTION_FACT_WRITE_BYTES: usize = 512;
const ESTIMATED_TRANSPARENT_OUTPUT_WRITE_BYTES: usize = 384;
const ESTIMATED_ADDRESS_OUTPUT_INDEX_WRITE_BYTES: usize = 256;
const ESTIMATED_TRANSPARENT_SPEND_FACT_WRITE_BYTES: usize = 512;

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

    /// Background source segment fetch task stopped before returning a result.
    #[error("source segment fetch task stopped unexpectedly: {reason}")]
    SourceSegmentFetchTaskStopped {
        /// Task join failure description.
        reason: String,
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

    /// A resolved previous transparent-output transaction did not contain the output
    /// referenced by the spending transaction.
    #[error(
        "previous transparent output {transaction_id:?}:{output_index} is missing from the resolved transaction"
    )]
    TransparentOutputOutputMissing {
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

    /// Backup requires the derive store created by the ingest writer.
    #[error(
        "derive store is missing at {path:?}; backup would be incomplete without the derive checkpoint"
    )]
    DeriveStoreMissing {
        /// Expected derive store path.
        path: PathBuf,
    },

    /// Canonical history exists without the projection store that previously
    /// consumed it, so a derive-only rebuild cannot be proven safe.
    #[error(
        "canonical history exists but its projection store is missing at {path:?}; configure a new empty canonical storage path and perform a full re-ingest because derive-only rebuilds are unsupported"
    )]
    ProjectionStoreMissingForCanonical {
        /// Expected projection store path.
        path: PathBuf,
    },

    /// Projection rows or cursors exist beside an empty canonical store, so
    /// the two stores cannot be proven to describe one chain history.
    #[error(
        "projection data exists at {path:?} without matching canonical history; configure a new empty canonical storage path and perform a full re-ingest because mixed storage pairs are unsupported"
    )]
    ProjectionStoreWithoutCanonicalHistory {
        /// Projection store whose data has no matching canonical epoch.
        path: PathBuf,
    },

    /// Canonical retention already deleted transparent spender facts that the
    /// durable projection cannot prove it covers or safely reconstruct.
    #[error(
        "retention-authoritative projection at {path:?} cannot cover canonical transparent deletions {required_from}..={deleted_through}: materialized through {materialized_through:?}, verified coverage {coverage_from:?}..={coverage_through:?}, destructive rebuild required: {destructive_rebuild}; configure a new empty canonical storage path and perform a full re-ingest"
    )]
    ProjectionRetentionCoverageInsufficient {
        /// Projection store whose coverage is incomplete.
        path: PathBuf,
        /// First canonical height the projection must cover.
        required_from: u32,
        /// Highest canonical height whose spender facts were deleted.
        deleted_through: u32,
        /// Highest materialized projection index height.
        materialized_through: Option<u32>,
        /// First height in verified contiguous projection coverage.
        coverage_from: Option<u32>,
        /// Last height in verified contiguous projection coverage.
        coverage_through: Option<u32>,
        /// Whether writer open would clear the existing projection.
        destructive_rebuild: bool,
    },

    /// Backup bundle staging path already exists, so the command cannot know
    /// whether replacing it would discard operator-owned data.
    #[error("backup checkpoint staging path already exists at {path:?}")]
    BackupCheckpointStagingExists {
        /// Existing staging path.
        path: PathBuf,
    },

    /// Backup destination already exists and is never replaced implicitly.
    #[error("backup checkpoint destination already exists at {path:?}")]
    BackupCheckpointDestinationExists {
        /// Existing destination path.
        path: PathBuf,
    },

    /// Backup could not atomically publish the staged canonical, projection,
    /// and manifest bundle.
    #[error("failed to install backup checkpoint from {from_path:?} to {to_path:?}: {source}")]
    BackupCheckpointInstall {
        /// Temporary bundle path.
        from_path: PathBuf,
        /// Final checkpoint bundle path.
        to_path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// Backup manifest could not be encoded.
    #[error("failed to encode backup manifest: {source}")]
    BackupManifestEncode {
        /// JSON encoding failure.
        #[source]
        source: serde_json::Error,
    },

    /// A backup or restore manifest could not be decoded into the supported
    /// structural contract.
    #[error("failed to decode backup or restore manifest at {path:?}: {source}")]
    BackupManifestDecode {
        /// Manifest path being validated.
        path: PathBuf,
        /// JSON decoding or structural-shape failure.
        #[source]
        source: serde_json::Error,
    },

    /// A backup or restore manifest disagrees with the checkpoint bundle it
    /// describes.
    #[error("backup or restore checkpoint validation failed at {path:?}: {reason}")]
    BackupCheckpointValidation {
        /// Staged bundle path being validated.
        path: PathBuf,
        /// Stable human-readable validation failure.
        reason: String,
    },

    /// Backup or restore manifest filesystem operation failed.
    #[error("backup or restore manifest {operation} failed at {path:?}: {source}")]
    BackupManifestIo {
        /// Filesystem operation being attempted.
        operation: &'static str,
        /// Path the operation targeted.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// Internal batching produced an empty commit.
    #[error("internal error: attempted to commit an empty canonical batch")]
    EmptyCanonicalBatch,

    /// Bulk-catchup loop ended without committing any batch.
    #[error("internal error: bulk catchup loop produced no commit")]
    BulkCatchupProducedNoCommit,

    /// Historical bulk catchup was asked to finalize blocks inside the live reorg window.
    #[error(
        "bulk catchup to height {to_height:?} is inside the node-reported reorg window: tip {tip_height:?}, reorg window {reorg_window_blocks} blocks, maximum historical height {maximum_historical_height:?}; pass --allow-near-tip-finalize only for local or explicitly disposable stores"
    )]
    NearTipBulkCatchupRequiresExplicitFinalize {
        /// Last requested bulk catchup height.
        to_height: BlockHeight,
        /// Current node tip height.
        tip_height: BlockHeight,
        /// Configured store reorg window in blocks.
        reorg_window_blocks: u32,
        /// Highest height that can be advanced past the safe tip without explicit override.
        maximum_historical_height: BlockHeight,
    },

    /// Bulk catchup cannot derive a chain-global commitment-tree size base.
    #[error(
        "bulk catchup from height {from_height:?} requires contiguous commitment-tree metadata; start a fresh store at height 1 or append immediately after current tip {current_tip_height:?}"
    )]
    BulkCatchupRequiresContiguousTipMetadata {
        /// First requested bulk catchup height.
        from_height: BlockHeight,
        /// Current store tip height, when the store is not empty.
        current_tip_height: Option<BlockHeight>,
    },

    /// Bulk-catchup checkpoint height does not match the requested `from_height`.
    #[error(
        "bulk catchup checkpoint height {checkpoint_height:?} does not align with from_height {from_height:?}; from_height must equal checkpoint_height + 1"
    )]
    BulkCatchupCheckpointMisaligned {
        /// Operator-supplied checkpoint height.
        checkpoint_height: BlockHeight,
        /// Requested first bulk catchup height.
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

    /// Reorg replacement exceeded the configured reorg window.
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

/// Built canonical artifacts produced for one source block.
///
/// Output of [`finalize_derived_block`](crate::artifact_builder::finalize_derived_block).
/// Each field is final once this struct exists; the consumer absorbs it
/// into an `CanonicalBatch` and the batch is then committed atomically.
#[derive(Debug)]
pub struct BuiltArtifacts {
    /// Canonical block-header facts.
    pub block_header: BlockHeaderArtifact,
    /// Optional raw block blob.
    pub block_blob: Option<BlockBlobArtifact>,
    /// Lightwalletd compact block with final `chain_metadata` stamped.
    pub compact_block: CompactBlockArtifact,
    /// Block-local transaction id index rows.
    pub block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    /// Mined transaction locations.
    pub transaction_locations: Vec<TransactionLocation>,
    /// Per-transaction public facts.
    pub transaction_facts: Vec<TransactionFactsArtifact>,
    /// Per-transaction signed shielded-pool balances.
    pub transaction_intrinsic_value_balances: Vec<TransactionIntrinsicValueBalancesArtifact>,
    /// Optional raw transaction blobs.
    pub transaction_blobs: Vec<TransactionBlobArtifact>,
    /// Transparent output artifacts keyed by outpoint. The store derives
    /// the address-output projection rows from these at commit.
    pub transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
    /// Running commitment-tree position after this block is folded in.
    pub tip_metadata: ChainTipMetadata,
}

/// In-flight canonical artifact batch accumulated between commits.
#[derive(Default)]
pub(crate) struct CanonicalBatch {
    pub(crate) block_headers: Vec<BlockHeaderArtifact>,
    pub(crate) block_blobs: Vec<BlockBlobArtifact>,
    pub(crate) compact_blocks: Vec<CompactBlockArtifact>,
    pub(crate) block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    pub(crate) transaction_locations: Vec<TransactionLocation>,
    pub(crate) transaction_facts: Vec<TransactionFactsArtifact>,
    pub(crate) transaction_intrinsic_value_balances: Vec<TransactionIntrinsicValueBalancesArtifact>,
    pub(crate) transaction_blobs: Vec<TransactionBlobArtifact>,
    pub(crate) tree_states: Vec<TreeStateArtifact>,
    pub(crate) final_note_commitment_roots: Vec<BlockFinalNoteCommitmentRoots>,
    pub(crate) subtree_roots: Vec<SubtreeRootArtifact>,
    pub(crate) transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
    pub(crate) transparent_spend_facts: Vec<TransparentSpendFact>,
    pub(crate) prefetched_spent_transparent_outputs: Vec<TransparentOutputArtifact>,
    pub(crate) tip_metadata: Option<ChainTipMetadata>,
    transactions: usize,
    transparent_outputs: usize,
    transparent_spend_references: usize,
    artifact_bytes: usize,
    estimated_write_bytes: usize,
}

impl CanonicalBatch {
    /// Appends one block.s built artifacts into the in-flight batch.
    ///
    /// Called once per `finalize_derived_block` result. Each field is
    /// moved into its matching `CanonicalBatch` vector; the running tip
    /// metadata is overwritten with the latest built value.
    pub(crate) fn absorb(&mut self, built: BuiltArtifacts) {
        self.absorb_with_prefetched_spent_outputs(built, Vec::new());
    }

    /// Returns the resource cost contributed by one finalized block.
    ///
    /// This is intentionally shared by pre-admission budgeting and the
    /// post-admission accumulator, so they cannot drift apart as canonical
    /// artifacts gain new write paths.
    pub(crate) fn work_cost_for_block(
        built: &BuiltArtifacts,
        prefetched_outputs: &[TransparentOutputArtifact],
    ) -> CanonicalBatchCost {
        let transparent_spend_references =
            transparent_spend_reference_count_for_transactions(&built.transaction_facts);
        CanonicalBatchCost {
            blocks: 1,
            transactions: built.transaction_facts.len(),
            transparent_outputs: built.transparent_outputs_by_outpoint.len(),
            transparent_spend_references,
            artifact_bytes: canonical_block_artifact_bytes(built).saturating_add(
                prefetched_spent_transparent_output_bytes(prefetched_outputs),
            ),
            estimated_write_bytes: canonical_block_estimated_write_bytes(
                built,
                transparent_spend_references,
            ),
        }
    }

    fn absorb_work_cost(&mut self, cost: CanonicalBatchCost) {
        self.transactions = self.transactions.saturating_add(cost.transactions);
        self.transparent_outputs = self
            .transparent_outputs
            .saturating_add(cost.transparent_outputs);
        self.transparent_spend_references = self
            .transparent_spend_references
            .saturating_add(cost.transparent_spend_references);
        self.artifact_bytes = self.artifact_bytes.saturating_add(cost.artifact_bytes);
        self.estimated_write_bytes = self
            .estimated_write_bytes
            .saturating_add(cost.estimated_write_bytes);
    }

    fn absorb_built_artifacts(&mut self, built: BuiltArtifacts) {
        self.block_headers.push(built.block_header);
        if let Some(block_blob) = built.block_blob {
            self.block_blobs.push(block_blob);
        }
        self.compact_blocks.push(built.compact_block);
        self.block_transaction_index
            .extend(built.block_transaction_index);
        self.transaction_locations
            .extend(built.transaction_locations);
        self.transaction_facts.extend(built.transaction_facts);
        self.transaction_intrinsic_value_balances
            .extend(built.transaction_intrinsic_value_balances);
        self.transaction_blobs.extend(built.transaction_blobs);
        self.transparent_outputs_by_outpoint
            .extend(built.transparent_outputs_by_outpoint);
        self.tip_metadata = Some(built.tip_metadata);
    }

    /// Appends built artifacts with transparent prevouts prefetched upstream.
    pub(crate) fn absorb_with_prefetched_spent_outputs(
        &mut self,
        built: BuiltArtifacts,
        prefetched_outputs: Vec<TransparentOutputArtifact>,
    ) {
        let cost = Self::work_cost_for_block(&built, &prefetched_outputs);
        self.absorb_work_cost(cost);
        self.absorb_built_artifacts(built);
        self.prefetched_spent_transparent_outputs
            .extend(prefetched_outputs);
    }

    pub(crate) fn push_tree_state_checkpoint(&mut self, tree_state: TreeStateArtifact) {
        self.artifact_bytes = self
            .artifact_bytes
            .saturating_add(tree_state.payload_bytes.len());
        self.tree_states.push(tree_state);
    }

    pub(crate) fn push_final_note_commitment_roots(
        &mut self,
        roots: BlockFinalNoteCommitmentRoots,
    ) {
        self.final_note_commitment_roots.push(roots);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.block_headers.is_empty()
    }

    pub(crate) fn work_cost(&self) -> CanonicalBatchCost {
        CanonicalBatchCost {
            blocks: self.block_headers.len(),
            transactions: self.transactions,
            transparent_outputs: self.transparent_outputs,
            transparent_spend_references: self.transparent_spend_references,
            artifact_bytes: self.artifact_bytes,
            estimated_write_bytes: self.estimated_write_bytes,
        }
    }

    fn clear_work_cost(&mut self) {
        self.transactions = 0;
        self.transparent_outputs = 0;
        self.transparent_spend_references = 0;
        self.artifact_bytes = 0;
        self.estimated_write_bytes = 0;
        self.prefetched_spent_transparent_outputs.clear();
    }
}

fn canonical_block_artifact_bytes(built: &BuiltArtifacts) -> usize {
    let block_blob_bytes = built
        .block_blob
        .as_ref()
        .map_or(0, |block_blob| block_blob.raw_block_bytes.len());
    let compact_block_bytes = built.compact_block.payload_bytes.len();
    let transaction_blob_bytes =
        built
            .transaction_blobs
            .iter()
            .fold(0usize, |bytes, transaction_blob| {
                bytes.saturating_add(transaction_blob.raw_transaction_bytes.len())
            });
    block_blob_bytes
        .saturating_add(compact_block_bytes)
        .saturating_add(transaction_blob_bytes)
}

fn canonical_block_estimated_write_bytes(
    built: &BuiltArtifacts,
    transparent_spend_references: usize,
) -> usize {
    let block_index_bytes = ESTIMATED_BLOCK_WRITE_BYTES
        .saturating_add(
            built
                .block_transaction_index
                .len()
                .saturating_mul(ESTIMATED_BLOCK_TRANSACTION_INDEX_WRITE_BYTES),
        )
        .saturating_add(
            built
                .transaction_locations
                .len()
                .saturating_mul(ESTIMATED_TRANSACTION_LOCATION_WRITE_BYTES),
        );
    let transaction_fact_bytes = built
        .transaction_facts
        .len()
        .saturating_mul(ESTIMATED_TRANSACTION_FACT_WRITE_BYTES);
    let transparent_output_bytes = built
        .transparent_outputs_by_outpoint
        .len()
        .saturating_mul(ESTIMATED_TRANSPARENT_OUTPUT_WRITE_BYTES);
    let address_output_index_bytes = built
        .transparent_outputs_by_outpoint
        .len()
        .saturating_mul(ESTIMATED_ADDRESS_OUTPUT_INDEX_WRITE_BYTES);
    let transparent_spend_fact_bytes =
        transparent_spend_references.saturating_mul(ESTIMATED_TRANSPARENT_SPEND_FACT_WRITE_BYTES);

    canonical_block_artifact_bytes(built)
        .saturating_add(block_index_bytes)
        .saturating_add(transaction_fact_bytes)
        .saturating_add(transparent_output_bytes)
        .saturating_add(address_output_index_bytes)
        .saturating_add(transparent_spend_fact_bytes)
}

pub(crate) fn prefetched_spent_transparent_output_bytes(
    outputs: &[TransparentOutputArtifact],
) -> usize {
    outputs.iter().fold(0usize, |bytes, output| {
        bytes.saturating_add(128usize.saturating_add(output.script_pub_key.len()))
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CanonicalBatchCost {
    pub(crate) blocks: usize,
    pub(crate) transactions: usize,
    pub(crate) transparent_outputs: usize,
    pub(crate) transparent_spend_references: usize,
    pub(crate) artifact_bytes: usize,
    pub(crate) estimated_write_bytes: usize,
}

impl CanonicalBatchCost {
    pub(crate) fn saturating_add(self, next: Self) -> Self {
        Self {
            blocks: self.blocks.saturating_add(next.blocks),
            transactions: self.transactions.saturating_add(next.transactions),
            transparent_outputs: self
                .transparent_outputs
                .saturating_add(next.transparent_outputs),
            transparent_spend_references: self
                .transparent_spend_references
                .saturating_add(next.transparent_spend_references),
            artifact_bytes: self.artifact_bytes.saturating_add(next.artifact_bytes),
            estimated_write_bytes: self
                .estimated_write_bytes
                .saturating_add(next.estimated_write_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalBatchBudget {
    max_blocks: NonZeroU32,
    max_artifact_bytes: NonZeroU64,
    max_estimated_write_bytes: NonZeroU64,
    min_blocks_before_estimated_write_close: NonZeroU32,
}

impl CanonicalBatchBudget {
    pub(crate) const fn new(
        max_blocks: NonZeroU32,
        max_artifact_bytes: NonZeroU64,
        max_estimated_write_bytes: NonZeroU64,
        min_blocks_before_estimated_write_close: NonZeroU32,
    ) -> Self {
        Self {
            max_blocks,
            max_artifact_bytes,
            max_estimated_write_bytes,
            min_blocks_before_estimated_write_close,
        }
    }

    pub(crate) fn commit_trigger(
        self,
        cost: CanonicalBatchCost,
    ) -> Option<CanonicalBatchCloseTrigger> {
        if cost.blocks >= nonzero_u32_to_usize(self.max_blocks) {
            return Some(CanonicalBatchCloseTrigger::BlockCount);
        }
        if cost.artifact_bytes >= nonzero_u64_to_usize(self.max_artifact_bytes) {
            return Some(CanonicalBatchCloseTrigger::ArtifactBytes);
        }
        if self.can_close_on_estimated_write_bytes(cost.blocks)
            && cost.estimated_write_bytes >= nonzero_u64_to_usize(self.max_estimated_write_bytes)
        {
            return Some(CanonicalBatchCloseTrigger::EstimatedWriteBytes);
        }
        None
    }

    /// Returns the close trigger for the current batch before admitting the
    /// next finalized block.
    ///
    /// Exact-limit blocks still join the current batch and close it through
    /// [`Self::commit_trigger`]. This path is only for an existing batch that
    /// the next block would push beyond a configured bound; an oversized first
    /// block remains valid and is committed on its own.
    pub(crate) fn commit_trigger_before_next_block(
        self,
        current: CanonicalBatchCost,
        next: CanonicalBatchCost,
    ) -> Option<CanonicalBatchCloseTrigger> {
        if current.blocks == 0 {
            return None;
        }
        let combined = current.saturating_add(next);
        if combined.blocks > nonzero_u32_to_usize(self.max_blocks) {
            return Some(CanonicalBatchCloseTrigger::BlockCount);
        }
        if combined.artifact_bytes > nonzero_u64_to_usize(self.max_artifact_bytes) {
            return Some(CanonicalBatchCloseTrigger::ArtifactBytes);
        }
        if self.can_close_on_estimated_write_bytes(combined.blocks)
            && combined.estimated_write_bytes > nonzero_u64_to_usize(self.max_estimated_write_bytes)
        {
            return Some(CanonicalBatchCloseTrigger::EstimatedWriteBytes);
        }
        None
    }

    fn can_close_on_estimated_write_bytes(self, block_count: usize) -> bool {
        block_count == 1
            || block_count >= nonzero_u32_to_usize(self.min_blocks_before_estimated_write_close)
    }
}

fn nonzero_u32_to_usize(amount: NonZeroU32) -> usize {
    match usize::try_from(amount.get()) {
        Ok(converted) => converted,
        Err(_error) => usize::MAX,
    }
}

fn nonzero_u64_to_usize(amount: NonZeroU64) -> usize {
    match usize::try_from(amount.get()) {
        Ok(converted) => converted,
        Err(_error) => usize::MAX,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CanonicalBatchCloseTrigger {
    BlockCount,
    ArtifactBytes,
    EstimatedWriteBytes,
}

impl CanonicalBatchCloseTrigger {
    pub(crate) const fn metric_label(self) -> &'static str {
        match self {
            Self::BlockCount => "block_count",
            Self::ArtifactBytes => "artifact_bytes",
            Self::EstimatedWriteBytes => "estimated_write_bytes",
        }
    }
}

/// Tracks the next subtree-root index per shielded protocol so a follow-up
/// batch knows which roots the source has already provided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IngestSubtreeRootIndexes {
    sapling: SubtreeRootIndex,
    orchard: SubtreeRootIndex,
    ironwood: SubtreeRootIndex,
}

impl Default for IngestSubtreeRootIndexes {
    fn default() -> Self {
        Self {
            sapling: SubtreeRootIndex::new(0),
            orchard: SubtreeRootIndex::new(0),
            ironwood: SubtreeRootIndex::new(0),
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
            ironwood: SubtreeRootIndex::new(
                tip_metadata.completed_subtree_count(ShieldedProtocol::Ironwood),
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
            ShieldedProtocol::Ironwood => Ok(self.ironwood),
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
            ShieldedProtocol::Ironwood => self.ironwood = subtree_index,
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
    let _active = SourceFetchActiveGauge::enter("fetch_block");
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

/// Fetches a bounded source-chain segment with the same retry policy as
/// single-block fetches.
pub(crate) async fn fetch_chain_segment_with_retry<Source>(
    request_timeout: Duration,
    source: &Source,
    limits: SourceChainSegmentLimits,
    retry_state: &mut IngestRetryState,
) -> Result<SourceChainSegment, IngestError>
where
    Source: NodeSource,
{
    let started_at = Instant::now();
    let operation = limits.cursor.next_connected_height().map_or_else(
        || "fetch source chain segment after cursor".to_owned(),
        |height| format!("fetch source chain segment from height {}", height.value()),
    );
    let _active = SourceFetchActiveGauge::enter("fetch_chain_segment");
    let source_outcome = retry_source_request(
        "fetch_chain_segment",
        operation,
        request_timeout,
        retry_state,
        || async { source.fetch_chain_segment(limits).await },
    )
    .await;
    record_ingest_source_outcome("fetch_chain_segment", started_at, &source_outcome);
    if let Ok(segment) = &source_outcome {
        let stats = segment.stats();
        metrics::histogram!("zinder_ingest_source_segment_max_blocks")
            .record(usize_to_u32_saturating(segment.len()));
        metrics::histogram!("zinder_ingest_source_segment_response_payload_bytes")
            .record(u64_to_f64(stats.response_payload_bytes()));
    }
    source_outcome
}

/// Observes one block's final roots without making canonical progress depend
/// on explorer enrichment availability.
pub(crate) async fn observe_final_note_commitment_roots<Source>(
    request_timeout: Duration,
    source: &Source,
    block_id: zinder_core::BlockId,
) -> Option<SourceTreeState>
where
    Source: NodeSource,
{
    if !source.capabilities().supports(NodeCapability::TreeState) {
        return None;
    }
    let outcome =
        tokio::time::timeout(request_timeout, source.fetch_tree_state_for_block(block_id)).await;
    match outcome {
        Ok(Ok(tree_state)) => Some(tree_state),
        Ok(Err(error)) => {
            record_optional_root_observation_failure(block_id, &error);
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::ingest",
                event = "final_note_commitment_roots_observation_failed",
                height = block_id.height.value(),
                block_hash = %hex::encode(block_id.hash.as_bytes()),
                error = %error,
                "final note-commitment roots are absent from this canonical commit; background backfill will retry"
            );
            None
        }
    }
}

fn record_optional_root_observation_failure(block_id: zinder_core::BlockId, error: &SourceError) {
    tracing::warn!(
        target: "zinder::ingest",
        event = "final_note_commitment_roots_observation_failed",
        height = block_id.height.value(),
        block_hash = %hex::encode(block_id.hash.as_bytes()),
        error = %error,
        "final note-commitment roots are absent from this canonical commit; background backfill will retry"
    );
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
    let _active = SourceFetchActiveGauge::enter("fetch_subtree_roots");
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
        .ok_or(IngestError::EmptyCanonicalBatch)?;

    if current_chain_epoch
        .visible_tip_height
        .next()
        .is_some_and(|next_tip| first_candidate.height == next_tip)
        && first_candidate.parent_hash == current_chain_epoch.visible_tip_hash
    {
        return Ok(ReorgWindowChange::Extend {
            block_range: BlockHeightRange::inclusive(first_candidate.height, last_candidate.height),
        });
    }

    let replacement_depth = current_chain_epoch
        .visible_tip_height
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
    batch: &mut CanonicalBatch,
    next_subtree_root_indexes: IngestSubtreeRootIndexes,
    retry_state: &mut IngestRetryState,
) -> Result<IngestSubtreeRootIndexes, IngestError>
where
    Source: NodeSource,
{
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyCanonicalBatch)?;
    let block_hash_by_height = batch
        .block_headers
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
            ShieldedProtocol::Ironwood,
            tip_metadata.completed_subtree_count(ShieldedProtocol::Ironwood),
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
    batch: &mut CanonicalBatch,
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
/// its mode: bulk catchup always advances finalization to the new tip; the
/// tip-follower issues `Extend` for tip advancement and `Replace` for
/// reorgs, then advances finalization separately once the new tip is at
/// least `reorg_window_blocks` deep.
pub(crate) async fn commit_ingest_batch(
    store: &PrimaryChainStore,
    chain_epoch: ChainEpoch,
    batch: &mut CanonicalBatch,
    reorg_window_change: ReorgWindowChange,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let started_at = Instant::now();
    let batch_cost = batch.work_cost();
    if batch.is_empty() {
        let commit_outcome = Err(IngestError::EmptyCanonicalBatch);
        record_ingest_commit_outcome(started_at, batch_cost, &commit_outcome);
        return commit_outcome;
    }

    batch.transparent_spend_facts = resolve_transparent_spend_facts_for_batch(
        store.clone(),
        batch,
        reorg_window_change.clone(),
    )
    .await?;
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

#[derive(Clone, Copy)]
struct TransparentSpendReference {
    spent_outpoint: TransparentOutPoint,
    input_index: u32,
    spending_transaction_id: TransactionId,
    tx_index_in_block: u32,
    block_height: BlockHeight,
    block_hash: BlockHash,
}

async fn resolve_transparent_spend_facts_for_batch(
    store: PrimaryChainStore,
    batch: &CanonicalBatch,
    reorg_window_change: ReorgWindowChange,
) -> Result<Vec<TransparentSpendFact>, IngestError> {
    let spend_references = transparent_spend_references_for_transactions(&batch.transaction_facts);
    if spend_references.is_empty() {
        return Ok(Vec::new());
    }
    let mut batch_outputs = batch
        .transparent_outputs_by_outpoint
        .iter()
        .cloned()
        .map(|output| (output.outpoint, output))
        .collect::<HashMap<_, _>>();
    for output in batch.prefetched_spent_transparent_outputs.iter().cloned() {
        batch_outputs.entry(output.outpoint).or_insert(output);
    }

    tokio::task::spawn_blocking(move || {
        resolve_transparent_spend_facts(
            &store,
            spend_references,
            batch_outputs,
            &reorg_window_change,
        )
    })
    .await
    .map_err(|join_error| IngestError::BlockingTaskFailed {
        reason: join_error.to_string(),
    })?
}

fn resolve_transparent_spend_facts(
    store: &PrimaryChainStore,
    spend_references: Vec<TransparentSpendReference>,
    mut outputs_by_outpoint: HashMap<TransparentOutPoint, TransparentOutputArtifact>,
    reorg_window_change: &ReorgWindowChange,
) -> Result<Vec<TransparentSpendFact>, IngestError> {
    let missing_store_outpoints = spend_references
        .iter()
        .filter_map(|spend| {
            (!outputs_by_outpoint.contains_key(&spend.spent_outpoint))
                .then_some(spend.spent_outpoint)
        })
        .collect::<Vec<_>>();
    if !missing_store_outpoints.is_empty()
        && let Some(current_chain_epoch) = store.current_chain_epoch()?
    {
        let store_outputs = store.transparent_outputs_by_outpoints_for_writer_commit(
            StoreReadCaller::CommitFallback,
            current_chain_epoch,
            &missing_store_outpoints,
        )?;
        for (outpoint, output) in store_outputs {
            if output_is_from_reverted_replacement_range(&output, reorg_window_change) {
                continue;
            }
            outputs_by_outpoint.insert(outpoint, output);
        }
    }

    let mut spend_facts = Vec::with_capacity(spend_references.len());
    let mut unresolved_count = 0usize;
    for spend in spend_references {
        let Some(output) = outputs_by_outpoint.get(&spend.spent_outpoint) else {
            unresolved_count = unresolved_count.saturating_add(1);
            continue;
        };
        spend_facts.push(TransparentSpendFact::from_input_and_output(
            spend.spent_outpoint,
            spend.input_index,
            spend.spending_transaction_id,
            spend.tx_index_in_block,
            spend.block_height,
            spend.block_hash,
            output,
        ));
    }
    record_transparent_spend_fact_resolution(spend_facts.len(), unresolved_count);

    Ok(spend_facts)
}

fn transparent_spend_references_for_transactions(
    transactions: &[TransactionFactsArtifact],
) -> Vec<TransparentSpendReference> {
    let mut spends = Vec::new();
    for transaction in transactions {
        for input in &transaction.transparent_inputs {
            if input.spent_outpoint.is_coinbase_sentinel() {
                continue;
            }
            spends.push(TransparentSpendReference {
                spent_outpoint: input.spent_outpoint,
                input_index: input.input_index,
                spending_transaction_id: transaction.location.transaction_id,
                tx_index_in_block: transaction.location.tx_index_in_block,
                block_height: transaction.location.block_height,
                block_hash: transaction.location.block_hash,
            });
        }
    }
    spends
}

fn transparent_spend_reference_count_for_transactions(
    transactions: &[TransactionFactsArtifact],
) -> usize {
    transactions.iter().fold(0usize, |count, transaction| {
        count.saturating_add(
            transaction
                .transparent_inputs
                .iter()
                .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
                .count(),
        )
    })
}

fn output_is_from_reverted_replacement_range(
    output: &TransparentOutputArtifact,
    reorg_window_change: &ReorgWindowChange,
) -> bool {
    matches!(
        reorg_window_change,
        ReorgWindowChange::Replace { from_height } if output.block_height >= *from_height
    )
}

fn drain_batch_into_chain_epoch_artifacts(
    chain_epoch: ChainEpoch,
    batch: &mut CanonicalBatch,
    reorg_window_change: ReorgWindowChange,
) -> ChainEpochArtifacts {
    let mut artifacts = ChainEpochArtifacts::new(
        chain_epoch,
        std::mem::take(&mut batch.block_headers),
        std::mem::take(&mut batch.compact_blocks),
    );
    if !batch.block_blobs.is_empty() {
        artifacts = artifacts.with_block_blobs(std::mem::take(&mut batch.block_blobs));
    }
    if !batch.block_transaction_index.is_empty() {
        artifacts = artifacts
            .with_block_transaction_index(std::mem::take(&mut batch.block_transaction_index));
    }
    if !batch.transaction_locations.is_empty() {
        artifacts =
            artifacts.with_transaction_locations(std::mem::take(&mut batch.transaction_locations));
    }
    if !batch.transaction_facts.is_empty() {
        artifacts = artifacts.with_transaction_facts(std::mem::take(&mut batch.transaction_facts));
    }
    if !batch.transaction_intrinsic_value_balances.is_empty() {
        artifacts = artifacts.with_transaction_intrinsic_value_balances(std::mem::take(
            &mut batch.transaction_intrinsic_value_balances,
        ));
    }
    if !batch.transaction_blobs.is_empty() {
        artifacts = artifacts.with_transaction_blobs(std::mem::take(&mut batch.transaction_blobs));
    }
    if !batch.tree_states.is_empty() {
        artifacts = artifacts.with_tree_states(std::mem::take(&mut batch.tree_states));
    }
    if !batch.final_note_commitment_roots.is_empty() {
        artifacts = artifacts.with_final_note_commitment_roots(std::mem::take(
            &mut batch.final_note_commitment_roots,
        ));
    }
    if !batch.subtree_roots.is_empty() {
        artifacts = artifacts.with_subtree_roots(std::mem::take(&mut batch.subtree_roots));
    }
    if !batch.transparent_outputs_by_outpoint.is_empty() {
        artifacts = artifacts.with_transparent_outputs_by_outpoint(std::mem::take(
            &mut batch.transparent_outputs_by_outpoint,
        ));
    }
    if !batch.transparent_spend_facts.is_empty() {
        artifacts = artifacts
            .with_transparent_spend_facts(std::mem::take(&mut batch.transparent_spend_facts));
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

/// In-flight gauge for source-side fetches, partitioned by `operation`.
///
/// The gauge tracks how many source requests are currently awaiting an
/// upstream response. Increment fires when the guard enters scope and the
/// decrement fires from `Drop`, so cancellation, timeout, and panic all
/// release the gauge without an explicit early-return path.
struct SourceFetchActiveGauge {
    operation: &'static str,
}

impl SourceFetchActiveGauge {
    fn enter(operation: &'static str) -> Self {
        metrics::gauge!(
            "zinder_ingest_source_fetch_active",
            "operation" => operation,
        )
        .increment(1.0);
        Self { operation }
    }
}

impl Drop for SourceFetchActiveGauge {
    fn drop(&mut self) {
        metrics::gauge!(
            "zinder_ingest_source_fetch_active",
            "operation" => self.operation,
        )
        .decrement(1.0);
    }
}

/// Records derive wall-clock and outcome for one source block.
///
/// In bulk catchup this wraps the parallel-safe `derive_block` call inside
/// the buffered stream; in tip-follow this wraps the one-block artifact
/// build. The histogram is the per-block CPU contribution to ingest
/// throughput before serial finalization and commit work.
pub(crate) fn record_ingest_block_prepare_outcome<T>(
    started_at: Instant,
    block_prepare_outcome: &Result<T, IngestError>,
) {
    metrics::histogram!(
        "zinder_ingest_block_prepare_duration_seconds",
        "status" => outcome_status(block_prepare_outcome),
        "error_class" => ingest_error_class(block_prepare_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_block_prepare_total",
        "status" => outcome_status(block_prepare_outcome),
        "error_class" => ingest_error_class(block_prepare_outcome.as_ref().err())
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

/// Publishes the current work cost of an in-flight canonical batch.
///
/// Called after every successful absorb and after every commit so the
/// gauge tracks the writer-visible queue from `0` to `canonical_batch_max_blocks`.
pub(crate) fn record_ingest_batch_work_cost(cost: CanonicalBatchCost) {
    metrics::gauge!("zinder_ingest_batch_accumulator_blocks")
        .set(f64::from(usize_to_u32_saturating(cost.blocks)));
    metrics::gauge!("zinder_ingest_batch_accumulator_transactions")
        .set(f64::from(usize_to_u32_saturating(cost.transactions)));
    metrics::gauge!("zinder_ingest_batch_accumulator_transparent_outputs")
        .set(f64::from(usize_to_u32_saturating(cost.transparent_outputs)));
    metrics::gauge!("zinder_ingest_batch_accumulator_transparent_spend_references").set(f64::from(
        usize_to_u32_saturating(cost.transparent_spend_references),
    ));
    metrics::gauge!("zinder_ingest_batch_accumulator_artifact_bytes")
        .set(u64_to_f64(usize_to_u64_saturating(cost.artifact_bytes)));
    metrics::gauge!("zinder_ingest_batch_accumulator_estimated_write_bytes").set(u64_to_f64(
        usize_to_u64_saturating(cost.estimated_write_bytes),
    ));
}

pub(crate) fn record_ingest_batch_commit_trigger(trigger: CanonicalBatchCloseTrigger) {
    metrics::counter!(
        "zinder_ingest_batch_commit_trigger_total",
        "trigger" => trigger.metric_label()
    )
    .increment(1);
}

fn record_transparent_spend_fact_resolution(resolved_count: usize, unresolved_count: usize) {
    if resolved_count > 0 {
        metrics::counter!(
            "zinder_ingest_transparent_spend_fact_resolution_total",
            "status" => "resolved"
        )
        .increment(u64::from(usize_to_u32_saturating(resolved_count)));
    }
    if unresolved_count > 0 {
        metrics::counter!(
            "zinder_ingest_transparent_spend_fact_resolution_total",
            "status" => "unresolved"
        )
        .increment(u64::from(usize_to_u32_saturating(unresolved_count)));
    }
}

fn record_ingest_commit_outcome(
    started_at: Instant,
    batch_cost: CanonicalBatchCost,
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
    .record(usize_to_u32_saturating(batch_cost.blocks));
    metrics::histogram!(
        "zinder_ingest_commit_batch_transaction_count",
        "status" => outcome_status(commit_outcome)
    )
    .record(usize_to_u32_saturating(batch_cost.transactions));
    metrics::histogram!(
        "zinder_ingest_commit_batch_transparent_output_count",
        "status" => outcome_status(commit_outcome)
    )
    .record(usize_to_u32_saturating(batch_cost.transparent_outputs));
    metrics::histogram!(
        "zinder_ingest_commit_batch_transparent_spend_reference_count",
        "status" => outcome_status(commit_outcome)
    )
    .record(usize_to_u32_saturating(
        batch_cost.transparent_spend_references,
    ));
    metrics::histogram!(
        "zinder_ingest_commit_batch_estimated_write_bytes",
        "status" => outcome_status(commit_outcome)
    )
    .record(u64_to_f64(usize_to_u64_saturating(
        batch_cost.estimated_write_bytes,
    )));
}

pub(crate) const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

pub(crate) fn ingest_error_class(error: Option<&IngestError>) -> &'static str {
    match error {
        None => "none",
        Some(IngestError::UnknownNodeSource { .. }) => "unknown_node_source",
        Some(IngestError::SourceSegmentFetchTaskStopped { .. }) => {
            "source_segment_fetch_task_stopped"
        }
        Some(IngestError::SubtreeRootsUnavailable { .. }) => "subtree_roots_unavailable",
        Some(IngestError::SubtreeRootCompletingBlockMissing { .. }) => {
            "subtree_root_completing_block_missing"
        }
        Some(IngestError::TransparentOutputOutputMissing { .. }) => {
            "transparent_output_output_missing"
        }
        Some(IngestError::UnsupportedShieldedProtocol { .. }) => "unsupported_shielded_protocol",
        Some(IngestError::EmptyCanonicalBatch) => "empty_canonical_batch",
        Some(IngestError::BulkCatchupProducedNoCommit) => "bulk_catchup_produced_no_commit",
        Some(IngestError::NearTipBulkCatchupRequiresExplicitFinalize { .. }) => {
            "near_tip_bulk_catchup_requires_explicit_finalize"
        }
        Some(IngestError::BulkCatchupRequiresContiguousTipMetadata { .. }) => {
            "bulk_catchup_requires_contiguous_tip_metadata"
        }
        Some(IngestError::BulkCatchupCheckpointMisaligned { .. }) => {
            "bulk_catchup_checkpoint_misaligned"
        }
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
        Some(IngestError::DeriveStoreMissing { .. }) => "derive_store_missing",
        Some(IngestError::ProjectionStoreMissingForCanonical { .. }) => {
            "projection_store_missing_for_canonical"
        }
        Some(IngestError::ProjectionStoreWithoutCanonicalHistory { .. }) => {
            "projection_store_without_canonical_history"
        }
        Some(IngestError::ProjectionRetentionCoverageInsufficient { .. }) => {
            "projection_retention_coverage_insufficient"
        }
        Some(IngestError::BackupCheckpointStagingExists { .. }) => {
            "backup_checkpoint_staging_exists"
        }
        Some(IngestError::BackupCheckpointDestinationExists { .. }) => {
            "backup_checkpoint_destination_exists"
        }
        Some(IngestError::BackupCheckpointInstall { .. }) => "backup_checkpoint_install",
        Some(IngestError::BackupManifestEncode { .. }) => "backup_manifest_encode",
        Some(IngestError::BackupManifestDecode { .. }) => "backup_manifest_decode",
        Some(IngestError::BackupManifestIo { .. }) => "backup_manifest_io",
        Some(IngestError::BackupCheckpointValidation { .. }) => "backup_checkpoint_validation",
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
///   range within the reorg window. Emitted at `WARN` because reorgs warrant operator
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
                tip_height = chain_epoch.visible_tip_height.value(),
                tip_hash = %display_block_hash(chain_epoch.visible_tip_hash),
                safe_tip_height = chain_epoch.settled_tip_height.value(),
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
                tip_height = chain_epoch.visible_tip_height.value(),
                tip_hash = %display_block_hash(chain_epoch.visible_tip_hash),
                safe_tip_height = chain_epoch.settled_tip_height.value(),
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
    .set(u32_to_f64(chain_epoch.visible_tip_height.value()));
    metrics::gauge!(
        "zinder_ingest_writer_safe_tip_height",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u32_to_f64(chain_epoch.settled_tip_height.value()));
}

fn display_block_hash(block_hash: BlockHash) -> String {
    encode_rpc_block_hash_hex(block_hash)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain progress values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
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
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;

    #[test]
    fn ingest_batch_budget_triggers_on_block_count_limit() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(1),
            NonZeroU64::MAX,
            NonZeroU64::MAX,
            NonZeroU32::MIN,
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 3,
                ..CanonicalBatchCost::default()
            }),
            Some(CanonicalBatchCloseTrigger::BlockCount)
        );
    }

    #[test]
    fn ingest_batch_budget_triggers_on_artifact_bytes() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU64::MIN,
            NonZeroU64::MAX,
            NonZeroU32::MIN,
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 1,
                artifact_bytes: 1,
                ..CanonicalBatchCost::default()
            }),
            Some(CanonicalBatchCloseTrigger::ArtifactBytes)
        );
    }

    #[test]
    fn ingest_batch_budget_triggers_on_artifact_bytes_before_estimated_write_floor() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU64::MIN,
            NonZeroU64::MAX,
            NonZeroU32::MIN.saturating_add(99),
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 99,
                artifact_bytes: 1,
                ..CanonicalBatchCost::default()
            }),
            Some(CanonicalBatchCloseTrigger::ArtifactBytes)
        );
    }

    #[test]
    fn ingest_batch_budget_triggers_on_estimated_write_bytes_after_floor() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU64::MAX,
            NonZeroU64::MIN,
            NonZeroU32::MIN.saturating_add(99),
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 100,
                estimated_write_bytes: 1,
                ..CanonicalBatchCost::default()
            }),
            Some(CanonicalBatchCloseTrigger::EstimatedWriteBytes)
        );
    }

    #[test]
    fn ingest_batch_budget_defers_estimated_write_bytes_before_floor() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU64::MAX,
            NonZeroU64::MIN,
            NonZeroU32::MIN.saturating_add(99),
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 99,
                estimated_write_bytes: 1,
                ..CanonicalBatchCost::default()
            }),
            None
        );
    }

    #[test]
    fn ingest_batch_budget_closes_single_oversized_block_on_estimated_write_bytes() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::MIN.saturating_add(999),
            NonZeroU64::MAX,
            NonZeroU64::MIN,
            NonZeroU32::MIN.saturating_add(99),
        );

        assert_eq!(
            budget.commit_trigger(CanonicalBatchCost {
                blocks: 1,
                estimated_write_bytes: 1,
                ..CanonicalBatchCost::default()
            }),
            Some(CanonicalBatchCloseTrigger::EstimatedWriteBytes)
        );
    }

    #[test]
    fn ingest_batch_budget_closes_before_a_dense_block_exceeds_write_budget() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN),
            NonZeroU64::MAX,
            NonZeroU64::new(100).unwrap_or(NonZeroU64::MIN),
            NonZeroU32::MIN,
        );

        assert_eq!(
            budget.commit_trigger_before_next_block(
                CanonicalBatchCost {
                    blocks: 2,
                    estimated_write_bytes: 90,
                    ..CanonicalBatchCost::default()
                },
                CanonicalBatchCost {
                    blocks: 1,
                    estimated_write_bytes: 20,
                    ..CanonicalBatchCost::default()
                },
            ),
            Some(CanonicalBatchCloseTrigger::EstimatedWriteBytes)
        );
    }

    #[test]
    fn ingest_batch_budget_admits_an_oversized_first_block() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::new(100).unwrap_or(NonZeroU32::MIN),
            NonZeroU64::MAX,
            NonZeroU64::new(100).unwrap_or(NonZeroU64::MIN),
            NonZeroU32::MIN,
        );

        assert_eq!(
            budget.commit_trigger_before_next_block(
                CanonicalBatchCost::default(),
                CanonicalBatchCost {
                    blocks: 1,
                    estimated_write_bytes: 120,
                    ..CanonicalBatchCost::default()
                },
            ),
            None
        );
    }

    #[test]
    fn ingest_batch_budget_keeps_exact_limit_block_in_current_batch() {
        let budget = canonical_batch_budget_for_tests(
            NonZeroU32::new(3).unwrap_or(NonZeroU32::MIN),
            NonZeroU64::MAX,
            NonZeroU64::MAX,
            NonZeroU32::MIN,
        );

        assert_eq!(
            budget.commit_trigger_before_next_block(
                CanonicalBatchCost {
                    blocks: 2,
                    ..CanonicalBatchCost::default()
                },
                CanonicalBatchCost {
                    blocks: 1,
                    ..CanonicalBatchCost::default()
                },
            ),
            None
        );
    }

    fn canonical_batch_budget_for_tests(
        blocks: NonZeroU32,
        artifact_bytes: NonZeroU64,
        estimated_write_bytes: NonZeroU64,
        min_blocks_before_estimated_write_close: NonZeroU32,
    ) -> CanonicalBatchBudget {
        CanonicalBatchBudget::new(
            blocks,
            artifact_bytes,
            estimated_write_bytes,
            min_blocks_before_estimated_write_close,
        )
    }
}
