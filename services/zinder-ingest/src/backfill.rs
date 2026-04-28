use std::{num::NonZeroU32, path::PathBuf};

use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
    TreeStateArtifact,
};
use zinder_source::{NodeSource, NodeTarget, SourceChainCheckpoint};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainEpochCommitOutcome,
    ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
};

use crate::chain_ingest::{
    ArtifactBuilder, IngestArtifactBuilder, IngestBatch, IngestError, IngestRetryState,
    IngestSubtreeRootIndexes, NodeSourceKind, commit_ingest_batch, current_unix_millis,
    fetch_block_with_retry, next_chain_epoch_id, next_chain_epoch_id_after,
    populate_subtree_root_artifacts,
};

/// Configuration for a one-shot historical backfill outside the reorg window.
#[derive(Clone, Debug)]
pub struct BackfillConfig {
    /// Resolved upstream node endpoint (network, JSON-RPC URL, auth, timeout,
    /// response-size cap). See [`NodeTarget`].
    pub node: NodeTarget,
    /// Upstream node source implementation.
    pub node_source: NodeSourceKind,
    /// Local canonical store path.
    pub storage_path: PathBuf,
    /// First block height to backfill.
    pub from_height: BlockHeight,
    /// Last block height to backfill.
    pub to_height: BlockHeight,
    /// Maximum number of blocks committed in one chain epoch.
    pub commit_batch_blocks: NonZeroU32,
    /// Allows finalizing blocks inside the upstream node's current reorg window.
    pub allow_near_tip_finalize: bool,
    /// Optional starting checkpoint for an empty store.
    ///
    /// When present and the store is empty, ingest seeds a stub chain epoch
    /// at `checkpoint.height` carrying the node-supplied
    /// `tip_metadata`, then begins backfill from `checkpoint.height + 1`.
    /// `from_height` must equal `checkpoint.height + 1` in this mode. Reads
    /// at heights below the checkpoint return `ArtifactUnavailable`.
    pub checkpoint: Option<SourceChainCheckpoint>,
}

/// Outcome of a historical backfill run.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum BackfillOutcome {
    /// The run committed at least one new chain epoch.
    Committed(Box<ChainEpochCommitOutcome>),
    /// The requested range was already present in the canonical store.
    AlreadyComplete {
        /// Current chain epoch that already covers the requested range.
        chain_epoch: ChainEpoch,
    },
}

impl BackfillOutcome {
    /// Returns the chain epoch visible after this backfill run.
    #[must_use]
    pub const fn chain_epoch(&self) -> ChainEpoch {
        match self {
            Self::Committed(commit_outcome) => commit_outcome.chain_epoch,
            Self::AlreadyComplete { chain_epoch } => *chain_epoch,
        }
    }
}

/// Runs a historical backfill and commits the requested range to canonical storage.
pub async fn backfill<Source>(
    config: &BackfillConfig,
    source: &Source,
) -> Result<BackfillOutcome, IngestError>
where
    Source: NodeSource,
{
    if config.from_height > config.to_height {
        return Err(IngestError::InvalidBackfillRange {
            from_height: config.from_height,
            to_height: config.to_height,
        });
    }

    let store_options = ChainStoreOptions::for_network(config.node.network);
    let node_tip_height = source.tip_id().await?.height;
    validate_backfill_finality_bound(config, node_tip_height, store_options.reorg_window_blocks)?;
    warn_if_checkpoint_within_reorg_window(
        config,
        node_tip_height,
        store_options.reorg_window_blocks,
    );

    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    let current_chain_epoch = match bootstrap_from_checkpoint_if_needed(
        &store,
        config.node.network,
        config.checkpoint,
        config.from_height,
    )? {
        Some(bootstrapped) => Some(bootstrapped),
        None => store.current_chain_epoch()?,
    };
    let Some(backfill_start) =
        backfill_start(current_chain_epoch, config.from_height, config.to_height)?
    else {
        return Ok(BackfillOutcome::AlreadyComplete {
            chain_epoch: current_chain_epoch.ok_or(IngestError::BackfillProducedNoCommit)?,
        });
    };
    let mut artifact_builder =
        IngestArtifactBuilder::with_initial_tip_metadata(backfill_start.initial_tip_metadata);

    backfill_from_source_with_store(
        config,
        source,
        &store,
        &mut artifact_builder,
        backfill_start,
    )
    .await
    .map(Box::new)
    .map(BackfillOutcome::Committed)
}

