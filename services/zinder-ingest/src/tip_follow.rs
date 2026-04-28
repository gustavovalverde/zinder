use std::{num::NonZeroU32, path::PathBuf, time::Duration};

use prost::Message;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockId, ChainEpoch, ChainEpochId, ChainTipMetadata,
    Network, TreeStateArtifact,
};
use zinder_proto::compat::lightwalletd::CompactBlock as LightwalletdCompactBlock;
use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
use zinder_source::{NodeSource, NodeTarget, SourceBlock};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainEpochReader, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::chain_ingest::{
    ArtifactBuilder, IngestArtifactBuilder, IngestBatch, IngestError, IngestRetryState,
    IngestSubtreeRootIndexes, NodeSourceKind, commit_ingest_batch, current_unix_millis,
    fetch_block_with_retry, next_chain_epoch_id_after, next_chain_epoch_id_from,
    populate_subtree_root_artifacts, record_commit_outcome, select_best_chain,
};

/// Default lag threshold (in blocks) below which tip-follow reports `Ready`.
pub const DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS: u64 = 1;

/// Configuration for polling the upstream node tip and committing live chain changes.
#[derive(Clone, Debug)]
pub struct TipFollowConfig {
    /// Resolved upstream node endpoint (network, JSON-RPC URL, auth, timeout,
    /// response-size cap). See [`NodeTarget`].
    pub node: NodeTarget,
    /// Upstream node source implementation.
    pub node_source: NodeSourceKind,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// Number of near-tip blocks that may be replaced by a reorg.
    pub reorg_window_blocks: u32,
    /// Maximum number of blocks committed in one chain epoch.
    pub commit_batch_blocks: NonZeroU32,
    /// Delay between tip polls when no cancellation is requested.
    pub poll_interval: Duration,
    /// Lag threshold (in blocks) below which tip-follow reports `Ready`.
    ///
    /// When `(node_tip - store_tip) <= lag_threshold_blocks` the readiness
    /// state flips from `Syncing` to `Ready`. The default is 1, meaning the
    /// service is ready as soon as the store is at most one block behind the
    /// observed node tip.
    pub lag_threshold_blocks: u64,
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
    validate_tip_follow_config(config)?;

    let store = open_tip_follow_store(config)?;
    tip_follow_with_primary_store(config, source, store, readiness, cancel).await
}

/// Opens the primary store with the tip-follow reorg-window policy.
///
/// Binaries use this when they need to share the primary store handle with a
/// process-local adapter, such as the private ingest-control endpoint.
pub fn open_tip_follow_store(config: &TipFollowConfig) -> Result<PrimaryChainStore, IngestError> {
    validate_tip_follow_config(config)?;

    let mut store_options = ChainStoreOptions::for_network(config.node.network);
    store_options.reorg_window_blocks = config.reorg_window_blocks;
    PrimaryChainStore::open(&config.storage_path, store_options).map_err(IngestError::from)
}

/// Follows the upstream node tip with a caller-owned primary store handle.
///
/// This is the same loop as [`tip_follow`], but it avoids opening the primary
/// twice when the runtime needs the store for colocated control-plane RPCs.
pub async fn tip_follow_with_primary_store<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: PrimaryChainStore,
    readiness: &Readiness,
    cancel: CancellationToken,
) -> Result<(), IngestError>
where
    Source: NodeSource,
{
    validate_tip_follow_config(config)?;

    let mut retry_state = IngestRetryState::default();

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(config.poll_interval) => {}
        }

        let iteration = tip_follow_once(config, source, &store, &mut retry_state).await?;

        let lag_state =
            compute_tip_follow_readiness_state(&store, iteration.observed_tip_id.height, config)?;
        set_tip_follow_readiness(readiness, lag_state);
    }
}

fn set_tip_follow_readiness(readiness: &Readiness, lag_state: ReadinessState) {
    if matches!(
        readiness.report().cause,
        ReadinessCause::CursorAtRisk { .. }
    ) && matches!(lag_state.cause, ReadinessCause::Ready)
    {
        return;
    }

    readiness.set(lag_state);
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

fn validate_tip_follow_config(config: &TipFollowConfig) -> Result<(), IngestError> {
    if config.reorg_window_blocks == 0 {
        return Err(IngestError::InvalidReorgWindowBlocks);
    }
    if config.poll_interval.is_zero() {
        return Err(IngestError::InvalidTipFollowPollInterval);
    }

    Ok(())
}

/// Result of one tip-follow iteration: the node tip identity observed in
/// this iteration, plus the commit outcome (when the iteration produced one).
pub(crate) struct TipFollowIteration {
    /// Node tip identity observed at the start of the iteration.
    pub(crate) observed_tip_id: BlockId,
    /// Commit outcome when the iteration produced one, otherwise `None`.
    pub(crate) commit_outcome: Option<ChainEpochCommitOutcome>,
}

async fn tip_follow_once<Source>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    retry_state: &mut IngestRetryState,
) -> Result<TipFollowIteration, IngestError>
where
    Source: NodeSource,
{
    let mut iteration = tip_follow_once_with_artifact_builder(
        config,
        source,
        store,
        retry_state,
        IngestArtifactBuilder::with_initial_tip_metadata,
    )
    .await?;

    if let Some(commit_outcome) = iteration.commit_outcome.as_ref()
        && let Some(finalized_outcome) =
            finalize_tip_if_ready(config, store, commit_outcome.chain_epoch)?
    {
        iteration.commit_outcome = Some(finalized_outcome);
    }

    Ok(iteration)
}