#[cfg(test)]
async fn backfill_from_source_with_artifact_builder<Source, Builder>(
    config: &BackfillConfig,
    source: &Source,
    artifact_builder: &mut Builder,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    Builder: ArtifactBuilder,
{
    let store_options = ChainStoreOptions::for_network(config.node.network);
    validate_backfill_finality_bound(
        config,
        source.tip_id().await?.height,
        store_options.reorg_window_blocks,
    )?;

    let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
    backfill_from_source_with_store(
        config,
        source,
        &store,
        artifact_builder,
        BackfillStart {
            from_height: config.from_height,
            initial_tip_metadata: ChainTipMetadata::empty(),
        },
    )
    .await
}

async fn backfill_from_source_with_store<Source, Builder>(
    config: &BackfillConfig,
    source: &Source,
    store: &PrimaryChainStore,
    artifact_builder: &mut Builder,
    backfill_start: BackfillStart,
) -> Result<ChainEpochCommitOutcome, IngestError>
where
    Source: NodeSource,
    Builder: ArtifactBuilder,
{
    let mut chain_epoch_id = next_chain_epoch_id(store)?;
    let mut batch = IngestBatch::default();
    let mut next_subtree_root_indexes =
        IngestSubtreeRootIndexes::from_tip_metadata(backfill_start.initial_tip_metadata);
    let mut last_commit_outcome = None;
    let mut retry_state = IngestRetryState::default();
    let commit_batch_blocks = usize::try_from(config.commit_batch_blocks.get())
        .map_err(|_| IngestError::InvalidCommitBatchBlocks)?;

    for height in BlockHeightRange::inclusive(backfill_start.from_height, config.to_height) {
        let source_block = fetch_block_with_retry(
            config.node.request_timeout,
            source,
            height,
            &mut retry_state,
        )
        .await?;
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

        if batch.len() == commit_batch_blocks {
            let updated_subtree_root_indexes = populate_subtree_root_artifacts(
                config.node.request_timeout,
                source,
                &mut batch,
                next_subtree_root_indexes,
                &mut retry_state,
            )
            .await?;
            let commit_outcome = commit_finalized_backfill_batch(
                store,
                config.node.network,
                chain_epoch_id,
                &mut batch,
            )?;
            next_subtree_root_indexes = updated_subtree_root_indexes;
            chain_epoch_id = next_chain_epoch_id_after(chain_epoch_id)?;
            last_commit_outcome = Some(commit_outcome);
        }
    }

    if !batch.is_empty() {
        let _updated_subtree_root_indexes = populate_subtree_root_artifacts(
            config.node.request_timeout,
            source,
            &mut batch,
            next_subtree_root_indexes,
            &mut retry_state,
        )
        .await?;
        let commit_outcome = commit_finalized_backfill_batch(
            store,
            config.node.network,
            chain_epoch_id,
            &mut batch,
        )?;
        last_commit_outcome = Some(commit_outcome);
    }

    last_commit_outcome.ok_or(IngestError::BackfillProducedNoCommit)
}

fn commit_finalized_backfill_batch(
    store: &PrimaryChainStore,
    network: Network,
    chain_epoch_id: ChainEpochId,
    batch: &mut IngestBatch,
) -> Result<ChainEpochCommitOutcome, IngestError> {
    let tip_block = batch
        .finalized_blocks
        .last()
        .ok_or(IngestError::EmptyIngestBatch)?;
    let tip_height = tip_block.height;
    let tip_hash = tip_block.block_hash;
    let tip_metadata = batch.tip_metadata.ok_or(IngestError::EmptyIngestBatch)?;
    let chain_epoch = ChainEpoch {
        id: chain_epoch_id,
        network,
        tip_height,
        tip_hash,
        finalized_height: tip_height,
        finalized_hash: tip_hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata,
        created_at: current_unix_millis()?,
    };
    commit_ingest_batch(
        store,
        chain_epoch,
        batch,
        ReorgWindowChange::FinalizeThrough { height: tip_height },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackfillStart {
    from_height: BlockHeight,
    initial_tip_metadata: ChainTipMetadata,
}

/// Seeds an empty store with a stub chain epoch derived from the operator's
/// checkpoint, so backfill can start at `checkpoint.height + 1` without
/// replaying every block from genesis.
///
/// Returns `Ok(Some(chain_epoch))` after a successful bootstrap commit.
/// Returns `Ok(None)` when no bootstrap is needed (no checkpoint provided,
/// or store already has a chain epoch).
/// Returns `Err(BackfillCheckpointMisaligned)` when `from_height` does not
/// match `checkpoint.height + 1`.
fn bootstrap_from_checkpoint_if_needed(
    store: &PrimaryChainStore,
    network: Network,
    checkpoint: Option<SourceChainCheckpoint>,
    from_height: BlockHeight,
) -> Result<Option<ChainEpoch>, IngestError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    if store.current_chain_epoch()?.is_some() {
        return Ok(None);
    }
    let expected_from_height = BlockHeight::new(checkpoint.height.value().saturating_add(1));
    if from_height != expected_from_height {
        return Err(IngestError::BackfillCheckpointMisaligned {
            checkpoint_height: checkpoint.height,
            from_height,
        });
    }

    let bootstrap_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network,
        tip_height: checkpoint.height,
        tip_hash: checkpoint.hash,
        finalized_height: checkpoint.height,
        finalized_hash: checkpoint.hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: checkpoint.tip_metadata,
        created_at: current_unix_millis()?,
    };
    let outcome = store.commit_chain_epoch(
        ChainEpochArtifacts::new(bootstrap_chain_epoch, Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
                height: checkpoint.height,
            }),
    )?;
    Ok(Some(outcome.chain_epoch))
}