async fn tip_follow_once_with_artifact_builder<Source, Builder, MakeBuilder>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    retry_state: &mut IngestRetryState,
    make_builder: MakeBuilder,
) -> Result<TipFollowIteration, IngestError>
where
    Source: NodeSource,
    Builder: ArtifactBuilder,
    MakeBuilder: FnOnce(ChainTipMetadata) -> Builder,
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
    let mut artifact_builder = make_builder(plan.parent_tip_metadata);

    let commit_outcome = commit_tip_follow_blocks(
        config,
        source,
        store,
        &mut artifact_builder,
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

    let next_height = BlockHeight::new(current_chain_epoch.tip_height.value().saturating_add(1));
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
        .block_at(height)?
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
    reason = "private commit helper keeps artifact-builder test injection visible"
)]
async fn commit_tip_follow_blocks<Source, Builder>(
    config: &TipFollowConfig,
    source: &Source,
    store: &PrimaryChainStore,
    artifact_builder: &mut Builder,
    plan: TipFollowPlan,
    current_chain_epoch: Option<ChainEpoch>,
    retry_state: &mut IngestRetryState,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    Builder: ArtifactBuilder,
{
    let mut batch = IngestBatch::default();
    for source_block in plan.source_blocks {
        if source_block.network != config.node.network {
            return Err(IngestError::Source(
                zinder_source::SourceError::SourceProtocolMismatch {
                    reason: "source block network does not match tip-follow configuration",
                },
            ));
        }

        let built_artifacts = artifact_builder.build(&source_block)?;
        if let Some(tree_state_payload_bytes) = source_block.tree_state_payload_bytes {
            batch.tree_states.push(TreeStateArtifact::new(
                source_block.height,
                source_block.hash,
                tree_state_payload_bytes,
            ));
        }
        batch.finalized_blocks.push(built_artifacts.block);
        batch.compact_blocks.push(built_artifacts.compact_block);
        batch.transactions.extend(built_artifacts.transactions);
        batch
            .transparent_address_utxos
            .extend(built_artifacts.transparent_address_utxos);
        batch
            .transparent_utxo_spends
            .extend(built_artifacts.transparent_utxo_spends);
        batch.tip_metadata = Some(built_artifacts.tip_metadata);
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
    let chain_epoch_id = next_chain_epoch_id_from(current_chain_epoch.as_ref())?;
    let chain_epoch = chain_epoch_for_tip_commit(
        config.node.network,
        chain_epoch_id,
        current_chain_epoch,
        &batch,
    )?;

    commit_ingest_batch(store, chain_epoch, &mut batch, plan.reorg_window_change)
}

fn chain_epoch_for_tip_commit(
    network: Network,
    chain_epoch_id: ChainEpochId,
    current_chain_epoch: Option<ChainEpoch>,
    batch: &IngestBatch,
) -> Result<ChainEpoch, IngestError> {
    let tip_block = batch
        .finalized_blocks
        .last()
        .ok_or(IngestError::EmptyIngestBatch)?;
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
        tip_metadata: batch.tip_metadata.ok_or(IngestError::EmptyIngestBatch)?,
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
    let finalized_block = reader.block_at(finalized_height)?.ok_or(
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
            ChainEpochArtifacts::new(finalized_chain_epoch, Vec::new(), Vec::new())
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
        error::Error,
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicU32, Ordering},
        },
    };

    use async_trait::async_trait;
    use prost::Message;
    use tempfile::tempdir;
    use zinder_core::{BlockArtifact, CompactBlockArtifact, SubtreeRootHash, SubtreeRootIndex};
    use zinder_proto::compat::lightwalletd::{
        ChainMetadata, CompactBlock as LightwalletdCompactBlock,
    };
    use zinder_runtime::ReadinessCause;
    use zinder_source::{
        NodeCapabilities, SourceBlockHeader, SourceError, SourceSubtreeRoot, SourceSubtreeRoots,
        ZebraJsonRpcSource,
    };

    use super::*;
    use crate::chain_ingest::BuiltArtifacts;

    #[tokio::test]
    async fn tip_follow_commits_first_available_height() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-empty-store");
        let config = test_tip_follow_config(&storage_path, 10)?;
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
        let config = test_tip_follow_config(&storage_path, 10)?;
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
        let mut config = test_tip_follow_config(&storage_path, 10)?;
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
                .block_at(BlockHeight::new(2))?
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
        let config = test_tip_follow_config(&storage_path, 10)?;
        let source = TestNodeSource::linear(2);
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let mut retry_state = IngestRetryState::default();
        let _first = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;
        let _second = test_tip_follow_once(&config, &source, &store, &mut retry_state).await?;

        source.replace_block(2, block_hash(20), block_hash(1))?;
        let reorged = test_tip_follow_once(&config, &source, &store, &mut retry_state)
            .await?
            .ok_or("expected replacement commit")?;

        assert_eq!(reorged.chain_epoch.tip_height, BlockHeight::new(2));
        assert_eq!(reorged.chain_epoch.tip_hash, block_hash(20));
        let reader = store.current_chain_epoch_reader()?;
        assert_eq!(
            reader
                .block_at(BlockHeight::new(2))?
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
        let config = test_tip_follow_config(&storage_path, 10)?;
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

        source.replace_block(2, block_hash(20), block_hash(1))?;
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
        let config = test_tip_follow_config(&storage_path, 10)?;
        let source = TestNodeSource::linear(1);
        let readiness = Readiness::default();
        let cancel = CancellationToken::new();
        cancel.cancel();

        tip_follow(&config, &source, &readiness, cancel).await?;

        Ok(())
    }

    #[tokio::test]
    async fn readiness_state_reports_ready_when_lag_within_threshold() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("tip-follow-readiness-ready-store");
        let mut config = test_tip_follow_config(&storage_path, 10)?;
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
        let mut config = test_tip_follow_config(&storage_path, 10)?;
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
        let mut config = test_tip_follow_config(&storage_path, 10)?;
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

    fn test_tip_follow_config(
        storage_path: &Path,
        reorg_window_blocks: u32,
    ) -> Result<TipFollowConfig, Box<dyn Error>> {
        Ok(TipFollowConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:39232".to_owned(),
                zinder_source::NodeAuth::None,
                Duration::from_secs(30),
                zinder_source::DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            node_source: NodeSourceKind::ZebraJsonRpc,
            storage_path: storage_path.to_owned(),
            reorg_window_blocks,
            commit_batch_blocks: NonZeroU32::new(1).ok_or("invalid batch size")?,
            poll_interval: Duration::from_millis(1),
            lag_threshold_blocks: super::DEFAULT_TIP_FOLLOW_LAG_THRESHOLD_BLOCKS,
        })
    }

    async fn test_tip_follow_once(
        config: &TipFollowConfig,
        source: &TestNodeSource,
        store: &PrimaryChainStore,
        retry_state: &mut IngestRetryState,
    ) -> Result<Option<ChainEpochCommitOutcome>, IngestError> {
        let iteration =
            tip_follow_once_with_artifact_builder(config, source, store, retry_state, |_| {
                TestArtifactBuilder
            })
            .await?;
        Ok(iteration.commit_outcome)
    }

    struct TestNodeSource {
        tip_height: AtomicU32,
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
                blocks: Mutex::new(blocks),
            }
        }

        fn set_tip_height(&self, tip_height: u32) {
            self.tip_height.store(tip_height, Ordering::SeqCst);
        }

        fn replace_block(
            &self,
            height: u32,
            hash: BlockHash,
            parent_hash: BlockHash,
        ) -> Result<(), Box<dyn Error>> {
            {
                let mut blocks = self
                    .blocks
                    .lock()
                    .map_err(|_| "test block mutex poisoned")?;
                if let Some(block) = blocks
                    .iter_mut()
                    .find(|block| block.height == BlockHeight::new(height))
                {
                    block.hash = hash;
                    block.parent_hash = parent_hash;
                }
            }

            Ok(())
        }
    }

    #[async_trait]
    impl NodeSource for TestNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_by_height(
            &self,
            height: BlockHeight,
        ) -> Result<SourceBlock, SourceError> {
            let block = self
                .blocks
                .lock()
                .map_err(|_| SourceError::NodeUnavailable {
                    reason: "test block mutex poisoned".to_owned(),
                    is_retryable: false,
                })?
                .iter()
                .copied()
                .find(|block| block.height == height)
                .ok_or_else(|| SourceError::BlockUnavailable {
                    height,
                    reason: "test block unavailable".to_owned(),
                    is_retryable: false,
                })?;
            let header = SourceBlockHeader {
                network: Network::ZcashRegtest,
                height,
                hash: block.hash,
                parent_hash: block.parent_hash,
                block_time_seconds: 1_774_668_400,
            };

            Ok(
                SourceBlock::new(header, format!("raw-block-{}", height.value()).into_bytes())
                    .with_tree_state_payload_bytes(test_tree_state_payload()),
            )
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            let height = BlockHeight::new(self.tip_height.load(Ordering::SeqCst));
            let hash = self
                .blocks
                .lock()
                .map_err(|_| SourceError::NodeUnavailable {
                    reason: "test block mutex poisoned".to_owned(),
                    is_retryable: false,
                })?
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

    fn test_tree_state_payload() -> Vec<u8> {
        br#"{"sapling":{"commitments":{"size":0}},"orchard":{"commitments":{"size":0}}}"#.to_vec()
    }

    struct TestArtifactBuilder;

    impl ArtifactBuilder for TestArtifactBuilder {
        fn build(
            &mut self,
            source_block: &SourceBlock,
        ) -> Result<crate::chain_ingest::BuiltArtifacts, crate::ArtifactDeriveError> {
            Ok(BuiltArtifacts {
                block: BlockArtifact::new(
                    source_block.height,
                    source_block.hash,
                    source_block.parent_hash,
                    source_block.raw_block_bytes.clone(),
                ),
                compact_block: CompactBlockArtifact::new(
                    source_block.height,
                    source_block.hash,
                    test_compact_block_payload(source_block.height, source_block.hash),
                ),
                transactions: Vec::new(),
                transparent_address_utxos: Vec::new(),
                transparent_utxo_spends: Vec::new(),
                tip_metadata: ChainTipMetadata::empty(),
            })
        }
    }

    fn test_compact_block_payload(height: BlockHeight, block_hash: BlockHash) -> Vec<u8> {
        LightwalletdCompactBlock {
            proto_version: 1,
            height: u64::from(height.value()),
            hash: block_hash.as_bytes().into(),
            prev_hash: Vec::new(),
            time: 1_774_668_400,
            header: Vec::new(),
            vtx: Vec::new(),
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
            }),
        }
        .encode_to_vec()
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }
}