fn backfill_start(
    current_chain_epoch: Option<ChainEpoch>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<Option<BackfillStart>, IngestError> {
    let Some(current_chain_epoch) = current_chain_epoch else {
        if from_height == BlockHeight::new(1) {
            return Ok(Some(BackfillStart {
                from_height,
                initial_tip_metadata: ChainTipMetadata::empty(),
            }));
        }

        return Err(IngestError::BackfillRequiresContiguousTipMetadata {
            from_height,
            current_tip_height: None,
        });
    };

    if current_chain_epoch.tip_height >= to_height {
        return Ok(None);
    }

    if current_chain_epoch
        .tip_height
        .value()
        .checked_add(1)
        .is_some_and(|next_height| from_height <= BlockHeight::new(next_height))
    {
        return Ok(Some(BackfillStart {
            from_height: BlockHeight::new(current_chain_epoch.tip_height.value().saturating_add(1)),
            initial_tip_metadata: current_chain_epoch.tip_metadata,
        }));
    }

    Err(IngestError::BackfillRequiresContiguousTipMetadata {
        from_height,
        current_tip_height: Some(current_chain_epoch.tip_height),
    })
}

fn validate_backfill_finality_bound(
    config: &BackfillConfig,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) -> Result<(), IngestError> {
    if config.allow_near_tip_finalize {
        return Ok(());
    }

    let maximum_historical_height =
        BlockHeight::new(tip_height.value().saturating_sub(reorg_window_blocks));
    if config.to_height <= maximum_historical_height {
        return Ok(());
    }

    Err(IngestError::NearTipBackfillRequiresExplicitFinalize {
        to_height: config.to_height,
        tip_height,
        reorg_window_blocks,
        maximum_historical_height,
    })
}

/// Emits a warning when the resolved checkpoint sits inside the upstream node's
/// reorg window.
///
/// The first reorg deeper than `tip - checkpoint_height` would surface
/// `ReorgWindowExceeded` with no recovery short of re-bootstrapping at a
/// deeper checkpoint, so operators need a heads-up before any state lands.
fn warn_if_checkpoint_within_reorg_window(
    config: &BackfillConfig,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) {
    let Some(checkpoint) = config.checkpoint.as_ref() else {
        return;
    };
    if !checkpoint_within_reorg_window(checkpoint.height, tip_height, reorg_window_blocks) {
        return;
    }
    let safe_floor = tip_height.value().saturating_sub(reorg_window_blocks);
    tracing::warn!(
        target: "zinder::ingest",
        event = "backfill_checkpoint_within_reorg_window",
        checkpoint_height = checkpoint.height.value(),
        tip_height = tip_height.value(),
        reorg_window_blocks,
        safe_checkpoint_floor = safe_floor,
        "checkpoint sits inside the node-reported reorg window; first reorg may surface ReorgWindowExceeded"
    );
}

const fn checkpoint_within_reorg_window(
    checkpoint_height: BlockHeight,
    tip_height: BlockHeight,
    reorg_window_blocks: u32,
) -> bool {
    checkpoint_height.value() > tip_height.value().saturating_sub(reorg_window_blocks)
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        num::NonZeroU32,
        path::Path,
        sync::atomic::{AtomicU32, Ordering},
    };

    use prost::Message;
    use tempfile::tempdir;
    use zinder_core::{
        BlockArtifact, BlockHash, BlockId, CompactBlockArtifact, SUBTREE_LEAF_COUNT,
        ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex, UnixTimestampMillis,
    };
    use zinder_proto::compat::lightwalletd::{
        ChainMetadata, CompactBlock as LightwalletdCompactBlock,
    };
    use zinder_source::{
        NodeCapabilities, SourceBlock, SourceBlockHeader, SourceError, SourceSubtreeRoot,
        SourceSubtreeRoots, ZebraJsonRpcSource,
    };
    use zinder_store::ChainEventHistoryRequest;

    use crate::{
        ArtifactDeriveError,
        chain_ingest::{BuiltArtifacts, source_error_is_retryable},
    };

    use super::*;

    #[tokio::test]
    async fn backfill_rejects_near_tip_finalize_without_explicit_override()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("near-tip-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 101, 150, 50, false)?;

        let mut artifact_builder = TestArtifactBuilder::default();
        let error = match backfill_from_source_with_artifact_builder(
            &config,
            &source,
            &mut artifact_builder,
        )
        .await
        {
            Ok(commit_outcome) => {
                return Err(format!("expected near-tip rejection, got {commit_outcome:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::NearTipBackfillRequiresExplicitFinalize {
                to_height,
                tip_height,
                reorg_window_blocks: 100,
                maximum_historical_height,
            } if to_height == BlockHeight::new(150)
                && tip_height == BlockHeight::new(200)
                && maximum_historical_height == BlockHeight::new(100)
        ));
        assert!(!storage_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn exact_divisor_backfill_returns_last_full_batch_outcome() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("exact-divisor-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 1, 10, 5, false)?;

        let mut artifact_builder = TestArtifactBuilder::default();
        let commit_outcome =
            backfill_from_source_with_artifact_builder(&config, &source, &mut artifact_builder)
                .await?;

        assert_eq!(commit_outcome.chain_epoch.id, ChainEpochId::new(2));
        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(10));
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        assert_eq!(
            store
                .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?
                .len(),
            2
        );

        Ok(())
    }

    #[tokio::test]
    async fn backfill_retries_retryable_block_fetch_failures() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("retryable-source-store");
        let source = FlakyNodeSource {
            delegate: TestNodeSource {
                tip_height: BlockHeight::new(200),
                network: Network::ZcashRegtest,
            },
            failure: FlakySourceFailure::NodeUnavailable,
            retryable_failures_before_success: AtomicU32::new(2),
            fetch_attempts: AtomicU32::new(0),
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let mut artifact_builder = TestArtifactBuilder::default();
        let commit_outcome =
            backfill_from_source_with_artifact_builder(&config, &source, &mut artifact_builder)
                .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        assert_eq!(source.fetch_attempts.load(Ordering::SeqCst), 3);

        Ok(())
    }

    #[tokio::test]
    async fn backfill_retries_retryable_block_unavailable_failures() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("retryable-block-unavailable-store");
        let source = FlakyNodeSource {
            delegate: TestNodeSource {
                tip_height: BlockHeight::new(200),
                network: Network::ZcashRegtest,
            },
            failure: FlakySourceFailure::BlockUnavailable { is_retryable: true },
            retryable_failures_before_success: AtomicU32::new(2),
            fetch_attempts: AtomicU32::new(0),
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let mut artifact_builder = TestArtifactBuilder::default();
        let commit_outcome =
            backfill_from_source_with_artifact_builder(&config, &source, &mut artifact_builder)
                .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        assert_eq!(source.fetch_attempts.load(Ordering::SeqCst), 3);

        Ok(())
    }

    #[tokio::test]
    async fn backfill_does_not_retry_fatal_block_unavailable_failures() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("fatal-block-unavailable-store");
        let source = FlakyNodeSource {
            delegate: TestNodeSource {
                tip_height: BlockHeight::new(200),
                network: Network::ZcashRegtest,
            },
            failure: FlakySourceFailure::BlockUnavailable {
                is_retryable: false,
            },
            retryable_failures_before_success: AtomicU32::new(2),
            fetch_attempts: AtomicU32::new(0),
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let mut artifact_builder = TestArtifactBuilder::default();
        let error = match backfill_from_source_with_artifact_builder(
            &config,
            &source,
            &mut artifact_builder,
        )
        .await
        {
            Ok(commit_outcome) => {
                return Err(format!("expected source error, got {commit_outcome:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::Source(SourceError::BlockUnavailable {
                is_retryable: false,
                ..
            })
        ));
        assert_eq!(source.fetch_attempts.load(Ordering::SeqCst), 1);

        Ok(())
    }

    #[tokio::test]
    async fn source_retry_classification_keeps_protocol_errors_fatal() {
        assert!(source_error_is_retryable(&SourceError::NodeUnavailable {
            reason: "connection reset".to_owned(),
            is_retryable: true,
        }));
        assert!(!source_error_is_retryable(&SourceError::BlockUnavailable {
            height: BlockHeight::new(1),
            reason: "invalid height parameter".to_owned(),
            is_retryable: false,
        }));
        assert!(!source_error_is_retryable(
            &SourceError::SourceProtocolMismatch {
                reason: "missing block hash",
            }
        ));
    }

    #[tokio::test]
    async fn backfill_commits_newly_completed_subtree_roots() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("subtree-root-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 1, 1, 1, false)?;

        let mut artifact_builder = TestArtifactBuilder {
            sapling_commitment_tree_size: SUBTREE_LEAF_COUNT,
            orchard_commitment_tree_size: 0,
        };
        let commit_outcome =
            backfill_from_source_with_artifact_builder(&config, &source, &mut artifact_builder)
                .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(1));
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let reader = store.current_chain_epoch_reader()?;
        let subtree_roots = reader.subtree_roots(zinder_core::SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(1).ok_or("invalid max entries")?,
        ))?;
        let subtree_root = subtree_roots
            .first()
            .and_then(Option::as_ref)
            .ok_or("missing committed subtree root")?;

        assert_eq!(subtree_root.protocol, ShieldedProtocol::Sapling);
        assert_eq!(subtree_root.subtree_index, SubtreeRootIndex::new(0));
        assert_eq!(subtree_root.root_hash.as_bytes(), [0x33; 32]);
        assert_eq!(subtree_root.completing_block_height, BlockHeight::new(1));
        assert_eq!(subtree_root.completing_block_hash, block_hash(1));

        Ok(())
    }

    #[tokio::test]
    async fn canonical_backfill_requires_genesis_or_contiguous_tree_size_base()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("non-genesis-store");
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };
        let config = test_backfill_config(&storage_path, 2, 2, 1, false)?;

        let error = match backfill(&config, &source).await {
            Ok(commit_outcome) => {
                return Err(
                    format!("expected tree-size base rejection, got {commit_outcome:?}").into(),
                );
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            IngestError::BackfillRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: None,
            } if from_height == BlockHeight::new(2)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn backfill_start_resumes_or_completes_from_current_tip() -> Result<(), Box<dyn Error>> {
        let tip_metadata = ChainTipMetadata::new(123, 456);
        let current_chain_epoch = test_chain_epoch(BlockHeight::new(9), tip_metadata);

        let contiguous_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(10),
            BlockHeight::new(20),
        )?
        .ok_or("contiguous range should need work")?;
        let resumed_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(20),
        )?
        .ok_or("partial rerun should need work")?;
        let completed_start = backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(1),
            BlockHeight::new(9),
        )?;
        let error = match backfill_start(
            Some(current_chain_epoch),
            BlockHeight::new(11),
            BlockHeight::new(20),
        ) {
            Ok(start) => {
                return Err(format!("expected non-contiguous rejection, got {start:?}").into());
            }
            Err(error) => error,
        };

        assert_eq!(contiguous_start.from_height, BlockHeight::new(10));
        assert_eq!(contiguous_start.initial_tip_metadata, tip_metadata);
        assert_eq!(resumed_start.from_height, BlockHeight::new(10));
        assert_eq!(resumed_start.initial_tip_metadata, tip_metadata);
        assert_eq!(completed_start, None);
        assert!(matches!(
            error,
            IngestError::BackfillRequiresContiguousTipMetadata {
                from_height,
                current_tip_height: Some(current_tip_height),
            } if from_height == BlockHeight::new(11)
                && current_tip_height == BlockHeight::new(9)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn backfill_seeds_chain_epoch_from_checkpoint_then_extends() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-bootstrap-store");
        let checkpoint_height = BlockHeight::new(10);
        // Match the TestNodeSource's block hash convention so the first
        // backfilled block (height 11) finds the right parent linkage.
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Tree sizes well below SUBTREE_LEAF_COUNT so no subtree completes
        // during backfill; the unit test validates the bootstrap + extend
        // round-trip without spawning a real source subtree path.
        let checkpoint_tip_metadata = ChainTipMetadata::new(0, 0);
        let mut config = test_backfill_config(&storage_path, 11, 12, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            checkpoint_height,
            checkpoint_hash,
            checkpoint_tip_metadata,
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let mut artifact_builder = TestArtifactBuilder::default();
        let commit_outcome = backfill_from_source_with_artifact_builder_and_bootstrap(
            &config,
            &source,
            &mut artifact_builder,
        )
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(12));

        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let event_history =
            store.chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?;
        // 1 bootstrap commit + 2 single-block backfill commits (heights 11
        // and 12 with commit_batch_blocks = 1).
        assert_eq!(
            event_history.len(),
            3,
            "checkpoint bootstrap commit plus per-block backfill commits"
        );

        Ok(())
    }

    #[tokio::test]
    async fn backfill_from_checkpoint_skips_pre_checkpoint_subtree_root_indexes()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("checkpoint-subtree-indexes-store");
        let checkpoint_height = BlockHeight::new(10);
        let checkpoint_hash = block_hash(checkpoint_height.value());
        // Checkpoint encodes one already-completed Sapling subtree. Without
        // seeding `IngestSubtreeRootIndexes` from `tip_metadata`, the backfill
        // would ask the node for subtree 0 (completing far below the
        // batch range) and surface SubtreeRootCompletingBlockMissing. This
        // mirrors the live mainnet failure observed when calibrating against
        // a checkpoint at `tip - 1000`.
        let checkpoint_tip_metadata = ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0);
        let mut config = test_backfill_config(&storage_path, 11, 11, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            checkpoint_height,
            checkpoint_hash,
            checkpoint_tip_metadata,
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let mut artifact_builder = TestArtifactBuilder {
            sapling_commitment_tree_size: SUBTREE_LEAF_COUNT,
            orchard_commitment_tree_size: 0,
        };
        let commit_outcome = backfill_from_source_with_artifact_builder_and_bootstrap(
            &config,
            &source,
            &mut artifact_builder,
        )
        .await?;

        assert_eq!(commit_outcome.chain_epoch.tip_height, BlockHeight::new(11));
        Ok(())
    }

    #[test]
    fn checkpoint_within_reorg_window_marks_only_inside_window() {
        // Tip 200, window 100 -> safe historical floor at height 100. A
        // checkpoint at 99 finalizes outside the window; 100 sits exactly on
        // the floor so the next commit needs no rewind. Anything above 100 is
        // inside the window and should warn.
        assert!(!checkpoint_within_reorg_window(
            BlockHeight::new(99),
            BlockHeight::new(200),
            100,
        ));
        assert!(!checkpoint_within_reorg_window(
            BlockHeight::new(100),
            BlockHeight::new(200),
            100,
        ));
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(101),
            BlockHeight::new(200),
            100,
        ));
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(200),
            BlockHeight::new(200),
            100,
        ));
        // Tip below the window: the safe floor saturates at 0, so every
        // checkpoint above 0 is inside the window.
        assert!(checkpoint_within_reorg_window(
            BlockHeight::new(1),
            BlockHeight::new(50),
            100,
        ));
    }

    #[tokio::test]
    async fn backfill_rejects_misaligned_checkpoint() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("misaligned-checkpoint-store");
        let mut config = test_backfill_config(&storage_path, 50, 60, 1, true)?;
        config.checkpoint = Some(SourceChainCheckpoint::new(
            BlockHeight::new(10),
            BlockHash::from_bytes([0xa5; 32]),
            ChainTipMetadata::empty(),
        ));
        let source = TestNodeSource {
            tip_height: BlockHeight::new(200),
            network: Network::ZcashRegtest,
        };

        let error = match backfill(&config, &source).await {
            Ok(commit_outcome) => {
                return Err(format!(
                    "expected misaligned-checkpoint rejection, got {commit_outcome:?}"
                )
                .into());
            }
            Err(error) => error,
        };
        assert!(matches!(
            error,
            IngestError::BackfillCheckpointMisaligned {
                checkpoint_height,
                from_height,
            } if checkpoint_height == BlockHeight::new(10)
                && from_height == BlockHeight::new(50)
        ));

        Ok(())
    }

    /// Wraps `backfill_from_source_with_artifact_builder` after running the
    /// checkpoint bootstrap path so unit tests can exercise both phases
    /// without spawning a real upstream node client.
    #[cfg(test)]
    async fn backfill_from_source_with_artifact_builder_and_bootstrap<Source, Builder>(
        config: &BackfillConfig,
        source: &Source,
        artifact_builder: &mut Builder,
    ) -> Result<ChainEpochCommitOutcome, IngestError>
    where
        Source: NodeSource,
        Builder: ArtifactBuilder,
    {
        let store_options = ChainStoreOptions::for_network(config.node.network);
        validate_backfill_finality_bound(
            config,
            source.tip_id().await?.height,
            store_options.reorg_window_blocks,
        )?;
        let store = PrimaryChainStore::open(&config.storage_path, store_options)?;
        let bootstrapped = bootstrap_from_checkpoint_if_needed(
            &store,
            config.node.network,
            config.checkpoint,
            config.from_height,
        )?;
        let initial_tip_metadata = bootstrapped
            .map_or_else(ChainTipMetadata::empty, |chain_epoch| {
                chain_epoch.tip_metadata
            });
        backfill_from_source_with_store(
            config,
            source,
            &store,
            artifact_builder,
            BackfillStart {
                from_height: config.from_height,
                initial_tip_metadata,
            },
        )
        .await
    }

    fn test_backfill_config(
        storage_path: &Path,
        from_height: u32,
        to_height: u32,
        commit_batch_blocks: u32,
        allow_near_tip_finalize: bool,
    ) -> Result<BackfillConfig, Box<dyn Error>> {
        Ok(BackfillConfig {
            node: NodeTarget::new(
                Network::ZcashRegtest,
                "http://127.0.0.1:39232".to_owned(),
                zinder_source::NodeAuth::None,
                std::time::Duration::from_secs(30),
                zinder_source::DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
            ),
            node_source: NodeSourceKind::ZebraJsonRpc,
            storage_path: storage_path.to_owned(),
            from_height: BlockHeight::new(from_height),
            to_height: BlockHeight::new(to_height),
            commit_batch_blocks: NonZeroU32::new(commit_batch_blocks)
                .ok_or("invalid test batch size")?,
            allow_near_tip_finalize,
            checkpoint: None,
        })
    }

    struct TestNodeSource {
        tip_height: BlockHeight,
        network: Network,
    }

    struct FlakyNodeSource {
        delegate: TestNodeSource,
        failure: FlakySourceFailure,
        retryable_failures_before_success: AtomicU32,
        fetch_attempts: AtomicU32,
    }

    #[derive(Clone, Copy)]
    enum FlakySourceFailure {
        NodeUnavailable,
        BlockUnavailable { is_retryable: bool },
    }

    impl FlakySourceFailure {
        fn source_error(self, height: BlockHeight) -> SourceError {
            match self {
                Self::NodeUnavailable => SourceError::NodeUnavailable {
                    reason: "temporary node outage".to_owned(),
                    is_retryable: true,
                },
                Self::BlockUnavailable { is_retryable } => SourceError::BlockUnavailable {
                    height,
                    reason: "node returned block error".to_owned(),
                    is_retryable,
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for FlakyNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            self.delegate.capabilities()
        }

        async fn fetch_block_by_height(
            &self,
            height: BlockHeight,
        ) -> Result<SourceBlock, SourceError> {
            self.fetch_attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .retryable_failures_before_success
                .load(Ordering::SeqCst)
                > 0
            {
                self.retryable_failures_before_success
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(self.failure.source_error(height));
            }

            self.delegate.fetch_block_by_height(height).await
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            self.delegate.tip_id().await
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: ShieldedProtocol,
            start_index: SubtreeRootIndex,
            max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            self.delegate
                .fetch_subtree_roots(protocol, start_index, max_entries)
                .await
        }
    }

    #[async_trait::async_trait]
    impl NodeSource for TestNodeSource {
        fn capabilities(&self) -> NodeCapabilities {
            ZebraJsonRpcSource::baseline_capabilities()
        }

        async fn fetch_block_by_height(
            &self,
            height: BlockHeight,
        ) -> Result<SourceBlock, SourceError> {
            let source_hash = block_hash(height.value());
            let parent_hash = block_hash(height.value().saturating_sub(1));
            let header = SourceBlockHeader {
                network: self.network,
                height,
                hash: source_hash,
                parent_hash,
                block_time_seconds: 1_774_668_400,
            };

            Ok(
                SourceBlock::new(header, format!("raw-block-{}", height.value()).into_bytes())
                    .with_tree_state_payload_bytes(
                        format!("tree-state-{}", height.value()).into_bytes(),
                    ),
            )
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(
                self.tip_height,
                block_hash(self.tip_height.value()),
            ))
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: ShieldedProtocol,
            start_index: SubtreeRootIndex,
            max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            let subtree_roots = (0..max_entries.get())
                .map(|offset| {
                    start_index
                        .value()
                        .checked_add(offset)
                        .map(SubtreeRootIndex::new)
                        .map(|index| {
                            SourceSubtreeRoot::new(
                                index,
                                SubtreeRootHash::from_bytes([0x33; 32]),
                                BlockHeight::new(1),
                            )
                        })
                        .ok_or(SourceError::SourceProtocolMismatch {
                            reason: "subtree roots response exceeds the SubtreeRootIndex range",
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(SourceSubtreeRoots::new(
                protocol,
                start_index,
                subtree_roots,
            ))
        }
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }

    fn test_chain_epoch(tip_height: BlockHeight, tip_metadata: ChainTipMetadata) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            tip_height,
            tip_hash: block_hash(tip_height.value()),
            finalized_height: tip_height,
            finalized_hash: block_hash(tip_height.value()),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata,
            created_at: UnixTimestampMillis::new(1_774_669_000_000),
        }
    }

    #[derive(Default)]
    struct TestArtifactBuilder {
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
    }

    impl ArtifactBuilder for TestArtifactBuilder {
        fn build(
            &mut self,
            source_block: &SourceBlock,
        ) -> Result<BuiltArtifacts, ArtifactDeriveError> {
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
                    test_compact_block_payload(
                        source_block.height,
                        source_block.hash,
                        self.sapling_commitment_tree_size,
                        self.orchard_commitment_tree_size,
                    ),
                ),
                transactions: Vec::new(),
                transparent_address_utxos: Vec::new(),
                transparent_utxo_spends: Vec::new(),
                tip_metadata: ChainTipMetadata::new(
                    self.sapling_commitment_tree_size,
                    self.orchard_commitment_tree_size,
                ),
            })
        }
    }

    fn test_compact_block_payload(
        height: BlockHeight,
        block_hash: BlockHash,
        sapling_commitment_tree_size: u32,
        orchard_commitment_tree_size: u32,
    ) -> Vec<u8> {
        LightwalletdCompactBlock {
            proto_version: 1,
            height: u64::from(height.value()),
            hash: block_hash.as_bytes().into(),
            prev_hash: vec![0; 32],
            time: 1_774_668_400,
            header: Vec::new(),
            vtx: Vec::new(),
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size,
                orchard_commitment_tree_size,
            }),
        }
        .encode_to_vec()
    }
}
